//! Copy mode: a frozen snapshot of a pane's scrollback and screen, vi-style
//! navigation, selection and search.

use crate::layout::PaneId;
use crate::server::input::Key;
use crate::term::{Cursor, Row};

const MAX_SEARCH_MATCHES: usize = 10_000;

#[derive(Clone)]
pub struct CopySnapshot {
    pub cols: usize,
    pub screen_rows: usize,
    pub rows: Vec<Row>,
    pub cursor: Cursor,
    pub title: String,
    pub reverse: bool,
}

#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub row: usize,
    /// Inclusive start cell column.
    pub start: usize,
    /// Exclusive end cell column.
    pub end: usize,
}

pub struct CopyState {
    pub window_id: u64,
    pub pane_id: PaneId,
    pub snapshot: CopySnapshot,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub desired_col: usize,
    /// Scroll offset from the bottom of the snapshot.
    pub scroll: usize,
    pub anchor: Option<(usize, usize)>,
    pub selecting: bool,
    pub query: Option<String>,
    pub matches: Vec<SearchMatch>,
    pub match_idx: Option<usize>,
    pub search_forward: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Continue,
    Exit,
    Copy(String),
    SearchPrompt { forward: bool },
    Toast(String),
}

impl CopyState {
    pub fn new(window_id: u64, pane_id: PaneId, snapshot: CopySnapshot) -> Self {
        let live = snapshot.cursor;
        let live_row = snapshot
            .rows
            .len()
            .saturating_sub(snapshot.screen_rows)
            .saturating_add(live.row);
        let row = live_row.min(snapshot.rows.len().saturating_sub(1));
        let col = Self::snap_col(&snapshot, row, live.col);
        Self {
            window_id,
            pane_id,
            snapshot,
            cursor_row: row,
            cursor_col: col,
            desired_col: col,
            scroll: 0,
            anchor: None,
            selecting: false,
            query: None,
            matches: Vec::new(),
            match_idx: None,
            search_forward: true,
        }
    }

    pub fn total_rows(&self) -> usize {
        self.snapshot.rows.len()
    }

    pub fn cols(&self) -> usize {
        self.snapshot.cols
    }

    /// Returns (top inclusive, bottom exclusive) for a viewport of `height`.
    pub fn viewport(&self, height: u16) -> (usize, usize) {
        let h = (height as usize).min(self.total_rows()).max(1);
        let bottom = self.total_rows().saturating_sub(self.scroll);
        let top = bottom.saturating_sub(h);
        (top, bottom)
    }

    fn max_scroll(&self, height: u16) -> usize {
        self.total_rows()
            .saturating_sub((height as usize).min(self.total_rows()).max(1))
    }

    pub fn scroll_view(&mut self, delta: isize, height: u16) {
        let max = self.max_scroll(height);
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
        let (top, bottom) = self.viewport(height);
        if self.cursor_row < top {
            self.cursor_row = top;
        } else if self.cursor_row >= bottom {
            self.cursor_row = bottom.saturating_sub(1);
        }
        self.cursor_col = Self::snap_col(&self.snapshot, self.cursor_row, self.cursor_col);
        self.desired_col = self.cursor_col;
    }

    pub fn ensure_visible(&mut self, height: u16) {
        let (top, bottom) = self.viewport(height);
        if self.cursor_row < top {
            self.scroll = self
                .total_rows()
                .saturating_sub(height as usize)
                .saturating_sub(self.cursor_row);
        } else if self.cursor_row >= bottom {
            self.scroll = self.total_rows().saturating_sub(self.cursor_row + 1);
        }
        let max = self.max_scroll(height);
        self.scroll = self.scroll.min(max);
    }

    fn row_len(&self, row: usize) -> usize {
        self.snapshot
            .rows
            .get(row)
            .map(|r| r.cells.len())
            .unwrap_or(0)
    }

    fn cell(&self, row: usize, col: usize) -> Option<&crate::term::Cell> {
        self.snapshot.rows.get(row)?.cells.get(col)
    }

    fn snap_col(snapshot: &CopySnapshot, row: usize, col: usize) -> usize {
        let Some(cells) = snapshot.rows.get(row).map(|r| &r.cells) else {
            return 0;
        };
        let mut c = col.min(cells.len().saturating_sub(1));
        if cells.get(c).map(|cell| cell.cont).unwrap_or(false) && c > 0 {
            c -= 1;
        }
        c
    }

