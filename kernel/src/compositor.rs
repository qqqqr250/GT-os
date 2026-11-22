//! A small kernel compositor built on the VBE framebuffer.
//!
//! Ring 3 cannot map the physical display. It creates a bounded window, paints
//! a tightly packed RGB buffer in its own heap, and commits that buffer. The
//! kernel keeps a private backing image per window plus one display shadow
//! frame. A commit updates only the window's backing image; `winpresent`
//! repaints the affected rectangles in layer order and blits the shadow to the
//! display.
//!
//! Windows have an integer `layer`: larger values paint on top, and creation
//! order breaks ties. Damage commits update a subrectangle of a window from a
//! larger guest buffer, so a toolkit need not resend whole frames. The kernel
//! draws a title bar and border over every window, and each window owns a
//! bounded mailbox for short cross-process messages. The model is still
//! deliberately bounded: fixed window slots, no general-purpose scheme IPC and
//! no DRM/KMS backend.

use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::FrameBufferInfo;
use spin::Mutex;

use crate::console::{self, Color};
use crate::font::{FONT, FONT_HEIGHT, FONT_WIDTH};
use crate::shm::Mapping;

/// Maximum number of simultaneously live windows.
const MAX_WINDOWS: usize = 16;
/// Fill colour for freshly created or removed windows.
const WINDOW_BG: Color = Color(0x00_12_18_24);
/// Largest payload accepted by `winpost`.
const MESSAGE_BYTES: usize = 64;
/// Messages buffered per window.
const MESSAGE_QUEUE: usize = 8;
/// Width of the kernel-drawn window frame.
const BORDER_PX: usize = 2;
/// Height of the kernel-drawn title bar inside that frame.
const TITLE_BAR_PX: usize = 18;
/// Longest title accepted by `winsettitle`.
const TITLE_BYTES: usize = 64;
const BORDER_COLOR: Color = Color(0x00_4f_c1_ff);
const TITLE_BG: Color = Color(0x00_1b_2a_3a);
const TITLE_TEXT: Color = Color(0x00_d8_de_e9);
const CLOSE_BG: Color = Color(0x00_3a_1e_24);

/// An axis-aligned rectangle in absolute framebuffer coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl Rect {
    fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.x.saturating_add(self.width).min(other.x + other.width);
        let bottom = self
            .y
            .saturating_add(self.height)
            .min(other.y + other.height);
        if x < right && y < bottom {
            Some(Self {
                x,
                y,
                width: right - x,
                height: bottom - y,
            })
        } else {
            None
        }
    }
}

/// One kernel-mediated message delivered to a window.
#[derive(Clone, Copy, Debug)]
pub struct Message {
    /// PID of the process that posted the message.
    pub sender: u64,
    /// Number of valid bytes in `data`.
    pub len: usize,
    /// Fixed-size payload; only the first `len` bytes are meaningful.
    pub data: [u8; MESSAGE_BYTES],
}

/// Rectangle inside a window's own pixel buffer, plus the source row stride.
#[derive(Clone, Copy, Debug)]
pub struct Damage {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    /// Bytes between rows in the caller's source buffer.
    pub stride: usize,
}

/// Pixel storage behind one window.
///
/// Owned windows keep a private kernel allocation. Shared windows read directly
/// from retained shared-memory frames, so a guest can paint a mapped region and
/// let `winpresent` repaint without copying pixels through a syscall again.
enum Pixels {
    Owned(Vec<u8>),
    Shared(Mapping),
}

impl Pixels {
    fn read(&self, offset: usize, destination: &mut [u8]) -> bool {
        match self {
            Self::Owned(pixels) => {
                let Some(end) = offset.checked_add(destination.len()) else {
                    return false;
                };
                if end > pixels.len() {
                    return false;
                }
                destination.copy_from_slice(&pixels[offset..end]);
                true
            }
            Self::Shared(mapping) => mapping.read(offset, destination),
        }
    }

    fn write(&mut self, offset: usize, source: &[u8]) -> bool {
        match self {
            Self::Owned(pixels) => {
                let Some(end) = offset.checked_add(source.len()) else {
                    return false;
                };
                if end > pixels.len() {
                    return false;
                }
                pixels[offset..end].copy_from_slice(source);
                true
            }
            Self::Shared(mapping) => mapping.write(offset, source),
        }
    }
}

