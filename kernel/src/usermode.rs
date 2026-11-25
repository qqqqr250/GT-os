//! Ring-3 user mode.
//!
//! `GTos` runs programs with real hardware isolation. The kernel owns the GDT,
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

use alloc::string::String;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bootloader_api::info::PixelFormat;
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::control::Cr3;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable};
use x86_64::structures::tss::TaskStateSegment;

use crate::address_space::AddressSpace;
use crate::console::println;
use crate::interrupts;
use crate::scheduler;

/// The virtual page-table slot user programs live in.
///
/// The bootloader identity-maps PML4 entries 0 through 5 at boot, so user space
/// starts in entry 6 (`0x0000_0300_...`). That keeps the kernel's identity map,
/// heap window and physical-memory window intact when CR3 is reloaded.
pub const USER_BASE: u64 = 0x0000_0300_0000_0000;
/// Program space: 64 KiB, so a flat binary may be up to [`MAX_PROGRAM_BYTES`].
const CODE_PAGES: u64 = 16;
/// Stack space: 16 KiB.
const STACK_PAGES: u64 = 4;
const PAGE_BYTES: u64 = 4096;
const STACK_BASE: u64 = USER_BASE + CODE_PAGES * PAGE_BYTES;
const STACK_TOP: u64 = STACK_BASE + STACK_PAGES * PAGE_BYTES;

/// End of the address range syscalls accept as user pointers. ELF images, their
/// heaps, shared-memory slots and stacks all live below this limit.
pub const USER_REGION_END: u64 = USER_BASE + 0x0100_0000;
/// Upper bound for ELF segments and `brk` growth.
///
/// Shared-memory slots start here and grow upward, while ELF stacks occupy the
/// top 64 KiB of the user region. Keeping the heap below this line lets both
/// features coexist without checking every allocation for an overlap.
pub const USER_BRK_END: u64 = USER_BASE + 0x0040_0000;

/// Number of `u64` slots in the live syscall/timer register frame.
const SYSCALL_FRAME_SLOTS: usize = 20;
/// Bytes of register state saved for a signal handler and restored by `sigreturn`.
const SIGNAL_FRAME_BYTES: u64 = (SYSCALL_FRAME_SLOTS * 8) as u64;
/// Bytes of `Siginfo` written after the saved frame for signal handlers.
const SIGINFO_BYTES: u64 = 40;

/// One entry in the stable `poll` array shared with `gtos-sys`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: u64,
    events: u16,
    revents: u16,
}

const _: () = assert!(core::mem::size_of::<PollFd>() == 16);
/// Upper bound on one `poll` call, keeping the syscall bounded in ring 0.
const MAX_POLL_FDS: usize = 64;

/// Argument block for `select`, passed by pointer because the syscall ABI has
/// only three argument registers.
///
/// Each fd set is sixteen `u64` words, so descriptor numbers `0..1024` are
/// selectable. A null pointer means the caller passed no set for that category.
#[repr(C)]
#[derive(Clone, Copy)]
struct SelectArgs {
    nfds: u64,
    readfds: u64,
    writefds: u64,
    exceptfds: u64,
    timeout_ms: u64,
}

const _: () = assert!(core::mem::size_of::<SelectArgs>() == 40);
/// Number of `u64` words in one `select` fd set.
const SELECT_FD_WORDS: usize = 16;
/// Number of descriptor bits in one `select` fd set.
const SELECT_FD_SETSIZE: usize = SELECT_FD_WORDS * 64;
/// Bytes occupied by one `select` fd set in userspace.
const SELECT_SET_BYTES: u64 = (SELECT_FD_WORDS * 8) as u64;

/// One validated `select` fd set: its userspace address and requested bits.
struct SelectSet {
    address: usize,
    bits: [u64; SELECT_FD_WORDS],
}

/// One atomically sampled `select` result: three output sets, a ready count and
/// the tick at which the sample was taken.
type SelectScan = (
    [u64; SELECT_FD_WORDS],
    [u64; SELECT_FD_WORDS],
    [u64; SELECT_FD_WORDS],
    usize,
    u64,
);

/// The largest flat binary [`run`] accepts.
pub const MAX_PROGRAM_BYTES: usize = (CODE_PAGES * PAGE_BYTES) as usize;
/// Upper bound on an ELF image passed to [`run`] or the `exec` syscall.
pub const MAX_ELF_BYTES: usize = 1024 * 1024;

/// Stable ABI for the framebuffer device exposed by `fbinfo`.
///
/// The explicit padding keeps the layout identical in the kernel and in
/// `guest/gtos-sys`, without depending on how a Rust enum is laid out.
#[repr(C)]
#[derive(Clone, Copy)]
struct FramebufferInfo {
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u32,
    pixel_format: u32,
    _reserved: u32,
    byte_len: u64,
}

/// Pixel rectangle requested by [`SYS_WINCREATE`].
#[repr(C)]
#[derive(Clone, Copy)]
struct WindowCreate {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// Larger layers paint on top; creation order breaks ties.
    layer: u32,
}

/// Subrectangle and source stride for [`SYS_WINCOMMIT_RECT`].
#[repr(C)]
#[derive(Clone, Copy)]
struct WindowDamage {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// Bytes between source rows; must be at least `width * bytes_per_pixel`.
    stride: u32,
}

/// Stable ABI for one message read from a window mailbox.
#[repr(C)]
#[derive(Clone, Copy)]
struct WindowMessage {
    sender: u64,
    length: u64,
    data: [u8; 64],
}

/// Selectors written into the GDT, in `append` order. The assembly hard-codes
/// them, so [`init`] asserts that `append` really produced these values.
const KERNEL_CODE_SEL: u16 = 0x08;
const KERNEL_DATA_SEL: u16 = 0x10;
pub(crate) const USER_CODE_SEL: u16 = 0x18 | 3;
pub(crate) const USER_DATA_SEL: u16 = 0x20 | 3;

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
/// `open(url, length)` -> scheme descriptor (>= 3) or `u64::MAX`.
const SYS_OPEN: u64 = 2;
/// `read(fd, buffer, length)` -> number of bytes read.
const SYS_READ: u64 = 3;
/// `close(fd)` -> 0 on success.
const SYS_CLOSE: u64 = 4;
/// `getpid()` -> the current process identifier.
const SYS_GETPID: u64 = 5;
/// `brk(address)` -> the new break address, or the current one when `address` is 0.
const SYS_BRK: u64 = 6;
/// `yield()` -> polls tasks and switches to the next runnable process.
const SYS_YIELD: u64 = 7;
/// `fork()` -> child pid in the parent, 0 in the child.
const SYS_FORK: u64 = 8;
/// `exec(image, length)` -> replaces the current process image.
const SYS_EXEC: u64 = 9;
/// `waitpid(pid, status, options)` -> pid when the child has exited.
const SYS_WAITPID: u64 = 10;
/// `sleep(milliseconds)` -> 0 after the deadline, yielding while peers run.
const SYS_SLEEP: u64 = 11;
/// `uptime()` -> milliseconds since the PIT was initialized.
const SYS_UPTIME: u64 = 12;
/// `clock_gettime()` -> monotonic nanoseconds since the PIT was initialized.
const SYS_CLOCK_GETTIME: u64 = 13;
/// `realtime()` -> Unix seconds from the CMOS RTC, or `u64::MAX`.
const SYS_REALTIME: u64 = 14;
/// `lseek(fd, offset, whence)` -> the new absolute offset, or `u64::MAX`.
const SYS_LSEEK: u64 = 15;
/// `signal(signum, handler)` -> the previous handler, or `u64::MAX`.
const SYS_SIGNAL: u64 = 16;
/// `kill(pid, signum, info)` -> 0 on success, or `u64::MAX`.
const SYS_KILL: u64 = 17;
/// `sigreturn(frame)` -> restores the frame saved before signal delivery.
const SYS_SIGRETURN: u64 = 18;
/// `fbinfo(info)` -> writes [`FramebufferInfo`] to a user buffer.
const SYS_FBINFO: u64 = 19;
/// `fbcommit(buffer, length)` -> blits a user shadow buffer to the screen.
const SYS_FBCOMMIT: u64 = 20;
/// `input(event)` -> 1 when an input event was written, 0 when none is queued.
const SYS_INPUT: u64 = 21;
/// `wincreate(description)` -> a window identifier.
const SYS_WINCREATE: u64 = 22;
/// `wincommit(id, buffer, length)` -> bytes copied into the window.
const SYS_WINCOMMIT: u64 = 23;
/// `winpresent()` -> blits the composited shadow frame to the display.
const SYS_WINPRESENT: u64 = 24;
/// `windestroy(id)` -> releases one window.
const SYS_WINDESTROY: u64 = 25;
/// `wincommitrect(id, buffer, damage)` -> updates one window subrectangle.
const SYS_WINCOMMIT_RECT: u64 = 26;
/// `winsetlayer(id, layer)` -> changes a window's stacking layer.
const SYS_WINSETLAYER: u64 = 27;
/// `winpost(id, buffer, length)` -> queue a message for a window.
const SYS_WINPOST: u64 = 28;
/// `winread(id, message, capacity)` -> read one window message, or 0 if empty.
const SYS_WINREAD: u64 = 29;
/// `winsettitle(id, buffer, length)` -> set a window's title bar text.
const SYS_WINSETTITLE: u64 = 30;
/// `shmcreate(length)` -> reserve a shared-memory region.
const SYS_SHMCREATE: u64 = 31;
/// `shmattach(id)` -> map a shared region and return its base address.
const SYS_SHMATTACH: u64 = 32;
/// `shmdetach(id)` -> unmap a shared region from the caller.
const SYS_SHMDETACH: u64 = 33;
/// `shmdestroy(id)` -> retire a shared region owned by the caller.
const SYS_SHMDESTROY: u64 = 34;
/// `winbindshm(id, region)` -> use a shared region as a window's pixel store.
const SYS_WINBINDSHM: u64 = 35;
/// `pipe(fds)` -> write a read/write descriptor pair into a user array.
const SYS_PIPE: u64 = 36;
/// `schemeregister(name, length, notify_fd)` -> serve a scheme from ring 3.
const SYS_SCHEMEREGISTER: u64 = 37;
/// `schemeunregister(name, length)` -> stop serving a scheme.
const SYS_SCHEMEUNREGISTER: u64 = 38;
/// `stat(path, length, info)` -> write VFS metadata into a fixed-size struct.
const SYS_STAT: u64 = 39;
/// `readdir(path, length, buffer)` -> write NUL-separated child names.
const SYS_READDIR: u64 = 40;
/// `chdir(path, length)` -> change the caller's current directory.
const SYS_CHDIR: u64 = 41;
/// `getcwd(buffer, length)` -> write the caller's current directory.
const SYS_GETCWD: u64 = 42;
/// `shmbind(fd, region)` -> use a shared region as a stream's byte transport.
const SYS_SHMBIND: u64 = 43;
/// `ioctl(fd, request, arg)` -> query or adjust a descriptor.
const SYS_IOCTL: u64 = 44;
/// `sigprocmask(how, set, oldset)` -> adjust or query the 64-bit signal mask.
const SYS_SIGPROCMASK: u64 = 45;
/// `sigpending(set)` -> write the 64-bit pending signal set.
const SYS_SIGPENDING: u64 = 46;
/// `socketpair(domain, type, fds)` -> create a datagram socket pair.
const SYS_SOCKETPAIR: u64 = 47;
/// `dup(fd)` -> duplicate a descriptor.
const SYS_DUP: u64 = 48;
/// `dup2(oldfd, newfd)` -> duplicate a descriptor to a specific number.
const SYS_DUP2: u64 = 49;
/// `shutdown(fd, how)` -> shut down read/write sides of a socket/stream.
const SYS_SHUTDOWN: u64 = 50;
/// `fcntl(fd, cmd, arg)` -> get/set file status flags.
const SYS_FCNTL: u64 = 51;
/// `sigtimedwait(set, info, timeout_ms)` -> signum, `ERROR` on timeout.
const SYS_SIGTIMEDWAIT: u64 = 52;
/// `signalfd(mask, flags)` -> descriptor that reads pending signal records.
const SYS_SIGNALFD: u64 = 53;
/// `poll(fds, length, timeout_ms)` -> number of ready descriptors.
const SYS_POLL: u64 = 54;
/// `select(args)` -> number of ready bits across the three descriptor sets.
const SYS_SELECT: u64 = 55;

/// Creates a pipe with status and descriptor flags applied atomically.
const SYS_PIPE2: u64 = 56;
/// Make a signal descriptor's empty reads fail instead of blocking.
const SFD_NONBLOCK: u64 = 0x0800;
/// Close a signal descriptor on `exec`.
const SFD_CLOEXEC: u64 = 0x0008_0000;
/// Internal return value telling guest SDK wrappers to retry after a wakeup.
///
/// Signal waits, signal descriptor reads, `poll` and `select` all use this
/// private value because a context switch returns to the user syscall boundary
/// rather than to the kernel call stack.
pub(crate) const SIGWAIT_RETRY: u64 = u64::MAX - 1;

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
/// PID of the process currently executing in ring 3, or 0 when none is.
static CURRENT_PID: AtomicU64 = AtomicU64::new(0);

global_asm!(
    ".global enter_user_mode",
    ".global enter_user_mode_with_args",
    ".global syscall_interrupt",
    ".global timer_interrupt",
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
    // void enter_user_mode_with_args(u64 entry, u64 stack_top, u64 argc, u64 argv)
    "enter_user_mode_with_args:",
    // The prologue mirrors `enter_user_mode`; only the final argument registers
    // differ, so `exit_user` still unwinds to the same kernel frame.
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov [rip + {kernel_rsp}], rsp",
    "    pushfq",
    "    pop rax",
    "    mov [rip + {kernel_rflags}], rax",
    "    mov ax, {user_data}",
    "    mov ds, ax",
    "    mov es, ax",
    "    mov fs, ax",
    "    mov gs, ax",
    "    push {user_data}",
    "    push rsi",
    "    push 0x202",
    "    push {user_code}",
    "    push rdi",
    // SysV arguments: rdi=entry, rsi=stack_top, rdx=argc, rcx=argv.
    // `iretq` restores only the iretq frame, so these become the C ABI's first
    // two arguments in user mode.
    "    mov rdi, rdx",
    "    mov rsi, rcx",
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
    // Hand the raw frame to the dispatcher so scheduler syscalls can save it.
    "    mov r8, rbp",
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
    // PIT interrupt (IDT vector 0x20). The CPU pushed RIP/CS/RFLAGS and, for a
    // ring-3 interrupt, the user RSP/SS as well. Save the same 15 GPRs as the
    // syscall stub so the scheduler can reuse `switch_context`. Interrupts from
    // ring 0 pass a null frame: the tick still needs an EOI, but no preemption.
    "timer_interrupt:",
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
    // CS is the second slot of the CPU frame, 15 saved GPRs above `rbp`.
    "    test qword ptr [rbp + 128], 3",
    "    jz .Ltimer_ring0",
    "    mov rdi, rbp",
    "    call {timer_tick}",
    "    jmp .Ltimer_restore",
    ".Ltimer_ring0:",
    "    xor rdi, rdi",
    "    call {timer_tick}",
    ".Ltimer_restore:",
    "    mov rsp, rbp",
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
    // Fault handlers for ring-3-deliverable CPU exceptions. They build the same
    // 20-slot frame as syscall/timer interrupts so a signal handler can resume.
    "fault_divide_error:",
    "    push 0",
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
    "    mov rdi, rsp",
    "    mov rsi, [rsp + 120]",
    "    and rsp, -16",
    "    call {divide_error_fault}",
    "    ud2",
    "fault_invalid_opcode:",
    "    push 0",
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
    "    mov rdi, rsp",
    "    mov rsi, [rsp + 120]",
    "    and rsp, -16",
    "    call {invalid_opcode_fault}",
    "    ud2",
    "fault_general_protection:",
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
    "    mov rsi, [rsp + 120]", // CPU error code
    "    mov rcx, 15",
    "2:",
    "    mov r11, [rsp + rcx*8 - 8]",
    "    mov [rsp + rcx*8], r11",
    "    loop 2b",
    "    add rsp, 8", // remove error code slot, yielding a normal 20-slot frame
    "    mov rdi, rsp",
    "    and rsp, -16",
    "    sub rsp, 8", // keep SysV alignment before call after `and`
    "    call {general_protection_fault_handler}",
    "    ud2",
    "fault_page:",
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
    "    mov rsi, [rsp + 120]", // CPU error code
    "    mov rcx, 15",
    "3:",
    "    mov r11, [rsp + rcx*8 - 8]",
    "    mov [rsp + rcx*8], r11",
    "    loop 3b",
    "    add rsp, 8",
    "    mov rdi, rsp",
    "    and rsp, -16",
    "    sub rsp, 8",
    "    call {page_fault_handler}",
    "    ud2",
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
    divide_error_fault = sym crate::interrupts::divide_error_fault,
    invalid_opcode_fault = sym crate::interrupts::invalid_opcode_fault,
    general_protection_fault_handler = sym crate::interrupts::general_protection_fault_handler,
    page_fault_handler = sym crate::interrupts::page_fault_handler,
    kernel_rsp = sym KERNEL_RSP,
    kernel_rflags = sym KERNEL_RFLAGS,
    exit_code = sym EXIT_CODE,
    dispatch = sym syscall_dispatch,
    timer_tick = sym crate::interrupts::timer_tick,
    user_data = const USER_DATA_SEL,
    user_code = const USER_CODE_SEL,
    kernel_data = const KERNEL_DATA_SEL,
);

