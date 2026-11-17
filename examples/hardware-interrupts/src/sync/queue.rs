//! A bounded, lock-free multi-producer/multi-consumer queue.
//!
//! This is the crate's stand-in for `crossbeam_queue::ArrayQueue`, written the
//! same way the repository writes the PIC, the PIT and the keyboard decoder: by
//! hand, with the reasoning in comments and unit tests for the corner cases.
//!
//! The queue is a ring of slots, each carrying a *stamp* next to its value. The
//! stamp is the sequence number of the position the slot is next allowed to hold,
//! so a producer and a consumer can tell "free", "full" and "another thread is
//! mid-operation" apart from the stamp alone, without a lock:
//!
//! - `stamp == position` — the slot is free and this position may write it,
//! - `stamp == position + 1` — the slot holds the value pushed at `position` and
//!   that position may read it,
//! - anything else — another thread is between its claim and its stamp update, so
//!   the position is reloaded and the check retried.
//!
//! This is Vyukov's bounded MPMC queue. It needs at least two slots: with one
//! slot the "full" and "free" stamps coincide.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Returned by [`ArrayQueue::pop`] when the queue is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopError;

/// A fixed-capacity FIFO queue that any number of threads may push to and pop
/// from at the same time.
pub struct ArrayQueue<T> {
    /// The position of the next slot to read.
    head: AtomicUsize,
    /// The position of the next slot to write.
    tail: AtomicUsize,
    buffer: Box<[Slot<T>]>,
}

