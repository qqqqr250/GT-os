//! A small clean-room widget toolkit for ring-3 `GTos` programs.
//!
//! It deliberately stays at the foundation layer: a clipping pixel canvas, an
//! 8x8 bitmap text renderer, geometry helpers, a few widgets and a window
//! surface backed by the compositor ABI. Every primitive is plain Rust; the
//! only host services used are `fbinfo`, `wincreate`, `wincommit`, `winpresent`
//! and `windestroy` from `gtos-sys`. There is no HTML, JavaScript, foreign UI
//! toolkit or compiled-in C code here.
//!
//! A typical program creates a [`Surface`], wraps its [`Canvas`], builds a
//! [`Ui`] with a [`Label`] and [`Button`], then dispatches [`Event`] values
//! translated from [`gtos_sys::InputEvent`].

#![no_std]
// Colours and geometry cross a C-style integer ABI; truncation and sign changes
// are bounded by the display dimensions that `Canvas::new` validates.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

extern crate alloc;

pub mod font;
pub mod shell;

mod theme;
mod view;

pub use theme::{Resolved, Style, Theme};
pub use view::{View, ViewBuilder};

use alloc::vec;
use alloc::vec::Vec;
use gtos_sys::{
    INPUT_KEY_DOWN, INPUT_KEY_UP, INPUT_MOUSE_BUTTON, INPUT_MOUSE_MOVE, InputEvent, KEY_DOWN,
    KEY_END, KEY_HOME, KEY_LEFT, KEY_PAGE_DOWN, KEY_PAGE_UP, KEY_RIGHT, KEY_UP, WindowCreate,
};

/// Width of one unscaled font glyph in pixels.
pub const GLYPH_WIDTH: usize = font::FONT_WIDTH;
/// Height of one unscaled font glyph in pixels.
pub const GLYPH_HEIGHT: usize = font::FONT_HEIGHT;

/// Carriage return key code.
const CR: u32 = 13;
/// Line feed key code.
const LF: u32 = 10;

/// A 24-bit `0xRRGGBB` colour, converted to the display format at draw time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Color(pub u32);

impl Color {
    /// Builds a colour from eight-bit channels.
    #[must_use]
    pub const fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }

    /// Splits the colour into its eight-bit channels.
    #[must_use]
    pub const fn rgb(self) -> (u8, u8, u8) {
        ((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
    }

    /// The bytes the kernel expects for one pixel in `format`.
    #[must_use]
    pub const fn bytes(self, format: PixelFormat) -> [u8; 4] {
        let (red, green, blue) = self.rgb();
        match format {
            PixelFormat::Rgb => [red, green, blue, 0xff],
            PixelFormat::Bgr => [blue, green, red, 0xff],
            PixelFormat::Monochrome => {
                // These weights sum to 256, so the shift always fits in a byte.
                let luma = ((red as u32) * 77 + (green as u32) * 150 + (blue as u32) * 29) >> 8;
                [luma as u8, luma as u8, luma as u8, 0xff]
            }
        }
    }
}

/// Dark window background, matching the kernel console palette.
pub const BACKGROUND: Color = Color(0x00_0b_0e_14);
/// Primary text and border colour.
pub const FOREGROUND: Color = Color(0x00_d8_de_e9);
/// Muted secondary text.
pub const DIM: Color = Color(0x00_70_7a_8a);
/// Normal widget face.
pub const SURFACE: Color = Color(0x00_1c_27_38);
/// Widget face while the pointer is over it.
pub const SURFACE_HOVER: Color = Color(0x00_2a_3b_52);
/// Widget face while it is pressed.
pub const SURFACE_PRESSED: Color = Color(0x00_3f_6b_86);
/// Accent used for selected controls.
pub const ACCENT: Color = Color(0x00_4f_c1_ff);

/// Pixel layout reported by `fbinfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Unknown or monochrome; channels are converted to perceived luminance.
    Monochrome,
    /// Red, green, blue byte order.
    Rgb,
    /// Blue, green, red byte order.
    Bgr,
}

impl PixelFormat {
    /// Converts the kernel's numeric `pixel_format` field.
    #[must_use]
    pub const fn from_abi(value: u32) -> Self {
        match value {
            1 => Self::Rgb,
            2 => Self::Bgr,
            _ => Self::Monochrome,
        }
    }
}

/// An integer point in window-local pixel coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    /// Horizontal coordinate.
    pub x: i32,
    /// Vertical coordinate.
    pub y: i32,
}

impl Point {
    /// Creates a point.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned pixel rectangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// Creates a rectangle from its origin and size.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// An empty rectangle, useful for disjoint clip regions.
    #[must_use]
    pub const fn empty() -> Self {
        Self::new(0, 0, 0, 0)
    }

    /// Exclusive right edge.
    #[must_use]
    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// Exclusive bottom edge.
    #[must_use]
    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    /// Whether the point lies inside this rectangle.
    #[must_use]
    pub fn contains(self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    /// The centre point, rounding down for odd sizes.
    #[must_use]
    pub fn center(self) -> Point {
        Point::new(
            self.x + (self.width / 2) as i32,
            self.y + (self.height / 2) as i32,
        )
    }

    /// The overlap of two rectangles, if any.
    #[must_use]
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            return None;
        }
        Some(Self::new(x, y, (right - x) as u32, (bottom - y) as u32))
    }

    /// Shrinks the rectangle on every side without changing its centre.
    #[must_use]
    pub fn inset(self, amount: u32) -> Self {
        Self::new(
            self.x.saturating_add(amount as i32),
            self.y.saturating_add(amount as i32),
            self.width.saturating_sub(amount.saturating_mul(2)),
            self.height.saturating_sub(amount.saturating_mul(2)),
        )
    }
}

/// A clipped drawing target over tightly packed window pixels.
pub struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: i32,
    height: i32,
    clip: Rect,
    stride: usize,
    bytes_per_pixel: usize,
    format: PixelFormat,
}

