//! Process control blocks and the process table.
//!
//! A [`Process`] is the kernel's bookkeeping for one ring-3 program: its PID,
//! state, exit status and private [`AddressSpace`]. The table is a fixed-size
//! array of slots, so allocating a PID never touches the heap and a process can
//! be looked up from an interrupt handler without taking a lock that could be
//! held by user code.
//!
//! This is the process foundation of the Unix-like rewrite: PCBs, the fixed
//! process table, private ring-0 stacks and copy-on-write `fork`.

use crate::address_space::AddressSpace;
use crate::context::Context;
use crate::scheme::FdTable;
use crate::signal::SignalState;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::interrupts;

/// Maximum number of processes the kernel can track at once.
pub const MAX_PROCESSES: usize = 16;
/// Size of each process's private ring-0 stack.
///
/// A debug-build `sys_sleep` path uses more than 80 KiB once a PIT tick is
/// nested in it, and the signal-wait state added in 1.55 makes `fork` frames
/// deeper still. 16 KiB and 64 KiB overflowed into adjacent heap blocks and
/// corrupted the allocator; 96 KiB was not enough after 1.55.
const KERNEL_STACK_BYTES: usize = 128 * 1024;

/// A process identifier. PIDs are never reused in a single boot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Pid(u64);

impl Pid {
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::SeqCst))
    }

    /// Rebuilds a PID from a syscall argument.
    pub fn from_u64(value: u64) -> Option<Self> {
        (value > 0).then_some(Self(value))
    }

    /// The numeric value used by syscalls and the shell.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Where a process is in its lifetime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum State {
    /// The process owns an address space and may run.
    Ready,
    /// The process is currently executing in ring 3.
    Running,
    /// The process is blocked until the absolute PIT tick `deadline`.
    Sleeping(u64),
    /// The process has called `exit` and keeps its status until it is reaped.
    Exited(i64),
}

/// One process's kernel-side state.
pub struct Process {
    pid: Pid,
    state: State,
    space: AddressSpace,
    /// The current `brk` value; pages between the initial image end and this
    /// address are mapped writable for the process's heap.
    brk: u64,
    /// The private ring-0 stack the CPU switches to on ring-3 entry.
    /// Kept alive by owning the allocation here.
    kernel_stack: Vec<u8>,
    kernel_stack_top: u64,
    /// Saved register state used by yield, exit and PIT preemption.
    pub(crate) context: Context,
    /// Descriptors opened by this process. 0-2 are the console streams.
    fds: FdTable,
    /// The current directory used to resolve relative VFS paths.
    cwd: String,
    /// Registered handlers and queued signals.
    signals: SignalState,
    /// Readiness generation observed when a `poll`/`select` wait was armed.
    /// `None` means the process is not in an event-driven readiness wait.
    readiness_wait: Option<u64>,
    /// The previous image after `exec`. It cannot be released until the new CR3
    /// is active, so the next syscall drops it at a safe point.
    retired: Option<AddressSpace>,
}

