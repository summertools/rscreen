//! Frame composition and highly efficient ANSI diff rendering.
//!
//! A `Canvas` is a semantic grid (characters + styles + hyperlinks). Frames are
//! diffed against the previous canvas and converted to a minimal ANSI update.
//! Tests feed the generated ANSI back into our own terminal emulator and assert
//! that the resulting grid matches the canvas, which catches wide-character,
//! style and cursor bugs.

use std::sync::Arc;

use crate::config::Theme;
use crate::protocol::Caps;
use crate::term::{attrs, Cell, Color, Style};
use crate::util::{truncate_display, width as text_width};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorView {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    /// DECSCUSR style 0..=6.
    pub shape: u8,
}

#[derive(Clone, Debug)]
pub struct Canvas {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    pub cursor: Option<CursorView>,
}

impl Canvas {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(Style::default()); cols as usize * rows as usize],
            cursor: None,
        }
    }

    #[inline]
    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.cols as usize + x as usize
    }

    #[inline]
    pub fn get(&self, x: u16, y: u16) -> &Cell {
        &self.cells[self.idx(x, y)]
    }

    pub fn set_cursor(&mut self, cursor: Option<CursorView>) {
        self.cursor = cursor;
    }

    /// Overwrite a cell, repairing any wide glyph it belonged to.
    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let i = self.idx(x, y);
        if self.cells[i].cont && x > 0 {
            let lead = self.idx(x - 1, y);
            let style = self.cells[lead].style;
            self.cells[lead].reset(style);
        }
        if self.cells[i].wide && x + 1 < self.cols {
            let cont = self.idx(x + 1, y);
            let style = self.cells[cont].style;
            self.cells[cont].reset(style);
        }
        self.cells[i] = cell;
    }

    pub fn fill(&mut self, x: u16, y: u16, w: u16, h: u16, cell: Cell) {
        let x1 = x.saturating_add(w).min(self.cols);
        let y1 = y.saturating_add(h).min(self.rows);
        for row in y..y1 {
            for col in x..x1 {
                self.set(col, row, cell.clone());
            }
        }
    }

    pub fn fill_style(&mut self, x: u16, y: u16, w: u16, h: u16, style: Style) {
        self.fill(x, y, w, h, Cell::blank(style));
    }

    /// Draw text clipped to `max_w` display columns. Returns the end column.
    pub fn put_str(&mut self, x: u16, y: u16, max_w: u16, text: &str, style: Style) -> u16 {
        let mut col = x;
        let limit = x.saturating_add(max_w).min(self.cols);
        let mut last: Option<u16> = None;
        for ch in text.chars() {
            if ch.is_control() {
                continue;
            }
            let w = crate::term::char_width(ch);
            if w == 0 {
                if let Some(pos) = last {
                    let i = self.idx(pos, y);
                    self.cells[i].append_combining(ch);
                }
                continue;
            }
            if col >= limit {
                break;
            }
            if w == 2 {
                if col + 1 >= limit {
                    self.set(col, y, Cell::blank(style));
                    col += 1;
                    break;
                }
                let lead = Cell {
                    ch,
                    extra: None,
                    style,
                    link: None,
                    wide: true,
                    cont: false,
                };
                self.set(col, y, lead);
                let mut cont = Cell::blank(style);
                cont.cont = true;
                self.set(col + 1, y, cont);
                last = Some(col);
                col += 2;
            } else {
                self.set(
                    col,
                    y,
                    Cell {
                        ch,
                        extra: None,
                        style,
                        link: None,
                        wide: false,
                        cont: false,
                    },
                );
                last = Some(col);
                col += 1;
            }
        }
        col.min(self.cols)
    }

    /// Copy a cell from a pane, repairing any wide glyph boundary damage caused
    /// by clipping.
    pub fn set_pane_cell(&mut self, x: u16, y: u16, cell: &Cell, reverse: bool) {
        let mut cell = cell.clone();
        if reverse {
            std::mem::swap(&mut cell.style.fg, &mut cell.style.bg);
        }
        self.set(x, y, cell);
    }

    /// Ensure every wide glyph has its companion and no stray continuations
    /// remain. Call after direct grid manipulation.
    pub fn normalize_wide(&mut self) {
        for y in 0..self.rows {
            for x in 0..self.cols {
                let i = self.idx(x, y);
                let (wide, cont) = (self.cells[i].wide, self.cells[i].cont);
                if cont {
                    let valid = x > 0 && self.cells[self.idx(x - 1, y)].wide;
                    if !valid {
                        let style = self.cells[i].style;
                        self.cells[i].reset(style);
                    }
                } else if wide {
                    let valid = x + 1 < self.cols && self.cells[self.idx(x + 1, y)].cont;
                    if !valid {
                        let style = self.cells[i].style;
                        self.cells[i].reset(style);
                    }
                }
            }
        }
    }

    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        self.cursor
            .filter(|c| c.visible)
            .map(|c| (c.x.min(self.cols - 1), c.y.min(self.rows - 1)))
    }

    #[cfg(test)]
    fn row_text_for_test(&self, y: u16) -> String {
        let mut out = String::new();
        for x in 0..self.cols {
            let cell = self.get(x, y);
            if !cell.cont {
                out.push(cell.ch);
                if let Some(extra) = &cell.extra {
                    out.push_str(extra);
                }
            }
        }
        out
    }
}