/// One ring slot: a value plus the stamp that says whether it is free or full.
struct Slot<T> {
    stamp: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the queue owns its slots and the stamps hand each value to exactly one
// thread at a time, so it may cross threads exactly when `T` may.
unsafe impl<T: Send> Send for ArrayQueue<T> {}
unsafe impl<T: Send> Sync for ArrayQueue<T> {}

impl<T> ArrayQueue<T> {
    /// Creates a queue that can hold `capacity` values.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is less than two: a one-slot ring cannot tell a full
    /// queue from a free slot.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "an ArrayQueue needs at least two slots");
        let mut buffer = Vec::with_capacity(capacity);
        for index in 0..capacity {
            buffer.push(Slot {
                stamp: AtomicUsize::new(index),
                value: UnsafeCell::new(MaybeUninit::uninit()),
            });
        }
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            buffer: buffer.into_boxed_slice(),
        }
    }

    /// The number of values the queue can hold.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Pushes `value` onto the queue.
    ///
    /// # Errors
    ///
    /// Returns the value back in the `Err` variant if the queue is full.
    pub fn push(&self, value: T) -> Result<(), T> {
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let slot = &self.buffer[tail % self.capacity()];
            let stamp = slot.stamp.load(Ordering::Acquire);
            match (stamp.wrapping_sub(tail) as isize).cmp(&0) {
                core::cmp::Ordering::Equal => {
                    // The slot is free. Claim this position first, so only one
                    // producer can write the slot, then publish the value with a
                    // release store of the stamp.
                    match self.tail.compare_exchange_weak(
                        tail,
                        tail.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            // SAFETY: the claim above gives this thread exclusive
                            // access to the slot until the stamp is bumped.
                            unsafe { (*slot.value.get()).write(value) };
                            slot.stamp.store(tail.wrapping_add(1), Ordering::Release);
                            return Ok(());
                        }
                        Err(current) => tail = current,
                    }
                }
                // The stamp is behind this position: the slot still holds a
                // value from the previous lap, so the queue is full.
                core::cmp::Ordering::Less => return Err(value),
                // A producer that claimed this position has not stamped the slot
                // yet; reload the tail and retry.
                core::cmp::Ordering::Greater => tail = self.tail.load(Ordering::Relaxed),
            }
        }
    }

    /// Pops the oldest value.
    ///
    /// # Errors
    ///
    /// Returns [`PopError`] if the queue is empty.
    pub fn pop(&self) -> Result<T, PopError> {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            let slot = &self.buffer[head % self.capacity()];
            let stamp = slot.stamp.load(Ordering::Acquire);
            match (stamp.wrapping_sub(head.wrapping_add(1)) as isize).cmp(&0) {
                core::cmp::Ordering::Equal => {
                    // The slot holds the value pushed at this position. Claim the
                    // position, take the value, then stamp the slot free for the
                    // next lap.
                    match self.head.compare_exchange_weak(
                        head,
                        head.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            // SAFETY: the claim above gives this thread exclusive
                            // access to the slot; the value is initialized and is
                            // read exactly once.
                            let value = unsafe { (*slot.value.get()).assume_init_read() };
                            slot.stamp
                                .store(head.wrapping_add(self.capacity()), Ordering::Release);
                            return Ok(value);
                        }
                        Err(current) => head = current,
                    }
                }
                // The slot has not been written at this position yet: empty.
                core::cmp::Ordering::Less => return Err(PopError),
                // Another consumer already claimed this position; reload and
                // retry.
                core::cmp::Ordering::Greater => head = self.head.load(Ordering::Relaxed),
            }
        }
    }

    /// The number of values currently queued.
    ///
    /// A value a producer is still writing is counted, so the result may be one
    /// higher than the number of fully published values. It is exact once the
    /// producers have stopped.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tail
            .load(Ordering::Relaxed)
            .wrapping_sub(self.head.load(Ordering::Relaxed))
    }

    /// Returns `true` if the queue holds no values.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for ArrayQueue<T> {
    fn drop(&mut self) {
        // Drain whatever is left so the values are dropped too. There are no
        // concurrent accessors while `&mut self` is held.
        while self.pop().is_ok() {}
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::ArrayQueue;

    #[test_case]
    fn a_fresh_queue_is_empty() {
        let queue: ArrayQueue<u8> = ArrayQueue::new(4);
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.capacity(), 4);
        assert!(queue.pop().is_err());
    }

    #[test_case]
    fn push_and_pop_preserve_order() {
        let queue = ArrayQueue::new(4);
        assert!(queue.push(1).is_ok());
        assert!(queue.push(2).is_ok());
        assert!(queue.push(3).is_ok());
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.pop(), Ok(1));
        assert_eq!(queue.pop(), Ok(2));
        assert_eq!(queue.pop(), Ok(3));
        assert!(queue.is_empty());
    }

    #[test_case]
    fn a_full_queue_hands_the_value_back() {
        let queue = ArrayQueue::new(2);
        assert!(queue.push(10).is_ok());
        assert!(queue.push(20).is_ok());
        assert_eq!(queue.push(30), Err(30), "a full queue must not overwrite");
        assert_eq!(queue.pop(), Ok(10));
        assert!(queue.push(30).is_ok(), "popping must free a slot");
        assert_eq!(queue.pop(), Ok(20));
        assert_eq!(queue.pop(), Ok(30));
    }

    #[test_case]
    fn the_ring_wraps_around_many_times() {
        let queue = ArrayQueue::new(2);
        for i in 0..1000_u32 {
            assert_eq!(queue.push(i), Ok(()));
            assert_eq!(queue.pop(), Ok(i));
        }
        assert!(queue.is_empty());
    }

    #[test_case]
    fn dropping_the_queue_drops_its_values() {
        static DROPS: AtomicUsize = AtomicUsize::new(0);

        struct CountsDrop;
        impl Drop for CountsDrop {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        {
            let queue = ArrayQueue::new(4);
            assert!(queue.push(CountsDrop).is_ok());
            assert!(queue.push(CountsDrop).is_ok());
            assert!(queue.push(CountsDrop).is_ok());
            // `queue` drops here, which must drop the three values.
        }
        assert_eq!(DROPS.load(Ordering::Relaxed), 3);
    }
}