    fn move_horizontal(&mut self, delta: isize) {
        let len = self.row_len(self.cursor_row);
        if len == 0 {
            return;
        }
        if delta < 0 {
            if self.cursor_col == 0 {
                self.cursor_col = len.saturating_sub(1);
            } else {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            }
        } else {
            let cell = self.cell(self.cursor_row, self.cursor_col);
            let step = cell.map(|c| c.display_width()).unwrap_or(1).max(1);
            self.cursor_col = (self.cursor_col + step).min(len.saturating_sub(1));
        }
        self.cursor_col = Self::snap_col(&self.snapshot, self.cursor_row, self.cursor_col);
        self.desired_col = self.cursor_col;
    }

    fn last_nonblank(&self, row: usize) -> usize {
        let Some(line) = self.snapshot.rows.get(row) else {
            return 0;
        };
        let mut last = 0;
        for (i, cell) in line.cells.iter().enumerate() {
            if cell.cont {
                continue;
            }
            if cell.ch != ' ' || cell.extra.is_some() {
                last = i;
            }
        }
        last
    }

    pub fn handle_key(&mut self, key: Key, height: u16) -> CopyOutcome {
        match key {
            Key::Esc | Key::Char('q') => return CopyOutcome::Exit,
            Key::Up | Key::Char('k') => {
                self.cursor_row = self.cursor_row.saturating_sub(1);
                self.cursor_col = Self::snap_col(
                    &self.snapshot,
                    self.cursor_row,
                    self.desired_col
                        .min(self.row_len(self.cursor_row).saturating_sub(1)),
                );
                self.ensure_visible(height);
            }
            Key::Down | Key::Char('j') => {
                self.cursor_row = (self.cursor_row + 1).min(self.total_rows().saturating_sub(1));
                self.cursor_col = Self::snap_col(
                    &self.snapshot,
                    self.cursor_row,
                    self.desired_col
                        .min(self.row_len(self.cursor_row).saturating_sub(1)),
                );
                self.ensure_visible(height);
            }
            Key::Left | Key::Char('h') => self.move_horizontal(-1),
            Key::Right | Key::Char('l') => self.move_horizontal(1),
            Key::Home | Key::Char('0') => {
                self.cursor_col = 0;
                self.desired_col = 0;
            }
            Key::End | Key::Char('$') => {
                self.cursor_col = self.last_nonblank(self.cursor_row);
                self.desired_col = self.cursor_col;
            }
            Key::Char('g') => {
                self.cursor_row = 0;
                self.cursor_col = 0;
                self.desired_col = 0;
                self.ensure_visible(height);
            }
            Key::Char('G') => {
                self.cursor_row = self.total_rows().saturating_sub(1);
                self.cursor_col = 0;
                self.desired_col = 0;
                self.ensure_visible(height);
            }
            Key::PageUp | Key::Char('\u{15}') | Key::Char('\u{2}') => {
                let page = (height as isize).max(2);
                self.cursor_row = self.cursor_row.saturating_sub(page as usize);
                self.ensure_visible(height);
            }
            Key::PageDown | Key::Char('\u{4}') | Key::Char('\u{6}') => {
                let page = (height as isize).max(2);
                self.cursor_row =
                    (self.cursor_row + page as usize).min(self.total_rows().saturating_sub(1));
                self.ensure_visible(height);
            }
            Key::Char(' ') | Key::Char('v') => {
                if self.selecting {
                    self.selecting = false;
                    self.anchor = None;
                    return CopyOutcome::Toast("Selection cleared".into());
                }
                self.anchor = Some((self.cursor_row, self.cursor_col));
                self.selecting = true;
                return CopyOutcome::Toast("Selection started".into());
            }
            Key::Enter | Key::Char('y') => {
                let text = self.selection_text();
                if text.is_empty() {
                    return CopyOutcome::Toast("Nothing to copy".into());
                }
                return CopyOutcome::Copy(text);
            }
            Key::Char('/') => return CopyOutcome::SearchPrompt { forward: true },
            Key::Char('?') => return CopyOutcome::SearchPrompt { forward: false },
            Key::Char('n') => {
                if !self.matches.is_empty() {
                    return self.jump_match(true, height);
                }
            }
            Key::Char('N') => {
                if !self.matches.is_empty() {
                    return self.jump_match(false, height);
                }
            }
            Key::Char('w') => self.word_forward(),
            Key::Char('b') => self.word_backward(),
            _ => {}
        }
        CopyOutcome::Continue
    }

