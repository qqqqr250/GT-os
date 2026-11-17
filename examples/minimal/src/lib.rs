//! `blog_os` post-02, done properly: a minimal bare-metal kernel with a safe
//! VGA text console.
//!
//! The tutorial's post-02 kernel writes `"Hello World!"` to `0xb8000` with a raw
//! `*mut u8` and stops there. This crate keeps that kernel's size and spirit but
//! replaces the raw pointer with the safe, tested [`vga`] module:
//!
//! - the 80x25 grid, the cursor, the colour attribute and scrolling are owned by
//!   a single writer that never hands out a raw pointer,
//! - device memory is reached only through the [`volatile`] wrapper,
//! - all sixteen text colours are available, and a 24-bit colour can be snapped
//!   to the nearest one,
//! - the hardware cursor is programmed through the CRTC,
//! - the writer is unit-tested against a RAM buffer, so `cargo test` runs on the
//!   host with no QEMU and no hardware,
//! - the panic handler prints a real message instead of spinning silently.
//!
//! The library builds `no_std` for the kernel and as an ordinary `std` crate
//! under `cfg(test)`, which is what lets the tests run on the host. The bootable
//! binary lives in `src/main.rs`; see the crate `README.md` for how to build and
//! run it.

#![cfg_attr(not(test), no_std)]
// The in-QEMU breakpoint test installs a handler with `extern "x86-interrupt"`,
// which needs this feature; a normal build never enables it.
#![cfg_attr(feature = "qemu-test", feature(abi_x86_interrupt))]
// A freestanding kernel cannot avoid `unsafe`: port I/O and the memory-mapped
// text buffer need it. Every `unsafe` block carries a SAFETY comment, and the
// raw pointers are confined to `volatile` and `vga`.
#![allow(unsafe_code)]
// Screen coordinates, palette indices and packed colour channels are converted
// with `as`, where the truncation or wrap-around is intended and checked by
// construction.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

pub mod console;
pub mod vga;
pub mod volatile;

mod port;
mod serial;

pub use bootloader_api::BootInfo;

/// Formats to the console (COM1 + the VGA text buffer).
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::print_args(format_args!($($arg)*))
    };
}