impl Process {
    /// Creates a new process with a fresh address space.
    pub fn new() -> Result<Self, &'static str> {
        Ok(Self::with_space(AddressSpace::new()?, 0))
    }

    /// Builds a process around an existing address space.
    fn with_space(space: AddressSpace, brk: u64) -> Self {
        let mut kernel_stack = vec![0u8; KERNEL_STACK_BYTES];
        let kernel_stack_top = kernel_stack.as_mut_ptr() as u64 + KERNEL_STACK_BYTES as u64;
        Self {
            pid: Pid::next(),
            state: State::Ready,
            space,
            brk,
            kernel_stack,
            kernel_stack_top,
            context: Context::initial(0, 0, 0),
            fds: FdTable::new(),
            cwd: String::from("/"),
            signals: SignalState::new(),
            readiness_wait: None,
            retired: None,
        }
    }

    /// Creates a child with copy-on-write user memory and copied descriptors.
    pub fn fork(&mut self) -> Result<(Self, u64), &'static str> {
        let (space, cr3) = self.space.fork()?;
        let mut child = Self::with_space(space, self.brk);
        child.fds = self.fds.clone();
        child.cwd.clone_from(&self.cwd);
        // Unix children inherit disposition but not pending signals.
        child.signals = self.signals.clone();
        child.signals.clear_pending();
        Ok((child, cr3))
    }

    /// This process's identifier.
    #[must_use]
    pub const fn pid(&self) -> Pid {
        self.pid
    }

    /// This process's current state.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// Records the status passed to `exit` and releases its descriptors.
    pub fn mark_exited(&mut self, status: i64) {
        self.fds.clear();
        crate::scheme::unregister_owner(self.pid.as_u64());
        bump_readiness_generation();
        self.readiness_wait = None;
        self.state = State::Exited(status);
    }

    /// This process's descriptor table.
    #[must_use]
    pub const fn fds(&self) -> &FdTable {
        &self.fds
    }

    /// Mutable access to this process's descriptor table, for the fd syscalls.
    pub fn fds_mut(&mut self) -> &mut FdTable {
        &mut self.fds
    }

    /// This process's current directory for relative VFS paths.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Sets this process's current directory after VFS validation.
    pub fn set_cwd(&mut self, path: String) {
        self.cwd = path;
    }

    /// This process's signal disposition and pending signals.
    #[must_use]
    pub const fn signals(&self) -> &SignalState {
        &self.signals
    }

    /// Mutable signal state, used by the signal syscalls and delivery path.
    pub fn signals_mut(&mut self) -> &mut SignalState {
        &mut self.signals
    }

    /// The process's private page tables.
    #[must_use]
    pub const fn address_space(&self) -> &AddressSpace {
        &self.space
    }

    /// Mutable access to the process's page tables, for mapping program images.
    pub fn address_space_mut(&mut self) -> &mut AddressSpace {
        &mut self.space
    }

    /// The current `brk` value.
    #[must_use]
    pub const fn brk(&self) -> u64 {
        self.brk
    }

    /// Sets the current `brk` after its pages have been mapped.
    pub fn set_brk(&mut self, value: u64) {
        self.brk = value;
    }

    /// The top of this process's private ring-0 stack.
    #[must_use]
    pub const fn kernel_stack_top(&self) -> u64 {
        self.kernel_stack_top
    }

    /// Installs a saved context for this process.
    pub fn set_context(&mut self, context: Context) {
        self.context = context;
    }

    /// Replaces this process's image after an `exec`.
    ///
    /// The old space is retired rather than dropped here: this code is still
    /// executing with the old CR3 until `resume_context` switches to the new one.
    pub fn replace_image(&mut self, space: AddressSpace, brk: u64, context: Context) {
        self.retired = Some(core::mem::replace(&mut self.space, space));
        self.brk = brk;
        self.context = context;
        self.state = State::Running;
    }

    /// Takes a retired image so the caller can release it outside the table lock.
    pub fn take_retired(&mut self) -> Option<AddressSpace> {
        self.retired.take()
    }

    /// Marks the process as currently executing.
    pub fn set_running(&mut self) {
        self.state = State::Running;
    }

    /// Marks the process as runnable again after a yield or a completed sleep.
    pub fn set_ready(&mut self) {
        self.state = State::Ready;
    }

    /// Blocks the process until `deadline`, measured in PIT ticks.
    pub fn set_sleeping(&mut self, deadline: u64) {
        self.state = State::Sleeping(deadline);
    }

    /// Records the readiness generation that a multiplexed wait will sleep on.
    ///
    /// The caller must already hold the process table lock, so the generation is
    /// sampled in the same critical section that checks readiness and changes
    /// the state to `Sleeping`.
    pub fn arm_readiness_wait(&mut self) {
        self.readiness_wait = Some(readiness_generation());
    }

    /// Removes a stale readiness registration.
    pub fn clear_readiness_wait(&mut self) {
        self.readiness_wait = None;
    }

    /// Whether this process can currently be scheduled.
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        matches!(self.state, State::Ready | State::Running)
    }

    /// Pointer to this process's saved context, stable in the process table.
    pub(crate) fn context_mut_ptr(&mut self) -> *mut Context {
        &raw mut self.context
    }

    /// Pointer to this process's saved context, stable in the process table.
    pub(crate) fn context_ptr(&self) -> *const Context {
        &raw const self.context
    }
}

/// Monotonic count of descriptor and signal readiness changes.
///
/// Waiters sample this under the process table lock; mutators advance it after
/// changing readiness. A mismatch means at least one event happened since the
/// waiter checked its descriptors, so the sleeper can be made runnable without
/// waiting for the next PIT tick.
static READINESS_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The fixed set of live processes, indexed by slot rather than PID.
static SLOTS: spin::Mutex<[Option<Process>; MAX_PROCESSES]> =
    spin::Mutex::new([const { None }; MAX_PROCESSES]);

/// Returns the current readiness generation.
fn readiness_generation() -> u64 {
    READINESS_GENERATION.load(Ordering::SeqCst)
}

