//! Safe access to the legacy VGA text-mode buffer at physical `0xb8000`.
//!
//! This is the "post-03, done properly" module. `blog_os` post-03 turns the
//! post-02 kernel into a real text writer — a global `WRITER` behind a spin lock,
//! a `Writer` that wraps lines and scrolls, the sixteen `Color`s, a `ColorCode`
//! attribute, `ScreenChar` cells and `print!`/`println!` macros — but it
//! dereferences `0xb8000` in a `lazy_static`, keeps the cursor at the bottom row,
//! has no hardware cursor and no tests. This version keeps post-03's names and
//! closes those gaps:
//!
//! - the global is a `Mutex<Option<Writer<_>>>` that [`init`] fills in, so nothing
//!   touches the buffer before the bootloader's window is known,
//! - the buffer is reached through that physical-memory window and only ever
//!   through [`Volatile`],
//! - the grid is abstracted behind [`Buffer`], so the unit tests at the bottom of
//!   this file drive the exact same writer against a plain RAM buffer,
//! - the cursor is tracked and programmed through the CRTC,
//! - a byte with no printable ASCII glyph becomes `0xfe` (post-03's "filled box")
//!   instead of vanishing, and
//! - every write is mirrored to COM1, so a headless QEMU run shows the output.
//!
//! `cargo test` runs those tests on the host, with no hardware.

use core::fmt;

use crate::port;
use crate::volatile::Volatile;
use spin::Mutex;

/// Physical address of the VGA text buffer.
pub const BUFFER_PHYS: u64 = 0xb8000;

/// Text columns in the buffer.
pub const WIDTH: usize = 80;
/// Text rows in the buffer.
pub const HEIGHT: usize = 25;

/// The sixteen colours the VGA text-mode attribute byte can encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    /// Black.
    Black = 0,
    /// Blue.
    Blue = 1,
    /// Green.
    Green = 2,
    /// Cyan.
    Cyan = 3,
    /// Red.
    Red = 4,
    /// Magenta.
    Magenta = 5,
    /// Brown.
    Brown = 6,
    /// Light gray (the default foreground).
    LightGray = 7,
    /// Dark gray.
    DarkGray = 8,
    /// Light blue.
    LightBlue = 9,
    /// Light green.
    LightGreen = 10,
    /// Light cyan.
    LightCyan = 11,
    /// Light red.
    LightRed = 12,
    /// Pink.
    Pink = 13,
    /// Yellow.
    Yellow = 14,
    /// White.
    White = 15,
}

impl Color {
    /// The sixteen RGB values the hardware palette powers up with.
    const PALETTE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (0, 0, 170),
        (0, 170, 0),
        (0, 170, 170),
        (170, 0, 0),
        (170, 0, 170),
        (170, 85, 0),
        (170, 170, 170),
        (85, 85, 85),
        (85, 85, 255),
        (85, 255, 85),
        (85, 255, 255),
        (255, 85, 85),
        (255, 85, 255),
        (255, 255, 85),
        (255, 255, 255),
    ];

    /// The colour of the palette closest to `(r, g, b)`.
    ///
    /// The 24-bit framebuffer console and this 4-bit text console share one
    /// palette, so a colour chosen for the framebuffer is snapped to the nearest
    /// text colour here instead of being dropped.
    #[must_use]
    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        let mut best = 0;
        let mut best_distance = u32::MAX;
        for (index, &(pr, pg, pb)) in Self::PALETTE.iter().enumerate() {
            let dr = i32::from(r) - i32::from(pr);
            let dg = i32::from(g) - i32::from(pg);
            let db = i32::from(b) - i32::from(pb);
            let distance = (dr * dr + dg * dg + db * db) as u32;
            if distance < best_distance {
                best_distance = distance;
                best = index;
            }
        }
        // `best` is one of the sixteen palette indices.
        Self::from_palette(best)
    }

    /// The colour with the given palette index (`0..=15`).
    #[must_use]
    pub const fn from_palette(index: usize) -> Self {
        match index {
            1 => Self::Blue,
            2 => Self::Green,
            3 => Self::Cyan,
            4 => Self::Red,
            5 => Self::Magenta,
            6 => Self::Brown,
            7 => Self::LightGray,
            8 => Self::DarkGray,
            9 => Self::LightBlue,
            10 => Self::LightGreen,
            11 => Self::LightCyan,
            12 => Self::LightRed,
            13 => Self::Pink,
            14 => Self::Yellow,
            15 => Self::White,
            _ => Self::Black,
        }
    }
}