unsafe extern "C" {
    /// Enters ring 3 at `entry` with `stack_top` as the user stack.
    fn enter_user_mode(entry: u64, stack_top: u64);
    /// Enters ring 3 with a seeded C startup frame in `argc` and `argv`.
    fn enter_user_mode_with_args(entry: u64, stack_top: u64, argc: u64, argv: u64);
    /// Terminates the running program and returns to [`run`].
    fn exit_user(code: u64) -> !;
    /// The `int 0x80` entry point.
    fn syscall_interrupt();
    /// The PIT interrupt entry point.
    fn timer_interrupt();
    fn fault_divide_error();
    fn fault_invalid_opcode();
    fn fault_general_protection();
    fn fault_page();
}

/// Address of the assembly timer handler for the IDT.
pub(crate) fn timer_interrupt_addr() -> VirtAddr {
    VirtAddr::new(timer_interrupt as *const () as u64)
}

/// Addresses of fault stubs that build syscall-shaped frames for signal delivery.
pub(crate) fn fault_divide_error_addr() -> VirtAddr {
    VirtAddr::new(fault_divide_error as *const () as u64)
}

pub(crate) fn fault_invalid_opcode_addr() -> VirtAddr {
    VirtAddr::new(fault_invalid_opcode as *const () as u64)
}

pub(crate) fn fault_general_protection_addr() -> VirtAddr {
    VirtAddr::new(fault_general_protection as *const () as u64)
}

pub(crate) fn fault_page_addr() -> VirtAddr {
    VirtAddr::new(fault_page as *const () as u64)
}

/// Installs the GDT and the TSS.
///
/// The TSS gets both stacks: the ring-0 privilege stack the CPU switches to on
/// a ring-3 to ring-0 transition, and the double-fault stack named by
/// [`DOUBLE_FAULT_IST_INDEX`].
///
/// Must run *before* [`crate::interrupts::init`], which points the double-fault
/// IDT entry at that IST slot: until the TSS is loaded the CPU has no stack for
/// it. Idempotent - the table is built once, so it is safe to call from both
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
    if let Some(pid) = current_pid() {
        crate::process::mark_exited(pid, status);
    }
    EXIT_CODE.store(status as u64, Ordering::Relaxed);
    // SAFETY: `exit_user` restores the kernel stack saved by `enter_user_mode`
    // and returns into `run`, exactly as a normal `exit` syscall would. The
    // running process has already been marked exited.
    unsafe { exit_user(status as u64) }
}

/// Delivers a hardware-generated fault as a signal to the current ring-3 process.
///
/// The faulting interrupt frame has the same shape as a syscall frame: 15 GPRs
/// followed by the CPU `iretq` frame. If a handler is installed, this rewrites
/// the frame and returns to user mode; otherwise the default fatal disposition
/// terminates the process. A kernel-mode fault never uses this path.
pub(crate) fn handle_user_fault(frame: *mut u64, signum: u8) -> ! {
    let Some(pid) = current_pid() else {
        // SAFETY: no process is running; unwind to the shell as a last resort.
        unsafe { exit_user(u64::MAX) }
    };
    let fault_signum = signum;
    let selected = crate::process::with_process_mut(pid, |process| {
        let signals = process.signals_mut();
        if signals.in_handler() {
            return None;
        }
        signals.queue(fault_signum, crate::signal::Siginfo::fault(fault_signum, 0));
        signals.take_pending()
    })
    .flatten();
    let (signum, disposition, info) = selected.unwrap_or((
        fault_signum,
        crate::signal::Disposition::Terminate,
        crate::signal::Siginfo::fault(fault_signum, 0),
    ));

    match disposition {
        crate::signal::Disposition::Handler(handler) => {
            let _ = crate::process::with_process_mut(pid, |process| {
                process.signals_mut().set_in_handler(true);
            });
            if enter_signal_handler(frame, signum, handler, info) {
                // SAFETY: the handler frame is a valid syscall-shaped frame on the
                // current kernel stack, so `resume_context`/`iretq` can return to it.
                unsafe { resume_fault_frame(frame) }
            }
            let _ = crate::process::with_process_mut(pid, |process| {
                process.signals_mut().set_in_handler(false);
            });
            let status = -i64::from(signum);
            crate::process::mark_exited(pid, status);
            terminate_current(frame, status);
        }
        crate::signal::Disposition::Terminate => {
            let status = -i64::from(signum);
            crate::process::mark_exited(pid, status);
            terminate_current(frame, status);
        }
        crate::signal::Disposition::Ignore => {
            // Ignoring a synchronous fault would re-execute the same faulting
            // instruction forever; contain it as a fatal signal instead.
            let status = -i64::from(signum);
            crate::process::mark_exited(pid, status);
            terminate_current(frame, status);
        }
    }
}

/// Loads `program` into a new process and runs it to completion.
///
/// ELF64 binaries are mapped by [`crate::elf`]; other files are treated as the
/// legacy flat binaries linked against [`USER_BASE`]. `startup_args` is a packed
/// NUL-terminated argument block seeded onto the initial user stack. Returns the
/// status passed to `exit`, or `-1` if the program could not be loaded.
pub fn run(program: &[u8], startup_args: &[u8]) -> i64 {
    if !is_ready() {
        return -1;
    }
    // Keys typed at the shell prompt belong to the shell's history, not to the
    // guest being launched. Start every program with an empty event mirror.
    crate::input::clear_user();

    let mut process = match crate::process::Process::new() {
        Ok(process) => process,
        Err(error) => {
            println!("cannot create user process: {error}");
            return -1;
        }
    };
    let pid = process.pid();

    let is_flat = !program.starts_with(&[0x7f, b'E', b'L', b'F']);
    let (entry, stack_top, brk_start) = if is_flat {
        // Map the legacy code window and its stack in the new address space.
        for page in 0..CODE_PAGES + STACK_PAGES {
            let address = USER_BASE + page * PAGE_BYTES;
            let executable = page < CODE_PAGES;
            if let Err(error) = process
                .address_space_mut()
                .map_user_page(address, true, executable)
            {
                println!("cannot map user page: {error}");
                return -1;
            }
        }
        (USER_BASE, STACK_TOP, STACK_TOP)
    } else {
        match crate::elf::load(program, process.address_space_mut()) {
            Ok(image) => (image.entry, image.stack_top, image.brk_start),
            Err(error) => {
                println!("cannot load ELF program: {error}");
                return -1;
            }
        }
    };
    process.set_brk(brk_start);
    process.set_running();
    let kernel_stack_top = process.kernel_stack_top();

    let (original_pml4, original_flags) = Cr3::read();
    // Switch before copying the flat binary: its destination pages only exist in
    // the process's own page tables. The physical-memory window is copied from
    // the kernel PML4, so the kernel can still reach heap and page-table frames.
    unsafe { process.address_space().switch_to() };
    if is_flat {
        let capacity = MAX_PROGRAM_BYTES;
        let length = program.len().min(capacity);
        // SAFETY: the code and stack pages were mapped above; the writes stay
        // inside those windows.
        unsafe {
            core::ptr::write_bytes(USER_BASE as *mut u8, 0, capacity);
            core::ptr::write_bytes(
                STACK_BASE as *mut u8,
                0,
                (STACK_PAGES * PAGE_BYTES) as usize,
            );
            core::ptr::copy_nonoverlapping(program.as_ptr(), USER_BASE as *mut u8, length);
        }
    }

    let startup = match seed_startup_stack(process.address_space_mut(), stack_top, startup_args) {
        Ok(startup) => startup,
        Err(error) => {
            // SAFETY: `original_pml4` is the kernel table we were on before the
            // switch and it is still valid.
            unsafe { crate::address_space::restore(original_pml4, original_flags) };
            println!("cannot seed user arguments: {error}");
            return -1;
        }
    };

    if let Err(error) = crate::process::register(process) {
        // SAFETY: `original_pml4` is the kernel table we were on before the
        // switch and it is still valid.
        unsafe { crate::address_space::restore(original_pml4, original_flags) };
        println!("cannot register user process: {error}");
        return -1;
    }

    CURRENT_PID.store(pid.as_u64(), Ordering::SeqCst);
    // Ring-3 syscalls must land on this process's private kernel stack, not the
    // boot stack shared by the shell.
    // SAFETY: `kernel_stack_top` is owned by the registered process.
    unsafe { set_kernel_stack(kernel_stack_top) };
    // SAFETY: the GDT, TSS, user pages and process page tables are installed.
    // `enter_user_mode_with_args` does not return; control comes back through
    // `exit_user`, which restores the kernel stack and returns here. The last
    // process to exit (or a fault) is the one that unwinds into this call.
    unsafe {
        enter_user_mode_with_args(entry, startup.rsp, startup.argc, startup.argv);
        crate::address_space::restore(original_pml4, original_flags);
        restore_boot_kernel_stack();
    }
    CURRENT_PID.store(0, Ordering::SeqCst);
    let status = EXIT_CODE.load(Ordering::Relaxed) as i64;
    crate::process::mark_exited_if_live(pid, status);
    // The shell's `run` is synchronous: once every spawned process has exited,
    // drop the whole tree so the next `run` starts with an empty table.
    crate::process::clear();

    status
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

/// A ring-3 program that exercises the scheme syscalls.
///
/// It opens `sys:/version`, reads it into a user buffer, writes that buffer to
/// the console, closes the descriptor and exits. All addresses are absolute in
/// the user window because the flat binary is loaded at [`USER_BASE`].
pub fn scheme_demo_program() -> Vec<u8> {
    const PATH: &[u8] = b"sys:/version";
    const PATH_OFFSET: u64 = 128;
    const BUFFER_OFFSET: u64 = 160;
    const BUFFER_LEN: u32 = 256;

    let mut program = Vec::new();
    // mov eax, SYS_OPEN
    program.extend_from_slice(&[0xb8]);
    program.extend_from_slice(&(SYS_OPEN as u32).to_le_bytes());
    // mov rdi, USER_BASE + PATH_OFFSET
    program.extend_from_slice(&[0x48, 0xbf]);
    program.extend_from_slice(&(USER_BASE + PATH_OFFSET).to_le_bytes());
    // mov esi, PATH.len()
    program.push(0xbe);
    program.extend_from_slice(&(PATH.len() as u32).to_le_bytes());
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    // mov r12, rax -- the descriptor must survive the next syscall
    program.extend_from_slice(&[0x49, 0x89, 0xc4]);
    // mov eax, SYS_READ
    program.extend_from_slice(&[0xb8]);
    program.extend_from_slice(&(SYS_READ as u32).to_le_bytes());
    // mov rdi, r12
    program.extend_from_slice(&[0x4c, 0x89, 0xe7]);
    // mov rsi, USER_BASE + BUFFER_OFFSET
    program.extend_from_slice(&[0x48, 0xbe]);
    program.extend_from_slice(&(USER_BASE + BUFFER_OFFSET).to_le_bytes());
    // mov edx, BUFFER_LEN
    program.push(0xba);
    program.extend_from_slice(&BUFFER_LEN.to_le_bytes());
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    // mov r13, rax -- the byte count must survive the next syscall
    program.extend_from_slice(&[0x49, 0x89, 0xc5]);
    // mov eax, SYS_WRITE
    program.extend_from_slice(&[0xb8]);
    program.extend_from_slice(&(SYS_WRITE as u32).to_le_bytes());
    // mov edi, 1
    program.extend_from_slice(&[0xbf, 0x01, 0x00, 0x00, 0x00]);
    // mov rsi, USER_BASE + BUFFER_OFFSET
    program.extend_from_slice(&[0x48, 0xbe]);
    program.extend_from_slice(&(USER_BASE + BUFFER_OFFSET).to_le_bytes());
    // mov rdx, r13
    program.extend_from_slice(&[0x4c, 0x89, 0xea]);
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    // mov eax, SYS_CLOSE
    program.extend_from_slice(&[0xb8]);
    program.extend_from_slice(&(SYS_CLOSE as u32).to_le_bytes());
    // mov rdi, r12
    program.extend_from_slice(&[0x4c, 0x89, 0xe7]);
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    // mov eax, SYS_EXIT
    program.extend_from_slice(&[0xb8]);
    program.extend_from_slice(&(SYS_EXIT as u32).to_le_bytes());
    // xor edi, edi
    program.extend_from_slice(&[0x31, 0xff]);
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);

    debug_assert!(program.len() <= PATH_OFFSET as usize);
    program.resize(PATH_OFFSET as usize, 0);
    program.extend_from_slice(PATH);
    program.resize((BUFFER_OFFSET + u64::from(BUFFER_LEN)) as usize, 0);
    program
}

