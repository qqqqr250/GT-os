//! Style tokens shared by the widget toolkit.
//!
//! A [`Theme`] supplies sensible defaults for every surface. A [`Style`] holds
//! optional per-widget overrides, so a declarative view can change one colour
//! without redefining the whole palette. Resolution is a simple chain of
//! `style override -> theme token`, performed once when a view is built.

use crate::Color;

/// A complete palette used by themed widgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Window and container background.
    pub background: Color,
    /// Primary text and outline colour.
    pub foreground: Color,
    /// Secondary text colour.
    pub dim: Color,
    /// Focus and selection colour.
    pub accent: Color,
    /// Normal control face.
    pub surface: Color,
    /// Control face while hovered.
    pub surface_hover: Color,
    /// Control face while pressed.
    pub surface_pressed: Color,
    /// Control border colour.
    pub border: Color,
}

impl Theme {
    /// The dark palette used by the kernel console and the first demo windows.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            background: Color(0x00_0b_0e_14),
            foreground: Color(0x00_d8_de_e9),
            dim: Color(0x00_70_7a_8a),
            accent: Color(0x00_4f_c1_ff),
            surface: Color(0x00_1c_27_38),
            surface_hover: Color(0x00_2a_3b_52),
            surface_pressed: Color(0x00_3f_6b_86),
            border: Color(0x00_70_7a_8a),
        }
    }

    /// A high-contrast light palette.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            background: Color(0x00_f2_f4_f7),
            foreground: Color(0x00_14_1b_26),
            dim: Color(0x0055_6273),
            accent: Color(0x00_0b_5d_9e),
            surface: Color(0x00_ff_ff_ff),
            surface_hover: Color(0x00_e4_ec_f6),
            surface_pressed: Color(0x00_c8_dc_f0),
            border: Color(0x0055_6273),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// Optional overrides applied on top of a [`Theme`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// Overrides [`Theme::foreground`].
    pub foreground: Option<Color>,
    /// Overrides [`Theme::background`].
    pub background: Option<Color>,
    /// Overrides [`Theme::accent`].
    pub accent: Option<Color>,
    /// Overrides [`Theme::surface`].
    pub surface: Option<Color>,
    /// Overrides [`Theme::border`].
    pub border: Option<Color>,
}

impl Style {
    /// Creates an empty override set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            foreground: None,
            background: None,
            accent: None,
            surface: None,
            border: None,
        }
    }

    /// Overrides the foreground colour.
    #[must_use]
    pub const fn foreground(mut self, color: Color) -> Self {
        self.foreground = Some(color);
        self
    }

    /// Overrides the background colour.
    #[must_use]
    pub const fn background(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    /// Overrides the accent colour.
    #[must_use]
    pub const fn accent(mut self, color: Color) -> Self {
        self.accent = Some(color);
        self
    }

    /// Overrides the control surface colour.
    #[must_use]
    pub const fn surface(mut self, color: Color) -> Self {
        self.surface = Some(color);
        self
    }

    /// Overrides the border colour.
    #[must_use]
    pub const fn border(mut self, color: Color) -> Self {
        self.border = Some(color);
        self
    }
}

/// A [`Theme`] with a [`Style`] already applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Effective background.
    pub background: Color,
    /// Effective foreground.
    pub foreground: Color,
    /// Effective dim text colour.
    pub dim: Color,
    /// Effective accent.
    pub accent: Color,
    /// Effective control face.
    pub surface: Color,
    /// Effective hovered face.
    pub surface_hover: Color,
    /// Effective pressed face.
    pub surface_pressed: Color,
    /// Effective border.
    pub border: Color,
}

impl Theme {
    /// Resolves a style against this theme.
    #[must_use]
    pub const fn resolve(self, style: Style) -> Resolved {
        Resolved {
            background: match style.background {
                Some(color) => color,
                None => self.background,
            },
            foreground: match style.foreground {
                Some(color) => color,
                None => self.foreground,
            },
            dim: self.dim,
            accent: match style.accent {
                Some(color) => color,
                None => self.accent,
            },
            surface: match style.surface {
                Some(color) => color,
                None => self.surface,
            },
            surface_hover: self.surface_hover,
            surface_pressed: self.surface_pressed,
            border: match style.border {
                Some(color) => color,
                None => self.border,
            },
        }
    }
}