/// A foreground/background colour pair, packed as the VGA attribute byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ColorCode(u8);

impl ColorCode {
    /// Packs a foreground and a background colour into an attribute byte.
    #[must_use]
    pub const fn new(foreground: Color, background: Color) -> Self {
        Self(((background as u8) << 4) | (foreground as u8))
    }

    /// The foreground colour.
    #[must_use]
    pub const fn foreground(self) -> Color {
        Color::from_palette((self.0 & 0x0f) as usize)
    }

    /// The background colour.
    #[must_use]
    pub const fn background(self) -> Color {
        Color::from_palette((self.0 >> 4) as usize)
    }

    /// The packed attribute byte.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// This pair with a different foreground.
    #[must_use]
    pub const fn with_foreground(self, foreground: Color) -> Self {
        Self((self.0 & 0xf0) | foreground as u8)
    }

    /// This pair with a different background.
    #[must_use]
    pub const fn with_background(self, background: Color) -> Self {
        Self((self.0 & 0x0f) | ((background as u8) << 4))
    }
}

/// One cell of the text buffer: an ASCII byte plus its colour attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ScreenChar {
    /// The character shown in the cell.
    pub ascii: u8,
    /// The foreground/background colours of the cell.
    pub color: ColorCode,
}

/// The default cell: a light-gray space on a black background.
pub const BLANK: ScreenChar = ScreenChar {
    ascii: b' ',
    color: ColorCode::new(Color::LightGray, Color::Black),
};

/// The glyph post-03 substitutes for a byte that has no printable ASCII form.
pub const PLACEHOLDER: u8 = 0xfe;

/// Abstract access to the cell grid behind a [`Writer`].
///
/// In post-03 `Buffer` *is* the memory layout; here the name is kept for the
/// trait the writer drives. The production implementation ([`DeviceBuffer`])
/// touches the VGA text buffer with volatile accesses; the unit tests substitute
/// [`TestBuffer`] so the scrolling, wrapping and colour logic can be checked
/// without hardware.
trait Buffer {
    /// Reads the cell at `index` (`row * WIDTH + column`).
    fn read(&self, index: usize) -> ScreenChar;
    /// Writes `value` to the cell at `index` (`row * WIDTH + column`).
    fn write(&mut self, index: usize, value: ScreenChar);
    /// Reports the cursor position to the hardware.
    ///
    /// The default is a no-op: a RAM-backed test buffer has no cursor to move.
    fn set_cursor(&mut self, _index: usize) {}
}

/// Volatile access to the legacy text buffer through the bootloader window.
struct DeviceBuffer {
    /// The first cell, at `physical_memory_offset + 0xb8000`.
    cells: Volatile<ScreenChar>,
}

impl Buffer for DeviceBuffer {
    fn read(&self, index: usize) -> ScreenChar {
        // SAFETY: `cells` points at the 80x25 text buffer and every caller keeps
        // `index < WIDTH * HEIGHT`, so the access stays inside the buffer.
        unsafe { self.cells.offset(index as isize).read() }
    }

    fn write(&mut self, index: usize, value: ScreenChar) {
        // SAFETY: as above; the caller has already checked the bounds.
        unsafe { self.cells.offset(index as isize).write(value) };
    }

    fn set_cursor(&mut self, index: usize) {
        port::set_text_cursor(index);
    }
}

/// A RAM-backed buffer used only by the unit tests.
#[cfg(test)]
struct TestBuffer {
    cells: [ScreenChar; WIDTH * HEIGHT],
    /// The last index passed to [`Buffer::set_cursor`].
    cursor: usize,
}

#[cfg(test)]
impl TestBuffer {
    fn new() -> Self {
        Self {
            cells: [BLANK; WIDTH * HEIGHT],
            cursor: 0,
        }
    }
}

#[cfg(test)]
impl Buffer for TestBuffer {
    fn read(&self, index: usize) -> ScreenChar {
        self.cells[index]
    }

    fn write(&mut self, index: usize, value: ScreenChar) {
        self.cells[index] = value;
    }

    fn set_cursor(&mut self, index: usize) {
        self.cursor = index;
    }
}

/// Cursor position and colour of the active text writer.
struct Writer<B: Buffer> {
    /// The cell grid being written to.
    buffer: B,
    /// Current cursor column.
    column: usize,
    /// Current cursor row.
    row: usize,
    /// Colour applied to newly written characters.
    color: ColorCode,
}