impl<'a> Canvas<'a> {
    /// Wraps a pixel buffer whose rows are `width * bytes_per_pixel` bytes apart.
    ///
    /// Returns `None` when the dimensions overflow or the buffer is too small.
    #[must_use]
    pub fn new(
        pixels: &'a mut [u8],
        width: usize,
        height: usize,
        bytes_per_pixel: usize,
        format: PixelFormat,
    ) -> Option<Self> {
        if width == 0 || height == 0 || !(1..=4).contains(&bytes_per_pixel) {
            return None;
        }
        let stride = width.checked_mul(bytes_per_pixel)?;
        if height.checked_mul(stride)? > pixels.len() {
            return None;
        }
        Some(Self {
            pixels,
            width: width as i32,
            height: height as i32,
            clip: Rect::new(0, 0, width as u32, height as u32),
            stride,
            bytes_per_pixel,
            format,
        })
    }

    /// The full drawable area of this canvas.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width as u32, self.height as u32)
    }

    /// The area drawing is currently restricted to.
    #[must_use]
    pub const fn clip(&self) -> Rect {
        self.clip
    }

    /// Borrows a subregion of this canvas for clipped drawing.
    ///
    /// Coordinates stay relative to the original buffer; only writes are
    /// restricted to the intersection of `rect` and the current clip.
    #[must_use]
    pub fn clipped(&mut self, rect: Rect) -> Canvas<'_> {
        Canvas {
            pixels: self.pixels,
            width: self.width,
            height: self.height,
            clip: self.clip.intersection(rect).unwrap_or_else(Rect::empty),
            stride: self.stride,
            bytes_per_pixel: self.bytes_per_pixel,
            format: self.format,
        }
    }

    /// The pixel layout of the backing buffer.
    #[must_use]
    pub const fn format(&self) -> PixelFormat {
        self.format
    }

    fn offset(&self, x: i32, y: i32) -> usize {
        y as usize * self.stride + x as usize * self.bytes_per_pixel
    }

    fn put(&mut self, point: Point, color: Color, clip: Rect) {
        if !self.bounds().contains(point) || !self.clip.contains(point) || !clip.contains(point) {
            return;
        }
        let offset = self.offset(point.x, point.y);
        let bytes = color.bytes(self.format);
        let count = self.bytes_per_pixel.min(bytes.len());
        self.pixels[offset..offset + count].copy_from_slice(&bytes[..count]);
    }

    /// Fills a rectangle, clipped to the canvas.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.fill_rect_clipped(rect, color, self.clip);
    }

    /// Fills a rectangle, clipped to both the canvas and `clip`.
    pub fn fill_rect_clipped(&mut self, rect: Rect, color: Color, clip: Rect) {
        let Some(area) = rect
            .intersection(clip)
            .and_then(|overlap| overlap.intersection(self.clip))
            .and_then(|overlap| overlap.intersection(self.bounds()))
        else {
            return;
        };
        let bytes = color.bytes(self.format);
        let row_len = area.width as usize * self.bytes_per_pixel;
        for y in area.y..area.bottom() {
            let start = self.offset(area.x, y);
            for pixel in self.pixels[start..start + row_len].chunks_exact_mut(self.bytes_per_pixel)
            {
                pixel.copy_from_slice(&bytes[..self.bytes_per_pixel]);
            }
        }
    }

    /// Draws a one-pixel Bresenham line, clipped to the canvas.
    pub fn draw_line(&mut self, from: Point, to: Point, color: Color) {
        self.draw_line_clipped(from, to, color, self.clip);
    }

    /// Draws a line clipped to both the canvas and `clip`.
    pub fn draw_line_clipped(&mut self, from: Point, to: Point, color: Color, clip: Rect) {
        let delta_x = (to.x - from.x).abs();
        let delta_y = (to.y - from.y).abs();
        let step_x = if from.x < to.x { 1 } else { -1 };
        let step_y = if from.y < to.y { 1 } else { -1 };
        let mut error = delta_x - delta_y;
        let (mut x, mut y) = (from.x, from.y);
        loop {
            self.put(Point::new(x, y), color, clip);
            if x == to.x && y == to.y {
                break;
            }
            let doubled = error * 2;
            if doubled > -delta_y {
                error -= delta_y;
                x += step_x;
            }
            if doubled < delta_x {
                error += delta_x;
                y += step_y;
            }
        }
    }

    /// Draws NUL-free ASCII text with the unscaled bitmap font.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &[u8], color: Color) {
        self.draw_text_scaled(x, y, text, color, 1, self.clip);
    }

    /// Draws text at an integer scale, clipped to both the canvas and `clip`.
    pub fn draw_text_scaled(
        &mut self,
        x: i32,
        y: i32,
        text: &[u8],
        color: Color,
        scale: usize,
        clip: Rect,
    ) {
        if scale == 0 {
            return;
        }
        let scaled = scale as i32;
        let mut cursor = x;
        for &byte in text {
            let glyph = &font::FONT[(byte & 0x7f) as usize];
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..font::FONT_WIDTH {
                    if bits & (1u8 << (font::FONT_WIDTH - 1 - column)) == 0 {
                        continue;
                    }
                    let left = cursor + column as i32 * scaled;
                    let top = y + row as i32 * scaled;
                    for dy in 0..scaled {
                        for dx in 0..scaled {
                            self.put(Point::new(left + dx, top + dy), color, clip);
                        }
                    }
                }
            }
            cursor += font::FONT_WIDTH as i32 * scaled;
            if cursor > clip.right() {
                break;
            }
        }
    }
}

/// A composited window plus its guest-side pixel buffer.
///
/// Dropping the surface releases the window, so callers cannot leak compositor
/// resources on an early return.
pub struct Surface {
    id: u64,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    format: PixelFormat,
    pixels: Vec<u8>,
}