    fn word_forward(&mut self) {
        let text = self.row_text(self.cursor_row);
        let cols = self.row_col_map(self.cursor_row);
        let chars: Vec<char> = text.chars().collect();
        let mut idx = cols
            .iter()
            .position(|c| *c >= self.cursor_col)
            .unwrap_or(chars.len());
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        while idx < chars.len() && is_word(chars[idx]) {
            idx += 1;
        }
        while idx < chars.len() && !is_word(chars[idx]) && chars[idx] != ' ' {
            idx += 1;
        }
        while idx < chars.len() && chars[idx] == ' ' {
            idx += 1;
        }
        if idx < chars.len() {
            self.cursor_col = cols.get(idx).copied().unwrap_or(0);
            self.desired_col = self.cursor_col;
        }
    }

    fn word_backward(&mut self) {
        let text = self.row_text(self.cursor_row);
        let cols = self.row_col_map(self.cursor_row);
        let chars: Vec<char> = text.chars().collect();
        let mut idx = cols.iter().position(|c| *c >= self.cursor_col).unwrap_or(0);
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        idx = idx.saturating_sub(1);
        while idx > 0 && chars.get(idx).map(|c| !is_word(*c)).unwrap_or(true) {
            idx -= 1;
        }
        while idx > 0 && chars.get(idx - 1).map(|c| is_word(*c)).unwrap_or(false) {
            idx -= 1;
        }
        self.cursor_col = cols.get(idx).copied().unwrap_or(0);
        self.desired_col = self.cursor_col;
    }

