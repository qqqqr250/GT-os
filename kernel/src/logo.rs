//! Renders the embedded GT logo into the linear framebuffer.
//!
//! The 2-bpp bitmap lives in [`crate::logo_data`] and is generated from
//! `tools/logo-source.jpg` by `tools/gen-logo.ps1`. Palette index 0 is
//! transparent, so the dark console background shows through the corners and
//! the logo sits cleanly above the boot log.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::console::{self, Color, pixel_bytes};
use crate::logo_data::{LOGO_PIXELS, LOGO_SIZE, ROW_BYTES};

/// Matches the palette in `tools/gen-logo.ps1`. Index 0 is never drawn.
const PALETTE: [Color; 4] = [
    Color(0x00_0b_0e_14), // transparent
    Color(0x00_fe_cb_00), // yellow disc
    Color(0x00_f8_04_01), // red GT letters
    Color(0x00_07_30_ea), // blue ring
];

static DRAWN: AtomicBool = AtomicBool::new(false);
static LOGO_X: AtomicUsize = AtomicUsize::new(0);
static LOGO_Y: AtomicUsize = AtomicUsize::new(0);
static LOGO_SCALE: AtomicUsize = AtomicUsize::new(1);

/// Returns the 2-bit palette index of one logo pixel.
fn index(x: usize, y: usize) -> usize {
    let byte = LOGO_PIXELS[y * ROW_BYTES + x / 4];
    usize::from((byte >> (6 - 2 * (x % 4))) & 0b11)
}

/// Draws the logo with its top-left at `(x0, y0)`, magnifying each pixel by
/// `scale`. Transparent pixels are skipped, and out-of-screen pixels are
/// clipped rather than wrapping around the framebuffer stride.
pub fn draw_at(x0: usize, y0: usize, scale: usize) {
    let info = console::framebuffer_info();
    if info.width == 0 || info.bytes_per_pixel == 0 || scale == 0 {
        return;
    }
    let bpp = info.bytes_per_pixel.min(4);
    let pixels = PALETTE.map(|color| pixel_bytes(color, info.pixel_format));
    let buffer = console::framebuffer_buffer();

    for y in 0..LOGO_SIZE {
        for x in 0..LOGO_SIZE {
            let color = index(x, y);
            if color == 0 {
                continue;
            }
            let pixel = &pixels[color][..bpp];
            for dy in 0..scale {
                let row = y0 + y * scale + dy;
                if row >= info.height {
                    break;
                }
                for dx in 0..scale {
                    let col = x0 + x * scale + dx;
                    if col >= info.width {
                        break;
                    }
                    let offset = (row * info.stride + col) * bpp;
                    if offset + bpp <= buffer.len() {
                        buffer[offset..offset + bpp].copy_from_slice(pixel);
                    }
                }
            }
        }
    }
}

/// Clears a strip at the current console cursor, draws the logo there, and
/// leaves the cursor just below it so following text never overlaps the bitmap.
/// Records the chosen position for later repaints. In VGA text mode this is a
/// no-op.
fn place() {
    let info = console::framebuffer_info();
    if info.width == 0 {
        return;
    }
    // Roughly a quarter of the screen height, never smaller than 1x or larger
    // than 4x magnification.
    let scale = (info.height / (4 * LOGO_SIZE)).clamp(1, 4);
    let size = LOGO_SIZE * scale;
    let y = console::reserve_bitmap(size);
    let x = info.width.saturating_sub(size) / 2;
    LOGO_X.store(x, Ordering::Relaxed);
    LOGO_Y.store(y, Ordering::Relaxed);
    LOGO_SCALE.store(scale, Ordering::Relaxed);
    draw_at(x, y, scale);
}

/// Draws the logo in a reserved strip at boot. Only the first call has any
/// effect; in VGA text mode there is no bitmap to draw.
pub fn draw_boot() {
    if DRAWN.swap(true, Ordering::Relaxed) {
        return;
    }
    place();
}

/// Repaints the logo, reserving a fresh strip so the shell banner and prompt
/// that follow are drawn below it. Used by the `logo` command; after `clear`
/// the cursor is back at the top, so this restores the boot layout.
pub fn redraw() {
    place();
}