/// Advances the readiness generation while the caller holds the process table
/// lock. Atomic-only so it is safe to call from inside `with_process_mut`.
pub(crate) fn bump_readiness_generation() {
    READINESS_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Wakes every process whose armed readiness generation is stale.
pub fn wake_readiness_waiters() {
    let generation = readiness_generation();
    with_slots(|slots| {
        for process in slots.iter_mut().flatten() {
            let stale = process
                .readiness_wait
                .is_some_and(|armed| armed != generation)
                && matches!(process.state(), State::Sleeping(_));
            if stale {
                process.clear_readiness_wait();
                process.set_ready();
            }
        }
    });
}

/// Advances readiness and wakes any event-driven multiplexed waiters.
pub fn notify_readiness() {
    bump_readiness_generation();
    wake_readiness_waiters();
}

/// Outcome of one atomic `waitpid` attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildWait {
    /// The target has exited and its status is ready to be consumed.
    Exited(i64),
    /// The target is still live; the caller is armed and sleeping.
    Pending,
    /// The target is no longer in the process table.
    Missing,
}

/// Checks a child's exit state, optionally arming the caller in one critical
/// section.
///
/// A child that exits after a blocking check bumps the readiness generation
/// under the same process-table lock, so the armed caller is woken instead of
/// missing the transition between inspection and sleep. `WNOHANG` callers pass
/// `block = false` and receive [`ChildWait::Pending`] without changing state.
pub fn check_child_wait(caller: Pid, target: Pid, block: bool) -> Option<ChildWait> {
    with_slots(|slots| {
        let caller_index = slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|p| p.pid() == caller))?;
        let target_state = slots
            .iter()
            .find(|slot| slot.as_ref().is_some_and(|p| p.pid() == target))
            .map(|slot| match slot.as_ref().map(Process::state) {
                Some(State::Exited(status)) => Some(status),
                Some(State::Ready | State::Running | State::Sleeping(_)) | None => None,
            });
        let Some(target_state) = target_state else {
            return Some(ChildWait::Missing);
        };
        if let Some(status) = target_state {
            return Some(ChildWait::Exited(status));
        }
        if block {
            let caller_process = slots[caller_index].as_mut()?;
            caller_process.arm_readiness_wait();
            caller_process.set_sleeping(u64::MAX);
        }
        Some(ChildWait::Pending)
    })
}

/// Runs `f` with the process table locked and interrupts disabled.
///
/// The PIT handler also takes this lock. Without `without_interrupts`, a timer
/// tick could arrive while the lock is held, spin in the handler, and prevent
/// the interrupted holder from ever releasing it.
fn with_slots<R>(f: impl FnOnce(&mut [Option<Process>; MAX_PROCESSES]) -> R) -> R {
    interrupts::without_interrupts(|| f(&mut SLOTS.lock()))
}

/// Registers a newly created process and returns its slot index.
pub fn register(process: Process) -> Result<usize, &'static str> {
    with_slots(|slots| {
        let index = slots
            .iter()
            .position(Option::is_none)
            .ok_or("process table is full")?;
        slots[index] = Some(process);
        Ok(index)
    })
}

/// Looks up a process by PID.
pub fn with_process<R>(pid: Pid, f: impl FnOnce(&Process) -> R) -> Option<R> {
    with_slots(|slots| slots.iter().flatten().find(|p| p.pid() == pid).map(f))
}

/// Mutates a process by PID, for syscalls that grow its address space.
pub fn with_process_mut<R>(pid: Pid, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    with_slots(|slots| slots.iter_mut().flatten().find(|p| p.pid() == pid).map(f))
}

/// Forks `parent_pid` and registers the child in one critical section.
///
/// Building and installing the child while the table is locked avoids copying a
/// large `Process` value back through the syscall frame, and guarantees the
/// child is visible to the scheduler before `fork` returns to ring 3.
pub fn fork_current(parent_pid: Pid, mut context: Context) -> Result<Pid, &'static str> {
    with_slots(|slots| {
        let parent = slots
            .iter_mut()
            .flatten()
            .find(|process| process.pid() == parent_pid)
            .ok_or("parent process not found")?;
        let (mut child, cr3) = parent.fork()?;
        context.rax = 0;
        context.cr3 = cr3;
        child.set_context(context);
        let child_pid = child.pid();
        let slot = slots
            .iter()
            .position(Option::is_none)
            .ok_or("process table is full")?;
        slots[slot] = Some(child);
        Ok(child_pid)
    })
}

/// Marks a process as exited with the given status.
pub fn mark_exited(pid: Pid, status: i64) -> bool {
    let exited = with_slots(|slots| {
        if let Some(process) = slots.iter_mut().flatten().find(|p| p.pid() == pid) {
            process.mark_exited(status);
            true
        } else {
            false
        }
    });
    if exited {
        wake_readiness_waiters();
        crate::compositor::release_process(pid.as_u64());
    }
    exited
}

