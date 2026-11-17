//! A tiny async/await runtime: tasks, executors and the async keyboard stream.
//!
//! `blog_os` post-12 introduces `async`/`await` in the kernel. A [`Task`] wraps a
//! future in a `Pin<Box<..>>` and gives it a unique [`TaskId`]; an executor polls
//! tasks and uses a [`Waker`](core::task::Waker) to learn when a parked task is
//! ready again. Two executors are provided:
//!
//! - [`simple_executor::SimpleExecutor`] polls every task in a loop and is the
//!   post's first, deliberately naive step,
//! - [`executor::Executor`] polls only the tasks whose waker fired and parks the
//!   CPU with `hlt` when none are ready.
//!
//! [`keyboard`] is the async driver the post builds on top: the keyboard
//! interrupt handler pushes raw scancodes into a lock-free queue and wakes the
//! task that decodes them. Here the decoded bytes go into the kernel's existing
//! [`crate::input`] queue, so the shell and the desktop keep reading input exactly
//! as before — they only have to poll the runtime first ([`poll`]).

use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};

use spin::Mutex;

use executor::Executor;

pub mod executor;
pub mod keyboard;
pub mod simple_executor;

/// A spawned future and the identity the executor tracks it by.
///
/// The boxed future is `Send` because the kernel's executor lives in a `static`
/// (see [`EXECUTOR`]); the futures the kernel spawns are all `Send`.
pub struct Task {
    id: TaskId,
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Task {
    /// Wraps `future` in a task with a fresh id.
    pub fn new(future: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            id: TaskId::new(),
            future: Box::pin(future),
        }
    }

    /// Polls the future once with `context`.
    fn poll(&mut self, context: &mut Context<'_>) -> Poll<()> {
        self.future.as_mut().poll(context)
    }
}

/// The unique id of a [`Task`], used to key it in an executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TaskId(u64);

impl TaskId {
    /// Hands out the next id. The counter is process-global, so ids never repeat.
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// The kernel's task executor, built once by [`init`].
///
/// `Executor::new` allocates, so it cannot live in a `const` `static`; the
/// `Option` is filled in once the heap exists.
static EXECUTOR: Mutex<Option<Executor>> = Mutex::new(None);

/// Starts the kernel's async runtime: the scancode queue and the keyboard task.
///
/// Must run after the heap is built (spawning a task allocates) and before
/// [`crate::interrupts::init`] unmasks the keyboard IRQ, so the interrupt handler
/// always finds an initialised scancode queue.
pub fn init() {
    keyboard::init();
    let mut executor = Executor::new();
    executor.spawn(Task::new(keyboard::print_keypresses()));
    *EXECUTOR.lock() = Some(executor);
}

/// Polls every task whose waker has fired, returning when all are parked.
///
/// The kernel's blocking loops (the shell's input wait and the desktop) call this
/// before they look for input or halt, so the keyboard task gets to decode the
/// scancodes the interrupt handler queued.
pub fn poll() {
    if let Some(executor) = EXECUTOR.lock().as_mut() {
        executor.run_ready_tasks();
    }
}

#[cfg(test)]
mod tests {
    use super::TaskId;

    #[test_case]
    fn task_ids_are_unique() {
        let first = TaskId::new();
        let second = TaskId::new();
        assert_ne!(first, second);
        assert!(first < second, "ids are handed out in order");
    }
}
