//! The waker-based executor: poll only the tasks that are ready.
//!
//! [`SimpleExecutor`](crate::task::simple_executor::SimpleExecutor) re-polls every
//! task in a busy loop, so a task waiting on the keyboard burns the CPU. This
//! executor fixes that with two pieces:
//!
//! - a ready queue of [`TaskId`]s. A task's waker pushes its id back onto the
//!   queue, so `run_ready_tasks` polls exactly the tasks that were woken,
//! - [`sleep_if_idle`](Executor::sleep_if_idle), which disables interrupts,
//!   checks the ready queue and only then executes `hlt`. Without the
//!   interrupt-disable the CPU could go to sleep *after* an interrupt queued a
//!   task but *before* the `hlt`, and the task would wait for the next interrupt.

// `run`/`sleep_if_idle` are kept as useful executor entry points; the kernel
// polls with `run_ready_tasks` from its own loops.
#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::task::Wake;
use core::task::{Context, Waker};

use super::{Task, TaskId};
use crate::sync::ArrayQueue;

/// How many task ids the ready queue holds before it starts panicking.
const QUEUE_CAPACITY: usize = 100;

/// An executor that polls only the tasks whose waker has fired.
pub struct Executor {
    /// The live tasks, keyed by id.
    tasks: BTreeMap<TaskId, Task>,
    /// The ids of the tasks that are ready to be polled.
    task_queue: Arc<ArrayQueue<TaskId>>,
    /// One waker per live task, so its allocation is reused across polls.
    waker_cache: BTreeMap<TaskId, Waker>,
}

impl Executor {
    /// Creates an executor with no tasks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            task_queue: Arc::new(ArrayQueue::new(QUEUE_CAPACITY)),
            waker_cache: BTreeMap::new(),
        }
    }

    /// Adds `task` and schedules it for its first poll.
    ///
    /// # Panics
    ///
    /// Panics if a task with the same id is already live, or if the ready queue
    /// is full (which would need more than [`QUEUE_CAPACITY`] wake-ups before a
    /// poll).
    pub fn spawn(&mut self, task: Task) {
        let task_id = task.id;
        assert!(
            self.tasks.insert(task_id, task).is_none(),
            "a task with the same id is already spawned"
        );
        self.task_queue
            .push(task_id)
            .expect("the ready queue is full");
    }

    /// Runs forever: poll ready tasks, then sleep until an interrupt wakes one.
    pub fn run(&mut self) -> ! {
        loop {
            self.run_ready_tasks();
            self.sleep_if_idle();
        }
    }

    /// Polls each ready task once, returning when none are left.
    ///
    /// Unlike [`run`](Self::run) this returns to the caller when every task is
    /// parked, which is what a kernel loop that also has other work wants.
    pub fn run_ready_tasks(&mut self) {
        // Destructure `self` so the borrow checker sees the three fields as
        // separate borrows.
        let Self {
            tasks,
            task_queue,
            waker_cache,
        } = self;

        while let Ok(task_id) = task_queue.pop() {
            let Some(task) = tasks.get_mut(&task_id) else {
                continue; // the task finished between the wake-up and now
            };
            let waker = waker_cache
                .entry(task_id)
                .or_insert_with(|| TaskWaker::new_waker(task_id, Arc::clone(task_queue)));
            let mut context = Context::from_waker(waker);
            if task.poll(&mut context).is_ready() {
                // The task is done: drop it and its cached waker.
                tasks.remove(&task_id);
                waker_cache.remove(&task_id);
            }
        }
    }

    /// Halts the CPU until the next interrupt, unless a task is already ready.
    fn sleep_if_idle(&self) {
        use x86_64::instructions::interrupts;

        interrupts::disable();
        if self.task_queue.is_empty() {
            // Nothing is ready, so the next interrupt is the only way forward.
            // `enable_and_hlt` enables interrupts and halts as one step, closing
            // the race in which an interrupt arrives between the check and the
            // halt.
            interrupts::enable_and_hlt();
        } else {
            // A task became ready while interrupts were off; let `run` poll it
            // rather than sleeping through it.
            interrupts::enable();
        }
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

/// The waker an [`Executor`] hands to each task: it re-queues the task's id.
struct TaskWaker {
    task_id: TaskId,
    task_queue: Arc<ArrayQueue<TaskId>>,
}

impl TaskWaker {
    /// Builds a waker for `task_id` that wakes into `task_queue`.
    fn new_waker(task_id: TaskId, task_queue: Arc<ArrayQueue<TaskId>>) -> Waker {
        Waker::from(Arc::new(Self {
            task_id,
            task_queue,
        }))
    }

    /// Pushes this task's id back onto the ready queue.
    ///
    /// # Panics
    ///
    /// Panics if the ready queue is full.
    fn wake_task(&self) {
        self.task_queue
            .push(self.task_id)
            .expect("the ready queue is full");
    }
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_task();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wake_task();
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{Context, Poll};

    use super::Executor;
    use crate::task::Task;

    #[test_case]
    fn the_executor_runs_a_ready_task() {
        static RAN: AtomicBool = AtomicBool::new(false);

        let mut executor = Executor::new();
        executor.spawn(Task::new(async {
            RAN.store(true, Ordering::Relaxed);
        }));
        executor.run_ready_tasks();
        assert!(RAN.load(Ordering::Relaxed));
    }

    #[test_case]
    fn the_executor_parks_a_waiting_task_until_its_waker_fires() {
        static RAN: AtomicBool = AtomicBool::new(false);

        /// A future that is pending once, wakes itself, then finishes.
        struct WakeOnce(bool);
        impl core::future::Future for WakeOnce {
            type Output = ();
            fn poll(mut self: core::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.0 {
                    Poll::Ready(())
                } else {
                    self.0 = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        let mut executor = Executor::new();
        executor.spawn(Task::new(async {
            WakeOnce(false).await;
            RAN.store(true, Ordering::Relaxed);
        }));

        // The task wakes itself, so a single `run_ready_tasks` polls it twice.
        executor.run_ready_tasks();
        assert!(RAN.load(Ordering::Relaxed));
    }
}