/// A ring-3 program that exercises `fork`, `yield` and `waitpid`.
///
/// The parent forks, then polls `waitpid` while yielding. The child yields once
/// before writing its line, so both context-switch directions are exercised.
pub fn fork_demo_program() -> Vec<u8> {
    const CHILD_MESSAGE: &[u8] = b"child: yielded and exited\r\n";
    const PARENT_MESSAGE: &[u8] = b"parent: child reaped\r\n";

    fn mov_eax(program: &mut Vec<u8>, value: u64) {
        program.push(0xb8);
        program.extend_from_slice(&(value as u32).to_le_bytes());
    }
    fn mov_imm32(program: &mut Vec<u8>, opcode: u8, value: u32) {
        program.push(opcode);
        program.extend_from_slice(&value.to_le_bytes());
    }
    fn int80(program: &mut Vec<u8>) {
        program.extend_from_slice(&[0xcd, 0x80]);
    }

    let mut program = Vec::new();

    // child_pid = fork()
    mov_eax(&mut program, SYS_FORK);
    int80(&mut program);
    program.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
    let jump_parent = program.len();
    program.extend_from_slice(&[0x75, 0x00]); // jnz parent (patched below)

    // Child: yield once, write, exit.
    mov_eax(&mut program, SYS_YIELD);
    int80(&mut program);
    mov_eax(&mut program, SYS_WRITE);
    mov_imm32(&mut program, 0xbf, 1); // mov edi, 1
    let child_message_pointer = program.len();
    program.extend_from_slice(&[0x48, 0xbe]); // movabs rsi,imm64 (patched below)
    program.extend_from_slice(&[0u8; 8]);
    mov_imm32(&mut program, 0xba, CHILD_MESSAGE.len() as u32); // mov edx,len
    int80(&mut program);
    mov_eax(&mut program, SYS_EXIT);
    program.extend_from_slice(&[0x31, 0xff]); // xor edi, edi
    int80(&mut program);

    let parent_label = program.len();
    program[jump_parent + 1] = (parent_label as isize - (jump_parent + 2) as isize) as i8 as u8;

    // Parent: poll waitpid, yielding to the child between attempts.
    program.extend_from_slice(&[0x49, 0x89, 0xc4]); // mov r12, rax
    let wait_loop = program.len();
    mov_eax(&mut program, SYS_YIELD);
    int80(&mut program);
    mov_eax(&mut program, SYS_WAITPID);
    program.extend_from_slice(&[0x4c, 0x89, 0xe7]); // mov rdi, r12
    program.extend_from_slice(&[0x31, 0xf6]); // xor esi, esi
    program.extend_from_slice(&[0x31, 0xd2]); // xor edx, edx
    int80(&mut program);
    program.extend_from_slice(&[0x48, 0x83, 0xf8, 0xff]); // cmp rax, -1
    let jump_wait = program.len();
    program.extend_from_slice(&[0x74, 0x00]); // je wait_loop (patched below)
    let after_jump = program.len();
    program[jump_wait + 1] = (wait_loop as isize - after_jump as isize) as i8 as u8;

    mov_eax(&mut program, SYS_WRITE);
    mov_imm32(&mut program, 0xbf, 1);
    let parent_message_pointer = program.len();
    program.extend_from_slice(&[0x48, 0xbe]);
    program.extend_from_slice(&[0u8; 8]);
    mov_imm32(&mut program, 0xba, PARENT_MESSAGE.len() as u32);
    int80(&mut program);
    mov_eax(&mut program, SYS_EXIT);
    program.extend_from_slice(&[0x31, 0xff]);
    int80(&mut program);

    let child_message_offset = program.len();
    program.extend_from_slice(CHILD_MESSAGE);
    let parent_message_offset = program.len();
    program.extend_from_slice(PARENT_MESSAGE);

    program[child_message_pointer + 2..child_message_pointer + 10]
        .copy_from_slice(&(USER_BASE + child_message_offset as u64).to_le_bytes());
    program[parent_message_pointer + 2..parent_message_pointer + 10]
        .copy_from_slice(&(USER_BASE + parent_message_offset as u64).to_le_bytes());
    program
}

/// A ring-3 program that proves the PIT preempts ring 3.
///
/// Parent and child each print a fixed number of their own letters without ever
/// calling `yield`. The letters interleave if timer interrupts switch between
/// them; without preemption one whole run would print before the other starts.
pub fn preempt_demo_program() -> Vec<u8> {
    const CHILD_CHAR: &[u8] = b"C";
    const PARENT_CHAR: &[u8] = b"P";
    const LETTERS: u64 = 200;

    fn mov_eax(program: &mut Vec<u8>, value: u64) {
        program.push(0xb8);
        program.extend_from_slice(&(value as u32).to_le_bytes());
    }
    fn mov_imm32(program: &mut Vec<u8>, opcode: u8, value: u32) {
        program.push(opcode);
        program.extend_from_slice(&value.to_le_bytes());
    }
    fn int80(program: &mut Vec<u8>) {
        program.extend_from_slice(&[0xcd, 0x80]);
    }
    fn write_loop(program: &mut Vec<u8>, count: u64) -> usize {
        program.extend_from_slice(&[0x49, 0xbc]); // mov r12, imm64
        program.extend_from_slice(&count.to_le_bytes());
        let start = program.len();
        mov_eax(program, SYS_WRITE);
        mov_imm32(program, 0xbf, 1); // mov edi, 1
        let pointer = program.len();
        program.extend_from_slice(&[0x48, 0xbe]); // movabs rsi, imm64
        program.extend_from_slice(&[0u8; 8]);
        mov_imm32(program, 0xba, 1); // mov edx, 1
        int80(program);
        program.extend_from_slice(&[0x49, 0xff, 0xcc]); // dec r12
        let rel = start as isize - (program.len() + 2) as isize;
        program.extend_from_slice(&[0x75, rel as i8 as u8]); // jnz start
        mov_eax(program, SYS_EXIT);
        program.extend_from_slice(&[0x31, 0xff]); // xor edi, edi
        int80(program);
        pointer
    }

    let mut program = Vec::new();

    mov_eax(&mut program, SYS_FORK);
    int80(&mut program);
    program.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
    let jump_parent = program.len();
    program.extend_from_slice(&[0x75, 0x00]); // jnz parent

    let child_pointer = write_loop(&mut program, LETTERS);

    let parent_label = program.len();
    program[jump_parent + 1] = (parent_label as isize - (jump_parent + 2) as isize) as i8 as u8;

    let parent_pointer = write_loop(&mut program, LETTERS);

    let child_offset = program.len();
    program.extend_from_slice(CHILD_CHAR);
    let parent_offset = program.len();
    program.extend_from_slice(PARENT_CHAR);

    program[child_pointer + 2..child_pointer + 10]
        .copy_from_slice(&(USER_BASE + child_offset as u64).to_le_bytes());
    program[parent_pointer + 2..parent_pointer + 10]
        .copy_from_slice(&(USER_BASE + parent_offset as u64).to_le_bytes());
    program
}

/// A ring-3 program that demonstrates timer-driven sleep.
///
/// The child sleeps longer than the parent. With blocking sleeps the parent's
/// message must appear first, even though the child is the branch that falls
/// through the `fork` test.
pub fn sleep_demo_program() -> Vec<u8> {
    const CHILD_MESSAGE: &[u8] = b"child: woke after sleep\r\n";
    const PARENT_MESSAGE: &[u8] = b"parent: woke after sleep\r\n";
    const CHILD_MS: u32 = 200;
    const PARENT_MS: u32 = 100;

    fn mov_eax(program: &mut Vec<u8>, value: u64) {
        program.push(0xb8);
        program.extend_from_slice(&(value as u32).to_le_bytes());
    }
    fn mov_imm32(program: &mut Vec<u8>, opcode: u8, value: u32) {
        program.push(opcode);
        program.extend_from_slice(&value.to_le_bytes());
    }
    fn int80(program: &mut Vec<u8>) {
        program.extend_from_slice(&[0xcd, 0x80]);
    }
    fn sleep_write_exit(program: &mut Vec<u8>, milliseconds: u32, message_len: usize) -> usize {
        mov_eax(program, SYS_SLEEP);
        mov_imm32(program, 0xbf, milliseconds);
        int80(program);
        mov_eax(program, SYS_WRITE);
        mov_imm32(program, 0xbf, 1);
        let pointer = program.len();
        program.extend_from_slice(&[0x48, 0xbe]); // movabs rsi, imm64
        program.extend_from_slice(&[0u8; 8]);
        mov_imm32(program, 0xba, message_len as u32);
        int80(program);
        mov_eax(program, SYS_EXIT);
        program.extend_from_slice(&[0x31, 0xff]); // xor edi, edi
        int80(program);
        pointer
    }

    let mut program = Vec::new();

    mov_eax(&mut program, SYS_FORK);
    int80(&mut program);
    program.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
    let jump_parent = program.len();
    program.extend_from_slice(&[0x75, 0x00]); // jnz parent

    let child_pointer = sleep_write_exit(&mut program, CHILD_MS, CHILD_MESSAGE.len());

    let parent_label = program.len();
    program[jump_parent + 1] = (parent_label as isize - (jump_parent + 2) as isize) as i8 as u8;

    let parent_pointer = sleep_write_exit(&mut program, PARENT_MS, PARENT_MESSAGE.len());

    let child_offset = program.len();
    program.extend_from_slice(CHILD_MESSAGE);
    let parent_offset = program.len();
    program.extend_from_slice(PARENT_MESSAGE);

    program[child_pointer + 2..child_pointer + 10]
        .copy_from_slice(&(USER_BASE + child_offset as u64).to_le_bytes());
    program[parent_pointer + 2..parent_pointer + 10]
        .copy_from_slice(&(USER_BASE + parent_offset as u64).to_le_bytes());
    program
}

/// A ring-3 program that embeds `demo_program` and replaces itself with it.
pub fn exec_demo_program() -> Vec<u8> {
    let payload = demo_program();
    let mut program = Vec::new();
    // mov eax, SYS_EXEC
    program.push(0xb8);
    program.extend_from_slice(&(SYS_EXEC as u32).to_le_bytes());
    // movabs rdi, USER_BASE + payload offset (patched below)
    program.extend_from_slice(&[0x48, 0xbf]);
    program.extend_from_slice(&[0u8; 8]);
    // mov esi, payload length
    program.push(0xbe);
    program.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    // xor edx, edx: no packed argument block for this built-in exec.
    program.extend_from_slice(&[0x31, 0xd2]);
    // int 0x80
    program.extend_from_slice(&[0xcd, 0x80]);
    // If exec failed: exit(0).
    program.push(0xb8);
    program.extend_from_slice(&(SYS_EXIT as u32).to_le_bytes());
    program.extend_from_slice(&[0x31, 0xff]);
    program.extend_from_slice(&[0xcd, 0x80]);

    let payload_offset = program.len();
    program[7..15].copy_from_slice(&(USER_BASE + payload_offset as u64).to_le_bytes());
    program.extend_from_slice(&payload);
    program
}

/// The `int 0x80` dispatcher, called from `syscall_interrupt`.
extern "C" fn syscall_dispatch(
    number: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    frame: *mut u64,
) -> u64 {
    // After `exec`, the previous address space stays alive until the new CR3 is
    // active. The first syscall in the new image is a safe point to free it.
    if let Some(pid) = current_pid() {
        crate::process::drop_retired(pid);
    }
    let result = match number {
        SYS_EXIT => {
            if let Some(pid) = current_pid() {
                crate::process::mark_exited(pid, arg0 as i64);
            }
            EXIT_CODE.store(arg0, Ordering::Relaxed);
            // If another process is ready, this call never returns: its saved
            // context is resumed directly from this interrupt frame. Only the
            // final exit falls through to `exit_user`.
            scheduler::exit_current(frame);
            // SAFETY: `exit_user` restores the kernel stack saved by
            // `enter_user_mode`; it never returns to this frame.
            unsafe { exit_user(arg0) }
        }
        SYS_WRITE => sys_write(arg0, arg1, arg2, frame),
        SYS_OPEN => sys_open(arg0, arg1, arg2),
        SYS_READ => sys_read(arg0, arg1, arg2, frame),
        SYS_CLOSE => sys_close(arg0, arg1, arg2),
        SYS_GETPID => current_pid().map_or(u64::MAX, crate::process::Pid::as_u64),
        SYS_BRK => sys_brk(arg0),
        SYS_YIELD => {
            // Yield still polls task state; the PIT now also preempts ring 3.
            crate::task::poll();
            scheduler::yield_now(frame)
        }
        SYS_FORK => sys_fork(frame),
        SYS_EXEC => sys_exec(arg0, arg1, arg2),
        SYS_WAITPID => sys_waitpid(arg0, arg1, arg2, frame),
        SYS_SLEEP => sys_sleep(arg0, frame),
        SYS_UPTIME => sys_uptime(),
        SYS_CLOCK_GETTIME => sys_clock_gettime(),
        SYS_REALTIME => sys_realtime(),
        SYS_LSEEK => sys_lseek(arg0, arg1, arg2),
        SYS_SIGNAL => sys_signal(arg0, arg1),
        SYS_KILL => sys_kill(arg0, arg1, arg2),
        SYS_SIGRETURN => sys_sigreturn(arg0, frame),
        SYS_FBINFO => sys_fbinfo(arg0),
        SYS_FBCOMMIT => sys_fbcommit(arg0, arg1),
        SYS_INPUT => sys_input(arg0),
        SYS_WINCREATE => sys_wincreate(arg0),
        SYS_WINCOMMIT => sys_wincommit(arg0, arg1, arg2),
        SYS_WINPRESENT => sys_winpresent(),
        SYS_WINDESTROY => sys_windestroy(arg0),
        SYS_WINCOMMIT_RECT => sys_wincommit_rect(arg0, arg1, arg2),
        SYS_WINSETLAYER => sys_winsetlayer(arg0, arg1),
        SYS_WINPOST => sys_winpost(arg0, arg1, arg2),
        SYS_WINREAD => sys_winread(arg0, arg1, arg2),
        SYS_WINSETTITLE => sys_winsettitle(arg0, arg1, arg2),
        SYS_SHMCREATE => sys_shmcreate(arg0),
        SYS_SHMATTACH => sys_shmattach(arg0),
        SYS_SHMDETACH => sys_shmdetach(arg0),
        SYS_SHMDESTROY => sys_shmdestroy(arg0),
        SYS_WINBINDSHM => sys_winbindshm(arg0, arg1),
        SYS_PIPE => sys_pipe(arg0),
        SYS_SCHEMEREGISTER => sys_schemeregister(arg0, arg1, arg2),
        SYS_SCHEMEUNREGISTER => sys_schemeunregister(arg0, arg1),
        SYS_STAT => sys_stat(arg0, arg1, arg2),
        SYS_READDIR => sys_readdir(arg0, arg1, arg2),
        SYS_CHDIR => sys_chdir(arg0, arg1),
        SYS_GETCWD => sys_getcwd(arg0, arg1),
        SYS_SHMBIND => sys_shmbind(arg0, arg1),
        SYS_IOCTL => sys_ioctl(arg0, arg1, arg2),
        SYS_SIGPROCMASK => sys_sigprocmask(arg0, arg1, arg2),
        SYS_SIGPENDING => sys_sigpending(arg0),
        SYS_SOCKETPAIR => sys_socketpair(arg0, arg1, arg2),
        SYS_DUP => sys_dup(arg0),
        SYS_DUP2 => sys_dup2(arg0, arg1),
        SYS_SHUTDOWN => sys_shutdown(arg0, arg1),
        SYS_FCNTL => sys_fcntl(arg0, arg1, arg2),
        SYS_SIGTIMEDWAIT => sys_sigtimedwait(arg0, arg1, arg2, frame),
        SYS_SIGNALFD => sys_signalfd(arg0, arg1),
        SYS_POLL => sys_poll(arg0, arg1, arg2, frame),
        SYS_SELECT => sys_select(arg0, frame),
        SYS_PIPE2 => sys_pipe2(arg0, arg1),
        _ => u64::MAX,
    };
    // Delivery is deliberately repeated after `sigreturn`: a handler may have
    // queued another signal, and that signal must run immediately after the
    // interrupted context is restored. `SYS_EXIT` never reaches this point.
    if number != SYS_EXIT {
        if let Some(status) = deliver_pending_signal(frame) {
            terminate_current(frame, status);
        }
    }
    result
}

/// Validates a user-memory range, returning `(virtual address, length)`.
///
/// Every pointer the syscalls accept is checked against the per-process user
/// region: a program can never name kernel memory, even with a wrapping address.
fn user_range(address: u64, length: u64) -> Option<(usize, usize)> {
    let end = address.checked_add(length)?;
    if address < USER_BASE || end > USER_REGION_END {
        return None;
    }
    Some((address as usize, usize::try_from(length).ok()?))
}