/// Marks a process as exited only if it has not already exited.
///
/// This keeps the first process's own status when a later child is the one that
/// finally unwinds the synchronous `run` call.
pub fn mark_exited_if_live(pid: Pid, status: i64) -> bool {
    let exited = with_slots(|slots| {
        let Some(process) = slots.iter_mut().flatten().find(|p| p.pid() == pid) else {
            return false;
        };
        if matches!(process.state(), State::Exited(_)) {
            return false;
        }
        process.mark_exited(status);
        true
    });
    if exited {
        wake_readiness_waiters();
        crate::compositor::release_process(pid.as_u64());
    }
    exited
}

/// Releases the previous image of a process that has completed an `exec`.
pub fn drop_retired(pid: Pid) {
    let retired = with_process_mut(pid, Process::take_retired).flatten();
    drop(retired);
}

/// Makes a sleeping process runnable so a newly queued signal can be delivered.
pub fn wake_for_signal(pid: Pid) {
    bump_readiness_generation();
    with_process_mut(pid, |process| {
        if matches!(process.state(), State::Sleeping(_)) {
            process.clear_readiness_wait();
            process.set_ready();
        }
    });
}

/// Blocks a process until an absolute PIT tick deadline.
pub fn set_sleeping(pid: Pid, deadline: u64) -> bool {
    with_process_mut(pid, |process| {
        if matches!(process.state(), State::Exited(_)) {
            return false;
        }
        process.set_sleeping(deadline);
        true
    })
    .unwrap_or(false)
}

/// Puts a signal waiter to sleep unless a matching signal is already pending.
///
/// Returns `false` when the process vanished or a signal is ready to consume,
/// in which case the caller retries without switching. Checking the pending set
/// here, while the process table is locked and interrupts are disabled, closes
/// the race between a sender's wakeup and the sleeper's state change.
pub fn set_sleeping_unless_pending(pid: Pid, deadline: u64, mask: u64) -> bool {
    with_process_mut(pid, |process| {
        if matches!(process.state(), State::Exited(_))
            || process.signals().pending_for_mask(mask) != 0
        {
            return false;
        }
        process.set_sleeping(deadline);
        true
    })
    .unwrap_or(false)
}

/// Whether any process is still waiting on a sleep deadline.
#[must_use]
pub fn has_sleepers() -> bool {
    with_slots(|slots| {
        slots
            .iter()
            .flatten()
            .any(|process| matches!(process.state(), State::Sleeping(_)))
    })
}

/// Whether any process has not yet exited.
#[must_use]
pub fn has_live() -> bool {
    with_slots(|slots| {
        slots
            .iter()
            .flatten()
            .any(|process| !matches!(process.state(), State::Exited(_)))
    })
}

/// Makes every sleeper whose deadline has passed runnable again.
pub fn wake_sleepers(now: u64) {
    with_slots(|slots| {
        for slot in slots.iter_mut().flatten() {
            if let State::Sleeping(deadline) = slot.state() {
                if deadline <= now {
                    slot.clear_readiness_wait();
                    slot.set_ready();
                }
            }
        }
    });
}

/// Removes every process from the table after a synchronous `run` completes.
pub fn clear() {
    // Take one process out at a time and drop it after releasing the table
    // lock. Dropping an address space touches other locks and can take long
    // enough for a PIT tick to arrive; no interrupt handler may find the
    // process table locked while it does so. Taking one at a time also keeps
    // the whole table off the kernel stack.
    while let Some(process) = with_slots(|slots| slots.iter_mut().find_map(Option::take)) {
        drop(process);
    }
    notify_readiness();
    crate::compositor::release_all();
}

/// Removes a process from the table, returning its exit status.
pub fn reap(pid: Pid) -> Option<i64> {
    let process = with_slots(|slots| {
        let index = slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|p| p.pid() == pid))?;
        slots[index].take()
    })?;
    let result = match process.state() {
        State::Exited(status) => Some(status),
        State::Ready | State::Running | State::Sleeping(_) => None,
    };
    drop(process);
    notify_readiness();
    result
}

/// The number of processes currently in the table.
#[must_use]
pub fn count() -> usize {
    with_slots(|slots| slots.iter().filter(|slot| slot.is_some()).count())
}

/// All process identifiers currently in the table.
#[must_use]
pub fn pids() -> Vec<Pid> {
    with_slots(|slots| slots.iter().flatten().map(Process::pid).collect())
}
