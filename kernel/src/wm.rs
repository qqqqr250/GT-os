//! Windows XP-style desktop environment: desktop background, taskbar with a
//! green Start button, draggable windows with blue title bars, a mouse cursor,
//! and a Start menu.
//!
//! The scene is composed in an off-screen back buffer and copied to the
//! framebuffer in a single pass, so a click never exposes a half-drawn frame
//! (no flicker). The pointer alone is then composited on top and only its
//! bounding box is copied across, which keeps mouse movement cheap.

use alloc::vec::Vec;

use crate::console::{self, Color};
use bootloader_api::info::{BootInfo, FrameBufferInfo};

use crate::browser;
use crate::input;
use crate::mouse;

/// Line spacing used for the Browser window's rendered page text.
const BROWSER_LINE_H: usize = 11;
/// Default address the Browser opens with.
const BROWSER_HOME: &str = "http://example.com/";

// ---------------------------------------------------------------------------
// Colour palette (XP Luna blue theme)
// ---------------------------------------------------------------------------

// (desktop wallpaper colors are inlined in Desktop::new)
const COL_TASKBAR: Color = Color(0x24_5E_DC);
// XP Luna Start button: orange gradient.
const COL_START_ORANGE_DARK: Color = Color(0xD0_60_00);
const COL_START_ORANGE_LIGHT: Color = Color(0xF5_95_20);
const COL_TITLE_ACTIVE: Color = Color(0x0A_24_6A);
const COL_TITLE_ACTIVE2: Color = Color(0xA6_CA_F0);
const COL_TITLE_INACTIVE: Color = Color(0x7A_96_DF);
const COL_TITLE_INACTIVE2: Color = Color(0xD0_D8_F0);
const COL_WINDOW_BG: Color = Color(0xFF_FF_FF);
const COL_CLOSE_RED: Color = Color(0xE8_11_23);
const COL_BORDER: Color = Color(0x00_3C_74);
const COL_TEXT: Color = Color(0x00_00_00);
const COL_TEXT_WHITE: Color = Color(0xFF_FF_FF);
const COL_MENU_BG: Color = Color(0xF0_F0_F0);
const COL_MENU_TEXT: Color = Color(0x00_00_00);
const COL_MENU_HILITE: Color = Color(0x31_6A_C6);
const COL_SHADOW: Color = Color(0x40_40_40);
const COL_CURSOR_EDGE: Color = Color(0x00_00_00);
const COL_CURSOR_FILL: Color = Color(0xFF_FF_FF);

// ---------------------------------------------------------------------------
// Screen metrics
// ---------------------------------------------------------------------------

const TASKBAR_HEIGHT: usize = 30;
const TITLE_BAR_HEIGHT: usize = 22;
const BORDER_WIDTH: usize = 3;
const CLOSE_BTN_SIZE: usize = 16;
const START_BTN_WIDTH: usize = 90;
const TASKBAR_BTN_WIDTH: usize = 140;
const TASKBAR_BTN_GAP: usize = 8;

/// Start-menu geometry. `MENU_HEIGHT` fits the header plus every item exactly,
/// so the menu never shows a blank strip at the bottom.
const MENU_WIDTH: usize = 200;
const MENU_HEADER: usize = 44;
const MENU_ITEM_HEIGHT: usize = 24;
const MENU_ITEMS: [&str; 7] = [
    "My Computer",
    "Control Panel",
    "Command Prompt",
    "Run...",
    "About RustOS",
    "Log Off",
    "Shut Down",
];
const MENU_HEIGHT: usize = MENU_HEADER + MENU_ITEMS.len() * MENU_ITEM_HEIGHT + 8;

/// The pointer's bounding box, in pixels (matches `CURSOR_SHAPE`).
const CURSOR_W: usize = 12;
const CURSOR_H: usize = 19;

/// The classic arrow pointer: `X` is the black outline, `#` the white fill and
/// `.` a transparent pixel. The hotspot (the arrow tip) is the top-left corner.
const CURSOR_SHAPE: [&str; 19] = [
    "X...........",
    "XX..........",
    "X#X.........",
    "X##X........",
    "X###X.......",
    "X####X......",
    "X#####X.....",
    "X######X....",
    "X#######X...",
    "X########X..",
    "X####XXXXX..",
    "X##X##X.....",
    "X#X.X##X....",
    "XX..X##X....",
    "X....X##X...",
    ".....X##X...",
    "......X##X..",
    "......X##X..",
    ".......XX...",
];

// ---------------------------------------------------------------------------
// Window state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

#[derive(Clone, Copy)]
pub struct Window {
    pub rect: Rect,
    pub title: &'static str,
    pub active: bool,
    pub dragging: bool,
    pub drag_offset_x: isize,
    pub drag_offset_y: isize,
    pub content: WindowContent,
}

/// What a window shows. Each Start-menu entry maps to one of these.
#[derive(Clone, Copy, PartialEq)]
pub enum WindowContent {
    Terminal,
    About,
    MyComputer,
    MyDocuments,
    Browser,
    ControlPanel,
    CommandPrompt,
    Run,
    LogOff,
}

impl WindowContent {
    /// Preferred size for a freshly opened window of this kind.
    const fn default_size(self) -> (usize, usize) {
        match self {
            WindowContent::Terminal => (480, 320),
            WindowContent::About => (400, 280),
            WindowContent::MyComputer => (440, 300),
            WindowContent::MyDocuments => (460, 320),
            WindowContent::Browser => (520, 360),
            WindowContent::ControlPanel => (420, 320),
            WindowContent::CommandPrompt => (480, 300),
            WindowContent::Run => (380, 190),
            WindowContent::LogOff => (320, 170),
        }
    }
}

/// The desktop's independent UI state flags (`start_menu_open`, `mouse_down`,
/// `mouse_was_down`, `cursor_saved`) are genuinely separate booleans, not a
/// state enum, so the pedantic bool-count lint does not apply.
#[allow(clippy::struct_excessive_bools)]
pub struct Desktop {
    pub width: usize,
    pub height: usize,
    /// Live windows, back (index 0) to front. A closed window frees its slot.
    pub windows: [Option<Window>; 8],
    pub active_window: Option<usize>,
    pub start_menu_open: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub mouse_down: bool,
    pub mouse_was_down: bool,
    pub dragging_window: Option<usize>,
    /// Cascade offset counter for newly opened windows.
    spawn_count: usize,
    /// Which Start-menu entry the pointer is over, if any.
    menu_hover: Option<usize>,
    /// Index of the selected Control Panel icon (if any).
    cp_selected: Option<usize>,
    /// When true, window content (text) is skipped during rendering to keep
    /// dragging smooth.
    skip_content: bool,
    /// Accumulated text shown in the Command Prompt window (input + output).
    cp_output: alloc::string::String,
    /// Current input line.
    cp_input: [u8; 128],
    cp_input_len: usize,
    /// Browser window: the editable address, the last rendered page (one
    /// `String` per display line), the scroll offset into it and a status line.
    browser_url: [u8; 160],
    browser_url_len: usize,
    browser_lines: Vec<alloc::string::String>,
    browser_scroll: usize,
    browser_status: alloc::string::String,
    /// Whether typing edits the address bar (as opposed to scrolling the page).
    browser_editing: bool,
    /// The pixels underneath the pointer, captured from the back buffer when it
    /// is drawn so it can be erased without repainting the whole scene. `bpp`
    /// bytes per pixel, at most 4.
    cursor_backup: [u8; CURSOR_W * CURSOR_H * 4],
    cursor_saved: bool,
    cursor_saved_x: usize,
    cursor_saved_y: usize,
    /// Off-screen frame the scene is composed in before being copied across.
    back: Vec<u8>,
    /// Cached desktop wallpaper (the vertical gradient), rendered once so a
    /// full repaint does not recompute ~900k gradient pixels every frame.
    wallpaper: Vec<u8>,
    /// Cached taskbar, re-rendered only when windows change.
    taskbar_cache: Vec<u8>,
    taskbar_dirty: bool,
    /// Boot info, needed to run shell commands from the desktop.
    boot_info: &'static BootInfo,
    fb_ptr: *mut u8,
    fb_len: usize,
    stride: usize,
    bpp: usize,
}

