# GTos minimal — `blog_os` post-02, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-02](https://github.com/phil-opp/blog_os/tree/post-02) — the
kernel that writes `"Hello World!"` to the VGA text buffer at `0xb8000` — and
rebuilds it as something you would actually ship. It is deliberately *not* part
of the main GTos workspace, so it can be built and tested on its own.

> Its siblings are [`examples/vga-text`](../vga-text/README.md), the same treatment
> for [`blog_os` post-03](https://github.com/phil-opp/blog_os/tree/post-03) (the
> post that introduces the `vga_buffer` writer), and
> [`examples/testing`](../testing/README.md), which is
> [`blog_os` post-04](https://github.com/phil-opp/blog_os/tree/post-04) (the
> *Testing* post). The writer here is already the hardened one post-03 builds
> towards; `vga-text` keeps post-03's public names so the two read side by side.
> This crate also carries a small in-QEMU integration test — the post-04 idea,
> applied to post-02.

## What post-02 does

```rust
let vga_buffer = 0xb8000 as *mut u8;
for (i, &byte) in HELLO.iter().enumerate() {
    unsafe {
        *vga_buffer.offset(i as isize * 2) = byte;
        *vga_buffer.offset(i as isize * 2 + 1) = 0xb;
    }
}
loop {}
```

A raw pointer, an unchecked index, a hard-coded colour, no scrolling, no cursor,
no tests, and a panic handler that spins forever in silence.

## What this crate does instead

| post-02 | here |
| --- | --- |
| raw `*mut u8` into device memory | `vga::init(physical_memory_offset)` reaches `0xb8000` through the bootloader's window and only through the [`Volatile`](src/volatile.rs) wrapper |
| unchecked `offset(i * 2)` | the [`Writer`](src/vga.rs) bounds every cell to the 80x25 grid |
| one hard-coded attribute (`0xb`) | all sixteen [`Color`](src/vga.rs)s, `set_fg`/`set_bg`/`set_color`, and `Color::from_rgb` to snap a 24-bit colour to the palette |
| no cursor | the writer tracks the cursor and programs the hardware cursor through the CRTC (`0x3D4`/`0x3D5`) |
| no scrolling | newline at the bottom scrolls the grid |
| no tests | 14 unit tests run on the host with `cargo test`, against a RAM-backed buffer, plus an in-QEMU integration test that boots the real kernel (`tests/qemu.rs`) |
| `panic` spins forever | a red-on-white `*** PANIC ***` banner with the panic message |
| output invisible to tooling | every write is mirrored to COM1, so `-serial stdio` shows it |

## Layout

```
Cargo.toml          standalone package (its own workspace)
.cargo/config.toml  cargo `runner` (boot the in-QEMU test in QEMU) + the `qtest` alias
src/lib.rs          modules, print!/println! macros, kernel_main, panic handler
src/main.rs         the bootable binary: bootloader config + entry_point!
src/vga.rs          the safe 80x25 text writer (the heart of the crate)
src/volatile.rs     Volatile<T>, the one place raw device pointers are touched
src/port.rs         x86 port I/O: the CRTC cursor, and the QEMU exit port
src/serial.rs       a minimal COM1 UART driver
src/console.rs      COM1 + VGA console, colours, banner
tests/qemu.rs       in-QEMU integration test (feature `qemu-test`)
```

## Test, build and run

The library builds `no_std` for the kernel and as a normal `std` crate under
`cfg(test)`, so the unit tests run on the **host** — no QEMU, no hardware:

```powershell
cargo test --manifest-path examples\minimal\Cargo.toml --lib
```

There is also an in-QEMU integration test (`tests/qemu.rs`) that boots the real
kernel, reaches physical memory and drives the device path. It is gated behind
the `qemu-test` feature and its own `[[test]]` target, so a plain host `cargo
test` (and `cargo clippy --all-targets`) never tries to build a bare-metal test
binary. Run it from the crate directory — Cargo reads `.cargo/config.toml` from
the current directory upwards, and it is that file which wires up the runner:

```powershell
cd examples\minimal
cargo qtest        # == cargo test --target x86_64-unknown-none --features qemu-test --test qemu
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1 -Test
```

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\minimal.ps1
```

The script builds the ELF for `x86_64-unknown-none`, wraps it in a bootable BIOS
disk image with the same `runner` crate the in-QEMU tests use, and starts QEMU
with COM1 on the terminal. Pass `-NoBoot` to only build the image, or extra QEMU
arguments after `--`.

> **Why the screen may look blank.** The `bootloader` crate always asks firmware
> for a linear framebuffer, so under QEMU the display ends up in graphics mode and
> the legacy text aperture at `0xb8000` is not backed — writing to it changes
> nothing on screen. The kernel notices this and says so, and COM1 carries the
> real output. On a machine whose firmware leaves the console in text mode the
> same code draws the banner on screen. This is exactly the situation GTos's own
> [`vga`](../../kernel/src/vga.rs) fallback handles.
