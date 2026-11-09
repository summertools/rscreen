//! A single terminal cell.

use std::sync::Arc;
use unicode_width::UnicodeWidthChar;

use super::style::Style;

/// One cell in the terminal grid.
///
/// Wide glyphs occupy two cells: the leading cell has `wide == true`, the
/// trailing cell has `cont == true` and is never rendered on its own. This
/// makes cursor movement and diff rendering predictable even though terminals
/// use a width-based addressing model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Zero-width combining marks that follow `ch`.
    pub extra: Option<Box<str>>,
    pub style: Style,
    /// OSC 8 hyperlink target, if any.
    pub link: Option<Arc<str>>,
    pub wide: bool,
    pub cont: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(Style::default())
    }
}

impl Cell {
    pub const fn blank(style: Style) -> Self {
        Self {
            ch: ' ',
            extra: None,
            style,
            link: None,
            wide: false,
            cont: false,
        }
    }

    pub const fn ch(ch: char, style: Style) -> Self {
        Self {
            ch,
            extra: None,
            style,
            link: None,
            wide: false,
            cont: false,
        }
    }

    pub fn with_link(mut self, link: Option<Arc<str>>) -> Self {
        self.link = link;
        self
    }

    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.extra.is_none() && !self.wide && !self.cont
    }

    /// Reset this cell to a blank with a new style.
    pub fn reset(&mut self, style: Style) {
        *self = Self::blank(style);
    }

    pub fn append_combining(&mut self, c: char) {
        match &mut self.extra {
            Some(s) => {
                let mut new = String::with_capacity(s.len() + c.len_utf8());
                new.push_str(s);
                new.push(c);
                *s = new.into_boxed_str();
            }
            None => {
                let mut s = String::with_capacity(c.len_utf8());
                s.push(c);
                self.extra = Some(s.into_boxed_str());
            }
        }
    }

    /// The full grapheme text stored in this cell (without the continuation
    /// marker).
    pub fn text(&self) -> String {
        let mut s = String::new();
        if self.cont {
            return s;
        }
        s.push(self.ch);
        if let Some(extra) = &self.extra {
            s.push_str(extra);
        }
        s
    }

    /// Display width of this cell as a leading cell.
    pub fn display_width(&self) -> usize {
        if self.cont {
            return 0;
        }
        if self.wide {
            return 2;
        }
        self.ch.width().unwrap_or(1).max(1)
    }

    /// Whether the glyph at this position should be emitted by the renderer.
    pub fn is_renderable(&self) -> bool {
        !self.cont
    }
}

/// Display width of a character, with control characters treated as zero.
pub fn char_width(c: char) -> usize {
    match c.width() {
        Some(0) => 0,
        Some(w) if w > 2 => 2,
        Some(w) => w,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::style::Style;

    #[test]
    fn widths() {
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('中'), 2);
        assert_eq!(char_width('\u{0301}'), 0);
        assert_eq!(char_width('\u{1b}'), 0);
    }

    #[test]
    fn combining_marks() {
        let mut c = Cell::ch('e', Style::default());
        c.append_combining('\u{0301}');
        assert_eq!(c.text(), "e\u{0301}");
        assert_eq!(c.display_width(), 1);
    }
}