impl Surface {
    /// Creates a titled window and a zero-filled backing image of the given size.
    #[must_use]
    pub fn create(
        info: gtos_sys::FramebufferInfo,
        x: u32,
        y: u32,
        width: usize,
        height: usize,
        layer: u32,
        title: &[u8],
    ) -> Option<Self> {
        let bytes_per_pixel = usize::try_from(info.bytes_per_pixel).ok()?;
        if !(1..=4).contains(&bytes_per_pixel) || width == 0 || height == 0 {
            return None;
        }
        let row_bytes = width.checked_mul(bytes_per_pixel)?;
        let byte_len = row_bytes.checked_mul(height)?;
        let id = gtos_sys::window_create(WindowCreate {
            x,
            y,
            width: u32::try_from(width).ok()?,
            height: u32::try_from(height).ok()?,
            layer,
        })?;
        let pixels = vec![0; byte_len];
        let surface = Self {
            id,
            width,
            height,
            bytes_per_pixel,
            format: PixelFormat::from_abi(info.pixel_format),
            pixels,
        };
        if !gtos_sys::window_set_title(id, title) {
            return None;
        }
        Some(surface)
    }

    /// The compositor's one-based window identifier.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Borrows the backing image as a drawable canvas.
    ///
    /// # Panics
    ///
    /// Never in practice: `create` validated the geometry and buffer length.
    #[must_use]
    pub fn canvas(&mut self) -> Canvas<'_> {
        Canvas::new(
            &mut self.pixels,
            self.width,
            self.height,
            self.bytes_per_pixel,
            self.format,
        )
        .expect("validated window geometry")
    }

    /// Copies the backing image to the compositor.
    #[must_use]
    pub fn commit(&self) -> bool {
        gtos_sys::window_commit(self.id, &self.pixels).is_some()
    }

    /// Commits the backing image and refreshes the display.
    #[must_use]
    pub fn present(&self) -> bool {
        self.commit() && gtos_sys::window_present()
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        let _ = gtos_sys::window_destroy(self.id);
    }
}

/// A toolkit-level input event, translated from the kernel's stable ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The pointer moved; `buttons` is the current button mask.
    PointerMove {
        /// Horizontal window-local coordinate.
        x: i32,
        /// Vertical window-local coordinate.
        y: i32,
        /// Bitmask of pressed buttons.
        buttons: u32,
    },
    /// A pointer button went down.
    PointerDown {
        /// Horizontal window-local coordinate.
        x: i32,
        /// Vertical window-local coordinate.
        y: i32,
        /// Button index: zero for left, one for right, two for middle.
        button: u32,
    },
    /// A pointer button went up.
    PointerUp {
        /// Horizontal window-local coordinate.
        x: i32,
        /// Vertical window-local coordinate.
        y: i32,
        /// Button index: zero for left, one for right, two for middle.
        button: u32,
    },
    /// A scroll gesture; positive `delta` moves content upward.
    ///
    /// The kernel has no wheel event yet, so programs synthesise this from
    /// key bindings or scrollbar interaction and feed it to containers.
    Scroll {
        /// Horizontal window-local coordinate.
        x: i32,
        /// Vertical window-local coordinate.
        y: i32,
        /// Signed number of pixels to scroll.
        delta: i32,
    },
    /// A key went down.
    KeyDown {
        /// Translated ASCII or control code.
        code: u32,
        /// Modifier bitmask.
        modifiers: u32,
    },
    /// A key went up.
    KeyUp {
        /// Translated ASCII or control code.
        code: u32,
        /// Modifier bitmask.
        modifiers: u32,
    },
    /// An event the toolkit does not route to widgets.
    Ignored,
}

impl Event {
    /// Translates one kernel event.
    #[must_use]
    pub fn from_host(event: InputEvent) -> Self {
        match event.kind {
            INPUT_MOUSE_MOVE => Self::PointerMove {
                x: event.x,
                y: event.y,
                buttons: event.value,
            },
            INPUT_MOUSE_BUTTON => {
                let pressed = event.value & (1u32 << event.code.min(31)) != 0;
                let x = event.x;
                let y = event.y;
                let button = event.code;
                if pressed {
                    Self::PointerDown { x, y, button }
                } else {
                    Self::PointerUp { x, y, button }
                }
            }
            INPUT_KEY_DOWN => Self::KeyDown {
                code: event.code,
                modifiers: event.modifiers,
            },
            INPUT_KEY_UP => Self::KeyUp {
                code: event.code,
                modifiers: event.modifiers,
            },
            // Serial bytes and unknown kinds are not routed to widgets.
            _ => Self::Ignored,
        }
    }

    /// The pointer position carried by pointer events.
    #[must_use]
    pub const fn position(self) -> Option<Point> {
        match self {
            Self::PointerMove { x, y, .. }
            | Self::PointerDown { x, y, .. }
            | Self::PointerUp { x, y, .. }
            | Self::Scroll { x, y, .. } => Some(Point::new(x, y)),
            Self::KeyDown { .. } | Self::KeyUp { .. } | Self::Ignored => None,
        }
    }
}

/// A behaviour shared by all drawn controls.
pub trait Widget {
    /// The widget's current placement.
    fn rect(&self) -> Rect;

    /// Assigns the area the widget should occupy.
    fn layout(&mut self, area: Rect);

    /// Draws the widget into a canvas.
    fn paint(&self, canvas: &mut Canvas<'_>);

    /// Handles one event, returning whether it was consumed.
    fn event(&mut self, event: Event) -> bool {
        let _ = event;
        false
    }

    /// Whether this widget participates in keyboard focus routing.
    fn accepts_focus(&self) -> bool {
        false
    }

    /// Called when this widget gains or loses keyboard focus.
    fn set_focus(&mut self, focused: bool) {
        let _ = focused;
    }

    /// A stable role name for accessibility introspection.
    fn accessible_role(&self) -> &'static str {
        "widget"
    }

    /// A short human-readable label, if this widget has one.
    fn accessible_label(&self) -> Option<&[u8]> {
        None
    }
}

/// One accessibility record gathered from a widget tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accessible<'a> {
    /// The widget role, for example `button` or `text field`.
    pub role: &'static str,
    /// The widget's label or current value.
    pub label: &'a [u8],
}

/// A single line of static text.
pub struct Label<'a> {
    rect: Rect,
    text: &'a [u8],
    color: Color,
}

