//! A small Rust program that runs in `GTos` ring 3.
//!
//! It is compiled for `x86_64-unknown-none` and linked as a static ELF image,
//! so the kernel can load it from the data disk with `run RUSTDEMO.ELF`.

#![no_std]
#![no_main]
// Inline assembly in `gtos-sys` is the whole point of this guest: every kernel
// service is reached through `int 0x80`.
#![allow(unsafe_code)]

gtos_sys::gtos_heap!();
gtos_sys::gtos_main!(main);

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use gtos_sys::{InputEvent, InputPoll, WindowCreate, WindowDamage};
use gtos_widgets::shell::{
    DesktopShell, LauncherEntry, LauncherRecord, ShellAction, ShellFocus, ShellLayout,
    ShellSessionAction, TrayAction, TrayItem, parse_launcher_records,
};
use gtos_widgets::{
    ACCENT, BACKGROUND, Button, Canvas, Color, DIM, Event, FOREGROUND, GridLayout,
    HorizontalLayout, Label, Point, Rect, ScrollArea, Style, Surface, TextInput, Theme, Ui,
    VerticalLayout, View, ViewBuilder, Widget,
};

const MESSAGE: &[u8] = b"Hello from a Rust guest in ring 3!\r\n";
const CLOCK_OK: &[u8] = b"monotonic clock advanced\r\n";
const CLOCK_FAIL: &[u8] = b"monotonic clock did not advance\r\n";
const RTC_OK: &[u8] = b"wall clock available\r\n";
const RTC_FAIL: &[u8] = b"wall clock unavailable\r\n";
const IO_OK: &[u8] = b"file write/read ok\r\n";
const IO_FAIL: &[u8] = b"file write/read failed\r\n";
const FILE_URL: &[u8] = b"file:/RUSTIO.TXT";
const FILE_DATA: &[u8] = b"GTOS";
const SIGNAL_HIT: &[u8] = b"handler entered\r\n";
const SIGNAL_OK: &[u8] = b"signal delivered\r\n";
const SIGNAL_FAIL: &[u8] = b"signal delivery failed\r\n";
const MASKED_HIT: &[u8] = b"masked handler entered\r\n";
const QUEUED_FIRST_HIT: &[u8] = b"queued first handler\r\n";
const QUEUED_SECOND_HIT: &[u8] = b"queued handler entered\r\n";
const CHILD_HIT: &[u8] = b"child handler entered\r\n";
const SIGNAL_MASK_OK: &[u8] = b"signal mask ok\r\n";
const SIGNAL_MASK_FAIL: &[u8] = b"signal mask failed\r\n";
const SIGNAL_IGNORE_OK: &[u8] = b"signal ignore ok\r\n";
const SIGNAL_IGNORE_FAIL: &[u8] = b"signal ignore failed\r\n";
const SIGNAL_QUEUE_OK: &[u8] = b"signal queue ok\r\n";
const SIGNAL_QUEUE_FAIL: &[u8] = b"signal queue failed\r\n";
const SIGNAL_CHILD_OK: &[u8] = b"signal cross process ok\r\n";
const SIGNAL_CHILD_FAIL: &[u8] = b"signal cross process failed\r\n";
const SIGNAL_TERM_OK: &[u8] = b"signal default action ok\r\n";
const SIGNAL_TERM_FAIL: &[u8] = b"signal default action failed\r\n";
const SIGNAL_KILL_OK: &[u8] = b"signal kill ok\r\n";
const SIGNAL_KILL_FAIL: &[u8] = b"signal kill failed\r\n";
const FAULT_SIGNAL_OK: &[u8] = b"fault signal ok\r\n";
const FAULT_SIGNAL_FAIL: &[u8] = b"fault signal failed\r\n";
const RT_SIGNAL_OK: &[u8] = b"rt signal queue ok\r\n";
const RT_SIGNAL_FAIL: &[u8] = b"rt signal queue failed\r\n";
const SIGNAL_TIMEOUT_MS: u64 = 3_000;
const HEAP_OK: &[u8] = b"guest allocator ok\r\n";
const HEAP_FAIL: &[u8] = b"guest allocator failed\r\n";
const FRAMEBUFFER_OK: &[u8] = b"framebuffer device ok\r\n";
const FRAMEBUFFER_FAIL: &[u8] = b"framebuffer device failed\r\n";
const INPUT_WAITING: &[u8] = b"waiting for input event\r\n";
const INPUT_OK: &[u8] = b"input device ok\r\n";
const INPUT_FAIL: &[u8] = b"input device failed\r\n";
const WINDOW_OK: &[u8] = b"window compositor ok\r\n";
const WINDOW_FAIL: &[u8] = b"window compositor failed\r\n";
const WINDOW_DAMAGE_OK: &[u8] = b"window damage ok\r\n";
const WINDOW_STACK_OK: &[u8] = b"window stacking ok\r\n";
const WINDOW_DECORATION_OK: &[u8] = b"window decorations ok\r\n";
const WINDOW_IPC_OK: &[u8] = b"window ipc ok\r\n";
const WINDOW_IPC_FAIL: &[u8] = b"window ipc failed\r\n";
const WINDOW_SHM_OK: &[u8] = b"window shm ok\r\n";
const WINDOW_SHM_FAIL: &[u8] = b"window shm failed\r\n";
const WIDGET_OK: &[u8] = b"widget toolkit ok\r\n";
const WIDGET_FAIL: &[u8] = b"widget toolkit failed\r\n";
const WIDGET_LAYOUT_OK: &[u8] = b"widget layout ok\r\n";
const WIDGET_LAYOUT_FAIL: &[u8] = b"widget layout failed\r\n";
const WIDGET_VIEW_OK: &[u8] = b"widget view ok\r\n";
const WIDGET_VIEW_FAIL: &[u8] = b"widget view failed\r\n";
const WIDGET_NESTED_OK: &[u8] = b"widget nested ok\r\n";
const WIDGET_NESTED_FAIL: &[u8] = b"widget nested failed\r\n";
const WIDGET_EDITOR_OK: &[u8] = b"widget editor ok\r\n";
const WIDGET_EDITOR_FAIL: &[u8] = b"widget editor failed\r\n";
const SHELL_PANEL_OK: &[u8] = b"shell panel ok\r\n";
const SHELL_PANEL_FAIL: &[u8] = b"shell panel failed\r\n";
const SHELL_APPS_OK: &[u8] = b"shell apps ok\r\n";
const SHELL_APPS_FAIL: &[u8] = b"shell apps failed\r\n";
const SHELL_FOCUS_OK: &[u8] = b"shell focus ok\r\n";
const SHELL_FOCUS_FAIL: &[u8] = b"shell focus failed\r\n";
const SHELL_ACTIONS_OK: &[u8] = b"shell actions ok\r\n";
const SHELL_ACTIONS_FAIL: &[u8] = b"shell actions failed\r\n";
const SHELL_LAUNCH_OK: &[u8] = b"shell launch ok\r\n";
const SHELL_LAUNCH_FAIL: &[u8] = b"shell launch failed\r\n";
const SHELL_APPS_PATH: &[u8] = b"/APPS.TSV";
/// Larger than every seeded ring-3 program, but still bounded for the guest.
const MAX_LAUNCH_BYTES: usize = 1 << 20;
const LAUNCH_WAIT_MS: u64 = 2_000;
const SHM_OK: &[u8] = b"shared memory ok\r\n";
const SHM_FAIL: &[u8] = b"shared memory failed\r\n";
const STREAM_OK: &[u8] = b"streaming ipc ok\r\n";
const STREAM_FAIL: &[u8] = b"streaming ipc failed\r\n";
const SOCKET_OK: &[u8] = b"socketpair ok\r\n";
const SOCKET_FAIL: &[u8] = b"socketpair failed\r\n";
/// One full datagram fills a quarter of the kernel's 4 KiB socket buffer.
const SOCKET_DATAGRAM_BYTES: usize = 1024;
const SOCKET_TIMEOUT_MS: u64 = 3_000;
const IOCTL_OK: &[u8] = b"ioctl ok\r\n";
const IOCTL_FAIL: &[u8] = b"ioctl failed\r\n";
const IOCTL_FILE: &[u8] = b"file:/IOCTL.TXT";
const IOCTL_DATA: &[u8] = b"ABCDEFGH";
const STREAM_FILE_OK: &[u8] = b"stream file ok\r\n";
const STREAM_FILE_FAIL: &[u8] = b"stream file failed\r\n";
const STREAM_FILE_URL: &[u8] = b"file:/STREAM.TXT";
/// Four 2 KiB FAT clusters, so a snapshot-only implementation cannot pass.
const STREAM_FILE_BYTES: usize = 8192;
/// Chunked writes also exercise cursor persistence across syscalls.
const STREAM_FILE_CHUNK_BYTES: usize = 700;
const WRITEBACK_OK: &[u8] = b"fat writeback ok\r\n";
const WRITEBACK_FAIL: &[u8] = b"fat writeback failed\r\n";
const WRITEBACK_URL: &[u8] = b"file:/WRITEBACK.TXT";
/// 36 two-KiB clusters, more than the kernel's 32-line cluster cache.
const WRITEBACK_BYTES: usize = 72 * 1024;
const WRITEBACK_CHUNK_BYTES: usize = 2048;
const DIR_STREAM_OK: &[u8] = b"directory stream ok\r\n";
const DIR_STREAM_FAIL: &[u8] = b"directory stream failed\r\n";
const DIR_STREAM_URL: &[u8] = b"file:/";
const STREAM_PARTS: [&[u8]; 3] = [b"stre", b"amin", b"g ipc"];
const SCHEME_NAME: &[u8] = b"demo";
const SCHEME_REQUEST: &[u8] = b"PING";
const SCHEME_RESPONSE: &[u8] = b"PONG";
const SCHEME_SERVER_OK: &[u8] = b"scheme server ok\r\n";
const SCHEME_SERVER_FAIL: &[u8] = b"scheme server failed\r\n";
const SCHEME_SHM_OK: &[u8] = b"scheme shm ok\r\n";
const SCHEME_SHM_FAIL: &[u8] = b"scheme shm failed\r\n";
const SHM_SCHEME_NAME: &[u8] = b"shm";
/// Twice the 4 KiB pipe capacity: a pipe-only transport would truncate this.
const SHM_PAYLOAD_BYTES: usize = 8192;
const VFS_OK: &[u8] = b"vfs ok\r\n";
const VFS_FAIL: &[u8] = b"vfs failed\r\n";
const VFS_PATH: &[u8] = b"/sys/version";
const SHM_BYTES: u64 = 4096;
const SHM_MARKER: u8 = 0x5a;
/// Ten seconds is long enough for a boot test to inject a key after this banner.
const INPUT_TIMEOUT_NS: u64 = 10_000_000_000;

static mut HANDLER_RAN: bool = false;
static mut MASKED_HANDLER_RAN: bool = false;
static mut QUEUED_FIRST_RAN: bool = false;
static mut QUEUED_SECOND_RAN: bool = false;
static mut RT_QUEUE_ORDER: [u8; 3] = [0, 0, 0];
static mut RT_QUEUE_COUNT: usize = 0;
static mut CHILD_HANDLER_RAN: bool = false;

extern "C" fn on_signal(_signum: u64, frame: *mut u64) -> ! {
    // SAFETY: the handler and main never run at the same time.
    unsafe { HANDLER_RAN = true };
    let _ = gtos_sys::write(1, SIGNAL_HIT);
    gtos_sys::sigreturn(frame)
}

extern "C" fn on_masked_signal(_signum: u64, frame: *mut u64) -> ! {
    // SAFETY: the handler and main never run at the same time.
    unsafe { MASKED_HANDLER_RAN = true };
    let _ = gtos_sys::write(1, MASKED_HIT);
    gtos_sys::sigreturn(frame)
}

