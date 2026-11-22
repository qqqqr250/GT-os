//! Interrupt descriptor table, 8259 PIC remapping, the PIT tick, and the
//! device interrupt handlers that drive the keyboard and COM1 input.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::{PrivilegeLevel, VirtAddr};

use crate::{console, input, serial};

/// Frequency of the programmable interval timer interrupt.
pub const TICK_HZ: u32 = 100;

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;
const PIC1_OFFSET: u8 = 0x20;
const PIC2_OFFSET: u8 = 0x28;
const PIC_EOI: u8 = 0x20;
const PIT_FREQUENCY: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// TSC value when the PIT was programmed.
static TSC_ANCHOR: AtomicU64 = AtomicU64::new(0);
/// TSC value at the most recent PIT tick.
static LAST_TICK_TSC: AtomicU64 = AtomicU64::new(0);
/// Number of TSC ticks in one PIT period, measured on the first tick.
static TSC_PER_TICK: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds in one PIT period.
const NS_PER_TICK: u64 = 1_000_000_000 / (TICK_HZ as u64);

/// How often the timer handler prints a dot when [`TIMER_DOTS`] is on.
const DOT_INTERVAL: u64 = 10;

/// Whether the timer handler prints a dot to COM1 every [`DOT_INTERVAL`] ticks.
///
/// Off by default so a normal boot stays quiet. It can be turned on to verify
/// that the handler really does print through the serial lock while the main
/// thread prints too — the case the `without_interrupts` fix in `serial` covers.
static TIMER_DOTS: AtomicBool = AtomicBool::new(false);

/// Number of breakpoint (`#BP`) exceptions handled since boot.
///
/// A breakpoint is recoverable, so this counts how many times the handler ran
/// and returned; the count goes up after an `int3`.
static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

static IDT: Mutex<InterruptDescriptorTable> = Mutex::new(InterruptDescriptorTable::new());

unsafe fn outb(port: u16, value: u8) {
    unsafe { Port::<u8>::new(port).write(value) };
}

fn halt_forever() -> ! {
    loop {
        interrupts::disable();
        x86_64::instructions::hlt();
    }
}

/// Reads the CPU's timestamp counter.
fn read_tsc() -> u64 {
    // SAFETY: `rdtsc` only reads a CPU counter and has no memory side effects.
    unsafe {
        let low: u32;
        let high: u32;
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack));
        (u64::from(high) << 32) | u64::from(low)
    }
}

pub fn init() {
    unsafe { remap_pic() };

    {
        let mut idt = IDT.lock();
        // SAFETY: fault stubs build syscall-shaped frames and never return.
        unsafe {
            idt.divide_error
                .set_handler_addr(crate::usermode::fault_divide_error_addr());
            idt.invalid_opcode
                .set_handler_addr(crate::usermode::fault_invalid_opcode_addr());
        }
        idt.breakpoint.set_handler_fn(breakpoint);
        // SAFETY: `usermode::init_gdt` has loaded the TSS, so the IST slot is
        // valid, and no other handler uses it.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault)
                .set_stack_index(crate::usermode::DOUBLE_FAULT_IST_INDEX);
        }
        unsafe {
            idt.general_protection_fault
                .set_handler_addr(crate::usermode::fault_general_protection_addr());
            idt.page_fault
                .set_handler_addr(crate::usermode::fault_page_addr());
        }
        // SAFETY: `timer_interrupt` is the assembly entry in `usermode.rs`. It
        // builds the same frame as `syscall_interrupt`, acknowledges the PIC and
        // either returns with `iretq` or switches to another process.
        unsafe {
            idt[PIC1_OFFSET]
                .set_handler_addr(crate::usermode::timer_interrupt_addr())
                .disable_interrupts(true);
        }
        idt[PIC1_OFFSET + 1].set_handler_fn(keyboard_interrupt);
        idt[PIC1_OFFSET + 4].set_handler_fn(serial_interrupt);
        idt[PIC2_OFFSET + 4].set_handler_fn(mouse_interrupt);
        // The IDT lives in a static and is never moved, so loading it through
        // the unsafe API is sound; using `load` would require `&'static` which
        // the mutex guard cannot provide.
        unsafe { idt.load_unsafe() };
    }

    // Anchor the interpolated clock just before the first PIT tick can fire.
    let anchor = read_tsc();
    TSC_ANCHOR.store(anchor, Ordering::Relaxed);
    LAST_TICK_TSC.store(anchor, Ordering::Relaxed);

    unsafe {
        let divisor = PIT_FREQUENCY / TICK_HZ;
        outb(0x43, 0x36); // channel 0, lo/hi byte, square wave generator
        outb(0x40, (divisor & 0xff) as u8);
        outb(0x40, ((divisor >> 8) & 0xff) as u8);

        // Unmask IRQ0 (timer), IRQ1 (keyboard), IRQ2 (slave PIC cascade) and
        // IRQ4 (COM1) on the master PIC. On the slave, unmask IRQ4, which is
        // the aux mouse line (IRQ12). In the 8259 IMR a set bit *masks* the line.
        let unmasked: u8 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 4);
        outb(PIC1_DATA, !unmasked);
        outb(PIC2_DATA, !(1u8 << 4));
    }

    interrupts::enable();
}