/// Formats to the console, appending a newline.
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::print_args(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::console::print_args(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Stops the CPU until the next interrupt, forever.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: `hlt` is always legal in ring 0 and is only left by an
        // interrupt.
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Hooks for the in-QEMU integration test (`tests/qemu.rs`).
///
/// Only compiled when the `qemu-test` feature is on, so a normal build and the
/// host unit tests never see it. The module is the only surface the bare-metal
/// test binary needs: the console and writer are already public.
#[cfg(feature = "qemu-test")]
pub mod testing {
    /// Exit codes understood by QEMU's `isa-debug-exit` device.
    ///
    /// QEMU reports `(value << 1) | 1` to the host, so `Success` (0x10) becomes
    /// the process exit code 33.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(u32)]
    pub enum QemuExitCode {
        /// Every check passed.
        Success = 0x10,
        /// A check failed.
        Failed = 0x11,
    }

    /// Initializes COM1 so the test's output reaches the host.
    pub fn init_serial() {
        crate::serial::init();
    }

    /// Powers the machine off through the `isa-debug-exit` device.
    ///
    /// The runner always passes `-device isa-debug-exit,iobase=0xf4,iosize=0x04`.
    pub fn exit_qemu(exit_code: QemuExitCode) -> ! {
        // SAFETY: port 0xf4 belongs to the `isa-debug-exit` device the test
        // runner attaches; writing it terminates the emulator with
        // `(code << 1) | 1`.
        unsafe {
            crate::port::outl(crate::port::DEBUG_EXIT, exit_code as u32);
        }
        crate::halt_forever()
    }

    // -----------------------------------------------------------------------
    // post-05: the interrupt descriptor table (see examples/cpu-exceptions)
    // -----------------------------------------------------------------------
    //
    // The rest of this crate has no IDT, so a CPU exception is a triple fault.
    // The breakpoint test installs just enough of one to prove the trap path
    // works: an `int3` must be handled and must return.

    use core::sync::atomic::{AtomicU64, Ordering};

    use x86_64::instructions::interrupts;
    use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

    /// How many breakpoints the handler below has seen.
    static BREAKPOINTS: AtomicU64 = AtomicU64::new(0);

    /// The IDT the breakpoint test loads. `InterruptDescriptorTable::new()` is a
    /// `const fn`, so the table can live in a `static` and be filled in here.
    static BREAKPOINT_IDT: spin::Mutex<InterruptDescriptorTable> =
        spin::Mutex::new(InterruptDescriptorTable::new());

    /// Counts the breakpoint and returns, so the interrupted code resumes.
    extern "x86-interrupt" fn count_breakpoint(_frame: InterruptStackFrame) {
        BREAKPOINTS.fetch_add(1, Ordering::Relaxed);
    }

    /// Installs a breakpoint handler, raises `int3`, and returns how many
    /// breakpoints the handler has seen.
    ///
    /// This is post-05 in miniature: without an IDT the `int3` below would be a
    /// triple fault. The handler only counts, so the caller can assert that the
    /// CPU really trapped *and* that it resumed afterwards.
    pub fn breakpoint_round_trip() -> u64 {
        {
            let mut idt = BREAKPOINT_IDT.lock();
            idt.breakpoint.set_handler_fn(count_breakpoint);
            // SAFETY: `BREAKPOINT_IDT` lives in a `static` and is never moved, so
            // the address the CPU caches in the IDTR stays valid. `load` would
            // need a `&'static` reference the mutex guard cannot hand out.
            unsafe { idt.load_unsafe() };
        }

        interrupts::int3();
        BREAKPOINTS.load(Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // post-06: the GDT, the TSS and the interrupt stack table
    // -----------------------------------------------------------------------
    //
    // The rest of this crate has no GDT of its own — it runs on the one the
    // bootloader loaded — so a stack-overflow double fault would still triple
    // fault. These hooks install a GDT with a TSS whose interrupt stack table
    // names a dedicated double-fault stack, exactly as `examples/double-faults`
    // does for post-06.

    use x86_64::VirtAddr;
    use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
    use x86_64::instructions::tables::load_tss;
    use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
    use x86_64::structures::tss::TaskStateSegment;

    /// The IST slot the double-fault handler uses.
    ///
    /// The software index runs `0..=6` and maps onto the hardware IST's `1..=7`
    /// (the `x86_64` crate's `set_stack_index` adds the one).
    pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

    /// Size of the dedicated double-fault stack.
    const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

    /// A stack with the 16-byte alignment the ABI expects.
    ///
    /// The array is never read or written by Rust: once the TSS names its top
    /// the CPU owns it, pushing the double-fault frame itself.
    #[repr(align(16))]
    #[allow(dead_code)]
    struct DoubleFaultStack([u8; DOUBLE_FAULT_STACK_SIZE]);

    /// The stack the CPU switches to on a double fault.
    ///
    /// `static mut` on purpose: an immutable `static` would land in a read-only
    /// section, and a read-only stack faults.
    static mut DOUBLE_FAULT_STACK: DoubleFaultStack =
        DoubleFaultStack([0; DOUBLE_FAULT_STACK_SIZE]);

    /// The task state segment; only its interrupt stack table matters here.
    static mut TSS: TaskStateSegment = TaskStateSegment::new();

    /// The table itself, built in place by [`init_gdt`].
    static GDT: spin::Mutex<GlobalDescriptorTable> = spin::Mutex::new(GlobalDescriptorTable::new());

    /// Builds the TSS and the GDT and loads both.
    ///
    /// Must run before the double-fault IDT entry is installed: that entry names
    /// [`DOUBLE_FAULT_IST_INDEX`], which only resolves once the TSS is loaded.
    pub fn init_gdt() {
        // SAFETY: single-threaded boot, before any exception can fire, and both
        // the TSS and the stack are statics, so the addresses written stay valid.
        let stack_top = unsafe {
            let stack = core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK).cast::<u8>();
            stack.add(DOUBLE_FAULT_STACK_SIZE) as u64
        };
        // SAFETY: as above; the bootloader's own stack is left untouched.
        unsafe {
            let tss = core::ptr::addr_of_mut!(TSS);
            (*tss).interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)] =
                VirtAddr::new(stack_top);
        }

        let mut gdt = GDT.lock();
        let code = gdt.append(Descriptor::kernel_code_segment());
        let data = gdt.append(Descriptor::kernel_data_segment());
        // SAFETY: the TSS lives in a static, so the `&'static` reference it needs
        // is valid for the whole program.
        let tss = gdt.append(Descriptor::tss_segment(unsafe {
            &*core::ptr::addr_of!(TSS)
        }));
        // SAFETY: `GDT` lives in a static and is never moved, so the address the
        // CPU caches stays valid; `load` would need a `&'static` the guard cannot
        // give.
        unsafe { gdt.load_unsafe() };
        drop(gdt);

        // SAFETY: the selectors name the descriptors appended just above and the
        // TSS is a static. Reloading the data-segment registers avoids leaving
        // them pointing at stale selectors from the bootloader's GDT.
        unsafe {
            CS::set_reg(code);
            SS::set_reg(data);
            DS::set_reg(data);
            ES::set_reg(data);
            FS::set_reg(data);
            GS::set_reg(data);
            load_tss(tss);
        }
    }

    /// The top of the double-fault stack the TSS names, or 0 before [`init_gdt`].
    #[must_use]
    pub fn double_fault_stack_top() -> u64 {
        // SAFETY: reading a `static` TSS is sound; boot is single-threaded and
        // the table is only written by `init_gdt`.
        unsafe {
            (*core::ptr::addr_of!(TSS)).interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)]
                .as_u64()
        }
    }

    /// Recurses until the kernel stack hits the guard page the bootloader placed
    /// below it.
    ///
    /// The page fault cannot be delivered on the exhausted stack, so the CPU
    /// raises a double fault and switches to the IST stack — which is the point
    /// of post-06. Returns only if the guard page was somehow never hit.
    pub fn overflow_stack() {
        #[allow(unconditional_recursion)]
        fn recurse() {
            // A local whose address is taken, so every call really reserves a
            // frame.
            let marker = 0u8;
            recurse();
            // A volatile read the optimizer may not remove: it keeps the
            // recursive call above from being rewritten as a loop (tail-call
            // elimination), so the stack keeps growing instead of staying flat.
            // SAFETY: reading a live local through a raw pointer is always sound.
            unsafe { core::ptr::read_volatile(&raw const marker) };
        }

        recurse();
    }
}

