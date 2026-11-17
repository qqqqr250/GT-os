//! The console: every write goes to COM1 and to the VGA text buffer.
//!
//! Mirroring to COM1 first means a crash in the text-buffer path still leaves a
//! serial log behind, and it is what makes a headless QEMU run show the output.

use core::fmt::{self, Write};

use crate::serial;
use crate::vga::{self, Color, ColorCode};

/// A [`fmt::Write`] adapter that renders into the VGA text buffer.
struct VgaWriter;

impl fmt::Write for VgaWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        vga::write_str(s);
        Ok(())
    }
}

/// Formats to COM1 and to the VGA text buffer.
pub fn print_args(args: fmt::Arguments<'_>) {
    serial::print_args(args);
    let _ = VgaWriter.write_fmt(args);
}

/// Sets the foreground colour of the text console.
pub fn set_fg(color: Color) {
    vga::set_fg(color);
}

/// Sets the background colour of the text console.
pub fn set_bg(color: Color) {
    vga::set_bg(color);
}

/// Sets both colours at once.
pub fn set_color(foreground: Color, background: Color) {
    vga::set_color(ColorCode::new(foreground, background));
}

/// Clears the screen and resets the colours.
pub fn clear_screen() {
    vga::set_color(ColorCode::new(Color::LightGray, Color::Black));
    vga::clear();
}

/// Prints the demo banner.
pub fn banner(offset: Option<u64>) {
    set_fg(Color::LightGray);
    print_args(format_args!(
        "+----------------------------------------------------------+\n"
    ));
    set_fg(Color::White);
    print_args(format_args!(
        "|  RustOS minimal: post-02, done properly                  |\n"
    ));
    set_fg(Color::LightGray);
    print_args(format_args!(
        "+----------------------------------------------------------+\n\n"
    ));

    set_fg(Color::LightCyan);
    print_args(format_args!("Hello, World!"));
    set_fg(Color::LightGray);
    print_args(format_args!(
        "  <- written to the 80x25 VGA text buffer at 0xb8000\n\n"
    ));

    if let Some(offset) = offset {
        print_args(format_args!(
            "physical-memory window: 0x{offset:012x}  (buffer at 0x{:012x})\n\n",
            offset.wrapping_add(vga::BUFFER_PHYS)
        ));
    } else {
        set_fg(Color::LightRed);
        print_args(format_args!(
            "no physical-memory window: the buffer is unreachable\n\n"
        ));
        set_fg(Color::LightGray);
    }
}

/// Prints a row of all sixteen colours so the palette is visible at a glance.
pub fn palette() {
    set_fg(Color::LightGray);
    print_args(format_args!("the sixteen text colours:\n  "));
    for index in 0..16 {
        let color = Color::from_palette(index);
        set_color(color, Color::Black);
        print_args(format_args!("{:02X}", color as u8));
    }
    set_color(Color::LightGray, Color::Black);
    print_args(format_args!("\n\n"));
}
