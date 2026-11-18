# Desktop environment

When firmware reports a linear framebuffer the kernel boots straight into a
Windows XP-style desktop instead of the text shell (a serial-only boot still
falls back to the shell). Any byte arriving on COM1 hands the console back to
the shell, so a serial connection can drive it even though the desktop owns the
screen. `kernel\src\wm.rs` draws everything with `put_pixel` on the
framebuffer. The whole scene is composed in an off-screen back buffer and copied
to the framebuffer in one pass, so a click never exposes a half-drawn frame;
moving the pointer just restores the 12x19 pixels it left behind (kept in a small
backing store) and redraws it at the new spot, so the cursor glides instead of
flickering through a full-screen repaint:

- a vertical blue gradient wallpaper,
- a taskbar along the bottom with a green **start** button that opens a Start
  menu; the Start-menu entries open real windows (My Computer, Settings,
  Command Prompt, Run..., About GTos, Log Off, Shut Down), and the taskbar
  window buttons raise their window when clicked,
- draggable windows with Luna-blue title bars, a close button (which frees the
  slot so a window can be reopened), and a drop shadow; the desktop starts empty
  at boot and windows open on demand from the Start menu, the desktop icons or
  **F1** (Browser) or **F2** (file manager), while the Windows key toggles the
  Start menu,
- a Browser with clickable links (**Tab** cycles through them, **Enter**
  follows the focused one) and address-bar web search,
- a Command Prompt that runs real shell commands and shares the shell's
  Ion-style history (**Up**/**Down**) and **Tab** completion,
- a classic 12x19 arrow pointer whose hotspot is the top-left pixel.

## File manager

**My Computer** (Start menu, desktop icon or **F2**) is a real file manager, not a
static panel. It reads the mounted FAT16 data disk through `kernel\src\fat.rs`
and shows the root directory with folder/file icons, a Quick Access sidebar,
an address bar, a search field and a status bar. **My Documents** opens the
same explorer.

- Click **Name**, **Size** or **Type** (toolbar button or column header) to
  sort; click again to reverse the direction. Folders stay above files.
- Type in the search field to filter instantly (case-insensitive substring,
  with `*` as a wildcard, e.g. `*.WAV`). **Backspace** deletes, **Esc** clears.
- Use the arrow keys, Page Up/Down, Home and End to move the selection;
  **Enter** opens it. To open with the mouse, click a row once to select it and
  click the selected row again (**double-click**). Opening does not depend on a
  timing window, so a slow or deliberate second click still works.
- Text files open in a read-only Text Viewer with arrow/Page Up/Down scrolling.
  Folders and binaries report an honest status message: this FAT16 driver only
  handles the root directory, so there are no subfolders to descend into.
- **Refresh** re-reads the disk, so files written by the shell (or by the
  Command Prompt window) appear without restarting.

Inspired by [Sigma File Manager](https://github.com/aleksey-hoffman/sigma-file-manager),
this is a deliberately small clean-room version: no Tauri, Vue, tabs, split
view, extensions or network sharing — just a compact GUI over the existing
FAT16 layer. The filtering/sorting/formatting logic lives in `kernel\src\fm.rs`
so it can be unit-tested without a framebuffer.

Input comes from `kernel\src\mouse.rs`, a PS/2 driver for the 8042 auxiliary
port. It configures the controller with the mouse interrupt (IRQ12) **masked**
during the command handshake and enables it only once the mouse is streaming, so
the ACK bytes cannot be stolen by the interrupt handler. `handle_byte`
resynchronises on the first packet byte (bit 3 must be set), which discards stray
bytes and keeps the pointer from drifting.

> **VirtualBox: click inside the VM window first.** VirtualBox only forwards
> mouse motion once the guest has captured the pointer, which happens when you
> click in the VM window (press the **Host key**, right Ctrl by default, to
> release it again). Until then the cursor will not move, even though the driver
> is working.
