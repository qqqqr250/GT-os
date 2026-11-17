//! Ring-3 user mode.
//!
//! `RustOS` runs programs with real hardware isolation. The kernel owns the GDT,
//! a TSS with a dedicated ring-0 stack, and a private page-table window at
//! [`USER_BASE`] that is the only memory a user program can reach. The single
//! system-call gate is `int 0x80`.
//!
//! The `global_asm!` block below defines three entry points:
//!
//! * `enter_user_mode` saves the kernel stack pointer and `iretq`s to ring 3,
//! * `syscall_interrupt` saves the user registers, calls the Rust dispatcher and
//!   `iretq`s back,
//! * `exit_user` restores the saved kernel stack and returns to [`run`].
//!
//! System calls use the usual argument registers: the number in `rax` and up to
//! three arguments in `rdi`, `rsi` and `rdx`, with the result returned in `rax`.

use alloc::vec::Vec;
use core::alloc::Layout;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::{PhysAddr, VirtAddr};

use crate::console::println;
use crate::interrupts;

/// The virtual page-table slot user programs live in.
///
/// The kernel occupies the lower half's first PML4 entry (its identity map) and
/// the bootloader's physical-memory window, so entry 1 (`0x0000_0080_...`) is
/// free for user space.
pub const USER_BASE: u64 = 0x0000_0080_0000_0000;
/// Program space: 64 KiB, so a flat binary may be up to [`MAX_PROGRAM_BYTES`].
const CODE_PAGES: u64 = 16;
/// Stack space: 16 KiB.
const STACK_PAGES: u64 = 4;
const PAGE_BYTES: u64 = 4096;
const STACK_BASE: u64 = USER_BASE + CODE_PAGES * PAGE_BYTES;
const STACK_TOP: u64 = STACK_BASE + STACK_PAGES * PAGE_BYTES;

/// The largest flat binary [`run`] accepts.
pub const MAX_PROGRAM_BYTES: usize = (CODE_PAGES * PAGE_BYTES) as usize;

/// Selectors written into the GDT, in `append` order. The assembly hard-codes
/// them, so [`init`] asserts that `append` really produced these values.
const KERNEL_CODE_SEL: u16 = 0x08;
const KERNEL_DATA_SEL: u16 = 0x10;
const USER_CODE_SEL: u16 = 0x18 | 3;
const USER_DATA_SEL: u16 = 0x20 | 3;

const KERNEL_STACK_BYTES: usize = 16 * 1024;
/// Size of the dedicated double-fault stack (see [`DOUBLE_FAULT_IST_INDEX`]).
const DOUBLE_FAULT_STACK_BYTES: usize = 16 * 1024;
const SYSCALL_VECTOR: u8 = 0x80;

/// The interrupt stack table slot the double-fault handler runs on.
///
/// The software index runs `0..=6` and maps onto the hardware IST's `1..=7` (the
/// `x86_64` crate's `set_stack_index` adds the one). Any slot would do; 0 is the
/// first. [`init_gdt`] names the stack and `interrupts::init` points the
/// double-fault IDT entry here, so a stack overflow is caught instead of
/// triple-faulting.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// `exit(status)`.
const SYS_EXIT: u64 = 0;
/// `write(fd, buffer, length)`.
const SYS_WRITE: u64 = 1;

static GDT: Mutex<GlobalDescriptorTable> = Mutex::new(GlobalDescriptorTable::new());

/// The task state segment. `privilege_stack_table[0]` is the stack the CPU
/// switches to on a ring-3 to ring-0 transition; `interrupt_stack_table` holds
/// the stacks named by an IDT entry's IST index.
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// The ring-0 stack used for interrupts and syscalls raised from user mode.
static mut KERNEL_STACK: [u8; KERNEL_STACK_BYTES] = [0; KERNEL_STACK_BYTES];

/// A stack with the 16-byte alignment the ABI expects.
///
/// The array is never read or written by Rust: once the TSS names its top the
/// CPU owns it, pushing the double-fault frame itself.
#[repr(align(16))]
#[allow(dead_code)]
struct DoubleFaultStack([u8; DOUBLE_FAULT_STACK_BYTES]);