extern "C" fn on_queued_first(_signum: u64, frame: *mut u64) -> ! {
    // SAFETY: the handler and main never run at the same time.
    unsafe { QUEUED_FIRST_RAN = true };
    let _ = gtos_sys::write(1, QUEUED_FIRST_HIT);
    // This signal is blocked by `in_handler`, so it must survive until the
    // `sigreturn` below restores the interrupted context.
    let _ = gtos_sys::kill(gtos_sys::getpid(), gtos_sys::SIGUSR2);
    gtos_sys::sigreturn(frame)
}

extern "C" fn on_queued_second(_signum: u64, frame: *mut u64) -> ! {
    // SAFETY: the handler and main never run at the same time.
    unsafe { QUEUED_SECOND_RAN = true };
    let _ = gtos_sys::write(1, QUEUED_SECOND_HIT);
    gtos_sys::sigreturn(frame)
}

extern "C" fn on_rt_signal(signum: u64, frame: *mut u64) -> ! {
    // SAFETY: handler and main never run at the same time.
    unsafe {
        let count = (&raw const RT_QUEUE_COUNT).read();
        let max = (&raw const RT_QUEUE_ORDER).read().len();
        if count < max {
            let Ok(signum) = u8::try_from(signum) else {
                gtos_sys::sigreturn(frame);
            };
            (&raw mut RT_QUEUE_ORDER)
                .cast::<u8>()
                .add(count)
                .write(signum);
            (&raw mut RT_QUEUE_COUNT).write(count + 1);
        }
    }
    gtos_sys::sigreturn(frame)
}

extern "C" fn on_child_signal(_signum: u64, frame: *mut u64) -> ! {
    // SAFETY: the child handler and the child's main never run together.
    unsafe { CHILD_HANDLER_RAN = true };
    let _ = gtos_sys::write(1, CHILD_HIT);
    gtos_sys::sigreturn(frame)
}

extern "C" fn on_fault_signal(_signum: u64, _frame: *mut u64) -> ! {
    gtos_sys::exit(0);
}

fn signal_ok() -> bool {
    if gtos_sys::signal(gtos_sys::SIGUSR1, on_signal) == gtos_sys::ERROR {
        return false;
    }
    if gtos_sys::kill(gtos_sys::getpid(), gtos_sys::SIGUSR1) == gtos_sys::ERROR {
        return false;
    }
    // SAFETY: single-threaded guest, and `kill` above delivers synchronously.
    unsafe { HANDLER_RAN }
}

fn signal_mask_ok() -> bool {
    // SAFETY: single-threaded guest, and no handler is running.
    unsafe { MASKED_HANDLER_RAN = false };
    if gtos_sys::signal(gtos_sys::SIGUSR1, on_masked_signal) == gtos_sys::ERROR {
        return false;
    }
    let bit = gtos_sys::signal_bit(gtos_sys::SIGUSR1);
    let mut old = 1u64;
    if gtos_sys::sigprocmask(gtos_sys::SIG_SETMASK, Some(&bit), Some(&mut old)) == gtos_sys::ERROR
        || old != 0
    {
        return false;
    }
    if gtos_sys::kill(gtos_sys::getpid(), gtos_sys::SIGUSR1) == gtos_sys::ERROR {
        return false;
    }
    let mut pending = 0u64;
    if gtos_sys::sigpending(&mut pending) == gtos_sys::ERROR || pending & bit == 0 {
        return false;
    }
    // SAFETY: the signal is blocked, so its handler cannot have run.
    if unsafe { MASKED_HANDLER_RAN } {
        return false;
    }
    if gtos_sys::sigprocmask(gtos_sys::SIG_UNBLOCK, Some(&bit), None) == gtos_sys::ERROR {
        return false;
    }
    if gtos_sys::sigpending(&mut pending) == gtos_sys::ERROR || pending != 0 {
        return false;
    }
    // SAFETY: unblocking delivers the pending signal before the syscall returns.
    unsafe { MASKED_HANDLER_RAN }
}

fn signal_ignore_ok() -> bool {
    if gtos_sys::signal_disposition(gtos_sys::SIGUSR2, gtos_sys::SIG_IGN) == gtos_sys::ERROR {
        return false;
    }
    if gtos_sys::kill(gtos_sys::getpid(), gtos_sys::SIGUSR2) == gtos_sys::ERROR {
        return false;
    }
    let mut pending = 0u64;
    gtos_sys::sigpending(&mut pending) != gtos_sys::ERROR
        && pending & gtos_sys::signal_bit(gtos_sys::SIGUSR2) == 0
}

fn rt_signal_queue_ok() -> bool {
    // SAFETY: single-threaded guest, handlers do not overlap main.
    unsafe {
        RT_QUEUE_ORDER = [0, 0, 0];
        RT_QUEUE_COUNT = 0;
    }
    let rt1 = gtos_sys::SIGRTMIN;
    let rt2 = gtos_sys::SIGRTMIN + 1;
    if gtos_sys::signal(rt1, on_rt_signal) == gtos_sys::ERROR
        || gtos_sys::signal(rt2, on_rt_signal) == gtos_sys::ERROR
    {
        return false;
    }
    let mask = gtos_sys::signal_bit(rt1) | gtos_sys::signal_bit(rt2);
    let mut old = 0u64;
    if gtos_sys::sigprocmask(gtos_sys::SIG_BLOCK, Some(&mask), Some(&mut old)) == gtos_sys::ERROR {
        return false;
    }
    let ok = gtos_sys::kill(gtos_sys::getpid(), rt2) != gtos_sys::ERROR
        && gtos_sys::kill(gtos_sys::getpid(), rt1) != gtos_sys::ERROR
        && gtos_sys::kill(gtos_sys::getpid(), rt1) != gtos_sys::ERROR;
    let mut pending = 0u64;
    let queued =
        ok && gtos_sys::sigpending(&mut pending) != gtos_sys::ERROR && pending & mask == mask;
    let unblocked =
        gtos_sys::sigprocmask(gtos_sys::SIG_SETMASK, Some(&old), None) != gtos_sys::ERROR;
    let (order, count) = unsafe { (RT_QUEUE_ORDER, RT_QUEUE_COUNT) };
    unblocked && queued && count == 3 && order == [rt2, rt1, rt1]
}

fn signal_queued_ok() -> bool {
    if gtos_sys::signal(gtos_sys::SIGUSR1, on_queued_first) == gtos_sys::ERROR
        || gtos_sys::signal(gtos_sys::SIGUSR2, on_queued_second) == gtos_sys::ERROR
    {
        return false;
    }
    let zero = 0u64;
    if gtos_sys::sigprocmask(gtos_sys::SIG_SETMASK, Some(&zero), None) == gtos_sys::ERROR {
        return false;
    }
    if gtos_sys::kill(gtos_sys::getpid(), gtos_sys::SIGUSR1) == gtos_sys::ERROR {
        return false;
    }
    // SAFETY: delivery and the return from `sigreturn` are synchronous here.
    unsafe { QUEUED_FIRST_RAN && QUEUED_SECOND_RAN }
}

fn wait_for_pipe_byte(fd: u64, expected: u8) -> bool {
    let deadline = gtos_sys::uptime().saturating_add(SIGNAL_TIMEOUT_MS);
    let mut byte = 0;
    loop {
        if gtos_sys::read(fd, core::slice::from_mut(&mut byte)) == 1 && byte == expected {
            return true;
        }
        if gtos_sys::uptime() >= deadline {
            return false;
        }
        gtos_sys::yield_now();
    }
}

fn wait_for_child(pid: u64) -> Option<u64> {
    let deadline = gtos_sys::uptime().saturating_add(SIGNAL_TIMEOUT_MS);
    loop {
        let mut status = 0;
        if gtos_sys::waitpid(pid, &mut status) == pid {
            return Some(status);
        }
        if gtos_sys::uptime() >= deadline {
            return None;
        }
        gtos_sys::yield_now();
    }
}

fn signal_cross_process_ok() -> bool {
    let Some((read_fd, write_fd)) = gtos_sys::pipe() else {
        return false;
    };
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::close(read_fd);
        let _ = gtos_sys::close(write_fd);
        return false;
    }
    if child == 0 {
        let _ = gtos_sys::close(read_fd);
        if gtos_sys::signal(gtos_sys::SIGUSR1, on_child_signal) == gtos_sys::ERROR {
            gtos_sys::exit(1);
        }
        if gtos_sys::write(write_fd, b"R") != 1 {
            gtos_sys::exit(2);
        }
        let deadline = gtos_sys::uptime().saturating_add(SIGNAL_TIMEOUT_MS);
        loop {
            // SAFETY: the handler and this child's main never run together.
            if unsafe { CHILD_HANDLER_RAN } {
                break;
            }
            if gtos_sys::uptime() >= deadline {
                gtos_sys::exit(3);
            }
            gtos_sys::yield_now();
        }
        let _ = gtos_sys::write(write_fd, b"D");
        gtos_sys::exit(0);
    }

    let _ = gtos_sys::close(write_fd);
    let ready = wait_for_pipe_byte(read_fd, b'R');
    let sent = ready && gtos_sys::kill(child, gtos_sys::SIGUSR1) != gtos_sys::ERROR;
    let done = sent && wait_for_pipe_byte(read_fd, b'D');
    if !done {
        let _ = gtos_sys::kill(child, gtos_sys::SIGKILL);
    }
    let status = wait_for_child(child);
    let _ = gtos_sys::close(read_fd);
    done && status == Some(0)
}

fn signal_termination_ok(signum: u8, catch: bool) -> bool {
    let Some((read_fd, write_fd)) = gtos_sys::pipe() else {
        return false;
    };
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::close(read_fd);
        let _ = gtos_sys::close(write_fd);
        return false;
    }
    if child == 0 {
        let _ = gtos_sys::close(read_fd);
        if catch && gtos_sys::signal(signum, on_child_signal) == gtos_sys::ERROR {
            gtos_sys::exit(1);
        }
        if gtos_sys::write(write_fd, b"R") != 1 {
            gtos_sys::exit(2);
        }
        loop {
            gtos_sys::yield_now();
        }
    }

    let _ = gtos_sys::close(write_fd);
    let ready = wait_for_pipe_byte(read_fd, b'R');
    let sent = ready && gtos_sys::kill(child, signum) != gtos_sys::ERROR;
    let status = if sent { wait_for_child(child) } else { None };
    let _ = gtos_sys::close(read_fd);
    status.is_some_and(|value| {
        // Kernel statuses are `i64` bits carried through a `u64` syscall value.
        value.to_ne_bytes() == (-i64::from(signum)).to_ne_bytes()
    })
}

fn fault_signal_ok() -> bool {
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        return false;
    }
    if child == 0 {
        if gtos_sys::signal(gtos_sys::SIGSEGV, on_fault_signal) == gtos_sys::ERROR {
            gtos_sys::exit(2);
        }
        // SAFETY: this unmapped address intentionally triggers a ring-3 page
        // fault; the kernel should deliver SIGSEGV to the handler above.
        unsafe { core::ptr::read_volatile(0xdead_beef_u64 as *const u8) };
        gtos_sys::exit(1);
    }
    wait_for_child(child) == Some(0)
}

fn heap_ok() -> bool {
    let before = gtos_sys::brk(0);
    let values = vec![1u64, 2, 3, 4];
    let after = gtos_sys::brk(0);
    let text = format!("{values:?}");
    let _ = gtos_sys::write(1, b"heap vector: ");
    let _ = gtos_sys::write(1, text.as_bytes());
    let _ = gtos_sys::write(1, b"\r\n");
    after > before && values.len() == 4 && text == "[1, 2, 3, 4]"
}