/// Number of timer interrupts since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Turns the timer handler's dot printing on or off (see [`TIMER_DOTS`]).
pub fn set_timer_dots(enabled: bool) {
    TIMER_DOTS.store(enabled, Ordering::Relaxed);
}

/// Number of breakpoint (`#BP`) exceptions handled since boot.
pub fn breakpoints() -> u64 {
    BREAKPOINTS.load(Ordering::Relaxed)
}

pub fn uptime_seconds() -> u64 {
    ticks() / u64::from(TICK_HZ)
}

/// Returns a monotonic nanosecond count since the PIT was initialized.
///
/// The PIT supplies the coarse 10 ms count; the TSC interpolates within the
/// current tick. It is calibrated against the first PIT period, so no CPU
/// frequency needs to be known in advance.
#[must_use]
pub fn monotonic_ns() -> u64 {
    let tick = ticks();
    let coarse = tick.saturating_mul(NS_PER_TICK);
    let per_tick = TSC_PER_TICK.load(Ordering::Relaxed);
    if per_tick == 0 {
        return coarse;
    }
    // Clamp the interpolation to one period: a tick may land between reading
    // `ticks()` and `read_tsc()`.
    let elapsed = read_tsc()
        .saturating_sub(LAST_TICK_TSC.load(Ordering::Relaxed))
        .min(per_tick);
    coarse.saturating_add(elapsed.saturating_mul(NS_PER_TICK) / per_tick)
}

/// Installs `handler` on `vector` as a gate that ring-3 code may invoke.
///
/// `set_handler_fn` cannot be used for the syscall entry point because the
/// handler is written in assembly, not as an `extern "x86-interrupt"` function.
///
/// # Safety
///
/// `handler` must be the address of a valid interrupt handler that preserves the
/// interrupted register state and returns with `iretq`.
pub unsafe fn set_user_gate(vector: u8, handler: u64) {
    let mut idt = IDT.lock();
    // SAFETY: the caller guarantees `handler` is a valid entry point; the IDT
    // lives in a static, so it stays mapped after `load_unsafe`.
    unsafe {
        idt[vector]
            .set_handler_addr(VirtAddr::new(handler))
            .set_privilege_level(PrivilegeLevel::Ring3);
    }
}