impl<B: Buffer> Writer<B> {
    /// Linear index of the cell at `(row, column)`.
    const fn cell_index(row: usize, column: usize) -> usize {
        row * WIDTH + column
    }

    fn write_cell(&mut self, row: usize, column: usize, screen_char: ScreenChar) {
        self.buffer
            .write(Self::cell_index(row, column), screen_char);
    }

    fn read_cell(&self, row: usize, column: usize) -> ScreenChar {
        self.buffer.read(Self::cell_index(row, column))
    }

    /// Pushes the current cursor position to the hardware.
    fn sync_cursor(&mut self) {
        self.buffer
            .set_cursor(Self::cell_index(self.row, self.column));
    }

    fn clear_row(&mut self, row: usize) {
        for column in 0..WIDTH {
            self.write_cell(row, column, BLANK);
        }
    }

    fn clear(&mut self) {
        for row in 0..HEIGHT {
            self.clear_row(row);
        }
        self.row = 0;
        self.column = 0;
        self.sync_cursor();
    }

    fn new_line(&mut self) {
        self.column = 0;
        if self.row + 1 < HEIGHT {
            self.row += 1;
        } else {
            self.scroll();
        }
        self.sync_cursor();
    }

    fn scroll(&mut self) {
        for row in 1..HEIGHT {
            for column in 0..WIDTH {
                let value = self.read_cell(row, column);
                self.write_cell(row - 1, column, value);
            }
        }
        self.clear_row(HEIGHT - 1);
    }

    fn backspace(&mut self) {
        if self.column > 0 {
            self.column -= 1;
            self.write_cell(self.row, self.column, BLANK);
        }
        self.sync_cursor();
    }

    /// Writes `byte` as a glyph at the cursor, wrapping first if the row is full.
    fn put_char(&mut self, byte: u8) {
        if self.column >= WIDTH {
            self.new_line();
        }
        self.write_cell(
            self.row,
            self.column,
            ScreenChar {
                ascii: byte,
                color: self.color,
            },
        );
        self.column += 1;
        self.sync_cursor();
    }

    /// Writes an ASCII byte, wrapping lines at [`WIDTH`] and scrolling the grid
    /// when the cursor leaves the last row.
    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            b'\r' => {
                self.column = 0;
                self.sync_cursor();
            }
            b'\t' => {
                // Advance to the next multiple of eight columns, like a terminal.
                let next = (self.column / 8 + 1) * 8;
                for _ in self.column..next.min(WIDTH) {
                    self.put_char(b' ');
                }
            }
            0x08 => self.backspace(),
            0x20..=0x7e => self.put_char(byte),
            // post-03 replaces a byte with no printable ASCII glyph by the CP437
            // "filled box" (0xfe) so it stays visible; the remaining control bytes
            // are dropped.
            0x7f..=0xff => self.put_char(PLACEHOLDER),
            _ => {}
        }
    }

    /// Writes every byte of `s`.
    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            self.write_byte(byte);
        }
    }
}

impl<B: Buffer> fmt::Write for Writer<B> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

/// The global writer, filled in by [`init`].
///
/// post-03 declares this as a `lazy_static` that dereferences `0xb8000` on first
/// use; keeping it behind an `Option` means nothing is written before the
/// bootloader has told us where physical memory is mapped.
static WRITER: Mutex<Option<Writer<DeviceBuffer>>> = Mutex::new(None);

/// Points the writer at the text buffer and clears the screen.
///
/// `physical_memory_offset` is the bootloader's physical-memory window, i.e.
/// `BootInfo::physical_memory_offset`.
pub fn init(physical_memory_offset: u64) {
    let mut writer = Writer {
        buffer: DeviceBuffer {
            cells: Volatile::new(
                physical_memory_offset.wrapping_add(BUFFER_PHYS) as *mut ScreenChar
            ),
        },
        column: 0,
        row: 0,
        color: BLANK.color,
    };
    writer.clear();
    *WRITER.lock() = Some(writer);
}

/// Whether [`init`] has run and the buffer is reachable.
#[must_use]
pub fn is_initialized() -> bool {
    WRITER.lock().is_some()
}

/// Writes one byte at the cursor, scrolling when the screen is full.
pub fn write_byte(byte: u8) {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.write_byte(byte);
    }
}

/// Writes every byte of `s`.
pub fn write_str(s: &str) {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.write_string(s);
    }
}

