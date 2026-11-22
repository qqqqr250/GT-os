# Console shell (no desktop)

GTos has **no graphical desktop environment**. There is no window manager, no
launcher, no mouse pointer and no GUI applications. The kernel boots straight
into the text shell, which renders on whichever console the firmware provides:

- a **linear framebuffer** (the normal VirtualBox path) is used as an 8x8
  pixel-font text console, with all sixteen colours and a scrollback grid;
- without a framebuffer the kernel falls back to the legacy **VGA text mode**;
- if neither is available, output is **serial-only** (COM1).

`kernel/src/console.rs` implements the framebuffer text renderer, the VGA text
fallback and the `print!`/`println!` macros. `kernel/src/font.rs` holds the 8x8
glyph bitmap, and `kernel/src/logo.rs` draws the boot logo strip above the first
lines of the boot log. Everything written to the console is also mirrored to
COM1, so a serial connection can drive the shell even when no video output is
available.

## Using the shell

The shell is a line-oriented command interpreter. At the `GTos>` prompt:

- type a command and press **Enter** to run it;
- **Up**/**Down** recall command history;
- **Tab** completes command names and, for `cat`, `open`, `play`, `rm` and `run`,
  FAT16 file names;
- type `help` to list every available command.

Commands include `help`, `echo`, `clear`, `mem`, `heap`, `alloc`, `uptime`,
`cpu`, `logo`, `about`, `net`, `dhcp`, `ping`, `dns`, `http`, `beep`, `tone`,
`disks`, `ls`, `cat`, `write`, `rm`, `play`, `format`, `run`, `pkg`, `panic`,
`reboot` and `shutdown`. See [Getting started](02-getting-started.md) for the
full table.

> **VirtualBox note:** click once inside the VM window so the guest captures
> the keyboard. Press the **Host key** (right Ctrl by default) to release it.
> The shell does not use the mouse, but the kernel driver feeds pointer events to
> ring 3 through the input device; pointer capture affects both devices.
