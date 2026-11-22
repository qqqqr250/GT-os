//! Round-robin process scheduler.
//!
//! The scheduler runs both inside the `int 0x80` handler (when a process yields
//! or exits) and inside the PIT handler (when a tick interrupts ring 3). The
//! live register frame is copied into the outgoing process's [`Context`], another
//! ready process is chosen round-robin, and `switch_context` resumes it with
//! `iretq`. Ticks that interrupt ring 0 only keep time; they never preempt the
//! shell.

use crate::context::{self, Context};
use crate::process::{self, Pid, State};
use crate::usermode;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::interrupts;
use x86_64::registers::control::Cr3;

/// Round-robin starting point for the next `pick_next`.
static CURSOR: AtomicU64 = AtomicU64::new(0);

/// Called by the `yield` syscall. Switches away if another process is ready.
pub fn yield_now(frame: *mut u64) -> u64 {
    let Some(current) = usermode::current_pid() else {
        return 0;
    };
    // The dispatcher would write 0 into `rax` after returning, but a context
    // switch copies the frame first. Set it now so the resumed process sees 0.
    // SAFETY: `frame` is the live syscall register frame.
    unsafe { *frame = 0 };
    if let Some(next) = pick_next(current) {
        // SAFETY: `frame` is the live syscall stack frame and `next` is a ready
        // process with a valid saved context and private kernel stack.
        unsafe { switch(current, next, frame) };
    }
    0
}

/// Called by the PIT handler when a tick interrupts ring 3.
///
/// # Safety
///
/// `frame` must point at the live register frame built by
/// [`usermode::timer_interrupt`]. If another process is ready this never
/// returns; otherwise the caller restores the frame and returns from the tick.
pub unsafe fn preempt(frame: *mut u64) {
    let Some(current) = usermode::current_pid() else {
        return;
    };
    if let Some(next) = pick_next(current) {
        // SAFETY: `frame` is the live interrupt frame and `next` has a valid
        // saved context, kernel stack and page table.
        unsafe { switch(current, next, frame) };
    }
}

/// Blocks `pid` until the absolute PIT tick `deadline`.
///
/// # Safety
///
/// `frame` must point at the live register frame built by `syscall_interrupt`.
/// If another process is runnable this never returns; the sleeper resumes later
/// from its saved context. Otherwise the kernel waits with `hlt` until the tick
/// arrives and this call returns normally.
pub fn sleep_current(pid: Pid, deadline: u64, frame: *mut u64) {
    if !process::set_sleeping(pid, deadline) {
        // SAFETY: `frame` is the live syscall frame and its first slot is `rax`.
        unsafe { *frame = u64::MAX };
        return;
    }
    // `sleep()` returns 0. A switch copies the frame before the dispatcher can
    // write its return value, so store it here as well.
    // SAFETY: `frame` is the live syscall register frame.
    unsafe { *frame = 0 };
    if let Some(next) = pick_next(pid) {
        // SAFETY: `frame` is the live syscall frame and `next` has a valid
        // saved context, kernel stack and page table.
        unsafe { switch(pid, next, frame) };
    }
    // No peer to run yet: halt until a tick wakes a sleeper or our deadline
    // arrives. A newly runnable peer is worth switching to immediately, so a
    // shorter sleep does not wait for a longer one to expire.
    while crate::interrupts::ticks() < deadline {
        if let Some(next) = pick_next(pid) {
            // SAFETY: `frame` is the live syscall frame of the sleeping process
            // and `next` has a valid saved context, stack and page table.
            unsafe { switch(pid, next, frame) };
        }
        interrupts::enable_and_hlt();
    }
    // If no switch happened we are returning directly to ring 3, so this
    // process is running again rather than merely ready.
    process::with_process_mut(pid, process::Process::set_running);
}

/// Called by the `exit` syscall. Resumes another ready process if one exists;
/// otherwise returns so the caller can unwind to the shell with `exit_user`.
pub fn exit_current(frame: *mut u64) {
    let Some(current) = usermode::current_pid() else {
        return;
    };
    if let Some(next) = pick_next(current) {
        // SAFETY: as in `yield_now`.
        unsafe { switch(current, next, frame) };
    }
    // A synchronous `run` must not return to the shell while a child is still
    // alive: it would clear the process table and discard that child. Wait for
    // sleepers to wake, then hand the CPU to the first one that is ready.
    while process::has_live() {
        if let Some(next) = pick_next(current) {
            // SAFETY: `frame` is the live syscall frame of the exited process
            // and `next` has a valid saved context, stack and page table.
            unsafe { switch(current, next, frame) };
        }
        interrupts::enable_and_hlt();
    }
}

fn pick_next(current: Pid) -> Option<Pid> {
    let pids = process::pids();
    if pids.len() < 2 {
        return None;
    }
    let start = CURSOR.fetch_add(1, Ordering::SeqCst) as usize;
    for offset in 0..pids.len() {
        let candidate = pids[(start + offset) % pids.len()];
        // `Running` and `Ready` processes can run; `Exited` and `Sleeping`
        // processes cannot. The sleeper is woken by the PIT handler.
        if candidate != current
            && process::with_process(candidate, process::Process::is_runnable).unwrap_or(false)
        {
            return Some(candidate);
        }
    }
    None
}

/// Saves `current` and resumes `next`. Never returns.
///
/// # Safety
///
/// `frame` must point at the live syscall register frame. Both processes must
/// still be in the process table and own valid kernel stacks and page tables.
unsafe fn switch(current: Pid, next: Pid, frame: *mut u64) -> ! {
    let (active_pml4, _) = Cr3::read();
    let mut saved = unsafe { *(frame as *const Context) };
    saved.cr3 = active_pml4.start_address().as_u64();

    let old_ptr = process::with_process_mut(current, |process| {
        process.set_context(saved);
        // An exiting process is already `Exited`; a yielding one goes back to
        // the ready queue.
        if process.state() == State::Running {
            process.set_ready();
        }
        process.context_mut_ptr()
    })
    .expect("current process disappeared during switch");

    let (new_ptr, stack_top, pid) = process::with_process_mut(next, |process| {
        process.set_running();
        (
            process.context_ptr(),
            process.kernel_stack_top(),
            process.pid(),
        )
    })
    .expect("next process disappeared during switch");

    // SAFETY: `stack_top` is owned by `next` and lives as long as the process.
    unsafe { usermode::set_kernel_stack(stack_top) };
    usermode::set_current_pid(pid.as_u64());

    // SAFETY: both pointers are stable fields of live process-table entries and
    // `frame` is the stack frame we just copied from.
    unsafe { context::switch_context(old_ptr, new_ptr, frame) }
}
