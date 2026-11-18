//! Interrupt descriptor table, 8259 PIC remapping, the PIT tick, and the
//! device interrupt handlers that drive the keyboard and COM1 input.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::{PrivilegeLevel, VirtAddr};

use crate::{console, input, mouse, serial};

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

pub fn init() {
    unsafe { remap_pic() };

    {
        let mut idt = IDT.lock();
        idt.divide_error.set_handler_fn(divide_error);
        idt.invalid_opcode.set_handler_fn(invalid_opcode);
        idt.breakpoint.set_handler_fn(breakpoint);
        // SAFETY: `usermode::init_gdt` has loaded the TSS, so the IST slot is
        // valid, and no other handler uses it.
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault)
                .set_stack_index(crate::usermode::DOUBLE_FAULT_IST_INDEX);
        }
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault);
        idt.page_fault.set_handler_fn(page_fault);
        idt[PIC1_OFFSET].set_handler_fn(timer_interrupt);
        idt[PIC1_OFFSET + 1].set_handler_fn(keyboard_interrupt);
        idt[PIC1_OFFSET + 4].set_handler_fn(serial_interrupt);
        idt[PIC2_OFFSET + 4].set_handler_fn(mouse_interrupt);
        // The IDT lives in a static and is never moved, so loading it through
        // the unsafe API is sound; using `load` would require `&'static` which
        // the mutex guard cannot provide.
        unsafe { idt.load_unsafe() };
    }

    unsafe {
        let divisor = PIT_FREQUENCY / TICK_HZ;
        outb(0x43, 0x36); // channel 0, lo/hi byte, square wave generator
        outb(0x40, (divisor & 0xff) as u8);
        outb(0x40, ((divisor >> 8) & 0xff) as u8);

        // Unmask IRQ0 (timer), IRQ1 (keyboard), IRQ2 (slave PIC cascade),
        // IRQ4 (COM1) on the master PIC, and IRQ12 (mouse) on the slave PIC.
        // In the 8259 IMR a set bit *masks* the line.
        let unmasked: u8 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 4);
        outb(PIC1_DATA, !unmasked);
        // Slave PIC: unmask IRQ12 (slave line 4).
        outb(PIC2_DATA, !(1 << 4));
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

extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    let tick = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    // A test turns this on to make the handler print through the serial lock,
    // which is what proves the `without_interrupts` fix holds.
    if TIMER_DOTS.load(Ordering::Relaxed) && tick % DOT_INTERVAL == 0 {
        serial::print_args(format_args!("."));
    }
    end_of_interrupt(0);
}

extern "x86-interrupt" fn keyboard_interrupt(_frame: InterruptStackFrame) {
    let scancode = unsafe { Port::<u8>::new(0x60).read() };
    // Post-12: the handler only queues the raw scancode and wakes the decoding
    // task; the task turns it into input on the main thread.
    crate::task::keyboard::add_scancode(scancode);
    end_of_interrupt(1);
}

extern "x86-interrupt" fn serial_interrupt(_frame: InterruptStackFrame) {
    // Read directly from the UART here: taking the serial mutex in interrupt
    // context could deadlock against a print in progress on the main thread.
    while let Some(byte) = serial::read_byte_irq() {
        input::push(byte);
        input::note_serial();
    }
    end_of_interrupt(4);
}

extern "x86-interrupt" fn mouse_interrupt(_frame: InterruptStackFrame) {
    let byte: u8 = unsafe { Port::<u8>::new(0x60).read() };
    mouse::handle_byte(byte);
    end_of_interrupt(12);
}

fn report_exception(
    name: &str,
    frame: &InterruptStackFrame,
    detail: core::fmt::Arguments<'_>,
) -> ! {
    // A fault raised in ring 3 is contained: report it and terminate the user
    // program, handing control back to the shell that launched it.
    if frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
        crate::console::panic_print(format_args!(
            "\n[user-mode {name}] {detail}\n  rip: {:#018x}\n  program terminated\n",
            frame.instruction_pointer.as_u64(),
        ));
        crate::usermode::abort(-1);
    }

    console::panic_print(format_args!(
        "\n*** CPU EXCEPTION: {name} ***\n  {detail}\n  rip: {:#018x}\n  rsp: {:#018x}\n",
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64(),
    ));
    halt_forever();
}

extern "x86-interrupt" fn divide_error(frame: InterruptStackFrame) {
    report_exception(
        "divide error",
        &frame,
        format_args!("the CPU tried to divide by zero"),
    );
}

extern "x86-interrupt" fn invalid_opcode(frame: InterruptStackFrame) {
    report_exception(
        "invalid opcode",
        &frame,
        format_args!("the CPU hit an undefined instruction"),
    );
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
    report_exception(
        "double fault",
        &frame,
        format_args!("error code: {error_code:#x}"),
    )
}

extern "x86-interrupt" fn general_protection_fault(frame: InterruptStackFrame, error_code: u64) {
    report_exception(
        "general protection fault",
        &frame,
        format_args!("error code: {error_code:#x}"),
    )
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    let address = Cr2::read().map_or(0, x86_64::VirtAddr::as_u64);
    report_exception(
        "page fault",
        &frame,
        format_args!("address: {address:#018x}, error: {error_code:?}"),
    )
}
