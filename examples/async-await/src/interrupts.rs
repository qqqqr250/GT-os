//! The interrupt descriptor table, the 8259 PIC, the PIT and the device
//! handlers.
//!
//! Post-07 is where hardware interrupts arrive: [`init`] remaps the PIC so IRQ0
//! (the timer) and IRQ1 (the keyboard) no longer collide with the CPU exception
//! vectors, programs the PIT to tick at [`crate::pit::TICK_HZ`], unmasks those
//! lines and enables interrupts. The double-fault handler from post-06 keeps its
//! dedicated stack.
//!
//! The timer handler *prints* a dot every [`DOT_INTERVAL`] ticks while
//! [`set_timer_dots`] is on. That is what makes post-07's deadlock reproducible:
//! the handler and the main thread share the serial lock, and only
//! [`crate::serial::print_args`]'s `without_interrupts` keeps them from
//! deadlocking. The exception handlers *record* what they saw, so a test can
//! assert a handler really ran.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

use crate::gdt;
use crate::{halt_forever, pic, pit, println, serial_print};

/// The vector the timer (IRQ0) is remapped to.
pub const TIMER_VECTOR: u8 = pic::PIC1_OFFSET;
/// The vector the keyboard (IRQ1) is remapped to.
pub const KEYBOARD_VECTOR: u8 = pic::PIC1_OFFSET + 1;

/// How many timer ticks pass between the handler's dots when they are enabled.
pub const DOT_INTERVAL: u64 = 10;

/// The value [`LAST_VECTOR`] holds before any exception has fired.
const NO_EXCEPTION: u8 = 0xff;

/// The vector of the most recent exception, or [`NO_EXCEPTION`].
static LAST_VECTOR: AtomicU8 = AtomicU8::new(NO_EXCEPTION);

/// How many exceptions have been handled since boot.
static EXCEPTION_COUNT: AtomicU64 = AtomicU64::new(0);

/// How many double faults have been handled.
static DOUBLE_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Number of timer interrupts since boot.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Whether the timer handler prints a dot. Off by default; a test turns it on.
static TIMER_DOTS: AtomicBool = AtomicBool::new(false);

/// The IDT, built in place and kept in a `static`.
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

/// Installs the handlers, remaps the PIC, programs the PIT and enables
/// interrupts.
///
/// [`crate::gdt::init`] must have run first: the double-fault entry names an IST
/// slot that only resolves once the TSS is loaded.
pub fn init() {
    {
        let mut idt = IDT.lock();
        idt.divide_error.set_handler_fn(divide_error);
        idt.breakpoint.set_handler_fn(breakpoint);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        // SAFETY: `gdt::init` has loaded the TSS, so the IST slot is valid, and
        // no other handler uses it.
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

        // SAFETY: `IDT` lives in a `static` and is never moved, so the address
        // the CPU caches in the IDTR stays valid for the life of the kernel.
        unsafe { idt.load_unsafe() };
    }

    pic::init();
    pit::init();
    // Unmask the timer (IRQ0), the keyboard (IRQ1) and the cascade to the slave
    // PIC (IRQ2); everything else stays masked.
    pic::unmask(pic::master_lines(), 0x00);

    interrupts::enable();
}

/// Number of timer interrupts since boot.
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Enables or disables the timer handler's dot output.
///
/// Off by default, so a normal boot and the test harness stay quiet; the deadlock
/// test turns it on so the handler really competes for the serial lock.
pub fn set_timer_dots(enabled: bool) {
    TIMER_DOTS.store(enabled, Ordering::Relaxed);
}

/// Whether the timer handler is currently printing dots.
#[must_use]
pub fn timer_dots() -> bool {
    TIMER_DOTS.load(Ordering::Relaxed)
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

/// How many double faults have been handled since boot.
#[must_use]
pub fn double_fault_count() -> u64 {
    DOUBLE_FAULTS.load(Ordering::Relaxed)
}

/// Raises a breakpoint exception (`int3`).
pub fn trigger_breakpoint() {
    x86_64::instructions::interrupts::int3();
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

extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if TIMER_DOTS.load(Ordering::Relaxed) && tick % DOT_INTERVAL == 0 {
        serial_print!(".");
    }
    pic::end_of_interrupt(0);
}

extern "x86-interrupt" fn keyboard_interrupt(_frame: InterruptStackFrame) {
    // SAFETY: port 0x60 is the PS/2 data port.
    let scancode = unsafe { Port::<u8>::new(0x60).read() };
    // Post-12: the handler only pushes the raw scancode and wakes the async
    // decoder; the decoding itself happens in the executor's keyboard task.
    crate::task::keyboard::add_scancode(scancode);
    pic::end_of_interrupt(1);
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    record(Vector::DivideError, &frame);
    halt_forever();
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    record(Vector::InvalidOpcode, &frame);
    halt_forever();
}

/// A breakpoint is recoverable: record it and return to the interrupted code.
extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    record(Vector::Breakpoint, &frame);
}

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