/// The stack the CPU switches to on a double fault.
///
/// `static mut` on purpose: an immutable `static` would land in a read-only
/// section, and a read-only stack faults. Unlike the kernel stack it has no
/// guard page, but the handler only reports and halts, so a few pages are
/// plenty.
static mut DOUBLE_FAULT_STACK: DoubleFaultStack = DoubleFaultStack([0; DOUBLE_FAULT_STACK_BYTES]);

/// Whether [`init_gdt`] has built the table.
///
/// It is idempotent because both `kernel_main` and [`init`] call it, and
/// appending the descriptors twice would shift every selector.
static GDT_READY: AtomicBool = AtomicBool::new(false);

/// Where `enter_user_mode` parks the kernel stack pointer.
static KERNEL_RSP: AtomicU64 = AtomicU64::new(0);
/// The kernel's `RFLAGS` at the moment user mode was entered.
///
/// `exit_user` does not return through `iretq` (the exit syscall unwinds to a
/// saved kernel stack instead), so it must restore the interrupt flag by hand:
/// `int 0x80` is an interrupt gate and leaves `IF` cleared.
static KERNEL_RFLAGS: AtomicU64 = AtomicU64::new(0);
/// The value passed to the last `exit` syscall.
static EXIT_CODE: AtomicU64 = AtomicU64::new(0);
/// The bootloader's physical-memory offset, stored by [`init`].
static OFFSET: AtomicU64 = AtomicU64::new(0);
/// Whether the user page window has been mapped.
static MAPPED: AtomicBool = AtomicBool::new(false);

global_asm!(
    ".global enter_user_mode",
    ".global syscall_interrupt",
    ".global exit_user",
    "",
    // void enter_user_mode(u64 entry, u64 stack_top)
    "enter_user_mode:",
    // Save the callee-saved registers the SysV ABI promises to preserve, so the
    // compiler's view of `run` is intact when `exit_user` returns into it.
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rip + {kernel_rsp}], rsp",
    // Remember the kernel's interrupt flag: `exit_user` returns with `ret`, not
    // `iretq`, so it cannot rely on the CPU to restore `RFLAGS`.
    "    pushfq",
    "    pop rax",
    "    mov [rip + {kernel_rflags}], rax",
    // Switch to the user data segment.
    "    mov ax, {user_data}",
    "    mov ds, ax",
    "    mov es, ax",
    "    mov fs, ax",
    "    mov gs, ax",
    // Build the iretq frame: SS, RSP, RFLAGS (IF set), CS, RIP.
    "    push {user_data}",
    "    push rsi",
    "    push 0x202",
    "    push {user_code}",
    "    push rdi",
    "    iretq",
    "",
    // void syscall_interrupt()   (IDT vector 0x80, DPL 3)
    "syscall_interrupt:",
    // The CPU has already pushed SS, RSP, RFLAGS, CS and RIP. Save everything
    // else in a fixed layout so the dispatcher can read the arguments. The last
    // register pushed sits at `[rsp]`, so `rax` ends up lowest.
    "    push r15",
    "    push r14",
    "    push r13",
    "    push r12",
    "    push rbp",
    "    push rbx",
    "    push r11",
    "    push r10",
    "    push r9",
    "    push r8",
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push rcx",
    "    push rax",
    "    mov rbp, rsp",
    "    and rsp, -16",
    // syscall_dispatch(rax, rdi, rsi, rdx)
    "    mov rdi, [rbp]",
    "    mov rsi, [rbp + 32]",
    "    mov rdx, [rbp + 24]",
    "    mov rcx, [rbp + 16]",
    "    call {dispatch}",
    "    mov rsp, rbp",
    "    mov [rsp], rax",
    "    pop rax",
    "    pop rcx",
    "    pop rdx",
    "    pop rsi",
    "    pop rdi",
    "    pop r8",
    "    pop r9",
    "    pop r10",
    "    pop r11",
    "    pop rbx",
    "    pop rbp",
    "    pop r12",
    "    pop r13",
    "    pop r14",
    "    pop r15",
    "    iretq",
    "",
    // void exit_user(u64 code) -> !
    "exit_user:",
    "    mov [rip + {exit_code}], rdi",
    "    mov ax, {kernel_data}",
    "    mov ds, ax",
    "    mov es, ax",
    "    mov fs, ax",
    "    mov gs, ax",
    "    mov rsp, [rip + {kernel_rsp}]",
    // Restore the kernel's `RFLAGS` (notably `IF`) before unwinding the saved
    // registers, so the shell resumes with device interrupts live again.
    "    push qword ptr [rip + {kernel_rflags}]",
    "    popfq",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    ret",
    kernel_rsp = sym KERNEL_RSP,
    kernel_rflags = sym KERNEL_RFLAGS,
    exit_code = sym EXIT_CODE,
    dispatch = sym syscall_dispatch,
    user_data = const USER_DATA_SEL,
    user_code = const USER_CODE_SEL,
    kernel_data = const KERNEL_DATA_SEL,
);