/// Draw a rounded panel with a shadow. Returns the inner content rectangle.
pub fn draw_panel(
    canvas: &mut Canvas,
    theme: &Theme,
    rect: Rect,
    title: Option<&str>,
    title_style: Style,
) -> Rect {
    if rect.w < 2 || rect.h < 2 {
        return rect;
    }
    // Soft shadow on the right and bottom.
    let shadow_fill = Cell::blank(Style::default().bg(theme.shadow));
    if rect.x + rect.w < canvas.cols && rect.y + rect.h + 1 < canvas.rows {
        canvas.fill(rect.x + 1, rect.y + rect.h, rect.w, 1, shadow_fill.clone());
    }
    if rect.y + rect.h < canvas.rows {
        canvas.fill(rect.x + rect.w, rect.y + 1, 1, rect.h, shadow_fill);
    }
    canvas.fill(
        rect.x,
        rect.y,
        rect.w,
        rect.h,
        Cell::blank(Style::default().bg(theme.overlay_bg)),
    );
    let border = Style::default()
        .fg(theme.overlay_border)
        .bg(theme.overlay_bg);
    let x1 = rect.x + rect.w - 1;
    let y1 = rect.y + rect.h - 1;
    for x in rect.x + 1..x1 {
        canvas.set(x, rect.y, Cell::ch('─', border));
        canvas.set(x, y1, Cell::ch('─', border));
    }
    for y in rect.y + 1..y1 {
        canvas.set(rect.x, y, Cell::ch('│', border));
        canvas.set(x1, y, Cell::ch('│', border));
    }
    canvas.set(rect.x, rect.y, Cell::ch('╭', border));
    canvas.set(x1, rect.y, Cell::ch('╮', border));
    canvas.set(rect.x, y1, Cell::ch('╰', border));
    canvas.set(x1, y1, Cell::ch('╯', border));
    let inner = Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.w.saturating_sub(4),
        rect.h.saturating_sub(2),
    );
    if let Some(title) = title {
        let label = format!(
            " {} ",
            truncate_display(title, rect.w.saturating_sub(6) as usize)
        );
        canvas.put_str(
            rect.x + 2,
            rect.y,
            rect.w.saturating_sub(4),
            &label,
            title_style,
        );
    }
    inner
}

/// Draw a single-line toast above `bottom`.
pub fn draw_toast(
    canvas: &mut Canvas,
    theme: &Theme,
    bottom: u16,
    text: &str,
    tone: Color,
    icon: char,
) {
    if canvas.rows == 0 || text.is_empty() {
        return;
    }
    let max = canvas.cols.saturating_sub(4);
    let label = truncate_display(text, max.saturating_sub(4) as usize);
    let text_w = text_width(&label) as u16;
    let total = text_w + 5;
    let x = canvas.cols.saturating_sub(total + 2);
    let y = bottom.saturating_sub(1);
    let bg = Style::default().fg(theme.status_fg).bg(theme.surface);
    canvas.fill(x, y, total, 1, Cell::blank(bg));
    canvas.set(
        x,
        y,
        Cell {
            ch: '▎',
            extra: None,
            style: Style::default().fg(tone).bg(theme.surface).bold(),
            link: None,
            wide: false,
            cont: false,
        },
    );
    canvas.set(
        x + 2,
        y,
        Cell {
            ch: icon,
            extra: None,
            style: Style::default().fg(tone).bg(theme.surface).bold(),
            link: None,
            wide: false,
            cont: false,
        },
    );
    let label_style = Style::default().fg(theme.status_fg).bg(theme.surface);
    canvas.put_str(x + 4, y, text_w, &label, label_style);
}