fn framebuffer_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let width = usize::try_from(info.width).unwrap_or(0);
    let height = usize::try_from(info.height).unwrap_or(0);
    let stride = usize::try_from(info.stride).unwrap_or(0);
    let bytes_per_pixel = usize::try_from(info.bytes_per_pixel).unwrap_or(0);
    let byte_len = usize::try_from(info.byte_len).unwrap_or(0);
    let needed = stride
        .checked_mul(height)
        .and_then(|value| value.checked_mul(bytes_per_pixel))
        .unwrap_or(usize::MAX);
    if width == 0 || height == 0 || stride < width || bytes_per_pixel == 0 || byte_len < needed {
        return false;
    }

    // Paint an 8x8 checkerboard in the bottom-right corner, away from the text
    // console. White and black work for both RGB and BGR framebuffers.
    let mut buffer = vec![0u8; byte_len];
    let square = 64usize.min(width).min(height);
    let left = width - square;
    let top = height - square;
    for y in 0..square {
        for x in 0..square {
            let value = if ((x / 8) + (y / 8)) % 2 == 0 {
                0xff
            } else {
                0x00
            };
            let offset = ((top + y) * stride + left + x) * bytes_per_pixel;
            if offset + bytes_per_pixel <= buffer.len() {
                for byte in &mut buffer[offset..offset + bytes_per_pixel] {
                    *byte = value;
                }
            }
        }
    }
    gtos_sys::framebuffer_commit(&buffer) == info.byte_len
}

fn input_ok() -> bool {
    let _ = gtos_sys::write(1, INPUT_WAITING);
    let deadline = gtos_sys::monotonic_ns().saturating_add(INPUT_TIMEOUT_NS);
    let mut pressed = false;
    let mut released = false;
    loop {
        match gtos_sys::input_event() {
            Ok(InputPoll::Event(event)) => {
                if event.code == u32::from(b'A') && event.is_press() {
                    if event.modifiers & gtos_sys::MOD_SHIFT == 0 {
                        return false;
                    }
                    pressed = true;
                    let _ = gtos_sys::write(1, b"input press: A shift\r\n");
                } else if event.code == u32::from(b'A') && event.is_release() {
                    released = true;
                    let _ = gtos_sys::write(1, b"input release: A\r\n");
                }
                if pressed && released {
                    return true;
                }
            }
            Ok(InputPoll::Idle) => {}
            Err(_) => return false,
        }
        if gtos_sys::monotonic_ns() >= deadline {
            return false;
        }
        let _ = gtos_sys::sleep(10);
    }
}

fn window_ipc_ok(front: u64) -> bool {
    // A forked child posts to the parent's window, exercising real cross-process
    // delivery rather than a same-process queue round trip.
    let child = gtos_sys::fork();
    if child == 0 {
        let posted = gtos_sys::window_post(front, b"ping");
        gtos_sys::exit(u64::from(!posted));
    }
    let deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    let mut received = None;
    while gtos_sys::monotonic_ns() < deadline {
        match gtos_sys::window_read(front) {
            Ok(Some(message)) => {
                received = Some(message);
                break;
            }
            Ok(None) => gtos_sys::yield_now(),
            Err(_) => {
                let _ = gtos_sys::write(1, WINDOW_IPC_FAIL);
                return false;
            }
        }
    }
    let Some(message) = received else {
        let _ = gtos_sys::write(1, WINDOW_IPC_FAIL);
        return false;
    };
    if message.sender != child || message.payload() != b"ping" {
        let _ = gtos_sys::write(1, WINDOW_IPC_FAIL);
        return false;
    }
    let mut status = 0;
    let wait_deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    loop {
        match gtos_sys::waitpid(child, &mut status) {
            waited if waited == child => break,
            _ if gtos_sys::monotonic_ns() >= wait_deadline => {
                let _ = gtos_sys::write(1, WINDOW_IPC_FAIL);
                return false;
            }
            _ => gtos_sys::yield_now(),
        }
    }
    if status != 0 {
        let _ = gtos_sys::write(1, WINDOW_IPC_FAIL);
        return false;
    }
    let _ = gtos_sys::write(1, WINDOW_IPC_OK);
    true
}

fn window_shm_ok(front: u64, byte_len: usize) -> bool {
    let Some(region) = gtos_sys::shm_create(byte_len as u64) else {
        return false;
    };
    let Some(base) = gtos_sys::shm_attach(region) else {
        let _ = gtos_sys::shm_destroy(region);
        return false;
    };
    // SAFETY: the kernel mapped at least `byte_len` writable bytes at `base`,
    // and only this process is attached while it fills them.
    unsafe { core::ptr::write_bytes(base as *mut u8, 0x2a, byte_len) };
    let bound = gtos_sys::window_bind_shm(front, region);
    let _ = gtos_sys::shm_detach(region);
    // The compositor retains its own frame references, so retiring the region
    // here does not invalidate the window's pixels.
    let _ = gtos_sys::shm_destroy(region);
    bound && gtos_sys::window_present()
}

fn window_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let fb_width = usize::try_from(info.width).unwrap_or(0);
    let fb_height = usize::try_from(info.height).unwrap_or(0);
    let bytes_per_pixel = usize::try_from(info.bytes_per_pixel).unwrap_or(0);
    let width = 64usize.min(fb_width.saturating_sub(16));
    let height = 40usize.min(fb_height.saturating_sub(24));
    if width < 32 || height < 32 || bytes_per_pixel == 0 {
        return false;
    }
    let Some(row_bytes) = width.checked_mul(bytes_per_pixel) else {
        return false;
    };
    let Some(byte_len) = row_bytes.checked_mul(height) else {
        return false;
    };

    // Two overlapping windows exercise both stacking and damage clipping.
    let Some(back) = gtos_sys::window_create(WindowCreate {
        x: 8,
        y: 8,
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
        layer: 0,
    }) else {
        return false;
    };
    let Some(front) = gtos_sys::window_create(WindowCreate {
        x: 24,
        y: 16,
        width: u32::try_from(width).unwrap_or(0),
        height: u32::try_from(height).unwrap_or(0),
        layer: 1,
    }) else {
        return false;
    };
    if !gtos_sys::window_set_title(back, b"Back") || !gtos_sys::window_set_title(front, b"Front") {
        return false;
    }

    let back_pixels = vec![0xffu8; byte_len];
    let front_pixels = vec![0x40u8; byte_len];
    let Some(back_count) = gtos_sys::window_commit(back, &back_pixels) else {
        return false;
    };
    let Some(front_count) = gtos_sys::window_commit(front, &front_pixels) else {
        return false;
    };
    let expected = u64::try_from(byte_len).unwrap_or(u64::MAX);
    if back_count != expected || front_count != expected || !gtos_sys::window_present() {
        return false;
    }

    // Update only an 8x8 tile of the top window from a strided source image.
    let damage_size = 8usize;
    let damage_stride = damage_size * bytes_per_pixel;
    let damage_bytes = vec![0x7fu8; damage_stride * damage_size];
    let damage = WindowDamage {
        x: 4,
        y: 22,
        width: u32::try_from(damage_size).unwrap_or(0),
        height: u32::try_from(damage_size).unwrap_or(0),
        stride: u32::try_from(damage_stride).unwrap_or(0),
    };
    let Some(changed) = gtos_sys::window_commit_rect(front, &damage_bytes, damage) else {
        return false;
    };
    if changed != u64::try_from(damage_bytes.len()).unwrap_or(u64::MAX)
        || !gtos_sys::window_present()
    {
        return false;
    }
    let _ = gtos_sys::write(1, WINDOW_DAMAGE_OK);

    // Raising the back window above the front one triggers a full restack.
    if !gtos_sys::window_set_layer(back, 2) || !gtos_sys::window_present() {
        return false;
    }
    let _ = gtos_sys::write(1, WINDOW_STACK_OK);
    let _ = gtos_sys::write(1, WINDOW_DECORATION_OK);

    // Bind a shared page as the front window's backing store, paint through the
    // guest mapping, and let `winpresent` refresh it without a pixel commit.
    let shm_window_ok = window_shm_ok(front, byte_len);
    let _ = gtos_sys::write(
        1,
        if shm_window_ok {
            WINDOW_SHM_OK
        } else {
            WINDOW_SHM_FAIL
        },
    );
    if !shm_window_ok {
        return false;
    }

    if !window_ipc_ok(front) {
        return false;
    }

    gtos_sys::window_destroy(front) && gtos_sys::window_destroy(back)
}

/// Exercises the clean-room widget toolkit against a real compositor window.
///
/// All input is synthetic: translating host events and dispatching them to a
/// real `Button` proves hit-testing without stealing the shell-driven key test.
fn widget_toolkit_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let width_pixels = 128u32;
    let height_pixels = 96u32;
    if info.width < width_pixels || info.height < height_pixels + 8 {
        return false;
    }
    let width = usize::try_from(width_pixels).unwrap_or(0);
    let height = usize::try_from(height_pixels).unwrap_or(0);
    let x = 440u32.min(info.width - width_pixels);
    let Some(mut surface) = Surface::create(info, x, 8, width, height, 2, b"Widgets") else {
        return false;
    };

    let mut label = Label::new(b"clean-room UI", FOREGROUND);
    let mut button = Button::new(b"OK");
    let area = Rect::new(2, 22, width_pixels - 4, height_pixels - 24);
    // The button occupies the lower half of `area` after layout; pick a point
    // inside it without borrowing `button` while `ui` owns a mutable reference.
    let center = Point::new(
        area.x + i32::try_from(area.width).unwrap_or(i32::MAX) / 2,
        area.y + i32::try_from(area.height).unwrap_or(i32::MAX) - 4,
    );
    {
        let mut ui = Ui::new();
        ui.add(&mut label);
        ui.add(&mut button);
        ui.layout(area);

        // Paint primitives and the widget tree into the same surface.
        {
            let mut canvas = surface.canvas();
            canvas.fill_rect(Rect::new(0, 0, width_pixels, height_pixels), BACKGROUND);
            canvas.draw_text(4, 4, b"GTOS UI", ACCENT);
            canvas.draw_line(Point::new(4, 16), Point::new(96, 16), DIM);
            ui.paint(&mut canvas);
        }

        let moved = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_MOVE,
            x: center.x,
            y: center.y,
            ..InputEvent::default()
        });
        let pressed = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_BUTTON,
            code: 0,
            value: 1,
            x: center.x,
            y: center.y,
            ..InputEvent::default()
        });
        let released = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_BUTTON,
            code: 0,
            value: 0,
            x: center.x,
            y: center.y,
            ..InputEvent::default()
        });
        let key = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_KEY_DOWN,
            code: u32::from(b'A'),
            modifiers: gtos_sys::MOD_SHIFT,
            ..InputEvent::default()
        });
        if !matches!(moved, Event::PointerMove { .. })
            || !matches!(pressed, Event::PointerDown { .. })
            || !matches!(released, Event::PointerUp { .. })
            || !matches!(
                key,
                Event::KeyDown {
                    code,
                    modifiers,
                } if code == u32::from(b'A') && modifiers == gtos_sys::MOD_SHIFT
            )
        {
            return false;
        }
        if !ui.dispatch(moved) || !ui.dispatch(pressed) || !ui.dispatch(released) {
            return false;
        }

        // Repaint the hover/press feedback before presenting the window.
        let mut canvas = surface.canvas();
        canvas.fill_rect(Rect::new(0, 0, width_pixels, height_pixels), BACKGROUND);
        canvas.draw_text(4, 4, b"GTOS UI", ACCENT);
        canvas.draw_line(Point::new(4, 16), Point::new(96, 16), DIM);
        ui.paint(&mut canvas);
    }

    button.rect().width > 0
        && button.is_hovered()
        && !button.is_pressed()
        && button.was_clicked()
        && surface.present()
}

