# GTos vga-text — `blog_os` post-03, done properly

This is a tiny, standalone crate that starts from
[`blog_os` post-03](https://github.com/phil-opp/blog_os/tree/post-03) — the
kernel that grows a `vga_buffer` module (a global writer, colours, scrolling and
`print!`/`println!`) and prints `"Hello World!"` — and rebuilds it as something
you would actually ship. It is deliberately *not* part of the main GTos
workspace, so it can be built and tested on its own.

It is the sibling of [`examples/minimal`](../minimal/README.md): that crate is
post-02 (the raw `*mut u8` write), this one is post-03 (the writer). Their other
sibling [`examples/testing`](../testing/README.md) is
[`blog_os` post-04](https://github.com/phil-opp/blog_os/tree/post-04), the
*Testing* post. This crate also carries a small in-QEMU integration test — the
post-04 idea, applied to post-03.

## What post-03 does

```rust
lazy_static! {
    pub static ref WRITER: Mutex<Writer> = Mutex::new(Writer {
        column_position: 0,
        color_code: ColorCode::new(Color::Yellow, Color::Black),
        buffer: unsafe { &mut *(0xb8000 as *mut Buffer) },
    });
}
```

The `Buffer` is a `[[Volatile<ScreenChar>; 80]; 25]` reached by dereferencing the
raw pointer `0xb8000`; `Writer` writes to the *bottom* row and scrolls on every
newline; there is no hardware cursor, no `\r`/`\t`/backspace handling, no way to
read a cell back, and no tests.

## What this crate does instead

| post-03 | here |
| --- | --- |
| `lazy_static` dereferences `0xb8000` on first use | `vga_buffer::init(physical_memory_offset)` reaches `0xb8000` through the bootloader's window and only through the [`Volatile`](src/volatile.rs) wrapper; until then the global is `None` |
| `Buffer` is the raw memory layout | [`Buffer`](src/vga_buffer.rs) is a trait the writer drives; the device and a RAM-backed test buffer both implement it |
| writes to the bottom row and scrolls on every newline | the writer tracks `(row, column)`, wraps at column 80, scrolls only when the last row is passed |
| no cursor | the writer programs the hardware cursor through the CRTC (`0x3D4`/`0x3D5`) |
| no `\r`, `\t` or backspace | carriage return, tab stops every 8 columns, and `0x08` backspace |
| non-printable bytes become `0xfe` | kept: `0xfe` (the CP437 "filled box") for any byte with no printable glyph |
| one hard-coded attribute (`Yellow` on `Black`) | all sixteen [`Color`](src/vga_buffer.rs)s, `set_fg`/`set_bg`/`set_color`, and `Color::from_rgb` to snap a 24-bit colour to the palette |
| no tests | 15 unit tests run on the host with `cargo test`, against a RAM-backed buffer, plus an in-QEMU integration test that boots the real kernel (`tests/qemu.rs`) |
| output invisible to tooling | every write is mirrored to COM1, so `-serial stdio` shows it |
| `panic` spins forever | a white-on-red `*** PANIC ***` banner with the panic message |

The public names are post-03's — `vga_buffer`, `Color`, `ColorCode`,
`ScreenChar`, `Buffer`, `Writer`, `WRITER`, `print!`/`println!`/`_print` — so the
diff against the tutorial reads as "the same code, hardened".

## Layout

```
Cargo.toml          standalone package (its own workspace)
.cargo/config.toml  cargo `runner` (boot the in-QEMU test in QEMU) + the `qtest` alias
src/lib.rs          modules, print!/println! macros, kernel_main, panic handler
src/main.rs         the bootable binary: bootloader config + entry_point!
src/vga_buffer.rs   the safe 80x25 text writer (the heart of the crate)
src/volatile.rs     Volatile<T>, the one place raw device pointers are touched
src/port.rs         x86 port I/O: the CRTC cursor, and the QEMU exit port
src/serial.rs       a minimal COM1 UART driver
tests/qemu.rs       in-QEMU integration test (feature `qemu-test`)
```

## Test, build and run

The library builds `no_std` for the kernel and as a normal `std` crate under
`cfg(test)`, so the unit tests run on the **host** — no QEMU, no hardware:

```powershell
cargo test --manifest-path examples\vga-text\Cargo.toml --lib
```

There is also an in-QEMU integration test (`tests/qemu.rs`) that boots the real
kernel, reaches physical memory and drives the device path. It is gated behind
the `qemu-test` feature and its own `[[test]]` target, so a plain host `cargo
test` (and `cargo clippy --all-targets`) never tries to build a bare-metal test
binary. Run it from the crate directory — Cargo reads `.cargo/config.toml` from
the current directory upwards, and it is that file which wires up the runner:

```powershell
cd examples\vga-text
cargo qtest        # == cargo test --target x86_64-unknown-none --features qemu-test --test qemu
```

or, from the repository root:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\vga-text.ps1 -Test
```

Build the bootable kernel and boot it (BIOS, via the shared `runner`):

```powershell
powershell -ExecutionPolicy Bypass -File scripts\vga-text.ps1
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