/// The hint strip used while the prefix key is pending (and in copy mode when
/// the compact status bar is active). Returns its y position.
pub fn draw_hint_strip(
    canvas: &mut Canvas,
    theme: &Theme,
    y: u16,
    badge_label: &str,
    badge_bg: Color,
    lead: &str,
    hints: &[(String, String)],
) -> u16 {
    if y >= canvas.rows {
        return y;
    }
    let style = Style::default().fg(theme.status_fg).bg(theme.surface_alt);
    canvas.fill(0, y, canvas.cols, 1, Cell::blank(style));
    let badge = Style::default().fg(theme.accent_text).bg(badge_bg);
    let mut x = 1;
    let badge_text = format!(" {} ", badge_label);
    let badge_w = text_width(&badge_text) as u16;
    x = canvas.put_str(x, y, badge_w, &badge_text, badge);
    x = canvas.put_str(x, y, 2, " ", style);
    let key_style = Style::default().fg(theme.yellow).bg(theme.surface_alt);
    let desc_style = Style::default().fg(theme.muted).bg(theme.surface_alt);
    let label_style = Style::default().fg(theme.accent).bg(theme.surface_alt);
    if !lead.is_empty() {
        x = canvas.put_str(x, y, canvas.cols.saturating_sub(x), lead, label_style);
        x = canvas.put_str(x, y, 1, " ", style);
    }
    let mut first = true;
    for (key, desc) in hints {
        let need = text_width(key) as u16 + text_width(desc) as u16 + 5;
        if x + need >= canvas.cols {
            break;
        }
        if !first {
            x = canvas.put_str(x, y, 3, "   ", style);
        }
        first = false;
        x = canvas.put_str(x, y, 8, key, key_style);
        x = canvas.put_str(x, y, 1, " ", style);
        x = canvas.put_str(x, y, canvas.cols.saturating_sub(x), desc, desc_style);
    }
    y
}

#[derive(Clone, Debug)]
pub struct WindowTab {
    pub index: usize,
    pub title: String,
    pub bell: bool,
    pub active: bool,
}

pub struct StatusView<'a> {
    pub session: &'a str,
    pub tabs: Vec<WindowTab>,
    pub pane_text: Option<String>,
    pub badge: Option<(String, Color, Color)>,
    pub clock: String,
    pub clients: usize,
    pub status_rows: u16,
    pub hints: &'a [(String, String)],
    pub version: &'a str,
}

/// Clickable regions produced while drawing the status bar.
#[derive(Default, Debug)]
pub struct StatusHits {
    /// (x, width, window index)
    pub tabs: Vec<(u16, u16, usize)>,
}