impl Desktop {
    pub fn new(
        width: usize,
        height: usize,
        fb: &'static mut [u8],
        info: &FrameBufferInfo,
        boot_info: &'static BootInfo,
    ) -> Self {
        let fb_len = fb.len();
        let mut d = Desktop {
            width,
            height,
            windows: [None; 8],
            active_window: None,
            start_menu_open: false,
            cursor_x: width as i32 / 2,
            cursor_y: height as i32 / 2,
            mouse_down: false,
            mouse_was_down: false,
            dragging_window: None,
            spawn_count: 0,
            menu_hover: None,
            cp_selected: None,
            skip_content: false,
            cp_output: alloc::string::String::new(),
            cp_input: [0; 128],
            cp_input_len: 0,
            browser_url: [0; 160],
            browser_url_len: 0,
            browser_lines: Vec::new(),
            browser_scroll: 0,
            browser_status: alloc::string::String::new(),
            browser_editing: true,
            cursor_backup: [0; CURSOR_W * CURSOR_H * 4],
            cursor_saved: false,
            cursor_saved_x: 0,
            cursor_saved_y: 0,
            back: alloc::vec![0u8; fb_len],
            wallpaper: alloc::vec![0u8; fb_len],
            taskbar_cache: alloc::vec![0u8; fb_len],
            taskbar_dirty: true,
            boot_info,
            fb_ptr: fb.as_mut_ptr(),
            fb_len,
            stride: info.stride,
            bpp: info.bytes_per_pixel,
        };

        // The desktop starts empty: no windows are opened at boot, so the user
        // sees a clean desktop. Programs open from the Start menu or desktop
        // icons instead.

        // Pre-render the XP Bliss-style wallpaper: sky gradient (blue to light
        // blue) with white cloud patches and a green hill band at the bottom.
        let sky_h = (d.height - TASKBAR_HEIGHT) * 2 / 3;
        // Sky gradient.
        let sky_rect = Rect {
            x: 0,
            y: 0,
            w: d.width,
            h: sky_h,
        };
        d.gradient_v(sky_rect, Color(0x20_60_C0), Color(0x87_CE_EB));
        // Green hill band.
        let hill_rect = Rect {
            x: 0,
            y: sky_h,
            w: d.width,
            h: d.height - TASKBAR_HEIGHT - sky_h,
        };
        d.gradient_v(hill_rect, Color(0x3A_7A_20), Color(0x6A_AA_30));
        // White cloud patches.
        let clouds = [
            (120usize, 60usize, 200usize, 60usize),
            (400usize, 40usize, 180usize, 50usize),
            (700usize, 80usize, 220usize, 70usize),
            (1000usize, 50usize, 160usize, 55usize),
        ];
        for (cx, cy, cw, ch) in clouds {
            d.fill_rect_xy(cx, cy, cw, ch, Color(0xFF_FF_FF));
            d.fill_rect_xy(cx + 20, cy - 15, cw - 40, ch, Color(0xFF_FF_FF));
            d.fill_rect_xy(cx + 40, cy + 10, cw - 80, ch - 20, Color(0xFF_FF_FF));
        }
        d.wallpaper.copy_from_slice(&d.back);

        // Seed the Browser window's address bar.
        d.set_browser_url(BROWSER_HOME);
        d.browser_status = alloc::string::String::from("Ready");

        d
    }

    // -----------------------------------------------------------------------
    // Window bookkeeping
    // -----------------------------------------------------------------------