/// Largest number of startup arguments copied onto a new user stack.
const MAX_ARGC: usize = 32;
/// Largest single startup argument accepted by [`seed_startup_stack`].
const MAX_ARG_BYTES: usize = 256;
/// Largest packed startup argument block accepted by the kernel.
const MAX_ARG_BLOCK_BYTES: usize = 4096;

/// Startup registers returned after writing an argument block to a user stack.
struct StartupStack {
    /// The stack pointer the process enters with.
    rsp: u64,
    /// The `argc` value passed in `rdi`.
    argc: u64,
    /// The `argv` address passed in `rsi`.
    argv: u64,
}

/// Splits a packed argument block into borrowed strings.
///
/// The block is a sequence of NUL-terminated strings followed by one extra NUL
/// byte. An empty block asks the caller to use a synthetic `gtos` argument.
fn parse_startup_args(block: &[u8]) -> Result<Vec<&[u8]>, &'static str> {
    if block.is_empty() {
        return Ok(Vec::new());
    }
    if block.len() > MAX_ARG_BLOCK_BYTES {
        return Err("argument block is too large");
    }
    let mut arguments = Vec::new();
    let mut start = 0;
    for (index, byte) in block.iter().copied().enumerate() {
        if byte != 0 {
            continue;
        }
        let argument = &block[start..index];
        if argument.is_empty() {
            return if index + 1 == block.len() {
                Ok(arguments)
            } else {
                Err("empty startup argument")
            };
        }
        if argument.len() > MAX_ARG_BYTES {
            return Err("startup argument is too long");
        }
        if arguments.len() == MAX_ARGC {
            return Err("too many startup arguments");
        }
        arguments.push(argument);
        start = index + 1;
    }
    Err("unterminated argument block")
}

/// Copies bytes into a mapped user stack through the kernel's frame window.
fn write_user_stack(
    space: &mut AddressSpace,
    address: u64,
    bytes: &[u8],
) -> Result<(), &'static str> {
    let mut cursor = address;
    let mut remaining = bytes;
    while !remaining.is_empty() {
        let offset = cursor % PAGE_BYTES;
        let count = usize::min(remaining.len(), (PAGE_BYTES - offset) as usize);
        let target = space
            .mapped_frame_address(cursor)
            .ok_or("startup argument page is not mapped")?;
        // SAFETY: `mapped_frame_address` proved the page is owned by `space`,
        // and `count` keeps the write inside that one page.
        unsafe {
            core::ptr::copy_nonoverlapping(remaining.as_ptr(), (target + offset) as *mut u8, count);
        }
        cursor += count as u64;
        remaining = &remaining[count..];
    }
    Ok(())
}

/// Writes a conventional C startup frame onto `stack_top` in `space`.
///
/// At `rsp` the stack holds `argc`, a NULL-terminated `argv` array and an empty
/// `envp`; the argument strings sit above that frame, toward `stack_top`, so
/// the growing stack cannot overwrite them. The returned registers let the C
/// ABI shim skip a naked entry stub while still receiving a real command line.
fn seed_startup_stack(
    space: &mut AddressSpace,
    stack_top: u64,
    block: &[u8],
) -> Result<StartupStack, &'static str> {
    let default: [&[u8]; 1] = [b"gtos"];
    let arguments = if block.is_empty() {
        default.to_vec()
    } else {
        parse_startup_args(block)?
    };
    let argc = arguments.len();
    let string_bytes: usize = arguments.iter().map(|argument| argument.len() + 1).sum();
    let pointer_bytes = (argc + 3) * core::mem::size_of::<u64>();
    let strings_end = stack_top
        .checked_sub(string_bytes as u64)
        .ok_or("startup stack underflow")?;
    let frame_start = strings_end
        .checked_sub(pointer_bytes as u64)
        .ok_or("startup stack underflow")?;
    // SysV requires a 16-byte-aligned stack at process entry. Aligning downward
    // leaves harmless padding between the strings and `stack_top`.
    let argc_address = frame_start & !0xf;
    // The pointer array is the low end of the startup frame and `rsp` points at
    // its `argc`. Strings live above it, so the stack grows away from them.
    let strings_start = argc_address + pointer_bytes as u64;

    let mut cursor = strings_start;
    let mut pointers = Vec::with_capacity(argc);
    for argument in &arguments {
        write_user_stack(space, cursor, argument)?;
        write_user_stack(space, cursor + argument.len() as u64, &[0])?;
        pointers.push(cursor);
        cursor += argument.len() as u64 + 1;
    }

    let mut slot = argc_address;
    write_user_stack(space, slot, &(argc as u64).to_ne_bytes())?;
    slot += 8;
    for pointer in pointers {
        write_user_stack(space, slot, &pointer.to_ne_bytes())?;
        slot += 8;
    }
    write_user_stack(space, slot, &0u64.to_ne_bytes())?;
    slot += 8;
    write_user_stack(space, slot, &0u64.to_ne_bytes())?;

    Ok(StartupStack {
        rsp: argc_address,
        argc: argc as u64,
        argv: argc_address + 8,
    })
}

/// Copies a syscall 9 argument block from the caller's old address space.
///
/// Argument 2 is either zero or a pointer to a `u64` byte count followed by the
/// packed NUL-terminated strings. The count lets the kernel validate the range
/// before it abandons the old page tables.
fn copy_startup_block(pointer: u64) -> Result<Vec<u8>, &'static str> {
    if pointer == 0 {
        return Ok(Vec::new());
    }
    let (start, _) = user_range(pointer, 8).ok_or("bad argument block pointer")?;
    // SAFETY: `user_range` proved the eight-byte length is inside user space.
    let length_bytes: [u8; 8] = unsafe { core::ptr::read_unaligned(start as *const [u8; 8]) };
    let length = u64::from_ne_bytes(length_bytes);
    if length > MAX_ARG_BLOCK_BYTES as u64 {
        return Err("argument block is too large");
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    let data_pointer = pointer.checked_add(8).ok_or("argument block overflow")?;
    let (start, length) = user_range(data_pointer, length).ok_or("bad argument block range")?;
    // SAFETY: `user_range` proved the full block lies in the caller's mapped
    // region, and the kernel copies it before switching address spaces.
    Ok(unsafe { Vec::from(core::slice::from_raw_parts(start as *const u8, length)) })
}

/// The PID of the currently running process, if any.
pub(crate) fn current_pid() -> Option<crate::process::Pid> {
    crate::process::Pid::from_u64(CURRENT_PID.load(Ordering::SeqCst))
}

/// Resolves a copy-on-write page fault for the running process.
pub(crate) fn handle_cow_fault(address: u64) -> bool {
    let Some(pid) = current_pid() else {
        return false;
    };
    crate::process::with_process_mut(pid, |process| {
        process.address_space_mut().handle_cow_write(address)
    })
    .unwrap_or(false)
}

/// Resolves copy-on-write pages before the kernel writes to a user buffer.
///
/// A normal ring-3 store faults and is fixed up by the page-fault handler, but
/// a syscall writes through the kernel's own pointer and would bypass it.
fn prepare_user_write(address: u64, length: usize) {
    let Ok(length) = u64::try_from(length) else {
        return;
    };
    let Some(end) = address.checked_add(length) else {
        return;
    };
    let mut page = address & !(PAGE_BYTES - 1);
    while page < end {
        handle_cow_fault(page);
        page += PAGE_BYTES;
    }
}

/// Mutates the current process's descriptor table.
fn with_current_fds<R>(f: impl FnOnce(&mut crate::scheme::FdTable) -> R) -> Option<R> {
    let pid = current_pid()?;
    crate::process::with_process_mut(pid, |process| f(process.fds_mut()))
}

/// Mutates descriptors and publishes the resulting readiness change.
///
/// The generation advances while the process table is still locked, so a
/// multiplexed waiter cannot slip between its readiness check and its sleep
/// registration. Wakeups happen after the lock is released to preserve the
/// process-table/resource lock ordering.
fn with_current_fds_notify<R>(f: impl FnOnce(&mut crate::scheme::FdTable) -> R) -> Option<R> {
    let pid = current_pid()?;
    let result = crate::process::with_process_mut(pid, |process| {
        let result = f(process.fds_mut());
        crate::process::bump_readiness_generation();
        result
    });
    crate::process::wake_readiness_waiters();
    result
}

/// The current process's working directory, used to resolve relative paths.
fn current_cwd() -> Option<String> {
    let pid = current_pid()?;
    crate::process::with_process(pid, |process| String::from(process.cwd()))
}

/// Updates the current process's working directory.
fn set_current_cwd(path: String) -> bool {
    let Some(pid) = current_pid() else {
        return false;
    };
    crate::process::with_process_mut(pid, |process| process.set_cwd(path)).is_some()
}

/// Copies kernel bytes into a validated user buffer, resolving COW first.
fn copy_to_user(address: u64, bytes: &[u8]) -> bool {
    let Ok(length) = u64::try_from(bytes.len()) else {
        return false;
    };
    let Some((start, _)) = user_range(address, length) else {
        return false;
    };
    prepare_user_write(address, bytes.len());
    // SAFETY: `user_range` proved the destination lies inside the mapped user
    // window, and `prepare_user_write` resolved COW on every page it covers.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), start as *mut u8, bytes.len()) };
    true
}

/// Updates the PID of the currently running process after a context switch.
pub(crate) fn set_current_pid(pid: u64) {
    CURRENT_PID.store(pid, Ordering::SeqCst);
}

/// Points the TSS's ring-0 stack at `top` for the next ring-3 transition.
///
/// # Safety
///
/// `top` must be the top of a valid, writable kernel stack owned by a live
/// process.
pub(crate) unsafe fn set_kernel_stack(top: u64) {
    // SAFETY: the TSS is a static and is only written during boot or a context
    // switch while interrupts are masked by the `int 0x80` gate.
    unsafe {
        (*core::ptr::addr_of_mut!(TSS)).privilege_stack_table[0] = VirtAddr::new(top);
    }
}

/// Points the TSS back at the boot ring-0 stack after a `run` completes.
///
/// # Safety
///
/// Must only be called after returning to the shell, when no process can raise
/// another ring-3 interrupt.
pub(crate) unsafe fn restore_boot_kernel_stack() {
    // SAFETY: `KERNEL_STACK` is a static array; one-past-the-end is a valid
    // downward-growing stack top.
    let top = unsafe {
        core::ptr::addr_of_mut!(KERNEL_STACK)
            .cast::<u8>()
            .add(KERNEL_STACK_BYTES) as u64
    };
    // SAFETY: the TSS is static and no ring-3 code is running at this point.
    unsafe { set_kernel_stack(top) };
}

const fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

/// Outcome of one ordinary descriptor write attempt.
enum OrdinaryFdWrite {
    /// The syscall has a final result; `notify` reports a successful produce.
    Completed { result: u64, notify: bool },
    /// The queue is full and the descriptor is blocking, so the caller waits.
    Wait,
}