pub fn draw_status(canvas: &mut Canvas, theme: &Theme, view: &StatusView<'_>) -> StatusHits {
    let mut hits = StatusHits::default();
    if canvas.rows == 0 {
        return hits;
    }
    let rows = view.status_rows.clamp(1, 2).min(canvas.rows);
    let y = canvas.rows - rows;
    let bg = Style::default().bg(theme.status_bg);
    canvas.fill(0, y, canvas.cols, rows, Cell::blank(bg));

    // Right-hand information is laid out first so the left side can reserve
    // a bounded area for it.
    let mut right = String::new();
    if let Some(pane) = &view.pane_text {
        right.push_str(pane);
    }
    if view.clients > 1 {
        if !right.is_empty() {
            right.push_str("  ·  ");
        }
        right.push_str(&format!("{} clients", view.clients));
    }
    if !view.clock.is_empty() {
        if !right.is_empty() {
            right.push_str("  ·  ");
        }
        right.push_str(&view.clock);
    }
    let right_style = Style::default().fg(theme.muted).bg(theme.status_bg);
    let max_right = if canvas.cols >= 30 {
        (canvas.cols / 2) as usize
    } else {
        0
    };
    let right = if max_right > 0 {
        truncate_display(&right, max_right)
    } else {
        String::new()
    };
    let right_w = text_width(&right) as u16;
    let content_end = if right_w > 0 {
        let right_x = canvas.cols.saturating_sub(right_w + 1);
        canvas.put_str(right_x, y, right_w, &right, right_style);
        right_x.saturating_sub(1)
    } else {
        canvas.cols
    };

    // Brand + session name, clipped in front of the reserved right area.
    let accent = Style::default().fg(theme.accent).bg(theme.status_bg);
    let session_style = Style::default()
        .fg(theme.status_fg)
        .bg(theme.status_bg)
        .bold();
    let mut x = canvas.put_str(0, y, content_end.min(3), " ◆ ", accent);
    let session_max = (canvas.cols / 3)
        .max(6)
        .min(content_end.saturating_sub(x + 2));
    if session_max > 1 {
        let session = truncate_display(view.session, session_max.saturating_sub(1) as usize);
        x = canvas.put_str(x, y, session_max, &session, session_style);
        x = canvas.put_str(x, y, content_end.saturating_sub(x).min(2), "  ", bg);
    }

    // Optional mode badge (COPY / ZOOM).
    if let Some((label, fg, badge_bg)) = &view.badge {
        let badge_style = Style::default().fg(*fg).bg(*badge_bg).bold();
        let label = format!(" {} ", label);
        let w = text_width(&label) as u16;
        if x + w < content_end {
            x = canvas.put_str(x, y, w, &label, badge_style);
            x = canvas.put_str(x, y, content_end.saturating_sub(x).min(1), " ", bg);
        }
    }

    // Tabs, scrolling around the active tab when space is tight.
    let active = view.tabs.iter().position(|t| t.active).unwrap_or(0);
    let available = content_end.saturating_sub(x);
    if !view.tabs.is_empty() && available > 4 {
        let label = |tab: &WindowTab| -> String {
            let bell = if tab.bell { "● " } else { "" };
            format!(
                " {} {}{} ",
                tab.index,
                bell,
                truncate_display(&tab.title, 18)
            )
        };
        let widths: Vec<u16> = view
            .tabs
            .iter()
            .map(|t| text_width(&label(t)) as u16)
            .collect();
        let mut start = active;
        let mut end = active + 1;
        let mut used = widths[active];
        loop {
            let mut moved = false;
            if end < view.tabs.len() && used + widths[end] <= available {
                used += widths[end];
                end += 1;
                moved = true;
            }
            if start > 0 && used + widths[start - 1] < available {
                used += widths[start - 1] + 1;
                start -= 1;
                moved = true;
            }
            if !moved {
                break;
            }
        }
        if start > 0 && x + 2 < content_end {
            x = canvas.put_str(
                x,
                y,
                2,
                "‹",
                Style::default().fg(theme.muted).bg(theme.status_bg),
            );
        }
        let tabs_end = content_end;
        for idx in start..end {
            let tab = &view.tabs[idx];
            let text = label(tab);
            let w = text_width(&text) as u16;
            if x + w > tabs_end {
                break;
            }
            let style = if tab.active {
                Style::default()
                    .fg(theme.accent_text)
                    .bg(theme.accent)
                    .bold()
            } else {
                Style::default().fg(theme.muted).bg(theme.status_bg)
            };
            let drawn_start = x;
            x = canvas.put_str(x, y, w, &text, style);
            hits.tabs.push((drawn_start, x - drawn_start, tab.index));
        }
        if end < view.tabs.len() && x < canvas.cols {
            let _ = canvas.put_str(
                x,
                y,
                2,
                "›",
                Style::default().fg(theme.muted).bg(theme.status_bg),
            );
        }
    }

    // Second row: key hints + version.
    if rows >= 2 {
        let hy = y + 1;
        let style = Style::default().fg(theme.muted).bg(theme.status_bg);
        canvas.fill(0, hy, canvas.cols, 1, Cell::blank(style));
        let key_style = Style::default().fg(theme.accent).bg(theme.status_bg);
        let desc_style = Style::default().fg(theme.muted).bg(theme.status_bg);
        let version_style = Style::default().fg(theme.muted).bg(theme.status_bg);
        let vw = text_width(view.version) as u16;
        let hint_limit = canvas.cols.saturating_sub(vw + 3);
        let mut hx = 1;
        for (key, desc) in view.hints {
            if hx >= hint_limit {
                break;
            }
            hx = canvas.put_str(hx, hy, hint_limit.saturating_sub(hx), key, key_style);
            hx = canvas.put_str(hx, hy, 1, " ", style);
            hx = canvas.put_str(hx, hy, hint_limit.saturating_sub(hx), desc, desc_style);
            hx = canvas.put_str(hx, hy, 3, "   ", style);
        }
        canvas.put_str(
            canvas.cols.saturating_sub(vw + 1),
            hy,
            vw,
            view.version,
            version_style,
        );
    }
    hits
}