/// A clipped rectangle owned by one process.
struct Window {
    owner: u64,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    layer: u32,
    /// Creation order; higher values win ties within the same layer.
    sequence: u64,
    /// Tightly packed image in the display's pixel format.
    pixels: Pixels,
    /// Printable ASCII title rendered in the kernel-drawn title bar.
    title: Vec<u8>,
    /// Bounded mailbox for kernel-mediated window IPC.
    messages: [Option<Message>; MESSAGE_QUEUE],
    message_head: usize,
    message_tail: usize,
    message_count: usize,
}

impl Window {
    fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// The compositor's fixed window table and display shadow frame.
struct Compositor {
    windows: [Option<Window>; MAX_WINDOWS],
    shadow: Vec<u8>,
    info: FrameBufferInfo,
    next_sequence: u64,
}

impl Compositor {
    const fn new() -> Self {
        Self {
            windows: [const { None }; MAX_WINDOWS],
            shadow: Vec::new(),
            info: empty_info(),
            next_sequence: 0,
        }
    }

    /// Copies the current display into the shadow frame on first use.
    ///
    /// Doing this lazily preserves the boot log and shell text until a guest
    /// actually creates a window, and avoids allocating during early boot.
    fn ensure_shadow(&mut self) -> bool {
        if !self.shadow.is_empty() {
            return true;
        }
        let info = console::framebuffer_info();
        if info.width == 0
            || info.height == 0
            || info.byte_len == 0
            || info.bytes_per_pixel == 0
            || info.bytes_per_pixel > 4
        {
            return false;
        }
        let display = console::framebuffer_buffer();
        if display.len() < info.byte_len {
            return false;
        }
        self.shadow = display[..info.byte_len].to_vec();
        self.info = info;
        true
    }

    /// The display-format pixel used to clear removed windows.
    fn background_pixel(&self) -> [u8; 4] {
        console::pixel_bytes(WINDOW_BG, self.info.pixel_format)
    }

    /// Paints a rectangle in one colour, clipped to the visible framebuffer.
    fn fill_solid(&mut self, rect: Rect, color: Color) {
        let bpp = self.info.bytes_per_pixel;
        let pixel = console::pixel_bytes(color, self.info.pixel_format);
        let bytes = &pixel[..bpp.min(pixel.len())];
        if bytes.is_empty() || rect.x >= self.info.width || rect.y >= self.info.height {
            return;
        }
        let width = rect.width.min(self.info.width - rect.x);
        if width == 0 {
            return;
        }
        for row in 0..rect.height {
            let y = rect.y + row;
            if y >= self.info.height {
                break;
            }
            let start = (y * self.info.stride + rect.x) * bpp;
            let end = start.saturating_add(width * bpp);
            let mut offset = start;
            while offset < end && offset < self.shadow.len() {
                let count = (end - offset)
                    .min(bytes.len())
                    .min(self.shadow.len() - offset);
                self.shadow[offset..offset + count].copy_from_slice(&bytes[..count]);
                offset += count;
            }
        }
    }

    /// Paints a rectangle in the colour used for removed windows.
    fn fill_rect(&mut self, rect: Rect) {
        self.fill_solid(rect, WINDOW_BG);
    }

    /// Paints one pixel, clipped to both the framebuffer and `clip`.
    fn put_pixel_clipped(&mut self, x: usize, y: usize, pixel: [u8; 4], clip: Rect) {
        let clip_right = clip.x.saturating_add(clip.width);
        let clip_bottom = clip.y.saturating_add(clip.height);
        if x < clip.x
            || y < clip.y
            || x >= clip_right
            || y >= clip_bottom
            || x >= self.info.width
            || y >= self.info.height
        {
            return;
        }
        let bpp = self.info.bytes_per_pixel;
        let len = bpp.min(pixel.len());
        let offset = (y * self.info.stride + x) * bpp;
        if len > 0 && offset + len <= self.shadow.len() {
            self.shadow[offset..offset + len].copy_from_slice(&pixel[..len]);
        }
    }

