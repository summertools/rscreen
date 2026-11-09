//! A VT100/xterm-compatible terminal screen model.
//!
//! This is the heart of rscreen: it interprets the byte stream coming out of a
//! pseudo terminal and maintains a grid of styled cells, scrollback, cursor
//! state and terminal modes. Rendering to the user's terminal is a separate
//! concern (see `server::ui`).
//!
//! Design notes:
//! - Wide characters occupy two cells (lead + continuation). Every mutation
//!   that can split a wide glyph repairs the pair afterwards.
//! - `wrap_pending` models the "last column" state: after printing in the last
//!   column, the cursor stays there until the *next* printable character wraps.
//! - The alternate screen is a separate buffer; the primary screen and its
//!   scrollback are preserved untouched while a full-screen app is running.

use std::collections::VecDeque;
use std::sync::Arc;

use vte::{Params, Perform};

use super::cell::{char_width, Cell};
use super::style::{attrs, Color, Style};

/// Approximate per-pane scrollback budget. The line limit alone is not enough:
/// wide terminals can make a few thousand lines consume hundreds of MiB.
const SCROLLBACK_BYTE_LIMIT: usize = 32 * 1024 * 1024;
const CELL_BYTES: usize = std::mem::size_of::<Cell>();

/// Events produced while parsing output. The server reacts to these (bells,
/// title changes, clipboard requests...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Bell,
    Title(String),
    Clipboard(String),
    Cwd(String),
    /// OSC 9 / OSC 777 desktop notification-ish message.
    Notify(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub pending_wrap: bool,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charset {
    Ascii,
    DecGraphics,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MouseMode {
    #[default]
    Off,
    X10,
    Normal,
    Button,
    Any,
}

/// Terminal modes shared by the primary and alternate buffers.
#[derive(Clone, Debug)]
pub struct Modes {
    pub autowrap: bool,
    pub origin: bool,
    pub insert: bool,
    pub newline: bool,
    pub cursor_visible: bool,
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub mouse: MouseMode,
    pub mouse_sgr: bool,
    pub mouse_utf8: bool,
    pub alternate_scroll: bool,
    pub reverse_video: bool,
    pub sync_output: bool,
}

impl Default for Modes {
    fn default() -> Self {
        Self {
            autowrap: true,
            origin: false,
            insert: false,
            newline: false,
            cursor_visible: true,
            app_cursor: false,
            app_keypad: false,
            bracketed_paste: false,
            focus_events: false,
            mouse: MouseMode::Off,
            mouse_sgr: false,
            mouse_utf8: false,
            alternate_scroll: false,
            reverse_video: false,
            sync_output: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Buffer {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub wrapped: Vec<bool>,
    pub scrollback: VecDeque<Row>,
    pub scrollback_limit: usize,
    pub scrollback_bytes: usize,
    pub cursor: Cursor,
    pub top: usize,
    pub bottom: usize,
}

impl Buffer {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(Style::default()); cols * rows],
            wrapped: vec![false; rows],
            scrollback: VecDeque::new(),
            scrollback_limit,
            scrollback_bytes: 0,
            cursor: Cursor::default(),
            top: 0,
            bottom: rows - 1,
        }
    }

    #[inline]
    pub fn row(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    #[inline]
    pub fn row_mut(&mut self, row: usize) -> &mut [Cell] {
        let start = row * self.cols;
        &mut self.cells[start..start + self.cols]
    }

    #[inline]
    pub fn idx(&self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }

    #[inline]
    pub fn get(&self, row: usize, col: usize) -> &Cell {
        &self.cells[self.idx(row, col)]
    }

    #[inline]
    pub fn get_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let i = self.idx(row, col);
        &mut self.cells[i]
    }

    /// Number of virtual rows (scrollback + live grid).
    pub fn total_rows(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// Get a row by virtual index: 0..scrollback_len are history.
    pub fn virtual_row(&self, index: usize) -> Option<&[Cell]> {
        if index < self.scrollback.len() {
            Some(&self.scrollback[index].cells)
        } else {
            let live = index - self.scrollback.len();
            if live < self.rows {
                Some(self.row(live))
            } else {
                None
            }
        }
    }

    pub fn virtual_wrapped(&self, index: usize) -> bool {
        if index < self.scrollback.len() {
            self.scrollback[index].wrapped
        } else {
            let live = index - self.scrollback.len();
            self.wrapped.get(live).copied().unwrap_or(false)
        }
    }

    /// Extract a row as text without continuation cells and without trailing
    /// blanks.
    pub fn row_text(&self, index: usize) -> String {
        let Some(cells) = self.virtual_row(index) else {
            return String::new();
        };
        let mut s = String::with_capacity(cells.len());
        for cell in cells {
            if cell.cont {
                continue;
            }
            s.push(cell.ch);
            if let Some(extra) = &cell.extra {
                s.push_str(extra);
            }
        }
        while s.ends_with(' ') {
            s.pop();
        }
        s
    }

    pub fn push_scrollback(&mut self, row: Row) {
        if self.scrollback_limit == 0 {
            return;
        }
        self.scrollback_bytes = self
            .scrollback_bytes
            .saturating_add(row.cells.len().saturating_mul(CELL_BYTES));
        self.scrollback.push_back(row);
        while self.scrollback.len() > self.scrollback_limit
            || self.scrollback_bytes > SCROLLBACK_BYTE_LIMIT
        {
            if let Some(front) = self.scrollback.pop_front() {
                self.scrollback_bytes = self
                    .scrollback_bytes
                    .saturating_sub(front.cells.len().saturating_mul(CELL_BYTES));
            } else {
                self.scrollback_bytes = 0;
                break;
            }
        }
    }

    /// Repair a row after cell-shifting operations.
    fn normalize_wide_row(&mut self, row: usize) {
        let cols = self.cols;
        for col in 0..cols {
            let (wide, cont) = {
                let c = self.get(row, col);
                (c.wide, c.cont)
            };
            if cont {
                let valid = col > 0 && self.get(row, col - 1).wide;
                if !valid {
                    let style = self.get(row, col).style;
                    self.get_mut(row, col).reset(style);
                }
            } else if wide {
                let valid = col + 1 < cols && self.get(row, col + 1).cont;
                if !valid {
                    let style = self.get(row, col).style;
                    self.get_mut(row, col).reset(style);
                }
            }
        }
    }

    /// Erase `[start, end)` in a row, expanding to cover whole wide glyphs.
    pub fn clear_range(&mut self, row: usize, mut start: usize, mut end: usize, style: Style) {
        let cols = self.cols;
        start = start.min(cols);
        end = end.min(cols);
        if start >= end {
            return;
        }
        if self.get(row, start).cont && start > 0 {
            start -= 1;
        }
        if self.get(row, end - 1).wide && end < cols {
            end += 1;
        }
        for col in start..end {
            self.get_mut(row, col).reset(style);
        }
        self.normalize_wide_row(row);
    }

    pub fn clear_row(&mut self, row: usize, style: Style) {
        let cols = self.cols;
        self.clear_range(row, 0, cols, style);
        self.wrapped[row] = false;
    }

    pub fn scroll_region_up(
        &mut self,
        top: usize,
        bottom: usize,
        n: usize,
        style: Style,
        keep: bool,
    ) {
        if top > bottom || bottom >= self.rows {
            return;
        }
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            if keep && top == 0 && bottom == self.rows - 1 {
                let cells = self.row(0).to_vec();
                let wrapped = self.wrapped[0];
                self.push_scrollback(Row { cells, wrapped });
            }
            for row in top..bottom {
                let next = self.row(row + 1).to_vec();
                let next_wrapped = self.wrapped[row + 1];
                self.row_mut(row).clone_from_slice(&next);
                self.wrapped[row] = next_wrapped;
            }
            self.clear_row(bottom, style);
        }
    }

    pub fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize, style: Style) {
        if top > bottom || bottom >= self.rows {
            return;
        }
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            for row in (top + 1..=bottom).rev() {
                let prev = self.row(row - 1).to_vec();
                let prev_wrapped = self.wrapped[row - 1];
                self.row_mut(row).clone_from_slice(&prev);
                self.wrapped[row] = prev_wrapped;
            }
            self.clear_row(top, style);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SavedState {
    cursor: Cursor,
    pen: Style,
    origin: bool,
    autowrap: bool,
    charsets: [Charset; 2],
    active_charset: usize,
}

/// The complete terminal state for one pane.
pub struct Screen {
    pub primary: Buffer,
    pub alternate: Option<Box<Buffer>>,
    pub alt_active: bool,
    pub modes: Modes,
    pub pen: Style,
    pub charsets: [Charset; 2],
    pub active_charset: usize,
    pub cursor_style: u8,
    pub tabs: Vec<bool>,
    pub last_char: Option<char>,
    pub title: String,
    pub cwd: Option<String>,
    pub responses: Vec<u8>,
    pub events: Vec<Event>,
    saved: SavedState,
    alt_return: Option<(SavedState, usize, usize)>,
    scrollback_limit: usize,
    /// OSC 8 hyperlink currently in effect.
    current_link: Option<Arc<str>>,
    /// Last cell that received a printed glyph, used for combining marks.
    last_print: Option<(usize, usize)>,
}

impl Screen {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let tabs = default_tabs(cols);
        Self {
            primary: Buffer::new(cols, rows, scrollback_limit),
            alternate: None,
            alt_active: false,
            modes: Modes::default(),
            pen: Style::default(),
            charsets: [Charset::Ascii, Charset::Ascii],
            active_charset: 0,
            cursor_style: 0,
            tabs,
            last_char: None,
            title: String::new(),
            cwd: None,
            responses: Vec::new(),
            events: Vec::new(),
            saved: SavedState {
                cursor: Cursor::default(),
                pen: Style::default(),
                origin: false,
                autowrap: true,
                charsets: [Charset::Ascii, Charset::Ascii],
                active_charset: 0,
            },
            alt_return: None,
            scrollback_limit,
            current_link: None,
            last_print: None,
        }
    }

    #[inline]
    pub fn cols(&self) -> usize {
        self.active().cols
    }

    #[inline]
    pub fn rows(&self) -> usize {
        self.active().rows
    }

    #[inline]
    pub fn active(&self) -> &Buffer {
        if self.alt_active {
            self.alternate.as_ref().expect("alt buffer active")
        } else {
            &self.primary
        }
    }

    #[inline]
    pub fn active_mut(&mut self) -> &mut Buffer {
        if self.alt_active {
            self.alternate.as_mut().expect("alt buffer active")
        } else {
            &mut self.primary
        }
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.responses)
    }

    fn respond(&mut self, bytes: &[u8]) {
        if self.responses.len() < 1 << 20 {
            self.responses.extend_from_slice(bytes);
        }
    }

    fn erase_style(&self) -> Style {
        self.pen.erase()
    }

    // ------------------------------------------------------------------
    // Cursor movement
    // ------------------------------------------------------------------

    pub fn goto(&mut self, row: usize, col: usize) {
        let (top, bottom) = {
            let b = self.active();
            (b.top, b.bottom)
        };
        let row = if self.modes.origin {
            (top + row).min(bottom)
        } else {
            row.min(self.active().rows.saturating_sub(1))
        };
        let col = col.min(self.active().cols.saturating_sub(1));
        let b = self.active_mut();
        b.cursor.row = row;
        b.cursor.col = col;
        b.cursor.pending_wrap = false;
    }

    fn move_up(&mut self, n: usize) {
        let (top, row, rows) = {
            let b = self.active();
            (b.top, b.cursor.row, b.rows)
        };
        let target = row.saturating_sub(n);
        let target = if row >= top {
            target.max(top)
        } else {
            target.min(rows.saturating_sub(1))
        };
        let b = self.active_mut();
        b.cursor.row = target;
        b.cursor.pending_wrap = false;
    }

    fn move_down(&mut self, n: usize) {
        let (bottom, row, rows) = {
            let b = self.active();
            (b.bottom, b.cursor.row, b.rows)
        };
        let target = row.saturating_add(n).min(rows.saturating_sub(1));
        let target = if row <= bottom {
            target.min(bottom)
        } else {
            target
        };
        let b = self.active_mut();
        b.cursor.row = target;
        b.cursor.pending_wrap = false;
    }

    fn move_left(&mut self, n: usize) {
        let b = self.active_mut();
        b.cursor.col = b.cursor.col.saturating_sub(n);
        b.cursor.pending_wrap = false;
    }

    fn move_right(&mut self, n: usize) {
        let cols = self.active().cols;
        let b = self.active_mut();
        b.cursor.col = (b.cursor.col + n).min(cols.saturating_sub(1));
        b.cursor.pending_wrap = false;
    }

    fn carriage_return(&mut self) {
        let b = self.active_mut();
        b.cursor.col = 0;
        b.cursor.pending_wrap = false;
    }

    fn backspace(&mut self) {
        let b = self.active_mut();
        b.cursor.pending_wrap = false;
        b.cursor.col = b.cursor.col.saturating_sub(1);
    }

    fn tab(&mut self) {
        let cols = self.active().cols;
        let mut col = self.active().cursor.col + 1;
        while col < cols && !self.tabs.get(col).copied().unwrap_or(false) {
            col += 1;
        }
        let b = self.active_mut();
        b.cursor.col = col.min(cols.saturating_sub(1));
        b.cursor.pending_wrap = false;
    }

    fn back_tab(&mut self, n: usize) {
        let cols = self.active().cols;
        let mut col = self.active().cursor.col;
        for _ in 0..n {
            if col == 0 {
                break;
            }
            col -= 1;
            while col > 0 && !self.tabs.get(col).copied().unwrap_or(false) {
                col -= 1;
            }
        }
        let b = self.active_mut();
        b.cursor.col = col.min(cols.saturating_sub(1));
        b.cursor.pending_wrap = false;
    }

    fn index(&mut self) {
        let (row, bottom) = {
            let b = self.active();
            (b.cursor.row, b.bottom)
        };
        if row == bottom {
            self.scroll_up(1);
        } else if row + 1 < self.active().rows {
            self.active_mut().cursor.row = row + 1;
        }
        self.active_mut().cursor.pending_wrap = false;
    }

    fn reverse_index(&mut self) {
        let (row, top) = {
            let b = self.active();
            (b.cursor.row, b.top)
        };
        if row == top {
            self.scroll_down(1);
        } else if row > 0 {
            self.active_mut().cursor.row = row - 1;
        }
        self.active_mut().cursor.pending_wrap = false;
    }

    fn linefeed(&mut self) {
        self.index();
        if self.modes.newline {
            self.carriage_return();
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let (top, bottom) = (self.active().top, self.active().bottom);
        let style = self.erase_style();
        let keep = !self.alt_active && top == 0 && bottom + 1 == self.active().rows;
        self.active_mut()
            .scroll_region_up(top, bottom, n, style, keep);
    }

    fn scroll_down(&mut self, n: usize) {
        let (top, bottom) = (self.active().top, self.active().bottom);
        let style = self.erase_style();
        self.active_mut().scroll_region_down(top, bottom, n, style);
    }

    /// Wrap to the next line, marking the current row as wrapped.
    fn wrap_line(&mut self) {
        let (row, bottom, rows) = {
            let b = self.active();
            (b.cursor.row, b.bottom, b.rows)
        };
        {
            let b = self.active_mut();
            b.wrapped[row] = true;
            b.cursor.col = 0;
            b.cursor.pending_wrap = false;
        }
        if row == bottom {
            self.scroll_up(1);
        } else if row + 1 < rows {
            self.active_mut().cursor.row = row + 1;
        }
    }

    // ------------------------------------------------------------------
    // Printing
    // ------------------------------------------------------------------

    fn append_combining(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let (row, col, pending) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col, b.cursor.pending_wrap)
        };
        let (target_row, target_col) = if pending {
            (row, col)
        } else if col > 0 {
            (row, col - 1)
        } else if row > 0 {
            let previous = (row - 1, self.active().cols - 1);
            let wrapped = self.active().wrapped[row - 1];
            // Only attach to the previous row when the cursor actually just
            // auto-wrapped there. After CR/LF/CUP a combining mark has no
            // preceding glyph on this line.
            if wrapped && self.last_print == Some(previous) {
                previous
            } else {
                return;
            }
        } else {
            return;
        };
        let b = self.active_mut();
        let is_cont = b.get(target_row, target_col).cont;
        let (lead_row, lead_col) = if is_cont && target_col > 0 {
            (target_row, target_col - 1)
        } else {
            (target_row, target_col)
        };
        b.get_mut(lead_row, lead_col).append_combining(c);
    }

    fn clear_wide_overlap(&mut self, row: usize, col: usize, width: usize) {
        let cols = self.active().cols;
        let b = self.active_mut();
        if b.get(row, col).cont && col > 0 {
            let style = b.get(row, col - 1).style;
            b.get_mut(row, col - 1).reset(style);
        }
        if b.get(row, col).wide && col + 1 < cols {
            let style = b.get(row, col + 1).style;
            b.get_mut(row, col + 1).reset(style);
        }
        if width == 2 && col + 1 < cols && b.get(row, col + 1).wide && col + 2 < cols {
            let style = b.get(row, col + 2).style;
            b.get_mut(row, col + 2).reset(style);
        }
    }

    fn put_char(&mut self, c: char) {
        if c.is_control() {
            // C1 controls can arrive as UTF-8 from an application. They must
            // never become printable cells, otherwise a frame could smuggle
            // escape-like bytes back to the outer terminal.
            return;
        }
        let width = char_width(c);
        if width == 0 {
            self.append_combining(c);
            return;
        }

        if self.modes.insert {
            let pending = self.active().cursor.pending_wrap;
            if !pending {
                self.insert_chars(width);
            }
        }

        let autowrap = self.modes.autowrap;
        let pending = self.active().cursor.pending_wrap;
        if pending {
            if autowrap {
                self.wrap_line();
            } else {
                self.active_mut().cursor.pending_wrap = false;
            }
        }

        let cols = self.active().cols;
        let (mut row, mut col) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col)
        };
        if width == 2 && col + 1 >= cols {
            if autowrap {
                self.wrap_line();
                row = self.active().cursor.row;
                col = self.active().cursor.col;
            } else {
                let style = self.pen;
                self.clear_wide_overlap(row, col, 1);
                let b = self.active_mut();
                b.get_mut(row, col).reset(style);
                b.cursor.pending_wrap = false;
                return;
            }
        }
        // A one-column screen cannot represent a double-width glyph even
        // after wrapping. Keep the grid invariant intact.
        if width == 2 && col + 1 >= cols {
            let style = self.pen;
            self.clear_wide_overlap(row, col, 1);
            let b = self.active_mut();
            b.get_mut(row, col).reset(style);
            b.cursor.col = col.min(cols.saturating_sub(1));
            b.cursor.pending_wrap = true;
            return;
        }

        self.clear_wide_overlap(row, col, width);

        let mapped = self.map_charset(c);
        let style = self.pen;
        let link = self.current_link.clone();
        {
            let b = self.active_mut();
            let cell = b.get_mut(row, col);
            *cell = Cell::ch(mapped, style).with_link(link.clone());
            cell.wide = width == 2;
            if width == 2 && col + 1 < cols {
                let trailing = b.get_mut(row, col + 1);
                *trailing = Cell::blank(style).with_link(link);
                trailing.cont = true;
            }
            if col + width >= cols {
                b.cursor.col = cols - 1;
                b.cursor.pending_wrap = true;
            } else {
                b.cursor.col = col + width;
                b.cursor.pending_wrap = false;
            }
            b.cursor.row = row;
        }
        self.last_char = Some(c);
        self.last_print = Some((row, col));
    }

    fn map_charset(&self, c: char) -> char {
        if self.charsets[self.active_charset] == Charset::DecGraphics {
            dec_graphics(c).unwrap_or(c)
        } else {
            c
        }
    }

    fn repeat_last(&mut self, n: usize) {
        if let Some(c) = self.last_char {
            for _ in 0..n.min(10_000) {
                self.put_char(c);
            }
        }
    }

    // ------------------------------------------------------------------
    // Erasing / editing
    // ------------------------------------------------------------------

    fn erase_in_line(&mut self, mode: u16) {
        let (row, col, cols) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col, b.cols)
        };
        let style = self.erase_style();
        let b = self.active_mut();
        match mode {
            0 => b.clear_range(row, col, cols, style),
            1 => b.clear_range(row, 0, col + 1, style),
            2 => b.clear_row(row, style),
            _ => {}
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        let (row, col, rows, cols) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col, b.rows, b.cols)
        };
        let style = self.erase_style();
        let b = self.active_mut();
        match mode {
            0 => {
                b.clear_range(row, col, cols, style);
                for r in row + 1..rows {
                    b.clear_row(r, style);
                }
            }
            1 => {
                for r in 0..row {
                    b.clear_row(r, style);
                }
                b.clear_range(row, 0, col + 1, style);
            }
            2 => {
                for r in 0..rows {
                    b.clear_row(r, style);
                }
            }
            3 => {
                b.scrollback.clear();
                b.scrollback_bytes = 0;
            }
            _ => {}
        }
    }

    fn erase_chars(&mut self, n: usize) {
        let (row, col, cols) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col, b.cols)
        };
        let style = self.erase_style();
        self.active_mut()
            .clear_range(row, col, (col + n).min(cols), style);
    }

    fn insert_chars(&mut self, n: usize) {
        let (row, col, cols) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col, b.cols)
        };
        if n == 0 || col >= cols {
            return;
        }
        let style = self.erase_style();
        let n = n.min(cols - col);
        let b = self.active_mut();
        {
            let old = b.row(row).to_vec();
            let mut new = old.clone();
            for i in (col..cols - n).rev() {
                new[i + n] = old[i].clone();
            }
            for cell in &mut new[col..col + n] {
                cell.reset(style);
            }
            b.row_mut(row).clone_from_slice(&new);
        }
        b.normalize_wide_row(row);
    }

    fn delete_chars(&mut self, n: usize) {
        let (row, col, cols) = {
            let b = self.active();
            (b.cursor.row, b.cursor.col, b.cols)
        };
        if n == 0 || col >= cols {
            return;
        }
        let style = self.erase_style();
        let n = n.min(cols - col);
        let b = self.active_mut();
        {
            let old = b.row(row).to_vec();
            let mut new = old.clone();
            for i in 0..cols - col - n {
                new[col + i] = old[col + n + i].clone();
            }
            for cell in &mut new[cols - n..cols] {
                cell.reset(style);
            }
            b.row_mut(row).clone_from_slice(&new);
        }
        b.normalize_wide_row(row);
    }

    fn insert_lines(&mut self, n: usize) {
        let (row, top, bottom) = {
            let b = self.active();
            (b.cursor.row, b.top, b.bottom)
        };
        if row < top || row > bottom {
            return;
        }
        let style = self.erase_style();
        let count = bottom - row + 1;
        let n = n.min(count);
        let b = self.active_mut();
        if n < count {
            for r in (row..=bottom - n).rev() {
                let src = b.row(r).to_vec();
                let src_wrapped = b.wrapped[r];
                b.row_mut(r + n).clone_from_slice(&src);
                b.wrapped[r + n] = src_wrapped;
            }
        }
        for r in row..row + n {
            b.clear_row(r, style);
        }
    }

    fn delete_lines(&mut self, n: usize) {
        let (row, top, bottom) = {
            let b = self.active();
            (b.cursor.row, b.top, b.bottom)
        };
        if row < top || row > bottom {
            return;
        }
        let style = self.erase_style();
        let count = bottom - row + 1;
        let n = n.min(count);
        let b = self.active_mut();
        if n < count {
            for r in row..=bottom - n {
                let src = b.row(r + n).to_vec();
                let src_wrapped = b.wrapped[r + n];
                b.row_mut(r).clone_from_slice(&src);
                b.wrapped[r] = src_wrapped;
            }
        }
        for r in bottom + 1 - n..=bottom {
            b.clear_row(r, style);
        }
    }

    // ------------------------------------------------------------------
    // Modes
    // ------------------------------------------------------------------

    fn set_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            1 => self.modes.app_cursor = on,
            3 => {}
            4 => self.modes.insert = on,
            5 => self.modes.reverse_video = on,
            6 => {
                self.modes.origin = on;
                self.goto(0, 0);
            }
            7 => self.modes.autowrap = on,
            12 => {}
            25 => self.modes.cursor_visible = on,
            47 | 1047 => {
                if on {
                    self.enter_alt(false, false);
                } else {
                    self.exit_alt(false);
                }
            }
            66 => self.modes.app_keypad = on,
            9 => self.set_mouse_mode(on, MouseMode::X10),
            1000 => self.set_mouse_mode(on, MouseMode::Normal),
            1002 => self.set_mouse_mode(on, MouseMode::Button),
            1003 => self.set_mouse_mode(on, MouseMode::Any),
            1004 => self.modes.focus_events = on,
            1005 => self.modes.mouse_utf8 = on,
            1006 => self.modes.mouse_sgr = on,
            1015 => self.modes.mouse_sgr = on,
            1048 => {
                if on {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1049 => {
                if on {
                    self.enter_alt(true, true);
                } else {
                    self.exit_alt(true);
                }
            }
            2004 => self.modes.bracketed_paste = on,
            2026 => self.modes.sync_output = on,
            1007 => self.modes.alternate_scroll = on,
            _ => {}
        }
    }

    fn set_mouse_mode(&mut self, on: bool, mode: MouseMode) {
        if on {
            self.modes.mouse = mode;
        } else if self.modes.mouse == mode {
            self.modes.mouse = MouseMode::Off;
        }
    }

    fn mode_status(&self, mode: u16) -> Option<bool> {
        Some(match mode {
            1 => self.modes.app_cursor,
            4 => self.modes.insert,
            5 => self.modes.reverse_video,
            6 => self.modes.origin,
            7 => self.modes.autowrap,
            25 => self.modes.cursor_visible,
            47 => self.alt_active,
            66 => self.modes.app_keypad,
            9 => self.modes.mouse == MouseMode::X10,
            1000 => self.modes.mouse == MouseMode::Normal,
            1002 => self.modes.mouse == MouseMode::Button,
            1003 => self.modes.mouse == MouseMode::Any,
            1004 => self.modes.focus_events,
            1005 => self.modes.mouse_utf8,
            1006 => self.modes.mouse_sgr,
            1015 => self.modes.mouse_sgr,
            1047 => self.alt_active,
            1048 => false,
            1049 => self.alt_active,
            2004 => self.modes.bracketed_paste,
            2026 => self.modes.sync_output,
            _ => return None,
        })
    }

    fn set_ansi_mode(&mut self, mode: u16, on: bool) {
        match mode {
            4 => self.modes.insert = on,
            20 => self.modes.newline = on,
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Alternate screen / save & restore
    // ------------------------------------------------------------------

    fn save_cursor(&mut self) {
        let cursor = self.active().cursor;
        self.saved = SavedState {
            cursor,
            pen: self.pen,
            origin: self.modes.origin,
            autowrap: self.modes.autowrap,
            charsets: self.charsets,
            active_charset: self.active_charset,
        };
    }

    fn restore_cursor(&mut self) {
        let saved = self.saved;
        self.modes.origin = saved.origin;
        self.modes.autowrap = saved.autowrap;
        self.pen = saved.pen;
        self.charsets = saved.charsets;
        self.active_charset = saved.active_charset;
        let b = self.active_mut();
        b.cursor = saved.cursor;
        b.cursor.row = b.cursor.row.min(b.rows - 1);
        b.cursor.col = b.cursor.col.min(b.cols - 1);
        b.cursor.pending_wrap = false;
    }

    fn enter_alt(&mut self, _clear: bool, save: bool) {
        if self.alt_active {
            return;
        }
        let cols = self.primary.cols;
        let rows = self.primary.rows;
        let mut alt = Box::new(Buffer::new(cols, rows, 0));
        alt.cursor = Cursor::default();
        alt.top = 0;
        alt.bottom = rows.saturating_sub(1);
        self.alternate = Some(alt);
        self.alt_active = true;
        self.alt_return = if save {
            Some((
                SavedState {
                    cursor: self.primary.cursor,
                    pen: self.pen,
                    origin: self.modes.origin,
                    autowrap: self.modes.autowrap,
                    charsets: self.charsets,
                    active_charset: self.active_charset,
                },
                self.primary.top,
                self.primary.bottom,
            ))
        } else {
            None
        };
    }

    fn exit_alt(&mut self, restore: bool) {
        if !self.alt_active {
            return;
        }
        self.alternate = None;
        self.alt_active = false;
        if restore {
            if let Some((state, top, bottom)) = self.alt_return.take() {
                self.modes.origin = state.origin;
                self.modes.autowrap = state.autowrap;
                self.pen = state.pen;
                self.charsets = state.charsets;
                self.active_charset = state.active_charset;
                self.primary.cursor = state.cursor;
                self.primary.cursor.row = self
                    .primary
                    .cursor
                    .row
                    .min(self.primary.rows.saturating_sub(1));
                self.primary.cursor.col = self
                    .primary
                    .cursor
                    .col
                    .min(self.primary.cols.saturating_sub(1));
                self.primary.cursor.pending_wrap = false;
                self.primary.top = top.min(self.primary.rows.saturating_sub(1));
                self.primary.bottom = bottom.min(self.primary.rows.saturating_sub(1));
                if self.primary.top >= self.primary.bottom {
                    self.primary.top = 0;
                    self.primary.bottom = self.primary.rows.saturating_sub(1);
                }
            }
        }
        self.primary.cursor.pending_wrap = false;
    }

    fn full_reset(&mut self) {
        let cols = self.primary.cols;
        let rows = self.primary.rows;
        let limit = self.scrollback_limit;
        self.primary = Buffer::new(cols, rows, limit);
        self.alternate = None;
        self.alt_active = false;
        self.modes = Modes::default();
        self.pen = Style::default();
        self.charsets = [Charset::Ascii, Charset::Ascii];
        self.active_charset = 0;
        self.cursor_style = 0;
        self.tabs = default_tabs(cols);
        self.last_char = None;
        self.current_link = None;
        self.last_print = None;
        self.saved = SavedState {
            cursor: Cursor::default(),
            pen: Style::default(),
            origin: false,
            autowrap: true,
            charsets: [Charset::Ascii, Charset::Ascii],
            active_charset: 0,
        };
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if self.primary.cols == cols && self.primary.rows == rows {
            return;
        }
        resize_buffer(&mut self.primary, cols, rows);
        if let Some(alt) = self.alternate.as_mut() {
            resize_buffer(alt, cols, rows);
        }
        let old_cols = self.tabs.len();
        let mut tabs = vec![false; cols];
        for (i, tab) in self.tabs.iter().enumerate().take(cols) {
            tabs[i] = *tab;
        }
        // Only new columns receive default stops; resizing must not resurrect
        // stops the user cleared with TBC.
        for i in (8..cols).step_by(8) {
            if i >= old_cols {
                tabs[i] = true;
            }
        }
        self.tabs = tabs;
        self.saved.cursor.row = self.saved.cursor.row.min(rows - 1);
        self.saved.cursor.col = self.saved.cursor.col.min(cols - 1);
        if let Some((row, col)) = self.last_print {
            if row >= rows || col >= cols {
                self.last_print = None;
            }
        }
    }

    // ------------------------------------------------------------------
    // SGR
    // ------------------------------------------------------------------

    fn sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        if groups.is_empty() {
            self.pen = Style::default();
            return;
        }
        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let n = group.first().copied().unwrap_or(0);
            match n {
                0 => self.pen = Style::default(),
                1 => self.pen.attrs |= attrs::BOLD,
                2 => self.pen.attrs |= attrs::DIM,
                3 => self.pen.attrs |= attrs::ITALIC,
                4 => {
                    let sub = group.get(1).copied().unwrap_or(1);
                    self.pen.attrs &= !attrs::UNDERLINE_MASK;
                    match sub {
                        0 => {}
                        2 => self.pen.attrs |= attrs::DOUBLE_UNDERLINE,
                        3 => self.pen.attrs |= attrs::CURLY_UNDERLINE,
                        4 => self.pen.attrs |= attrs::DOTTED_UNDERLINE,
                        5 => self.pen.attrs |= attrs::DASHED_UNDERLINE,
                        _ => self.pen.attrs |= attrs::UNDERLINE,
                    }
                }
                5 => self.pen.attrs |= attrs::BLINK,
                7 => self.pen.attrs |= attrs::REVERSE,
                8 => self.pen.attrs |= attrs::HIDDEN,
                9 => self.pen.attrs |= attrs::STRIKE,
                21 => {
                    self.pen.attrs &= !attrs::UNDERLINE_MASK;
                    self.pen.attrs |= attrs::DOUBLE_UNDERLINE;
                }
                22 => self.pen.attrs &= !(attrs::BOLD | attrs::DIM),
                23 => self.pen.attrs &= !attrs::ITALIC,
                24 => self.pen.attrs &= !attrs::UNDERLINE_MASK,
                25 => self.pen.attrs &= !attrs::BLINK,
                27 => self.pen.attrs &= !attrs::REVERSE,
                28 => self.pen.attrs &= !attrs::HIDDEN,
                29 => self.pen.attrs &= !attrs::STRIKE,
                30..=37 => self.pen.fg = Color::Indexed((n - 30) as u8),
                38 => {
                    if let Some((color, used)) = parse_extended_color(&groups, i, group) {
                        self.pen.fg = color;
                        i += used;
                    }
                }
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((n - 40) as u8),
                48 => {
                    if let Some((color, used)) = parse_extended_color(&groups, i, group) {
                        self.pen.bg = color;
                        i += used;
                    }
                }
                49 => self.pen.bg = Color::Default,
                58 => {
                    if let Some((_, used)) = parse_extended_color(&groups, i, group) {
                        i += used;
                    }
                }
                59 => {}
                90..=97 => self.pen.fg = Color::Indexed((n - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((n - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }

    // ------------------------------------------------------------------
    // OSC
    // ------------------------------------------------------------------

    fn handle_osc(&mut self, params: &[&[u8]], _bell: bool) {
        let Some(first) = params.first() else {
            return;
        };
        let code = String::from_utf8_lossy(first);
        match code.as_ref() {
            "0" | "2" => {
                if params.len() > 1 {
                    let raw = params[1..]
                        .iter()
                        .map(|p| String::from_utf8_lossy(p))
                        .collect::<Vec<_>>()
                        .join(";");
                    let title = sanitize_text(&raw, 256);
                    if !title.is_empty() {
                        self.title = title.clone();
                        self.events.push(Event::Title(title));
                    }
                }
            }
            "1" => {}
            "7" => {
                if let Some(uri) = params.get(1) {
                    let uri = String::from_utf8_lossy(uri).into_owned();
                    if let Some(path) = parse_file_uri(&uri) {
                        self.cwd = Some(path.clone());
                        self.events.push(Event::Cwd(path));
                    }
                }
            }
            "8" => {
                if params.len() >= 3 {
                    let uri = params[2..]
                        .iter()
                        .map(|p| String::from_utf8_lossy(p))
                        .collect::<Vec<_>>()
                        .join(";");
                    if uri.is_empty() {
                        self.current_link = None;
                    } else {
                        let cleaned = sanitize_text(&uri, 2048);
                        if cleaned.is_empty() || !safe_hyperlink(&cleaned) {
                            self.current_link = None;
                        } else {
                            self.current_link = Some(Arc::from(cleaned));
                        }
                    }
                }
            }
            "9" => {
                if let Some(msg) = params.get(1) {
                    let msg = sanitize_text(&String::from_utf8_lossy(msg), 512);
                    if !msg.is_empty() {
                        self.events.push(Event::Notify(msg));
                    }
                }
            }
            "52" => {
                if let Some(data) = params.get(2) {
                    if data == b"?" {
                        return;
                    }
                    use base64::Engine;
                    if data.len() <= 4 * 1024 * 1024 {
                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                            if let Ok(text) = String::from_utf8(bytes) {
                                self.events.push(Event::Clipboard(text));
                            }
                        }
                    }
                }
            }
            "104" => {}
            "133" | "633" => {}
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Reports
    // ------------------------------------------------------------------

    fn report_cursor(&mut self, private: bool) {
        let b = self.active();
        let row = b.cursor.row + 1;
        let col = b.cursor.col + 1;
        if private {
            self.respond(format!("\x1b[?{};{}R", row, col).as_bytes());
        } else {
            self.respond(format!("\x1b[{};{}R", row, col).as_bytes());
        }
    }

    fn report_mode(&mut self, mode: u16, private: bool) {
        let status = if private {
            self.mode_status(mode)
        } else {
            match mode {
                4 => Some(self.modes.insert),
                20 => Some(self.modes.newline),
                _ => None,
            }
        };
        let code = match status {
            Some(true) => 1,
            Some(false) => 2,
            None => 0,
        };
        let q = if private { "?" } else { "" };
        self.respond(format!("\x1b[{}{};{}$y", q, mode, code).as_bytes());
    }

    fn device_attributes(&mut self, params: &Params, secondary: bool) {
        if secondary {
            self.respond(b"\x1b[>1;10;0c");
        } else {
            let first = params
                .iter()
                .next()
                .and_then(|g| g.first().copied())
                .unwrap_or(0);
            if first == 0 {
                self.respond(b"\x1b[?62;22c");
            }
        }
    }

    fn window_op(&mut self, params: &Params) {
        let vals: Vec<u16> = params
            .iter()
            .map(|g| g.first().copied().unwrap_or(0))
            .collect();
        match vals.first().copied().unwrap_or(0) {
            14 => {
                let (rows, cols) = (self.rows(), self.cols());
                self.respond(format!("\x1b[4;{};{}t", rows * 16, cols * 8).as_bytes());
            }
            18 => {
                let (rows, cols) = (self.rows(), self.cols());
                self.respond(format!("\x1b[8;{};{}t", rows, cols).as_bytes());
            }
            22 | 23 => {}
            _ => {}
        }
    }

    fn soft_reset(&mut self) {
        self.modes.insert = false;
        self.modes.origin = false;
        self.modes.autowrap = true;
        self.modes.app_cursor = false;
        self.modes.app_keypad = false;
        self.modes.cursor_visible = true;
        self.pen = Style::default();
        self.charsets = [Charset::Ascii, Charset::Ascii];
        self.active_charset = 0;
        self.active_mut().top = 0;
        let bottom = self.active().rows.saturating_sub(1);
        self.active_mut().bottom = bottom;
    }

    fn decaln(&mut self) {
        let (cols, rows) = (self.active().cols, self.active().rows);
        let style = self.pen;
        let b = self.active_mut();
        for row in 0..rows {
            for col in 0..cols {
                *b.get_mut(row, col) = Cell::ch('E', style);
            }
            b.wrapped[row] = false;
        }
        b.top = 0;
        b.bottom = rows - 1;
    }

    /// Human-readable dump used by tests and `--dump` diagnostics.
    pub fn dump(&self) -> String {
        let b = self.active();
        let mut out = String::new();
        for row in 0..b.rows {
            for cell in b.row(row) {
                if cell.cont {
                    continue;
                }
                out.push(cell.ch);
            }
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
        }
        out
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    let mut tabs = vec![false; cols];
    let mut i = 8;
    while i < cols {
        tabs[i] = true;
        i += 8;
    }
    tabs
}

fn resize_buffer(buf: &mut Buffer, cols: usize, rows: usize) {
    let old_cols = buf.cols;
    let old_rows = buf.rows;
    let mut new_cells = vec![Cell::blank(Style::default()); cols * rows];
    for row in 0..rows.min(old_rows) {
        let src_start = row * old_cols;
        let width = cols.min(old_cols);
        let dst_start = row * cols;
        new_cells[dst_start..dst_start + width]
            .clone_from_slice(&buf.cells[src_start..src_start + width]);
    }
    buf.cells = new_cells;
    buf.cols = cols;
    buf.rows = rows;
    let mut wrapped = vec![false; rows];
    for (i, w) in buf.wrapped.iter().enumerate().take(rows) {
        wrapped[i] = *w;
    }
    buf.wrapped = wrapped;
    for row in 0..rows {
        let start = row * cols;
        let end = start + cols;
        if buf.cells[start].cont {
            let style = buf.cells[start].style;
            buf.cells[start].reset(style);
        }
        if buf.cells[end - 1].wide {
            let style = buf.cells[end - 1].style;
            buf.cells[end - 1].reset(style);
        }
    }
    for row in buf.scrollback.iter_mut() {
        if row.cells.len() > cols {
            row.cells.truncate(cols);
            if row.cells.last().map(|c| c.wide).unwrap_or(false) {
                if let Some(last) = row.cells.last_mut() {
                    let style = last.style;
                    last.reset(style);
                }
            }
        } else if row.cells.len() < cols {
            row.cells.extend(std::iter::repeat_n(
                Cell::blank(Style::default()),
                cols - row.cells.len(),
            ));
        }
    }
    buf.scrollback_bytes = buf
        .scrollback
        .iter()
        .map(|row| row.cells.len().saturating_mul(CELL_BYTES))
        .sum();
    while buf.scrollback_bytes > SCROLLBACK_BYTE_LIMIT {
        if let Some(front) = buf.scrollback.pop_front() {
            buf.scrollback_bytes = buf
                .scrollback_bytes
                .saturating_sub(front.cells.len().saturating_mul(CELL_BYTES));
        } else {
            buf.scrollback_bytes = 0;
            break;
        }
    }
    if buf.top >= rows {
        buf.top = 0;
    }
    if buf.bottom >= rows {
        buf.bottom = rows - 1;
    }
    if buf.top >= buf.bottom {
        buf.top = 0;
        buf.bottom = rows - 1;
    }
    buf.cursor.row = buf.cursor.row.min(rows - 1);
    buf.cursor.col = buf.cursor.col.min(cols - 1);
    buf.cursor.pending_wrap = false;
}

fn parse_extended_color(groups: &[&[u16]], i: usize, group: &[u16]) -> Option<(Color, usize)> {
    // Colon form: 38:5:n or 38:2:r:g:b (subparams in one group).
    if group.len() > 1 {
        return match group.get(1).copied()? {
            5 => Some((Color::Indexed(group.get(2).copied()? as u8), 0)),
            2 => {
                let r = group.get(2).copied().unwrap_or(0) as u8;
                let g = group.get(3).copied().unwrap_or(0) as u8;
                let b = group.get(4).copied().unwrap_or(0) as u8;
                Some((Color::Rgb(r, g, b), 0))
            }
            _ => None,
        };
    }
    // Semicolon form across several groups.
    let kind = groups.get(i + 1)?.first().copied()?;
    match kind {
        5 => Some((
            Color::Indexed(groups.get(i + 2)?.first().copied()? as u8),
            2,
        )),
        2 => {
            let r = groups.get(i + 2)?.first().copied()? as u8;
            let g = groups.get(i + 3)?.first().copied()? as u8;
            let b = groups.get(i + 4)?.first().copied()? as u8;
            Some((Color::Rgb(r, g, b), 4))
        }
        _ => None,
    }
}

fn safe_hyperlink(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    ["http://", "https://", "mailto:", "file://", "ftp://"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
        || !lower.contains(':')
}

fn sanitize_text(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

fn parse_file_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(idx) => &rest[idx..],
        None => return None,
    };
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            if let Ok(v) = u8::from_str_radix(&path[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn dec_graphics(c: char) -> Option<char> {
    Some(match c {
        '`' => '◆',
        'a' => '▒',
        'b' => '␉',
        'c' => '␌',
        'd' => '␍',
        'e' => '␊',
        'f' => '°',
        'g' => '±',
        'h' => '␤',
        'i' => '␋',
        'j' => '┘',
        'k' => '┐',
        'l' => '┌',
        'm' => '└',
        'n' => '┼',
        'o' => '⎺',
        'p' => '⎻',
        'q' => '─',
        'r' => '⎼',
        's' => '⎽',
        't' => '├',
        'u' => '┤',
        'v' => '┴',
        'w' => '┬',
        'x' => '│',
        'y' => '≤',
        'z' => '≥',
        '{' => 'π',
        '|' => '≠',
        '}' => '£',
        '~' => '·',
        _ => return None,
    })
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.events.push(Event::Bell),
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0a..=0x0c => self.linefeed(),
            0x0d => self.carriage_return(),
            0x0e => self.active_charset = 1,
            0x0f => self.active_charset = 0,
            0x18 | 0x1a => {}
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let p = |idx: usize, default: u16| -> u16 {
            params
                .iter()
                .nth(idx)
                .and_then(|g| g.first().copied())
                .map(|v| if v == 0 { default } else { v })
                .unwrap_or(default)
        };
        let raw =
            |idx: usize| -> Option<u16> { params.iter().nth(idx).and_then(|g| g.first().copied()) };
        let private = intermediates.contains(&b'?');
        let dollar = intermediates.contains(&b'$');
        let space = intermediates.contains(&b' ');

        if dollar && action == 'p' {
            if let Some(mode) = raw(0) {
                self.report_mode(mode, private);
            }
            return;
        }

        match action {
            'A' => self.move_up(p(0, 1) as usize),
            'B' | 'e' => self.move_down(p(0, 1) as usize),
            'C' | 'a' => self.move_right(p(0, 1) as usize),
            'D' => self.move_left(p(0, 1) as usize),
            'E' => {
                self.move_down(p(0, 1) as usize);
                self.carriage_return();
            }
            'F' => {
                self.move_up(p(0, 1) as usize);
                self.carriage_return();
            }
            'G' | '`' => {
                let col = p(0, 1) as usize - 1;
                let col = col.min(self.active().cols.saturating_sub(1));
                let b = self.active_mut();
                b.cursor.col = col;
                b.cursor.pending_wrap = false;
            }
            'd' => {
                let row = p(0, 1) as usize - 1;
                let col = self.active().cursor.col;
                self.goto(row, col);
            }
            'H' | 'f' => {
                let row = p(0, 1) as usize - 1;
                let col = p(1, 1) as usize - 1;
                self.goto(row, col);
            }
            'I' => self.tab(),
            'Z' => self.back_tab(p(0, 1) as usize),
            'J' => self.erase_in_display(p(0, 0)),
            'K' => self.erase_in_line(p(0, 0)),
            'L' => self.insert_lines(p(0, 1) as usize),
            'M' => self.delete_lines(p(0, 1) as usize),
            'P' => self.delete_chars(p(0, 1) as usize),
            '@' => self.insert_chars(p(0, 1) as usize),
            'X' => self.erase_chars(p(0, 1) as usize),
            'S' => self.scroll_up(p(0, 1) as usize),
            'T' => self.scroll_down(p(0, 1) as usize),
            'b' => self.repeat_last(p(0, 1) as usize),
            'g' => match p(0, 0) {
                0 => {
                    let col = self.active().cursor.col;
                    if let Some(t) = self.tabs.get_mut(col) {
                        *t = false;
                    }
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            'h' => {
                for mode in param_values(params) {
                    if private {
                        self.set_private_mode(mode, true);
                    } else {
                        self.set_ansi_mode(mode, true);
                    }
                }
            }
            'l' => {
                for mode in param_values(params) {
                    if private {
                        self.set_private_mode(mode, false);
                    } else {
                        self.set_ansi_mode(mode, false);
                    }
                }
            }
            'm' => self.sgr(params),
            'n' => match p(0, 0) {
                5 => self.respond(b"\x1b[0n"),
                6 => self.report_cursor(private),
                _ => {}
            },
            'c' => self.device_attributes(params, intermediates.contains(&b'>')),
            'r' => {
                let rows = self.active().rows;
                let top = p(0, 1) as usize - 1;
                let bottom = p(1, rows as u16) as usize - 1;
                if top < bottom && bottom < rows {
                    let b = self.active_mut();
                    b.top = top;
                    b.bottom = bottom;
                    b.cursor.pending_wrap = false;
                    self.goto(0, 0);
                }
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            't' => self.window_op(params),
            'q' if space => {
                self.cursor_style = p(0, 0).min(6) as u8;
            }
            'p' if intermediates.contains(&b'!') => self.soft_reset(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if intermediates.contains(&b'#') {
            if byte == b'8' {
                self.decaln();
            }
            return;
        }
        match byte {
            b'7' => self.save_cursor(),
            b'8' => self.restore_cursor(),
            b'D' => self.index(),
            b'E' => {
                self.index();
                self.carriage_return();
            }
            b'M' => self.reverse_index(),
            b'H' => {
                let col = self.active().cursor.col;
                if let Some(t) = self.tabs.get_mut(col) {
                    *t = true;
                }
            }
            b'c' => self.full_reset(),
            b'=' => self.modes.app_keypad = true,
            b'>' => self.modes.app_keypad = false,
            b'0' | b'B' | b'A' | b'U' => {
                let target = match intermediates.first() {
                    Some(b'(') => Some(0),
                    Some(b')') => Some(1),
                    Some(b'*') | Some(b'+') => Some(0), // G2/G3 are not used
                    _ => None,
                };
                if let Some(index) = target {
                    self.charsets[index] = if byte == b'0' {
                        Charset::DecGraphics
                    } else {
                        Charset::Ascii
                    };
                }
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.handle_osc(params, bell_terminated);
    }
}

fn param_values(params: &Params) -> Vec<u16> {
    params
        .iter()
        .map(|g| g.first().copied().unwrap_or(0))
        .collect()
}

/// Convenience wrapper pairing the VT parser with its screen.
pub struct Terminal {
    pub screen: Screen,
    parser: vte::Parser,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        Self {
            screen: Screen::new(cols, rows, scrollback),
            parser: vte::Parser::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    pub fn take_events(&mut self) -> Vec<Event> {
        self.screen.take_events()
    }

    pub fn take_responses(&mut self) -> Vec<u8> {
        self.screen.take_responses()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: usize, rows: usize) -> Terminal {
        Terminal::new(cols, rows, 100)
    }

    fn feed(t: &mut Terminal, s: &str) {
        t.feed(s.as_bytes());
    }

    #[test]
    fn plain_text_and_wrap() {
        let mut t = term(5, 3);
        feed(&mut t, "abcdefg");
        assert_eq!(t.screen.primary.row(0)[0].ch, 'a');
        assert_eq!(t.screen.primary.row(0)[4].ch, 'e');
        assert_eq!(t.screen.primary.row(1)[0].ch, 'f');
        assert_eq!(t.screen.primary.row(1)[1].ch, 'g');
        assert!(t.screen.primary.wrapped[0]);
        assert_eq!(t.screen.active().cursor.row, 1);
        assert_eq!(t.screen.active().cursor.col, 2);
    }

    #[test]
    fn pending_wrap_moves_on_next_char() {
        let mut t = term(3, 2);
        feed(&mut t, "abc");
        assert_eq!(t.screen.active().cursor.col, 2);
        assert!(t.screen.active().cursor.pending_wrap);
        feed(&mut t, "d");
        assert_eq!(t.screen.primary.row(1)[0].ch, 'd');
        assert_eq!(t.screen.active().cursor.row, 1);
    }

    #[test]
    fn wide_chars_occupy_two_cells() {
        let mut t = term(4, 2);
        feed(&mut t, "中a");
        assert!(t.screen.primary.row(0)[0].wide);
        assert!(t.screen.primary.row(0)[1].cont);
        assert_eq!(t.screen.primary.row(0)[2].ch, 'a');
        assert_eq!(t.screen.active().cursor.col, 3);
    }

    #[test]
    fn wide_char_wraps_at_edge() {
        let mut t = term(3, 2);
        feed(&mut t, "ab中");
        // 'a','b' in row 0; wide glyph wraps to row 1.
        assert_eq!(t.screen.primary.row(0)[0].ch, 'a');
        assert_eq!(t.screen.primary.row(0)[1].ch, 'b');
        assert!(t.screen.primary.row(1)[0].wide);
        assert!(t.screen.primary.row(1)[1].cont);
    }

    #[test]
    fn combining_attaches_to_previous() {
        let mut t = term(5, 2);
        feed(&mut t, "e\u{0301}");
        let cell = &t.screen.primary.row(0)[0];
        assert_eq!(cell.ch, 'e');
        assert_eq!(cell.extra.as_deref(), Some("\u{0301}"));
    }

    #[test]
    fn scrolling_pushes_scrollback() {
        let mut t = term(5, 2);
        feed(&mut t, "a\r\nb\r\nc");
        assert_eq!(t.screen.primary.scrollback.len(), 1);
        assert_eq!(t.screen.primary.scrollback[0].cells[0].ch, 'a');
        assert_eq!(t.screen.primary.row(1)[0].ch, 'c');
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = term(5, 2);
        feed(&mut t, "hello");
        feed(&mut t, "\x1b[?1049h");
        assert!(t.screen.alt_active);
        feed(&mut t, "ALT");
        assert_eq!(t.screen.dump().lines().next().unwrap(), "ALT");
        feed(&mut t, "\x1b[?1049l");
        assert!(!t.screen.alt_active);
        assert_eq!(t.screen.dump().lines().next().unwrap(), "hello");
    }

    #[test]
    fn erase_and_cursor() {
        let mut t = term(5, 2);
        feed(&mut t, "hello\x1b[2;1H\x1b[K");
        assert_eq!(t.screen.dump().lines().next().unwrap(), "hello");
        feed(&mut t, "\x1b[1;2H\x1b[1K");
        let row0: String = t.screen.primary.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(&row0[..4], "  ll");
    }

    #[test]
    fn sgr_colors() {
        let mut t = term(5, 2);
        feed(&mut t, "\x1b[1;38;5;196;48;2;10;20;30mA");
        let cell = &t.screen.primary.row(0)[0];
        assert!(cell.style.attrs & attrs::BOLD != 0);
        assert_eq!(cell.style.fg, Color::Indexed(196));
        assert_eq!(cell.style.bg, Color::Rgb(10, 20, 30));
        feed(&mut t, "\x1b[0m");
        assert_eq!(t.screen.pen, Style::default());
    }

    #[test]
    fn scroll_region_limited() {
        let mut t = term(3, 4);
        feed(&mut t, "1\r\n2\r\n3\r\n4");
        feed(&mut t, "\x1b[2;3r\x1b[3;1H\nX");
        // Region rows 1..2 scrolled; row 0 untouched.
        assert_eq!(t.screen.primary.row(0)[0].ch, '1');
        assert_eq!(t.screen.primary.row(1)[0].ch, '3');
        assert_eq!(t.screen.primary.row(2)[0].ch, 'X');
        assert_eq!(t.screen.primary.row(3)[0].ch, '4');
    }

    #[test]
    fn insert_delete_chars() {
        let mut t = term(8, 1);
        feed(&mut t, "abcdef\x1b[1;2H\x1b[2@");
        let row: String = t.screen.primary.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(row, "a  bcdef");
        feed(&mut t, "\x1b[2P");
        let row: String = t.screen.primary.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(row, "abcdef  ");
    }

    #[test]
    fn device_reports() {
        let mut t = term(10, 5);
        feed(&mut t, "\x1b[3;4H\x1b[6n");
        assert_eq!(t.take_responses(), b"\x1b[3;4R");
        feed(&mut t, "\x1b[c");
        assert_eq!(t.take_responses(), b"\x1b[?62;22c");
    }

    #[test]
    fn osc_title_and_clipboard() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b]2;my title\x07");
        let events = t.take_events();
        assert_eq!(events[0], Event::Title("my title".into()));
        feed(&mut t, "\x1b]52;c;aGVsbG8=\x07");
        let events = t.take_events();
        assert_eq!(events[0], Event::Clipboard("hello".into()));
    }

    #[test]
    fn alternate_screen_does_not_add_scrollback() {
        let mut t = term(5, 2);
        feed(&mut t, "a\r\nb");
        let before = t.screen.primary.scrollback.len();
        feed(&mut t, "\x1b[?1049h");
        feed(&mut t, "1\r\n2\r\n3\r\n4");
        assert_eq!(t.screen.primary.scrollback.len(), before);
        assert!(t.screen.alternate.as_ref().unwrap().scrollback.is_empty());
    }

    #[test]
    fn overwriting_half_of_a_wide_glyph_repairs_the_row() {
        let mut t = term(4, 1);
        feed(&mut t, "中\x1b[1;2Hx");
        assert_eq!(t.screen.primary.row(0)[0].ch, ' ');
        assert!(!t.screen.primary.row(0)[0].wide);
        assert_eq!(t.screen.primary.row(0)[1].ch, 'x');
        assert!(!t.screen.primary.row(0)[1].cont);
    }

    #[test]
    fn c1_controls_never_become_printable_cells() {
        let mut t = term(10, 2);
        // UTF-8 encoding of U+009B (C1 CSI), U+009D (OSC).
        feed(&mut t, "a\u{009b}31mX\u{009d}evil\u{009c}b");
        for row in 0..2 {
            for cell in t.screen.primary.row(row) {
                assert!(!cell.ch.is_control(), "control char in cell: {:?}", cell.ch);
                if let Some(extra) = &cell.extra {
                    assert!(!extra.chars().any(|c| c.is_control()));
                }
            }
        }
        assert!(t.screen.dump().contains('X'));
    }

    #[test]
    fn alt_screen_resize_then_exit_does_not_leave_invalid_cursor() {
        let mut t = term(10, 5);
        feed(&mut t, "\x1b[5;10H\x1b[?1049h");
        t.resize(3, 2);
        feed(&mut t, "\x1b[?1049l");
        let c = t.screen.active().cursor;
        assert!(c.row < 2 && c.col < 3);
        feed(&mut t, "x");
        assert!(t.screen.active().cursor.row < 2);
    }

    #[test]
    fn alt_screen_restores_modes_and_margins() {
        let mut t = term(10, 5);
        feed(
            &mut t,
            "\x1b[2;4r\x1b[?1049h\x1b[?7l\x1b[?6h\x1b(0\x1b[?1049l",
        );
        assert!(t.screen.modes.autowrap);
        assert!(!t.screen.modes.origin);
        assert_eq!(t.screen.charsets[0], Charset::Ascii);
        assert_eq!(t.screen.primary.top, 1);
        assert_eq!(t.screen.primary.bottom, 3);
    }

    #[test]
    fn cuu_cud_respect_cursor_outside_margins() {
        let mut t = term(10, 5);
        feed(&mut t, "\x1b[2;4r\x1b[A");
        assert_eq!(t.screen.active().cursor.row, 0);
        feed(&mut t, "\x1b[5;1H\x1b[B");
        assert_eq!(t.screen.active().cursor.row, 4);
    }

    #[test]
    fn autowrap_off_at_edge_repairs_old_wide_pair() {
        let mut t = term(3, 1);
        feed(&mut t, "a中\x1b[?7l中");
        assert!(!t.screen.primary.row(0)[1].wide);
        assert!(!t.screen.primary.row(0)[2].cont);
    }

    #[test]
    fn one_column_screen_never_creates_a_wide_lead() {
        let mut t = term(1, 2);
        feed(&mut t, "中");
        assert!(!t.screen.primary.row(0)[0].wide);
        assert!(!t.screen.primary.row(1)[0].wide);
        assert!(!t.screen.primary.row(0)[0].cont);
        assert!(!t.screen.primary.row(1)[0].cont);
    }

    #[test]
    fn osc7_uses_the_standard_parameter_and_decodes_safely() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b]7;file:///tmp/a%E4%B8%AD\x07");
        assert_eq!(t.take_events(), vec![Event::Cwd("/tmp/a中".into())]);
        // A percent followed by a multibyte character must not slice a UTF-8
        // boundary or panic.
        feed(&mut t, "\x1b]7;file:///a%😀\x07");
    }

    #[test]
    fn resize_does_not_restore_cleared_tab_stops() {
        let mut t = term(20, 2);
        feed(&mut t, "\x1b[1;9H\x1b[0g\x1b[1;1H");
        t.resize(21, 2);
        feed(&mut t, "\t");
        assert_eq!(t.screen.active().cursor.col, 16);
    }

    #[test]
    fn combining_after_newline_does_not_decorate_previous_row() {
        let mut t = term(5, 2);
        feed(&mut t, "AB\r\n");
        feed(&mut t, "\u{0301}");
        for cell in t.screen.primary.row(0) {
            assert!(cell.extra.is_none());
        }
    }

    #[test]
    fn resize_keeps_content_and_cursor_valid() {
        let mut t = term(10, 4);
        feed(&mut t, "hello\r\nworld");
        t.resize(5, 2);
        assert_eq!(t.screen.primary.row(0)[0].ch, 'h');
        assert!(t.screen.active().cursor.col < 5);
        assert!(t.screen.active().cursor.row < 2);
    }

    #[test]
    fn bracketed_paste_mode() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b[?2004h");
        assert!(t.screen.modes.bracketed_paste);
        feed(&mut t, "\x1b[?2004l");
        assert!(!t.screen.modes.bracketed_paste);
    }

    #[test]
    fn zero_size_is_clamped() {
        let t = term(0, 0);
        assert_eq!(t.screen.cols(), 1);
        assert_eq!(t.screen.rows(), 1);
    }

    #[test]
    fn delete_lines_whole_region_does_not_panic() {
        let mut t = term(4, 3);
        feed(&mut t, "a\r\nb\r\nc\x1b[1;1H\x1b[9M");
        assert_eq!(t.screen.primary.row(0)[0].ch, ' ');
    }

    #[test]
    fn origin_mode_positions_relative_to_margin() {
        let mut t = term(10, 5);
        feed(&mut t, "\x1b[2;4r\x1b[?6h\x1b[2;3H");
        assert_eq!(t.screen.active().cursor.row, 2);
        assert_eq!(t.screen.active().cursor.col, 2);
        feed(&mut t, "\x1b[?6l\x1b[1;1H");
        assert_eq!(t.screen.active().cursor.row, 0);
        assert_eq!(t.screen.active().cursor.col, 0);
    }

    #[test]
    fn alt_screen_restores_cursor() {
        let mut t = term(10, 5);
        feed(&mut t, "\x1b[2;3H\x1b[?1049h\x1b[1;1H");
        assert_eq!(t.screen.active().cursor.row, 0);
        feed(&mut t, "\x1b[?1049l");
        assert_eq!(t.screen.active().cursor.row, 1);
        assert_eq!(t.screen.active().cursor.col, 2);
    }

    #[test]
    fn wide_glyph_with_autowrap_off_does_not_break_grid() {
        let mut t = term(2, 1);
        feed(&mut t, "\x1b[?7la中");
        assert!(!t.screen.primary.row(0)[1].cont);
        assert!(!t.screen.primary.row(0)[0].wide);
        assert_eq!(t.screen.primary.row(0)[0].ch, 'a');
    }

    #[test]
    fn title_can_contain_semicolons() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b]2;hello; world\x07");
        let events = t.take_events();
        assert_eq!(events[0], Event::Title("hello; world".into()));
    }

    #[test]
    fn dangerous_hyperlink_schemes_are_dropped() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b]8;;javascript:alert(1)\x1b\\x");
        assert!(t.screen.primary.row(0)[0].link.is_none());
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\y");
        assert_eq!(
            t.screen.primary.row(0)[1].link.as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn osc8_hyperlinks_are_captured() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b]8;;https://example.com\x1b\\x");
        let cell = &t.screen.primary.row(0)[0];
        assert_eq!(cell.ch, 'x');
        assert_eq!(cell.link.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn dec_graphics_charset() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b(0lqk\x1b(B");
        let row: String = t
            .screen
            .primary
            .row(0)
            .iter()
            .take(3)
            .map(|c| c.ch)
            .collect();
        assert_eq!(row, "┌─┐");
    }

    #[test]
    fn custom_tab_stops() {
        let mut t = term(20, 1);
        feed(&mut t, "\x1b[1;4H\x1bH\x1b[1;1H\tX");
        assert_eq!(t.screen.primary.row(0)[3].ch, 'X');
    }

    #[test]
    fn delete_wide_char_pair() {
        let mut t = term(6, 1);
        feed(&mut t, "a中b\x1b[1;2H\x1b[2P");
        let row: String = t.screen.primary.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(&row[..3], "ab ");
        assert!(!t.screen.primary.row(0)[1].cont);
    }

    #[test]
    fn scrollback_is_capped() {
        let mut t = Terminal::new(3, 1, 2);
        feed(&mut t, "1\r\n2\r\n3\r\n4");
        assert_eq!(t.screen.primary.scrollback.len(), 2);
        assert_eq!(t.screen.primary.scrollback[0].cells[0].ch, '2');
        assert_eq!(t.screen.primary.scrollback[1].cells[0].ch, '3');
    }

    #[test]
    fn erase_display_three_clears_history() {
        let mut t = term(3, 1);
        feed(&mut t, "1\r\n2\x1b[3J");
        assert!(t.screen.primary.scrollback.is_empty());
    }

    #[test]
    fn repeat_last_character() {
        let mut t = term(10, 1);
        feed(&mut t, "x\x1b[3b");
        let row: String = t.screen.primary.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(&row[..4], "xxxx");
    }

    #[test]
    fn mode_nine_enables_x10_mouse() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b[?9h");
        assert_eq!(t.screen.modes.mouse, MouseMode::X10);
        feed(&mut t, "\x1b[?9l");
        assert_eq!(t.screen.modes.mouse, MouseMode::Off);
    }

    #[test]
    fn cursor_style_is_tracked() {
        let mut t = term(10, 2);
        feed(&mut t, "\x1b[5 q");
        assert_eq!(t.screen.cursor_style, 5);
    }

    #[test]
    fn decaln_fills_screen() {
        let mut t = term(4, 2);
        feed(&mut t, "\x1b#8");
        assert_eq!(t.screen.dump().lines().next().unwrap(), "EEEE");
    }

    #[test]
    fn bell_event() {
        let mut t = term(4, 2);
        feed(&mut t, "\x07");
        assert_eq!(t.take_events(), vec![Event::Bell]);
    }

    #[test]
    fn save_restore_cursor() {
        let mut t = term(10, 3);
        feed(&mut t, "\x1b[2;3H\x1b7\x1b[1;1H\x1b8");
        assert_eq!(t.screen.active().cursor.row, 1);
        assert_eq!(t.screen.active().cursor.col, 2);
    }
}