    /// Stores a window in the first free slot, or returns `None` when all eight
    /// slots are taken.
    fn add_window(&mut self, w: Window) -> Option<usize> {
        for (i, slot) in self.windows.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(w);
                self.taskbar_dirty = true;
                return Some(i);
            }
        }
        None
    }

    /// The front-most occupied slot, if any.
    fn topmost_window(&self) -> Option<usize> {
        (0..self.windows.len())
            .rev()
            .find(|&i| self.windows[i].is_some())
    }

    /// Moves window `i` to the front of the draw order, returning its new index.
    fn raise_window(&mut self, i: usize) -> usize {
        let Some(w) = self.windows[i].take() else {
            return i;
        };
        let mut top = i;
        for j in (0..self.windows.len()).rev() {
            if self.windows[j].is_some() {
                top = j;
                break;
            }
        }
        for j in i..top {
            self.windows[j] = self.windows[j + 1];
        }
        self.windows[top] = Some(w);
        top
    }

    /// Raises window `i`, makes it the active window and clears the flag on all
    /// others. Returns the window's (possibly new) slot index.
    fn activate(&mut self, i: usize) -> usize {
        if self.windows[i].is_none() {
            return i;
        }
        let top = self.raise_window(i);
        self.active_window = Some(top);
        self.taskbar_dirty = true;
        for (j, slot) in self.windows.iter_mut().enumerate() {
            if let Some(w) = slot {
                w.active = j == top;
            }
        }
        top
    }

    /// Closes window `i`, freeing its slot so a later window can reuse it.
    fn close_window(&mut self, i: usize) {
        self.windows[i] = None;
        self.taskbar_dirty = true;
        if self.active_window == Some(i) {
            self.active_window = self.topmost_window();
        }
        if let Some(top) = self.active_window {
            for (j, slot) in self.windows.iter_mut().enumerate() {
                if let Some(w) = slot {
                    w.active = j == top;
                }
            }
        }
    }

    /// Opens a window for a Start-menu entry, focusing the existing one instead
    /// of duplicating it. New windows are cascaded so they do not stack exactly.
    fn spawn_window(&mut self, title: &'static str, content: WindowContent) -> Option<usize> {
        let existing = self
            .windows
            .iter()
            .position(|slot| slot.is_some_and(|w| w.content == content));
        if let Some(i) = existing {
            self.activate(i);
            return Some(i);
        }

        let (w, h) = content.default_size();
        let step = self.spawn_count % 6;
        self.spawn_count += 1;
        let x = (90 + step * 28).min(self.width.saturating_sub(w));
        let y = (70 + step * 28).min((self.height - TASKBAR_HEIGHT).saturating_sub(h));

        let idx = self.add_window(Window {
            rect: Rect { x, y, w, h },
            title,
            active: false,
            dragging: false,
            drag_offset_x: 0,
            drag_offset_y: 0,
            content,
        })?;
        if content == WindowContent::CommandPrompt {
            self.cp_output.clear();
            self.cp_output.push_str("RustOS Command Prompt\n");
            self.cp_output.push_str("Type 'help' for commands.\n\n");
            self.cp_output.push_str("C:\\> ");
        }
        self.activate(idx);
        Some(idx)
    }

    // -----------------------------------------------------------------------
    // Browser window
    // -----------------------------------------------------------------------

    fn set_browser_url(&mut self, url: &str) {
        let bytes = url.as_bytes();
        let n = bytes.len().min(self.browser_url.len());
        self.browser_url[..n].copy_from_slice(&bytes[..n]);
        self.browser_url_len = n;
    }

    fn browser_url_str(&self) -> &str {
        core::str::from_utf8(&self.browser_url[..self.browser_url_len]).unwrap_or("")
    }

    /// Width, in characters, of the Browser window's text area.
    fn browser_columns(&self) -> usize {
        let width = self.browser_window().map_or(480, |w| w.rect.w);
        width.saturating_sub(2 * BORDER_WIDTH + 20) / 8
    }

    /// Height, in lines, of the Browser window's text area.
    fn browser_rows(&self) -> usize {
        let height = self.browser_window().map_or(360, |w| w.rect.h);
        height.saturating_sub(TITLE_BAR_HEIGHT + 48) / BROWSER_LINE_H
    }

    fn browser_window(&self) -> Option<&Window> {
        self.windows
            .iter()
            .flatten()
            .find(|w| w.content == WindowContent::Browser)
    }

    /// Fetches the address in the bar and renders it into the window. The fetch
    /// is synchronous, so a "Loading..." page is drawn first.
    fn browser_navigate(&mut self) {
        let url: alloc::string::String = self.browser_url_str().trim().into();
        if url.is_empty() {
            self.browser_status = alloc::string::String::from("Enter an address");
            return;
        }

        if !crate::net::is_up() {
            self.browser_status = alloc::string::String::from("No network adapter");
            self.browser_lines = alloc::vec![
                alloc::string::String::from("The RTL8139 adapter is not up."),
                alloc::string::String::from("Boot QEMU with -nic user,model=rtl8139."),
            ];
            self.browser_scroll = 0;
            return;
        }

        self.browser_status = alloc::string::String::from("Loading...");
        self.browser_lines = alloc::vec![alloc::format!("Loading {url} ...")];
        self.browser_scroll = 0;
        self.render();

        let without = url.strip_prefix("http://").unwrap_or(&url);
        let (host, path) = match without.split_once('/') {
            Some((host, path)) => (alloc::string::String::from(host), alloc::format!("/{path}")),
            None => (
                alloc::string::String::from(without),
                alloc::string::String::from("/"),
            ),
        };
        if host.is_empty() {
            self.browser_status = alloc::string::String::from("Invalid address");
            self.browser_lines =
                alloc::vec![alloc::string::String::from("The address has no host name.")];
            return;
        }

        let columns = self.browser_columns();
        if let Some(response) = crate::net::http_get(&host, &path) {
            self.browser_status = browser::status_line(&response);
            let body = browser::body(&response);
            let mut lines = alloc::vec::Vec::new();
            if let Some(title) = browser::title(body) {
                lines.push(title);
                lines.push(alloc::string::String::new());
            }
            lines.extend(browser::render(body, columns));
            if lines.is_empty() {
                lines.push(alloc::string::String::from(
                    "(the page had no readable text)",
                ));
            }
            self.browser_lines = lines;
        } else {
            self.browser_status = alloc::string::String::from("Request failed");
            self.browser_lines = alloc::vec![
                alloc::format!("Could not fetch http://{host}{path}"),
                alloc::string::String::from("Check the address and the network."),
            ];
        }
        self.browser_scroll = 0;
    }

    /// Handles one input byte for the Browser window. Returns `true` when the
    /// display needs to be repainted.
    fn browser_key(&mut self, byte: u8) -> bool {
        match byte {
            input::KEY_UP => {
                self.browser_scroll = self.browser_scroll.saturating_sub(1);
                true
            }
            input::KEY_DOWN => {
                let last = self.browser_lines.len().saturating_sub(1);
                self.browser_scroll = (self.browser_scroll + 1).min(last);
                true
            }
            input::KEY_PAGE_UP => {
                self.browser_scroll = self.browser_scroll.saturating_sub(self.browser_rows());
                true
            }
            input::KEY_PAGE_DOWN => {
                let last = self.browser_lines.len().saturating_sub(1);
                self.browser_scroll = (self.browser_scroll + self.browser_rows()).min(last);
                true
            }
            input::KEY_HOME => {
                self.browser_scroll = 0;
                true
            }
            input::KEY_END => {
                self.browser_scroll = self.browser_lines.len().saturating_sub(1);
                true
            }
            b'\n' | b'\r' => {
                self.browser_editing = true;
                self.browser_navigate();
                true
            }
            0x08 => {
                if self.browser_editing && self.browser_url_len > 0 {
                    self.browser_url_len -= 1;
                    true
                } else {
                    false
                }
            }
            0x20..=0x7e
                if self.browser_editing && self.browser_url_len < self.browser_url.len() - 1 =>
            {
                self.browser_url[self.browser_url_len] = byte;
                self.browser_url_len += 1;
                true
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Command Prompt window
    // -----------------------------------------------------------------------

    /// Handles one input byte for the Command Prompt window. Returns `true`
    /// when the display needs to be repainted.
    #[allow(clippy::too_many_lines)]
    fn cp_key(&mut self, byte: u8) -> bool {
        match byte {
            b'\n' | b'\r' => {
                let cmd = alloc::string::String::from(
                    core::str::from_utf8(&self.cp_input[..self.cp_input_len])
                        .unwrap_or("")
                        .trim(),
                );
                self.cp_output.push_str("C:\\> ");
                self.cp_output.push_str(&cmd);
                self.cp_output.push('\n');

                let captured = if cmd == "echo" || cmd.starts_with("echo ") {
                    let rest = cmd.strip_prefix("echo").unwrap_or("").trim();
                    alloc::string::String::from(rest) + "\n"
                } else if cmd == "heap" {
                    let (start, size) = crate::memory::info();
                    alloc::format!(
                        "heap: 0x{start:012x} .. 0x{:012x} ({} KiB)\n",
                        start + size,
                        size / 1024
                    )
                } else if cmd == "mem" {
                    let mut out = alloc::string::String::new();
                    out.push_str("start            end                      size  kind\n");
                    let mut usable = 0u64;
                    for region in self.boot_info.memory_regions.iter() {
                        let size = region.end.saturating_sub(region.start);
                        let kind = match region.kind {
                            bootloader_api::info::MemoryRegionKind::Usable => {
                                usable += size;
                                "usable"
                            }
                            bootloader_api::info::MemoryRegionKind::Bootloader => "bootloader",
                            _ => "other",
                        };
                        let _ = core::fmt::Write::write_fmt(
                            &mut out,
                            format_args!(
                                "0x{:012x}   0x{:012x}   {:>8} KiB  {}\n",
                                region.start,
                                region.end,
                                size / 1024,
                                kind
                            ),
                        );
                    }
                    let _ = core::fmt::Write::write_fmt(
                        &mut out,
                        format_args!(
                            "\n{} regions, {} MiB usable\n",
                            self.boot_info.memory_regions.len(),
                            usable / (1024 * 1024)
                        ),
                    );
                    out
                } else {
                    console::start_capture();
                    if !cmd.is_empty() {
                        crate::shell::execute(&cmd, self.boot_info);
                    }
                    console::stop_capture()
                };

                if !captured.is_empty() {
                    self.cp_output.push_str(&captured);
                    if !captured.ends_with('\n') {
                        self.cp_output.push('\n');
                    }
                }
                self.cp_input_len = 0;
                true
            }
            0x08 => {
                if self.cp_input_len > 0 {
                    self.cp_input_len -= 1;
                    true
                } else {
                    false
                }
            }
            0x20..=0x7e if self.cp_input_len < 127 => {
                self.cp_input[self.cp_input_len] = byte;
                self.cp_input_len += 1;
                true
            }
            _ => false,
        }
    }

    // -----------------------------------------------------------------------
    // Drawing primitives (operate on the off-screen back buffer)
    // -----------------------------------------------------------------------

    /// Base pointer of the back buffer. The desktop uniquely owns `back`, so
    /// writing through this pointer is sound; the `unsafe` call sites bound
    /// every access.
    fn back_ptr(&self) -> *mut u8 {
        self.back.as_ptr().cast_mut()
    }

    fn put_pixel(&self, x: usize, y: usize, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }
        let info = console::framebuffer_info();
        let offset = (y * self.stride + x) * self.bpp;
        let bytes = console::pixel_bytes(color, info.pixel_format);
        let bpp = self.bpp.min(bytes.len());
        if offset + bpp <= self.back.len() {
            // SAFETY: `back_ptr`/`back.len()` describe the owned back buffer and
            // the bounds check keeps the copy inside it.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.back_ptr().add(offset), bpp);
            }
        }
    }

    /// Writes `w` pixels of one colour at `(x, y)` in a tight loop: the colour
    /// bytes are expanded once into a repeating pattern, so a whole row costs a
    /// handful of wide copies instead of one call per pixel.
    fn fill_rect(&self, r: Rect, color: Color) {
        let info = console::framebuffer_info();
        let bytes = console::pixel_bytes(color, info.pixel_format);
        let bpp = self.bpp.min(bytes.len());
        for y in r.y..r.y + r.h {
            self.fill_span(r.x, y, r.w, &bytes, bpp);
        }
    }

    /// Writes `w` pixels of one colour at `(x, y)` in a tight loop: the colour
    /// bytes are expanded once into a compact repeating pattern, so a whole row
    /// costs a handful of wide copies instead of one call per pixel.
    fn fill_span(&self, x: usize, y: usize, w: usize, bytes: &[u8; 4], bpp: usize) {
        const PATTERN_PIXELS: usize = 32;

        if bpp == 0 || y >= self.height || w == 0 {
            return;
        }
        let x1 = (x + w).min(self.width);
        if x >= x1 {
            return;
        }

        // Compact: `bpp` bytes per pixel, no inter-pixel padding, so the
        // pattern maps 1:1 onto the `bpp`-bytes-per-pixel back buffer.
        let mut pattern = [0u8; PATTERN_PIXELS * 4];
        for i in 0..PATTERN_PIXELS {
            pattern[i * bpp..i * bpp + bpp].copy_from_slice(&bytes[..bpp]);
        }

        let back = self.back_ptr();
        let mut offset = (y * self.stride + x) * bpp;
        let mut remaining = x1 - x;
        while remaining >= PATTERN_PIXELS {
            // SAFETY: the pattern holds `PATTERN_PIXELS * bpp` valid bytes and
            // the bounds check above keeps this span on screen.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    pattern.as_ptr(),
                    back.add(offset),
                    PATTERN_PIXELS * bpp,
                );
            }
            offset += PATTERN_PIXELS * bpp;
            remaining -= PATTERN_PIXELS;
        }
        while remaining > 0 {
            // SAFETY: `bpp` bytes per pixel stay inside the span.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), back.add(offset), bpp);
            }
            offset += bpp;
            remaining -= 1;
        }
    }

    fn fill_rect_xy(&self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        self.fill_rect(Rect { x, y, w, h }, color);
    }

    fn hline(&self, x: usize, y: usize, w: usize, color: Color) {
        let info = console::framebuffer_info();
        let bytes = console::pixel_bytes(color, info.pixel_format);
        self.fill_span(x, y, w, &bytes, self.bpp.min(bytes.len()));
    }

    fn vline(&self, x: usize, y: usize, h: usize, color: Color) {
        for i in 0..h {
            self.put_pixel(x, y + i, color);
        }
    }

    fn gradient_v(&self, r: Rect, top: Color, bot: Color) {
        let info = console::framebuffer_info();
        let (tr, tg, tb) = split(top);
        let (br, bg, bb) = split(bot);
        for y in 0..r.h {
            let t = y as u32 * 256 / r.h.max(1) as u32;
            let color = Color(
                (lerp(tr, br, t) as u32) << 16
                    | (lerp(tg, bg, t) as u32) << 8
                    | (lerp(tb, bb, t) as u32),
            );
            let bytes = console::pixel_bytes(color, info.pixel_format);
            self.fill_span(r.x, r.y + y, r.w, &bytes, self.bpp.min(bytes.len()));
        }
    }

    fn gradient_h(&self, r: Rect, left: Color, right: Color) {
        let info = console::framebuffer_info();
        let (lr, lg, lb) = split(left);
        let (rr, rg, rb) = split(right);
        for x in 0..r.w {
            let t = x as u32 * 256 / r.w.max(1) as u32;
            let color = Color(
                (lerp(lr, rr, t) as u32) << 16
                    | (lerp(lg, rg, t) as u32) << 8
                    | (lerp(lb, rb, t) as u32),
            );
            let bytes = console::pixel_bytes(color, info.pixel_format);
            let bpp = self.bpp.min(bytes.len());
            for y in 0..r.h {
                self.fill_span(r.x + x, r.y + y, 1, &bytes, bpp);
            }
        }
    }

    fn draw_text(&self, x: usize, y: usize, text: &str, color: Color, scale: usize) {
        let mut cx = x;
        for ch in text.bytes() {
            if ch == b'\n' {
                cx = x;
                // (no y advance for single-line)
                continue;
            }
            let glyph = &crate::font::FONT[(ch & 0x7f) as usize];
            for (row, &bits) in glyph.iter().enumerate() {
                for col in 0..8usize {
                    if bits & (1 << col) != 0 {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                self.put_pixel(cx + col * scale + dx, y + row * scale + dy, color);
                            }
                        }
                    }
                }
            }
            cx += 8 * scale;
        }
    }

    fn draw_rect_border(&self, r: Rect, color: Color, width: usize) {
        for i in 0..width {
            self.hline(r.x + i, r.y + i, r.w - 2 * i, color);
            self.hline(r.x + i, r.y + r.h - 1 - i, r.w - 2 * i, color);
            self.vline(r.x + i, r.y + i, r.h - 2 * i, color);
            self.vline(r.x + r.w - 1 - i, r.y + i, r.h - 2 * i, color);
        }
    }

    // -----------------------------------------------------------------------
    // Presentation
    // -----------------------------------------------------------------------

    /// Copies the whole back buffer to the framebuffer in one pass. Doing this
    /// after the scene is finished is what removes the click-time flicker.
    /// Copies the back buffer to the framebuffer, applying a CRT-style
    /// post-processing pass: horizontal scanlines and a soft vignette.
    #[allow(clippy::many_single_char_names)]
    fn blit_all(&self) {
        let bpp = self.bpp;
        if bpp == 0 {
            return;
        }
        let w = self.width;
        let h = self.height;
        let stride = self.stride;
        let back = self.back_ptr();
        let fb = self.fb_ptr;
        let len = self.fb_len.min(self.back.len());
        if len == 0 {
            return;
        }

        for y in 0..h {
            // Scanline: darken every other row.
            let scanline = if y % 2 == 1 { 70u8 } else { 100u8 }; // percent
            // Vignette: darken based on distance from center.
            let dy = (y as i32 - h as i32 / 2).unsigned_abs();
            let vignette_y = if dy > h as u32 / 2 {
                60u8
            } else {
                100u8 - (dy * 40 / (h as u32 / 2)) as u8
            };
            let row_off = y * stride * bpp;
            for x in 0..w {
                let off = row_off + x * bpp;
                if off + bpp > len {
                    break;
                }
                // Read pixel from back buffer.
                let r = unsafe { *back.add(off) };
                let g = unsafe { *back.add(off + 1) };
                let b = if bpp >= 3 {
                    unsafe { *back.add(off + 2) }
                } else {
                    r
                };

                // Apply scanline + vignette.
                let vx = (x as i32 - w as i32 / 2).unsigned_abs();
                let vignette_x = if vx > w as u32 / 2 {
                    60u8
                } else {
                    100u8 - (vx * 40 / (w as u32 / 2)) as u8
                };
                let factor = (scanline as u32 * vignette_x as u32 * vignette_y as u32) / 10000;
                let factor = factor.max(30) as u8; // never fully black

                let nr = (r as u32 * factor as u32 / 100) as u8;
                let ng = (g as u32 * factor as u32 / 100) as u8;
                let nb = (b as u32 * factor as u32 / 100) as u8;

                unsafe {
                    *fb.add(off) = nr;
                    if bpp >= 2 {
                        *fb.add(off + 1) = ng;
                    }
                    if bpp >= 3 {
                        *fb.add(off + 2) = nb;
                    }
                }
            }
        }
    }

    /// Copies one screen-aligned rectangle from the back buffer to the
    /// framebuffer, used for pointer-only updates.
    #[allow(clippy::many_single_char_names)]
    fn blit_region(&self, x: usize, y: usize, w: usize, h: usize) {
        let bpp = self.bpp;
        if bpp == 0 || w == 0 || h == 0 {
            return;
        }
        let x1 = (x + w).min(self.width);
        let y1 = (y + h).min(self.height);
        if x >= x1 || y >= y1 {
            return;
        }
        let total_w = self.width;
        let total_h = self.height;
        let stride = self.stride;
        let back = self.back_ptr();
        let fb = self.fb_ptr;
        let len = self.fb_len.min(self.back.len());
        for row in y..y1 {
            let scanline = if row % 2 == 1 { 70u8 } else { 100u8 };
            let dy = (row as i32 - total_h as i32 / 2).unsigned_abs();
            let vignette_y = if dy > total_h as u32 / 2 {
                60u8
            } else {
                100u8 - (dy * 40 / (total_h as u32 / 2)) as u8
            };
            for col in x..x1 {
                let off = (row * stride + col) * bpp;
                if off + bpp > len {
                    continue;
                }
                let r = unsafe { *back.add(off) };
                let g = unsafe { *back.add(off + 1) };
                let b = if bpp >= 3 {
                    unsafe { *back.add(off + 2) }
                } else {
                    r
                };
                let vx = (col as i32 - total_w as i32 / 2).unsigned_abs();
                let vignette_x = if vx > total_w as u32 / 2 {
                    60u8
                } else {
                    100u8 - (vx * 40 / (total_w as u32 / 2)) as u8
                };
                let factor = (scanline as u32 * vignette_x as u32 * vignette_y as u32) / 10000;
                let factor = factor.max(30) as u8;
                unsafe {
                    *fb.add(off) = (r as u32 * factor as u32 / 100) as u8;
                    if bpp >= 2 {
                        *fb.add(off + 1) = (g as u32 * factor as u32 / 100) as u8;
                    }
                    if bpp >= 3 {
                        *fb.add(off + 2) = (b as u32 * factor as u32 / 100) as u8;
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&mut self) {
        mouse::set_bounds(self.width as i32, self.height as i32);
        self.cursor_x = mouse::x();
        self.cursor_y = mouse::y();

        // A full repaint overwrites everything, including the pixels saved
        // under the pointer, so the backup must be captured again.
        self.cursor_saved = false;

        // Start from the cached wallpaper (a fast memcpy) instead of
        // recomputing the gradient on every frame.
        self.back.copy_from_slice(&self.wallpaper);

        // Draw desktop icons.
        self.draw_desktop_icons();

        // Draw windows (back to front).
        for (i, slot) in self.windows.iter().enumerate() {
            if let Some(w) = slot {
                self.draw_window(w, self.active_window == Some(i));
            }
        }

        // Taskbar (cached; re-rendered only when windows change).
        if self.taskbar_dirty {
            self.draw_taskbar();
            let tb_y = self.height - TASKBAR_HEIGHT;
            for y in tb_y..self.height {
                let off = y * self.stride * self.bpp;
                let row_bytes = self.width * self.bpp;
                self.taskbar_cache[off..off + row_bytes]
                    .copy_from_slice(&self.back[off..off + row_bytes]);
            }
            self.taskbar_dirty = false;
        } else {
            let tb_y = self.height - TASKBAR_HEIGHT;
            for y in tb_y..self.height {
                let off = y * self.stride * self.bpp;
                let row_bytes = self.width * self.bpp;
                self.back[off..off + row_bytes]
                    .copy_from_slice(&self.taskbar_cache[off..off + row_bytes]);
            }
        }

        // Start menu.
        if self.start_menu_open {
            let hover = self.menu_hover_item();
            self.menu_hover = hover;
            self.draw_start_menu();
        } else {
            self.menu_hover = None;
        }

        // Present the finished scene, then composite the pointer on top.
        self.blit_all();
        self.draw_cursor();
        self.blit_region(self.cursor_saved_x, self.cursor_saved_y, CURSOR_W, CURSOR_H);
    }

    fn draw_desktop_icons(&self) {
        let icons = [
            (20usize, 20usize, "My Computer", Color(0xC0_C0_C0)),
            (20usize, 100usize, "My Documents", Color(0xFF_D7_00)),
            (20usize, 180usize, "Browser", Color(0x00_00_FF)),
            (20usize, 260usize, "Recycle Bin", Color(0x80_80_80)),
        ];
        for (ix, iy, label, color) in icons {
            // Icon (simple colored square with border).
            self.fill_rect_xy(ix, iy, 32, 32, color);
            self.draw_rect_border(
                Rect {
                    x: ix,
                    y: iy,
                    w: 32,
                    h: 32,
                },
                COL_BORDER,
                1,
            );
            // Highlight.
            self.fill_rect_xy(ix + 4, iy + 4, 24, 24, Color(0xFF_FF_FF));
            self.fill_rect_xy(ix + 8, iy + 8, 16, 16, color);
            // Label.
            self.draw_text(ix, iy + 36, label, COL_TEXT_WHITE, 1);
        }
    }

    fn draw_window(&self, w: &Window, active: bool) {
        let r = w.rect;
        // Shadow.
        self.fill_rect_xy(r.x + 4, r.y + 4, r.w, r.h, COL_SHADOW);

        // Window body.
        self.fill_rect_xy(
            r.x + BORDER_WIDTH,
            r.y + TITLE_BAR_HEIGHT,
            r.w - 2 * BORDER_WIDTH,
            r.h - TITLE_BAR_HEIGHT - BORDER_WIDTH,
            COL_WINDOW_BG,
        );

        // Title bar gradient.
        let title_rect = Rect {
            x: r.x + BORDER_WIDTH,
            y: r.y + BORDER_WIDTH,
            w: r.w - 2 * BORDER_WIDTH,
            h: TITLE_BAR_HEIGHT - BORDER_WIDTH,
        };
        if active {
            self.gradient_h(title_rect, COL_TITLE_ACTIVE, COL_TITLE_ACTIVE2);
        } else {
            self.gradient_h(title_rect, COL_TITLE_INACTIVE, COL_TITLE_INACTIVE2);
        }

        // Border.
        self.draw_rect_border(r, COL_BORDER, BORDER_WIDTH);

        // Title text.
        self.draw_text(r.x + 8, r.y + BORDER_WIDTH + 4, w.title, COL_TEXT_WHITE, 1);

        // Close button.
        let cb_x = r.x + r.w - BORDER_WIDTH - CLOSE_BTN_SIZE - 4;
        let cb_y = r.y + BORDER_WIDTH + 3;
        self.fill_rect_xy(
            cb_x,
            cb_y,
            CLOSE_BTN_SIZE,
            CLOSE_BTN_SIZE - 2,
            COL_CLOSE_RED,
        );
        // X on close button.
        for i in 0..CLOSE_BTN_SIZE - 6 {
            self.put_pixel(cb_x + 3 + i, cb_y + 2 + i, COL_TEXT_WHITE);
            self.put_pixel(cb_x + 3 + i, cb_y + CLOSE_BTN_SIZE - 4 - i, COL_TEXT_WHITE);
        }

        // Window content (skipped during drag for performance).
        if !self.skip_content {
            self.draw_window_content(w);
        }
    }

    #[allow(
        clippy::too_many_lines,
        clippy::many_single_char_names,
        clippy::needless_range_loop
    )]
    fn draw_window_content(&self, w: &Window) {
        let r = w.rect;
        let cx = r.x + BORDER_WIDTH + 8;
        let cy = r.y + TITLE_BAR_HEIGHT + 8;
        match w.content {
            WindowContent::Terminal => {
                self.draw_text(cx, cy, "RustOS Terminal", COL_TEXT, 1);
                self.draw_text(cx, cy + 16, "rustos> help", COL_TEXT, 1);
                self.draw_text(cx, cy + 32, "Type 'help' for commands.", COL_TEXT, 1);
                self.draw_text(cx, cy + 48, "Kernel running in ring 0.", COL_TEXT, 1);
            }
            WindowContent::About => {
                self.draw_text(cx, cy, "RustOS 0.1.0", COL_TEXT, 1);
                self.draw_text(cx, cy + 16, "A bare-metal x86_64 OS", COL_TEXT, 1);
                self.draw_text(cx, cy + 32, "written in Rust.", COL_TEXT, 1);
                self.draw_text(cx, cy + 48, "No Linux. No std. No libc.", COL_TEXT, 1);
            }
            WindowContent::MyComputer => {
                self.draw_text(cx, cy, "My Computer", COL_TEXT, 1);
                self.draw_text(cx, cy + 24, "Local Disk (C:)   32 MiB", COL_TEXT, 1);
                self.draw_text(cx, cy + 40, "Floppy Disk (A:)  none", COL_TEXT, 1);
                self.draw_text(cx, cy + 64, "System: RustOS 0.1.0", COL_TEXT, 1);
                self.draw_text(cx, cy + 80, "Heap: 32 MiB, ring 0", COL_TEXT, 1);
            }
            WindowContent::MyDocuments => {
                self.draw_text(cx, cy, "My Documents", COL_TEXT, 1);
                self.draw_text(cx, cy + 20, "Name              Size    Type", COL_TEXT, 1);
                self.draw_text(
                    cx,
                    cy + 32,
                    "----------------  ------  --------",
                    COL_TEXT,
                    1,
                );
                self.draw_text(cx, cy + 44, "readme.txt        1 KB    Text", COL_TEXT, 1);
                self.draw_text(
                    cx,
                    cy + 56,
                    "notes.md          2 KB    Markdown",
                    COL_TEXT,
                    1,
                );
                self.draw_text(cx, cy + 68, "rustos.log        4 KB    Log", COL_TEXT, 1);
                self.draw_text(cx, cy + 80, "config.toml       1 KB    TOML", COL_TEXT, 1);
                self.draw_text(cx, cy + 100, "4 files, 8 KB total", COL_TEXT, 1);
            }
            WindowContent::Browser => {
                // Address bar.
                let bar_w = r.w.saturating_sub(2 * BORDER_WIDTH + 8);
                let bar = Rect {
                    x: cx - 4,
                    y: cy - 4,
                    w: bar_w,
                    h: 20,
                };
                self.fill_rect_xy(bar.x, bar.y, bar.w, bar.h, Color(0xFF_FF_FF));
                self.draw_rect_border(bar, COL_BORDER, 1);

                // Address text, clipped to the bar.
                let url = self.browser_url_str();
                let max_chars = bar.w.saturating_sub(12) / 8;
                let shown: alloc::string::String = url.chars().take(max_chars).collect();
                self.draw_text(bar.x + 4, bar.y + 6, &shown, COL_TEXT, 1);
                if self.browser_editing {
                    // Caret after the address.
                    let caret_x = bar.x + 4 + shown.len() * 8;
                    self.vline(caret_x, bar.y + 4, 12, COL_TEXT);
                }

                // Go button.
                let go = Rect {
                    x: bar.x + bar.w - 34,
                    y: bar.y + 2,
                    w: 32,
                    h: 16,
                };
                self.fill_rect_xy(go.x, go.y, go.w, go.h, Color(0xEC_E9_D8));
                self.draw_rect_border(go, COL_BORDER, 1);
                self.draw_text(go.x + 8, go.y + 4, "Go", COL_TEXT, 1);

                // Status line.
                let status_y = bar.y + bar.h + 4;
                self.draw_text(cx, status_y, &self.browser_status, Color(0x00_58_EE), 1);

                // Page text.
                let text_y = status_y + 14;
                let bottom = r.y + r.h - BORDER_WIDTH - 4;
                if self.browser_lines.is_empty() {
                    self.draw_text(cx, text_y, "Type an address and press Enter.", COL_TEXT, 1);
                } else {
                    let mut y = text_y;
                    for line in self.browser_lines.iter().skip(self.browser_scroll) {
                        if y + 8 > bottom {
                            break;
                        }
                        self.draw_text(cx, y, line, COL_TEXT, 1);
                        y += BROWSER_LINE_H;
                    }
                }
            }
            WindowContent::ControlPanel => {
                self.draw_text(cx, cy, "Control Panel", COL_TEXT, 1);
                let labels = ["Display", "Sound", "Mouse", "Keyboard", "System", "Network"];
                for (i, label) in labels.iter().enumerate() {
                    let ix = cx + (i % 3) * 124;
                    let iy = cy + 26 + (i / 3) * 72;
                    // Highlight the selected icon.
                    if self.cp_selected == Some(i) {
                        self.fill_rect_xy(ix - 4, iy - 4, 40, 52, Color(0xD0_E0_F8));
                        self.draw_rect_border(
                            Rect {
                                x: ix - 4,
                                y: iy - 4,
                                w: 40,
                                h: 52,
                            },
                            Color(0x00_58_EE),
                            1,
                        );
                    }
                    self.fill_rect_xy(ix, iy, 32, 32, Color(0x4A_90_D9));
                    self.draw_rect_border(
                        Rect {
                            x: ix,
                            y: iy,
                            w: 32,
                            h: 32,
                        },
                        COL_BORDER,
                        1,
                    );
                    self.draw_text(ix, iy + 38, label, COL_TEXT, 1);
                }
                if let Some(i) = self.cp_selected {
                    let labels = ["Display", "Sound", "Mouse", "Keyboard", "System", "Network"];
                    self.draw_text(cx, cy + 170, "Selected:", COL_TEXT, 1);
                    self.draw_text(cx + 70, cy + 170, labels[i], Color(0x00_58_EE), 1);
                }
            }
            WindowContent::CommandPrompt => {
                let inner = Rect {
                    x: r.x + BORDER_WIDTH + 4,
                    y: r.y + TITLE_BAR_HEIGHT + 4,
                    w: r.w.saturating_sub(2 * BORDER_WIDTH + 8),
                    h: r.h.saturating_sub(TITLE_BAR_HEIGHT + BORDER_WIDTH + 8),
                };
                self.fill_rect(inner, Color(0x0C_0C_0C));
                let text = Color(0xD0_D0_D0);
                let tx = inner.x + 6;
                let ty = inner.y + 6;
                let line_h = 10usize;
                let max_lines = (inner.h / line_h).max(1);

                // Collect the tail of the output into a temporary buffer to avoid
                // borrow issues. Show the last 512 bytes so recent commands are
                // always visible even after a long history.
                let mut buf = [0u8; 512];
                let out = self.cp_output.as_bytes();
                let total = out.len();
                let n = total.min(512);
                let start = total.saturating_sub(n);
                buf[..n].copy_from_slice(&out[start..start + n]);

                // Count newlines and find the start of the last max_lines lines.
                let mut total_lines = 1usize;
                for i in 0..n {
                    if buf[i] == b'\n' {
                        total_lines += 1;
                    }
                }
                // Find the byte offset of the first line to display.
                let mut skip = total_lines.saturating_sub(max_lines);
                let mut off = 0usize;
                if skip > 0 {
                    for i in 0..n {
                        if buf[i] == b'\n' {
                            skip -= 1;
                            if skip == 0 {
                                off = i + 1;
                                break;
                            }
                        }
                    }
                }

                // Draw lines from cp_output.
                let mut y = ty;
                let mut cx = tx;
                let mut i = off;
                while i < n && y < inner.y + inner.h {
                    let ch = buf[i];
                    if ch == b'\n' {
                        y += line_h;
                        cx = tx;
                        i += 1;
                        continue;
                    }
                    let glyph = &crate::font::FONT[(ch & 0x7f) as usize];
                    for (row, &bits) in glyph.iter().enumerate() {
                        for col in 0..8usize {
                            if bits & (1 << col) != 0 {
                                self.put_pixel(cx + col, y + row, text);
                            }
                        }
                    }
                    cx += 8;
                    i += 1;
                    // Wrap long lines.
                    if cx >= inner.x + inner.w - 8 {
                        y += line_h;
                        cx = tx;
                    }
                }
                // Draw the current input line: "C:\> " + input + "_"
                if y < inner.y + inner.h {
                    let prompt = b"C:\\> ";
                    for &ch in prompt {
                        let glyph = &crate::font::FONT[(ch & 0x7f) as usize];
                        for (row, &bits) in glyph.iter().enumerate() {
                            for col in 0..8usize {
                                if bits & (1 << col) != 0 {
                                    self.put_pixel(cx + col, y + row, Color(0x00_FF_00));
                                }
                            }
                        }
                        cx += 8;
                    }
                    for j in 0..self.cp_input_len {
                        let ch = self.cp_input[j];
                        let glyph = &crate::font::FONT[(ch & 0x7f) as usize];
                        for (row, &bits) in glyph.iter().enumerate() {
                            for col in 0..8usize {
                                if bits & (1 << col) != 0 {
                                    self.put_pixel(cx + col, y + row, text);
                                }
                            }
                        }
                        cx += 8;
                    }
                    // Cursor.
                    self.put_pixel(cx, y, text);
                    self.put_pixel(cx, y + 1, text);
                    self.put_pixel(cx, y + 2, text);
                    self.put_pixel(cx, y + 3, text);
                    self.put_pixel(cx, y + 4, text);
                    self.put_pixel(cx, y + 5, text);
                    self.put_pixel(cx, y + 6, text);
                    self.put_pixel(cx, y + 7, text);
                }
            }
            WindowContent::Run => {
                let box_w = r.w.saturating_sub(2 * BORDER_WIDTH + 16);
                self.draw_text(cx, cy, "Type the name of a program, folder", COL_TEXT, 1);
                self.draw_text(
                    cx,
                    cy + 14,
                    "or document, and RustOS will open it.",
                    COL_TEXT,
                    1,
                );
                let box_y = cy + 40;
                self.fill_rect_xy(cx, box_y, box_w, 22, Color(0xFF_FF_FF));
                self.draw_rect_border(
                    Rect {
                        x: cx,
                        y: box_y,
                        w: box_w,
                        h: 22,
                    },
                    COL_BORDER,
                    1,
                );
                self.draw_text(cx + 4, box_y + 7, "C:\\>", COL_TEXT, 1);
                let by = box_y + 32;
                for (bi, label) in ["OK", "Cancel"].iter().enumerate() {
                    let bx = cx + bi * 80;
                    self.fill_rect_xy(bx, by, 70, 22, Color(0xEC_E9_D8));
                    self.draw_rect_border(
                        Rect {
                            x: bx,
                            y: by,
                            w: 70,
                            h: 22,
                        },
                        COL_BORDER,
                        1,
                    );
                    self.draw_text(bx + 12, by + 7, label, COL_TEXT, 1);
                }
            }
            WindowContent::LogOff => {
                self.draw_text(cx, cy, "Log Off RustOS", COL_TEXT, 1);
                self.draw_text(cx, cy + 24, "No user session is running", COL_TEXT, 1);
                self.draw_text(cx, cy + 40, "yet. Click the X to close.", COL_TEXT, 1);
            }
        }
    }

    /// Screen rectangle of the taskbar button for each visible window, in
    /// draw order. Used by both the renderer and the click handler so the two
    /// never disagree about where a button is.
    fn taskbar_buttons(&self) -> [Option<(usize, Rect)>; 8] {
        let tb_y = self.height - TASKBAR_HEIGHT;
        let mut out: [Option<(usize, Rect)>; 8] = [None; 8];
        let mut btn_x = START_BTN_WIDTH + TASKBAR_BTN_GAP;
        let mut n = 0;
        for (i, slot) in self.windows.iter().enumerate() {
            if slot.is_some() {
                out[n] = Some((
                    i,
                    Rect {
                        x: btn_x,
                        y: tb_y + 4,
                        w: TASKBAR_BTN_WIDTH,
                        h: TASKBAR_HEIGHT - 8,
                    },
                ));
                n += 1;
                btn_x += TASKBAR_BTN_WIDTH + TASKBAR_BTN_GAP;
            }
        }
        out
    }

    fn draw_taskbar(&self) {
        let tb_y = self.height - TASKBAR_HEIGHT;
        // Taskbar background.
        self.fill_rect_xy(0, tb_y, self.width, TASKBAR_HEIGHT, COL_TASKBAR);
        // Highlight line at top.
        self.hline(0, tb_y, self.width, Color(0x5B_9B_F5));

        // Start button (XP Luna orange).
        let start_rect = Rect {
            x: 0,
            y: tb_y + 2,
            w: START_BTN_WIDTH,
            h: TASKBAR_HEIGHT - 4,
        };
        self.gradient_v(start_rect, COL_START_ORANGE_LIGHT, COL_START_ORANGE_DARK);
        // Start button border.
        self.draw_rect_border(start_rect, Color(0x80_40_00), 1);
        // Start button text.
        self.draw_text(28, tb_y + 8, "start", COL_TEXT_WHITE, 1);
        // Windows logo (4 colored squares).
        self.fill_rect_xy(6, tb_y + 8, 6, 6, Color(0xFF_00_00)); // red
        self.fill_rect_xy(13, tb_y + 8, 6, 6, Color(0x00_FF_00)); // green
        self.fill_rect_xy(6, tb_y + 15, 6, 6, Color(0x00_00_FF)); // blue
        self.fill_rect_xy(13, tb_y + 15, 6, 6, Color(0xFF_FF_00)); // yellow

        // Clock area (right side).
        let clock_x = self.width - 80;
        self.fill_rect_xy(clock_x, tb_y + 2, 78, TASKBAR_HEIGHT - 4, Color(0x1A_4A_8A));
        self.draw_text(clock_x + 8, tb_y + 8, "12:00 PM", COL_TEXT_WHITE, 1);

        // Window buttons on taskbar.
        for entry in self.taskbar_buttons().iter().flatten() {
            let (i, r) = *entry;
            let color = if self.active_window == Some(i) {
                Color(0x3A_6E_A5)
            } else {
                Color(0x2A_5A_9A)
            };
            self.fill_rect(r, color);
            self.draw_rect_border(r, Color(0x1A_3A_6A), 1);
            if let Some(w) = &self.windows[i] {
                self.draw_text(r.x + 4, r.y + 4, w.title, COL_TEXT_WHITE, 1);
            }
        }
    }

    fn start_menu_rect(&self) -> Rect {
        Rect {
            x: 0,
            y: self.height - TASKBAR_HEIGHT - MENU_HEIGHT,
            w: MENU_WIDTH,
            h: MENU_HEIGHT,
        }
    }

    /// Index of the Start-menu entry under the pointer, if the menu is open and
    /// the pointer is over an entry.
    fn menu_hover_item(&self) -> Option<usize> {
        if !self.start_menu_open {
            return None;
        }
        let m = self.start_menu_rect();
        if self.cursor_x < m.x as i32 || self.cursor_y < m.y as i32 {
            return None;
        }
        let x = self.cursor_x as usize;
        let y = self.cursor_y as usize;
        if x >= m.x + m.w || y >= m.y + m.h {
            return None;
        }
        if y < m.y + MENU_HEADER {
            return None;
        }
        let item = (y - (m.y + MENU_HEADER)) / MENU_ITEM_HEIGHT;
        (item < MENU_ITEMS.len()).then_some(item)
    }

    fn draw_start_menu(&self) {
        let m = self.start_menu_rect();

        // Shadow.
        self.fill_rect_xy(m.x + 3, m.y + 3, m.w, m.h, COL_SHADOW);
        // Background.
        self.fill_rect(m, COL_MENU_BG);
        // Border.
        self.draw_rect_border(m, COL_BORDER, 1);

        // Header.
        self.gradient_v(
            Rect {
                x: m.x + 1,
                y: m.y + 1,
                w: m.w - 2,
                h: MENU_HEADER - 8,
            },
            COL_TITLE_ACTIVE,
            COL_TITLE_ACTIVE2,
        );
        self.draw_text(m.x + 10, m.y + 12, "RustOS", COL_TEXT_WHITE, 1);

        // Menu items, with the hovered one highlighted.
        let mut iy = m.y + MENU_HEADER;
        for (idx, item) in MENU_ITEMS.iter().enumerate() {
            let hovered = self.menu_hover == Some(idx);
            if hovered {
                self.fill_rect_xy(
                    m.x + 2,
                    iy - 3,
                    m.w - 4,
                    MENU_ITEM_HEIGHT - 2,
                    COL_MENU_HILITE,
                );
            }
            let color = if hovered {
                COL_TEXT_WHITE
            } else {
                COL_MENU_TEXT
            };
            self.draw_text(m.x + 12, iy, item, color, 1);
            iy += MENU_ITEM_HEIGHT;
        }
    }

    /// Copies the framebuffer pixels under the pointer into `cursor_backup`.
    fn save_cursor_bg(&mut self) {
        let bpp = self.bpp;
        let cx = self.cursor_saved_x;
        let cy = self.cursor_saved_y;
        for row in 0..CURSOR_H {
            let y = cy + row;
            if y >= self.height {
                break;
            }
            for col in 0..CURSOR_W {
                let x = cx + col;
                if x >= self.width {
                    break;
                }
                let src = (y * self.stride + x) * bpp;
                let dst = (row * CURSOR_W + col) * bpp;
                if src + bpp <= self.back.len() {
                    // SAFETY: both buffers hold `bpp`-byte pixels and the bounds
                    // check keeps the copy inside the back buffer.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            self.back_ptr().add(src),
                            self.cursor_backup.as_mut_ptr().add(dst),
                            bpp,
                        );
                    }
                }
            }
        }
    }

    /// Writes the saved pixels back, removing the pointer.
    fn restore_cursor_bg(&mut self) {
        let bpp = self.bpp;
        let cx = self.cursor_saved_x;
        let cy = self.cursor_saved_y;
        for row in 0..CURSOR_H {
            let y = cy + row;
            if y >= self.height {
                break;
            }
            for col in 0..CURSOR_W {
                let x = cx + col;
                if x >= self.width {
                    break;
                }
                let dst = (y * self.stride + x) * bpp;
                let src = (row * CURSOR_W + col) * bpp;
                if dst + bpp <= self.back.len() {
                    // SAFETY: as above, both buffers hold `bpp`-byte pixels and
                    // the bounds check keeps the copy inside the back buffer.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            self.cursor_backup.as_ptr().add(src),
                            self.back_ptr().add(dst),
                            bpp,
                        );
                    }
                }
            }
        }
    }

    /// Removes the pointer by restoring the pixels saved underneath it.
    fn erase_cursor(&mut self) {
        if self.cursor_saved {
            self.restore_cursor_bg();
            self.cursor_saved = false;
        }
    }

    /// Draws the pointer into the back buffer, saving whatever is underneath it.
    fn draw_cursor(&mut self) {
        self.cursor_saved_x = self.cursor_x.max(0) as usize;
        self.cursor_saved_y = self.cursor_y.max(0) as usize;
        self.save_cursor_bg();
        self.cursor_saved = true;

        let cx = self.cursor_saved_x;
        let cy = self.cursor_saved_y;
        for (row, line) in CURSOR_SHAPE.iter().enumerate() {
            for (col, ch) in line.bytes().enumerate() {
                let color = match ch {
                    b'X' => COL_CURSOR_EDGE,
                    b'#' => COL_CURSOR_FILL,
                    _ => continue, // transparent
                };
                self.put_pixel(cx + col, cy + row, color);
            }
        }
    }

    /// Screen rectangle covering both the old and the new pointer positions.
    fn cursor_union(
        &self,
        old_x: usize,
        old_y: usize,
        had_old: bool,
    ) -> (usize, usize, usize, usize) {
        if !had_old {
            return (self.cursor_saved_x, self.cursor_saved_y, CURSOR_W, CURSOR_H);
        }
        let x0 = old_x.min(self.cursor_saved_x);
        let y0 = old_y.min(self.cursor_saved_y);
        let x1 = (old_x + CURSOR_W).max(self.cursor_saved_x + CURSOR_W);
        let y1 = (old_y + CURSOR_H).max(self.cursor_saved_y + CURSOR_H);
        (x0, y0, x1 - x0, y1 - y0)
    }

    /// Moves the pointer without repainting the rest of the scene: erase the old
    /// pointer, draw it at its new position, then copy just the affected area to
    /// the framebuffer. This is what keeps mouse movement smooth.
    fn render_cursor(&mut self) {
        let (old_x, old_y, had_old) = (self.cursor_saved_x, self.cursor_saved_y, self.cursor_saved);
        self.erase_cursor();
        self.draw_cursor();
        let (x, y, w, h) = self.cursor_union(old_x, old_y, had_old);
        self.blit_region(x, y, w, h);
    }

    /// Repaints just the Start menu (used when the hovered entry changes) and
    /// re-composites the pointer.
    fn repaint_menu(&mut self) {
        let (old_x, old_y, had_old) = (self.cursor_saved_x, self.cursor_saved_y, self.cursor_saved);
        self.erase_cursor();

        self.draw_start_menu();
        let menu_rect = self.start_menu_rect();
        self.blit_region(menu_rect.x, menu_rect.y, menu_rect.w + 4, menu_rect.h + 4);

        self.draw_cursor();
        let (union_x, union_y, union_w, union_h) = self.cursor_union(old_x, old_y, had_old);
        self.blit_region(union_x, union_y, union_w, union_h);
    }

    // -----------------------------------------------------------------------
    // Input handling
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_lines)]
    fn handle_click(&mut self, x: usize, y: usize) {
        let tb_y = self.height - TASKBAR_HEIGHT;

        // Start button.
        if x < START_BTN_WIDTH && y >= tb_y {
            self.start_menu_open = !self.start_menu_open;
            return;
        }

        // Start menu items.
        if self.start_menu_open {
            let m = self.start_menu_rect();
            if x >= m.x && x < m.x + m.w && y >= m.y && y < m.y + m.h {
                self.start_menu_click(y);
                return;
            }
            self.start_menu_open = false;
        }

        // Taskbar window buttons.
        if y >= tb_y {
            for entry in self.taskbar_buttons().iter().flatten() {
                let (i, r) = *entry;
                if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
                    self.activate(i);
                    return;
                }
            }
            return;
        }

        // Desktop icons.
        let icons: [(usize, usize, &str, WindowContent); 4] = [
            (20, 20, "My Computer", WindowContent::MyComputer),
            (20, 100, "My Documents", WindowContent::MyDocuments),
            (20, 180, "Browser", WindowContent::Browser),
            (20, 260, "Recycle Bin", WindowContent::Terminal),
        ];
        for (ix, iy, _label, content) in icons {
            if x >= ix && x < ix + 32 && y >= iy && y < iy + 48 {
                self.spawn_window(
                    match content {
                        WindowContent::MyComputer => "My Computer",
                        WindowContent::MyDocuments => "My Documents",
                        WindowContent::Browser => "Browser",
                        _ => "Terminal",
                    },
                    content,
                );
                return;
            }
        }

        // Check windows (front to back).
        for i in (0..self.windows.len()).rev() {
            let Some(w) = self.windows[i] else {
                continue;
            };
            let r = w.rect;
            if x < r.x || x >= r.x + r.w || y < r.y || y >= r.y + r.h {
                continue;
            }

            // Close button.
            let cb_x = r.x + r.w - BORDER_WIDTH - CLOSE_BTN_SIZE - 4;
            let cb_y = r.y + BORDER_WIDTH + 3;
            if x >= cb_x && x < cb_x + CLOSE_BTN_SIZE && y >= cb_y && y < cb_y + CLOSE_BTN_SIZE - 2
            {
                self.close_window(i);
                return;
            }

            // Run dialog: clicking OK or Cancel closes the window.
            if w.content == WindowContent::Run {
                let cx = r.x + BORDER_WIDTH + 8;
                let cy = r.y + TITLE_BAR_HEIGHT + 8;
                let box_y = cy + 40;
                let by = box_y + 32;
                // OK button at (cx, by), Cancel at (cx + 80, by).
                if y >= by && y < by + 22 {
                    if x >= cx && x < cx + 70 {
                        self.close_window(i);
                        return;
                    }
                    if x >= cx + 80 && x < cx + 150 {
                        self.close_window(i);
                        return;
                    }
                }
            }

            // Control Panel: clicking an icon selects it.
            if w.content == WindowContent::ControlPanel {
                let cx = r.x + BORDER_WIDTH + 8;
                let cy = r.y + TITLE_BAR_HEIGHT + 8;
                for ii in 0..6usize {
                    let ix = cx + (ii % 3) * 124;
                    let iy = cy + 26 + (ii / 3) * 72;
                    if x >= ix && x < ix + 32 && y >= iy && y < iy + 32 {
                        self.cp_selected = Some(ii);
                        break;
                    }
                }
            }

            // Browser: the address bar, the Go button, or the page area.
            // Fetching is synchronous, so this returns after navigating.
            if w.content == WindowContent::Browser {
                let cx = r.x + BORDER_WIDTH + 8;
                let cy = r.y + TITLE_BAR_HEIGHT + 8;
                let bar = Rect {
                    x: cx - 4,
                    y: cy - 4,
                    w: r.w.saturating_sub(2 * BORDER_WIDTH + 8),
                    h: 20,
                };
                let go = Rect {
                    x: bar.x + bar.w - 34,
                    y: bar.y + 2,
                    w: 32,
                    h: 16,
                };
                if x >= go.x && x < go.x + go.w && y >= go.y && y < go.y + go.h {
                    self.activate(i);
                    self.browser_editing = false;
                    self.browser_navigate();
                    return;
                }
                if x >= bar.x && x < bar.x + bar.w && y >= bar.y && y < bar.y + bar.h {
                    self.activate(i);
                    self.browser_editing = true;
                    return;
                }
                self.browser_editing = false;
            }

            // Raise and activate, then start a title-bar drag on the window's
            // new slot (raising can move it).
            let idx = self.activate(i);
            if y >= r.y + BORDER_WIDTH && y < r.y + TITLE_BAR_HEIGHT {
                self.dragging_window = Some(idx);
                if let Some(w) = &mut self.windows[idx] {
                    w.dragging = true;
                    w.drag_offset_x = x as isize - r.x as isize;
                    w.drag_offset_y = y as isize - r.y as isize;
                }
            }
            return;
        }
    }

    /// Runs the action for the Start-menu entry the click landed on.
    fn start_menu_click(&mut self, y: usize) {
        let m = self.start_menu_rect();
        self.start_menu_open = false;
        self.menu_hover = None;
        if y < m.y + MENU_HEADER {
            return; // Header, no action.
        }
        match (y - (m.y + MENU_HEADER)) / MENU_ITEM_HEIGHT {
            0 => {
                self.spawn_window("My Computer", WindowContent::MyComputer);
            }
            1 => {
                self.spawn_window("Control Panel", WindowContent::ControlPanel);
            }
            2 => {
                self.spawn_window("Command Prompt", WindowContent::CommandPrompt);
            }
            3 => {
                self.spawn_window("Run", WindowContent::Run);
            }
            4 => {
                self.spawn_window("About RustOS", WindowContent::About);
            }
            5 => {
                self.spawn_window("Log Off", WindowContent::LogOff);
            }
            6 => crate::shell::shutdown(),
            _ => {}
        }
    }

    fn handle_drag(&mut self, x: usize, y: usize) {
        if let Some(i) = self.dragging_window {
            if let Some(w) = &mut self.windows[i] {
                let new_x = (x as isize - w.drag_offset_x).max(0) as usize;
                let new_y = (y as isize - w.drag_offset_y).max(0) as usize;
                w.rect.x = new_x.min(self.width.saturating_sub(w.rect.w));
                w.rect.y = new_y.min((self.height - TASKBAR_HEIGHT).saturating_sub(w.rect.h));
            }
        }
    }

    fn handle_release(&mut self) {
        if let Some(i) = self.dragging_window {
            if let Some(w) = &mut self.windows[i] {
                w.dragging = false;
            }
        }
        self.dragging_window = None;
    }
}