impl<'a> Label<'a> {
    /// Creates a label; call [`Widget::layout`] before painting it.
    #[must_use]
    pub const fn new(text: &'a [u8], color: Color) -> Self {
        Self {
            rect: Rect::new(0, 0, 0, 0),
            text,
            color,
        }
    }
}

impl Widget for Label<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let text_y =
            self.rect.y + (self.rect.height / 2).saturating_sub(GLYPH_HEIGHT as u32) as i32;
        canvas.draw_text_scaled(
            self.rect.x + 2,
            text_y.max(self.rect.y),
            self.text,
            self.color,
            1,
            self.rect,
        );
    }

    fn accessible_role(&self) -> &'static str {
        "text"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        Some(self.text)
    }
}

/// A read-only multi-line text block that can scroll by whole lines.
pub struct TextView<'a> {
    rect: Rect,
    text: &'a [u8],
    line: usize,
    theme: Theme,
    style: Style,
}

impl<'a> TextView<'a> {
    /// Creates a text view; call [`Widget::layout`] before painting it.
    #[must_use]
    pub const fn new(text: &'a [u8]) -> Self {
        Self {
            rect: Rect::new(0, 0, 0, 0),
            text,
            line: 0,
            theme: Theme::dark(),
            style: Style::new(),
        }
    }

    /// Creates a text view whose colours come from `theme` and `style`.
    #[must_use]
    pub const fn styled(text: &'a [u8], theme: Theme, style: Style) -> Self {
        Self {
            theme,
            style,
            ..Self::new(text)
        }
    }

    /// The full text, including newline separators.
    #[must_use]
    pub const fn text(&self) -> &'a [u8] {
        self.text
    }

    /// The number of logical lines in the text.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.text.split(|&byte| byte == LF as u8).count()
    }

    /// The index of the first visible line.
    #[must_use]
    pub const fn line_offset(&self) -> usize {
        self.line
    }

    /// Clamps and sets the first visible line.
    pub fn set_line_offset(&mut self, line: usize) {
        self.line = line.min(self.line_count().saturating_sub(1));
    }

    /// Scrolls by whole lines; one scroll event advances one line.
    pub fn scroll_by_lines(&mut self, lines: i32) {
        if lines >= 0 {
            self.line = self.line.saturating_add(lines as usize);
        } else {
            self.line = self.line.saturating_sub(lines.unsigned_abs() as usize);
        }
        self.set_line_offset(self.line);
    }
}

impl Widget for TextView<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        self.set_line_offset(self.line);
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.background, self.rect);
        let inner = self.rect.inset(2);
        let mut y = inner.y;
        for line in self.text.split(|&byte| byte == LF as u8).skip(self.line) {
            if y + GLYPH_HEIGHT as i32 > inner.bottom() {
                break;
            }
            canvas.draw_text_scaled(inner.x, y, line, colors.foreground, 1, inner);
            y += GLYPH_HEIGHT as i32;
        }
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Scroll { x, y, delta } if self.rect.contains(Point::new(x, y)) && delta != 0 => {
                self.scroll_by_lines(if delta > 0 { 1 } else { -1 });
                true
            }
            _ => false,
        }
    }

    fn accessible_role(&self) -> &'static str {
        "text view"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        Some(self.text)
    }
}

/// A push button with hover, pressed, focused and clicked states.
// The four flags are independent visual/interaction states, not a mode.
#[allow(clippy::struct_excessive_bools)]
pub struct Button<'a> {
    rect: Rect,
    label: Label<'a>,
    pressed: bool,
    hovered: bool,
    focused: bool,
    clicked: bool,
    theme: Theme,
    style: Style,
}

impl<'a> Button<'a> {
    /// Creates a button; call [`Widget::layout`] before painting it.
    #[must_use]
    pub const fn new(text: &'a [u8]) -> Self {
        Self {
            rect: Rect::new(0, 0, 0, 0),
            label: Label::new(text, FOREGROUND),
            pressed: false,
            hovered: false,
            focused: false,
            clicked: false,
            theme: Theme::dark(),
            style: Style::new(),
        }
    }

    /// Creates a button whose colours come from `theme` and `style`.
    #[must_use]
    pub const fn styled(text: &'a [u8], theme: Theme, style: Style) -> Self {
        let foreground = match style.foreground {
            Some(color) => color,
            None => theme.foreground,
        };
        Self {
            rect: Rect::new(0, 0, 0, 0),
            label: Label::new(text, foreground),
            pressed: false,
            hovered: false,
            focused: false,
            clicked: false,
            theme,
            style,
        }
    }

    /// Whether the pointer is currently over the button.
    #[must_use]
    pub const fn is_hovered(&self) -> bool {
        self.hovered
    }

    /// Whether the button currently owns keyboard focus.
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// Whether the button is currently held down.
    #[must_use]
    pub const fn is_pressed(&self) -> bool {
        self.pressed
    }

    /// Whether the last completed press released inside the button.
    ///
    /// The flag stays set until the next press starts, so callers can observe
    /// it after dispatching an event batch.
    #[must_use]
    pub const fn was_clicked(&self) -> bool {
        self.clicked
    }
}

impl Widget for Button<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        let face = if self.pressed {
            colors.surface_pressed
        } else if self.hovered {
            colors.surface_hover
        } else {
            colors.surface
        };
        let border = if self.focused {
            colors.accent
        } else {
            colors.border
        };
        canvas.fill_rect_clipped(self.rect, face, self.rect);
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
            border,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            border,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.y, 1, self.rect.height),
            border,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.right() - 1, self.rect.y, 1, self.rect.height),
            border,
            self.rect,
        );
        let label_area = self.rect.inset(3);
        let text_y =
            label_area.y + label_area.height.saturating_sub(GLYPH_HEIGHT as u32) as i32 / 2;
        canvas.draw_text_scaled(
            label_area.x + 2,
            text_y.max(label_area.y),
            self.label.text,
            self.label.color,
            1,
            label_area,
        );
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerMove { x, y, .. } => {
                self.hovered = self.rect.contains(Point::new(x, y));
                self.hovered
            }
            Event::PointerDown { x, y, button } => {
                if button != 0 || !self.rect.contains(Point::new(x, y)) {
                    return false;
                }
                self.pressed = true;
                self.clicked = false;
                true
            }
            Event::PointerUp { x, y, button } => {
                if !self.pressed || button != 0 {
                    return false;
                }
                self.pressed = false;
                self.clicked = self.rect.contains(Point::new(x, y));
                true
            }
            Event::Scroll { .. } | Event::KeyDown { .. } | Event::KeyUp { .. } | Event::Ignored => {
                false
            }
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn accessible_role(&self) -> &'static str {
        "button"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        Some(self.label.text)
    }
}

