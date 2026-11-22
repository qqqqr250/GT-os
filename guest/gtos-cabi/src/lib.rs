//! A Rust-only C runtime and ABI shim for `GTos` ring-3 programs.
//!
//! This crate gives C and C++-style guests a small POSIX surface without
//! importing libc: `int 0x80` entry, `main(argc, argv)` startup, error mapping
//! to `-1`, descriptor and process wrappers, `errno`, a `brk`-backed heap with
//! a recycling free list, classic `string.h`/`stdlib.h` routines, `atexit`
//! handlers and an empty environment block. Everything is written in Rust and
//! linked statically; no foreign source or runtime is vendored.

#![no_std]
// Inline assembly, raw heap headers and C strings are the shim's entire purpose.
#![allow(unsafe_code)]
// The `string.h` routines use `*u8` where C prototypes use `*c_char`/`*c_void`.
// ABIs are identical on x86_64 and the byte-typed bodies stay readable.
#![allow(suspicious_runtime_symbol_definitions)]
// Heap blocks are laid out on 16-byte boundaries by construction, so the raw
// casts below are always aligned even though clippy cannot prove it from the
// opaque `*u8` block pointers.
#![allow(clippy::cast_ptr_alignment, clippy::ptr_as_ptr)]

use core::arch::asm;
use core::ffi::c_void;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Standard input descriptor.
pub const STDIN_FILENO: i32 = 0;
/// Standard output descriptor.
pub const STDOUT_FILENO: i32 = 1;
/// Standard error descriptor.
pub const STDERR_FILENO: i32 = 2;

/// Bytes available to read from a descriptor.
pub const IOCTL_FIONREAD: u64 = 1;
/// Free bytes in a descriptor's outgoing buffer.
pub const IOCTL_FIONWRITE: u64 = 2;
/// Current size of a file descriptor.
pub const IOCTL_FILE_SIZE: u64 = 3;
/// Resize a writable file descriptor.
pub const IOCTL_FILE_TRUNCATE: u64 = 4;
/// Flush a writable file descriptor.
pub const IOCTL_FILE_SYNC: u64 = 5;

/// The value the kernel returns for a failed syscall.
pub const GTOS_ERROR: u64 = u64::MAX;

// Conventional POSIX errno values, kept small and stable for clean-room guests.
/// No such file or directory.
pub const ENOENT: i32 = 2;
/// Input/output error.
pub const EIO: i32 = 5;
/// Bad file descriptor.
pub const EBADF: i32 = 9;
/// Out of memory.
pub const ENOMEM: i32 = 12;
/// Invalid argument.
pub const EINVAL: i32 = 22;

static mut ERRNO: i32 = 0;

#[inline]
fn set_errno(value: i32) {
    // SAFETY: a single-threaded ring-3 process owns this static; no references
    // outlive the write and the crate is compiled without shared threading.
    unsafe {
        ERRNO = value;
    }
}

#[inline]
fn fail_i32(code: i32) -> i32 {
    set_errno(code);
    -1
}

#[inline]
fn fail_isize(code: i32) -> isize {
    set_errno(code);
    -1
}

#[inline]
fn fail_i64(code: i32) -> i64 {
    set_errno(code);
    -1
}

/// Returns the address of the process-wide `errno` cell, as C's
/// `__errno_location` does. Dereferencing requires an unsafe block.
#[unsafe(no_mangle)]
pub extern "C" fn __errno_location() -> *mut i32 {
    &raw mut ERRNO
}

// ---------------------------------------------------------------------------
// Raw syscall entry and small argument helpers
// ---------------------------------------------------------------------------

/// Calls a `GTos` syscall directly with the C ABI.
///
/// Arguments follow the kernel's `int 0x80` convention: `number` in `rax`, then
/// up to three payload words in `rdi`, `rsi` and `rdx`. The raw `u64::MAX` error
/// value is returned unchanged; the named wrappers below convert it to `-1` and
/// set `errno`.
#[unsafe(no_mangle)]
pub extern "C" fn gtos_syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let result: u64;
    // SAFETY: the kernel validates every user pointer before dereferencing it.
    // This block only moves register values and does not touch memory.
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") number => result,
            in("rdi") arg0,
            in("rsi") arg1,
            in("rdx") arg2,
            options(nostack),
        );
    }
    result
}

#[inline]
fn syscall(number: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    gtos_syscall(number, arg0, arg1, arg2)
}