fn split(color: Color) -> (u8, u8, u8) {
    (
        ((color.0 >> 16) & 0xff) as u8,
        ((color.0 >> 8) & 0xff) as u8,
        (color.0 & 0xff) as u8,
    )
}

fn lerp(a: u8, b: u8, t: u32) -> u8 {
    let a = a as i32;
    let b = b as i32;
    (a + (b - a) * t as i32 / 256) as u8
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

static mut DESKTOP: Option<Desktop> = None;

pub fn init(width: usize, height: usize, boot_info: &'static BootInfo) {
    let fb = console::framebuffer_buffer();
    let info = console::framebuffer_info();
    unsafe {
        DESKTOP = Some(Desktop::new(width, height, fb, &info, boot_info));
    }
}

pub fn desktop() -> &'static mut Desktop {
    unsafe { (&raw mut DESKTOP).as_mut().unwrap().as_mut().unwrap() }
}

#[allow(clippy::too_many_lines)]
pub fn run() {
    // Initial render.
    {
        let d = desktop();
        d.render();
    }
    let mut last = (mouse::x(), mouse::y(), mouse::buttons());

    loop {
        // Any byte on COM1 hands the console to the serial shell. This keeps
        // the desktop usable on real hardware while letting a headless test
        // drive the shell without a display adapter.
        if input::serial_seen() {
            return;
        }

        // Keep the pointer on screen: the driver only clamps at zero, and a
        // cursor-only update no longer goes through `render()`.
        {
            let d = desktop();
            mouse::set_bounds(d.width as i32, d.height as i32);
        }

        // Poll keyboard input for the active window (even when the mouse is
        // stationary).
        {
            let d = desktop();
            let active = d
                .active_window
                .and_then(|i| d.windows.get(i))
                .and_then(|slot| slot.as_ref())
                .map(|w| w.content);
            let mut changed = false;
            while let Some(byte) = input::pop() {
                changed |= match active {
                    Some(WindowContent::CommandPrompt) => d.cp_key(byte),
                    Some(WindowContent::Browser) => d.browser_key(byte),
                    _ => false,
                };
            }
            if changed {
                d.render();
            }
        }

        // Poll mouse state.
        let state = (mouse::x(), mouse::y(), mouse::buttons());
        if state != last {
            last = state;
            let (mx, my, buttons) = state;
            let d = desktop();
            d.cursor_x = mx;
            d.cursor_y = my;
            d.mouse_down = buttons & 0x01 != 0;

            // Clicks and drags change the scene; a pure move does not, so a
            // move only repaints the pointer (a full repaint on every packet
            // would make the desktop flicker).
            let was_down = d.mouse_was_down;
            let mut scene_changed = false;
            if d.mouse_down && !was_down {
                d.handle_click(mx as usize, my as usize);
                scene_changed = true;
            }
            if !d.mouse_down && was_down {
                d.handle_release();
                scene_changed = true;
            }
            if d.mouse_down && d.dragging_window.is_some() {
                d.handle_drag(mx as usize, my as usize);
                scene_changed = true;
            }
            d.mouse_was_down = d.mouse_down;

            // Skip window content during drag to keep it smooth.
            d.skip_content = d.dragging_window.is_some();

            if scene_changed {
                d.render();
            } else if d.start_menu_open {
                // Moving over the menu changes only the highlighted entry, so
                // repaint the menu rectangle instead of the whole scene.
                let hover = d.menu_hover_item();
                if hover == d.menu_hover {
                    d.render_cursor();
                } else {
                    d.menu_hover = hover;
                    d.repaint_menu();
                }
            } else {
                d.render_cursor();
            }
        }

        x86_64::instructions::hlt();
    }
}