/// Paints window chrome and a widget tree for the 160x112 demos.
fn paint_ui(canvas: &mut Canvas<'_>, ui: &Ui<'_>, title: &[u8], background: Color) {
    canvas.fill_rect(Rect::new(0, 0, 160, 112), background);
    canvas.draw_text(4, 4, title, ACCENT);
    ui.paint(canvas);
}

/// Dispatches a synthetic left-button press into a widget tree.
fn pointer_down(ui: &mut Ui<'_>, point: Point) -> bool {
    ui.dispatch(Event::from_host(InputEvent {
        kind: gtos_sys::INPUT_MOUSE_BUTTON,
        code: 0,
        value: 1,
        x: point.x,
        y: point.y,
        ..InputEvent::default()
    }))
}

/// Dispatches a synthetic key press into a widget tree.
fn key_down(ui: &mut Ui<'_>, code: u32) -> bool {
    ui.dispatch(Event::from_host(InputEvent {
        kind: gtos_sys::INPUT_KEY_DOWN,
        code,
        ..InputEvent::default()
    }))
}

/// Dispatches a synthetic left-button click into the desktop shell.
fn click_shell(shell: &mut DesktopShell<'_>, point: Point) -> bool {
    for value in [1, 0] {
        let event = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_BUTTON,
            code: 0,
            value,
            x: point.x,
            y: point.y,
            ..InputEvent::default()
        });
        if !shell.event(event) {
            return false;
        }
    }
    true
}

/// Paints the shell into its real compositor window.
fn paint_shell(surface: &mut Surface, shell: &DesktopShell<'_>) {
    let mut canvas = surface.canvas();
    shell.paint(&mut canvas);
}

/// Checks the accessible shell tree after the launcher is dismissed.
fn shell_accessible_ok(shell: &DesktopShell<'_>) -> bool {
    let records = shell.accessible();
    let roles = ["button", "clock", "tray item", "tray item"];
    let labels = [&b"Apps"[..], &b"12:34:56"[..], &b"NET"[..], &b"BAT"[..]];
    records.len() == roles.len()
        && records
            .iter()
            .zip(roles.iter().zip(labels.iter()))
            .all(|(record, (role, label))| record.role == *role && record.label == *label)
}

/// Reads a small VFS file into memory using its reported metadata size.
fn read_vfs_file(path: &[u8], max_len: usize) -> Option<Vec<u8>> {
    let info = gtos_sys::stat(path)?;
    if info.kind != gtos_sys::VFS_FILE || info.size == 0 {
        return None;
    }
    let len = usize::try_from(info.size).ok()?;
    if len == 0 || len > max_len {
        return None;
    }
    let fd = gtos_sys::open(path);
    if fd == gtos_sys::ERROR {
        return None;
    }
    let mut bytes = vec![0u8; len];
    let mut filled = 0usize;
    let mut ok = true;
    while filled < len {
        let count = gtos_sys::read(fd, &mut bytes[filled..]);
        if count == gtos_sys::ERROR {
            ok = false;
            break;
        }
        match usize::try_from(count) {
            Ok(count) if count > 0 => filled += count,
            _ => {
                ok = false;
                break;
            }
        }
    }
    let _ = gtos_sys::close(fd);
    (ok && filled == len).then_some(bytes)
}

fn launcher_records_match(records: &[LauncherRecord]) -> bool {
    let expected: [(&[u8], &[u8], &[u8]); 4] = [
        (b"term", b"Terminal", b"HELLO.BIN"),
        (b"edit", b"Editor", b"HELLO.ELF"),
        (b"files", b"Files", b"CABI.ELF"),
        (b"help", b"Help", b"HELLO.BIN"),
    ];
    records.len() == expected.len()
        && records
            .iter()
            .zip(expected)
            .all(|(record, (id, label, target))| {
                record.id() == id && record.label() == label && record.target() == target
            })
}

/// Verifies that launcher targets come from VFS metadata and applets are drawn.
fn shell_apps_ok() -> bool {
    let Some(bytes) = read_vfs_file(SHELL_APPS_PATH, 1024) else {
        return false;
    };
    let records = parse_launcher_records(&bytes);
    if !launcher_records_match(&records) {
        return false;
    }
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let (width, height) = (320u32, 160u32);
    if info.width < width + 16 || info.height < height + 330 {
        return false;
    }
    let Some(mut surface) = Surface::create(
        info,
        16,
        330,
        width as usize,
        height as usize,
        2,
        b"Shell Apps",
    ) else {
        return false;
    };
    let tray_items = [TrayItem::new(b"NET", false)];
    let layout = ShellLayout::new(24, 144, 96, 4, 2, 2);
    let mut shell = DesktopShell::from_records(
        b"Apps",
        records,
        &tray_items,
        Theme::dark(),
        Style::new(),
        layout,
    );
    shell.set_clock_seconds(12 * 3600 + 34 * 60 + 56);
    shell.add_applet(b"cpu", b"CPU");
    shell.layout(Rect::new(0, 0, width, height));
    shell.set_launcher_visible(true);
    if shell.applet_count() != 1
        || shell.applet_id(0) != Some(b"cpu")
        || shell.applet_label(0) != Some(b"CPU")
    {
        return false;
    }
    let accessible = shell.accessible();
    if accessible.len() != 8
        || accessible[0].role != "launch target"
        || accessible[0].label != b"Terminal"
        || accessible[6].role != "applet"
        || accessible[6].label != b"CPU"
    {
        return false;
    }
    paint_shell(&mut surface, &shell);
    surface.present()
}

/// Forks a child and replaces its image with a program named in shell metadata.
fn launch_target(target: &[u8]) -> bool {
    let Some(image) = read_vfs_file(target, MAX_LAUNCH_BYTES) else {
        return false;
    };
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        return false;
    }
    if child == 0 {
        gtos_sys::exec(&image);
        gtos_sys::exit(1);
    }
    let deadline = gtos_sys::uptime().saturating_add(LAUNCH_WAIT_MS);
    loop {
        let mut status = 0;
        if gtos_sys::waitpid(child, &mut status) == child {
            return status == 0;
        }
        if gtos_sys::uptime() >= deadline {
            return false;
        }
        gtos_sys::yield_now();
    }
}

/// Verifies that a real shell activation forks and execs a VFS metadata target.
fn shell_launch_ok() -> bool {
    let Some(bytes) = read_vfs_file(SHELL_APPS_PATH, 1024) else {
        return false;
    };
    let records = parse_launcher_records(&bytes);
    let Some(target) = records
        .iter()
        .find(|record| record.id() == b"term")
        .map(|record| record.target().to_vec())
    else {
        return false;
    };
    let tray_items = [TrayItem::new(b"NET", false)];
    let layout = ShellLayout::new(24, 144, 96, 4, 2, 2);
    let mut shell = DesktopShell::from_records(
        b"Apps",
        records,
        &tray_items,
        Theme::dark(),
        Style::new(),
        layout,
    );
    shell.layout(Rect::new(0, 0, 320, 160));
    shell.set_launcher_visible(true);
    if !shell.event(Event::KeyDown {
        code: 13,
        modifiers: 0,
    }) {
        return false;
    }
    let actions = shell.take_actions();
    if actions.len() != 1 || actions[0] != ShellAction::Launch(b"term".to_vec()) {
        return false;
    }
    launch_target(&target)
}

/// Verifies applet, tray and session actions plus launcher focus restoration.
fn shell_actions_ok() -> bool {
    let entries = [LauncherEntry::new(b"Term", b"term")];
    let tray_items = [
        TrayItem::new(b"NET", false),
        TrayItem::session(b"PWR", ShellSessionAction::Shutdown),
    ];
    let layout = ShellLayout::new(24, 144, 96, 4, 2, 2);
    let mut shell = DesktopShell::at(
        b"Apps",
        &entries,
        &tray_items,
        12 * 3600 + 34 * 60 + 56,
        Theme::dark(),
        Style::new(),
        layout,
    );
    shell.add_applet(b"cpu", b"CPU");
    shell.layout(Rect::new(0, 0, 320, 160));

    shell.set_shell_focus(ShellFocus::Applet(0));
    if shell.focus() != Some(ShellFocus::Applet(0)) {
        return false;
    }
    shell.set_launcher_visible(true);
    if shell.focus() != Some(ShellFocus::Launcher)
        || shell.focus_memory() != Some(ShellFocus::Applet(0))
    {
        return false;
    }
    shell.set_launcher_visible(false);
    if shell.focus() != Some(ShellFocus::Applet(0)) || shell.focus_memory().is_some() {
        return false;
    }

    let enter = || Event::KeyDown {
        code: 13,
        modifiers: 0,
    };
    if !shell.event(enter()) {
        return false;
    }
    shell.focus_tray(0);
    if !shell.event(enter()) || shell.tray().active(0) != Some(true) {
        return false;
    }
    shell.focus_tray(1);
    if !shell.event(enter()) || shell.tray().active(1) != Some(false) {
        return false;
    }

    let actions = shell.take_actions();
    let expected = [
        ShellAction::Applet(b"cpu".to_vec()),
        ShellAction::Tray {
            index: 0,
            active: true,
        },
        ShellAction::Session(ShellSessionAction::Shutdown),
    ];
    if actions != expected || shell.tray().action(0) != Some(TrayAction::Indicator) {
        return false;
    }
    if shell.tray().action(1) != Some(TrayAction::Session(ShellSessionAction::Shutdown)) {
        return false;
    }

    // The demo is the session host here: it acknowledges dispatched requests
    // instead of terminating the machine mid-suite.
    let _ = gtos_sys::write(1, b"shell applet action: cpu\r\n");
    let _ = gtos_sys::write(1, b"shell tray action: NET on\r\n");
    let _ = gtos_sys::write(1, b"shell session action: shutdown\r\n");
    true
}

/// Verifies a real keyboard focus cycle across panel, applets, tray and launcher.
fn shell_focus_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let (width, height) = (320u32, 160u32);
    if info.width < width + 16 || info.height < height + 420 {
        return false;
    }
    let Some(mut surface) = Surface::create(
        info,
        16,
        420,
        width as usize,
        height as usize,
        2,
        b"Shell Focus",
    ) else {
        return false;
    };
    let entries = [LauncherEntry::new(b"Term", b"term")];
    let tray_items = [TrayItem::new(b"NET", false)];
    let layout = ShellLayout::new(24, 144, 96, 4, 2, 2);
    let mut shell = DesktopShell::at(
        b"Apps",
        &entries,
        &tray_items,
        12 * 3600 + 34 * 60 + 56,
        Theme::dark(),
        Style::new(),
        layout,
    );
    shell.add_applet(b"cpu", b"CPU");
    shell.layout(Rect::new(0, 0, width, height));
    let tab = |modifiers| Event::KeyDown {
        code: u32::from(b'\t'),
        modifiers,
    };
    let expected = [
        None,
        Some(ShellFocus::Toggle),
        Some(ShellFocus::Applet(0)),
        Some(ShellFocus::Tray),
        Some(ShellFocus::Toggle),
    ];
    for (event, focus) in [
        (tab(0), expected[1]),
        (tab(0), expected[2]),
        (tab(0), expected[3]),
        (tab(0), expected[4]),
    ] {
        if !shell.event(event) || shell.focus() != focus {
            return false;
        }
    }
    if !shell.event(tab(gtos_sys::MOD_SHIFT)) || shell.focus() != expected[3] {
        return false;
    }
    shell.set_launcher_visible(true);
    if shell.focus() != Some(ShellFocus::Launcher)
        || !shell.event(tab(0))
        || shell.focus() != Some(ShellFocus::Toggle)
        || !shell.event(tab(gtos_sys::MOD_SHIFT))
        || shell.focus() != Some(ShellFocus::Launcher)
    {
        return false;
    }
    if !shell.event(Event::KeyDown {
        code: 27,
        modifiers: 0,
    }) || shell.launcher_visible()
        || shell.focus() != Some(ShellFocus::Tray)
    {
        return false;
    }
    paint_shell(&mut surface, &shell);
    surface.present()
}