/// `write(fd, buffer, length)`: writes to the console on fd 1 or 2, or to a
/// writable scheme descriptor such as a FAT file opened with `open`.
///
/// A full blocking pipe or datagram socket arms the readiness registration in
/// the same critical section as the write attempt and sleeps until a reader
/// frees capacity. `O_NONBLOCK` still returns the public error value. A context
/// switch resumes the user task with [`SIGWAIT_RETRY`], which the SDK wrapper
/// hides.
fn sys_write(fd: u64, buffer: u64, length: u64, frame: *mut u64) -> u64 {
    if fd != 1 && fd != 2 {
        let Some(pid) = current_pid() else {
            return u64::MAX;
        };
        let Some((start, length)) = user_range(buffer, length) else {
            return u64::MAX;
        };
        // SAFETY: `user_range` proved the range lies inside the mapped user
        // window, and the kernel only reads from it.
        let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
        loop {
            // Write and arm together: a reader either drains before this call
            // and frees capacity now, or after the registration and wakes us.
            let outcome = crate::process::with_process_mut(pid, |process| {
                let result = process.fds_mut().write(fd, bytes);
                let would_block = result
                    .as_ref()
                    .is_err_and(|error| crate::scheme::is_would_block(error));
                if would_block && !process.fds().is_nonblocking(fd) {
                    process.arm_readiness_wait();
                    process.set_sleeping(u64::MAX);
                    return OrdinaryFdWrite::Wait;
                }
                process.clear_readiness_wait();
                match result {
                    Ok(count) => {
                        if count > 0 {
                            // Producing bytes makes new data readable on the
                            // peer and frees nothing here, but it still changes
                            // readiness for any reader waiting on this queue.
                            crate::process::bump_readiness_generation();
                        }
                        OrdinaryFdWrite::Completed {
                            result: count as u64,
                            notify: count > 0,
                        }
                    }
                    Err(_) => OrdinaryFdWrite::Completed {
                        result: u64::MAX,
                        notify: false,
                    },
                }
            });
            let Some(outcome) = outcome else {
                return u64::MAX;
            };
            match outcome {
                OrdinaryFdWrite::Completed { result, notify } => {
                    if notify {
                        crate::process::wake_readiness_waiters();
                    }
                    return result;
                }
                OrdinaryFdWrite::Wait => {
                    scheduler::block_current_until_ready(pid, u64::MAX, frame);
                    if current_pid().is_none() {
                        return u64::MAX;
                    }
                }
            }
        }
    }
    let Some((start, length)) = user_range(buffer, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the range lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    crate::console::write_bytes(bytes);
    length as u64
}

/// `open(url, length)`: opens a scheme resource, returning a descriptor >= 3.
fn sys_open(url: u64, length: u64, _arg2: u64) -> u64 {
    let Some((start, length)) = user_range(url, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the range lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Ok(url) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    let cwd = current_cwd().unwrap_or_else(|| String::from("/"));
    let resolved = crate::vfs::resolve(&cwd, url).unwrap_or_else(|_| {
        if url == "/" {
            String::from("file:/")
        } else {
            String::from(url)
        }
    });
    let url = resolved.as_str();
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    if !with_current_fds(|fds| fds.has_room(1)).unwrap_or(false) {
        return u64::MAX;
    }
    if let Some(handle) = crate::scheme::connect(url, pid.as_u64()) {
        crate::process::notify_readiness();
        return with_current_fds_notify(|fds| fds.insert_stream(handle))
            .and_then(Result::ok)
            .unwrap_or(u64::MAX);
    }
    with_current_fds_notify(|fds| fds.open(url).unwrap_or(u64::MAX)).unwrap_or(u64::MAX)
}

/// Outcome of the signal-descriptor special case in `read`.
enum SignalFdRead {
    /// This descriptor is an ordinary scheme resource.
    NotSignalFd,
    /// One `Siginfo` record was consumed and written to the user buffer.
    Consumed(usize),
    /// No matching signal is pending; the caller must block and retry.
    Block { mask: u64 },
    /// Bad buffer or descriptor flags.
    Error,
}

/// Consumes one pending signal for a signal descriptor in one critical section.
///
/// Reading the descriptor mask and the process's pending set together keeps a
/// concurrent sender from being lost between inspection and consumption.
fn read_signalfd(pid: crate::process::Pid, fd: u64, out: &mut [u8]) -> SignalFdRead {
    crate::process::with_process_mut(pid, |process| {
        let Some((mask, nonblocking)) = process.fds().signalfd(fd) else {
            return SignalFdRead::NotSignalFd;
        };
        if out.len() < SIGINFO_BYTES as usize {
            return SignalFdRead::Error;
        }
        if let Some((_, info)) = process.signals_mut().take_for_wait(mask) {
            // SAFETY: `user_range` proved at least `SIGINFO_BYTES` writable bytes
            // and `prepare_user_write` resolved COW on every page in the range.
            unsafe {
                out.as_mut_ptr()
                    .cast::<crate::signal::Siginfo>()
                    .write_unaligned(info);
            };
            crate::process::bump_readiness_generation();
            return SignalFdRead::Consumed(SIGINFO_BYTES as usize);
        }
        if nonblocking {
            SignalFdRead::Error
        } else {
            SignalFdRead::Block { mask }
        }
    })
    .unwrap_or(SignalFdRead::Error)
}

/// Outcome of one ordinary descriptor read attempt.
enum OrdinaryFdRead {
    /// The syscall has a final result; `notify` reports a successful consume.
    Completed { result: u64, notify: bool },
    /// The descriptor has no data yet and is blocking, so the caller must wait.
    Wait,
}

/// `read(fd, buffer, length)`: reads up to `length` bytes from a scheme.
///
/// Signal descriptors are handled here instead of through [`FdTable::read`]
/// because consumption and blocking must both operate on the caller's signal
/// state, not on descriptor-local bytes. A blocking pipe or socket with no data
/// arms a readiness registration and sleeps until a peer changes it; `O_NONBLOCK`
/// preserves the existing `ERROR` response. A context switch resumes the user
/// task with [`SIGWAIT_RETRY`], which the SDK wrapper hides.
fn sys_read(fd: u64, buffer: u64, length: u64, frame: *mut u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Some((start, length)) = user_range(buffer, length) else {
        return u64::MAX;
    };
    prepare_user_write(buffer, length);
    // SAFETY: `user_range` proved the range lies inside the mapped user window,
    // and `prepare_user_write` resolved any COW mapping in it.
    let bytes = unsafe { core::slice::from_raw_parts_mut(start as *mut u8, length) };
    loop {
        match read_signalfd(pid, fd, &mut *bytes) {
            SignalFdRead::NotSignalFd => {
                // Read and arm in one process-table critical section: a writer
                // either lands before this read and is visible now, or after the
                // registration and sees a stale generation to wake us.
                let outcome = crate::process::with_process_mut(pid, |process| {
                    let result = process.fds_mut().read(fd, &mut *bytes);
                    let would_block = result
                        .as_ref()
                        .is_err_and(|error| crate::scheme::is_would_block(error));
                    if would_block && !process.fds().is_nonblocking(fd) {
                        process.arm_readiness_wait();
                        process.set_sleeping(u64::MAX);
                        return OrdinaryFdRead::Wait;
                    }
                    process.clear_readiness_wait();
                    match result {
                        Ok(count) => {
                            if count > 0 {
                                // Consuming bytes frees pipe/socket capacity
                                // for writers waiting on this queue.
                                crate::process::bump_readiness_generation();
                            }
                            OrdinaryFdRead::Completed {
                                result: count as u64,
                                notify: count > 0,
                            }
                        }
                        Err(_) => OrdinaryFdRead::Completed {
                            result: u64::MAX,
                            notify: false,
                        },
                    }
                });
                let Some(outcome) = outcome else {
                    return u64::MAX;
                };
                match outcome {
                    OrdinaryFdRead::Completed { result, notify } => {
                        if notify {
                            crate::process::wake_readiness_waiters();
                        }
                        return result;
                    }
                    OrdinaryFdRead::Wait => {
                        scheduler::block_current_until_ready(pid, u64::MAX, frame);
                        if current_pid().is_none() {
                            return u64::MAX;
                        }
                    }
                }
            }
            SignalFdRead::Consumed(count) => return count as u64,
            SignalFdRead::Error => return u64::MAX,
            SignalFdRead::Block { mask } => {
                // A context switch resumes at the user syscall boundary with
                // `SIGWAIT_RETRY`; the SDK `read` wrapper calls us again. This
                // loop only continues here when no switch was needed.
                scheduler::block_current_for_signal(pid, mask, u64::MAX, frame);
                if current_pid().is_none() {
                    return u64::MAX;
                }
            }
        }
    }
}

/// Computes one entry's `revents` while the process table is held.
fn poll_entry(process: &crate::process::Process, fd: u64, events: u16) -> u16 {
    use crate::scheme::{POLLIN, POLLOUT};

    match fd {
        0 => events & POLLIN,
        1 | 2 => events & POLLOUT,
        _ => {
            let signal_ready = process
                .fds()
                .signalfd(fd)
                .is_some_and(|(mask, _)| process.signals().pending_for_mask(mask) != 0);
            process.fds().poll(fd, events, signal_ready)
        }
    }
}

/// `poll(fds, length, timeout_ms)`: waits for readiness on several descriptors.
///
/// `fds` is an array of 16-byte `PollFd` records. The kernel writes each
/// `revents` back in place and returns the number of entries with a nonzero
/// result. Blocking waits arm one readiness registration in the same critical
/// section that checks the descriptors, then sleep until a readiness event or
/// the deadline. A context switch resumes the user task with [`SIGWAIT_RETRY`],
/// which the SDK wrapper hides by recomputing the remaining timeout.
fn sys_poll(fds: u64, length: u64, timeout_ms: u64, frame: *mut u64) -> u64 {
    let Ok(nfds) = usize::try_from(length) else {
        return u64::MAX;
    };
    if nfds > MAX_POLL_FDS {
        return u64::MAX;
    }
    let fd_bytes = (nfds * core::mem::size_of::<PollFd>()) as u64;
    let Some((start, _)) = user_range(fds, fd_bytes) else {
        return u64::MAX;
    };
    prepare_user_write(fds, fd_bytes as usize);

    let mut requests = Vec::with_capacity(nfds);
    for index in 0..nfds {
        let base = start + index * core::mem::size_of::<PollFd>();
        // SAFETY: `user_range` validated the whole array and COW was resolved.
        let fd = unsafe { (base as *const u64).read_unaligned() };
        // SAFETY: `events` lies inside the same validated 16-byte record.
        let events = unsafe {
            ((base + core::mem::offset_of!(PollFd, events)) as *const u16).read_unaligned()
        };
        requests.push(PollFd {
            fd,
            events,
            revents: 0,
        });
    }

    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let deadline = if timeout_ms == u64::MAX {
        u64::MAX
    } else {
        if timeout_ms > 60_000 {
            return u64::MAX;
        }
        let hz = crate::interrupts::TICK_HZ as u64;
        crate::interrupts::ticks()
            .saturating_add(timeout_ms.saturating_mul(hz).div_ceil(1000).max(1))
    };

    loop {
        let now = crate::interrupts::ticks();
        let Some((ready, results)) = crate::process::with_process_mut(pid, |process| {
            let mut results = alloc::vec![0; nfds];
            let mut count = 0;
            for (slot, request) in requests.iter().enumerate() {
                let revents = poll_entry(process, request.fd, request.events);
                results[slot] = revents;
                if revents != 0 {
                    count += 1;
                }
            }
            if count == 0 && timeout_ms != 0 && now < deadline {
                process.arm_readiness_wait();
                process.set_sleeping(deadline);
            } else {
                process.clear_readiness_wait();
            }
            (count, results)
        }) else {
            return u64::MAX;
        };

        for (index, revents) in results.iter().enumerate() {
            let address = start
                + index * core::mem::size_of::<PollFd>()
                + core::mem::offset_of!(PollFd, revents);
            // SAFETY: `user_range` and `prepare_user_write` proved this field is
            // writable in the caller's mapped user region.
            unsafe { (address as *mut u16).write_unaligned(*revents) };
        }

        if ready != 0 {
            return ready as u64;
        }
        if timeout_ms == 0 || now >= deadline {
            return 0;
        }
        scheduler::block_current_until_ready(pid, deadline, frame);
        if current_pid().is_none() {
            return u64::MAX;
        }
    }
}

/// Reads one `select` fd set and resolves copy-on-write before a later write.
///
/// A null pointer denotes an omitted set and yields zero bits.
fn read_select_set(pointer: u64) -> Option<(usize, [u64; SELECT_FD_WORDS])> {
    if pointer == 0 {
        return Some((0, [0; SELECT_FD_WORDS]));
    }
    let (start, _) = user_range(pointer, SELECT_SET_BYTES)?;
    prepare_user_write(pointer, SELECT_SET_BYTES as usize);
    let mut bits = [0; SELECT_FD_WORDS];
    for (index, word) in bits.iter_mut().enumerate() {
        // SAFETY: `user_range` validated every word and COW was resolved above.
        *word = unsafe { (start as *const u64).add(index).read_unaligned() };
    }
    Some((start, bits))
}

/// Publishes the final `select` result for one fd set.
fn write_select_set(address: usize, bits: [u64; SELECT_FD_WORDS]) {
    if address == 0 {
        return;
    }
    for (index, word) in bits.iter().enumerate() {
        // SAFETY: `read_select_set` validated this address and resolved COW.
        unsafe { (address as *mut u64).add(index).write_unaligned(*word) };
    }
}

/// Clears bits at or beyond `nfds` from every word of one fd set.
fn mask_select_words(bits: &mut [u64; SELECT_FD_WORDS], nfds: usize) {
    for (index, word) in bits.iter_mut().enumerate() {
        let start = index * 64;
        if start >= nfds {
            *word = 0;
        } else if start + 64 > nfds {
            let kept = nfds - start;
            *word &= if kept == 64 {
                u64::MAX
            } else {
                (1u64 << kept) - 1
            };
        }
    }
}

/// Validates the `select` argument block and its three fd sets.
fn parse_select_args(args_pointer: u64) -> Option<(usize, SelectSet, SelectSet, SelectSet, u64)> {
    let (args_start, _) = user_range(args_pointer, core::mem::size_of::<SelectArgs>() as u64)?;
    let field = |offset: usize| (args_start + offset) as *const u64;
    // SAFETY: `user_range` validated the whole argument block above.
    let nfds_value = unsafe { field(core::mem::offset_of!(SelectArgs, nfds)).read_unaligned() };
    // SAFETY: same validated argument block.
    let read_pointer =
        unsafe { field(core::mem::offset_of!(SelectArgs, readfds)).read_unaligned() };
    // SAFETY: same validated argument block.
    let write_pointer =
        unsafe { field(core::mem::offset_of!(SelectArgs, writefds)).read_unaligned() };
    // SAFETY: same validated argument block.
    let except_pointer =
        unsafe { field(core::mem::offset_of!(SelectArgs, exceptfds)).read_unaligned() };
    // SAFETY: same validated argument block.
    let timeout_ms =
        unsafe { field(core::mem::offset_of!(SelectArgs, timeout_ms)).read_unaligned() };

    let nfds = usize::try_from(nfds_value).ok()?;
    if nfds > SELECT_FD_SETSIZE {
        return None;
    }
    // Aliased output sets would overwrite each other; the SDK never produces
    // this, but the kernel rejects it rather than guessing a precedence.
    if read_pointer != 0 && (read_pointer == write_pointer || read_pointer == except_pointer)
        || write_pointer != 0 && write_pointer == except_pointer
    {
        return None;
    }
    let (read_address, mut read_bits) = read_select_set(read_pointer)?;
    let (write_address, mut write_bits) = read_select_set(write_pointer)?;
    let (except_address, mut except_bits) = read_select_set(except_pointer)?;
    mask_select_words(&mut read_bits, nfds);
    mask_select_words(&mut write_bits, nfds);
    mask_select_words(&mut except_bits, nfds);
    Some((
        nfds,
        SelectSet {
            address: read_address,
            bits: read_bits,
        },
        SelectSet {
            address: write_address,
            bits: write_bits,
        },
        SelectSet {
            address: except_address,
            bits: except_bits,
        },
        timeout_ms,
    ))
}

/// Scans one snapshot of all requested descriptors and arms a blocked wait.
fn scan_select_sets(
    pid: crate::process::Pid,
    nfds: usize,
    read: &SelectSet,
    write: &SelectSet,
    except: &SelectSet,
    timeout_ms: u64,
    deadline: u64,
) -> Option<SelectScan> {
    use crate::scheme::{POLLERR, POLLHUP, POLLIN, POLLNVAL, POLLOUT};

    let mut out_read = [0; SELECT_FD_WORDS];
    let mut out_write = [0; SELECT_FD_WORDS];
    let mut out_except = [0; SELECT_FD_WORDS];
    crate::process::with_process_mut(pid, |process| {
        let now = crate::interrupts::ticks();
        let mut count = 0;
        for fd in 0..nfds {
            let bit = 1u64 << (fd % 64);
            let word = fd / 64;
            let wants_read = read.bits[word] & bit != 0;
            let wants_write = write.bits[word] & bit != 0;
            let wants_except = except.bits[word] & bit != 0;
            if !wants_read && !wants_write && !wants_except {
                continue;
            }
            let mut events = 0;
            if wants_read {
                events |= POLLIN;
            }
            if wants_write {
                events |= POLLOUT;
            }
            let revents = poll_entry(process, fd as u64, events);
            if wants_read && revents & POLLIN != 0 {
                out_read[word] |= bit;
                count += 1;
            }
            if wants_write && revents & POLLOUT != 0 {
                out_write[word] |= bit;
                count += 1;
            }
            if wants_except && revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                out_except[word] |= bit;
                count += 1;
            }
        }
        if count == 0 && timeout_ms != 0 && now < deadline {
            process.arm_readiness_wait();
            process.set_sleeping(deadline);
        } else {
            process.clear_readiness_wait();
        }
        (count, now)
    })
    .map(|(ready, now)| (out_read, out_write, out_except, ready, now))
}

/// `select(args)`: classic fd-bitmask multiplexing over pipes, sockets and signals.
///
/// The caller passes a pointer to [`SelectArgs`] because the syscall ABI exposes
/// three argument registers. Each set holds sixteen `u64` words, so descriptor
/// numbers `0..1024` are selectable. Read and write sets report `POLLIN`/`POLLOUT`;
/// the exception set reports `POLLERR`, `POLLHUP` and `POLLNVAL`. Sets are rewritten
/// only on the final return, so a readiness wake leaves the caller's request intact
/// for the SDK wrapper's retry.
fn sys_select(args_pointer: u64, frame: *mut u64) -> u64 {
    let Some((nfds, read, write, except, timeout_ms)) = parse_select_args(args_pointer) else {
        return u64::MAX;
    };
    let deadline = if timeout_ms == u64::MAX {
        u64::MAX
    } else {
        if timeout_ms > 60_000 {
            return u64::MAX;
        }
        let hz = crate::interrupts::TICK_HZ as u64;
        crate::interrupts::ticks()
            .saturating_add(timeout_ms.saturating_mul(hz).div_ceil(1000).max(1))
    };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };

    loop {
        let Some((out_read, out_write, out_except, ready, now)) =
            scan_select_sets(pid, nfds, &read, &write, &except, timeout_ms, deadline)
        else {
            return u64::MAX;
        };
        if ready != 0 {
            write_select_set(read.address, out_read);
            write_select_set(write.address, out_write);
            write_select_set(except.address, out_except);
            return ready as u64;
        }
        if timeout_ms == 0 || now >= deadline {
            let empty = [0; SELECT_FD_WORDS];
            write_select_set(read.address, empty);
            write_select_set(write.address, empty);
            write_select_set(except.address, empty);
            return 0;
        }
        scheduler::block_current_until_ready(pid, deadline, frame);
        if current_pid().is_none() {
            return u64::MAX;
        }
    }
}

/// `close(fd)`: releases a scheme descriptor.
fn sys_close(fd: u64, _arg1: u64, _arg2: u64) -> u64 {
    if with_current_fds_notify(|fds| fds.close(fd)).unwrap_or(false) {
        0
    } else {
        u64::MAX
    }
}

