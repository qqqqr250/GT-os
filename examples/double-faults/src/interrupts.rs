//! The interrupt descriptor table (IDT) and the CPU exception handlers.
//!
//! Post-05 installed an IDT so a CPU exception stopped being a silent triple
//! fault. Post-06 adds the piece that makes it survive the *worst* exception:
//! the double-fault handler is pinned to a dedicated stack from the interrupt
//! stack table (see [`crate::gdt`]), so even a double fault caused by a stack
//! overflow reaches a handler instead of resetting the machine.
//!
//! The handlers *record* what they saw ([`last_vector`], [`exception_count`],
//! [`double_fault_count`]) rather than only printing, so a test can assert that
//! a handler really ran.
//!
//! As a forward-looking hook, [`init`] also brings forward post-07's hardware
//! interrupts: it remaps the 8259 PIC, programs the PIT and unmasks the timer, so
//! [`ticks`] advances, and installs a keyboard handler that feeds post-12's async
//! scancode queue. The full post-07 treatment is
//! [`examples/hardware-interrupts`].

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt;
use crate::{halt_forever, pic, pit, println};

/// The value [`LAST_VECTOR`] holds before any exception has fired.
const NO_EXCEPTION: u8 = 0xff;

/// The vector of the most recent exception, or [`NO_EXCEPTION`].
static LAST_VECTOR: AtomicU8 = AtomicU8::new(NO_EXCEPTION);

/// How many exceptions have been handled since boot.
static EXCEPTION_COUNT: AtomicU64 = AtomicU64::new(0);

/// The vector the timer (IRQ0) is remapped to.
pub const TIMER_VECTOR: u8 = pic::PIC1_OFFSET;

/// The vector the keyboard (IRQ1) is remapped to.
///
/// Post-12's async keyboard: the handler pushes the raw scancode into the task
/// module's lock-free queue (see [`crate::task::keyboard`]).
pub const KEYBOARD_VECTOR: u8 = pic::PIC1_OFFSET + 1;

/// Number of timer interrupts since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// How many double faults have been handled.
///
/// A double fault is fatal, so a running kernel sees this go to at most one; it
/// exists so the handler's effect is observable from a test.
static DOUBLE_FAULTS: AtomicU64 = AtomicU64::new(0);

/// The IDT, built in place and kept in a `static`.
///
/// [`InterruptDescriptorTable::new`] is a `const fn`, so the table can live in a
/// `static` and be filled in at run time. It is never moved, which is what makes
/// the [`load_unsafe`](InterruptDescriptorTable::load_unsafe) call in [`init`]
/// sound.
static IDT: Mutex<InterruptDescriptorTable> = Mutex::new(InterruptDescriptorTable::new());

/// A CPU exception vector this kernel installs a handler for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Vector {
    /// `#DE` — the CPU divided by zero.
    DivideError = 0,
    /// `#BP` — a debugger, or the `int3` instruction.
    Breakpoint = 3,
    /// `#UD` — the CPU decoded an undefined instruction.
    InvalidOpcode = 6,
    /// `#DF` — an exception was raised while handling another one.
    DoubleFault = 8,
    /// `#GP` — a protection rule was violated.
    GeneralProtectionFault = 13,
    /// `#PF` — a memory access failed.
    PageFault = 14,
}

impl Vector {
    /// The name the CPU manual gives this exception.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::DivideError => "DIVIDE ERROR",
            Self::Breakpoint => "BREAKPOINT",
            Self::InvalidOpcode => "INVALID OPCODE",
            Self::DoubleFault => "DOUBLE FAULT",
            Self::GeneralProtectionFault => "GENERAL PROTECTION FAULT",
            Self::PageFault => "PAGE FAULT",
        }
    }

    /// Converts a raw vector number, if this kernel handles it.
    ///
    /// Vectors outside the handled set (including [`NO_EXCEPTION`]) return
    /// `None`.
    #[must_use]
    pub const fn from_u8(vector: u8) -> Option<Self> {
        Some(match vector {
            0 => Self::DivideError,
            3 => Self::Breakpoint,
            6 => Self::InvalidOpcode,
            8 => Self::DoubleFault,
            13 => Self::GeneralProtectionFault,
            14 => Self::PageFault,
            _ => return None,
        })
    }
}

