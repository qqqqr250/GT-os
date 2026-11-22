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

/// Maximum number of processes the kernel can track at once.
pub const MAX_PROCESSES: usize = 16;
/// Size of each process's private ring-0 stack.
const KERNEL_STACK_BYTES: usize = 64 * 1024;

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

    /// Releases a retired image once the new address space is active.
    pub fn drop_retired(&mut self) {
        self.retired = None;
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

/// The fixed set of live processes, indexed by slot rather than PID.
static SLOTS: spin::Mutex<[Option<Process>; MAX_PROCESSES]> =
    spin::Mutex::new([const { None }; MAX_PROCESSES]);

/// Registers a newly created process and returns its slot index.
pub fn register(process: Process) -> Result<usize, &'static str> {
    let mut slots = SLOTS.lock();
    let index = slots
        .iter()
        .position(Option::is_none)
        .ok_or("process table is full")?;
    slots[index] = Some(process);
    Ok(index)
}

/// Looks up a process by PID.
pub fn with_process<R>(pid: Pid, f: impl FnOnce(&Process) -> R) -> Option<R> {
    let slots = SLOTS.lock();
    slots.iter().flatten().find(|p| p.pid() == pid).map(f)
}

/// Mutates a process by PID, for syscalls that grow its address space.
pub fn with_process_mut<R>(pid: Pid, f: impl FnOnce(&mut Process) -> R) -> Option<R> {
    let mut slots = SLOTS.lock();
    slots.iter_mut().flatten().find(|p| p.pid() == pid).map(f)
}

/// Forks `parent_pid` and registers the child in one critical section.
///
/// Building and installing the child while the table is locked avoids copying a
/// large `Process` value back through the syscall frame, and guarantees the
/// child is visible to the scheduler before `fork` returns to ring 3.
pub fn fork_current(parent_pid: Pid, mut context: Context) -> Result<Pid, &'static str> {
    let mut slots = SLOTS.lock();
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
}

/// Marks a process as exited with the given status.
pub fn mark_exited(pid: Pid, status: i64) -> bool {
    let exited = {
        let mut slots = SLOTS.lock();
        if let Some(process) = slots.iter_mut().flatten().find(|p| p.pid() == pid) {
            process.mark_exited(status);
            true
        } else {
            false
        }
    };
    if exited {
        crate::compositor::release_process(pid.as_u64());
    }
    exited
}

/// Marks a process as exited only if it has not already exited.
///
/// This keeps the first process's own status when a later child is the one that
/// finally unwinds the synchronous `run` call.
pub fn mark_exited_if_live(pid: Pid, status: i64) -> bool {
    let exited = {
        let mut slots = SLOTS.lock();
        let Some(process) = slots.iter_mut().flatten().find(|p| p.pid() == pid) else {
            return false;
        };
        if matches!(process.state(), State::Exited(_)) {
            return false;
        }
        process.mark_exited(status);
        true
    };
    if exited {
        crate::compositor::release_process(pid.as_u64());
    }
    exited
}

/// Releases the previous image of a process that has completed an `exec`.
pub fn drop_retired(pid: Pid) {
    with_process_mut(pid, Process::drop_retired);
}

/// Makes a sleeping process runnable so a newly queued signal can be delivered.
pub fn wake_for_signal(pid: Pid) {
    with_process_mut(pid, |process| {
        if matches!(process.state(), State::Sleeping(_)) {
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

/// Whether any process is still waiting on a sleep deadline.
#[must_use]
pub fn has_sleepers() -> bool {
    SLOTS
        .lock()
        .iter()
        .flatten()
        .any(|process| matches!(process.state(), State::Sleeping(_)))
}

/// Whether any process has not yet exited.
#[must_use]
pub fn has_live() -> bool {
    SLOTS
        .lock()
        .iter()
        .flatten()
        .any(|process| !matches!(process.state(), State::Exited(_)))
}

/// Makes every sleeper whose deadline has passed runnable again.
pub fn wake_sleepers(now: u64) {
    let mut slots = SLOTS.lock();
    for slot in slots.iter_mut().flatten() {
        if let State::Sleeping(deadline) = slot.state() {
            if deadline <= now {
                slot.set_ready();
            }
        }
    }
}

/// Removes every process from the table after a synchronous `run` completes.
pub fn clear() {
    {
        let mut slots = SLOTS.lock();
        for slot in slots.iter_mut() {
            *slot = None;
        }
    }
    crate::compositor::release_all();
}

/// Removes a process from the table, returning its exit status.
pub fn reap(pid: Pid) -> Option<i64> {
    let mut slots = SLOTS.lock();
    let index = slots
        .iter()
        .position(|slot| slot.as_ref().is_some_and(|p| p.pid() == pid))?;
    match slots[index].take()?.state() {
        State::Exited(status) => Some(status),
        State::Ready | State::Running | State::Sleeping(_) => None,
    }
}

/// The number of processes currently in the table.
#[must_use]
pub fn count() -> usize {
    SLOTS.lock().iter().filter(|slot| slot.is_some()).count()
}

/// All process identifiers currently in the table.
#[must_use]
pub fn pids() -> Vec<Pid> {
    SLOTS.lock().iter().flatten().map(Process::pid).collect()
}
