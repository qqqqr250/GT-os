//! Text console: renders 8x8 glyphs into the linear framebuffer provided by the
//! bootloader and mirrors all output to COM1.

use core::fmt::{self, Write};

use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};
use spin::Mutex;
use x86_64::instructions::interrupts;

use crate::font::{FONT, FONT_HEIGHT, FONT_WIDTH};
use crate::serial;

/// A 24-bit `0xRRGGBB` color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    /// The `(r, g, b)` channels of the colour.
    #[must_use]
    pub const fn rgb(self) -> (u8, u8, u8) {
        (
            ((self.0 >> 16) & 0xff) as u8,
            ((self.0 >> 8) & 0xff) as u8,
            (self.0 & 0xff) as u8,
        )
    }

    /// The VGA text colour this console colour renders as.
    ///
    /// The named console colours have an intended text-mode counterpart, so they
    /// are matched exactly; every other colour is snapped to the nearest entry of
    /// the text palette by [`crate::vga::Color::from_rgb`].
    #[must_use]
    pub fn to_vga_text(self) -> crate::vga::Color {
        use crate::vga::Color as Vga;
        if self == BG {
            Vga::Black
        } else if self == FG {
            Vga::LightGray
        } else if self == DIM {
            Vga::DarkGray
        } else if self == GREEN {
            Vga::LightGreen
        } else if self == CYAN {
            Vga::LightCyan
        } else if self == RED {
            Vga::LightRed
        } else {
            let (r, g, b) = self.rgb();
            Vga::from_rgb(r, g, b)
        }
    }
}

pub const BG: Color = Color(0x00_0b_0e_14);
pub const FG: Color = Color(0x00_d8_de_e9);
pub const DIM: Color = Color(0x00_70_7a_8a);
pub const GREEN: Color = Color(0x00_7e_c6_99);
pub const CYAN: Color = Color(0x00_4f_c1_ff);
pub const RED: Color = Color(0x00_e0_6c_75);

pub fn pixel_bytes(color: Color, format: PixelFormat) -> [u8; 4] {
    let r = ((color.0 >> 16) & 0xff) as u8;
    let g = ((color.0 >> 8) & 0xff) as u8;
    let b = (color.0 & 0xff) as u8;

    match format {
        PixelFormat::Rgb => [r, g, b, 0xff],
        PixelFormat::Bgr => [b, g, r, 0xff],
        // Perceived luminance, good enough for a monochrome display.
        PixelFormat::U8 => {
            // The weights sum to 256 and each channel is at most 255, so the
            // shifted result always fits in a `u8`.
            let luma = ((u32::from(r) * 77 + u32::from(g) * 150 + u32::from(b) * 29) >> 8) as u8;
            [luma, luma, luma, 0xff]
        }
        PixelFormat::Unknown {
            red_position,
            green_position,
            blue_position,
        } => {
            let value = (u32::from(r) << red_position)
                | (u32::from(g) << green_position)
                | (u32::from(b) << blue_position);
            value.to_le_bytes()
        }
        _ => [0, 0, 0, 0],
    }
}

struct TextConsole {
    buffer: &'static mut [u8],
    info: FrameBufferInfo,
    scale: usize,
    fg: Color,
    bg: Color,
    /// Pixel position of the next glyph cell.
    x: usize,
    y: usize,
}

impl TextConsole {
    fn new(buffer: &'static mut [u8], info: FrameBufferInfo) -> Self {
        // Prefer ~80 columns and at least 25 rows, but never go below 1x or
        // above 3x magnification.
        let by_width = info.width / (FONT_WIDTH * 80);
        let by_height = info.height / ((FONT_HEIGHT + 1) * 25);
        let scale = by_width.min(by_height).clamp(1, 3);

        Self {
            buffer,
            info,
            scale,
            fg: FG,
            bg: BG,
            x: 0,
            y: 0,
        }
    }

    fn char_width(&self) -> usize {
        FONT_WIDTH * self.scale
    }

    fn line_height(&self) -> usize {
        (FONT_HEIGHT + 1) * self.scale
    }