/// `dup(fd)`: duplicates a descriptor.
fn sys_dup(fd: u64) -> u64 {
    with_current_fds_notify(|fds| fds.dup(fd).unwrap_or(u64::MAX)).unwrap_or(u64::MAX)
}

/// `dup2(oldfd, newfd)`: duplicates a descriptor to a specific number.
fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    with_current_fds_notify(|fds| fds.dup2(oldfd, newfd).unwrap_or(u64::MAX)).unwrap_or(u64::MAX)
}

/// `shutdown(fd, how)`: shuts down read/write sides of a socket/stream.
fn sys_shutdown(fd: u64, how: u64) -> u64 {
    if with_current_fds_notify(|fds| fds.shutdown(fd, how).is_ok()).unwrap_or(false) {
        0
    } else {
        u64::MAX
    }
}

/// `fcntl(fd, cmd, arg)`: gets/sets file status flags.
fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    with_current_fds_notify(|fds| fds.fcntl(fd, cmd, arg).unwrap_or(u64::MAX)).unwrap_or(u64::MAX)
}

/// `lseek(fd, offset, whence)`: moves a descriptor's read/write cursor.
fn sys_lseek(fd: u64, offset: u64, whence: u64) -> u64 {
    let Ok(whence) = u8::try_from(whence) else {
        return u64::MAX;
    };
    with_current_fds(|fds| fds.seek(fd, offset as i64, whence).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX)
}

/// `ioctl(fd, request, arg)`: queries or adjusts one descriptor.
fn sys_ioctl(fd: u64, request: u64, arg: u64) -> u64 {
    with_current_fds(|fds| fds.ioctl(fd, request, arg).unwrap_or(u64::MAX)).unwrap_or(u64::MAX)
}

/// Unix-domain socket family.
const AF_UNIX: u64 = 1;
/// Connection-oriented byte stream socket.
const SOCK_STREAM: u64 = 1;
/// Connection-oriented datagram socket, preserving message boundaries.
const SOCK_SEQPACKET: u64 = 5;
/// Create the socket pair with nonblocking endpoints.
const SOCK_NONBLOCK: u64 = 0x0800;
/// Create the socket pair with close-on-exec endpoints.
const SOCK_CLOEXEC: u64 = 0x8_0000;

/// Creates a bounded pipe, applies creation flags and writes both descriptors.
fn create_pipe(fds: u64, flags: u64) -> u64 {
    const SUPPORTED_FLAGS: u64 = crate::scheme::O_NONBLOCK | crate::scheme::O_CLOEXEC;
    if flags & !SUPPORTED_FLAGS != 0 {
        return u64::MAX;
    }
    let bytes = 2 * core::mem::size_of::<u64>() as u64;
    let Some((start, _)) = user_range(fds, bytes) else {
        return u64::MAX;
    };
    prepare_user_write(fds, bytes as usize);
    let Some(Ok((read_fd, write_fd))) = with_current_fds(crate::scheme::FdTable::pipe) else {
        return u64::MAX;
    };
    if flags != 0 {
        let ok = with_current_fds_notify(|table| {
            if flags & crate::scheme::O_NONBLOCK != 0
                && (table
                    .fcntl(read_fd, crate::scheme::F_SETFL, crate::scheme::O_NONBLOCK)
                    .is_err()
                    || table
                        .fcntl(write_fd, crate::scheme::F_SETFL, crate::scheme::O_NONBLOCK)
                        .is_err())
            {
                return false;
            }
            if flags & crate::scheme::O_CLOEXEC != 0
                && (table
                    .fcntl(read_fd, crate::scheme::F_SETFD, crate::scheme::FD_CLOEXEC)
                    .is_err()
                    || table
                        .fcntl(write_fd, crate::scheme::F_SETFD, crate::scheme::FD_CLOEXEC)
                        .is_err())
            {
                return false;
            }
            true
        })
        .unwrap_or(false);
        if !ok {
            let _ = with_current_fds_notify(|table| {
                let _ = table.close(read_fd);
                let _ = table.close(write_fd);
            });
            return u64::MAX;
        }
    }
    // SAFETY: `user_range` validated sixteen bytes and `prepare_user_write`
    // resolved COW on every page in the range. Unaligned writes are safe because
    // the syscall ABI makes no alignment promise about this output array.
    unsafe {
        core::ptr::write_unaligned(start as *mut u64, read_fd);
        core::ptr::write_unaligned((start + 8) as *mut u64, write_fd);
    }
    0
}

/// `pipe(fds)`: creates a bounded pipe and writes `[read_fd, write_fd]` to `fds`.
fn sys_pipe(fds: u64) -> u64 {
    create_pipe(fds, 0)
}

/// `pipe2(fds, flags)`: creates a pipe with flags applied before either
/// descriptor becomes visible to the caller.
fn sys_pipe2(fds: u64, flags: u64) -> u64 {
    create_pipe(fds, flags)
}

/// `socketpair(domain, type, fds)`: creates two connected endpoints.
///
/// The protocol is implicitly zero; supports `AF_UNIX`/`SOCK_SEQPACKET` and
/// `AF_UNIX`/`SOCK_STREAM`. `SOCK_NONBLOCK` and `SOCK_CLOEXEC` may be OR-ed
/// into `type`.
fn sys_socketpair(domain: u64, socket_type: u64, fds: u64) -> u64 {
    if domain != AF_UNIX {
        return u64::MAX;
    }
    let bytes = 2 * core::mem::size_of::<u64>() as u64;
    let Some((start, _)) = user_range(fds, bytes) else {
        return u64::MAX;
    };
    prepare_user_write(fds, bytes as usize);
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let owner_pid = pid.as_u64();
    let nonblock = socket_type & SOCK_NONBLOCK != 0;
    let cloexec = socket_type & SOCK_CLOEXEC != 0;
    let base_type = socket_type & !(SOCK_NONBLOCK | SOCK_CLOEXEC);
    let pair = match base_type {
        SOCK_SEQPACKET => with_current_fds(|fds| fds.socketpair_seqpacket(owner_pid)),
        SOCK_STREAM => with_current_fds(|fds| fds.socketpair_stream(owner_pid)),
        _ => return u64::MAX,
    };
    let Some(Ok((first, second))) = pair else {
        return u64::MAX;
    };
    if nonblock || cloexec {
        let ok = with_current_fds_notify(|fds| {
            (!nonblock
                || (fds
                    .fcntl(first, crate::scheme::F_SETFL, crate::scheme::O_NONBLOCK)
                    .is_ok()
                    && fds
                        .fcntl(second, crate::scheme::F_SETFL, crate::scheme::O_NONBLOCK)
                        .is_ok()))
                && (!cloexec
                    || (fds
                        .fcntl(first, crate::scheme::F_SETFD, crate::scheme::FD_CLOEXEC)
                        .is_ok()
                        && fds
                            .fcntl(second, crate::scheme::F_SETFD, crate::scheme::FD_CLOEXEC)
                            .is_ok()))
        })
        .unwrap_or(false);
        if !ok {
            let _ = with_current_fds_notify(|fds| {
                let _ = fds.close(first);
                let _ = fds.close(second);
            });
            return u64::MAX;
        }
    }
    // SAFETY: `user_range` validated sixteen bytes and `prepare_user_write`
    // resolved COW on every page in the range.
    unsafe {
        core::ptr::write_unaligned(start as *mut u64, first);
        core::ptr::write_unaligned((start + 8) as *mut u64, second);
    }
    0
}