#[inline]
fn is_error(result: u64) -> bool {
    result == GTOS_ERROR
}

#[inline]
fn fd_arg(fd: i32) -> Option<u64> {
    u32::try_from(fd).ok().map(u64::from)
}

/// Returns the length of a NUL-terminated string, or `None` for a null pointer.
///
/// # Safety
///
/// `text` must point at a readable NUL-terminated byte string.
unsafe fn c_string_len(text: *const u8) -> Option<usize> {
    if text.is_null() {
        return None;
    }
    let mut length = 0usize;
    loop {
        // SAFETY: the caller guarantees a readable NUL-terminated string.
        if unsafe { *text.add(length) } == 0 {
            return Some(length);
        }
        length = length.checked_add(1)?;
    }
}

#[inline]
fn path_arg(path: *const u8) -> Option<(u64, u64)> {
    // SAFETY: C callers pass NUL-terminated paths; a null pointer is rejected.
    let length = unsafe { c_string_len(path) }?;
    Some((path as u64, u64::try_from(length).ok()?))
}

// ---------------------------------------------------------------------------
// Descriptor and process wrappers
// ---------------------------------------------------------------------------

/// Writes up to `count` bytes, returning the count or `-1` on error.
///
/// # Safety
///
/// `buffer` must be readable for `count` bytes when `count` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn write(fd: i32, buffer: *const u8, count: usize) -> isize {
    let (Some(fd), false) = (fd_arg(fd), buffer.is_null() && count != 0) else {
        return fail_isize(EBADF);
    };
    let result = syscall(
        1,
        fd,
        buffer as u64,
        u64::try_from(count).unwrap_or(u64::MAX),
    );
    if is_error(result) {
        fail_isize(EIO)
    } else {
        isize::try_from(result).unwrap_or(-1)
    }
}

/// Reads up to `count` bytes, returning the count, zero at EOF, or `-1`.
///
/// An empty nonblocking stream returns `-1` while a writer remains; callers can
/// retry after [`sched_yield`].
///
/// # Safety
///
/// `buffer` must be writable for `count` bytes when `count` is nonzero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn read(fd: i32, buffer: *mut u8, count: usize) -> isize {
    let (Some(fd), false) = (fd_arg(fd), buffer.is_null() && count != 0) else {
        return fail_isize(EBADF);
    };
    let result = syscall(
        3,
        fd,
        buffer as u64,
        u64::try_from(count).unwrap_or(u64::MAX),
    );
    if is_error(result) {
        fail_isize(EIO)
    } else {
        isize::try_from(result).unwrap_or(-1)
    }
}

/// Opens a NUL-terminated `GTos` path or scheme URL, returning `-1` on error.
///
/// `flags` is accepted for C source compatibility but reserved; `GTos` currently
/// opens descriptors read/write according to the backing resource.
///
/// # Safety
///
/// `path` must point at a readable NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(path: *const u8, _flags: i32) -> i32 {
    let Some((address, length)) = path_arg(path) else {
        return fail_i32(EINVAL);
    };
    let result = syscall(2, address, length, 0);
    if is_error(result) {
        fail_i32(ENOENT)
    } else {
        i32::try_from(result).unwrap_or(-1)
    }
}

/// Closes one descriptor, returning zero on success or `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn close(fd: i32) -> i32 {
    let Some(fd) = fd_arg(fd) else {
        return fail_i32(EBADF);
    };
    match syscall(4, fd, 0, 0) {
        0 => 0,
        _ => fail_i32(EBADF),
    }
}

/// Moves a descriptor's cursor, returning its new offset or `-1`.
#[unsafe(no_mangle)]
pub extern "C" fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    let Some(fd) = fd_arg(fd) else {
        return fail_i64(EBADF);
    };
    let bits = u64::from_ne_bytes(offset.to_ne_bytes());
    let result = syscall(15, fd, bits, u64::try_from(whence).unwrap_or(u64::MAX));
    if is_error(result) {
        fail_i64(EINVAL)
    } else {
        i64::try_from(result).unwrap_or(-1)
    }
}

/// Performs a `GTos` descriptor control request, returning `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn ioctl(fd: i32, request: u64, arg: u64) -> i64 {
    let Some(fd) = fd_arg(fd) else {
        return fail_i64(EBADF);
    };
    let result = syscall(44, fd, request, arg);
    if is_error(result) {
        fail_i64(EINVAL)
    } else {
        i64::try_from(result).unwrap_or(-1)
    }
}