unsafe fn remap_pic() {
    // ICW1: start initialization, expect ICW4.
    unsafe { outb(PIC1_COMMAND, 0x11) };
    io_wait();
    unsafe { outb(PIC2_COMMAND, 0x11) };
    io_wait();
    // ICW2: vector offsets.
    unsafe { outb(PIC1_DATA, PIC1_OFFSET) };
    io_wait();
    unsafe { outb(PIC2_DATA, PIC2_OFFSET) };
    io_wait();
    // ICW3: wiring of the two PICs.
    unsafe { outb(PIC1_DATA, 0x04) };
    io_wait();
    unsafe { outb(PIC2_DATA, 0x02) };
    io_wait();
    // ICW4: 8086 mode.
    unsafe { outb(PIC1_DATA, 0x01) };
    io_wait();
    unsafe { outb(PIC2_DATA, 0x01) };
    io_wait();

    // Start with everything masked; `init` unmasks what is actually used.
    unsafe {
        outb(PIC1_DATA, 0xff);
        outb(PIC2_DATA, 0xff);
    }
}

fn io_wait() {
    unsafe { outb(0x80, 0x00) };
}

fn end_of_interrupt(irq: u8) {
    if irq >= 8 {
        unsafe { outb(PIC2_COMMAND, PIC_EOI) };
    }
    unsafe { outb(PIC1_COMMAND, PIC_EOI) };
}

/// Called by the assembly PIT handler with a syscall-shaped frame when the tick
/// interrupted ring 3, or with a null frame when it interrupted ring 0.
///
/// # Safety
///
/// `frame` must either be null or point at the 20-slot register frame built by
/// `usermode::timer_interrupt`. The caller must not return to user mode if
/// [`crate::scheduler::preempt`] switches away, because it resumes with `iretq`.
pub(crate) extern "C" fn timer_tick(frame: *mut u64) {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let tsc = read_tsc();
    if tick == 1 {
        // The first interrupt follows one full PIT period, so its TSC delta is
        // the calibration factor used to interpolate later ticks.
        let delta = tsc.saturating_sub(TSC_ANCHOR.load(Ordering::Relaxed));
        if delta > 0 {
            TSC_PER_TICK.store(delta, Ordering::Relaxed);
        }
    }
    LAST_TICK_TSC.store(tsc, Ordering::Relaxed);
    // A test turns this on to make the handler print through the serial lock,
    // which is what proves the `without_interrupts` fix holds.
    if TIMER_DOTS.load(Ordering::Relaxed) && tick % DOT_INTERVAL == 0 {
        serial::print_args(format_args!("."));
    }
    end_of_interrupt(0);
    crate::process::wake_sleepers(tick);
    if !frame.is_null() {
        // SAFETY: the frame was built by `timer_interrupt` and is live on the
        // current process's private kernel stack.
        unsafe {
            if let Some(status) = crate::usermode::deliver_pending_signal(frame) {
                crate::usermode::terminate_current(frame, status);
            }
            crate::scheduler::preempt(frame);
        }
    }
}

extern "x86-interrupt" fn keyboard_interrupt(_frame: InterruptStackFrame) {
    let scancode = unsafe { Port::<u8>::new(0x60).read() };
    // Post-12: the handler only queues the raw scancode and wakes the decoding
    // task; the task turns it into input on the main thread.
    crate::task::keyboard::add_scancode(scancode);
    end_of_interrupt(1);
}

extern "x86-interrupt" fn mouse_interrupt(_frame: InterruptStackFrame) {
    // The 8042 shares data port 0x60 with the keyboard, but the source of this
    // interrupt is the auxiliary device, so the byte belongs to the mouse.
    let byte = unsafe { Port::<u8>::new(0x60).read() };
    crate::mouse::handle_byte(byte);
    // IRQ12 comes from the slave PIC, so `end_of_interrupt` acknowledges both.
    end_of_interrupt(12);
}

extern "x86-interrupt" fn serial_interrupt(_frame: InterruptStackFrame) {
    // Read directly from the UART here: taking the serial mutex in interrupt
    // context could deadlock against a print in progress on the main thread.
    while let Some(byte) = serial::read_byte_irq() {
        input::push_serial(byte);
        input::note_serial();
    }
    end_of_interrupt(4);
}