    /// Draws printable ASCII with the console font, clipped to `clip`.
    fn draw_text_clipped(&mut self, x: usize, y: usize, text: &[u8], color: Color, clip: Rect) {
        let pixel = console::pixel_bytes(color, self.info.pixel_format);
        for (index, &byte) in text.iter().enumerate() {
            let glyph = &FONT[(byte & 0x7f) as usize];
            let origin_x = x.saturating_add(index.saturating_mul(FONT_WIDTH));
            for (row, &bits) in glyph.iter().enumerate() {
                for col in 0..FONT_WIDTH {
                    if bits & (1 << col) == 0 {
                        continue;
                    }
                    self.put_pixel_clipped(origin_x + col, y + row, pixel, clip);
                }
            }
        }
    }

    /// Draws a border and title bar over one window's content.
    fn draw_decorations(&mut self, index: usize, clip: Rect) {
        let (rect, title) = {
            let Some(window) = self.windows[index].as_ref() else {
                return;
            };
            (window.rect(), window.title.clone())
        };
        if rect.width <= BORDER_PX * 2 || rect.height <= BORDER_PX * 2 + TITLE_BAR_PX {
            return;
        }
        let inner = Rect {
            x: rect.x + BORDER_PX,
            y: rect.y + BORDER_PX,
            width: rect.width - BORDER_PX * 2,
            height: rect.height - BORDER_PX * 2,
        };
        let borders = [
            Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: BORDER_PX,
            },
            Rect {
                x: rect.x,
                y: rect.y + rect.height - BORDER_PX,
                width: rect.width,
                height: BORDER_PX,
            },
            Rect {
                x: rect.x,
                y: rect.y + BORDER_PX,
                width: BORDER_PX,
                height: rect.height - BORDER_PX * 2,
            },
            Rect {
                x: rect.x + rect.width - BORDER_PX,
                y: rect.y + BORDER_PX,
                width: BORDER_PX,
                height: rect.height - BORDER_PX * 2,
            },
        ];
        for border in borders {
            if let Some(area) = border.intersection(clip) {
                self.fill_solid(area, BORDER_COLOR);
            }
        }
        let title_bar = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: TITLE_BAR_PX,
        };
        if let Some(area) = title_bar.intersection(clip) {
            self.fill_solid(area, TITLE_BG);
        }

        let close_size = FONT_WIDTH + 2;
        let close = Rect {
            x: inner.x + inner.width.saturating_sub(close_size + 2),
            y: inner.y + (TITLE_BAR_PX - FONT_HEIGHT) / 2,
            width: close_size.min(inner.width),
            height: FONT_HEIGHT.min(TITLE_BAR_PX),
        };
        if let Some(area) = close.intersection(clip) {
            self.fill_solid(area, CLOSE_BG);
        }
        self.draw_text_clipped(close.x + 1, close.y, b"x", TITLE_TEXT, clip);