    fn put_pixel(&mut self, x: usize, y: usize, pixel: [u8; 4]) {
        let info = self.info;
        if x >= info.width || y >= info.height {
            return;
        }
        let offset = (y * info.stride + x) * info.bytes_per_pixel;
        let len = info.bytes_per_pixel.min(pixel.len());
        if offset + len <= self.buffer.len() {
            self.buffer[offset..offset + len].copy_from_slice(&pixel[..len]);
        }
    }

    fn fill_rect(&mut self, x: usize, y: usize, width: usize, height: usize, color: Color) {
        let pixel = pixel_bytes(color, self.info.pixel_format);
        for row in y..y + height {
            for col in x..x + width {
                self.put_pixel(col, row, pixel);
            }
        }
    }

    fn draw_glyph(&mut self, byte: u8) {
        let glyph = &FONT[(byte & 0x7f) as usize];
        let scale = self.scale;
        let (x0, y0) = (self.x, self.y);
        let fg = pixel_bytes(self.fg, self.info.pixel_format);
        let bg = pixel_bytes(self.bg, self.info.pixel_format);

        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..FONT_WIDTH {
                let lit = bits & (1 << col) != 0;
                let pixel = if lit { fg } else { bg };
                for dy in 0..scale {
                    for dx in 0..scale {
                        self.put_pixel(x0 + col * scale + dx, y0 + row * scale + dy, pixel);
                    }
                }
            }
        }
    }

    fn clear(&mut self) {
        self.fill_rect(0, 0, self.info.width, self.info.height, self.bg);
        self.x = 0;
        self.y = 0;
    }

    fn scroll(&mut self) {
        let info = self.info;
        let line_bytes = info.stride * info.bytes_per_pixel;
        let shift = self.line_height() * line_bytes;
        let visible = info.height * line_bytes;

        if shift < visible && visible <= self.buffer.len() {
            self.buffer.copy_within(shift..visible, 0);
        }

        let top = info.height.saturating_sub(self.line_height());
        self.fill_rect(0, top, info.width, self.line_height(), self.bg);
        self.x = 0;
        self.y = top;
    }

    fn newline(&mut self) {
        let next = self.y + self.line_height();
        if next + self.line_height() > self.info.height {
            self.scroll();
        } else {
            self.x = 0;
            self.y = next;
        }
    }

    fn backspace(&mut self) {
        if self.x >= self.char_width() {
            self.x -= self.char_width();
            self.fill_rect(
                self.x,
                self.y,
                self.char_width(),
                self.line_height(),
                self.bg,
            );
        }
    }

    fn put_char(&mut self, byte: u8) {
        if self.x + self.char_width() > self.info.width {
            self.newline();
        }
        if self.y + self.line_height() > self.info.height {
            self.scroll();
        }
        self.draw_glyph(byte);
        self.x += self.char_width();
    }

    fn write_ascii(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.x = 0,
            b'\t' => {
                let cells = 4 - ((self.x / self.char_width()) % 4);
                for _ in 0..cells {
                    self.put_char(b' ');
                }
            }
            0x08 => self.backspace(),
            0x00..=0x1f => {}
            _ => self.put_char(byte),
        }
    }
}

struct Console {
    fb: Option<TextConsole>,
}

impl Console {
    fn write_byte(&mut self, byte: u8) {
        if let Some(fb) = self.fb.as_mut() {
            fb.write_ascii(byte);
        } else {
            // No linear framebuffer: fall back to the firmware's VGA text mode.
            crate::vga::write_byte(byte);
        }
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Mirror to the serial console first so that a crash in the renderer
        // still leaves a log behind.
        serial::write_str(s);
        for ch in s.chars() {
            if ch.is_ascii() {
                self.write_byte(ch as u8);
            } else {
                self.write_byte(b'?');
            }
        }
        Ok(())
    }
}

static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

// Capture buffer: when Some, print_args appends to it instead of the framebuffer.
static CAPTURE: Mutex<Option<alloc::string::String>> = Mutex::new(None);