/// Returns the caller's process identifier.
#[unsafe(no_mangle)]
pub extern "C" fn getpid() -> i32 {
    let result = syscall(5, 0, 0, 0);
    i32::try_from(result).unwrap_or(-1)
}

/// Splits the current process, returning the child PID to the parent and zero
/// to the child, or `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn fork() -> i32 {
    let result = syscall(8, 0, 0, 0);
    i32::try_from(result).unwrap_or(-1)
}

/// Waits for a child, writing its 64-bit status into `status` when non-null.
///
/// # Safety
///
/// `status` must be null or point at a writable `i32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32 {
    let Some(pid) = fd_arg(pid) else {
        return fail_i32(EINVAL);
    };
    let mut wide_status = 0u64;
    let result = syscall(
        10,
        pid,
        &raw mut wide_status as u64,
        u64::try_from(options).unwrap_or(u64::MAX),
    );
    if is_error(result) {
        return fail_i32(EINVAL);
    }
    // SAFETY: the caller guaranteed that `status` is writable or null.
    unsafe {
        if !status.is_null() {
            *status = i32::try_from(wide_status).unwrap_or(-1);
        }
    }
    i32::try_from(result).unwrap_or(-1)
}

/// Sends a signal to one process, returning zero on success or `-1`.
#[unsafe(no_mangle)]
pub extern "C" fn kill(pid: i32, signal: i32) -> i32 {
    let (Some(pid), Some(signal)) = (fd_arg(pid), u32::try_from(signal).ok()) else {
        return fail_i32(EINVAL);
    };
    match syscall(17, pid, u64::from(signal), 0) {
        0 => 0,
        _ => fail_i32(EINVAL),
    }
}

/// Changes the process working directory, returning zero on success.
///
/// # Safety
///
/// `path` must point at a readable NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn chdir(path: *const u8) -> i32 {
    let Some((address, length)) = path_arg(path) else {
        return fail_i32(EINVAL);
    };
    match syscall(41, address, length, 0) {
        0 => 0,
        _ => fail_i32(ENOENT),
    }
}

/// Writes the working directory into `buffer`, returning it or null on error.
///
/// # Safety
///
/// `buffer` must be writable for `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getcwd(buffer: *mut u8, size: usize) -> *mut u8 {
    if buffer.is_null() || size == 0 {
        set_errno(EINVAL);
        return core::ptr::null_mut();
    }
    let result = syscall(
        42,
        buffer as u64,
        u64::try_from(size).unwrap_or(u64::MAX),
        0,
    );
    if is_error(result) {
        set_errno(EINVAL);
        core::ptr::null_mut()
    } else {
        buffer
    }
}

/// Sleeps for at least `milliseconds`, returning zero or `-1` on error.
#[unsafe(no_mangle)]
pub extern "C" fn msleep(milliseconds: u32) -> i32 {
    match syscall(11, u64::from(milliseconds), 0, 0) {
        0 => 0,
        _ => fail_i32(EINVAL),
    }
}

/// Sleeps for `seconds`, returning the number of seconds actually slept.
#[unsafe(no_mangle)]
pub extern "C" fn sleep(seconds: u32) -> u32 {
    let mut completed = 0;
    while completed < seconds {
        if msleep(1000) != 0 {
            break;
        }
        completed += 1;
    }
    completed
}

/// Gives up the current timeslice, returning zero.
#[unsafe(no_mangle)]
pub extern "C" fn sched_yield() -> i32 {
    let _ = syscall(7, 0, 0, 0);
    0
}

// ---------------------------------------------------------------------------
// brk-backed heap with a recycling free list
// ---------------------------------------------------------------------------

/// Every block header and payload starts on this boundary.
const MAX_ALIGN: usize = 16;
/// A `Header` is a single `usize`, rounded up to `MAX_ALIGN`.
const HEADER_BYTES: usize = 16;

/// Allocated and free blocks both begin with their total size (header included).
#[repr(C)]
struct Header {
    size: usize,
}

static mut HEAP_START: u64 = 0;
static mut HEAP_END: u64 = 0;
/// Free-list nodes are block header pointers; `next` lives in the payload.
static mut FREE_HEAD: *mut u8 = core::ptr::null_mut();

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|sum| sum & !(alignment - 1))
}

#[inline]
fn raw_brk(address: u64) -> u64 {
    syscall(6, address, 0, 0)
}