        let text_x = inner.x + 4;
        let text_y = inner.y + (TITLE_BAR_PX - FONT_HEIGHT) / 2;
        let available = close.x.saturating_sub(text_x + 4) / FONT_WIDTH;
        self.draw_text_clipped(
            text_x,
            text_y,
            &title[..title.len().min(available)],
            TITLE_TEXT,
            clip,
        );
    }

    /// Copies one clipped part of a window's backing image into the shadow.
    fn blit_window(&mut self, index: usize, clip: Rect) {
        let Some(window) = self.windows[index].as_ref() else {
            return;
        };
        let Some(area) = window.rect().intersection(clip) else {
            return;
        };
        let bpp = self.info.bytes_per_pixel;
        let row_bytes = window.width * bpp;
        let source_x = (area.x - window.x) * bpp;
        let count = area.width * bpp;
        for row in 0..area.height {
            let y = area.y + row;
            if y >= self.info.height {
                break;
            }
            let source = (y - window.y) * row_bytes + source_x;
            let destination = (y * self.info.stride + area.x) * bpp;
            if destination + count <= self.shadow.len() {
                let _ = window
                    .pixels
                    .read(source, &mut self.shadow[destination..destination + count]);
            }
        }
    }

    /// Repaints one rectangle from every intersecting window in stacking order.
    fn repaint_rect(&mut self, rect: Rect) {
        if !self.ensure_shadow() {
            return;
        }
        let screen = Rect {
            x: 0,
            y: 0,
            width: self.info.width,
            height: self.info.height,
        };
        let Some(area) = rect.intersection(screen) else {
            return;
        };
        self.fill_rect(area);

        let mut order: Vec<usize> = (0..MAX_WINDOWS)
            .filter(|&index| self.windows[index].is_some())
            .collect();
        order.sort_by_key(|&index| {
            let window = self.windows[index]
                .as_ref()
                .expect("index was just filtered");
            (window.layer, window.sequence)
        });
        for index in order {
            self.blit_window(index, area);
            self.draw_decorations(index, area);
        }
    }

    /// Repaints the union of all window rectangles after a stack change.
    fn repaint_all(&mut self) {
        let rects: Vec<Rect> = self.windows.iter().flatten().map(Window::rect).collect();
        for rect in rects {
            self.repaint_rect(rect);
        }
    }

    /// Reserves a window for `owner` and returns its one-based identifier.
    fn create(
        &mut self,
        owner: u64,
        x: u64,
        y: u64,
        width: u64,
        height: u64,
        layer: u32,
    ) -> Option<u64> {
        if !self.ensure_shadow() {
            return None;
        }
        let x = usize::try_from(x).ok()?;
        let y = usize::try_from(y).ok()?;
        let width = usize::try_from(width).ok()?;
        let height = usize::try_from(height).ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        if x.checked_add(width)? > self.info.width || y.checked_add(height)? > self.info.height {
            return None;
        }
        let bpp = self.info.bytes_per_pixel;
        let byte_len = width.checked_mul(height)?.checked_mul(bpp)?;
        let pixel = self.background_pixel();
        let background = &pixel[..bpp.min(pixel.len())];
        let mut pixels = vec![0; byte_len];
        for row in pixels.chunks_exact_mut(bpp) {
            row.copy_from_slice(background);
        }

        let index = self.windows.iter().position(Option::is_none)?;
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.checked_add(1)?;
        self.windows[index] = Some(Window {
            owner,
            x,
            y,
            width,
            height,
            layer,
            sequence,
            pixels: Pixels::Owned(pixels),
            title: Vec::from(&b"GTos window"[..]),
            messages: [const { None }; MESSAGE_QUEUE],
            message_head: 0,
            message_tail: 0,
            message_count: 0,
        });
        self.repaint_rect(Rect {
            x,
            y,
            width,
            height,
        });
        Some(index as u64 + 1)
    }

    fn window_index(&self, owner: u64, id: u64) -> Option<usize> {
        let index = usize::try_from(id).ok()?.checked_sub(1)?;
        let window = self.windows.get(index)?.as_ref()?;
        (window.owner == owner).then_some(index)
    }

    /// Bytes a caller must supply to commit the whole window.
    fn commit_length(&self, owner: u64, id: u64) -> Option<usize> {
        let index = self.window_index(owner, id)?;
        let window = self.windows[index].as_ref()?;
        window
            .width
            .checked_mul(window.height)?
            .checked_mul(self.info.bytes_per_pixel)
    }

    /// Bytes a caller must supply for one damage rectangle.
    fn damage_length(&self, owner: u64, id: u64, damage: Damage) -> Option<usize> {
        let index = self.window_index(owner, id)?;
        let window = self.windows[index].as_ref()?;
        if damage.width == 0
            || damage.height == 0
            || damage.x.checked_add(damage.width)? > window.width
            || damage.y.checked_add(damage.height)? > window.height
        {
            return None;
        }
        let row = damage.width.checked_mul(self.info.bytes_per_pixel)?;
        if damage.stride < row {
            return None;
        }
        damage
            .height
            .checked_sub(1)?
            .checked_mul(damage.stride)?
            .checked_add(row)
    }

    /// Replaces a window's whole backing image and repaints it.
    fn commit(&mut self, owner: u64, id: u64, source: &[u8]) -> Option<u64> {
        let index = self.window_index(owner, id)?;
        let needed = self.commit_length(owner, id)?;
        if source.len() < needed {
            return None;
        }
        let rect = self.windows[index].as_ref()?.rect();
        if !self.windows[index]
            .as_mut()?
            .pixels
            .write(0, &source[..needed])
        {
            return None;
        }
        self.repaint_rect(rect);
        Some(needed as u64)
    }

    /// Replaces one subrectangle of a window from a strided source buffer.
    fn commit_damage(&mut self, owner: u64, id: u64, source: &[u8], damage: Damage) -> Option<u64> {
        let index = self.window_index(owner, id)?;
        let needed = self.damage_length(owner, id, damage)?;
        if source.len() < needed {
            return None;
        }
        let bpp = self.info.bytes_per_pixel;
        let (rect, row_bytes) = {
            let window = self.windows[index].as_ref()?;
            (window.rect(), window.width.checked_mul(bpp)?)
        };
        for row in 0..damage.height {
            let source_offset = row * damage.stride;
            let destination = (damage.y + row) * row_bytes + damage.x * bpp;
            let count = damage.width * bpp;
            if !self.windows[index]
                .as_mut()?
                .pixels
                .write(destination, &source[source_offset..source_offset + count])
            {
                return None;
            }
        }

        let area = Rect {
            x: rect.x + damage.x,
            y: rect.y + damage.y,
            width: damage.width,
            height: damage.height,
        };
        self.repaint_rect(area);
        Some(needed as u64)
    }

    /// Replaces a window's backing store with retained shared-memory frames.
    fn bind_shm(&mut self, owner: u64, id: u64, mapping: Mapping) -> bool {
        let Some(index) = self.window_index(owner, id) else {
            return false;
        };
        let Some(rect) = self.windows[index].as_ref().map(Window::rect) else {
            return false;
        };
        self.windows[index]
            .as_mut()
            .expect("index was validated")
            .pixels = Pixels::Shared(mapping);
        self.repaint_rect(rect);
        true
    }

    /// Queues a message for a window, regardless of which process owns it.
    fn post(&mut self, sender: u64, id: u64, bytes: &[u8]) -> bool {
        if bytes.is_empty() || bytes.len() > MESSAGE_BYTES {
            return false;
        }
        let Ok(index) = usize::try_from(id) else {
            return false;
        };
        let Some(index) = index.checked_sub(1) else {
            return false;
        };
        let Some(window) = self.windows.get_mut(index).and_then(Option::as_mut) else {
            return false;
        };
        if window.message_count == MESSAGE_QUEUE {
            return false;
        }
        let mut data = [0u8; MESSAGE_BYTES];
        data[..bytes.len()].copy_from_slice(bytes);
        let slot = window.message_tail;
        window.messages[slot] = Some(Message {
            sender,
            len: bytes.len(),
            data,
        });
        window.message_tail = (slot + 1) % MESSAGE_QUEUE;
        window.message_count += 1;
        true
    }

    /// Removes the oldest message from a window owned by `owner`.
    fn read(&mut self, owner: u64, id: u64) -> Option<Message> {
        let index = self.window_index(owner, id)?;
        let window = self.windows[index].as_mut()?;
        if window.message_count == 0 {
            return None;
        }
        let slot = window.message_head;
        let message = window.messages[slot].take()?;
        window.message_head = (slot + 1) % MESSAGE_QUEUE;
        window.message_count -= 1;
        Some(message)
    }

    /// Replaces a window's title, used by the kernel-drawn title bar.
    fn set_title(&mut self, owner: u64, id: u64, bytes: &[u8]) -> bool {
        let Some(index) = self.window_index(owner, id) else {
            return false;
        };
        let title: Vec<u8> = bytes
            .iter()
            .take(TITLE_BYTES)
            .map(|&byte| if byte.is_ascii_graphic() { byte } else { b' ' })
            .collect();
        let Some(rect) = self.windows[index].as_ref().map(Window::rect) else {
            return false;
        };
        self.windows[index]
            .as_mut()
            .expect("index was just validated")
            .title = title;
        self.repaint_rect(rect);
        true
    }

    /// Changes a window's stacking layer and repaints the affected area.
    fn set_layer(&mut self, owner: u64, id: u64, layer: u32) -> bool {
        let Some(index) = self.window_index(owner, id) else {
            return false;
        };
        self.windows[index]
            .as_mut()
            .expect("index was just validated")
            .layer = layer;
        self.repaint_all();
        true
    }

    /// Blits the shadow frame to the physical display.
    fn present(&mut self) -> bool {
        if !self.ensure_shadow() {
            return false;
        }
        // Shared backing stores change directly in ring 3 without a commit
        // syscall, so refresh every window before blitting the display.
        self.repaint_all();
        let display = console::framebuffer_buffer();
        let count = self.shadow.len().min(display.len());
        if count == 0 {
            return false;
        }
        display[..count].copy_from_slice(&self.shadow[..count]);
        true
    }

    fn destroy(&mut self, owner: u64, id: u64) -> bool {
        let Some(index) = usize::try_from(id)
            .ok()
            .and_then(|index| index.checked_sub(1))
        else {
            return false;
        };
        let Some(window) = self.windows.get_mut(index).and_then(Option::take) else {
            return false;
        };
        if window.owner != owner {
            self.windows[index] = Some(window);
            return false;
        }
        self.repaint_rect(window.rect());
        true
    }

    /// Removes every window owned by a process, then repaints the display.
    fn release_process(&mut self, owner: u64) -> bool {
        let mut rects = Vec::new();
        for slot in &mut self.windows {
            if slot.as_ref().is_some_and(|window| window.owner == owner) {
                let window = slot.take().expect("owner was just checked");
                rects.push(window.rect());
            }
        }
        for rect in &rects {
            self.repaint_rect(*rect);
        }
        if !rects.is_empty() {
            self.present();
        }
        !rects.is_empty()
    }

    fn release_all(&mut self) {
        let rects: Vec<Rect> = self
            .windows
            .iter_mut()
            .filter_map(Option::take)
            .map(|window| window.rect())
            .collect();
        for rect in &rects {
            self.repaint_rect(*rect);
        }
        self.present();
    }
}