/// A single-line ASCII text field with keyboard focus and a visible caret.
pub struct TextInput {
    rect: Rect,
    bytes: Vec<u8>,
    max_len: usize,
    focused: bool,
    theme: Theme,
    style: Style,
}

impl TextInput {
    /// Creates an empty field holding at most `max_len` bytes.
    #[must_use]
    pub fn new(max_len: usize) -> Self {
        Self {
            rect: Rect::new(0, 0, 0, 0),
            bytes: Vec::with_capacity(max_len),
            max_len,
            focused: false,
            theme: Theme::dark(),
            style: Style::new(),
        }
    }

    /// Creates a themed text field.
    #[must_use]
    pub fn styled(max_len: usize, theme: Theme, style: Style) -> Self {
        Self {
            theme,
            style,
            ..Self::new(max_len)
        }
    }

    /// The current text bytes.
    #[must_use]
    pub fn text(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether the field currently owns keyboard focus.
    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    /// Removes all text.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

impl Widget for TextInput {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.surface, self.rect);
        let border = if self.focused {
            colors.accent
        } else {
            colors.border
        };
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
            border,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            border,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.y, 1, self.rect.height),
            border,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.right() - 1, self.rect.y, 1, self.rect.height),
            border,
            self.rect,
        );
        let inner = self.rect.inset(3);
        canvas.draw_text_scaled(
            inner.x + 1,
            inner.y + 1,
            &self.bytes,
            colors.foreground,
            1,
            inner,
        );
        if self.focused {
            let caret_x = inner.x + 1 + (self.bytes.len() * GLYPH_WIDTH) as i32;
            canvas.fill_rect_clipped(
                Rect::new(caret_x, inner.y + 1, 1, GLYPH_HEIGHT as u32),
                colors.accent,
                self.rect,
            );
        }
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerDown { x, y, button } => {
                if button == 0 && self.rect.contains(Point::new(x, y)) {
                    self.focused = true;
                    return true;
                }
                false
            }
            Event::KeyDown { code, .. } => match code {
                0x08 | 0x7f => {
                    self.bytes.pop();
                    true
                }
                CR | LF => true,
                0x20..=0x7e if self.bytes.len() < self.max_len => {
                    self.bytes.push(code as u8);
                    true
                }
                _ => false,
            },
            Event::Scroll { .. }
            | Event::PointerMove { .. }
            | Event::PointerUp { .. }
            | Event::KeyUp { .. }
            | Event::Ignored => false,
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn accessible_role(&self) -> &'static str {
        "text field"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        Some(&self.bytes)
    }
}

/// A multi-line ASCII editor with a caret, cursor navigation and line scrolling.
pub struct TextArea {
    rect: Rect,
    lines: Vec<Vec<u8>>,
    cursor_line: usize,
    cursor_column: usize,
    first_line: usize,
    max_bytes: usize,
    focused: bool,
    theme: Theme,
    style: Style,
}