/// Center a panel of desired size within the canvas.
pub fn centered_rect(cols: u16, rows: u16, want_w: u16, want_h: u16) -> Rect {
    let w = want_w.min(cols.saturating_sub(2)).max(1);
    let h = want_h.min(rows.saturating_sub(1)).max(1);
    let x = (cols.saturating_sub(w)) / 2;
    let y = (rows.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

/// Encode a minimal ANSI update from `old` to `new`.
pub fn encode_frame(
    old: Option<&Canvas>,
    old_cursor: Option<CursorView>,
    new: &Canvas,
    caps: Caps,
) -> Vec<u8> {
    let full = match old {
        Some(old) => old.cols != new.cols || old.rows != new.rows,
        None => true,
    };
    let blank = if full {
        Some(Canvas::new(new.cols, new.rows))
    } else {
        None
    };
    let old = match (full, blank.as_ref()) {
        (true, Some(blank)) => blank,
        _ => old.unwrap(),
    };

    let mut out = Vec::new();
    let mut body = Vec::new();
    let mut link_open = false;
    let mut painted = false;

    if full {
        body.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
    }

    for y in 0..new.rows {
        let mut x = 0;
        while x < new.cols {
            let old_i = y as usize * old.cols as usize + x as usize;
            let new_i = y as usize * new.cols as usize + x as usize;
            if old.cells[old_i] == new.cells[new_i] {
                x += 1;
                continue;
            }
            let mut start = x;
            let mut end = x + 1;
            while end < new.cols {
                let oi = y as usize * old.cols as usize + end as usize;
                let ni = y as usize * new.cols as usize + end as usize;
                if old.cells[oi] != new.cells[ni] {
                    end += 1;
                } else {
                    break;
                }
            }
            // Ensure whole wide glyphs are painted and never start mid-glyph.
            if new.get(start, y).cont && start > 0 {
                start -= 1;
            }
            if old.get(start, y).cont && start > 0 {
                start -= 1;
            }
            if new.get(end - 1, y).wide && end < new.cols {
                end += 1;
            }
            if old.get(end - 1, y).wide && end < new.cols {
                end += 1;
            }
            if end <= start {
                x += 1;
                continue;
            }

            body.extend_from_slice(format!("\x1b[{};{}H", y + 1, start + 1).as_bytes());
            if link_open {
                body.extend_from_slice(b"\x1b]8;;\x1b\\");
                link_open = false;
            }
            body.extend_from_slice(b"\x1b[0m");
            let mut cur_style = Some(Style::default());
            let mut link: Option<Arc<str>> = None;
            for col in start..end {
                let cell = new.get(col, y);
                if cell.cont {
                    continue;
                }
                if cell.link != link {
                    if link_open {
                        body.extend_from_slice(b"\x1b]8;;\x1b\\");
                    }
                    if let Some(target) = &cell.link {
                        if target.chars().any(char::is_control) {
                            link_open = false;
                        } else {
                            body.extend_from_slice(b"\x1b]8;;");
                            body.extend_from_slice(target.as_bytes());
                            body.extend_from_slice(b"\x1b\\");
                            link_open = true;
                        }
                    } else {
                        link_open = false;
                    }
                    link = cell.link.clone();
                }
                if cur_style != Some(cell.style) {
                    body.extend_from_slice(sgr_bytes(cell.style, caps).as_slice());
                    cur_style = Some(cell.style);
                }
                let mut buf = [0u8; 4];
                let ch = if cell.ch.is_control() { ' ' } else { cell.ch };
                body.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                if let Some(extra) = &cell.extra {
                    for extra_ch in extra.chars().filter(|c| !c.is_control()) {
                        body.extend_from_slice(extra_ch.encode_utf8(&mut buf).as_bytes());
                    }
                }
                painted = true;
            }
            if link_open {
                body.extend_from_slice(b"\x1b]8;;\x1b\\");
                link_open = false;
            }
            x = end;
        }
    }

    let cursor_changed = old_cursor != new.cursor;
    if !painted && !cursor_changed && !full {
        return Vec::new();
    }

    body.extend_from_slice(b"\x1b[0m");
    if let Some(cursor) = new.cursor {
        let x = cursor.x.min(new.cols - 1);
        let y = cursor.y.min(new.rows - 1);
        body.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        if cursor.visible {
            let previous_shape = old_cursor.filter(|c| c.visible).map(|c| c.shape);
            if previous_shape != Some(cursor.shape) {
                body.extend_from_slice(cursor_shape(cursor.shape).as_bytes());
            }
            body.extend_from_slice(b"\x1b[?25h");
        } else {
            body.extend_from_slice(b"\x1b[?25l");
        }
    } else {
        body.extend_from_slice(b"\x1b[?25l");
    }

    out.extend_from_slice(b"\x1b[?2026h");
    // Close a hyperlink that a previous application (or a partially delivered
    // frame) may have left open, so no cell inherits a stale URL.
    out.extend_from_slice(b"\x1b]8;;\x1b\\");
    out.extend_from_slice(&body);
    out.extend_from_slice(b"\x1b[?2026l");
    out
}

fn cursor_shape(style: u8) -> &'static str {
    // 0 means terminal default; emit 0 to restore it.
    match style {
        1 => "\x1b[1 q",
        2 => "\x1b[2 q",
        3 => "\x1b[3 q",
        4 => "\x1b[4 q",
        5 => "\x1b[5 q",
        6 => "\x1b[6 q",
        _ => "\x1b[0 q",
    }
}

pub fn sgr_bytes(style: Style, caps: Caps) -> Vec<u8> {
    let mut parts: Vec<String> = vec!["0".to_string()];
    if style.attrs & attrs::BOLD != 0 {
        parts.push("1".into());
    }
    if style.attrs & attrs::DIM != 0 {
        parts.push("2".into());
    }
    if style.attrs & attrs::ITALIC != 0 {
        parts.push("3".into());
    }
    if style.attrs & attrs::UNDERLINE != 0 {
        parts.push("4".into());
    }
    if style.attrs & attrs::DOUBLE_UNDERLINE != 0 {
        parts.push("21".into());
    }
    if style.attrs & attrs::CURLY_UNDERLINE != 0 {
        parts.push("4:3".into());
    }
    if style.attrs & attrs::DOTTED_UNDERLINE != 0 {
        parts.push("4:4".into());
    }
    if style.attrs & attrs::DASHED_UNDERLINE != 0 {
        parts.push("4:5".into());
    }
    if style.attrs & attrs::BLINK != 0 {
        parts.push("5".into());
    }
    if style.attrs & attrs::REVERSE != 0 {
        parts.push("7".into());
    }
    if style.attrs & attrs::HIDDEN != 0 {
        parts.push("8".into());
    }
    if style.attrs & attrs::STRIKE != 0 {
        parts.push("9".into());
    }
    push_color(&mut parts, style.fg, 38, 30, 90, caps);
    push_color(&mut parts, style.bg, 48, 40, 100, caps);
    format!("\x1b[{}m", parts.join(";")).into_bytes()
}

fn push_color(
    parts: &mut Vec<String>,
    color: Color,
    extended: u16,
    base: u16,
    bright: u16,
    caps: Caps,
) {
    let color = if caps.truecolor {
        color
    } else {
        color.to_256()
    };
    match color {
        Color::Default => {}
        Color::Indexed(n) if n < 8 => parts.push(format!("{}", base + n as u16)),
        Color::Indexed(n) if n < 16 => parts.push(format!("{}", bright + (n as u16 - 8))),
        Color::Indexed(n) => parts.push(format!("{};5;{}", extended, n)),
        Color::Rgb(r, g, b) => parts.push(format!("{};2;{};{};{}", extended, r, g, b)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::Terminal;

    #[test]
    fn canvas_puts_wide_text_without_splitting() {
        let mut c = Canvas::new(5, 1);
        c.put_str(4, 0, 5, "中", Style::default());
        // Cannot fit; clipped to a blank.
        assert!(!c.get(4, 0).wide);
    }

    #[test]
    fn diff_roundtrips_through_emulator() {
        let mut new = Canvas::new(20, 6);
        let accent = Style::default()
            .fg(Color::Rgb(0x89, 0xb4, 0xfa))
            .bg(Color::Rgb(0x11, 0x11, 0x1b));
        new.put_str(1, 1, 10, "hello 中", accent);
        new.put_str(2, 2, 10, "world", Style::default().bold());
        new.set_cursor(Some(CursorView {
            x: 7,
            y: 1,
            visible: true,
            shape: 2,
        }));
        let frame = encode_frame(None, None, &new, Caps::default());
        assert!(!frame.is_empty());

        let mut term = Terminal::new(20, 6, 100);
        term.feed(&frame);
        for y in 0..6 {
            for x in 0..20 {
                let a = new.get(x, y);
                let b = term.screen.primary.get(y as usize, x as usize);
                assert_eq!(a.ch, b.ch, "char at {} {}", x, y);
                if !a.cont {
                    assert_eq!(a.style, b.style, "style at {} {}", x, y);
                }
            }
        }
        assert_eq!(term.screen.active().cursor.row, 1);
        assert_eq!(term.screen.active().cursor.col, 7);
        assert_eq!(term.screen.cursor_style, 2);

        // Second frame diff: change one cell and move the cursor.
        let mut second = new.clone();
        second.put_str(1, 3, 10, "again", Style::default());
        second.set_cursor(Some(CursorView {
            x: 0,
            y: 0,
            visible: true,
            shape: 2,
        }));
        let frame2 = encode_frame(Some(&new), new.cursor, &second, Caps::default());
        term.feed(&frame2);
        for y in 0..6 {
            for x in 0..20 {
                let a = second.get(x, y);
                let b = term.screen.primary.get(y as usize, x as usize);
                assert_eq!(a.ch, b.ch, "char at {} {}", x, y);
                assert_eq!(a.style, b.style, "style at {} {}", x, y);
            }
        }
        assert_eq!(term.screen.active().cursor.row, 0);
        assert_eq!(term.screen.active().cursor.col, 0);
    }

    #[test]
    fn text_input_cannot_inject_control_characters() {
        let mut canvas = Canvas::new(20, 2);
        canvas.put_str(0, 0, 20, "a\u{1b}]2;owned\u{7}b", Style::default());
        let text: String = canvas
            .row_text_for_test(0)
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        assert!(text.starts_with("a]2;ownedb"));
    }

    #[test]
    fn frame_encoder_sanitizes_control_cells_and_links() {
        let mut canvas = Canvas::new(20, 2);
        let mut cell = Cell::ch('\u{1b}', Style::default());
        cell.extra = Some("\u{009b}".into());
        cell.link = Some(std::sync::Arc::from("https://example.com/\u{1b}]8;;evil"));
        canvas.set(0, 0, cell);
        let frame = encode_frame(None, None, &canvas, Caps::default());
        let mut term = Terminal::new(20, 2, 100);
        term.feed(&frame);
        let got = term.screen.primary.get(0, 0);
        assert_eq!(got.ch, ' ');
        assert!(got.extra.is_none());
        assert!(got.link.is_none());
    }

    #[test]
    fn frame_closes_a_stale_hyperlink() {
        let mut term = Terminal::new(20, 2, 100);
        term.feed(b"\x1b]8;;https://stale.example\x1b\\old");
        let mut canvas = Canvas::new(20, 2);
        canvas.put_str(0, 1, 18, "plain", Style::default());
        let frame = encode_frame(None, None, &canvas, Caps::default());
        term.feed(&frame);
        assert!(term.screen.primary.get(1, 0).link.is_none());
        assert_eq!(term.screen.primary.get(1, 0).ch, 'p');
    }

    #[test]
    fn resize_to_blank_frame_still_emits_clear() {
        let old = Canvas::new(10, 3);
        let new = Canvas::new(20, 5);
        let frame = encode_frame(
            Some(&old),
            Some(CursorView {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
            }),
            &new,
            Caps::default(),
        );
        assert!(!frame.is_empty());
        assert!(frame.windows(4).any(|w| w == b"\x1b[2J"));
    }

    #[test]
    fn hyperlinks_survive_roundtrip() {
        let mut new = Canvas::new(20, 3);
        let mut cell = Cell::ch('L', Style::default());
        cell.link = Some(std::sync::Arc::from("https://example.com/a?b=c"));
        new.set(3, 1, cell);
        let frame = encode_frame(None, None, &new, Caps::default());
        let mut term = Terminal::new(20, 3, 100);
        term.feed(&frame);
        let got = term.screen.primary.get(1, 3);
        assert_eq!(got.ch, 'L');
        assert_eq!(got.link.as_deref(), Some("https://example.com/a?b=c"));
    }

    #[test]
    fn tiny_status_bars_do_not_panic() {
        let theme = Theme::default();
        for cols in 1..14 {
            for rows in 1..5 {
                let mut c = Canvas::new(cols, rows);
                let hints = vec![("C-a c".to_string(), "new".to_string())];
                let view = StatusView {
                    session: "s",
                    tabs: vec![WindowTab {
                        index: 1,
                        title: "long window title that gets clipped".into(),
                        bell: true,
                        active: true,
                    }],
                    pane_text: Some("2 panes".into()),
                    badge: Some(("COPY 99%".into(), Color::Default, Color::Default)),
                    clock: "23:59".into(),
                    clients: 3,
                    status_rows: 2,
                    hints: &hints,
                    version: "rscreen 0.1.0",
                };
                let _ = draw_status(&mut c, &theme, &view);
            }
        }
    }

    #[test]
    fn randomized_diff_roundtrip() {
        let mut term = Terminal::new(30, 10, 100);
        let mut canvas = Canvas::new(30, 10);
        let mut prev: Option<Canvas> = None;
        let mut cursor: Option<CursorView> = None;
        let mut seed = 0x1234_5678u32;
        let mut rng = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            seed
        };
        for frame_no in 0..40 {
            for _ in 0..12 {
                let x = (rng() % 30) as u16;
                let y = (rng() % 10) as u16;
                let style = Style::default()
                    .fg(Color::Rgb(
                        (rng() % 255) as u8,
                        (rng() % 255) as u8,
                        (rng() % 255) as u8,
                    ))
                    .bg(Color::Rgb((rng() % 255) as u8, 0, (rng() % 255) as u8));
                let text = match rng() % 4 {
                    0 => "中",
                    1 => "abc",
                    2 => " ",
                    _ => "→",
                };
                canvas.put_str(x, y, 30 - x, text, style);
            }
            for _ in 0..3 {
                let x = (rng() % 30) as u16;
                let y = (rng() % 10) as u16;
                canvas.fill_style(x, y, 1, 1, Style::default().bg(Color::Rgb(1, 2, 3)));
            }
            let cv = if frame_no % 3 == 0 {
                None
            } else {
                Some(CursorView {
                    x: (rng() % 30) as u16,
                    y: (rng() % 10) as u16,
                    visible: frame_no % 2 == 0,
                    shape: (rng() % 7) as u8,
                })
            };
            canvas.set_cursor(cv);
            let bytes = encode_frame(prev.as_ref(), cursor, &canvas, Caps::default());
            term.feed(&bytes);
            prev = Some(canvas.clone());
            cursor = cv;

            for y in 0..10 {
                for x in 0..30 {
                    let a = canvas.get(x, y);
                    let b = term.screen.primary.get(y as usize, x as usize);
                    assert_eq!(a.ch, b.ch, "frame {} cell {} {}", frame_no, x, y);
                    assert_eq!(a.style, b.style, "frame {} style {} {}", frame_no, x, y);
                    assert_eq!(a.cont, b.cont, "frame {} cont {} {}", frame_no, x, y);
                }
            }
        }
    }

    #[test]
    fn rgb_is_quantized_when_truecolor_is_unavailable() {
        let mut canvas = Canvas::new(10, 2);
        canvas.put_str(
            0,
            0,
            10,
            "x",
            Style::default()
                .fg(Color::Rgb(255, 0, 0))
                .bg(Color::Rgb(0, 128, 255)),
        );
        let frame = encode_frame(None, None, &canvas, Caps { truecolor: false });
        assert!(!frame.windows(5).any(|w| w == b"38;2;"));
        assert!(frame.windows(5).any(|w| w == b"38;5;"));
        let mut term = Terminal::new(10, 2, 100);
        term.feed(&frame);
        assert!(matches!(
            term.screen.primary.get(0, 0).style.fg,
            Color::Indexed(_)
        ));
    }

    #[test]
    fn unchanged_frame_is_empty() {
        let c = Canvas::new(10, 3);
        let frame = encode_frame(Some(&c), None, &c, Caps::default());
        assert!(frame.is_empty());
    }

    #[test]
    fn status_bar_renders_tabs() {
        let mut c = Canvas::new(80, 24);
        let theme = Theme::default();
        let hints = vec![("^a c".to_string(), "new".to_string())];
        let view = StatusView {
            session: "calm-fox",
            tabs: vec![
                WindowTab {
                    index: 1,
                    title: "bash".into(),
                    bell: false,
                    active: true,
                },
                WindowTab {
                    index: 2,
                    title: "vim".into(),
                    bell: true,
                    active: false,
                },
            ],
            pane_text: Some("2 panes".into()),
            badge: None,
            clock: "09:41".into(),
            clients: 1,
            status_rows: 2,
            hints: &hints,
            version: "rscreen 0.1.0",
        };
        let hits = draw_status(&mut c, &theme, &view);
        assert_eq!(hits.tabs.len(), 2);
        let text: String = c
            .cells
            .iter()
            .filter(|cell| !cell.cont)
            .map(|cell| cell.ch)
            .collect();
        assert!(text.contains("calm-fox"));
        assert!(text.contains("bash"));
        assert!(text.contains("09:41"));
    }
}