unsafe extern "C" {
    /// Enters ring 3 at `entry` with `stack_top` as the user stack.
    fn enter_user_mode(entry: u64, stack_top: u64);
    /// Terminates the running program and returns to [`run`].
    fn exit_user(code: u64) -> !;
    /// The `int 0x80` entry point.
    fn syscall_interrupt();
}

/// Installs the GDT and the TSS.
///
/// The TSS gets both stacks: the ring-0 privilege stack the CPU switches to on
/// a ring-3 to ring-0 transition, and the double-fault stack named by
/// [`DOUBLE_FAULT_IST_INDEX`].
///
/// Must run *before* [`crate::interrupts::init`], which points the double-fault
/// IDT entry at that IST slot: until the TSS is loaded the CPU has no stack for
/// it. Idempotent — the table is built once, so it is safe to call from both
/// `kernel_main` and [`init`].
pub fn init_gdt() {
    // Build the table only once: `init` also calls this, and appending the
    // descriptors twice would shift every selector.
    if GDT_READY.swap(true, Ordering::SeqCst) {
        return;
    }

    // SAFETY: `KERNEL_STACK` and `DOUBLE_FAULT_STACK` are static arrays;
    // one-past-the-end is a valid address to form and is used as a
    // downward-growing stack top.
    let (kernel_stack_top, double_fault_stack_top) = unsafe {
        let kernel = core::ptr::addr_of_mut!(KERNEL_STACK).cast::<u8>();
        let double_fault = core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK).cast::<u8>();
        (
            kernel.add(KERNEL_STACK_BYTES) as u64,
            double_fault.add(DOUBLE_FAULT_STACK_BYTES) as u64,
        )
    };
    // SAFETY: single-threaded boot, before any user program exists and before
    // any exception can fire; the TSS is a static, so the addresses stay valid.
    unsafe {
        let tss = core::ptr::addr_of_mut!(TSS);
        (*tss).privilege_stack_table[0] = VirtAddr::new(kernel_stack_top);
        (*tss).interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)] =
            VirtAddr::new(double_fault_stack_top);
    }

    let mut gdt = GDT.lock();
    let kernel_code = gdt.append(Descriptor::kernel_code_segment());
    let kernel_data = gdt.append(Descriptor::kernel_data_segment());
    let user_code = gdt.append(Descriptor::user_code_segment());
    let user_data = gdt.append(Descriptor::user_data_segment());
    // SAFETY: the TSS lives in a static, so the `&'static` reference it needs is
    // valid for the whole program.
    let tss_selector = gdt.append(Descriptor::tss_segment(unsafe {
        &*core::ptr::addr_of!(TSS)
    }));
    // SAFETY: the GDT lives in a static and is never modified again.
    unsafe { gdt.load_unsafe() };
    drop(gdt);

    debug_assert_eq!(kernel_code.0, KERNEL_CODE_SEL);
    debug_assert_eq!(kernel_data.0, KERNEL_DATA_SEL);
    debug_assert_eq!(user_code.0, USER_CODE_SEL);
    debug_assert_eq!(user_data.0, USER_DATA_SEL);

    // SAFETY: the selectors name our own descriptors and the TSS is static.
    unsafe {
        use x86_64::instructions::segmentation::{CS, DS, ES, FS, GS, SS, Segment};
        use x86_64::instructions::tables::load_tss;

        CS::set_reg(kernel_code);
        SS::set_reg(kernel_data);
        DS::set_reg(kernel_data);
        ES::set_reg(kernel_data);
        FS::set_reg(kernel_data);
        GS::set_reg(kernel_data);
        load_tss(tss_selector);
    }
}