#[inline]
unsafe fn free_next(header: *mut u8) -> *mut u8 {
    // SAFETY: `header` is a live block whose payload holds the free-list link.
    unsafe { *(header.add(HEADER_BYTES) as *const *mut u8) }
}

#[inline]
unsafe fn set_free_next(header: *mut u8, next: *mut u8) {
    // SAFETY: `header` is a live block whose payload is reserved for the link.
    unsafe { *(header.add(HEADER_BYTES) as *mut *mut u8) = next };
}

fn heap_init() -> bool {
    let current = raw_brk(0);
    if current == GTOS_ERROR || current == 0 {
        return false;
    }
    // SAFETY: single-threaded startup; the initial break is read once.
    unsafe {
        HEAP_START = current;
        HEAP_END = current;
    }
    true
}

/// Removes the first free block of at least `total` bytes from the list and
/// returns its payload pointer. A large block is split so the remainder stays
/// available for later allocations.
fn take_free_block(total: usize) -> Option<*mut u8> {
    // SAFETY: the free list is only touched by this single-threaded process.
    let mut current = unsafe { FREE_HEAD };
    let mut previous: *mut u8 = core::ptr::null_mut();
    while !current.is_null() {
        // SAFETY: every list node is a live block with a valid header.
        let size = unsafe { (*(current as *mut Header)).size };
        if size < total {
            previous = current;
            // SAFETY: `current` is a live free-list node.
            current = unsafe { free_next(current) };
            continue;
        }

        // SAFETY: `current` is a live free-list node.
        let next = unsafe { free_next(current) };
        let remainder = size - total;
        if remainder >= HEADER_BYTES + MAX_ALIGN {
            // SAFETY: the remainder lies inside the same live block.
            let split = unsafe { current.add(total) };
            // SAFETY: `split` is aligned and inside the original block.
            unsafe {
                (*(split as *mut Header)).size = remainder;
                set_free_next(split, next);
                (*(current as *mut Header)).size = total;
            }
            if previous.is_null() {
                // SAFETY: startup owns the free-list head.
                unsafe { FREE_HEAD = split };
            } else {
                // SAFETY: `previous` is a live free-list node.
                unsafe { set_free_next(previous, split) };
            }
        } else if previous.is_null() {
            // SAFETY: startup owns the free-list head.
            unsafe { FREE_HEAD = next };
        } else {
            // SAFETY: `previous` is a live free-list node.
            unsafe { set_free_next(previous, next) };
        }
        // SAFETY: the header is followed by `size - HEADER_BYTES` payload bytes.
        return Some(unsafe { current.add(HEADER_BYTES) });
    }
    None
}

/// Grows the kernel heap and carves one block out of the freshly mapped page.
fn grow_and_alloc(total: usize) -> Option<*mut u8> {
    // SAFETY: single-threaded heap state.
    if unsafe { HEAP_START } == 0 && !heap_init() {
        return None;
    }
    // SAFETY: single-threaded heap state.
    let end = unsafe { HEAP_END };
    let wanted = end.checked_add(u64::try_from(total).ok()?)?;
    let actual = raw_brk(wanted);
    if actual == GTOS_ERROR || actual < wanted {
        set_errno(ENOMEM);
        return None;
    }
    // SAFETY: single-threaded heap state.
    unsafe { HEAP_END = actual };
    let start = end as *mut u8;
    // SAFETY: the kernel mapped `[end, actual)`, so the header is writable.
    unsafe { (*(start as *mut Header)).size = total };
    // SAFETY: the block payload follows its header inside the mapped region.
    Some(unsafe { start.add(HEADER_BYTES) })
}

/// Allocates `size` zero-uninitialised bytes from the `brk` heap.
#[unsafe(no_mangle)]
pub extern "C" fn malloc(size: usize) -> *mut c_void {
    if size == 0 {
        return core::ptr::null_mut();
    }
    let Some(padded) = size.checked_add(HEADER_BYTES) else {
        set_errno(ENOMEM);
        return core::ptr::null_mut();
    };
    let Some(total) = align_up(padded, MAX_ALIGN) else {
        set_errno(ENOMEM);
        return core::ptr::null_mut();
    };
    let payload = take_free_block(total)
        .or_else(|| grow_and_alloc(total))
        .unwrap_or(core::ptr::null_mut());
    if payload.is_null() {
        set_errno(ENOMEM);
    }
    payload as *mut c_void
}