impl TextArea {
    /// Creates an empty editor holding at most `max_bytes` bytes, separators included.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            rect: Rect::empty(),
            lines: vec![Vec::new()],
            cursor_line: 0,
            cursor_column: 0,
            first_line: 0,
            max_bytes,
            focused: false,
            theme: Theme::dark(),
            style: Style::new(),
        }
    }

    /// Creates a themed editor.
    #[must_use]
    pub fn styled(max_bytes: usize, theme: Theme, style: Style) -> Self {
        Self {
            theme,
            style,
            ..Self::new(max_bytes)
        }
    }

    /// Returns all lines joined by newline bytes.
    #[must_use]
    pub fn text(&self) -> Vec<u8> {
        let mut joined = Vec::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                joined.push(LF as u8);
            }
            joined.extend_from_slice(line);
        }
        joined
    }

    /// The current `(line, column)` cursor position.
    #[must_use]
    pub const fn cursor(&self) -> (usize, usize) {
        (self.cursor_line, self.cursor_column)
    }

    /// The index of the first visible line.
    #[must_use]
    pub const fn first_line(&self) -> usize {
        self.first_line
    }

    /// The number of logical lines.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn byte_count(&self) -> usize {
        self.lines.iter().map(Vec::len).sum::<usize>() + self.lines.len().saturating_sub(1)
    }

    fn visible_lines(&self) -> usize {
        (self.rect.height.saturating_sub(7) as usize / GLYPH_HEIGHT).max(1)
    }

    fn ensure_visible(&mut self) {
        self.first_line = self.first_line.min(self.cursor_line);
        let last_visible = self.first_line + self.visible_lines();
        if self.cursor_line >= last_visible {
            self.first_line = self.cursor_line + 1 - self.visible_lines();
        }
    }

    fn clamp_cursor(&mut self) {
        self.cursor_line = self.cursor_line.min(self.lines.len() - 1);
        self.cursor_column = self.cursor_column.min(self.lines[self.cursor_line].len());
        self.ensure_visible();
    }

    fn insert(&mut self, byte: u8) {
        if self.byte_count() >= self.max_bytes {
            return;
        }
        let column = self.cursor_column.min(self.lines[self.cursor_line].len());
        self.lines[self.cursor_line].insert(column, byte);
        self.cursor_column = column + 1;
        self.ensure_visible();
    }

    fn insert_newline(&mut self) {
        if self.byte_count() >= self.max_bytes {
            return;
        }
        let column = self.cursor_column.min(self.lines[self.cursor_line].len());
        let rest = self.lines[self.cursor_line].split_off(column);
        self.lines.insert(self.cursor_line + 1, rest);
        self.cursor_line += 1;
        self.cursor_column = 0;
        self.ensure_visible();
    }

    fn backspace(&mut self) {
        if self.cursor_column > 0 {
            self.cursor_column -= 1;
            self.lines[self.cursor_line].remove(self.cursor_column);
        } else if self.cursor_line > 0 {
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_column = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].extend_from_slice(&current);
        }
        self.clamp_cursor();
    }

    fn delete(&mut self) {
        let len = self.lines[self.cursor_line].len();
        if self.cursor_column < len {
            self.lines[self.cursor_line].remove(self.cursor_column);
        } else if self.cursor_line + 1 < self.lines.len() {
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].extend_from_slice(&next);
        }
        self.clamp_cursor();
    }

    fn move_cursor(&mut self, code: u32) {
        let page = self.visible_lines();
        match code {
            KEY_LEFT => {
                if self.cursor_column > 0 {
                    self.cursor_column -= 1;
                } else if self.cursor_line > 0 {
                    self.cursor_line -= 1;
                    self.cursor_column = self.lines[self.cursor_line].len();
                }
            }
            KEY_RIGHT => {
                if self.cursor_column < self.lines[self.cursor_line].len() {
                    self.cursor_column += 1;
                } else if self.cursor_line + 1 < self.lines.len() {
                    self.cursor_line += 1;
                    self.cursor_column = 0;
                }
            }
            KEY_UP => {
                self.cursor_line = self.cursor_line.saturating_sub(1);
                self.cursor_column = self.cursor_column.min(self.lines[self.cursor_line].len());
            }
            KEY_DOWN => {
                self.cursor_line = (self.cursor_line + 1).min(self.lines.len() - 1);
                self.cursor_column = self.cursor_column.min(self.lines[self.cursor_line].len());
            }
            KEY_PAGE_UP => {
                self.cursor_line = self.cursor_line.saturating_sub(page);
                self.cursor_column = self.cursor_column.min(self.lines[self.cursor_line].len());
            }
            KEY_PAGE_DOWN => {
                self.cursor_line = (self.cursor_line + page).min(self.lines.len() - 1);
                self.cursor_column = self.cursor_column.min(self.lines[self.cursor_line].len());
            }
            KEY_HOME => self.cursor_column = 0,
            KEY_END => self.cursor_column = self.lines[self.cursor_line].len(),
            _ => return,
        }
        self.clamp_cursor();
    }

    /// Scrolls by whole lines, clamped to the content bounds.
    pub fn scroll_by_lines(&mut self, lines: i32) {
        let max = self.line_count().saturating_sub(self.visible_lines());
        if lines >= 0 {
            self.first_line = self.first_line.saturating_add(lines as usize);
        } else {
            self.first_line = self
                .first_line
                .saturating_sub(lines.unsigned_abs() as usize);
        }
        self.first_line = self.first_line.min(max);
    }
}

impl Widget for TextArea {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        self.ensure_visible();
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        let colors = self.theme.resolve(self.style);
        canvas.fill_rect_clipped(self.rect, colors.background, self.rect);
        let border = if self.focused {
            colors.accent
        } else {
            colors.border
        };
        for rect in [
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            Rect::new(self.rect.x, self.rect.y, 1, self.rect.height),
            Rect::new(self.rect.right() - 1, self.rect.y, 1, self.rect.height),
        ] {
            canvas.fill_rect_clipped(rect, border, self.rect);
        }
        let inner = self.rect.inset(3);
        let mut y = inner.y;
        for (index, line) in self
            .lines
            .iter()
            .enumerate()
            .skip(self.first_line)
            .take(self.visible_lines())
        {
            if y + GLYPH_HEIGHT as i32 > inner.bottom() {
                break;
            }
            canvas.draw_text_scaled(inner.x + 1, y, line, colors.foreground, 1, inner);
            if self.focused && index == self.cursor_line {
                let caret_x = inner.x + 1 + (self.cursor_column * GLYPH_WIDTH) as i32;
                canvas.fill_rect_clipped(
                    Rect::new(caret_x, y, 1, GLYPH_HEIGHT as u32),
                    colors.accent,
                    inner,
                );
            }
            y += GLYPH_HEIGHT as i32;
        }
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::PointerDown { x, y, button } => {
                if button == 0 && self.rect.contains(Point::new(x, y)) {
                    self.focused = true;
                    return true;
                }
                false
            }
            Event::Scroll { x, y, delta } if delta != 0 && self.rect.contains(Point::new(x, y)) => {
                self.scroll_by_lines(if delta > 0 { 1 } else { -1 });
                true
            }
            Event::KeyDown { code, .. } => match code {
                0x08 => {
                    self.backspace();
                    true
                }
                0x7f => {
                    self.delete();
                    true
                }
                CR | LF => {
                    self.insert_newline();
                    true
                }
                0x20..=0x7e => {
                    self.insert(code as u8);
                    true
                }
                KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN | KEY_PAGE_UP | KEY_PAGE_DOWN
                | KEY_HOME | KEY_END => {
                    self.move_cursor(code);
                    true
                }
                _ => false,
            },
            Event::Scroll { .. }
            | Event::PointerMove { .. }
            | Event::PointerUp { .. }
            | Event::KeyUp { .. }
            | Event::Ignored => false,
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn accessible_role(&self) -> &'static str {
        "text area"
    }

    fn accessible_label(&self) -> Option<&[u8]> {
        self.lines.first().map(Vec::as_slice)
    }
}

/// Assigns rectangles to a slice of widgets.
pub trait Layout {
    /// Positions every widget within `area`.
    fn arrange(&self, area: Rect, widgets: &mut [&mut dyn Widget]);
}

/// Stacks widgets from top to bottom with a fixed vertical gap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerticalLayout {
    /// Pixels between adjacent widgets.
    pub spacing: u32,
}

impl VerticalLayout {
    /// Creates a vertical layout with the given gap.
    #[must_use]
    pub const fn new(spacing: u32) -> Self {
        Self { spacing }
    }
}