/// Installs the `int 0x80` gate and records the physical-memory offset.
///
/// Calls [`init_gdt`] first, so it is a complete "user mode is ready" call; the
/// GDT is built once even if `kernel_main` already loaded it early.
///
/// Must be called once, after the interrupt controller is up and before the
/// first [`run`].
pub fn init(physical_memory_offset: u64) {
    OFFSET.store(physical_memory_offset, Ordering::Relaxed);
    init_gdt();

    let handler: unsafe extern "C" fn() = syscall_interrupt;
    let handler = handler as *const () as u64;
    // SAFETY: `syscall_interrupt` saves and restores the user registers and
    // returns with `iretq`, which is what the IDT entry expects.
    unsafe { interrupts::set_user_gate(SYSCALL_VECTOR, handler) };
}

/// The top of the double-fault stack the TSS names, or 0 before [`init_gdt`].
#[must_use]
pub fn double_fault_stack_top() -> u64 {
    // SAFETY: reading a `static` TSS is sound; boot is single-threaded and the
    // table is only written by `init_gdt`.
    unsafe {
        (*core::ptr::addr_of!(TSS)).interrupt_stack_table[usize::from(DOUBLE_FAULT_IST_INDEX)]
            .as_u64()
    }
}

/// Whether [`init`] has run and user programs can be started.
pub fn is_ready() -> bool {
    OFFSET.load(Ordering::Relaxed) != 0
}

/// Terminates the running user program from outside the syscall path.
///
/// The exception handlers call this when a fault originates in ring 3, so a
/// crashing program returns to the shell instead of halting the whole machine.
pub fn abort(status: i64) -> ! {
    // SAFETY: `exit_user` records the status, restores the kernel stack saved by
    // `enter_user_mode` and returns into `run`, exactly as a normal `exit`
    // syscall would.
    unsafe { exit_user(status as u64) }
}

/// Loads `program` into the user window and runs it to completion.
///
/// Returns the status the program passed to `exit`, or `-1` if it could not be
/// loaded. The program is a flat binary linked against [`USER_BASE`].
pub fn run(program: &[u8]) -> i64 {
    if !is_ready() {
        return -1;
    }
    if let Err(error) = ensure_mapped() {
        println!("cannot map user memory: {error}");
        return -1;
    }

    let capacity = MAX_PROGRAM_BYTES;
    let length = program.len().min(capacity);
    // SAFETY: the user window is mapped writable for the kernel and spans
    // exactly `CODE_PAGES` pages of program space followed by `STACK_PAGES`
    // pages of stack.
    unsafe {
        core::ptr::write_bytes(USER_BASE as *mut u8, 0, capacity);
        core::ptr::write_bytes(
            STACK_BASE as *mut u8,
            0,
            (STACK_PAGES * PAGE_BYTES) as usize,
        );
        core::ptr::copy_nonoverlapping(program.as_ptr(), USER_BASE as *mut u8, length);
    }

    // SAFETY: the GDT, TSS and user pages are installed. `enter_user_mode` does
    // not return; control comes back only through `exit_user`, which restores
    // the kernel stack and returns here.
    unsafe { enter_user_mode(USER_BASE, STACK_TOP) };

    EXIT_CODE.load(Ordering::Relaxed) as i64
}

/// A hand-assembled ring-3 program: `write(1, message, len); exit(0)`.
///
/// This is the same binary that ships on the data disk as `HELLO.BIN`, built in
/// memory so `run` works even without a disk.
pub fn demo_program() -> Vec<u8> {
    const MESSAGE: &[u8] = b"Hello from ring 3! (via int 0x80)\r\n";
    const MESSAGE_OFFSET: usize = 36;

    let mut program = Vec::with_capacity(MESSAGE_OFFSET + MESSAGE.len());
    // mov eax, SYS_WRITE
    program.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00]);
    // mov edi, 1
    program.extend_from_slice(&[0xbf, 0x01, 0x00, 0x00, 0x00]);
    // movabs rsi, USER_BASE + MESSAGE_OFFSET
    program.push(0x48);
    program.push(0xbe);
    program.extend_from_slice(&(USER_BASE + MESSAGE_OFFSET as u64).to_le_bytes());
    // mov edx, MESSAGE.len()
    program.push(0xba);
    program.extend_from_slice(&(MESSAGE.len() as u32).to_le_bytes());
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    // mov eax, SYS_EXIT
    program.extend_from_slice(&[0xb8, 0x00, 0x00, 0x00, 0x00]);
    // xor edi, edi
    program.extend_from_slice(&[0x31, 0xff]);
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    debug_assert_eq!(program.len(), MESSAGE_OFFSET);
    program.extend_from_slice(MESSAGE);
    program
}