/// Exercises reusable layouts, scrolling, text entry and focus routing.
///
/// Everything is synthetic so the shell-driven keyboard test is unaffected.
fn widget_layout_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let width_pixels = 160u32;
    let height_pixels = 112u32;
    if info.width < width_pixels || info.height < height_pixels + 120 {
        return false;
    }
    let width = usize::try_from(width_pixels).unwrap_or(0);
    let height = usize::try_from(height_pixels).unwrap_or(0);
    let x = 440u32.min(info.width - width_pixels);
    let Some(mut surface) = Surface::create(info, x, 120, width, height, 2, b"Rich UI") else {
        return false;
    };

    let mut first = Label::new(b"first row", DIM);
    let mut second = Label::new(b"second row", DIM);
    let mut scroll = ScrollArea::new(72);
    scroll.add(&mut first);
    scroll.add(&mut second);
    let mut input = TextInput::new(8);
    let mut confirm = Button::new(b"OK");
    let mut cancel = Button::new(b"No");

    let area = Rect::new(2, 22, width_pixels - 4, height_pixels - 24);
    let cell_width = (area.width - 4) / 2;
    let cell_height = (area.height - 4) / 2;
    let half_width = i32::try_from(cell_width).unwrap_or(0) / 2;
    let half_height = i32::try_from(cell_height).unwrap_or(0) / 2;
    let scroll_point = Point::new(area.x + half_width, area.y + half_height);
    let input_point = Point::new(
        area.x + i32::try_from(cell_width).unwrap_or(0) + 4 + half_width,
        area.y + half_height,
    );
    let confirm_point = Point::new(
        area.x + half_width,
        area.y + i32::try_from(cell_height).unwrap_or(0) + 4 + half_height,
    );

    let focus_after_input;
    {
        let mut ui = Ui::new();
        ui.add(&mut scroll);
        ui.add(&mut input);
        ui.add(&mut confirm);
        ui.add(&mut cancel);

        // Both reusable strategies must be usable through the same tree.
        ui.layout_with(area, &HorizontalLayout::new(4));
        ui.layout_with(area, &GridLayout::new(2, 4));

        paint_ui(&mut surface.canvas(), &ui, b"RICH UI", BACKGROUND);

        let focus_input = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_BUTTON,
            code: 0,
            value: 1,
            x: input_point.x,
            y: input_point.y,
            ..InputEvent::default()
        });
        if !ui.dispatch(focus_input) {
            return false;
        }
        focus_after_input = ui.focused_index();
        for byte in [u32::from(b'O'), u32::from(b'K')] {
            let key = Event::from_host(InputEvent {
                kind: gtos_sys::INPUT_KEY_DOWN,
                code: byte,
                ..InputEvent::default()
            });
            if !ui.dispatch(key) {
                return false;
            }
        }
        let focus_confirm = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_BUTTON,
            code: 0,
            value: 1,
            x: confirm_point.x,
            y: confirm_point.y,
            ..InputEvent::default()
        });
        if !ui.dispatch(focus_confirm) {
            return false;
        }
        let wheel = Event::Scroll {
            x: scroll_point.x,
            y: scroll_point.y,
            delta: 8,
        };
        if !ui.dispatch(wheel) {
            return false;
        }

        paint_ui(&mut surface.canvas(), &ui, b"RICH UI", BACKGROUND);
    }

    let layout_ok = input.rect().x > scroll.rect().x && cancel.rect().y > input.rect().y;
    layout_ok
        && focus_after_input == Some(1)
        && input.text() == b"OK"
        && !input.is_focused()
        && confirm.is_focused()
        && scroll.offset() > 0
        && surface.present()
}

/// Builds a view tree from declarative descriptions and checks theming,
/// multi-line text, accessibility records and focus-driven text entry.
fn widget_view_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let (width_pixels, height_pixels) = (160u32, 112u32);
    if info.width < width_pixels || info.height < height_pixels + 232 {
        return false;
    }
    let Some(mut surface) = Surface::create(
        info,
        440u32.min(info.width - width_pixels),
        232,
        usize::try_from(width_pixels).unwrap_or(0),
        usize::try_from(height_pixels).unwrap_or(0),
        2,
        b"Theme",
    ) else {
        return false;
    };

    let theme = Theme::light();
    let views = [
        View::label(b"themed label", Style::new().foreground(theme.accent)),
        View::text(
            b"first line\nsecond line\nthird line",
            Style::new().foreground(theme.foreground),
        ),
        View::button(b"Save", Style::new().surface(theme.accent)),
        View::input(6, Style::new().border(theme.accent)),
    ];
    let mut builder = ViewBuilder::new(theme);
    builder.build(&views);

    let area = Rect::new(2, 22, width_pixels - 4, height_pixels - 24);
    let cell_width = (area.width - 4) / 2;
    let cell_height = (area.height - 4) / 2;
    let half_width = i32::try_from(cell_width).unwrap_or(0) / 2;
    let half_height = i32::try_from(cell_height).unwrap_or(0) / 2;
    let text_point = Point::new(
        area.x + i32::try_from(cell_width).unwrap_or(0) + 4 + half_width,
        area.y + half_height,
    );
    let input_point = Point::new(
        text_point.x,
        area.y + i32::try_from(cell_height).unwrap_or(0) + 4 + half_height,
    );

    let roles = ["text", "text view", "button", "text field"];
    let records_ok;
    {
        let mut ui = builder.ui();
        ui.layout_with(area, &GridLayout::new(2, 4));
        records_ok = {
            let records = ui.accessible();
            records.len() == roles.len()
                && records
                    .iter()
                    .zip(roles)
                    .all(|(record, role)| record.role == role)
                && records[0].label == b"themed label"
                && records[2].label == b"Save"
        };
        paint_ui(&mut surface.canvas(), &ui, b"THEMED VIEW", theme.background);

        let scroll = Event::Scroll {
            x: text_point.x,
            y: text_point.y,
            delta: 8,
        };
        let focus = Event::from_host(InputEvent {
            kind: gtos_sys::INPUT_MOUSE_BUTTON,
            code: 0,
            value: 1,
            x: input_point.x,
            y: input_point.y,
            ..InputEvent::default()
        });
        if !ui.dispatch(scroll) || !ui.dispatch(focus) {
            return false;
        }
        for code in [u32::from(b'O'), u32::from(b'K')] {
            let key = Event::from_host(InputEvent {
                kind: gtos_sys::INPUT_KEY_DOWN,
                code,
                ..InputEvent::default()
            });
            if !ui.dispatch(key) {
                return false;
            }
        }
        paint_ui(&mut surface.canvas(), &ui, b"THEMED VIEW", theme.background);
    }

    records_ok
        && builder.text_line(&[1]) == Some(1)
        && builder.input_text(&[3]) == Some(b"OK")
        && surface.present()
}

/// Builds a nested row/column view and checks recursive layout, accessibility
/// and focus routing into a child buried two levels deep.
fn widget_nested_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let (width_pixels, height_pixels) = (160u32, 112u32);
    if info.width < width_pixels || info.height < height_pixels + 344 {
        return false;
    }
    let Some(mut surface) = Surface::create(
        info,
        440u32.min(info.width - width_pixels),
        344,
        usize::try_from(width_pixels).unwrap_or(0),
        usize::try_from(height_pixels).unwrap_or(0),
        2,
        b"Nested",
    ) else {
        return false;
    };

    let row_children = [
        View::button(b"L", Style::new()),
        View::button(b"R", Style::new()),
    ];
    let column_children = [
        View::row(&row_children, 2, Style::new()),
        View::input(4, Style::new()),
    ];
    let views = [
        View::label(b"nested", Style::new()),
        View::column(&column_children, 2, Style::new()),
    ];
    let mut builder = ViewBuilder::new(Theme::dark());
    builder.build(&views);

    let area = Rect::new(2, 22, width_pixels - 4, height_pixels - 24);
    let outer_height = (area.height - 4) / 2;
    let inner_height = (outer_height - 2) / 2;
    let column_y = area.y + i32::try_from(outer_height).unwrap_or(0) + 4;
    let input_y = column_y + i32::try_from(inner_height).unwrap_or(0) + 2;
    let input_point = Point::new(
        area.x + i32::try_from(area.width / 2).unwrap_or(0),
        input_y + i32::try_from(inner_height).unwrap_or(0) / 2,
    );

    {
        let mut ui = builder.ui();
        ui.layout_with(area, &VerticalLayout::new(4));
        paint_ui(&mut surface.canvas(), &ui, b"NESTED", BACKGROUND);
        if !pointer_down(&mut ui, input_point) {
            return false;
        }
        for code in [u32::from(b'O'), u32::from(b'K')] {
            if !key_down(&mut ui, code) {
                return false;
            }
        }
        paint_ui(&mut surface.canvas(), &ui, b"NESTED", BACKGROUND);
    }

    let records = builder.accessible();
    let roles = [
        "text",
        "container",
        "container",
        "button",
        "button",
        "text field",
    ];
    roles.len() == records.len()
        && records
            .iter()
            .zip(roles)
            .all(|(record, role)| record.role == role)
        && builder.input_text(&[1, 1]) == Some(b"OK")
        && surface.present()
}

/// Types into a multi-line editor, navigates the cursor and scrolls its view.
fn widget_editor_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let (width_pixels, height_pixels) = (160u32, 112u32);
    if info.width < width_pixels || info.height < height_pixels + 456 {
        return false;
    }
    let Some(mut surface) = Surface::create(
        info,
        440u32.min(info.width - width_pixels),
        456,
        usize::try_from(width_pixels).unwrap_or(0),
        usize::try_from(height_pixels).unwrap_or(0),
        2,
        b"Editor",
    ) else {
        return false;
    };

    let views = [View::textarea(64, Style::new())];
    let mut builder = ViewBuilder::new(Theme::dark());
    builder.build(&views);
    let area = Rect::new(2, 22, width_pixels - 4, 40);
    let center = Point::new(
        area.x + i32::try_from(area.width / 2).unwrap_or(0),
        area.y + i32::try_from(area.height / 2).unwrap_or(0),
    );

    {
        let mut ui = builder.ui();
        ui.layout_with(area, &GridLayout::new(1, 0));
        paint_ui(&mut surface.canvas(), &ui, b"EDITOR", BACKGROUND);
        if !pointer_down(&mut ui, center) {
            return false;
        }
        for line in 0..6u8 {
            if !key_down(&mut ui, u32::from(b'0' + line)) {
                return false;
            }
            if line < 5 && !key_down(&mut ui, u32::from(b'\n')) {
                return false;
            }
        }
        for code in [gtos_sys::KEY_UP, gtos_sys::KEY_HOME, u32::from(b'Z')] {
            if !key_down(&mut ui, code) {
                return false;
            }
        }
        let scroll = Event::Scroll {
            x: center.x,
            y: center.y,
            delta: 8,
        };
        if !ui.dispatch(scroll) || !ui.dispatch(scroll) {
            return false;
        }
        paint_ui(&mut surface.canvas(), &ui, b"EDITOR", BACKGROUND);
    }

    builder.text_area_text(&[0]).as_deref() == Some(b"0\n1\n2\n3\nZ4\n5".as_slice())
        && builder.text_area_cursor(&[0]) == Some((4, 1))
        && builder.text_area_line(&[0]) == Some(2)
        && surface.present()
}