fn fault_from_user(frame: *const u64) -> bool {
    // SAFETY: fault stubs build a 20-slot frame: CPU CS is slot 16.
    let cs = unsafe { *frame.add(16) };
    (cs & 0b11) == 3
}

fn fault_rip(frame: *const u64) -> u64 {
    // SAFETY: slot 15 is RIP in the syscall/timer/fault frame layout.
    unsafe { *frame.add(15) }
}

fn fault_rsp(frame: *const u64) -> u64 {
    // SAFETY: slot 18 is RSP in the syscall/timer/fault frame layout.
    unsafe { *frame.add(18) }
}

fn handle_fault(frame: *mut u64, name: &str, signum: u8, detail: core::fmt::Arguments<'_>) -> ! {
    let rip = fault_rip(frame);
    let rsp = fault_rsp(frame);
    if fault_from_user(frame) {
        crate::console::panic_print(format_args!(
            "\n[user-mode {name}] {detail}\n  rip: {rip:#018x}\n  signal: {signum}\n",
        ));
        crate::usermode::handle_user_fault(frame, signum);
    }

    console::panic_print(format_args!(
        "\n*** CPU EXCEPTION: {name} ***\n  {detail}\n  rip: {rip:#018x}\n  rsp: {rsp:#018x}\n",
    ));
    halt_forever();
}

/// Assembly fault stubs call these with a syscall-shaped frame.
pub(crate) extern "C" fn divide_error_fault(frame: *mut u64, _error: u64) -> ! {
    handle_fault(
        frame,
        "divide error",
        crate::signal::SIGFPE,
        format_args!("the CPU tried to divide by zero"),
    )
}

pub(crate) extern "C" fn invalid_opcode_fault(frame: *mut u64, _error: u64) -> ! {
    handle_fault(
        frame,
        "invalid opcode",
        crate::signal::SIGILL,
        format_args!("the CPU hit an undefined instruction"),
    )
}

extern "x86-interrupt" fn breakpoint(frame: InterruptStackFrame) {
    // A breakpoint is recoverable: report it and resume.
    BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
    console::panic_print(format_args!(
        "\n[breakpoint] rip: {:#018x}\n",
        frame.instruction_pointer.as_u64()
    ));
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    console::panic_print(format_args!(
        "\n*** CPU EXCEPTION: double fault ***\n  error code: {error_code:#x}\n  rip: {:#018x}\n  rsp: {:#018x}\n",
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64(),
    ));
    halt_forever();
}

pub(crate) extern "C" fn general_protection_fault_handler(frame: *mut u64, error_code: u64) -> ! {
    handle_fault(
        frame,
        "general protection fault",
        crate::signal::SIGBUS,
        format_args!("error code: {error_code:#x}"),
    )
}

pub(crate) extern "C" fn page_fault_handler(frame: *mut u64, error_code: u64) -> ! {
    let address = Cr2::read().map_or(0, x86_64::VirtAddr::as_u64);
    let error = PageFaultErrorCode::from_bits_truncate(error_code);
    let from_user = fault_from_user(frame);
    if from_user
        && error.contains(PageFaultErrorCode::CAUSED_BY_WRITE)
        && crate::usermode::handle_cow_fault(address)
    {
        // SAFETY: the fault frame is live and the kernel has resolved the COW page.
        unsafe { crate::usermode::resume_fault_frame(frame) }
    }
    // User page faults map to SIGSEGV; protection/key faults are reported as
    // SIGSEGV too because GTos does not model bus-fault address classes yet.
    let signum = crate::signal::SIGSEGV;
    handle_fault(
        frame,
        "page fault",
        signum,
        format_args!("address: {address:#018x}, error: {error:?}"),
    )
}