/// Maps the user program and stack pages on first use.
fn ensure_mapped() -> Result<(), &'static str> {
    if MAPPED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let offset = VirtAddr::new(OFFSET.load(Ordering::Relaxed));
    let (pml4_frame, _) = Cr3::read();
    let pml4_physical = pml4_frame.start_address().as_u64();
    let pml4_virtual = offset.as_u64() + pml4_physical;
    // SAFETY: the PML4 frame is the current address space's root; it is
    // reachable through the bootloader's physical-memory window and lives for
    // as long as the kernel does.
    let pml4: &'static mut PageTable = unsafe { &mut *(pml4_virtual as *mut PageTable) };
    // SAFETY: the offset maps every physical frame and this PML4 is the only
    // reference to the table.
    let mut mapper = unsafe { OffsetPageTable::new(pml4, offset) };
    let mut frames = HeapFrames {
        offset: offset.as_u64(),
    };

    for index in 0..CODE_PAGES + STACK_PAGES {
        let address = USER_BASE + index * PAGE_BYTES;
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(address));
        let frame = frames.allocate_frame().ok_or("out of physical frames")?;
        let mut flags =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        if address >= STACK_BASE {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        // SAFETY: the page is not mapped yet and the frame is freshly allocated,
        // so it cannot alias anything the kernel owns.
        unsafe {
            mapper
                .map_to(page, frame, flags, &mut frames)
                .map_err(|_| "map_to failed")?
                .flush();
        }
    }
    Ok(())
}

/// Hands out page-aligned physical frames carved from the kernel heap.
///
/// The kernel's heap already sits in the bootloader's physical-memory window,
/// so a heap allocation's physical address is just its virtual address minus the
/// window offset.
struct HeapFrames {
    offset: u64,
}

// SAFETY: every call allocates a fresh page from the heap, so no frame is ever
// handed out twice.
unsafe impl FrameAllocator<Size4KiB> for HeapFrames {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        let layout = Layout::from_size_align(PAGE_BYTES as usize, PAGE_BYTES as usize).ok()?;
        // SAFETY: the layout has a non-zero size.
        let pointer = unsafe { alloc::alloc::alloc(layout) };
        if pointer.is_null() {
            return None;
        }
        let physical = (pointer as u64).checked_sub(self.offset)?;
        Some(PhysFrame::containing_address(PhysAddr::new(physical)))
    }
}

/// The `int 0x80` dispatcher, called from `syscall_interrupt`.
extern "C" fn syscall_dispatch(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    match number {
        // SAFETY: `exit_user` restores the kernel stack saved by
        // `enter_user_mode`; it never returns to this frame.
        SYS_EXIT => unsafe { exit_user(arg0) },
        SYS_WRITE => sys_write(arg0, arg1, arg2),
        _ => u64::MAX,
    }
}

/// `write(fd, buffer, length)`: writes to the console on fd 1 or 2.
fn sys_write(fd: u64, buffer: u64, length: u64) -> u64 {
    if fd != 1 && fd != 2 {
        return u64::MAX;
    }
    let end = buffer.saturating_add(length);
    if buffer < USER_BASE || end > STACK_TOP {
        return u64::MAX;
    }
    let Ok(length) = usize::try_from(length) else {
        return u64::MAX;
    };
    // SAFETY: the range was just checked to lie inside the user window, which is
    // mapped and readable for the whole run.
    let bytes = unsafe { core::slice::from_raw_parts(buffer as *const u8, length) };
    crate::console::write_bytes(bytes);
    length as u64
}