impl Layout for VerticalLayout {
    fn arrange(&self, area: Rect, widgets: &mut [&mut dyn Widget]) {
        let count = widgets.len();
        if count == 0 {
            return;
        }
        let gap = self.spacing * (count as u32 - 1);
        let height = area.height.saturating_sub(gap) / count as u32;
        for (index, widget) in widgets.iter_mut().enumerate() {
            let y = area.y + index as i32 * (height + self.spacing) as i32;
            widget.layout(Rect::new(area.x, y, area.width, height));
        }
    }
}

/// Places widgets from left to right with a fixed horizontal gap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HorizontalLayout {
    /// Pixels between adjacent widgets.
    pub spacing: u32,
}

impl HorizontalLayout {
    /// Creates a horizontal layout with the given gap.
    #[must_use]
    pub const fn new(spacing: u32) -> Self {
        Self { spacing }
    }
}

impl Layout for HorizontalLayout {
    fn arrange(&self, area: Rect, widgets: &mut [&mut dyn Widget]) {
        let count = widgets.len();
        if count == 0 {
            return;
        }
        let gap = self.spacing * (count as u32 - 1);
        let width = area.width.saturating_sub(gap) / count as u32;
        for (index, widget) in widgets.iter_mut().enumerate() {
            let x = area.x + index as i32 * (width + self.spacing) as i32;
            widget.layout(Rect::new(x, area.y, width, area.height));
        }
    }
}

/// Places widgets in a row-major grid of fixed column count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GridLayout {
    /// Number of columns; zero leaves the widgets untouched.
    pub columns: usize,
    /// Pixels between adjacent cells.
    pub spacing: u32,
}

impl GridLayout {
    /// Creates a grid layout with `columns` columns and the given gap.
    #[must_use]
    pub const fn new(columns: usize, spacing: u32) -> Self {
        Self { columns, spacing }
    }
}

impl Layout for GridLayout {
    fn arrange(&self, area: Rect, widgets: &mut [&mut dyn Widget]) {
        let count = widgets.len();
        let columns = self.columns.min(count);
        if columns == 0 {
            return;
        }
        let rows = count.div_ceil(columns);
        let gap_x = self.spacing * (columns as u32 - 1);
        let gap_y = self.spacing * (rows as u32 - 1);
        let cell_width = area.width.saturating_sub(gap_x) / columns as u32;
        let cell_height = area.height.saturating_sub(gap_y) / rows as u32;
        for (index, widget) in widgets.iter_mut().enumerate() {
            let column = index % columns;
            let row = index / columns;
            let x = area.x + column as i32 * (cell_width + self.spacing) as i32;
            let y = area.y + row as i32 * (cell_height + self.spacing) as i32;
            widget.layout(Rect::new(x, y, cell_width, cell_height));
        }
    }
}

/// Width of the scrollbar drawn inside a [`ScrollArea`].
pub const SCROLLBAR_WIDTH: u32 = 4;

/// A vertically scrolling container with its own focus routing.
///
/// Children are laid out in a content region taller than the viewport and are
/// translated by the current offset. Painting uses a clipped canvas view, so
/// children cannot draw outside the container. Pointer events already use the
/// translated child rectangles; key events go to the focused child first.
pub struct ScrollArea<'a> {
    rect: Rect,
    content_height: u32,
    offset: u32,
    widgets: Vec<&'a mut dyn Widget>,
    focus: Option<usize>,
}

impl<'a> ScrollArea<'a> {
    /// Creates a scroll area whose content is `content_height` pixels tall.
    #[must_use]
    pub fn new(content_height: u32) -> Self {
        Self {
            rect: Rect::empty(),
            content_height,
            offset: 0,
            widgets: Vec::new(),
            focus: None,
        }
    }

    /// Adds a child; the last added child is hit-tested first.
    pub fn add(&mut self, widget: &'a mut dyn Widget) {
        self.widgets.push(widget);
    }

    /// The current vertical scroll offset in content pixels.
    #[must_use]
    pub const fn offset(&self) -> u32 {
        self.offset
    }

    /// The total content height.
    #[must_use]
    pub const fn content_height(&self) -> u32 {
        self.content_height
    }

    /// Clamps and sets the scroll offset.
    pub fn set_offset(&mut self, offset: u32) {
        self.offset = offset.min(self.max_offset());
    }

    /// Scrolls by a signed number of pixels, clamped to the content bounds.
    pub fn scroll_by(&mut self, delta: i32) {
        if delta >= 0 {
            self.offset = self.offset.saturating_add(delta as u32);
        } else {
            self.offset = self.offset.saturating_sub(delta.unsigned_abs());
        }
        self.offset = self.offset.min(self.max_offset());
    }

    fn viewport(&self) -> Rect {
        Rect::new(
            self.rect.x + 1,
            self.rect.y + 1,
            self.rect.width.saturating_sub(SCROLLBAR_WIDTH + 1),
            self.rect.height.saturating_sub(2),
        )
    }

    fn scrollbar(&self) -> Rect {
        Rect::new(
            self.rect.right() - SCROLLBAR_WIDTH as i32,
            self.rect.y + 1,
            SCROLLBAR_WIDTH.saturating_sub(1),
            self.rect.height.saturating_sub(2),
        )
    }

    fn max_offset(&self) -> u32 {
        self.content_height.saturating_sub(self.viewport().height)
    }

    fn thumb(&self, track: Rect) -> Rect {
        let max = self.max_offset();
        let height = if max == 0 {
            track.height
        } else {
            (track.height * self.viewport().height / self.content_height.max(1))
                .max(4)
                .min(track.height)
        };
        let progress = self
            .offset
            .saturating_mul(track.height - height)
            .checked_div(max)
            .unwrap_or(0);
        let y = track.y + progress as i32;
        Rect::new(track.x, y, track.width, height)
    }

    fn focus_child(&mut self, next: Option<usize>) {
        let old = self.focus;
        if old == next {
            return;
        }
        self.focus = next;
        if let Some(index) = old {
            if let Some(widget) = self.widgets.get_mut(index) {
                widget.set_focus(false);
            }
        }
        if let Some(index) = next {
            if let Some(widget) = self.widgets.get_mut(index) {
                widget.set_focus(true);
            }
        }
    }
}