/// Allocates and zeroes `count` items of `size` bytes each.
#[unsafe(no_mangle)]
pub extern "C" fn calloc(count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        set_errno(ENOMEM);
        return core::ptr::null_mut();
    };
    let ptr = malloc(total);
    if !ptr.is_null() {
        // SAFETY: `malloc` returned a writable block of at least `total` bytes.
        unsafe { memset(ptr as *mut u8, 0, total) };
    }
    ptr
}

/// Returns a block to the free list for later reuse.
#[unsafe(no_mangle)]
pub extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    let payload = ptr as *mut u8;
    // SAFETY: `ptr` came from this allocator, so the header precedes it.
    let header = unsafe { payload.sub(HEADER_BYTES) };
    // SAFETY: single-threaded heap state and a live block header.
    let head = unsafe { FREE_HEAD };
    // SAFETY: `payload` is reserved for the free-list link once freed.
    unsafe {
        set_free_next(header, head);
        FREE_HEAD = header;
    }
}

/// Resizes an allocation, preserving the common prefix and freeing the old block.
#[unsafe(no_mangle)]
pub extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if ptr.is_null() {
        return malloc(size);
    }
    if size == 0 {
        free(ptr);
        return core::ptr::null_mut();
    }
    // SAFETY: `ptr` came from this allocator, so the header precedes it.
    let old_header = unsafe { (ptr as *mut u8).sub(HEADER_BYTES) } as *mut Header;
    // SAFETY: the header of a live block is readable.
    let old_total = unsafe { (*old_header).size };
    let old_payload = old_total - HEADER_BYTES;

    let new_ptr = malloc(size);
    if new_ptr.is_null() {
        return core::ptr::null_mut();
    }
    let copy = old_payload.min(size);
    // SAFETY: the old and new blocks are distinct and each covers `copy` bytes.
    unsafe { memcpy(new_ptr as *mut u8, ptr as *const u8, copy) };
    free(ptr);
    new_ptr
}

// ---------------------------------------------------------------------------
// Classic string.h / stdlib.h routines
// ---------------------------------------------------------------------------

/// Returns the length of a NUL-terminated byte string.
///
/// # Safety
///
/// `text` must point at a readable NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strlen(text: *const u8) -> usize {
    let mut length = 0;
    // SAFETY: the caller guarantees a readable NUL-terminated string.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    length
}

/// Compares two NUL-terminated strings lexicographically.
///
/// # Safety
///
/// Both pointers must reference readable NUL-terminated byte strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcmp(left: *const u8, right: *const u8) -> i32 {
    let mut index = 0;
    loop {
        // SAFETY: the caller guarantees readable NUL-terminated strings.
        let a = unsafe { *left.add(index) };
        // SAFETY: the caller guarantees readable NUL-terminated strings.
        let b = unsafe { *right.add(index) };
        if a != b {
            return i32::from(a) - i32::from(b);
        }
        if a == 0 {
            return 0;
        }
        index += 1;
    }
}

/// Compares at most `count` bytes of two NUL-terminated strings.
///
/// # Safety
///
/// Both pointers must reference readable strings that are NUL-terminated or at
/// least `count` bytes long.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncmp(left: *const u8, right: *const u8, count: usize) -> i32 {
    let mut index = 0;
    while index < count {
        // SAFETY: the caller guarantees readable input for `count` bytes.
        let a = unsafe { *left.add(index) };
        // SAFETY: the caller guarantees readable input for `count` bytes.
        let b = unsafe { *right.add(index) };
        if a != b {
            return i32::from(a) - i32::from(b);
        }
        if a == 0 {
            return 0;
        }
        index += 1;
    }
    0
}

/// Copies a NUL-terminated string into `dest`.
///
/// # Safety
///
/// `src` must be NUL-terminated and `dest` must have room for the whole string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcpy(dest: *mut u8, src: *const u8) -> *mut u8 {
    let mut index = 0;
    loop {
        // SAFETY: the caller guarantees a readable NUL-terminated source.
        let byte = unsafe { *src.add(index) };
        // SAFETY: the caller guarantees enough room in `dest`.
        unsafe { *dest.add(index) = byte };
        if byte == 0 {
            break;
        }
        index += 1;
    }
    dest
}