/// Installs the exception handlers and the timer handler, loads the IDT, then
/// remaps the PIC, starts the PIT and enables interrupts.
///
/// [`crate::gdt::init`] must have run first: the double-fault entry is pointed at
/// [`gdt::DOUBLE_FAULT_IST_INDEX`], which names a stack that only exists once the
/// TSS is loaded.
///
/// Safe to call more than once; each call simply reloads the same table and
/// re-programs the PIC and PIT. The handlers must be in place *before* the table
/// is loaded, which is why the whole sequence happens under one lock.
pub fn init() {
    {
        let mut idt = IDT.lock();
        idt.divide_error.set_handler_fn(divide_error);
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        // SAFETY: `gdt::init` has loaded the TSS, so the IST slot is valid, and no
        // other handler uses it.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault)
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault);
        idt.page_fault.set_handler_fn(page_fault);
        idt[TIMER_VECTOR].set_handler_fn(timer_interrupt);
        idt[KEYBOARD_VECTOR].set_handler_fn(keyboard_interrupt);

        // SAFETY: `IDT` lives in a `static` and is never moved, so the address the
        // CPU caches in the IDTR stays valid for the life of the kernel. `load`
        // would need a `&'static` reference that the mutex guard cannot hand out, so
        // the unsafe API is the correct one here.
        unsafe { idt.load_unsafe() }
    }

    // Post-07, brought forward: remap the PIC so IRQ0 no longer collides with
    // the double fault, start the PIT, unmask the timer line and enable
    // interrupts. The order matters — an IRQ before the remap would arrive on
    // vector 8.
    pic::init();
    pit::init();
    pic::unmask(pic::master_lines(), 0x00);
    x86_64::instructions::interrupts::enable();
}

/// Raises a breakpoint exception (`int3`).
///
/// The handler must already be installed (see [`init`]); calling this before
/// [`init`] faults.
pub fn trigger_breakpoint() {
    x86_64::instructions::interrupts::int3();
}

/// The vector of the most recent exception, or `None` if none has fired.
#[must_use]
pub fn last_vector() -> Option<Vector> {
    Vector::from_u8(LAST_VECTOR.load(Ordering::Relaxed))
}

/// How many exceptions have been handled since boot.
#[must_use]
pub fn exception_count() -> u64 {
    EXCEPTION_COUNT.load(Ordering::Relaxed)
}

/// Number of timer interrupts since boot.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// How many double faults have been handled since boot.
#[must_use]
pub fn double_fault_count() -> u64 {
    DOUBLE_FAULTS.load(Ordering::Relaxed)
}

/// Records `vector`, prints where it happened, and returns to the caller.
fn record(vector: Vector, frame: &InterruptStackFrame) {
    LAST_VECTOR.store(vector as u8, Ordering::Relaxed);
    EXCEPTION_COUNT.fetch_add(1, Ordering::Relaxed);
    println!(
        "EXCEPTION: {} at rip {:#018x}, rsp {:#018x}",
        vector.name(),
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64(),
    );
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    record(Vector::DivideError, &frame);
    halt_forever();
}

/// The timer (IRQ0): count the tick and acknowledge it on the PIC.
///
/// Without the end-of-interrupt the PIC would never deliver IRQ0 again, so the
/// count would stall at one.
extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    pic::end_of_interrupt(0);
}

/// The keyboard (IRQ1): read the scancode and hand it to the async decoder.
///
/// Post-12 moved the decoding out of the handler: this only pushes the raw
/// scancode into [`crate::task::keyboard`]'s lock-free queue and wakes the task
/// that decodes it.
extern "x86-interrupt" fn keyboard_interrupt(_frame: InterruptStackFrame) {
    // SAFETY: port 0x60 is the PS/2 data port.
    let scancode = unsafe { Port::<u8>::new(0x60).read() };
    crate::task::keyboard::add_scancode(scancode);
    pic::end_of_interrupt(1);
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    record(Vector::InvalidOpcode, &frame);
    halt_forever();
}

/// A breakpoint is recoverable: record it and return to the interrupted code.
extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    record(Vector::Breakpoint, &frame);
}

/// The double fault is fatal, and runs on the dedicated IST stack.
///
/// The stack frame it is handed is the *interrupted* one, so `rsp` in the
/// printed line is the (possibly bad) stack the CPU was on when the fault
/// happened — the handler itself is already on the good stack.
extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    DOUBLE_FAULTS.fetch_add(1, Ordering::Relaxed);
    record(Vector::DoubleFault, &frame);
    println!("  error code: {error_code:#x} (always zero for a double fault)");
    halt_forever();
}

extern "x86-interrupt" fn general_protection_fault(frame: InterruptStackFrame, error_code: u64) {
    record(Vector::GeneralProtectionFault, &frame);
    println!("  error code: {error_code:#x}");
    halt_forever();
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    record(Vector::PageFault, &frame);
    let address = Cr2::read().map_or(0, x86_64::VirtAddr::as_u64);
    println!("  address: {address:#018x}, error: {error_code:?}");
    halt_forever();
}
