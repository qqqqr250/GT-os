//! A ring-3 guest written against the C ABI shim.
//!
//! It exercises the kernel-seeded `main(argc, argv)` entry convention, raw
//! syscall entry, descriptor wrappers including pipe creation, duplication,
//! flag queries and socketpair shutdown, `errno`, the `brk` heap with recycling,
//! classic string routines, `puts` and `atexit`. The source is still Rust, so
//! the repository stays Rust-only.

#![no_std]
#![no_main]
#![allow(unsafe_code)]
// C's conventional `argc`/`argv` names are deliberately kept in the entry point.
#![allow(clippy::similar_names)]

gtos_cabi::gtos_cabi_main!(main);

const OK: &[u8] = b"cabi ok\r\n";
const FAIL: &[u8] = b"cabi failed\r\n";
const CRT_OK: &[u8] = b"crt ok\r\n";
const DESCRIPTORS_OK: &[u8] = b"cabi descriptors ok\r\n";
const DESCRIPTORS_FAIL: &[u8] = b"cabi descriptors failed\r\n";
const SOCKETPAIR_OK: &[u8] = b"cabi socketpair ok\r\n";
const SOCKETPAIR_FAIL: &[u8] = b"cabi socketpair failed\r\n";
const ARGV_OK: &[u8] = b"cabi argv ok\r\n";
const ARGV_FAIL: &[u8] = b"cabi argv failed\r\n";
const PATH: &[u8] = b"file:/CABI.TXT\0";
const BAD_PATH: &[u8] = b"bogus:/missing\0";
const DATA: &[u8] = b"CABI";
const HELLO: &[u8] = b"hello\0";
const WORLD: &[u8] = b"world\0";
const HELX: &[u8] = b"helX\0";
const WORLD_TEXT: &[u8] = b"helloworld";
const TARGET_FD: i32 = 20;
const INTEGER: &[u8] = b"  -42xyz\0";

/// Compares a C string with an expected byte slice including its final NUL.
///
/// # Safety
///
/// `text` must be null or point at a readable NUL-terminated byte string.
unsafe fn cstr_matches(text: *const u8, expected: &[u8]) -> bool {
    if text.is_null() {
        return false;
    }
    for (index, byte) in expected.iter().enumerate() {
        // SAFETY: the caller guaranteed a readable NUL-terminated string.
        if unsafe { *text.add(index) } != *byte {
            return false;
        }
    }
    true
}

/// Exercises `malloc`/`realloc`/`calloc`/`free`, including block recycling.
fn heap_ok() -> bool {
    let ptr = gtos_cabi::malloc(64);
    if ptr.is_null() {
        return false;
    }
    let bytes = ptr.cast::<u8>();
    // SAFETY: `malloc` returned a writable 64-byte block.
    unsafe { gtos_cabi::memset(bytes, 0xA5, 64) };
    let mut ok = unsafe { *bytes } == 0xA5 && unsafe { *bytes.add(63) } == 0xA5;

    let resized = gtos_cabi::realloc(ptr, 128);
    if resized.is_null() {
        return false;
    }
    let resized_bytes = resized.cast::<u8>();
    ok &= unsafe { *resized_bytes } == 0xA5 && unsafe { *resized_bytes.add(63) } == 0xA5;

    let zeroed = gtos_cabi::calloc(8, 4);
    if zeroed.is_null() {
        return false;
    }
    ok &= unsafe { *zeroed.cast::<u8>() } == 0;
    gtos_cabi::free(zeroed);
    gtos_cabi::free(resized);

    // A freed block should be handed out again rather than growing the heap.
    let reused = gtos_cabi::malloc(32);
    ok &= !reused.is_null();
    gtos_cabi::free(reused);
    ok
}

/// Exercises the clean-room `string.h` and `stdlib.h` routines.
fn string_ok() -> bool {
    // SAFETY: the byte strings below are NUL-terminated static constants.
    let mut ok = unsafe { gtos_cabi::strlen(HELLO.as_ptr()) } == 5;
    ok &= unsafe { gtos_cabi::strcmp(HELLO.as_ptr(), HELLO.as_ptr()) } == 0;
    ok &= unsafe { gtos_cabi::strcmp(HELLO.as_ptr(), WORLD.as_ptr()) } != 0;
    ok &= unsafe { gtos_cabi::strncmp(HELLO.as_ptr(), HELX.as_ptr(), 3) } == 0;

    let mut joined = [0u8; 32];
    // SAFETY: `joined` has room for both static strings and their terminator.
    unsafe {
        gtos_cabi::strcpy(joined.as_mut_ptr(), HELLO.as_ptr());
        gtos_cabi::strcat(joined.as_mut_ptr(), WORLD.as_ptr());
    }
    ok &= unsafe { gtos_cabi::strlen(joined.as_ptr()) } == WORLD_TEXT.len();
    ok &= unsafe { gtos_cabi::memcmp(joined.as_ptr(), WORLD_TEXT.as_ptr(), WORLD_TEXT.len()) } == 0;

    let mut shifting = [0u8; 16];
    // SAFETY: both regions are inside `shifting` and `memmove` handles overlap.
    unsafe {
        gtos_cabi::memcpy(shifting.as_mut_ptr(), b"abcdef".as_ptr(), 6);
        gtos_cabi::memmove(shifting.as_mut_ptr().add(2), shifting.as_ptr(), 6);
    }
    ok &= &shifting[2..8] == b"abcdef";
    ok &= gtos_cabi::abs(-7) == 7;
    // SAFETY: INTEGER is a NUL-terminated static byte string.
    ok &= unsafe { gtos_cabi::atoi(INTEGER.as_ptr()) } == -42;
    ok
}