const fn empty_info() -> FrameBufferInfo {
    FrameBufferInfo {
        byte_len: 0,
        width: 0,
        height: 0,
        pixel_format: bootloader_api::info::PixelFormat::Unknown {
            red_position: 0,
            green_position: 0,
            blue_position: 0,
        },
        bytes_per_pixel: 0,
        stride: 0,
    }
}

static COMPOSITOR: Mutex<Compositor> = Mutex::new(Compositor::new());

/// Creates a window; the returned identifier is valid until it is destroyed or
/// its owner exits. Higher `layer` values paint on top.
pub fn create(owner: u64, x: u64, y: u64, width: u64, height: u64, layer: u32) -> Option<u64> {
    COMPOSITOR.lock().create(owner, x, y, width, height, layer)
}

/// Number of source bytes required by [`commit`].
pub fn commit_length(owner: u64, id: u64) -> Option<usize> {
    COMPOSITOR.lock().commit_length(owner, id)
}

/// Number of source bytes required by [`commit_damage`].
pub fn damage_length(owner: u64, id: u64, damage: Damage) -> Option<usize> {
    COMPOSITOR.lock().damage_length(owner, id, damage)
}

/// Copies a user-owned window image into the shadow frame.
pub fn commit(owner: u64, id: u64, source: &[u8]) -> Option<u64> {
    COMPOSITOR.lock().commit(owner, id, source)
}

