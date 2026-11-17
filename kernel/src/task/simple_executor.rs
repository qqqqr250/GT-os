//! The post's first executor: poll every task in turn, forever.
//!
//! This is the naive step the tutorial starts from. It has no waker support — it
//! hands each task a *dummy* waker that does nothing — so a task that returns
//! `Pending` is simply put back at the back of the queue and polled again. That
//! burns the CPU on a task that is waiting, which is exactly the problem
//! [`crate::task::executor::Executor`] solves.

// The naive executor is kept for parity with `examples/async-await` and is only
// exercised by this module's unit tests; the kernel uses the waker-based one.
#![allow(dead_code)]

use alloc::collections::VecDeque;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use super::Task;

/// Polls a queue of tasks in a loop until every task has finished.
pub struct SimpleExecutor {
    task_queue: VecDeque<Task>,
}

impl SimpleExecutor {
    /// Creates an executor with no tasks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            task_queue: VecDeque::new(),
        }
    }

    /// Adds `task` to the back of the queue.
    pub fn spawn(&mut self, task: Task) {
        self.task_queue.push_back(task);
    }

    /// Runs every task to completion.
    ///
    /// A task that returns `Pending` goes back to the end of the queue and is
    /// polled again, so a task that is waiting for an event that never comes
    /// spins here forever.
    pub fn run(&mut self) {
        while let Some(mut task) = self.task_queue.pop_front() {
            let waker = dummy_waker();
            let mut context = Context::from_waker(&waker);
            match task.poll(&mut context) {
                Poll::Ready(()) => {} // task done
                Poll::Pending => self.task_queue.push_back(task),
            }
        }
    }
}

impl Default for SimpleExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// A waker whose vtable functions all do nothing.
///
/// The naive executor never uses the waker, so a waker that drops the wake-up is
/// enough; the task is re-polled from the queue instead.
fn dummy_raw_waker() -> RawWaker {
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        dummy_raw_waker()
    }

    let vtable = &RawWakerVTable::new(clone, no_op, no_op, no_op);
    RawWaker::new(core::ptr::null(), vtable)
}

/// Builds the no-op [`Waker`].
fn dummy_waker() -> Waker {
    // SAFETY: every function in the vtable ignores the null data pointer, so the
    // waker never dereferences it.
    unsafe { Waker::from_raw(dummy_raw_waker()) }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicU32, Ordering};

    use super::SimpleExecutor;
    use crate::task::Task;

    #[test_case]
    fn the_simple_executor_runs_every_task() {
        static RUNS: AtomicU32 = AtomicU32::new(0);

        let mut executor = SimpleExecutor::new();
        executor.spawn(Task::new(async {
            RUNS.fetch_add(1, Ordering::Relaxed);
        }));
        executor.spawn(Task::new(async {
            RUNS.fetch_add(1, Ordering::Relaxed);
        }));
        executor.run();
        assert_eq!(RUNS.load(Ordering::Relaxed), 2);
    }

    #[test_case]
    fn the_simple_executor_resumes_a_yielding_task() {
        // A future that yields once before finishing: the naive executor must put
        // it back on the queue and poll it again.
        static STEPS: AtomicU32 = AtomicU32::new(0);

        struct YieldOnce(bool);
        impl core::future::Future for YieldOnce {
            type Output = ();
            fn poll(
                mut self: core::pin::Pin<&mut Self>,
                cx: &mut core::task::Context<'_>,
            ) -> core::task::Poll<()> {
                if self.0 {
                    core::task::Poll::Ready(())
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    core::task::Poll::Pending
                }
            }
        }

        let mut executor = SimpleExecutor::new();
        executor.spawn(Task::new(async {
            YieldOnce(false).await;
            STEPS.fetch_add(1, Ordering::Relaxed);
        }));
        executor.run();
        assert_eq!(STEPS.load(Ordering::Relaxed), 1);
    }
}
