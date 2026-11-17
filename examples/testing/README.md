# RustOS testing — `blog_os` post-04, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-04](https://github.com/phil-opp/blog_os/tree/post-04) — the
*Testing* post, which builds a minimal bare-metal kernel and a custom test
framework that runs it inside QEMU — and rebuilds it as something you would
actually ship. It is deliberately *not* part of the main RustOS workspace, so it
can be built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md) (post-02) and
[`examples/vga-text`](../vga-text/README.md) (post-03). Those two keep their unit
tests on the host; this one is the post that makes `cargo test` mean "boot the
kernel in QEMU and run the tests there".

## What post-04 does

A bare-metal binary cannot run under the host's test harness, so post-04 builds
its own:

```rust
#![feature(custom_test_frameworks)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]

pub trait Testable {
    fn run(&self);
}

impl<T> Testable for T where T: Fn() {
    fn run(&self) {
        serial_print!("{}...\t", core::any::type_name::<T>());
        self();
        serial_println!("[ok]");
    }
}

pub fn test_runner(tests: &[&dyn Testable]) {
    serial_println!("Running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}

pub fn exit_qemu(exit_code: QemuExitCode) {
    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }
}
```

`#[test_case]` functions are collected into a `test_runner`, the kernel reports
pass/fail on COM1, and QEMU is powered off through the `isa-debug-exit` device
(`0xF4`); the `bootimage` runner maps QEMU's exit code back onto `cargo test`. The
`should_panic` integration test proves the harness notices a panic.

## What this crate does instead

The framework is the same — `Testable`, `test_runner`, `test_main`,
`#[test_case]`, `serial_print!`/`serial_println!`, `QemuExitCode`, `exit_qemu` —
but rebuilt around this repository's conventions:

| post-04 | here |
| --- | --- |
| `bootimage` injects the `.bootloader-config` section | every test binary boots through [`entry_point!`](https://docs.rs/bootloader_api), so its ELF carries the section itself |
| `exit_qemu` writes port `0xf4` inline | it goes through the [`port`](src/port.rs) abstraction, and a unit test asserts the `(value << 1) \| 1` encoding |
| the `serial_print!` macros reach into a public `serial` module | the macros call one `#[doc(hidden)]` path; the UART driver stays private |
| a bare `_start` calls `test_main` | the test entry point is `entry_point!`-generated, matching the real binary |
| `spin` and a `lazy_static` UART | a plain 16550 driver with no global lock — the tests are single-threaded |
| one trivial unit test | three `#[test_case]` unit tests, a `basic_boot` integration test and a `should_panic` test |
| no runner of its own | the shared [`runner/`](../../runner/src/main.rs) adds a timeout and maps the exit code |

The public names are post-04's, so the diff against the tutorial reads as "the
same code, wired into this repository".

## Layout

```
Cargo.toml            standalone package (its own workspace)
.cargo/config.toml    cargo `runner` (boot the test ELF in QEMU) + the `qtest` alias
src/lib.rs            serial_print! macros, Testable/test_runner, exit_qemu, kernel_main
src/main.rs           the bootable binary: bootloader config + entry_point!
src/serial.rs         a minimal COM1 UART driver (the test console)
src/port.rs           x86 port I/O, used by the UART and exit_qemu
tests/basic_boot.rs   in-QEMU integration test (serial + physical-memory window)
tests/should_panic.rs in-QEMU test that the harness notices a panic
```

## Test, build and run

The crate is `no_std` even under `cfg(test)`, so `cargo test` links bare-metal
test binaries and hands each one to the shared `runner`, which wraps it in a
bootable BIOS disk image, boots it in QEMU with `-device isa-debug-exit` and COM1
on stdout, and maps QEMU's exit code back onto `cargo test`. Because the runner is
configured by `.cargo/config.toml`, the tests must be run **from this directory**:

```powershell
cd examples\testing
cargo qtest        # == cargo test --target x86_64-unknown-none
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\testing.ps1 -Test
```

Three test binaries run — the library's unit tests, `tests/basic_boot.rs` and
`tests/should_panic.rs` — and QEMU reports success as exit code 33 (the framework
writes `0x10`, which QEMU reports as `(0x10 << 1) | 1`).

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\testing.ps1
```

Pass `-NoBoot` to only build the image, `-Release` for an optimised build, or
extra QEMU arguments after `--`.

> **Why there is no VGA writer here.** Post-04's kernel prints to the VGA text
> buffer; this crate's console is COM1, because that is the only console a
> headless `cargo test` can read. The point of post-04 is the test framework, not
> the display — the display is [`examples/vga-text`](../vga-text/README.md).