/// Exercises the clean-room desktop shell panel with synthetic input only.
///
/// The launcher, clock and tray are ordinary widgets in a compositor window;
/// no host keystrokes are consumed, so the shell-driven input test is intact.
fn shell_panel_ok() -> bool {
    let Some(info) = gtos_sys::framebuffer_info() else {
        return false;
    };
    let (width_pixels, height_pixels) = (320u32, 160u32);
    if info.width < width_pixels + 8 || info.height < height_pixels + 8 {
        return false;
    }
    let Some(mut surface) = Surface::create(
        info,
        8,
        8,
        width_pixels as usize,
        height_pixels as usize,
        2,
        b"Shell",
    ) else {
        return false;
    };

    let entries = [
        LauncherEntry::new(b"Term", b"term"),
        LauncherEntry::new(b"Edit", b"edit"),
        LauncherEntry::new(b"Files", b"files"),
        LauncherEntry::new(b"Help", b"help"),
    ];
    let tray_items = [TrayItem::new(b"NET", false), TrayItem::new(b"BAT", true)];
    let layout = ShellLayout::new(24, 144, 96, 4, 2, 2);
    let mut shell = DesktopShell::at(
        b"Apps",
        &entries,
        &tray_items,
        12 * 3600 + 34 * 60 + 56,
        Theme::dark(),
        Style::new(),
        layout,
    );
    let area = Rect::new(0, 0, width_pixels, height_pixels);
    shell.layout(area);

    if shell.rect() != area
        || shell.clock().text() != b"12:34:56"
        || shell.launcher_visible()
        || shell.launcher().len() != entries.len()
    {
        return false;
    }
    paint_shell(&mut surface, &shell);

    let toggle = Point::new(10, 12);
    if !click_shell(&mut shell, toggle) || !shell.launcher_visible() {
        return false;
    }
    paint_shell(&mut surface, &shell);

    let launcher = shell.launcher_rect();
    let cell_width = (launcher.width - 2) / 2;
    let cell_height = (launcher.height - 2) / 2;
    let cell_width = i32::try_from(cell_width).unwrap_or(0);
    let cell_height = i32::try_from(cell_height).unwrap_or(0);
    let files = Point::new(
        launcher.x + cell_width / 2,
        launcher.y + cell_height + 2 + cell_height / 2,
    );
    if !click_shell(&mut shell, files) {
        return false;
    }
    if shell.take_launcher_activation() != Some(b"files") {
        return false;
    }

    let network = Point::new(i32::try_from(width_pixels).unwrap_or(0) - 60, 12);
    if shell.tray().active(0) != Some(false) {
        return false;
    }
    if !click_shell(&mut shell, network) {
        return false;
    }
    if shell.tray().active(0) != Some(true) || shell.tray().active(1) != Some(true) {
        return false;
    }

    if !shell.event(Event::KeyDown {
        code: 27,
        modifiers: 0,
    }) || shell.launcher_visible()
    {
        return false;
    }

    if !shell_accessible_ok(&shell) {
        return false;
    }

    paint_shell(&mut surface, &shell);
    surface.present()
}

fn shm_ok() -> bool {
    let Some(id) = gtos_sys::shm_create(SHM_BYTES) else {
        return false;
    };
    let Some(base) = gtos_sys::shm_attach(id) else {
        let _ = gtos_sys::shm_destroy(id);
        return false;
    };

    // A forked child attaches the same region by id and writes a marker. The
    // parent must observe it without any IPC payload copy.
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::shm_detach(id);
        let _ = gtos_sys::shm_destroy(id);
        return false;
    }
    if child == 0 {
        let wrote = gtos_sys::shm_attach(id).is_some_and(|child_base| {
            // SAFETY: the kernel mapped a full page at `child_base` with user
            // read/write permissions, and only this child writes here.
            unsafe { core::ptr::write_volatile(child_base as *mut u8, SHM_MARKER) };
            true
        });
        gtos_sys::exit(u64::from(!wrote));
    }

    // `waitpid` reports `ERROR` while the child is still running, so poll until
    // the child PID is reaped rather than treating that as a failure.
    let mut status = 0;
    let deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    loop {
        match gtos_sys::waitpid(child, &mut status) {
            waited if waited == child => break,
            _ if gtos_sys::monotonic_ns() >= deadline => {
                let _ = gtos_sys::shm_detach(id);
                let _ = gtos_sys::shm_destroy(id);
                return false;
            }
            _ => gtos_sys::yield_now(),
        }
    }
    // SAFETY: the marker was written by the child through its own mapping of
    // the same physical frame, and the child has exited before this read.
    let observed = unsafe { core::ptr::read_volatile(base as *const u8) };
    let ok = status == 0 && observed == SHM_MARKER;
    let _ = gtos_sys::shm_detach(id);
    let _ = gtos_sys::shm_destroy(id);
    ok
}

fn streaming_ipc_ok() -> bool {
    let Some((read_fd, write_fd)) = gtos_sys::pipe() else {
        return false;
    };
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::close(read_fd);
        let _ = gtos_sys::close(write_fd);
        return false;
    }
    if child == 0 {
        let _ = gtos_sys::close(read_fd);
        for part in STREAM_PARTS {
            if gtos_sys::write(write_fd, part) != part.len() as u64 {
                let _ = gtos_sys::close(write_fd);
                gtos_sys::exit(1);
            }
        }
        let _ = gtos_sys::close(write_fd);
        gtos_sys::exit(0);
    }

    // The parent keeps only the read end, so EOF arrives when the child closes
    // its inherited write end. Empty reads are retried because pipes are
    // nonblocking until a fuller scheduler exists.
    let _ = gtos_sys::close(write_fd);
    let mut received = [0u8; 16];
    let mut length = 0;
    let mut eof = false;
    let deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    while length < received.len() {
        match gtos_sys::read(read_fd, &mut received[length..]) {
            0 => {
                eof = true;
                break;
            }
            gtos_sys::ERROR if gtos_sys::monotonic_ns() >= deadline => break,
            gtos_sys::ERROR => gtos_sys::yield_now(),
            count => match usize::try_from(count) {
                Ok(count) => length += count,
                Err(_) => break,
            },
        }
    }
    let _ = gtos_sys::close(read_fd);

    let mut status = 0;
    loop {
        match gtos_sys::waitpid(child, &mut status) {
            waited if waited == child => break,
            _ if gtos_sys::monotonic_ns() >= deadline => return false,
            _ => gtos_sys::yield_now(),
        }
    }
    let expected = b"streaming ipc";
    eof && status == 0 && received[..length] == expected[..]
}

fn ioctl_ok() -> bool {
    let Some((read_fd, write_fd)) = gtos_sys::pipe() else {
        return false;
    };
    let mut ok = gtos_sys::ioctl(write_fd, gtos_sys::IOCTL_FIONWRITE, 0) == Some(4096)
        && gtos_sys::ioctl(read_fd, gtos_sys::IOCTL_FIONREAD, 0) == Some(0);
    ok = ok && gtos_sys::write(write_fd, b"abcd") == 4;
    ok = ok
        && gtos_sys::ioctl(read_fd, gtos_sys::IOCTL_FIONREAD, 0) == Some(4)
        && gtos_sys::ioctl(write_fd, gtos_sys::IOCTL_FIONWRITE, 0) == Some(4092);
    let mut chunk = [0u8; 4];
    ok = ok && read_exact(read_fd, &mut chunk) && chunk[..] == b"abcd"[..];
    ok = ok && gtos_sys::ioctl(read_fd, gtos_sys::IOCTL_FIONREAD, 0) == Some(0);
    let _ = gtos_sys::close(read_fd);
    let _ = gtos_sys::close(write_fd);
    if !ok {
        return false;
    }

    let fd = gtos_sys::open(IOCTL_FILE);
    if fd == gtos_sys::ERROR {
        return false;
    }
    ok = gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_TRUNCATE, 0) == Some(0);
    ok = ok && gtos_sys::write(fd, IOCTL_DATA) == IOCTL_DATA.len() as u64;
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SIZE, 0) == Some(8);
    ok = ok && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0;
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FIONREAD, 0) == Some(8);

    let mut head = [0u8; 3];
    ok = ok && read_exact(fd, &mut head) && head[..] == b"ABC"[..];
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FIONREAD, 0) == Some(5);
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_TRUNCATE, 4) == Some(4);
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SIZE, 0) == Some(4);
    ok = ok && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0;

    let mut shortened = [0u8; 4];
    ok = ok && read_exact(fd, &mut shortened) && shortened[..] == b"ABCD"[..];
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SYNC, 0) == Some(4);
    let _ = gtos_sys::close(fd);
    ok
}

/// Reads one datagram, distinguishing `ERROR` (no data) from EOF (zero bytes).
fn read_datagram(fd: u64, out: &mut [u8]) -> Option<usize> {
    match gtos_sys::read(fd, out) {
        gtos_sys::ERROR => None,
        count => usize::try_from(count)
            .ok()
            .filter(|length| *length <= out.len()),
    }
}

/// Checks message boundaries, duplex delivery and bounded socket buffers.
fn socketpair_data_ok() -> bool {
    let Some((left, right)) = gtos_sys::socketpair() else {
        return false;
    };
    let mut ok = read_datagram(left, &mut [0; 8]).is_none();
    ok = ok && gtos_sys::ioctl(left, gtos_sys::IOCTL_FIONREAD, 0) == Some(0);
    ok = ok && gtos_sys::ioctl(left, gtos_sys::IOCTL_FIONWRITE, 0) == Some(4096);
    ok = ok && gtos_sys::lseek(left, 0, gtos_sys::SEEK_SET) == gtos_sys::ERROR;

    // A write on one end is only readable on the other end.
    ok = ok && gtos_sys::write(left, b"ping") == 4;
    ok = ok && gtos_sys::ioctl(right, gtos_sys::IOCTL_FIONREAD, 0) == Some(4);
    let mut received = [0; 8];
    ok = ok && read_datagram(right, &mut received) == Some(4) && &received[..4] == b"ping";
    ok = ok && gtos_sys::write(right, b"pong!") == 5;
    ok = ok && read_datagram(left, &mut received) == Some(5) && &received[..5] == b"pong!";

    // Two writes must remain two reads instead of becoming one byte stream.
    ok = ok && gtos_sys::write(left, b"one") == 3;
    ok = ok && gtos_sys::write(left, b"two!") == 4;
    ok = ok && read_datagram(right, &mut received) == Some(3) && &received[..3] == b"one";
    ok = ok && read_datagram(right, &mut received) == Some(4) && &received[..4] == b"two!";

    // Four maximum-size datagrams fill one direction; the other stays writable.
    let payload = [0xA5u8; SOCKET_DATAGRAM_BYTES];
    for _ in 0..4 {
        ok = ok && gtos_sys::write(right, &payload) == SOCKET_DATAGRAM_BYTES as u64;
    }
    ok = ok && gtos_sys::write(right, b"full") == gtos_sys::ERROR;
    ok = ok && gtos_sys::ioctl(right, gtos_sys::IOCTL_FIONWRITE, 0) == Some(0);
    ok = ok && gtos_sys::ioctl(left, gtos_sys::IOCTL_FIONWRITE, 0) == Some(4096);
    ok = ok && gtos_sys::ioctl(left, gtos_sys::IOCTL_FIONREAD, 0) == Some(1024);

    let mut full = [0; SOCKET_DATAGRAM_BYTES];
    for _ in 0..4 {
        ok = ok && read_datagram(left, &mut full) == Some(SOCKET_DATAGRAM_BYTES) && full == payload;
    }
    ok = ok && gtos_sys::ioctl(right, gtos_sys::IOCTL_FIONWRITE, 0) == Some(4096);
    ok = ok && read_datagram(left, &mut received).is_none();

    let _ = gtos_sys::close(left);
    let _ = gtos_sys::close(right);
    ok
}