/// `schemeregister(name, length, notify_fd)`: serves a scheme from ring 3.
fn sys_schemeregister(name: u64, length: u64, notify_fd: u64) -> u64 {
    let Some((start, length)) = user_range(name, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the name lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Ok(name) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Some(notify) = with_current_fds(|fds| fds.write_handle(notify_fd)).flatten() else {
        return u64::MAX;
    };
    let registered = crate::scheme::register(name, pid.as_u64(), notify);
    crate::process::notify_readiness();
    if registered { 0 } else { u64::MAX }
}

/// `schemeunregister(name, length)`: stops serving a scheme.
fn sys_schemeunregister(name: u64, length: u64) -> u64 {
    let Some((start, length)) = user_range(name, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the name lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Ok(name) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let unregistered = crate::scheme::unregister(name, pid.as_u64());
    crate::process::notify_readiness();
    if unregistered { 0 } else { u64::MAX }
}

/// `stat(path, length, info)`: copies VFS metadata into a fixed-size struct.
fn sys_stat(path: u64, length: u64, info_ptr: u64) -> u64 {
    let Some((start, length)) = user_range(path, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the path lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    let cwd = current_cwd().unwrap_or_else(|| String::from("/"));
    let Ok(info) = crate::vfs::stat(&cwd, path) else {
        return u64::MAX;
    };
    let size = core::mem::size_of::<crate::vfs::Stat>();
    // SAFETY: `info` is a plain `repr(C)` struct with every field initialized.
    let raw = unsafe { core::slice::from_raw_parts((&raw const info).cast::<u8>(), size) };
    if copy_to_user(info_ptr, raw) {
        0
    } else {
        u64::MAX
    }
}

/// `readdir(path, length, buffer)`: writes NUL-separated names into a fixed buffer.
fn sys_readdir(path: u64, length: u64, buffer: u64) -> u64 {
    let Some((start, length)) = user_range(path, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the path lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    let cwd = current_cwd().unwrap_or_else(|| String::from("/"));
    let Ok(names) = crate::vfs::read_dir(&cwd, path) else {
        return u64::MAX;
    };
    let encoded = crate::vfs::encode_names(&names);
    if encoded.len() > crate::vfs::READDIR_BYTES || !copy_to_user(buffer, &encoded) {
        return u64::MAX;
    }
    encoded.len() as u64
}

/// `chdir(path, length)`: changes the caller's current directory.
fn sys_chdir(path: u64, length: u64) -> u64 {
    let Some((start, length)) = user_range(path, length) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the path lies inside the mapped user window.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Ok(path) = core::str::from_utf8(bytes) else {
        return u64::MAX;
    };
    let cwd = current_cwd().unwrap_or_else(|| String::from("/"));
    let Ok(absolute) = crate::vfs::absolute_path(&cwd, path) else {
        return u64::MAX;
    };
    let is_dir =
        crate::vfs::stat_absolute(&absolute).is_ok_and(|info| info.kind == crate::vfs::KIND_DIR);
    if is_dir && set_current_cwd(absolute) {
        0
    } else {
        u64::MAX
    }
}

/// `getcwd(buffer, length)`: writes the caller's current directory, NUL-terminated.
fn sys_getcwd(buffer: u64, length: u64) -> u64 {
    let Some(cwd) = current_cwd() else {
        return u64::MAX;
    };
    let mut encoded = cwd.into_bytes();
    encoded.push(0);
    let needed = encoded.len() as u64;
    if length < needed || !copy_to_user(buffer, &encoded) {
        return u64::MAX;
    }
    needed
}

/// `fbinfo(info)`: copies the framebuffer geometry into a user buffer.
fn sys_fbinfo(info_ptr: u64) -> u64 {
    let size = core::mem::size_of::<FramebufferInfo>() as u64;
    let Some((start, _)) = user_range(info_ptr, size) else {
        return u64::MAX;
    };
    let info = crate::console::framebuffer_info();
    if info.width == 0 || info.byte_len == 0 {
        return u64::MAX;
    }
    let pixel_format = match info.pixel_format {
        PixelFormat::Rgb => 1,
        PixelFormat::Bgr => 2,
        _ => 0,
    };
    let out = FramebufferInfo {
        width: u32::try_from(info.width).unwrap_or(u32::MAX),
        height: u32::try_from(info.height).unwrap_or(u32::MAX),
        stride: u32::try_from(info.stride).unwrap_or(u32::MAX),
        bytes_per_pixel: u32::try_from(info.bytes_per_pixel).unwrap_or(u32::MAX),
        pixel_format,
        _reserved: 0,
        byte_len: u64::try_from(info.byte_len).unwrap_or(u64::MAX),
    };
    prepare_user_write(info_ptr, size as usize);
    // SAFETY: `user_range` checked the whole struct, and COW pages were resolved
    // above. The layout is plain integer fields with no Rust enum representation.
    unsafe { core::ptr::write_unaligned(start as *mut FramebufferInfo, out) };
    0
}

/// `fbcommit(buffer, length)`: copies a user shadow buffer to the screen.
///
/// The kernel never exposes its own framebuffer pointer to ring 3. A guest
/// allocates a normal heap buffer, fills it with pixels, and commits it here;
/// the copy is bounded by both the user range and the real framebuffer length.
fn sys_fbcommit(buffer: u64, length: u64) -> u64 {
    let info = crate::console::framebuffer_info();
    let byte_len = u64::try_from(info.byte_len).unwrap_or(u64::MAX);
    if byte_len == 0 || length < byte_len {
        return u64::MAX;
    }
    let Some((start, _)) = user_range(buffer, byte_len) else {
        return u64::MAX;
    };
    let target = crate::console::framebuffer_buffer();
    if target.is_empty() {
        return u64::MAX;
    }
    let count = usize::try_from(byte_len)
        .unwrap_or(usize::MAX)
        .min(target.len());
    // SAFETY: `user_range` proved the source bytes lie in the caller's mapped
    // user region, and `count` is bounded by the kernel framebuffer slice. Ring
    // 0 interrupts are masked by the `int 0x80` gate for the whole copy.
    let source = unsafe { core::slice::from_raw_parts(start as *const u8, count) };
    target[..count].copy_from_slice(source);
    count as u64
}

/// `input(event)`: writes one queued decoded input event to a user struct.
///
/// Polling the async executor first is essential: while a guest runs, the shell
/// is blocked inside `run` and nobody else decodes the raw scancode queue.
fn sys_input(event_ptr: u64) -> u64 {
    let size = core::mem::size_of::<crate::input::Event>() as u64;
    let Some((start, _)) = user_range(event_ptr, size) else {
        return u64::MAX;
    };
    prepare_user_write(event_ptr, size as usize);
    crate::task::poll();
    let Some(event) = crate::input::pop_user_event() else {
        return 0;
    };
    // SAFETY: `user_range` checked the whole struct and COW was resolved above.
    unsafe { core::ptr::write_unaligned(start as *mut crate::input::Event, event) };
    1
}

/// `wincreate(description)`: reserves a window for the current process.
fn sys_wincreate(description: u64) -> u64 {
    let size = core::mem::size_of::<WindowCreate>() as u64;
    let Some((start, _)) = user_range(description, size) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` checked all sixteen bytes, and a user reading its own
    // mapping cannot trigger a copy-on-write fault.
    let description = unsafe { core::ptr::read_unaligned(start as *const WindowCreate) };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    crate::compositor::create(
        pid.as_u64(),
        u64::from(description.x),
        u64::from(description.y),
        u64::from(description.width),
        u64::from(description.height),
        description.layer,
    )
    .unwrap_or(u64::MAX)
}

/// `wincommit(id, buffer, length)`: blits a user image into its window.
fn sys_wincommit(id: u64, buffer: u64, length: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let owner = pid.as_u64();
    let Some(needed) = crate::compositor::commit_length(owner, id) else {
        return u64::MAX;
    };
    let needed_u64 = u64::try_from(needed).unwrap_or(u64::MAX);
    if length < needed_u64 {
        return u64::MAX;
    }
    let Some((start, _)) = user_range(buffer, needed_u64) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the whole needed range lies in the caller's
    // user region; the compositor copies at most that many bytes.
    let source = unsafe { core::slice::from_raw_parts(start as *const u8, needed) };
    crate::compositor::commit(owner, id, source).unwrap_or(u64::MAX)
}

/// `winpresent()`: blits the composited shadow frame to the display.
fn sys_winpresent() -> u64 {
    if crate::compositor::present() {
        0
    } else {
        u64::MAX
    }
}

/// `windestroy(id)`: releases a window owned by the current process.
fn sys_windestroy(id: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    if crate::compositor::destroy(pid.as_u64(), id) {
        0
    } else {
        u64::MAX
    }
}

/// `wincommitrect(id, buffer, damage)`: updates one window subrectangle.
///
/// `damage` is relative to the window and also carries the byte stride of the
/// caller's source image, so a toolkit can update a small area of a larger
/// shadow buffer without repacking rows.
fn sys_wincommit_rect(id: u64, buffer: u64, damage_ptr: u64) -> u64 {
    let size = core::mem::size_of::<WindowDamage>() as u64;
    let Some((start, _)) = user_range(damage_ptr, size) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` checked the whole struct and a user reading its own
    // mapping cannot trigger copy-on-write.
    let raw = unsafe { core::ptr::read_unaligned(start as *const WindowDamage) };
    let damage = crate::compositor::Damage {
        x: usize::try_from(raw.x).unwrap_or(usize::MAX),
        y: usize::try_from(raw.y).unwrap_or(usize::MAX),
        width: usize::try_from(raw.width).unwrap_or(usize::MAX),
        height: usize::try_from(raw.height).unwrap_or(usize::MAX),
        stride: usize::try_from(raw.stride).unwrap_or(usize::MAX),
    };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let owner = pid.as_u64();
    let Some(needed) = crate::compositor::damage_length(owner, id, damage) else {
        return u64::MAX;
    };
    let needed_u64 = u64::try_from(needed).unwrap_or(u64::MAX);
    let Some((start, _)) = user_range(buffer, needed_u64) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the whole needed range lies in the caller's
    // user region; the compositor copies at most that many bytes.
    let source = unsafe { core::slice::from_raw_parts(start as *const u8, needed) };
    crate::compositor::commit_damage(owner, id, source, damage).unwrap_or(u64::MAX)
}

/// `winsetlayer(id, layer)`: changes the stacking order of one window.
fn sys_winsetlayer(id: u64, layer: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let layer = u32::try_from(layer).unwrap_or(u32::MAX);
    if crate::compositor::set_layer(pid.as_u64(), id, layer) {
        0
    } else {
        u64::MAX
    }
}

/// `winpost(id, buffer, length)`: queues a message for any live window.
fn sys_winpost(id: u64, buffer: u64, length: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Ok(length) = usize::try_from(length) else {
        return u64::MAX;
    };
    let Some((start, _)) = user_range(buffer, length as u64) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the whole source range lies in the caller's
    // user region; the compositor copies at most that many bytes.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    if crate::compositor::post(pid.as_u64(), id, bytes) {
        0
    } else {
        u64::MAX
    }
}

/// `winread(id, message, capacity)`: reads one message from an owned window.
///
/// Returns the payload length, 0 when the mailbox is empty, or `u64::MAX` on
/// error. The caller's buffer must fit the whole [`WindowMessage`] struct.
fn sys_winread(id: u64, message_ptr: u64, capacity: u64) -> u64 {
    let size = core::mem::size_of::<WindowMessage>() as u64;
    if capacity < size {
        return u64::MAX;
    }
    let Some((start, _)) = user_range(message_ptr, size) else {
        return u64::MAX;
    };
    prepare_user_write(message_ptr, size as usize);
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Some(message) = crate::compositor::read(pid.as_u64(), id) else {
        return 0;
    };
    let length = message.len as u64;
    let mut data = [0u8; 64];
    data[..message.len].copy_from_slice(&message.data[..message.len]);
    let output = WindowMessage {
        sender: message.sender,
        length,
        data,
    };
    // SAFETY: `user_range` checked the whole struct and COW was resolved above.
    unsafe { core::ptr::write_unaligned(start as *mut WindowMessage, output) };
    length
}

/// `winsettitle(id, buffer, length)`: sets the title of an owned window.
fn sys_winsettitle(id: u64, buffer: u64, length: u64) -> u64 {
    let Ok(length) = usize::try_from(length) else {
        return u64::MAX;
    };
    if length > 64 {
        return u64::MAX;
    }
    let Some((start, _)) = user_range(buffer, length as u64) else {
        return u64::MAX;
    };
    // SAFETY: `user_range` proved the whole source range lies in the caller's
    // user region; the compositor copies at most that many bytes.
    let bytes = unsafe { core::slice::from_raw_parts(start as *const u8, length) };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    if crate::compositor::set_title(pid.as_u64(), id, bytes) {
        0
    } else {
        u64::MAX
    }
}

/// `shmcreate(length)`: reserves a shared region owned by the caller.
fn sys_shmcreate(length: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    crate::shm::create(pid.as_u64(), length).unwrap_or(u64::MAX)
}

/// `shmattach(id)`: maps a shared region into the caller's address space.
fn sys_shmattach(id: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    crate::shm::attach(pid, id).unwrap_or(u64::MAX)
}

/// `shmdetach(id)`: removes a shared mapping from the caller.
fn sys_shmdetach(id: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    if crate::shm::detach(pid, id) {
        0
    } else {
        u64::MAX
    }
}

/// `shmdestroy(id)`: retires a shared region owned by the caller.
fn sys_shmdestroy(id: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    if crate::shm::destroy(pid.as_u64(), id) {
        0
    } else {
        u64::MAX
    }
}

/// `shmbind(fd, region)`: uses shared frames as a scheme stream's byte ring.
fn sys_shmbind(fd: u64, region: u64) -> u64 {
    let Some(mapping) = crate::shm::stream_mapping(region) else {
        return u64::MAX;
    };
    let bound = with_current_fds_notify(|fds| fds.bind_shm(fd, mapping)).unwrap_or(false);
    if bound { 0 } else { u64::MAX }
}

/// `winbindshm(id, region)`: binds shared frames as a window's pixel store.
fn sys_winbindshm(id: u64, region: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let owner = pid.as_u64();
    let Some(needed) = crate::compositor::commit_length(owner, id) else {
        return u64::MAX;
    };
    let Some(mapping) = crate::shm::window_mapping(region, needed) else {
        return u64::MAX;
    };
    if crate::compositor::bind_shm(owner, id, mapping) {
        0
    } else {
        u64::MAX
    }
}

/// `signal(signum, disposition)`: installs a handler and returns the old one.
///
/// `0` resets to the default action and `1` explicitly ignores the signal. Any
/// other value must be a handler address in the caller's user window.
fn sys_signal(signum: u64, disposition: u64) -> u64 {
    let Ok(signum) = u8::try_from(signum) else {
        return u64::MAX;
    };
    if !crate::signal::SignalState::valid(signum) {
        return u64::MAX;
    }
    // A handler must live in the caller's user window; delivery checks this too,
    // but rejecting bad addresses early keeps the syscall contract predictable.
    if disposition > crate::signal::SIG_IGN && !(USER_BASE..USER_REGION_END).contains(&disposition)
    {
        return u64::MAX;
    }
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    crate::process::with_process_mut(pid, |process| {
        process.signals_mut().set_handler(signum, disposition)
    })
    .unwrap_or(u64::MAX)
}

/// `kill(pid, signum, info)`: queues a signal for another process or the caller.
///
/// A null `info` pointer means a plain `kill` with an implicit `SI_USER` payload.
/// Otherwise `info` points at a [`crate::signal::Siginfo`] whose `si_value` is
/// carried through to the handler; this is the clean-room `sigqueue` path and
/// deliberately reuses syscall 17 so the 0..=51 syscall range stays fixed.
fn sys_kill(pid: u64, signum: u64, info_pointer: u64) -> u64 {
    let Ok(signum) = u8::try_from(signum) else {
        return u64::MAX;
    };
    if !crate::signal::SignalState::valid(signum) {
        return u64::MAX;
    }
    let Some(target) = crate::process::Pid::from_u64(pid) else {
        return u64::MAX;
    };
    let sender = current_pid().map_or(0, crate::process::Pid::as_u64);
    let info = if info_pointer == 0 {
        crate::signal::Siginfo::user(signum, sender)
    } else {
        let Some((start, _)) = user_range(info_pointer, SIGINFO_BYTES) else {
            return u64::MAX;
        };
        prepare_user_write(info_pointer, SIGINFO_BYTES as usize);
        // SAFETY: `user_range` proved the whole struct lies in this process's
        // user window, and COW was resolved above.
        let mut info: crate::signal::Siginfo =
            unsafe { (start as *const crate::signal::Siginfo).read_unaligned() };
        info.si_signo = u64::from(signum);
        info.si_code = crate::signal::SI_QUEUE;
        info.si_pid = sender;
        info
    };
    let queued = crate::process::with_process_mut(target, |process| {
        if matches!(process.state(), crate::process::State::Exited(_)) {
            return None;
        }
        Some(process.signals_mut().queue(signum, info))
    })
    .flatten();
    match queued {
        Some(crate::signal::QueueResult::Queued) => {
            crate::process::wake_for_signal(target);
            0
        }
        // `kill` succeeds even for ignored signals: the send was accepted, its
        // disposition simply discards it.
        Some(crate::signal::QueueResult::Ignored) => 0,
        Some(crate::signal::QueueResult::Full) | None => u64::MAX,
    }
}

/// `sigprocmask(how, set, oldset)`: adjusts or queries the caller's 64-bit mask.
fn sys_sigprocmask(how: u64, set_pointer: u64, old_pointer: u64) -> u64 {
    let Ok(how) = u8::try_from(how) else {
        return u64::MAX;
    };
    let change = if set_pointer == 0 {
        None
    } else {
        let Some((start, _)) = user_range(set_pointer, 8) else {
            return u64::MAX;
        };
        prepare_user_write(set_pointer, 8);
        // SAFETY: `user_range` proved the 8-byte slot lies in the mapped user
        // region and COW was resolved above.
        Some(unsafe { (start as *const u64).read_unaligned() })
    };
    if old_pointer != 0 && user_range(old_pointer, 8).is_none() {
        return u64::MAX;
    }
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Some(previous) = crate::process::with_process_mut(pid, |process| {
        let signals = process.signals_mut();
        let previous = signals.mask();
        if let Some(mask) = change {
            match how {
                0 => {
                    signals.block(mask);
                }
                1 => {
                    signals.unblock(mask);
                }
                2 => {
                    signals.set_mask(mask);
                }
                _ => return None,
            }
        } else if how > 2 {
            return None;
        }
        Some(previous)
    })
    .flatten() else {
        return u64::MAX;
    };
    if old_pointer != 0 {
        let Some((start, _)) = user_range(old_pointer, 8) else {
            return u64::MAX;
        };
        prepare_user_write(old_pointer, 8);
        // SAFETY: validated above; the slot lies in this process's mapped user
        // region and any COW page has been resolved.
        unsafe { (start as *mut u64).write_unaligned(previous) };
    }
    0
}

/// `sigpending(set)`: writes the caller's 64-bit pending signal set.
fn sys_sigpending(pointer: u64) -> u64 {
    let Some((start, _)) = user_range(pointer, 8) else {
        return u64::MAX;
    };
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Some(pending) =
        crate::process::with_process(pid, |process| process.signals().pending_bits())
    else {
        return u64::MAX;
    };
    prepare_user_write(pointer, 8);
    // SAFETY: `user_range` proved the 8-byte slot lies in the mapped user
    // region and COW was resolved above.
    unsafe { (start as *mut u64).write_unaligned(pending) };
    0
}

/// `signalfd(mask, flags)`: creates a descriptor that reads pending signals.
///
/// Every signal in `mask` must already be blocked, matching the kernel's
/// `sigtimedwait` rule. `SFD_NONBLOCK` makes empty reads return `ERROR`, and
/// `SFD_CLOEXEC` closes the descriptor on `exec`.
fn sys_signalfd(mask_pointer: u64, flags: u64) -> u64 {
    if flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return u64::MAX;
    }
    let Some((start, _)) = user_range(mask_pointer, 8) else {
        return u64::MAX;
    };
    prepare_user_write(mask_pointer, 8);
    // SAFETY: `user_range` proved the 8-byte mask lies in the mapped user
    // region and COW was resolved above.
    let raw_mask = unsafe { (start as *const u64).read_unaligned() };
    let mask = raw_mask & !crate::signal::signal_bit(crate::signal::SIGKILL);
    if mask == 0 {
        return u64::MAX;
    }
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let blocked =
        crate::process::with_process(pid, |process| process.signals().mask() & mask == mask)
            .unwrap_or(false);
    if !blocked {
        return u64::MAX;
    }
    let nonblocking = flags & SFD_NONBLOCK != 0;
    let cloexec = flags & SFD_CLOEXEC != 0;
    with_current_fds(|fds| fds.insert_signalfd(mask, nonblocking, cloexec))
        .and_then(Result::ok)
        .unwrap_or(u64::MAX)
}

/// `sigtimedwait(set, info, timeout_ms)`: consumes a blocked signal from `set`.
///
/// Returns the signal number, `ERROR` on timeout or bad arguments, and the
/// internal [`SIGWAIT_RETRY`] value while blocked. The guest SDK wrapper hides
/// the retry value by yielding and calling again after a signal wakes the task.
fn sys_sigtimedwait(set_pointer: u64, info_pointer: u64, timeout_ms: u64, frame: *mut u64) -> u64 {
    let Some((set_start, _)) = user_range(set_pointer, 8) else {
        return u64::MAX;
    };
    if info_pointer != 0 && user_range(info_pointer, SIGINFO_BYTES).is_none() {
        return u64::MAX;
    }
    let raw_mask = unsafe { (set_start as *const u64).read_unaligned() };
    // Every bit maps to signals 1..=64. `SIGKILL` can neither be blocked nor
    // waited for, so it is never eligible for consumption here.
    let mask = raw_mask & !crate::signal::signal_bit(crate::signal::SIGKILL);
    if mask == 0 {
        return u64::MAX;
    }
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    // POSIX requires the waited signals to be blocked so a handler cannot race
    // the waiter. Enforce that here rather than silently stealing a delivery.
    let Some(mask) = crate::process::with_process_mut(pid, |process| {
        let signals = process.signals_mut();
        (signals.mask() & mask == mask).then_some(mask)
    })
    .flatten() else {
        return u64::MAX;
    };

    let take = || {
        crate::process::with_process_mut(pid, |process| process.signals_mut().take_for_wait(mask))
            .flatten()
    };
    if let Some((signum, info)) = take() {
        return if write_siginfo(info_pointer, info) {
            u64::from(signum)
        } else {
            u64::MAX
        };
    }
    if timeout_ms == 0 {
        return u64::MAX;
    }
    let deadline = if timeout_ms == u64::MAX {
        u64::MAX
    } else {
        if timeout_ms > 60_000 {
            return u64::MAX;
        }
        let hz = crate::interrupts::TICK_HZ as u64;
        let delta = timeout_ms.saturating_mul(hz).div_ceil(1000).max(1);
        crate::interrupts::ticks().saturating_add(delta)
    };

    scheduler::block_current_for_signal(pid, mask, deadline, frame);
    // `block_current` returns here only when no peer could be scheduled. A
    // signal may still have arrived before the timeout; consume it before
    // reporting failure.
    if let Some((signum, info)) = take() {
        return if write_siginfo(info_pointer, info) {
            u64::from(signum)
        } else {
            u64::MAX
        };
    }
    unsafe { *frame = u64::MAX };
    u64::MAX
}

/// Copies a kernel `Siginfo` into a validated user buffer.
fn write_siginfo(pointer: u64, info: crate::signal::Siginfo) -> bool {
    if pointer == 0 {
        return true;
    }
    let Some((start, _)) = user_range(pointer, SIGINFO_BYTES) else {
        return false;
    };
    prepare_user_write(pointer, SIGINFO_BYTES as usize);
    // SAFETY: `user_range` proved the whole structure lies in the mapped user
    // region and COW was resolved above.
    unsafe { (start as *mut crate::signal::Siginfo).write_unaligned(info) };
    true
}

/// Restores the register frame saved before a handler was entered.
fn sys_sigreturn(frame_address: u64, frame: *mut u64) -> u64 {
    let Some((start, _)) = user_range(frame_address, SIGNAL_FRAME_BYTES) else {
        return u64::MAX;
    };
    let mut slots = [0; SYSCALL_FRAME_SLOTS];
    // SAFETY: `user_range` proved the frame lies inside the user window, and the
    // kernel wrote these slots when it entered the handler.
    unsafe { core::ptr::copy_nonoverlapping(start as *const u64, slots.as_mut_ptr(), slots.len()) };

    // Do not let a handler forge a ring-0 return frame or a kernel stack.
    if slots[16] != u64::from(USER_CODE_SEL)
        || slots[19] != u64::from(USER_DATA_SEL)
        || !(USER_BASE..USER_REGION_END).contains(&slots[15])
        || !(USER_BASE..=USER_REGION_END).contains(&slots[18])
    {
        return u64::MAX;
    }

    if let Some(pid) = current_pid() {
        let _ = crate::process::with_process_mut(pid, |process| {
            process.signals_mut().set_in_handler(false);
        });
    }
    // SAFETY: `frame` is the live register frame and both buffers hold exactly
    // `SYSCALL_FRAME_SLOTS` values.
    unsafe { core::ptr::copy_nonoverlapping(slots.as_ptr(), frame, slots.len()) };
    // The syscall stub writes its return value into slot 0 after this function
    // returns, so returning the saved `rax` preserves it exactly.
    slots[0]
}

/// The action selected for the current process by [`deliver_pending_signal`].
enum PendingDelivery {
    Handler(u8, u64, crate::signal::Siginfo),
    Terminate(i64),
}

/// Delivers a pending signal by rewriting `frame` into a call to its handler.
///
/// Returns the exit status when the selected signal's default action terminates
/// the process. The caller must then switch away or unwind with
/// [`terminate_current`]; this function has already marked the process exited.
pub(crate) fn deliver_pending_signal(frame: *mut u64) -> Option<i64> {
    let pid = current_pid()?;
    let delivery = crate::process::with_process_mut(pid, |process| {
        let signals = process.signals_mut();
        if signals.in_handler() {
            return None;
        }
        let (signum, disposition, info) = signals.take_pending()?;
        match disposition {
            crate::signal::Disposition::Handler(handler) => {
                signals.set_in_handler(true);
                Some(PendingDelivery::Handler(signum, handler, info))
            }
            crate::signal::Disposition::Terminate => {
                Some(PendingDelivery::Terminate(-i64::from(signum)))
            }
            crate::signal::Disposition::Ignore => None,
        }
    })
    .flatten();
    match delivery {
        Some(PendingDelivery::Handler(signum, handler, info)) => {
            if !enter_signal_handler(frame, signum, handler, info) {
                // A bad user stack must not leave the process stuck "in handler".
                let _ = crate::process::with_process_mut(pid, |process| {
                    process.signals_mut().set_in_handler(false);
                });
            }
            None
        }
        Some(PendingDelivery::Terminate(status)) => {
            crate::process::mark_exited(pid, status);
            Some(status)
        }
        None => None,
    }
}

/// Finishes a fatal default signal action from a syscall or interrupt frame.
pub(crate) fn terminate_current(frame: *mut u64, status: i64) -> ! {
    scheduler::exit_current(frame);
    // SAFETY: `exit_user` restores the kernel stack saved by `enter_user_mode`.
    // The process has already been marked exited, so returning to ring 3 would be
    // invalid whether or not another process was available to schedule.
    unsafe { exit_user(status as u64) }
}

/// Resumes a live syscall/timer/fault frame.
///
/// # Safety
///
/// `frame` must point at a live 20-slot register frame on the current kernel
/// stack. This never returns.
pub(crate) unsafe fn resume_fault_frame(frame: *const u64) -> ! {
    // SAFETY: caller guarantees `frame` is a live 20-slot syscall/timer/fault
    // frame on the current kernel stack.
    unsafe {
        core::arch::asm!(
            "mov rsp, {frame}",
            "pop rax",
            "pop rcx",
            "pop rdx",
            "pop rsi",
            "pop rdi",
            "pop r8",
            "pop r9",
            "pop r10",
            "pop r11",
            "pop rbx",
            "pop rbp",
            "pop r12",
            "pop r13",
            "pop r14",
            "pop r15",
            "iretq",
            frame = in(reg) frame,
            options(noreturn),
        );
    }
}

/// Builds a synthetic handler call on the process's user stack.
///
/// The handler receives `(signum, saved_frame, siginfo)`: `rdi`, `rsi` and `rdx`
/// respectively. `siginfo` lives immediately after the saved register frame and
/// is not part of `sigreturn`'s 20-slot ABI.
fn enter_signal_handler(
    frame: *mut u64,
    signum: u8,
    handler: u64,
    info: crate::signal::Siginfo,
) -> bool {
    if !(USER_BASE..USER_REGION_END).contains(&handler) {
        return false;
    }
    let mut slots = [0; SYSCALL_FRAME_SLOTS];
    // SAFETY: `frame` is the live register frame with exactly this many slots.
    unsafe { core::ptr::copy_nonoverlapping(frame, slots.as_mut_ptr(), slots.len()) };

    let old_rsp = slots[18];
    if !(USER_BASE..=USER_REGION_END).contains(&old_rsp) {
        return false;
    }
    let reserve = SIGNAL_FRAME_BYTES + SIGINFO_BYTES + 8;
    let Some(reserved) = old_rsp.checked_sub(reserve) else {
        return false;
    };
    // SysV entry alignment: a `call` leaves rsp % 16 == 8 in the callee.
    let new_rsp = reserved & !0xf;
    let context_address = new_rsp + 8;
    let info_address = context_address + SIGNAL_FRAME_BYTES;
    if info_address + SIGINFO_BYTES > USER_REGION_END {
        return false;
    }

    prepare_user_write(new_rsp, reserve as usize);
    // SAFETY: the range was checked against the user window and COW pages were
    // resolved above. The kernel writes a fake return slot, the saved frame and
    // its immediately following `Siginfo`.
    unsafe {
        *(new_rsp as *mut u64) = 0;
        core::ptr::copy_nonoverlapping(slots.as_ptr(), context_address as *mut u64, slots.len());
        (info_address as *mut crate::signal::Siginfo).write_unaligned(info);
    }

    slots[0] = 0;
    slots[2] = info_address;
    slots[3] = context_address;
    slots[4] = u64::from(signum);
    slots[15] = handler;
    slots[18] = new_rsp;
    // SAFETY: `frame` is the live register frame with exactly this many slots.
    unsafe { core::ptr::copy_nonoverlapping(slots.as_ptr(), frame, slots.len()) };
    true
}

/// `brk(address)`: grows the process heap to `address` and returns the new
/// break. Passing 0 queries the current break. The break never moves backwards
/// and stays inside the process's user region.
fn sys_brk(address: u64) -> u64 {
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let current =
        crate::process::with_process(pid, crate::process::Process::brk).unwrap_or(u64::MAX);
    if current == u64::MAX || address == 0 || address <= current {
        return current;
    }
    if address > USER_BRK_END {
        return current;
    }

    let old_page = align_up(current, PAGE_BYTES);
    let new_break = align_up(address, PAGE_BYTES);
    crate::process::with_process_mut(pid, |process| {
        let mut page = old_page;
        while page < new_break {
            if process
                .address_space_mut()
                .map_user_page(page, true, false)
                .is_err()
            {
                return process.brk();
            }
            page += PAGE_BYTES;
        }
        process.set_brk(new_break);
        new_break
    })
    .unwrap_or(u64::MAX)
}

/// `fork()`: clones the caller's address space with copy-on-write pages and
/// registers a child.
///
/// The child's saved context is the live syscall frame with `rax` changed to 0,
/// so it resumes just after `int 0x80` exactly like a normal fork. No switch
/// happens here; the scheduler picks the child on the next `yield` or `exit`.
fn sys_fork(frame: *mut u64) -> u64 {
    let Some(parent_pid) = current_pid() else {
        return u64::MAX;
    };
    // SAFETY: `frame` is the 20-slot register frame built by
    // `syscall_interrupt`; `fork_current` installs the child's own CR3 below.
    let context = unsafe { crate::context::Context::from_frame(frame, 0) };
    match crate::process::fork_current(parent_pid, context) {
        Ok(child_pid) => child_pid.as_u64(),
        Err(_) => u64::MAX,
    }
}

/// `exec(image, length, args)`: replaces the calling process's image with a flat
/// or ELF program supplied in a user buffer.
///
/// `args` is zero or a pointer to a length-prefixed packed argument block. Both
/// buffers are copied into kernel memory before the old address space is
/// abandoned, so they may live in that old space. Success never returns:
/// `resume_context` enters the new image with `iretq`.
fn sys_exec(image: u64, length: u64, args: u64) -> u64 {
    let Some((start, length)) = user_range(image, length) else {
        return u64::MAX;
    };
    if length == 0 || length > MAX_ELF_BYTES {
        return u64::MAX;
    }
    // SAFETY: `user_range` proved the buffer lies in the caller's mapped user
    // region. Copy it before any page tables change.
    let bytes: Vec<u8> =
        unsafe { Vec::from(core::slice::from_raw_parts(start as *const u8, length)) };
    let Ok(packed_args) = copy_startup_block(args) else {
        return u64::MAX;
    };

    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let Ok(mut space) = crate::address_space::AddressSpace::new() else {
        return u64::MAX;
    };

    let is_flat = !bytes.starts_with(&[0x7f, b'E', b'L', b'F']);
    let (entry, stack_top, brk_start) = if is_flat {
        if bytes.len() > MAX_PROGRAM_BYTES {
            return u64::MAX;
        }
        for page in 0..CODE_PAGES + STACK_PAGES {
            let address = USER_BASE + page * PAGE_BYTES;
            let Ok(target) = space.map_user_page(address, true, page < CODE_PAGES) else {
                return u64::MAX;
            };
            // SAFETY: `target` is a fresh frame owned by `space` and mapped
            // through the physical-memory window.
            unsafe { core::ptr::write_bytes(target as *mut u8, 0, PAGE_BYTES as usize) };
            if page >= CODE_PAGES {
                continue;
            }
            let offset = (page * PAGE_BYTES) as usize;
            if offset < bytes.len() {
                let count = usize::min(PAGE_BYTES as usize, bytes.len() - offset);
                // SAFETY: the source slice was checked and the destination is a
                // fresh 4 KiB frame mapped through the physical-memory window.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr().add(offset),
                        target as *mut u8,
                        count,
                    );
                }
            }
        }
        (USER_BASE, STACK_TOP, STACK_TOP)
    } else {
        match crate::elf::load(&bytes, &mut space) {
            Ok(image) => (image.entry, image.stack_top, image.brk_start),
            Err(_) => return u64::MAX,
        }
    };

    let Ok(startup) = seed_startup_stack(&mut space, stack_top, &packed_args) else {
        return u64::MAX;
    };
    let cr3 = space.pml4_frame().start_address().as_u64();
    let context = crate::context::Context::initial_with_args(
        entry,
        startup.rsp,
        cr3,
        startup.argc,
        startup.argv,
    );
    let context_ptr = crate::process::with_process_mut(pid, |process| {
        process.fds_mut().close_cloexec();
        process.replace_image(space, brk_start, context);
        process.context_ptr()
    })
    .expect("current process disappeared during exec");

    // SAFETY: `context_ptr` points at the live process's freshly installed
    // context, whose CR3 and user stack are both valid.
    unsafe { crate::context::resume_context(context_ptr) }
}