/// The kernel entry point, called by the bootloader (see `src/main.rs`).
pub fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    serial::init();

    let offset = boot_info.physical_memory_offset.into_option();
    if let Some(offset) = offset {
        vga::init(offset);
    }
    console::clear_screen();

    console::banner(offset);
    console::palette();

    // Read the "Hello, World!" row straight back out of the text buffer. This is
    // the safe counterpart to the raw pointer write: the same buffer the writer
    // owns can be inspected cell by cell.
    console::set_fg(vga::Color::LightGray);
    crate::print!("read back row 4: '");
    for column in 0..13 {
        let cell = vga::read_screen_char(4, column);
        let ch = if (0x20..0x7f).contains(&cell.ascii) {
            char::from(cell.ascii)
        } else {
            '.'
        };
        crate::print!("{ch}");
    }
    crate::println!(
        "'  (attribute byte 0x{:02x})",
        vga::read_screen_char(4, 0).color.raw()
    );
    if vga::read_screen_char(4, 0).ascii == b'H' {
        crate::println!("the buffer is backed: the write and the read agree.");
    } else {
        crate::println!("the buffer is not backed here: the bootloader switched the display");
        crate::println!("to a linear framebuffer, so 0xb8000 is not the text aperture.");
        crate::println!("A real text-mode console would show the banner above.");
    }
    crate::println!();

    crate::println!("The writer owns the cursor, the colour attribute and scrolling.");
    crate::println!("Printing more than 25 lines makes the screen scroll:");
    for line in 1..=28 {
        crate::println!("  line {line:02}");
    }
    crate::println!();
    crate::println!("^ the screen scrolled; the top lines are gone.");
    crate::println!();

    console::set_fg(vga::Color::LightGreen);
    crate::println!("done. halting the CPU.");
    console::set_fg(vga::Color::LightGray);

    halt_forever()
}

/// The kernel panic handler, referenced by the binary's `#[panic_handler]`.
pub fn panic_handler(info: &core::panic::PanicInfo<'_>) -> ! {
    // A red-on-white banner is impossible to miss on a text screen.
    vga::set_color(vga::ColorCode::new(vga::Color::White, vga::Color::Red));
    crate::println!("\n*** PANIC ***\n{info}");
    halt_forever()
}