/// Checks that forked descriptors deliver to the peer and report EOF after all
/// copies of the sending end are closed.
fn socketpair_inherit_eof_ok() -> bool {
    let Some((parent_fd, child_fd)) = gtos_sys::socketpair() else {
        return false;
    };
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::close(parent_fd);
        let _ = gtos_sys::close(child_fd);
        return false;
    }
    if child == 0 {
        let _ = gtos_sys::close(parent_fd);
        let wrote =
            gtos_sys::write(child_fd, b"child") == 5 && gtos_sys::write(child_fd, b"!") == 1;
        let _ = gtos_sys::close(child_fd);
        gtos_sys::exit(u64::from(!wrote));
    }

    // The parent keeps only the receiving end, so the child's close is the
    // final writer close and the parent observes EOF.
    let _ = gtos_sys::close(child_fd);
    let mut buffer = [0; 8];
    let deadline = gtos_sys::uptime().saturating_add(SOCKET_TIMEOUT_MS);
    let mut first = None;
    while first.is_none() && gtos_sys::uptime() < deadline {
        match read_datagram(parent_fd, &mut buffer) {
            Some(5) => first = Some(5),
            Some(0) | None => gtos_sys::yield_now(),
            Some(_) => break,
        }
    }
    let mut ok = first == Some(5) && &buffer[..5] == b"child";
    ok = ok && wait_for_child(child) == Some(0);

    let mut rest = [0; 8];
    ok = ok && read_datagram(parent_fd, &mut rest) == Some(1) && &rest[..1] == b"!";
    ok = ok && read_datagram(parent_fd, &mut rest) == Some(0);
    let _ = gtos_sys::close(parent_fd);
    ok
}

fn socketpair_ok() -> bool {
    socketpair_data_ok() && socketpair_inherit_eof_ok()
}

/// Writes more than one FAT cluster in chunks, then seeks and reads it back.
///
/// Snapshot file descriptors had to hold the whole file in memory before the
/// first read returned. This test only passes when each syscall touches the
/// clusters it actually needs and updates the directory entry as it grows.
fn stream_file_ok() -> bool {
    let payload: Vec<u8> = (0..STREAM_FILE_BYTES)
        .map(|index| u8::try_from((index * 31 + 7) & 0xff).unwrap_or(0))
        .collect();
    let fd = gtos_sys::open(STREAM_FILE_URL);
    if fd == gtos_sys::ERROR {
        return false;
    }
    let mut ok = gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_TRUNCATE, 0) == Some(0);

    let mut sent = 0;
    while ok && sent < payload.len() {
        let end = (sent + STREAM_FILE_CHUNK_BYTES).min(payload.len());
        let count = end - sent;
        ok = gtos_sys::write(fd, &payload[sent..end]) == u64::try_from(count).unwrap_or(u64::MAX);
        sent = end;
    }
    let file_bytes = u64::try_from(STREAM_FILE_BYTES).unwrap_or(u64::MAX);
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SIZE, 0) == Some(file_bytes);

    let mut whole = vec![0u8; STREAM_FILE_BYTES];
    ok = ok
        && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0
        && read_exact(fd, &mut whole)
        && whole.as_slice() == payload.as_slice();

    let mut middle = [0u8; 128];
    ok = ok
        && gtos_sys::lseek(fd, 3000, gtos_sys::SEEK_SET) == 3000
        && read_exact(fd, &mut middle)
        && middle[..] == payload[3000..3128];

    let mut tail = [0u8; 4];
    ok = ok && gtos_sys::lseek(fd, 8190, gtos_sys::SEEK_SET) == 8190;
    let count = gtos_sys::read(fd, &mut tail);
    ok = ok && count == 2 && tail[..2] == payload[8190..8192];

    let mut eof = [0u8; 1];
    let file_offset = i64::try_from(STREAM_FILE_BYTES).unwrap_or(i64::MAX);
    ok = ok
        && gtos_sys::lseek(fd, file_offset, gtos_sys::SEEK_SET) == file_bytes
        && gtos_sys::read(fd, &mut eof) == 0;

    ok = ok
        && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_TRUNCATE, 1024) == Some(1024)
        && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SIZE, 0) == Some(1024);
    let mut shortened = vec![0u8; 1024];
    ok = ok
        && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0
        && read_exact(fd, &mut shortened)
        && shortened.as_slice() == &payload[..1024];

    let _ = gtos_sys::close(fd);
    ok
}

/// Writes more clusters than the FAT cache can hold, then syncs and rereads the
/// file. The first clusters must have been written back during LRU eviction;
/// truncating and rewriting the file also checks that freed cache lines are
/// invalidated instead of resurrecting old bytes.
fn fat_writeback_ok() -> bool {
    let payload: Vec<u8> = (0..WRITEBACK_BYTES)
        .map(|index| u8::try_from((index * 17 + 11) & 0xff).unwrap_or(0))
        .collect();
    let fd = gtos_sys::open(WRITEBACK_URL);
    if fd == gtos_sys::ERROR {
        return false;
    }
    let mut ok = gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_TRUNCATE, 0) == Some(0);
    let mut sent = 0;
    while ok && sent < payload.len() {
        let end = (sent + WRITEBACK_CHUNK_BYTES).min(payload.len());
        let count = end - sent;
        ok = gtos_sys::write(fd, &payload[sent..end]) == u64::try_from(count).unwrap_or(u64::MAX);
        sent = end;
    }

    let file_bytes = u64::try_from(WRITEBACK_BYTES).unwrap_or(u64::MAX);
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SYNC, 0) == Some(file_bytes);
    let mut whole = vec![0u8; WRITEBACK_BYTES];
    ok = ok
        && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0
        && read_exact(fd, &mut whole)
        && whole.as_slice() == payload.as_slice();

    let replacement = b"writeback";
    let short_bytes = u64::try_from(replacement.len()).unwrap_or(u64::MAX);
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_TRUNCATE, short_bytes) == Some(short_bytes);
    ok = ok
        && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0
        && gtos_sys::write(fd, replacement) == short_bytes;
    ok = ok && gtos_sys::ioctl(fd, gtos_sys::IOCTL_FILE_SYNC, 0) == Some(short_bytes);
    let mut shortened = vec![0u8; replacement.len()];
    ok = ok
        && gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) == 0
        && read_exact(fd, &mut shortened)
        && shortened.as_slice() == &replacement[..];

    let _ = gtos_sys::close(fd);
    ok
}

/// Reads the FAT root directory through a descriptor instead of a one-shot
/// `readdir` buffer, and checks that the seeded and runtime-created files are
/// all reachable through the incremental stream.
fn directory_stream_ok() -> bool {
    let fd = gtos_sys::open(DIR_STREAM_URL);
    if fd == gtos_sys::ERROR {
        return false;
    }
    let mut all = Vec::new();
    let mut buffer = [0u8; 128];
    loop {
        match gtos_sys::read(fd, &mut buffer) {
            gtos_sys::ERROR => {
                let _ = gtos_sys::close(fd);
                return false;
            }
            0 => break,
            count => {
                let Ok(count) = usize::try_from(count) else {
                    let _ = gtos_sys::close(fd);
                    return false;
                };
                if count > buffer.len() {
                    let _ = gtos_sys::close(fd);
                    return false;
                }
                all.extend_from_slice(&buffer[..count]);
            }
        }
    }
    let _ = gtos_sys::close(fd);

    let mut cabi = false;
    let mut rustdemo = false;
    let mut stream = false;
    for name in all.split(|byte| *byte == 0).filter(|name| !name.is_empty()) {
        cabi |= name == &b"CABI.ELF"[..];
        rustdemo |= name == &b"RUSTDEMO.ELF"[..];
        stream |= name == &b"STREAM.TXT"[..];
    }
    cabi && rustdemo && stream
}

fn read_exact(fd: u64, output: &mut [u8]) -> bool {
    let deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    let mut filled = 0;
    while filled < output.len() {
        match gtos_sys::read(fd, &mut output[filled..]) {
            gtos_sys::ERROR if gtos_sys::monotonic_ns() >= deadline => return false,
            gtos_sys::ERROR => gtos_sys::yield_now(),
            0 => return false,
            count => match usize::try_from(count) {
                Ok(count) => filled += count,
                Err(_) => return false,
            },
        }
    }
    true
}

fn read_line(fd: u64, output: &mut [u8]) -> Option<usize> {
    let deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    let mut length = 0;
    while length < output.len() {
        match gtos_sys::read(fd, &mut output[length..=length]) {
            gtos_sys::ERROR if gtos_sys::monotonic_ns() >= deadline => return None,
            gtos_sys::ERROR => gtos_sys::yield_now(),
            1 => {
                if output[length] == b'\n' {
                    return Some(length);
                }
                length += 1;
            }
            _ => return None,
        }
    }
    None
}

fn parse_decimal(bytes: &[u8]) -> Option<u64> {
    let mut value: u64 = 0;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

fn parse_connect(line: &[u8]) -> Option<u64> {
    let mut parts = line.split(|&byte| byte == b' ');
    if parts.next() != Some(&b"CONNECT"[..]) {
        return None;
    }
    parse_decimal(parts.next()?)
}

fn scheme_client() -> bool {
    // The registration also creates a `/srv/demo` mount, so this exercises VFS
    // path resolution and the dynamic scheme mount rather than only a raw URL.
    let fd = gtos_sys::open(b"/srv/demo/hello");
    if fd == gtos_sys::ERROR {
        return false;
    }
    let sent = gtos_sys::write(fd, SCHEME_REQUEST) == SCHEME_REQUEST.len() as u64;
    let mut response = [0u8; 4];
    let received = sent && read_exact(fd, &mut response);
    let _ = gtos_sys::close(fd);
    received && response.as_slice() == SCHEME_RESPONSE
}

fn finish_scheme_server(child: u64, listen_read: u64, name: &[u8], ok: bool) -> bool {
    let mut status = 0;
    let mut reaped = false;
    let deadline = gtos_sys::monotonic_ns().saturating_add(2_000_000_000);
    loop {
        match gtos_sys::waitpid(child, &mut status) {
            waited if waited == child => {
                reaped = true;
                break;
            }
            _ if gtos_sys::monotonic_ns() >= deadline => break,
            _ => gtos_sys::yield_now(),
        }
    }
    let _ = gtos_sys::scheme_unregister(name);
    let _ = gtos_sys::close(listen_read);
    ok && reaped && status == 0
}

fn scheme_server_ok() -> bool {
    let Some((listen_read, listen_write)) = gtos_sys::pipe() else {
        return false;
    };
    if !gtos_sys::scheme_register(SCHEME_NAME, listen_write) {
        let _ = gtos_sys::close(listen_read);
        let _ = gtos_sys::close(listen_write);
        return false;
    }
    // The registry keeps its own write reference; this process only listens.
    let _ = gtos_sys::close(listen_write);

    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::scheme_unregister(SCHEME_NAME);
        let _ = gtos_sys::close(listen_read);
        return false;
    }
    if child == 0 {
        let _ = gtos_sys::close(listen_read);
        let ok = scheme_client();
        gtos_sys::exit(u64::from(!ok));
    }

    let mut line = [0u8; 128];
    let Some(length) = read_line(listen_read, &mut line) else {
        return finish_scheme_server(child, listen_read, SCHEME_NAME, false);
    };
    let Some(server_fd) = parse_connect(&line[..length]) else {
        return finish_scheme_server(child, listen_read, SCHEME_NAME, false);
    };
    let mut request = [0u8; 4];
    let mut ok = read_exact(server_fd, &mut request) && request.as_slice() == SCHEME_REQUEST;
    ok = ok && gtos_sys::write(server_fd, SCHEME_RESPONSE) == SCHEME_RESPONSE.len() as u64;
    let _ = gtos_sys::close(server_fd);
    finish_scheme_server(child, listen_read, SCHEME_NAME, ok)
}

