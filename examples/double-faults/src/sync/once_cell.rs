//! A one-shot initialiser for a value shared between threads.
//!
//! This is the crate's stand-in for `conquer_once::spin::OnceCell`. It exists for
//! the scancode queue in [`crate::task::keyboard`]: the queue has to be created
//! once (because the keyboard interrupt handler must be able to reach it) but the
//! kernel only builds it when the async keyboard task is first polled.
//!
//! The value is written exactly once and never moved afterwards, so
//! [`OnceCell::try_get`] can hand out a shared reference from any thread. A small
//! atomic state makes the "exactly once" true even when two threads race to
//! initialise it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, Ordering};

/// The cell has not been initialised and no thread is initialising it.
const UNINITIALIZED: u8 = 0;
/// A thread is running the initialiser.
const INITIALIZING: u8 = 1;
/// The value has been written and will never move again.
const INITIALIZED: u8 = 2;

/// A value that is written once and read many times.
pub struct OnceCell<T> {
    state: AtomicU8,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: a shared reference to the cell only ever reads the value, and the
// value is written before the state flips to `INITIALIZED` (a release store that
// a reader pairs with an acquire load), so `Sync` needs `T: Send + Sync` and
// `Send` needs only `T: Send`.
unsafe impl<T: Send + Sync> Sync for OnceCell<T> {}
unsafe impl<T: Send> Send for OnceCell<T> {}

/// Returned by [`OnceCell::try_init_once`] when the cell already holds a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyInitialized;

/// Returned by [`OnceCell::try_get`] before the cell has been initialised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotInitialized;

impl<T> OnceCell<T> {
    /// Creates an empty cell.
    #[must_use]
    pub const fn uninit() -> Self {
        Self {
            state: AtomicU8::new(UNINITIALIZED),
            value: UnsafeCell::new(None),
        }
    }

    /// Runs `init` and stores its result, unless the cell already holds a value.
    ///
    /// # Errors
    ///
    /// Returns [`AlreadyInitialized`] if the cell was initialised before (or by
    /// a racing thread), leaving the original value in place.
    pub fn try_init_once(&self, init: impl FnOnce() -> T) -> Result<(), AlreadyInitialized> {
        let won = self
            .state
            .compare_exchange(
                UNINITIALIZED,
                INITIALIZING,
                Ordering::Acquire,
                Ordering::Acquire,
            )
            .is_ok();
        if won {
            // SAFETY: this thread won the race, so it is the only writer and no
            // reader can see the value before the release store below.
            unsafe { *self.value.get() = Some(init()) };
            self.state.store(INITIALIZED, Ordering::Release);
            Ok(())
        } else {
            // Wait for a racing initialiser to finish so that a caller that sees
            // `Err` can still `try_get` the value afterwards.
            while self.state.load(Ordering::Acquire) == INITIALIZING {
                core::hint::spin_loop();
            }
            Err(AlreadyInitialized)
        }
    }

    /// Returns a shared reference to the value.
    ///
    /// # Errors
    ///
    /// Returns [`NotInitialized`] if no value has been stored yet.
    pub fn try_get(&self) -> Result<&T, NotInitialized> {
        if self.state.load(Ordering::Acquire) == INITIALIZED {
            // SAFETY: `INITIALIZED` is only stored after the value is written and
            // it is never written again, so the reference is valid for the rest
            // of the cell's life.
            unsafe { (*self.value.get()).as_ref() }.ok_or(NotInitialized)
        } else {
            Err(NotInitialized)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OnceCell;

    #[test_case]
    fn an_uninitialized_cell_returns_nothing() {
        let cell: OnceCell<u32> = OnceCell::uninit();
        assert!(cell.try_get().is_err());
    }

    #[test_case]
    fn the_cell_initializes_once_and_keeps_the_value() {
        let cell = OnceCell::uninit();
        assert!(cell.try_init_once(|| 7).is_ok());
        assert_eq!(cell.try_get(), Ok(&7));
        // A second initialiser must fail and leave the first value alone.
        assert!(cell.try_init_once(|| 9).is_err());
        assert_eq!(cell.try_get(), Ok(&7));
    }
}