    pub fn row_text(&self, row: usize) -> String {
        let Some(line) = self.snapshot.rows.get(row) else {
            return String::new();
        };
        let mut s = String::new();
        for cell in &line.cells {
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

    fn row_col_map(&self, row: usize) -> Vec<usize> {
        let Some(line) = self.snapshot.rows.get(row) else {
            return Vec::new();
        };
        let mut map = Vec::new();
        for (col, cell) in line.cells.iter().enumerate() {
            if cell.cont {
                continue;
            }
            map.push(col);
            let text = cell.text();
            let mut chars = text.chars();
            chars.next();
            for _ in chars {
                map.push(col);
            }
        }
        map
    }

    pub fn scroll_percent(&self) -> u16 {
        let total = self.total_rows();
        if total <= 1 {
            return 100;
        }
        let bottom = total.saturating_sub(self.scroll);
        ((bottom.saturating_sub(1) * 100) / (total - 1)) as u16
    }

    pub fn set_search(&mut self, query: &str, forward: bool, height: u16) {
        let q = query.to_lowercase();
        self.query = if q.is_empty() { None } else { Some(q.clone()) };
        self.matches.clear();
        self.match_idx = None;
        self.search_forward = forward;
        if q.is_empty() {
            return;
        }
        let qchars: Vec<char> = q.chars().collect();
        if qchars.is_empty() {
            return;
        }
        'rows: for (row, line) in self.snapshot.rows.iter().enumerate() {
            // Lowercasing can expand one source character into several
            // lowercase chars (İ -> i + combining dot). Keep a parallel map
            // back to the originating cell column so match highlights and
            // jumps stay aligned.
            let mut lower: Vec<char> = Vec::new();
            let mut cols: Vec<usize> = Vec::new();
            for (col, cell) in line.cells.iter().enumerate() {
                if cell.cont {
                    continue;
                }
                for ch in cell.text().chars() {
                    for folded in ch.to_lowercase() {
                        lower.push(folded);
                        cols.push(col);
                    }
                }
            }
            if lower.len() < qchars.len() {
                continue;
            }
            let mut i = 0;
            while i + qchars.len() <= lower.len() {
                if lower[i..i + qchars.len()] == qchars[..] {
                    let start = cols[i];
                    let end_col = cols[i + qchars.len() - 1];
                    let cell_width = line
                        .cells
                        .get(end_col)
                        .map(|c| c.display_width())
                        .unwrap_or(1)
                        .max(1);
                    let end = (end_col + cell_width).min(self.snapshot.cols);
                    self.matches.push(SearchMatch { row, start, end });
                    if self.matches.len() >= MAX_SEARCH_MATCHES {
                        break 'rows;
                    }
                    i += qchars.len();
                } else {
                    i += 1;
                }
            }
        }
        if self.matches.is_empty() {
            return;
        }
        // Pick the nearest match in the requested direction.
        let current = (self.cursor_row, self.cursor_col);
        let ordered: Vec<usize> = if forward {
            (0..self.matches.len()).collect()
        } else {
            (0..self.matches.len()).rev().collect()
        };
        let chosen = ordered
            .into_iter()
            .find(|idx| {
                let m = &self.matches[*idx];
                if forward {
                    (m.row, m.start) >= current
                } else {
                    (m.row, m.start) < current
                }
            })
            .or_else(|| {
                if forward {
                    Some(0)
                } else {
                    self.matches.len().checked_sub(1)
                }
            });
        if let Some(idx) = chosen {
            self.match_idx = Some(idx);
            let m = self.matches[idx].clone();
            self.cursor_row = m.row;
            self.cursor_col = m.start;
            self.desired_col = m.start;
            self.ensure_visible(height);
        }
    }

    pub fn jump_match(&mut self, forward: bool, height: u16) -> CopyOutcome {
        if self.matches.is_empty() {
            return CopyOutcome::Toast("No matches".into());
        }
        let cur = self.match_idx.unwrap_or(0);
        let next = if forward {
            (cur + 1) % self.matches.len()
        } else if cur == 0 {
            self.matches.len() - 1
        } else {
            cur - 1
        };
        self.match_idx = Some(next);
        let m = self.matches[next].clone();
        self.cursor_row = m.row;
        self.cursor_col = m.start;
        self.desired_col = m.start;
        self.ensure_visible(height);
        CopyOutcome::Continue
    }

    pub fn search_status(&self) -> Option<String> {
        let query = self.query.as_ref()?;
        let idx = self.match_idx.map(|i| i + 1).unwrap_or(0);
        Some(format!("/{}  {}/{}", query, idx, self.matches.len()))
    }

    fn selection_range(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.anchor?;
        let a = (anchor.0, anchor.1);
        let b = (self.cursor_row, self.cursor_col);
        if a <= b {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        if !self.selecting {
            return false;
        }
        let Some((start, end)) = self.selection_range() else {
            return false;
        };
        if row < start.0 || row > end.0 {
            return false;
        }
        if row == start.0 && col < start.1 {
            return false;
        }
        if row == end.0 && col > end.1 {
            return false;
        }
        true
    }

    pub fn match_highlight(&self, row: usize, col: usize) -> Option<bool> {
        // Matches are generated in row/column order.
        let idx = self
            .matches
            .partition_point(|m| (m.row, m.start) <= (row, col));
        if idx == 0 {
            return None;
        }
        let m = &self.matches[idx - 1];
        if m.row == row && col >= m.start && col < m.end {
            Some(self.match_idx == Some(idx - 1))
        } else {
            None
        }
    }

    pub fn selection_text(&self) -> String {
        let (start, end) = match self.selection_range() {
            Some(range) => range,
            None => {
                // No explicit selection: copy the current line.
                let row = self.cursor_row;
                let line = self.row_text(row);
                return line.trim_end().to_string();
            }
        };
        let mut out = String::new();
        for row in start.0..=end.0 {
            let line = match self.snapshot.rows.get(row) {
                Some(line) => line,
                None => continue,
            };
            let start_col = if row == start.0 {
                self.adjust_start(row, start.1)
            } else {
                0
            };
            let end_col = if row == end.0 {
                self.adjust_end(row, end.1)
            } else {
                line.cells.len().saturating_sub(1)
            };
            let mut text = String::new();
            for (col, cell) in line.cells.iter().enumerate() {
                if col < start_col || col > end_col {
                    continue;
                }
                if cell.cont {
                    continue;
                }
                text.push(cell.ch);
                if let Some(extra) = &cell.extra {
                    text.push_str(extra);
                }
            }
            if !line.wrapped || row == end.0 {
                while text.ends_with(' ') {
                    text.pop();
                }
            }
            out.push_str(&text);
            if row != end.0 && !line.wrapped {
                out.push('\n');
            }
        }
        out
    }

    fn adjust_start(&self, row: usize, col: usize) -> usize {
        let Some(line) = self.snapshot.rows.get(row) else {
            return col;
        };
        let mut c = col;
        if line.cells.get(c).map(|cell| cell.cont).unwrap_or(false) && c > 0 {
            c -= 1;
        }
        c
    }

    fn adjust_end(&self, _row: usize, col: usize) -> usize {
        col
    }

    pub fn title(&self) -> &str {
        &self.snapshot.title
    }

    pub fn cursor_position(&self, height: u16) -> Option<(u16, u16)> {
        let (top, bottom) = self.viewport(height);
        if self.cursor_row < top || self.cursor_row >= bottom {
            return None;
        }
        Some((self.cursor_col as u16, (self.cursor_row - top) as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Style;

    fn snapshot() -> CopySnapshot {
        let mut rows = Vec::new();
        for text in ["first line", "second line", "third line"] {
            let mut cells = Vec::new();
            for ch in text.chars() {
                cells.push(crate::term::Cell::ch(ch, Style::default()));
            }
            rows.push(Row {
                cells,
                wrapped: false,
            });
        }
        CopySnapshot {
            cols: 12,
            screen_rows: 3,
            rows,
            cursor: Cursor {
                row: 1,
                col: 3,
                pending_wrap: false,
            },
            title: "bash".into(),
            reverse: false,
        }
    }

    #[test]
    fn enters_at_live_cursor() {
        let copy = CopyState::new(1, 1, snapshot());
        assert_eq!(copy.cursor_row, 1);
        assert_eq!(copy.cursor_col, 3);
    }

    #[test]
    fn selection_copies_lines() {
        let mut copy = CopyState::new(1, 1, snapshot());
        copy.anchor = Some((0, 0));
        copy.selecting = true;
        copy.cursor_row = 1;
        copy.cursor_col = 5;
        assert_eq!(copy.selection_text(), "first line\nsecond");
    }

    #[test]
    fn no_selection_copies_current_line() {
        let mut copy = CopyState::new(1, 1, snapshot());
        copy.cursor_row = 2;
        assert_eq!(copy.selection_text(), "third line");
    }

    #[test]
    fn search_finds_and_jumps() {
        let mut copy = CopyState::new(1, 1, snapshot());
        copy.set_search("line", true, 3);
        assert_eq!(copy.matches.len(), 3);
        assert_eq!(copy.matches[0].row, 0);
        assert_eq!(copy.cursor_row, 1);
        copy.jump_match(true, 3);
        assert_eq!(copy.cursor_row, 2);
    }

    #[test]
    fn wrapped_lines_keep_real_trailing_spaces() {
        let mut c0: Vec<_> = "abcd "
            .chars()
            .map(|c| crate::term::Cell::ch(c, Style::default()))
            .collect();
        c0.truncate(5);
        let c1: Vec<_> = "e"
            .chars()
            .map(|c| crate::term::Cell::ch(c, Style::default()))
            .collect();
        let snapshot = CopySnapshot {
            cols: 5,
            screen_rows: 2,
            rows: vec![
                Row {
                    cells: c0,
                    wrapped: true,
                },
                Row {
                    cells: c1,
                    wrapped: false,
                },
            ],
            cursor: Cursor {
                row: 0,
                col: 0,
                pending_wrap: false,
            },
            title: "test".into(),
            reverse: false,
        };
        let mut copy = CopyState::new(1, 1, snapshot);
        copy.anchor = Some((0, 0));
        copy.selecting = true;
        copy.cursor_row = 1;
        copy.cursor_col = 0;
        assert_eq!(copy.selection_text(), "abcd e");
    }

    #[test]
    fn search_survives_lowercase_expansion() {
        let mut cells = Vec::new();
        for ch in "İaaa".chars() {
            cells.push(crate::term::Cell::ch(ch, Style::default()));
        }
        let snapshot = CopySnapshot {
            cols: 4,
            screen_rows: 1,
            rows: vec![Row {
                cells,
                wrapped: false,
            }],
            cursor: Cursor {
                row: 0,
                col: 0,
                pending_wrap: false,
            },
            title: "test".into(),
            reverse: false,
        };
        let mut copy = CopyState::new(1, 1, snapshot);
        copy.set_search("a", true, 1);
        let starts: Vec<usize> = copy.matches.iter().map(|m| m.start).collect();
        assert_eq!(starts, vec![1, 2, 3]);
    }

    #[test]
    fn match_highlight_uses_sorted_ranges() {
        let mut copy = CopyState::new(1, 1, snapshot());
        copy.set_search("line", true, 3);
        assert_eq!(copy.match_highlight(0, 6), Some(false));
        assert_eq!(copy.match_highlight(0, 0), None);
        let current_row = copy.matches[copy.match_idx.unwrap()].row;
        let current_col = copy.matches[copy.match_idx.unwrap()].start;
        assert_eq!(copy.match_highlight(current_row, current_col), Some(true));
    }

    #[test]
    fn scroll_percent_moves() {
        let mut copy = CopyState::new(1, 1, snapshot());
        assert_eq!(copy.scroll_percent(), 100);
        copy.scroll_view(1, 2);
        assert!(copy.scroll_percent() < 100);
    }

    #[test]
    fn wheel_and_navigation_keep_cursor_visible() {
        let mut copy = CopyState::new(1, 1, snapshot());
        copy.cursor_row = 0;
        copy.ensure_visible(1);
        let (top, bottom) = copy.viewport(1);
        assert!(copy.cursor_row >= top && copy.cursor_row < bottom);
    }
}
