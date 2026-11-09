//! Colors and text attributes used by the terminal emulator.

use std::fmt;

/// A terminal color.
///
/// `Default` means "use the terminal's default foreground/background".
/// Indexed colors cover the 256-color palette; `Rgb` is 24-bit true color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// Parse a config color such as `#89b4fa`, `blue`, or `12`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(hex) = s.strip_prefix('#') {
            if hex.len() == 6 {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                return Some(Color::Rgb(r, g, b));
            }
        }
        if let Ok(n) = s.parse::<u8>() {
            return Some(Color::Indexed(n));
        }
        let names = [
            ("black", 0),
            ("red", 1),
            ("green", 2),
            ("yellow", 3),
            ("blue", 4),
            ("magenta", 5),
            ("cyan", 6),
            ("white", 7),
            ("brightblack", 8),
            ("brightred", 9),
            ("brightgreen", 10),
            ("brightyellow", 11),
            ("brightblue", 12),
            ("brightmagenta", 13),
            ("brightcyan", 14),
            ("brightwhite", 15),
        ];
        names
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(s))
            .map(|(_, n)| Color::Indexed(*n))
    }

    /// Approximate an RGB color with the xterm 256 color cube.
    pub fn to_256(self) -> Self {
        match self {
            Color::Rgb(r, g, b) => {
                // Grayscale ramp is usually much better for near-gray colors.
                let ri = r as i32;
                let gi = g as i32;
                let bi = b as i32;
                let avg = (ri + gi + bi) / 3;
                let spread = (ri - gi).abs().max((gi - bi).abs()).max((ri - bi).abs());
                if spread < 12 {
                    let idx = if avg < 8 {
                        16
                    } else if avg > 238 {
                        231
                    } else {
                        232 + ((avg - 8) * 24 / 247).min(23)
                    };
                    return Color::Indexed(idx as u8);
                }
                let q = |v: i32| -> i32 {
                    if v < 48 {
                        0
                    } else if v < 115 {
                        1
                    } else {
                        (v - 35) / 40
                    }
                };
                let (qr, qg, qb) = (q(ri), q(gi), q(bi));
                let idx = 16 + 36 * qr + 6 * qg + qb;
                Color::Indexed(idx as u8)
            }
            other => other,
        }
    }

    pub fn is_default(self) -> bool {
        self == Color::Default
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Color::Default => write!(f, "default"),
            Color::Indexed(n) => write!(f, "{}", n),
            Color::Rgb(r, g, b) => write!(f, "#{:02x}{:02x}{:02x}", r, g, b),
        }
    }
}

/// Text attribute bit flags.
pub mod attrs {
    pub const BOLD: u16 = 1 << 0;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const BLINK: u16 = 1 << 4;
    pub const REVERSE: u16 = 1 << 5;
    pub const HIDDEN: u16 = 1 << 6;
    pub const STRIKE: u16 = 1 << 7;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 8;
    pub const CURLY_UNDERLINE: u16 = 1 << 9;
    pub const DOTTED_UNDERLINE: u16 = 1 << 10;
    pub const DASHED_UNDERLINE: u16 = 1 << 11;

    pub const UNDERLINE_MASK: u16 =
        UNDERLINE | DOUBLE_UNDERLINE | CURLY_UNDERLINE | DOTTED_UNDERLINE | DASHED_UNDERLINE;
}

/// A rendered style: foreground, background and attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attrs: u16,
}

impl Style {
    pub const fn new(fg: Color, bg: Color, attrs: u16) -> Self {
        Self { fg, bg, attrs }
    }

    pub const fn fg(mut self, fg: Color) -> Self {
        self.fg = fg;
        self
    }

    pub const fn bg(mut self, bg: Color) -> Self {
        self.bg = bg;
        self
    }

    pub const fn bold(mut self) -> Self {
        self.attrs |= attrs::BOLD;
        self
    }

    /// The style used to erase cells: preserve only the background color
    /// (back-color-erase semantics).
    pub fn erase(self) -> Self {
        Self {
            fg: Color::Default,
            bg: self.bg,
            attrs: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(Color::parse("#89b4fa"), Some(Color::Rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(Color::parse("red"), Some(Color::Indexed(1)));
        assert_eq!(Color::parse("200"), Some(Color::Indexed(200)));
        assert_eq!(Color::parse("nope"), None);
    }
}