/// Runs at process exit through `atexit`, proving `crt0` invokes handlers.
extern "C" fn crt_atexit() {
    let _ = unsafe { gtos_cabi::write(gtos_cabi::STDOUT_FILENO, CRT_OK.as_ptr(), CRT_OK.len()) };
}

/// Exercises the C ABI pipe, duplication and flag wrappers.
fn descriptor_wrappers_ok() -> bool {
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` is a writable two-element array owned by this frame.
    if unsafe { gtos_cabi::pipe(fds.as_mut_ptr()) } != 0 {
        return false;
    }
    let mut ok = fds[0] >= 0 && fds[1] >= 0;
    // SAFETY: `fds[1]` is a writable pipe end and the byte array is readable.
    ok = ok && unsafe { gtos_cabi::write(fds[1], b"cd".as_ptr(), 2) } == 2;

    let duplicated = gtos_cabi::dup(fds[0]);
    ok = ok && duplicated >= 0;
    let relocated = gtos_cabi::dup2(duplicated, TARGET_FD);
    ok = ok && relocated == TARGET_FD;

    let mut bytes = [0u8; 2];
    // SAFETY: `relocated` is open and `bytes` is writable for two bytes.
    ok = ok
        && unsafe { gtos_cabi::read(relocated, bytes.as_mut_ptr(), bytes.len()) } == 2
        && bytes == *b"cd";
    ok = ok && gtos_cabi::close(relocated) == 0;
    ok = ok && gtos_cabi::close(duplicated) == 0;
    ok = ok && gtos_cabi::close(fds[1]) == 0;
    ok = ok && gtos_cabi::close(fds[0]) == 0;
    if !ok {
        return false;
    }

    let mut flagged = [-1i32; 2];
    let flags = gtos_cabi::O_NONBLOCK | gtos_cabi::O_CLOEXEC;
    // SAFETY: `flagged` is a writable two-element array owned by this frame.
    if unsafe { gtos_cabi::pipe2(flagged.as_mut_ptr(), flags) } != 0 {
        return false;
    }
    let mut ok = flagged[0] >= 0 && flagged[1] >= 0;
    ok = ok
        && gtos_cabi::fcntl(flagged[0], gtos_cabi::F_GETFL, 0) == gtos_cabi::O_NONBLOCK
        && gtos_cabi::fcntl(flagged[1], gtos_cabi::F_GETFL, 0) == gtos_cabi::O_NONBLOCK;
    ok = ok
        && gtos_cabi::fcntl(flagged[0], gtos_cabi::F_GETFD, 0) == gtos_cabi::FD_CLOEXEC
        && gtos_cabi::fcntl(flagged[1], gtos_cabi::F_GETFD, 0) == gtos_cabi::FD_CLOEXEC;

    let mut empty = 0u8;
    // SAFETY: the read end is open and nonblocking, and `empty` is writable.
    ok = ok
        && unsafe { gtos_cabi::read(flagged[0], &raw mut empty, 1) } == -1
        && unsafe { *gtos_cabi::__errno_location() } != 0;
    ok = ok && gtos_cabi::close(flagged[1]) == 0;
    ok = ok && gtos_cabi::close(flagged[0]) == 0;
    ok
}

/// Exercises the C ABI socketpair and shutdown wrappers.
fn socketpair_wrappers_ok() -> bool {
    let mut fds = [-1i32; 2];
    // SAFETY: `fds` is a writable two-element array owned by this frame.
    if unsafe {
        gtos_cabi::socketpair(
            gtos_cabi::AF_UNIX,
            gtos_cabi::SOCK_SEQPACKET,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return false;
    }
    let mut ok = fds[0] >= 0 && fds[1] >= 0;
    let payload = b"cabi";
    // SAFETY: `fds[1]` is an open datagram endpoint and `payload` is readable.
    ok = ok
        && unsafe { gtos_cabi::write(fds[1], payload.as_ptr(), payload.len()) }
            == isize::try_from(payload.len()).unwrap_or(-1);

    let mut received = [0u8; 4];
    // SAFETY: `fds[0]` is the peer endpoint and `received` is writable.
    ok = ok
        && unsafe { gtos_cabi::read(fds[0], received.as_mut_ptr(), received.len()) }
            == isize::try_from(payload.len()).unwrap_or(-1)
        && received == *payload;

    ok = ok && gtos_cabi::shutdown(fds[1], gtos_cabi::SHUT_WR) == 0;
    let mut eof = [0u8; 1];
    // SAFETY: the writer is half-closed and `eof` is writable.
    ok = ok && unsafe { gtos_cabi::read(fds[0], eof.as_mut_ptr(), eof.len()) } == 0;
    ok = ok && gtos_cabi::close(fds[1]) == 0;
    ok = ok && gtos_cabi::close(fds[0]) == 0;
    ok
}

/// Checks the command line seeded by the kernel before `_start`.
///
/// # Safety
///
/// `argv` must point at the kernel-installed NULL-terminated pointer array.
unsafe fn kernel_argv_ok(argc: i32, argv: *const *const u8) -> bool {
    if argc != 3 || argv.is_null() {
        return false;
    }
    // SAFETY: the caller guaranteed a readable pointer array with `argc` entries
    // plus its terminator.
    unsafe {
        cstr_matches(*argv, b"CABI.ELF\0")
            && cstr_matches(*argv.add(1), b"alpha\0")
            && cstr_matches(*argv.add(2), b"beta\0")
            && (*argv.add(3)).is_null()
    }
}

extern "C" fn main(argc: i32, argv: *const *const u8) -> i32 {
    // SAFETY: `_start` receives the kernel-installed argument array verbatim.
    let argv_ok = unsafe { kernel_argv_ok(argc, argv) };
    let mut ok = argv_ok;

    let pid = gtos_cabi::getpid();
    ok = ok
        && pid > 0
        && gtos_cabi::gtos_syscall(5, 0, 0, 0) == u64::from(u32::try_from(pid).unwrap_or_default());

    // SAFETY: PATH is a NUL-terminated static byte string.
    let fd = unsafe { gtos_cabi::open(PATH.as_ptr(), 0) };
    if fd < 0 {
        ok = false;
    } else {
        let written = unsafe { gtos_cabi::write(fd, DATA.as_ptr(), DATA.len()) };
        ok = ok && written == isize::try_from(DATA.len()).unwrap_or(-1);
        ok = ok && gtos_cabi::close(fd) == 0;
    }

    // SAFETY: PATH is still a valid NUL-terminated static byte string.
    let fd = unsafe { gtos_cabi::open(PATH.as_ptr(), 0) };
    if fd < 0 {
        ok = false;
    } else {
        let mut buffer = [0u8; 4];
        let count = unsafe { gtos_cabi::read(fd, buffer.as_mut_ptr(), buffer.len()) };
        ok = ok && count == isize::try_from(DATA.len()).unwrap_or(-1);
        ok = ok && buffer.as_slice() == DATA;
        ok = ok && gtos_cabi::close(fd) == 0;
    }

    // A failing open must report -1 and a nonzero errno through the C cell.
    // SAFETY: BAD_PATH is a NUL-terminated static byte string.
    let bad = unsafe { gtos_cabi::open(BAD_PATH.as_ptr(), 0) };
    ok = ok && bad == -1 && unsafe { *gtos_cabi::__errno_location() } != 0;

    ok &= heap_ok();
    ok &= string_ok();
    let descriptors_ok = descriptor_wrappers_ok();
    ok &= descriptors_ok;
    let socketpair_ok = socketpair_wrappers_ok();
    ok &= socketpair_ok;
    ok &= gtos_cabi::atexit(crt_atexit) == 0;

    // `puts` writes the string plus a newline to standard output.
    ok &= unsafe { gtos_cabi::puts(HELLO.as_ptr()) } == 0;

    let descriptor_message = if descriptors_ok {
        DESCRIPTORS_OK
    } else {
        DESCRIPTORS_FAIL
    };
    let _ = unsafe {
        gtos_cabi::write(
            gtos_cabi::STDOUT_FILENO,
            descriptor_message.as_ptr(),
            descriptor_message.len(),
        )
    };
    let socketpair_message = if socketpair_ok {
        SOCKETPAIR_OK
    } else {
        SOCKETPAIR_FAIL
    };
    let _ = unsafe {
        gtos_cabi::write(
            gtos_cabi::STDOUT_FILENO,
            socketpair_message.as_ptr(),
            socketpair_message.len(),
        )
    };
    let argv_message = if argv_ok { ARGV_OK } else { ARGV_FAIL };
    let _ = unsafe {
        gtos_cabi::write(
            gtos_cabi::STDOUT_FILENO,
            argv_message.as_ptr(),
            argv_message.len(),
        )
    };
    let message = if ok { OK } else { FAIL };
    let _ = unsafe { gtos_cabi::write(gtos_cabi::STDOUT_FILENO, message.as_ptr(), message.len()) };
    i32::from(!ok)
}
