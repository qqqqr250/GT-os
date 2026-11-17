//! A tiny volatile-memory wrapper.
//!
//! Device memory does not behave like RAM: the compiler is free to cache, reorder
//! or elide ordinary loads and stores, so every access to a memory-mapped device
//! has to go through `read_volatile`/`write_volatile`. Wrapping the raw pointer in
//! [`Volatile`] keeps that rule in one place and stops a raw pointer from leaking
//! into the rest of the code.

use core::ptr;

/// A raw pointer whose accesses are always volatile.
///
/// The pointer itself is not owned or allocated by this type: constructing a
/// [`Volatile`] does not create the memory it points at.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct Volatile<T> {
    pointer: *mut T,
}

// SAFETY: `Volatile<T>` is only a pointer; moving one between threads moves no
// memory and creates no aliasing. The crate keeps its single device pointer
// behind a `Mutex`, which is what actually serializes access.
unsafe impl<T> Send for Volatile<T> {}

impl<T: Copy> Volatile<T> {
    /// Wraps `pointer`. Does not dereference it.
    #[must_use]
    pub const fn new(pointer: *mut T) -> Self {
        Self { pointer }
    }

    /// Volatile read of the pointed-to value.
    ///
    /// # Safety
    ///
    /// `pointer` must be valid for reads and properly aligned.
    #[must_use]
    pub unsafe fn read(&self) -> T {
        // SAFETY: the caller guarantees the pointer is valid for reads.
        unsafe { ptr::read_volatile(self.pointer) }
    }

    /// Volatile write of `value`.
    ///
    /// # Safety
    ///
    /// `pointer` must be valid for writes and properly aligned.
    pub unsafe fn write(&self, value: T) {
        // SAFETY: the caller guarantees the pointer is valid for writes.
        unsafe { ptr::write_volatile(self.pointer, value) };
    }

    /// A new wrapper `count` elements past this one.
    ///
    /// # Safety
    ///
    /// The result must stay inside the same allocation as `self`.
    #[must_use]
    pub unsafe fn offset(self, count: isize) -> Self {
        // SAFETY: the caller guarantees the result stays in bounds.
        Self::new(unsafe { self.pointer.offset(count) })
    }

    /// The wrapped pointer.
    #[must_use]
    pub const fn as_ptr(self) -> *mut T {
        self.pointer
    }
}