/// Clears the whole screen and moves the cursor to the top-left corner.
pub fn clear() {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.clear();
    }
}

/// Sets the colour applied to newly written characters.
pub fn set_color(color: ColorCode) {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.color = color;
    }
}

/// Sets the foreground colour, keeping the current background.
pub fn set_fg(color: Color) {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.color = writer.color.with_foreground(color);
    }
}

/// Sets the background colour, keeping the current foreground.
pub fn set_bg(color: Color) {
    if let Some(writer) = WRITER.lock().as_mut() {
        writer.color = writer.color.with_background(color);
    }
}

/// The colour applied to newly written characters.
#[must_use]
pub fn color() -> ColorCode {
    WRITER
        .lock()
        .as_ref()
        .map_or(BLANK.color, |writer| writer.color)
}

/// Reads a cell back out of the buffer.
///
/// Returns a blank cell when `row`/`column` are out of bounds or the buffer has
/// not been initialized.
#[must_use]
pub fn read_screen_char(row: usize, column: usize) -> ScreenChar {
    if row >= HEIGHT || column >= WIDTH {
        return BLANK;
    }
    match WRITER.lock().as_ref() {
        Some(writer) => writer.read_cell(row, column),
        None => BLANK,
    }
}

/// Prints the given formatted string to the VGA text buffer through the global
/// [`WRITER`] — post-03's `_print` — and mirrors it to COM1.
///
/// This is the entry point behind the [`print!`](crate::print) and
/// [`println!`](crate::println) macros. The serial copy is what makes a headless
/// QEMU run show the same text; a write before [`init`] still reaches COM1.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    // Mirror to COM1 first so a crash in the text-buffer path still leaves a log.
    crate::serial::print_args(args);
    if let Some(writer) = WRITER.lock().as_mut() {
        // Writing to the text buffer cannot fail.
        let _ = fmt::Write::write_fmt(writer, args);
    }
}

// Unit tests for the writer logic. They run on the host through the crate's own
// `cargo test` (the crate is a normal `std` crate under `cfg(test)`), driving a
// RAM-backed [`TestBuffer`] instead of a live text-mode device.
#[cfg(test)]
mod tests {
    use super::{BLANK, Color, ColorCode, HEIGHT, PLACEHOLDER, TestBuffer, WIDTH, Writer};

    /// A cleared writer over a RAM buffer.
    fn writer() -> Writer<TestBuffer> {
        let mut writer = Writer {
            buffer: TestBuffer::new(),
            column: 0,
            row: 0,
            color: BLANK.color,
        };
        writer.clear();
        writer
    }

    /// The ASCII byte at `(row, column)`.
    fn char_at(writer: &Writer<TestBuffer>, row: usize, column: usize) -> u8 {
        writer.read_cell(row, column).ascii
    }

    #[test]
    fn color_code_packs_foreground_low_background_high() {
        // The attribute byte is `background << 4 | foreground`.
        assert_eq!(ColorCode::new(Color::LightGray, Color::Black).raw(), 0x07);
        assert_eq!(ColorCode::new(Color::White, Color::Blue).raw(), 0x1f);
        assert_eq!(ColorCode::new(Color::Black, Color::White).raw(), 0xf0);
    }

    #[test]
    fn color_code_round_trips_its_halves() {
        let code = ColorCode::new(Color::Yellow, Color::Red);
        assert_eq!(code.foreground(), Color::Yellow);
        assert_eq!(code.background(), Color::Red);
        assert_eq!(code.with_foreground(Color::Cyan).foreground(), Color::Cyan);
        assert_eq!(code.with_background(Color::Blue).background(), Color::Blue);
        // The untouched half survives.
        assert_eq!(code.with_foreground(Color::Cyan).background(), Color::Red);
    }

    #[test]
    fn from_rgb_snaps_to_the_nearest_palette_entry() {
        assert_eq!(Color::from_rgb(0, 0, 0), Color::Black);
        assert_eq!(Color::from_rgb(255, 255, 255), Color::White);
        // A slightly off white is still white.
        assert_eq!(Color::from_rgb(250, 250, 240), Color::White);
        // Anything dark and warm lands on brown or red.
        assert_eq!(Color::from_rgb(170, 85, 0), Color::Brown);
    }