/// Copies at most `count` bytes, padding the rest of `dest` with NUL.
///
/// # Safety
///
/// `src` must be NUL-terminated or readable for `count` bytes, and `dest` must
/// be writable for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    let mut index = 0;
    while index < count {
        // SAFETY: the caller guarantees readable input for `count` bytes.
        let byte = unsafe { *src.add(index) };
        // SAFETY: the caller guarantees `count` writable bytes.
        unsafe { *dest.add(index) = byte };
        if byte == 0 {
            index += 1;
            while index < count {
                // SAFETY: the caller guarantees `count` writable bytes.
                unsafe { *dest.add(index) = 0 };
                index += 1;
            }
            return dest;
        }
        index += 1;
    }
    dest
}

/// Appends a NUL-terminated string to `dest`.
///
/// # Safety
///
/// `dest` must hold a NUL-terminated string with enough spare room, and `src`
/// must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcat(dest: *mut u8, src: *const u8) -> *mut u8 {
    // SAFETY: the caller guarantees `dest` is a NUL-terminated string.
    let mut tail = unsafe { strlen(dest) };
    let mut index = 0;
    loop {
        // SAFETY: the caller guarantees a readable NUL-terminated source.
        let byte = unsafe { *src.add(index) };
        // SAFETY: the caller guarantees enough spare room in `dest`.
        unsafe { *dest.add(tail) = byte };
        if byte == 0 {
            break;
        }
        tail += 1;
        index += 1;
    }
    dest
}

/// Copies `count` bytes between regions that must not overlap.
///
/// # Safety
///
/// `src` and `dest` must be readable/writable for `count` bytes and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcpy(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    let mut index = 0;
    while index < count {
        // SAFETY: the caller guarantees non-overlapping readable/writable input.
        unsafe { *dest.add(index) = *src.add(index) };
        index += 1;
    }
    dest
}

/// Copies `count` bytes between regions that may overlap.
///
/// # Safety
///
/// `src` and `dest` must be readable/writable for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memmove(dest: *mut u8, src: *const u8, count: usize) -> *mut u8 {
    if (dest as usize) < (src as usize) {
        // Destination is below the source, so a forward copy cannot clobber
        // input bytes that have not been read yet.
        // SAFETY: the caller guarantees readable/writable input for `count`.
        unsafe { memcpy(dest, src, count) }
    } else {
        // Otherwise copy backwards so overlapping writes preserve the input.
        let mut index = count;
        while index > 0 {
            index -= 1;
            // SAFETY: the caller guarantees readable/writable input for `count`.
            unsafe { *dest.add(index) = *src.add(index) };
        }
        dest
    }
}

/// Fills `count` bytes with the low byte of `value`.
///
/// # Safety
///
/// `dest` must be writable for `count` bytes.
// C's `memset` converts its `int` argument to `unsigned char`, so keeping only
// the low eight bits is intentional.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memset(dest: *mut u8, value: i32, count: usize) -> *mut u8 {
    let byte = value as u8;
    let mut index = 0;
    while index < count {
        // SAFETY: the caller guarantees `count` writable bytes.
        unsafe { *dest.add(index) = byte };
        index += 1;
    }
    dest
}

/// Compares `count` bytes of two memory regions.
///
/// # Safety
///
/// Both pointers must be readable for `count` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(left: *const u8, right: *const u8, count: usize) -> i32 {
    let mut index = 0;
    while index < count {
        // SAFETY: the caller guarantees readable input for `count` bytes.
        let a = unsafe { *left.add(index) };
        // SAFETY: the caller guarantees readable input for `count` bytes.
        let b = unsafe { *right.add(index) };
        if a != b {
            return i32::from(a) - i32::from(b);
        }
        index += 1;
    }
    0
}

/// Returns the absolute value of `value` without overflowing.
#[unsafe(no_mangle)]
pub extern "C" fn abs(value: i32) -> i32 {
    value.wrapping_abs()
}