impl Widget for ScrollArea<'_> {
    fn rect(&self) -> Rect {
        self.rect
    }

    fn layout(&mut self, area: Rect) {
        self.rect = area;
        let view = self.viewport();
        let content = Rect::new(
            view.x,
            view.y - self.offset as i32,
            view.width,
            self.content_height,
        );
        VerticalLayout::new(2).arrange(content, &mut self.widgets);
    }

    fn paint(&self, canvas: &mut Canvas<'_>) {
        canvas.fill_rect_clipped(self.rect, BACKGROUND, self.rect);
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
            FOREGROUND,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.bottom() - 1, self.rect.width, 1),
            FOREGROUND,
            self.rect,
        );
        canvas.fill_rect_clipped(
            Rect::new(self.rect.x, self.rect.y, 1, self.rect.height),
            FOREGROUND,
            self.rect,
        );
        {
            let mut view = canvas.clipped(self.viewport());
            for widget in &self.widgets {
                widget.paint(&mut view);
            }
        }
        let track = self.scrollbar();
        canvas.fill_rect_clipped(track, SURFACE, self.rect);
        let thumb = self.thumb(track);
        let color = if self.focus.is_some() {
            ACCENT
        } else {
            FOREGROUND
        };
        canvas.fill_rect_clipped(thumb, color, self.rect);
    }

    fn event(&mut self, event: Event) -> bool {
        match event {
            Event::Scroll { x, y, delta } => {
                if self.rect.contains(Point::new(x, y)) {
                    self.scroll_by(delta);
                    true
                } else {
                    false
                }
            }
            Event::PointerDown { x, y, button } => {
                let point = Point::new(x, y);
                let track = self.scrollbar();
                if button == 0 && track.contains(point) {
                    let thumb = self.thumb(track);
                    let travel = track.height.saturating_sub(thumb.height);
                    let within = point.y.saturating_sub(track.y) as u32;
                    let max = self.max_offset();
                    self.offset = if max == 0 || travel == 0 {
                        0
                    } else {
                        (within.saturating_sub(thumb.height / 2) * max / travel).min(max)
                    };
                    return true;
                }
                for (index, widget) in self.widgets.iter_mut().enumerate().rev() {
                    let focusable = widget.accepts_focus();
                    if widget.event(event) {
                        if focusable {
                            self.focus_child(Some(index));
                        }
                        return true;
                    }
                }
                false
            }
            Event::KeyDown { .. } | Event::KeyUp { .. } => {
                if let Some(index) = self.focus {
                    if let Some(widget) = self.widgets.get_mut(index) {
                        if widget.event(event) {
                            return true;
                        }
                    }
                }
                false
            }
            Event::PointerMove { .. } | Event::PointerUp { .. } | Event::Ignored => {
                for widget in self.widgets.iter_mut().rev() {
                    if widget.event(event) {
                        return true;
                    }
                }
                false
            }
        }
    }

    fn accepts_focus(&self) -> bool {
        true
    }

    fn set_focus(&mut self, focused: bool) {
        if !focused {
            self.focus_child(None);
        }
    }
}

/// A minimal retained-mode tree that stacks widgets vertically.
pub struct Ui<'a> {
    widgets: Vec<&'a mut dyn Widget>,
    focus: Option<usize>,
}

impl<'a> Ui<'a> {
    /// Creates an empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self {
            widgets: Vec::new(),
            focus: None,
        }
    }

    /// Adds a widget; the last added widget is hit-tested first.
    pub fn add(&mut self, widget: &'a mut dyn Widget) {
        self.widgets.push(widget);
    }

    /// Gives each widget an equal-height slice of `area` with four-pixel gaps.
    pub fn layout(&mut self, area: Rect) {
        VerticalLayout::new(4).arrange(area, &mut self.widgets);
    }

    /// Arranges the tree with an explicit reusable layout strategy.
    pub fn layout_with(&mut self, area: Rect, layout: &dyn Layout) {
        layout.arrange(area, &mut self.widgets);
    }

    /// The index of the widget currently owning keyboard focus.
    #[must_use]
    pub const fn focused_index(&self) -> Option<usize> {
        self.focus
    }

    fn focus_widget(&mut self, next: Option<usize>) {
        let old = self.focus;
        if old == next {
            return;
        }
        self.focus = next;
        if let Some(index) = old {
            if let Some(widget) = self.widgets.get_mut(index) {
                widget.set_focus(false);
            }
        }
        if let Some(index) = next {
            if let Some(widget) = self.widgets.get_mut(index) {
                widget.set_focus(true);
            }
        }
    }

    /// Paints widgets in insertion order.
    pub fn paint(&self, canvas: &mut Canvas<'_>) {
        for widget in &self.widgets {
            widget.paint(canvas);
        }
    }

    /// Collects accessibility records in insertion order.
    #[must_use]
    pub fn accessible(&self) -> Vec<Accessible<'_>> {
        self.widgets
            .iter()
            .map(|widget| Accessible {
                role: widget.accessible_role(),
                label: widget.accessible_label().unwrap_or_default(),
            })
            .collect()
    }

    /// Delivers an event to the focused widget first, then topmost-first.
    ///
    /// A pointer press that a focusable widget consumes also moves focus to it.
    pub fn dispatch(&mut self, event: Event) -> bool {
        if matches!(event, Event::KeyDown { .. } | Event::KeyUp { .. }) {
            if let Some(index) = self.focus {
                if let Some(widget) = self.widgets.get_mut(index) {
                    if widget.event(event) {
                        return true;
                    }
                }
            }
        }
        for (index, widget) in self.widgets.iter_mut().enumerate().rev() {
            let focusable = widget.accepts_focus();
            if widget.event(event) {
                if matches!(event, Event::PointerDown { .. }) && focusable {
                    self.focus_widget(Some(index));
                }
                return true;
            }
        }
        false
    }
}

impl Default for Ui<'_> {
    fn default() -> Self {
        Self::new()
    }
}