// Global framebuffer access for the window manager.
static mut FB_BUFFER: Option<&'static mut [u8]> = None;
static mut FB_INFO: Option<FrameBufferInfo> = None;

/// Installs the console. Returns the framebuffer description, if one exists.
pub fn init(framebuffer: Option<FrameBuffer>) -> Option<FrameBufferInfo> {
    let (fb, info) = match framebuffer {
        Some(fb) => {
            let info = fb.info();
            let buffer = fb.into_buffer();
            unsafe {
                FB_BUFFER = Some(buffer);
                FB_INFO = Some(info);
            }
            let buffer = unsafe { (&raw mut FB_BUFFER).as_mut().unwrap().as_mut().unwrap() };
            (Some(TextConsole::new(buffer, info)), Some(info))
        }
        None => (None, None),
    };

    let mut console = Console { fb };
    // Paint the whole screen once; firmware may have left garbage behind.
    if let Some(fb) = console.fb.as_mut() {
        fb.clear();
    }
    *CONSOLE.lock() = Some(console);
    info
}

/// Formats to the console and to COM1.
///
/// The whole path runs with interrupts disabled (post-07): the timer handler
/// prints through the serial lock this reaches, and holding either lock with
/// interrupts enabled would let the handler deadlock the main thread.
pub fn print_args(args: fmt::Arguments<'_>) {
    interrupts::without_interrupts(|| {
        // If capture mode is active, append to the capture buffer.
        {
            let mut cap = CAPTURE.lock();
            if let Some(buf) = cap.as_mut() {
                let _ = core::fmt::Write::write_fmt(buf, args);
                return;
            }
        }
        {
            let mut lock = CONSOLE.try_lock();
            if let Some(console) = lock.as_mut().and_then(|guard| guard.as_mut()) {
                let _ = console.write_fmt(args);
                return;
            }
        }
        // The console lock is held (panic inside a print, for example). Degrade to
        // serial-only output instead of deadlocking.
        let _ = serial::Writer.write_fmt(args);
    });
}

/// Writes raw bytes to the console and to COM1.
///
/// Used by the ring-3 `write` syscall, whose buffer is user memory that may hold
/// any byte value rather than valid UTF-8.
pub fn write_bytes(bytes: &[u8]) {
    interrupts::without_interrupts(|| {
        {
            let mut cap = CAPTURE.lock();
            if let Some(buf) = cap.as_mut() {
                for &byte in bytes {
                    buf.push(if byte.is_ascii() { byte as char } else { '?' });
                }
                return;
            }
        }
        // Mirror to COM1 exactly like `Console::write_str` does, so headless tests
        // still see the program's output.
        serial::write_bytes(bytes);
        let mut lock = CONSOLE.try_lock();
        if let Some(console) = lock.as_mut().and_then(|guard| guard.as_mut()) {
            for &byte in bytes {
                console.write_byte(byte);
            }
        }
    });
}

pub fn set_fg(color: Color) {
    // No-op during capture mode.
    let cap = CAPTURE.lock();
    if cap.is_some() {
        return;
    }
    drop(cap);
    let mut guard = CONSOLE.lock();
    if let Some(fb) = guard.as_mut().and_then(|console| console.fb.as_mut()) {
        fb.fg = color;
    } else {
        // No linear framebuffer: render through the VGA text fallback, mapping
        // the console colour to its text-mode counterpart.
        crate::vga::set_fg(color.to_vga_text());
    }
}

pub fn start_capture() {
    *CAPTURE.lock() = Some(alloc::string::String::new());
}

pub fn stop_capture() -> alloc::string::String {
    CAPTURE.lock().take().unwrap_or_default()
}

pub fn clear_screen() {
    let cap = CAPTURE.lock();
    if cap.is_some() {
        return;
    }
    drop(cap);
    let mut guard = CONSOLE.lock();
    if let Some(fb) = guard.as_mut().and_then(|console| console.fb.as_mut()) {
        fb.clear();
    } else {
        // Reset the VGA text fallback to the default colours, then clear it.
        crate::vga::set_bg(crate::vga::Color::Black);
        crate::vga::set_fg(crate::vga::Color::LightGray);
        crate::vga::clear();
    }
}

