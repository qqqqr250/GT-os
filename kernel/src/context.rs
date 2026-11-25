//! Saved ring-3 context and the low-level context switch.
//!
//! The layout matches the stack `usermode!syscall_interrupt` builds: fifteen
//! general-purpose registers followed by the CPU's `iretq` frame and the
//! process's `CR3`. [`switch_context`] copies the live frame into the outgoing
//! process's [`Context`], loads the incoming context and returns to ring 3 with
//! `iretq`. It never returns to its Rust caller.

use core::arch::global_asm;

/// Number of `u64` slots copied from the syscall frame (15 registers + the
/// five-slot `iretq` frame). This must match the field count of [`Context`].
const FRAME_SLOTS: usize = 20;
/// Byte offset of `cr3` in [`Context`].
const CR3_OFFSET: usize = FRAME_SLOTS * 8;

/// Everything needed to resume a ring-3 process after a context switch.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
    pub cr3: u64,
}

// Catch any future field addition that would make a `Context` wider than the
// 20-slot frame plus its explicit CR3 slot.
const _: () = assert!(core::mem::size_of::<Context>() == (FRAME_SLOTS + 1) * 8);

impl Context {
    /// Reads a saved context from a live syscall/timer register frame.
    ///
    /// The frame has [`FRAME_SLOTS`] entries and does not contain `cr3`; the
    /// caller supplies the address-space value that belongs with the context.
    ///
    /// # Safety
    ///
    /// `frame` must point at [`FRAME_SLOTS`] readable `u64` values, such as the
    /// register frame built by `syscall_interrupt` or `timer_interrupt`.
    #[must_use]
    pub unsafe fn from_frame(frame: *const u64, cr3: u64) -> Self {
        // SAFETY: the caller guarantees the frame has `FRAME_SLOTS` readable
        // entries.
        let slots = unsafe { core::slice::from_raw_parts(frame, FRAME_SLOTS) };
        Self {
            rax: slots[0],
            rcx: slots[1],
            rdx: slots[2],
            rsi: slots[3],
            rdi: slots[4],
            r8: slots[5],
            r9: slots[6],
            r10: slots[7],
            r11: slots[8],
            rbx: slots[9],
            rbp: slots[10],
            r12: slots[11],
            r13: slots[12],
            r14: slots[13],
            r15: slots[14],
            rip: slots[15],
            cs: slots[16],
            rflags: slots[17],
            rsp: slots[18],
            ss: slots[19],
            cr3,
        }
    }

    /// Builds an initial context that enters `entry` with `user_stack` in ring 3.
    #[must_use]
    pub fn initial(entry: u64, user_stack: u64, cr3: u64) -> Self {
        Self::initial_with_args(entry, user_stack, cr3, 0, 0)
    }

    /// Builds an initial context with the C ABI's first two arguments set.
    #[must_use]
    #[allow(clippy::similar_names)]
    pub fn initial_with_args(entry: u64, user_stack: u64, cr3: u64, argc: u64, argv: u64) -> Self {
        Self {
            rax: 0,
            rcx: 0,
            rdx: 0,
            rsi: argv,
            rdi: argc,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: entry,
            cs: crate::usermode::USER_CODE_SEL as u64,
            rflags: 0x202,
            rsp: user_stack,
            ss: crate::usermode::USER_DATA_SEL as u64,
            cr3,
        }
    }
}

global_asm!(
    ".global resume_context",
    "resume_context:",
    // rdi = &Context. Load its CR3 and return directly to ring 3. Used by
    // `exec`, which has no old context worth saving.
    "    mov r11, rdi",
    "    mov rsp, r11",
    "    mov rax, [r11 + {cr3_offset}]",
    "    mov cr3, rax",
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
    cr3_offset = const CR3_OFFSET,
);

global_asm!(
    ".global switch_context",
    "switch_context:",
    // rdi = &mut old, rsi = &new, rdx = live syscall frame
    "    mov r10, rdi",
    "    mov r11, rsi",
    "    mov r12, rdx",
    "    mov rcx, {slots}",
    "1:",
    "    mov rax, [r12]",
    "    mov [r10], rax",
    "    add r12, 8",
    "    add r10, 8",
    "    loop 1b",
    // r10 now points at the old context's cr3 slot.
    "    mov rax, cr3",
    "    mov [r10], rax",
    // Load the incoming context. CR3 reload also flushes user TLB entries.
    "    mov rsp, r11",
    "    mov rax, [r11 + {cr3_offset}]",
    "    mov cr3, rax",
    // Pop in the order the syscall stub pushed, then let iretq restore the CPU
    // frame (RIP, CS, RFLAGS, RSP, SS).
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
    slots = const FRAME_SLOTS,
    cr3_offset = const CR3_OFFSET,
);

unsafe extern "C" {
    /// Loads `new` and resumes it in ring 3 without saving the current context.
    pub fn resume_context(new: *const Context) -> !;
    /// Saves `frame` into `old`, loads `new` and resumes it in ring 3.
    pub fn switch_context(old: *mut Context, new: *const Context, frame: *const u64) -> !;
}