/// Bit in `waitpid` options that requests a nonblocking status check.
const WNOHANG: u64 = 1;

/// `waitpid(pid, status, options)`: returns `pid` once that process has exited.
///
/// The exit status is written to `status` as a `u64` when the pointer is
/// non-null. A live target arms the caller's readiness registration in the same
/// critical section as the state check and parks until the child exits; a
/// context switch resumes the caller with [`SIGWAIT_RETRY`], which the SDK
/// wrappers hide. `WNOHANG` instead returns zero immediately for a live target.
fn sys_waitpid(pid: u64, status_pointer: u64, options: u64, frame: *mut u64) -> u64 {
    let Some(target) = crate::process::Pid::from_u64(pid) else {
        return u64::MAX;
    };
    let Some(caller) = current_pid() else {
        return u64::MAX;
    };
    if target == caller {
        return u64::MAX;
    }
    let nohang = options & WNOHANG != 0;
    loop {
        match crate::process::check_child_wait(caller, target, !nohang) {
            Some(crate::process::ChildWait::Exited(status)) => {
                let _ = crate::process::with_process_mut(
                    caller,
                    crate::process::Process::clear_readiness_wait,
                );
                if status_pointer != 0 {
                    let Some((start, _)) = user_range(status_pointer, 8) else {
                        return u64::MAX;
                    };
                    prepare_user_write(status_pointer, 8);
                    // SAFETY: `user_range` proved the eight-byte slot lies in
                    // the caller's mapped region, and COW was resolved above.
                    unsafe { (start as *mut u64).write_unaligned(status as u64) };
                }
                crate::process::reap(target);
                return target.as_u64();
            }
            Some(crate::process::ChildWait::Missing) => {
                let _ = crate::process::with_process_mut(
                    caller,
                    crate::process::Process::clear_readiness_wait,
                );
                return u64::MAX;
            }
            Some(crate::process::ChildWait::Pending) if nohang => return 0,
            Some(crate::process::ChildWait::Pending) => {
                scheduler::block_current_until_ready(caller, u64::MAX, frame);
                if current_pid().is_none() {
                    return u64::MAX;
                }
            }
            None => return u64::MAX,
        }
    }
}

/// `sleep(milliseconds)`: blocks the caller, letting peer processes run.
fn sys_sleep(milliseconds: u64, frame: *mut u64) -> u64 {
    if milliseconds == 0 {
        return 0;
    }
    // Keep deadlines small enough to stay far from `u64` wraparound and to
    // catch accidental argument mistakes in hand-assembled programs.
    if milliseconds > 60_000 {
        return u64::MAX;
    }
    let Some(pid) = current_pid() else {
        return u64::MAX;
    };
    let hz = crate::interrupts::TICK_HZ as u64;
    let delta = milliseconds.saturating_mul(hz).div_ceil(1000).max(1);
    let deadline = crate::interrupts::ticks().saturating_add(delta);
    scheduler::sleep_current(pid, deadline, frame);
    0
}

/// `uptime()`: milliseconds since the PIT was initialized.
fn sys_uptime() -> u64 {
    crate::interrupts::ticks().saturating_mul(1000) / crate::interrupts::TICK_HZ as u64
}

/// `clock_gettime()`: monotonic nanoseconds interpolated with the TSC.
fn sys_clock_gettime() -> u64 {
    crate::interrupts::monotonic_ns()
}

/// `realtime()`: Unix seconds from the CMOS RTC, or the cached reading.
fn sys_realtime() -> u64 {
    crate::rtc::unix_seconds().unwrap_or_else(crate::rtc::cached_seconds)
}
