//! Two hand-tuned palettes, dark and light. Everything the TUI paints goes
//! through one `Palette`, so theming is a single struct swap.

use ratatui::style::Color;

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub dim: Color,
    pub accent: Color,
    pub accent2: Color,
    pub border: Color,
    pub sel_bg: Color,
    pub danger: Color,
    pub ok: Color,
    pub warn: Color,
    /// Size-bar fills: >50%, >20%, rest of the largest sibling.
    pub bar_hi: Color,
    pub bar_mid: Color,
    pub bar_lo: Color,
    pub mark: Color,
}

impl Palette {
    pub const DARK: Palette = Palette {
        bg: Color::Rgb(13, 15, 18),
        fg: Color::Rgb(216, 222, 233),
        dim: Color::Rgb(110, 118, 132),
        accent: Color::Rgb(102, 187, 255),
        accent2: Color::Rgb(255, 183, 77),
        border: Color::Rgb(52, 58, 70),
        sel_bg: Color::Rgb(36, 42, 54),
        danger: Color::Rgb(255, 107, 107),
        ok: Color::Rgb(135, 214, 132),
        warn: Color::Rgb(255, 193, 94),
        bar_hi: Color::Rgb(255, 107, 107),
        bar_mid: Color::Rgb(255, 193, 94),
        bar_lo: Color::Rgb(74, 140, 120),
        mark: Color::Rgb(255, 183, 77),
    };

    pub const LIGHT: Palette = Palette {
        bg: Color::Rgb(247, 246, 243),
        fg: Color::Rgb(40, 44, 52),
        dim: Color::Rgb(120, 128, 140),
        accent: Color::Rgb(0, 102, 180),
        accent2: Color::Rgb(170, 98, 0),
        border: Color::Rgb(205, 203, 196),
        sel_bg: Color::Rgb(222, 229, 240),
        danger: Color::Rgb(186, 26, 26),
        ok: Color::Rgb(22, 120, 60),
        warn: Color::Rgb(160, 110, 0),
        bar_hi: Color::Rgb(186, 26, 26),
        bar_mid: Color::Rgb(160, 110, 0),
        bar_lo: Color::Rgb(60, 130, 110),
        mark: Color::Rgb(170, 98, 0),
    };

    pub fn name(self) -> &'static str {
        if self == Self::DARK {
            "dark"
        } else {
            "light"
        }
    }

    pub fn toggle(&mut self) {
        *self = if *self == Self::DARK {
            Self::LIGHT
        } else {
            Self::DARK
        };
    }
}