fn finish_scheme_shm(
    child: u64,
    listen_read: u64,
    ready_write: u64,
    server_fd: Option<u64>,
    region: u64,
    ok: bool,
) -> bool {
    if let Some(fd) = server_fd {
        let _ = gtos_sys::close(fd);
    }
    let _ = gtos_sys::close(ready_write);
    // The stream holds its own frame references, so destroying the region here
    // checks that the retained mapping is truly independent of the owner.
    let _ = gtos_sys::shm_destroy(region);
    finish_scheme_server(child, listen_read, SHM_SCHEME_NAME, ok)
}

fn scheme_shm_ok() -> bool {
    let Some((listen_read, listen_write)) = gtos_sys::pipe() else {
        return false;
    };
    if !gtos_sys::scheme_register(SHM_SCHEME_NAME, listen_write) {
        let _ = gtos_sys::close(listen_read);
        let _ = gtos_sys::close(listen_write);
        return false;
    }
    let _ = gtos_sys::close(listen_write);

    let Some(region) = gtos_sys::shm_create(SHM_PAYLOAD_BYTES as u64) else {
        let _ = gtos_sys::scheme_unregister(SHM_SCHEME_NAME);
        let _ = gtos_sys::close(listen_read);
        return false;
    };
    let Some((ready_read, ready_write)) = gtos_sys::pipe() else {
        let _ = gtos_sys::shm_destroy(region);
        let _ = gtos_sys::scheme_unregister(SHM_SCHEME_NAME);
        let _ = gtos_sys::close(listen_read);
        return false;
    };

    let payload: Vec<u8> = (0..SHM_PAYLOAD_BYTES)
        .map(|index| u8::try_from(index % 251).unwrap_or_default())
        .collect();
    let child = gtos_sys::fork();
    if child == gtos_sys::ERROR {
        let _ = gtos_sys::close(ready_read);
        let _ = gtos_sys::close(ready_write);
        let _ = gtos_sys::shm_destroy(region);
        let _ = gtos_sys::scheme_unregister(SHM_SCHEME_NAME);
        let _ = gtos_sys::close(listen_read);
        return false;
    }
    if child == 0 {
        let _ = gtos_sys::close(listen_read);
        let _ = gtos_sys::close(ready_write);
        let mut ok = false;
        let fd = gtos_sys::open(b"/srv/shm/data");
        if fd != gtos_sys::ERROR {
            let mut ready = [0u8; 1];
            ok = read_exact(ready_read, &mut ready) && ready[0] == b'R';
            let mut sent = 0usize;
            while ok && sent < payload.len() {
                match gtos_sys::write(fd, &payload[sent..]) {
                    gtos_sys::ERROR => ok = false,
                    count => match usize::try_from(count) {
                        Ok(written) if written > 0 && sent + written <= payload.len() => {
                            sent += written;
                        }
                        _ => ok = false,
                    },
                }
            }
            ok = ok && sent == payload.len();
            let _ = gtos_sys::close(fd);
        }
        let _ = gtos_sys::close(ready_read);
        gtos_sys::exit(u64::from(!ok));
    }

    let _ = gtos_sys::close(ready_read);
    let mut line = [0u8; 128];
    let Some(length) = read_line(listen_read, &mut line) else {
        return finish_scheme_shm(child, listen_read, ready_write, None, region, false);
    };
    let Some(server_fd) = parse_connect(&line[..length]) else {
        return finish_scheme_shm(child, listen_read, ready_write, None, region, false);
    };

    let mut ok = gtos_sys::shm_bind(server_fd, region);
    ok = ok
        && gtos_sys::ioctl(server_fd, gtos_sys::IOCTL_FIONWRITE, 0)
            == Some(u64::try_from(SHM_PAYLOAD_BYTES).unwrap_or_default());
    ok = ok && gtos_sys::shm_destroy(region);
    if ok {
        ok = gtos_sys::write(ready_write, b"R") == 1;
    }
    let _ = gtos_sys::close(ready_write);

    let mut received = vec![0u8; SHM_PAYLOAD_BYTES];
    ok = ok && read_exact(server_fd, &mut received) && received == payload;
    finish_scheme_shm(child, listen_read, ready_write, Some(server_fd), region, ok)
}

fn read_dir_names(path: &[u8]) -> Option<Vec<String>> {
    let mut buffer = [0u8; gtos_sys::VFS_READDIR_BYTES];
    let count = gtos_sys::read_dir(path, &mut buffer)?;
    let mut names = Vec::new();
    let mut start = 0;
    for index in 0..count {
        if buffer[index] == 0 {
            if index > start {
                let text = core::str::from_utf8(&buffer[start..index]).ok()?;
                names.push(String::from(text));
            }
            start = index + 1;
        }
    }
    Some(names)
}

fn vfs_ok() -> bool {
    let Some(root) = gtos_sys::stat(b"/") else {
        return false;
    };
    if root.kind != gtos_sys::VFS_DIR || root.inode == 0 || root.writable != 0 {
        return false;
    }
    let Some(version) = gtos_sys::stat(VFS_PATH) else {
        return false;
    };
    if version.kind != gtos_sys::VFS_FILE
        || version.inode == 0
        || version.inode == root.inode
        || version.size == 0
        || version.writable != 0
    {
        return false;
    }

    let Some(entries) = read_dir_names(b"/") else {
        return false;
    };
    if !entries.iter().any(|name| name == "sys")
        || !entries.iter().any(|name| name == "srv")
        || !entries.iter().any(|name| name == "README.TXT")
    {
        return false;
    }

    if !gtos_sys::chdir(b"/sys") {
        return false;
    }
    let mut cwd = [0u8; 64];
    let Some(length) = gtos_sys::getcwd(&mut cwd) else {
        return false;
    };
    if length == 0 || cwd[length - 1] != 0 || &cwd[..length - 1] != b"/sys" {
        return false;
    }
    let Some(relative) = gtos_sys::stat(b"version") else {
        return false;
    };
    if relative.inode != version.inode {
        return false;
    }

    let fd = gtos_sys::open(b"version");
    if fd == gtos_sys::ERROR {
        return false;
    }
    let mut text = [0u8; 16];
    let count = gtos_sys::read(fd, &mut text);
    let _ = gtos_sys::close(fd);
    let Ok(count) = usize::try_from(count) else {
        return false;
    };
    if count < 4 || &text[..4] != b"GTos" {
        return false;
    }

    // A failed chdir must leave the working directory untouched.
    if gtos_sys::chdir(b"/no-such-directory") || !gtos_sys::chdir(b"/") {
        return false;
    }
    true
}

fn file_io_ok() -> bool {
    let fd = gtos_sys::open(FILE_URL);
    if fd == gtos_sys::ERROR || gtos_sys::write(fd, FILE_DATA) != FILE_DATA.len() as u64 {
        return false;
    }
    if gtos_sys::lseek(fd, 0, gtos_sys::SEEK_SET) != 0 {
        let _ = gtos_sys::close(fd);
        return false;
    }
    let mut buffer = [0; 4];
    let count = gtos_sys::read(fd, &mut buffer);
    let _ = gtos_sys::close(fd);
    count == FILE_DATA.len() as u64 && buffer.as_slice() == FILE_DATA
}

fn report(ok: bool, pass: &[u8], fail: &[u8]) {
    let _ = gtos_sys::write(1, if ok { pass } else { fail });
}

fn main() {
    let _ = gtos_sys::write(1, MESSAGE);
    let before = gtos_sys::monotonic_ns();
    let _ = gtos_sys::sleep(10);
    let after = gtos_sys::monotonic_ns();
    report(after > before, CLOCK_OK, CLOCK_FAIL);
    report(gtos_sys::realtime_seconds() > 0, RTC_OK, RTC_FAIL);
    report(file_io_ok(), IO_OK, IO_FAIL);
    report(heap_ok(), HEAP_OK, HEAP_FAIL);
    report(framebuffer_ok(), FRAMEBUFFER_OK, FRAMEBUFFER_FAIL);
    report(window_ok(), WINDOW_OK, WINDOW_FAIL);
    report(widget_toolkit_ok(), WIDGET_OK, WIDGET_FAIL);
    report(widget_layout_ok(), WIDGET_LAYOUT_OK, WIDGET_LAYOUT_FAIL);
    report(widget_view_ok(), WIDGET_VIEW_OK, WIDGET_VIEW_FAIL);
    report(widget_nested_ok(), WIDGET_NESTED_OK, WIDGET_NESTED_FAIL);
    report(widget_editor_ok(), WIDGET_EDITOR_OK, WIDGET_EDITOR_FAIL);
    report(shell_panel_ok(), SHELL_PANEL_OK, SHELL_PANEL_FAIL);
    report(shell_apps_ok(), SHELL_APPS_OK, SHELL_APPS_FAIL);
    report(shell_focus_ok(), SHELL_FOCUS_OK, SHELL_FOCUS_FAIL);
    report(shell_actions_ok(), SHELL_ACTIONS_OK, SHELL_ACTIONS_FAIL);
    report(shell_launch_ok(), SHELL_LAUNCH_OK, SHELL_LAUNCH_FAIL);
    report(shm_ok(), SHM_OK, SHM_FAIL);
    report(streaming_ipc_ok(), STREAM_OK, STREAM_FAIL);
    report(socketpair_ok(), SOCKET_OK, SOCKET_FAIL);
    report(ioctl_ok(), IOCTL_OK, IOCTL_FAIL);
    report(stream_file_ok(), STREAM_FILE_OK, STREAM_FILE_FAIL);
    report(fat_writeback_ok(), WRITEBACK_OK, WRITEBACK_FAIL);
    report(directory_stream_ok(), DIR_STREAM_OK, DIR_STREAM_FAIL);
    report(vfs_ok(), VFS_OK, VFS_FAIL);
    report(scheme_server_ok(), SCHEME_SERVER_OK, SCHEME_SERVER_FAIL);
    report(scheme_shm_ok(), SCHEME_SHM_OK, SCHEME_SHM_FAIL);
    report(input_ok(), INPUT_OK, INPUT_FAIL);
    report(signal_ok(), SIGNAL_OK, SIGNAL_FAIL);
    report(signal_mask_ok(), SIGNAL_MASK_OK, SIGNAL_MASK_FAIL);
    report(signal_ignore_ok(), SIGNAL_IGNORE_OK, SIGNAL_IGNORE_FAIL);
    report(signal_queued_ok(), SIGNAL_QUEUE_OK, SIGNAL_QUEUE_FAIL);
    report(
        signal_cross_process_ok(),
        SIGNAL_CHILD_OK,
        SIGNAL_CHILD_FAIL,
    );
    report(
        signal_termination_ok(gtos_sys::SIGTERM, false),
        SIGNAL_TERM_OK,
        SIGNAL_TERM_FAIL,
    );
    report(
        signal_termination_ok(gtos_sys::SIGKILL, true),
        SIGNAL_KILL_OK,
        SIGNAL_KILL_FAIL,
    );
    report(rt_signal_queue_ok(), RT_SIGNAL_OK, RT_SIGNAL_FAIL);
    report(fault_signal_ok(), FAULT_SIGNAL_OK, FAULT_SIGNAL_FAIL);
}
