# Desktop environment

When firmware reports a linear framebuffer the kernel boots straight into a
Windows XP-style desktop instead of the text shell (a serial-only boot still
falls back to the shell). Any byte arriving on COM1 hands the console back to
the shell, so a headless test can still drive the shell even though the desktop
owns the screen. `kernel\src\wm.rs` draws everything with `put_pixel` on the
framebuffer. The whole scene is composed in an off-screen back buffer and copied
to the framebuffer in one pass, so a click never exposes a half-drawn frame;
moving the pointer just restores the 12x19 pixels it left behind (kept in a small
backing store) and redraws it at the new spot, so the cursor glides instead of
flickering through a full-screen repaint:

- a vertical blue gradient wallpaper,
- a taskbar along the bottom with a green **start** button that opens a Start
  menu; the Start-menu entries open real windows (My Computer, Control Panel,
  Command Prompt, Run..., About RustOS, Log Off, Shut Down), and the taskbar
  window buttons raise their window when clicked,
- draggable windows with Luna-blue title bars, a close button (which frees the
  slot so a window can be reopened), and a drop shadow; the desktop starts empty
  at boot and windows open on demand from the Start menu or the desktop icons,
- a classic 12x19 arrow pointer whose hotspot is the top-left pixel.

Input comes from `kernel\src\mouse.rs`, a PS/2 driver for the 8042 auxiliary
port. It configures the controller with the mouse interrupt (IRQ12) **masked**
during the command handshake and enables it only once the mouse is streaming, so
the ACK bytes cannot be stolen by the interrupt handler. `handle_byte`
resynchronises on the first packet byte (bit 3 must be set), which discards stray
bytes and keeps the pointer from drifting.

> **QEMU: click inside the window first.** QEMU does not forward mouse motion
> until its pointer is *grabbed*, which happens when you click in the guest
> window (press **Ctrl+Alt+G** to release it again). Until then the cursor will
> not move, even though the driver is working.