/// Copies one subrectangle from a strided user buffer into a window.
pub fn commit_damage(owner: u64, id: u64, source: &[u8], damage: Damage) -> Option<u64> {
    COMPOSITOR.lock().commit_damage(owner, id, source, damage)
}

/// Binds retained shared-memory frames as a window's backing store.
pub fn bind_shm(owner: u64, id: u64, mapping: Mapping) -> bool {
    COMPOSITOR.lock().bind_shm(owner, id, mapping)
}

/// Changes a window's stacking layer.
pub fn set_layer(owner: u64, id: u64, layer: u32) -> bool {
    COMPOSITOR.lock().set_layer(owner, id, layer)
}

/// Sets the printable ASCII title shown in a window's title bar.
pub fn set_title(owner: u64, id: u64, bytes: &[u8]) -> bool {
    COMPOSITOR.lock().set_title(owner, id, bytes)
}

/// Queues a message for a window owned by any process.
pub fn post(sender: u64, id: u64, bytes: &[u8]) -> bool {
    COMPOSITOR.lock().post(sender, id, bytes)
}

/// Reads the oldest message from a window owned by `owner`.
pub fn read(owner: u64, id: u64) -> Option<Message> {
    COMPOSITOR.lock().read(owner, id)
}

/// Blits the shadow frame to the display.
pub fn present() -> bool {
    COMPOSITOR.lock().present()
}

/// Destroys one window owned by `owner`.
pub fn destroy(owner: u64, id: u64) -> bool {
    COMPOSITOR.lock().destroy(owner, id)
}

/// Releases all windows belonging to an exited process.
pub fn release_process(owner: u64) {
    COMPOSITOR.lock().release_process(owner);
}

/// Drops every window between synchronous `run` invocations.
pub fn release_all() {
    COMPOSITOR.lock().release_all();
}
