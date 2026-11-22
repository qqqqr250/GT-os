//! A ring-3 guest written against the C ABI shim.
//!
//! It exercises the `main(argc, argv)` entry convention, raw syscall entry,
//! descriptor wrappers, `errno`, the `brk` heap with recycling, classic string
//! routines, `puts` and `atexit`. The source is still Rust, so the repository
//! stays Rust-only.

#![no_std]
#![no_main]
#![allow(unsafe_code)]
// C's conventional `argc`/`argv` names are deliberately kept in the entry point.
#![allow(clippy::similar_names)]

gtos_cabi::gtos_cabi_main!(main);

const OK: &[u8] = b"cabi ok\r\n";
const FAIL: &[u8] = b"cabi failed\r\n";
const CRT_OK: &[u8] = b"crt ok\r\n";
const PATH: &[u8] = b"file:/CABI.TXT\0";
const BAD_PATH: &[u8] = b"bogus:/missing\0";
const DATA: &[u8] = b"CABI";
const HELLO: &[u8] = b"hello\0";
const WORLD: &[u8] = b"world\0";
const HELX: &[u8] = b"helX\0";
const WORLD_TEXT: &[u8] = b"helloworld";
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

extern "C" fn main(argc: i32, argv: *const *const u8) -> i32 {
    let mut ok = argc == 1 && !argv.is_null();
    if ok {
        // SAFETY: the shim installed a one-element argv with a valid string.
        ok = unsafe { cstr_matches(*argv, b"gtos\0") };
    }

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
    ok &= gtos_cabi::atexit(crt_atexit) == 0;

    // `puts` writes the string plus a newline to standard output.
    ok &= unsafe { gtos_cabi::puts(HELLO.as_ptr()) } == 0;

    let message = if ok { OK } else { FAIL };
    let _ = unsafe { gtos_cabi::write(gtos_cabi::STDOUT_FILENO, message.as_ptr(), message.len()) };
    i32::from(!ok)
}