/// Clears a strip of `pixels` rows at the current cursor and moves the cursor
/// just below it. Returns the pixel `y` of the strip's top edge, so a caller can
/// draw a bitmap there without text overwriting it. Returns `0` when there is
/// no framebuffer (the VGA text fallback has no pixel coordinates).
pub fn reserve_bitmap(pixels: usize) -> usize {
    let mut guard = CONSOLE.lock();
    let Some(console) = guard.as_mut() else {
        return 0;
    };
    let Some(fb) = console.fb.as_mut() else {
        return 0;
    };
    let line = fb.line_height();
    let rows = pixels.div_ceil(line);
    let strip = rows * line;
    let top = fb.y;
    let available = fb.info.height.saturating_sub(top);
    fb.fill_rect(0, top, fb.info.width, strip.min(available), fb.bg);
    fb.y = (top + strip).min(fb.info.height.saturating_sub(line));
    fb.x = 0;
    top
}

/// Console geometry as `(columns, rows)`, if a framebuffer is present.
/// Returns a mutable reference to the framebuffer buffer, if one exists.
static mut EMPTY_BUF: [u8; 0] = [];

pub fn framebuffer_buffer() -> &'static mut [u8] {
    unsafe {
        if let Some(buf) = (&raw mut FB_BUFFER).as_mut().unwrap().as_mut() {
            buf
        } else {
            // No framebuffer yet: hand back an empty buffer.
            core::slice::from_raw_parts_mut((&raw mut EMPTY_BUF).cast::<u8>(), 0)
        }
    }
}

/// Returns the framebuffer info, if one exists.
pub fn framebuffer_info() -> FrameBufferInfo {
    unsafe {
        FB_INFO.unwrap_or(FrameBufferInfo {
            byte_len: 0,
            width: 0,
            height: 0,
            pixel_format: PixelFormat::Unknown {
                red_position: 0,
                green_position: 0,
                blue_position: 0,
            },
            bytes_per_pixel: 0,
            stride: 0,
        })
    }
}

pub fn dimensions() -> Option<(usize, usize)> {
    let guard = CONSOLE.lock();
    let fb = guard.as_ref()?.fb.as_ref()?;
    Some((
        fb.info.width / fb.char_width(),
        fb.info.height / fb.line_height(),
    ))
}

/// Panic-path output: red text, never blocks on a poisoned console.
pub fn panic_print(args: fmt::Arguments<'_>) {
    let mut lock = CONSOLE.try_lock();
    if let Some(console) = lock.as_mut().and_then(|guard| guard.as_mut()) {
        if let Some(fb) = console.fb.as_mut() {
            fb.fg = RED;
        } else {
            // No framebuffer: highlight the panic on the VGA text console.
            crate::vga::set_color(crate::vga::ColorCode::new(
                crate::vga::Color::White,
                crate::vga::Color::Red,
            ));
        }
        let _ = console.write_fmt(args);
        return;
    }
    // The console could not take the message. Say why: a lock held by another
    // CPU ("busy") is a different situation from a console that was never
    // initialized, and calling the latter "busy" is misleading.
    let reason = if lock.is_none() {
        "PANIC (console busy): "
    } else {
        "PANIC (console not initialized): "
    };
    drop(lock);
    serial::try_write_str(reason);
    let _ = serial::Writer.write_fmt(args);
}

macro_rules! print {
    ($($arg:tt)*) => {
        $crate::console::print_args(format_args!($($arg)*))
    };
}

macro_rules! println {
    () => {
        $crate::console::print_args(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::console::print_args(format_args!("{}\n", format_args!($($arg)*)))
    };
}

// Re-exported per crate so modules can `use crate::console::{print, println};`
// without fighting the `println!` that the Rust prelude already provides.
pub(crate) use print;
pub(crate) use println;