    #[test]
    fn writes_characters_and_advances_the_cursor() {
        let mut writer = writer();
        writer.write_string("GTos");
        assert_eq!(char_at(&writer, 0, 0), b'G');
        assert_eq!(char_at(&writer, 0, 3), b's');
        assert_eq!(writer.row, 0);
        assert_eq!(writer.column, 4);
    }

    #[test]
    fn written_characters_carry_the_active_colour() {
        let mut writer = writer();
        writer.color = ColorCode::new(Color::Green, Color::Blue);
        writer.write_string("x");
        let cell = writer.read_cell(0, 0);
        assert_eq!(cell.color.foreground(), Color::Green);
        assert_eq!(cell.color.background(), Color::Blue);
    }

    #[test]
    fn wraps_to_the_next_line_when_a_row_is_full() {
        let mut writer = writer();
        // Exactly WIDTH characters fill the first row; the next one wraps.
        for _ in 0..WIDTH {
            writer.write_byte(b'x');
        }
        writer.write_byte(b'y');
        assert_eq!(char_at(&writer, 0, WIDTH - 1), b'x');
        assert_eq!(char_at(&writer, 1, 0), b'y');
        assert_eq!(writer.row, 1);
        assert_eq!(writer.column, 1);
    }

    #[test]
    fn scrolls_when_the_screen_is_full() {
        let mut writer = writer();
        // HEIGHT newline-terminated lines fill the screen; the final newline
        // scrolls it, so the top row becomes the second line.
        for _ in 0..HEIGHT {
            writer.write_string("0123456789\n");
        }
        writer.write_string("last");
        assert_eq!(char_at(&writer, 0, 0), b'0');
        assert_eq!(char_at(&writer, HEIGHT - 1, 0), b'l');
        assert_eq!(char_at(&writer, HEIGHT - 1, 3), b't');
    }

    #[test]
    fn clear_resets_the_cursor_and_the_cells() {
        let mut writer = writer();
        writer.write_string("hello");
        writer.clear();
        assert_eq!(writer.row, 0);
        assert_eq!(writer.column, 0);
        assert_eq!(char_at(&writer, 0, 0), b' ');
    }

    #[test]
    fn backspace_erases_the_previous_cell() {
        let mut writer = writer();
        writer.write_string("ab");
        writer.write_byte(0x08);
        assert_eq!(writer.column, 1);
        assert_eq!(char_at(&writer, 0, 1), b' ');
    }

    #[test]
    fn backspace_at_the_start_of_a_line_is_a_no_op() {
        let mut writer = writer();
        writer.write_byte(0x08);
        assert_eq!(writer.column, 0);
        assert_eq!(writer.row, 0);
    }

    #[test]
    fn carriage_return_moves_to_column_zero() {
        let mut writer = writer();
        writer.write_string("ab");
        writer.write_byte(b'\r');
        assert_eq!(writer.column, 0);
    }

    #[test]
    fn tab_advances_to_the_next_stop() {
        let mut writer = writer();
        writer.write_byte(b'\t');
        assert_eq!(writer.column, 8);
        writer.write_string("x");
        writer.write_byte(b'\t');
        assert_eq!(writer.column, 16);
    }

    #[test]
    fn cursor_is_reported_to_the_hardware() {
        let mut writer = writer();
        writer.write_string("abc");
        // Three cells written: the cursor sits on the fourth.
        assert_eq!(writer.buffer.cursor, 3);
        writer.clear();
        assert_eq!(writer.buffer.cursor, 0);
    }

    #[test]
    fn read_out_of_bounds_is_blank() {
        // Out-of-range reads short-circuit to a blank cell without touching the
        // buffer, so they are safe before `init` has run.
        assert_eq!(super::read_screen_char(HEIGHT, 0).ascii, b' ');
        assert_eq!(super::read_screen_char(0, WIDTH).ascii, b' ');
    }

    #[test]
    fn non_printable_bytes_show_the_box_placeholder() {
        // post-03's `write_string` maps a byte with no printable glyph to 0xfe.
        // It iterates bytes, so a multi-byte character becomes one box per byte.
        let mut writer = writer();
        writer.write_string("\u{e9}"); // 'é' is two UTF-8 bytes: 0xc3 0xa9
        assert_eq!(char_at(&writer, 0, 0), PLACEHOLDER);
        assert_eq!(char_at(&writer, 0, 1), PLACEHOLDER);
        assert_eq!(writer.column, 2);
        // Control bytes still vanish.
        writer.write_byte(0x01);
        assert_eq!(writer.column, 2);
    }
}