/// Converts the leading decimal integer in a NUL-terminated string.
///
/// # Safety
///
/// `text` must point at a readable NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atoi(text: *const u8) -> i32 {
    let mut index = 0;
    // SAFETY: the caller guarantees a readable NUL-terminated string.
    while matches!(unsafe { *text.add(index) }, b' ' | b'\t' | b'\n' | b'\r') {
        index += 1;
    }
    let mut negative = false;
    // SAFETY: the caller guarantees a readable NUL-terminated string.
    match unsafe { *text.add(index) } {
        b'-' => {
            negative = true;
            index += 1;
        }
        b'+' => index += 1,
        _ => {}
    }
    let mut value = 0i32;
    loop {
        // SAFETY: the caller guarantees a readable NUL-terminated string.
        let byte = unsafe { *text.add(index) };
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(i32::from(byte - b'0'));
        index += 1;
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// Writes a NUL-terminated string followed by a newline to standard output.
///
/// # Safety
///
/// `text` must point at a readable NUL-terminated byte string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn puts(text: *const u8) -> i32 {
    // SAFETY: the caller guarantees a readable NUL-terminated string.
    let length = unsafe { strlen(text) };
    let written = unsafe { write(STDOUT_FILENO, text, length) };
    if written != isize::try_from(length).unwrap_or(-1) {
        return -1;
    }
    let newline = unsafe { write(STDOUT_FILENO, b"\n".as_ptr(), 1) };
    if newline == 1 { 0 } else { -1 }
}

// ---------------------------------------------------------------------------
// crt0: environment, atexit handlers and process termination
// ---------------------------------------------------------------------------

/// The C environment block. `GTos` passes no variables to ring 3, so this is a
/// single null pointer.
/// A single null pointer stored as `usize` so the backing static is `Sync`.
static EMPTY_ENV: [usize; 1] = [0];

/// C's `char **environ`, exported for guests that inspect the environment.
#[unsafe(no_mangle)]
pub static mut environ: *mut *mut u8 = EMPTY_ENV.as_ptr() as *mut *mut u8;

const ATEXIT_MAX: usize = 16;
static ATEXIT_COUNT: AtomicUsize = AtomicUsize::new(0);
static mut ATEXIT_HANDLERS: [Option<extern "C" fn()>; ATEXIT_MAX] = [None; ATEXIT_MAX];

/// Registers `handler` to run, last-in-first-out, when `exit` is called.
#[unsafe(no_mangle)]
pub extern "C" fn atexit(handler: extern "C" fn()) -> i32 {
    let slot = ATEXIT_COUNT.fetch_add(1, Ordering::SeqCst);
    if slot >= ATEXIT_MAX {
        ATEXIT_COUNT.fetch_sub(1, Ordering::SeqCst);
        return fail_i32(EINVAL);
    }
    // SAFETY: the slot was reserved atomically and is only written once.
    let cell = unsafe { (&raw mut ATEXIT_HANDLERS[slot]) };
    // SAFETY: `cell` is a reserved slot owned by this registration.
    unsafe { *cell = Some(handler) };
    0
}

fn run_atexit() {
    let count = ATEXIT_COUNT.load(Ordering::SeqCst).min(ATEXIT_MAX);
    for slot in (0..count).rev() {
        // SAFETY: indexes below `count` were initialised by `atexit`.
        let cell = unsafe { &raw mut ATEXIT_HANDLERS[slot] };
        // SAFETY: the cell is a valid initialised slot.
        if let Some(handler) = unsafe { (*cell).take() } {
            handler();
        }
    }
}

fn raw_exit(status: i32) -> ! {
    let _ = syscall(0, u64::try_from(status).unwrap_or(u64::MAX), 0, 0);
    loop {
        spin_loop();
    }
}

/// Runs all registered `atexit` handlers and terminates the process.
#[unsafe(no_mangle)]
pub extern "C" fn exit(status: i32) -> ! {
    run_atexit();
    raw_exit(status)
}

/// Terminates the process abnormally without running `atexit` handlers.
#[unsafe(no_mangle)]
pub extern "C" fn abort() -> ! {
    raw_exit(134)
}

/// Supplies `_start`, an `argc`/`argv` call into `main`, and a panic handler.
///
/// The guest declares `#![no_main]` and calls this macro once. C-style `main`
/// receives a synthetic `argv[0]` of `"gtos"` because the kernel starts ring-3
/// processes without a command line. At exit, the macro's call to [`exit`] runs
/// any handlers registered through [`atexit`] before leaving the process.
#[macro_export]
macro_rules! gtos_cabi_main {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            static PROGRAM: &[u8; 5] = b"gtos\0";
            let argv = [PROGRAM.as_ptr(), core::ptr::null()];
            let entry: extern "C" fn(i32, *const *const u8) -> i32 = $main;
            let status = entry(1, argv.as_ptr());
            $crate::exit(status)
        }

        #[panic_handler]
        fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
            $crate::abort()
        }
    };
}
