//! Session UI state and modal overlays: command palette, help, confirmations,
//! prompts and toasts.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::layout::PaneId;
use crate::render::{centered_rect, draw_panel, Canvas, Rect, StatusView};
use crate::term::{attrs, Color, Style};
use crate::util::{fuzzy_score, truncate_display, width as text_width};

use super::copy::CopyState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    SelectWindow(u64),
    NewWindow,
    SplitRight,
    SplitDown,
    ClosePane,
    NextPane,
    PrevPane,
    ZoomPane,
    RenameWindow,
    KillWindow,
    NextWindow,
    PrevWindow,
    WindowPicker,
    CopyMode,
    PasteBuffer,
    Help,
    Detach,
    DetachAll,
    KillSession,
    ToggleStatusRows,
    SendPrefix,
}

#[derive(Clone, Debug)]
pub struct PaletteItem {
    pub label: String,
    pub hint: String,
    pub action: PaletteAction,
    pub category: PaletteCategory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteCategory {
    Window,
    Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteKind {
    All,
    Windows,
}

pub const MAX_INPUT_CHARS: usize = 4096;

/// Return a horizontally scrolled view of `line` that keeps the cursor inside
/// `max` display columns, plus the cursor's display offset.
fn input_window(line: &InputLine, max: usize) -> (String, usize) {
    let max = max.max(1);
    let chars = &line.chars;
    let mut start = 0;
    let mut cursor_width: usize = chars[..line.cursor.min(chars.len())]
        .iter()
        .map(|c| text_width(&c.to_string()))
        .sum();
    while start < line.cursor && cursor_width >= max {
        cursor_width = cursor_width.saturating_sub(text_width(&chars[start].to_string()));
        start += 1;
    }
    let tail: String = chars[start..].iter().collect();
    let shown = truncate_display(&tail, max);
    (shown, cursor_width.min(max.saturating_sub(1)))
}

#[derive(Clone, Debug)]
pub struct InputLine {
    pub chars: Vec<char>,
    pub cursor: usize,
}

impl InputLine {
    pub fn new(value: &str) -> Self {
        let chars: Vec<char> = value.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        if self.chars.len() >= MAX_INPUT_CHARS {
            return;
        }
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let room = MAX_INPUT_CHARS.saturating_sub(self.chars.len());
        for c in s.chars().take(room) {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    pub fn kill_to_start(&mut self) {
        self.chars.drain(..self.cursor);
        self.cursor = 0;
    }

    pub fn kill_to_end(&mut self) {
        self.chars.truncate(self.cursor);
    }

    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }
}

pub struct PaletteState {
    pub kind: PaletteKind,
    pub query: InputLine,
    pub selected: usize,
    pub scroll: usize,
    pub items: Vec<PaletteItem>,
}

impl PaletteState {
    pub fn new(kind: PaletteKind, items: Vec<PaletteItem>) -> Self {
        Self {
            kind,
            query: InputLine::new(""),
            selected: 0,
            scroll: 0,
            items,
        }
    }

    pub fn filtered(&self) -> Vec<&PaletteItem> {
        let q = self.query.text();
        if q.trim().is_empty() {
            return self.items.iter().collect();
        }
        let mut scored: Vec<(i32, &PaletteItem)> = self
            .items
            .iter()
            .filter_map(|item| {
                let score = fuzzy_score(&q, &item.label).or_else(|| fuzzy_score(&q, &item.hint))?;
                Some((score, item))
            })
            .collect();
        scored.sort_by_key(|(score, _)| -score);
        scored.into_iter().map(|(_, item)| item).collect()
    }

    pub fn selected_action(&self) -> Option<PaletteAction> {
        let filtered = self.filtered();
        filtered.get(self.selected).map(|item| item.action.clone())
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.filtered().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(len - 1) as isize;
        self.selected = (cur + delta).rem_euclid(len as isize) as usize;
    }

    pub fn query_changed(&mut self) {
        self.selected = 0;
        self.scroll = 0;
    }
}

pub struct HelpState {
    pub scroll: usize,
    pub sections: Vec<HelpSection>,
    /// Last visible line count, filled in by the renderer. Interior mutability
    /// keeps frame composition immutable while letting End/PageDown clamp the
    /// scroll position correctly.
    pub viewport: Cell<usize>,
}

pub struct HelpSection {
    pub title: &'static str,
    pub rows: Vec<(String, String)>,
}

impl HelpState {
    /// Total line count including group headers and blank separators.
    pub fn line_count(&self) -> usize {
        let rows: usize = self.sections.iter().map(|s| s.rows.len()).sum();
        rows + self.sections.len() + self.sections.len().saturating_sub(1)
    }

    pub fn max_scroll(&self) -> usize {
        self.line_count().saturating_sub(self.viewport.get().max(1))
    }

    pub fn new(sections: Vec<HelpSection>) -> Self {
        Self {
            scroll: 0,
            sections,
            viewport: Cell::new(20),
        }
    }
}

pub struct ConfirmState {
    pub message: String,
    pub detail: String,
    pub action: ConfirmAction,
    pub danger: bool,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    ClosePane { window_id: u64, pane: PaneId },
    CloseWindow { window_id: u64 },
    KillSession,
}

pub struct PromptState {
    pub title: String,
    pub line: InputLine,
    pub placeholder: String,
    pub action: PromptAction,
}

#[derive(Clone, Debug)]
pub enum PromptAction {
    RenameWindow { window_id: u64 },
    CopySearch { forward: bool },
}

pub enum Overlay {
    None,
    Palette(PaletteState),
    Help(HelpState),
    Confirm(ConfirmState),
    Prompt(PromptState),
}

pub struct Toast {
    pub text: String,
    pub tone: Color,
    pub icon: char,
    pub until: Instant,
}

pub struct UiState {
    pub overlay: Overlay,
    pub copy: Option<CopyState>,
    pub toast: Option<Toast>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            overlay: Overlay::None,
            copy: None,
            toast: None,
        }
    }
}

impl UiState {
    pub fn toast(&mut self, theme: &crate::config::Theme, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            tone: theme.green,
            icon: '✔',
            until: Instant::now() + Duration::from_millis(2200),
        });
    }

    pub fn toast_error(&mut self, theme: &crate::config::Theme, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            tone: theme.red,
            icon: '✖',
            until: Instant::now() + Duration::from_millis(3200),
        });
    }

    pub fn toast_info(&mut self, theme: &crate::config::Theme, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            tone: theme.accent,
            icon: '●',
            until: Instant::now() + Duration::from_millis(2200),
        });
    }

    pub fn tick(&mut self) {
        if let Some(toast) = &self.toast {
            if Instant::now() >= toast.until {
                self.toast = None;
            }
        }
    }

    pub fn overlay_open(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    pub fn is_modal(&self) -> bool {
        self.overlay_open()
    }

    pub fn close_overlay(&mut self) {
        self.overlay = Overlay::None;
    }

    pub fn selected_toast(&self) -> Option<&Toast> {
        self.toast.as_ref()
    }
}

pub fn draw_overlay(
    canvas: &mut Canvas,
    theme: &crate::config::Theme,
    ui: &UiState,
    status_rows: u16,
) {
    canvas.set_cursor(None);
    match &ui.overlay {
        Overlay::None => {}
        Overlay::Palette(palette) => draw_palette(canvas, theme, palette, status_rows),
        Overlay::Help(help) => draw_help(canvas, theme, help),
        Overlay::Confirm(confirm) => draw_confirm(canvas, theme, confirm),
        Overlay::Prompt(prompt) => draw_prompt(canvas, theme, prompt),
    }
}

fn scrim(canvas: &mut Canvas, rect: Rect) {
    for y in 0..canvas.rows {
        for x in 0..canvas.cols {
            if !rect.contains(x, y) {
                let cell = &mut canvas.cells[y as usize * canvas.cols as usize + x as usize];
                cell.style.attrs |= attrs::DIM;
            }
        }
    }
}

fn draw_palette(
    canvas: &mut Canvas,
    theme: &crate::config::Theme,
    palette: &PaletteState,
    _status_rows: u16,
) {
    let matches = palette.filtered().len();
    let visible = matches.min(10);
    let desired = if matches == 0 {
        8
    } else {
        (visible + 6) as u16
    };
    let want_h = if palette.kind == PaletteKind::Windows {
        desired.min(12)
    } else {
        desired
    };
    let rect = centered_rect(canvas.cols, canvas.rows, 72, want_h.max(8));
    scrim(canvas, rect);
    let title = match palette.kind {
        PaletteKind::All => "Command palette",
        PaletteKind::Windows => "Switch window",
    };
    let title_style = Style::default()
        .fg(theme.accent)
        .bg(theme.overlay_bg)
        .bold();
    let inner = draw_panel(canvas, theme, rect, Some(title), title_style);
    if inner.w < 8 || inner.h < 2 {
        return;
    }
    let muted = Style::default().fg(theme.muted).bg(theme.overlay_bg);
    let text = Style::default().fg(theme.status_fg).bg(theme.overlay_bg);
    let accent = Style::default().fg(theme.accent).bg(theme.overlay_bg);
    let selected_style = Style::default().fg(theme.status_fg).bg(theme.surface);
    let selected_accent = Style::default().fg(theme.accent).bg(theme.surface).bold();

    // Query line.
    let qy = inner.y;
    canvas.put_str(inner.x, qy, 2, "❯ ", accent);
    let query = palette.query.text();
    let query_x = inner.x + 2;
    let query_max = inner.w.saturating_sub(2);
    let cursor_x = if query.is_empty() {
        canvas.put_str(
            query_x + 1,
            qy,
            query_max.saturating_sub(1),
            "Type a window name or command…",
            muted,
        );
        query_x
    } else {
        let (shown, offset) = input_window(&palette.query, query_max as usize);
        canvas.put_str(query_x, qy, query_max, &shown, text);
        query_x + offset as u16
    };
    let cursor_x = cursor_x.min(query_x + query_max.saturating_sub(1));
    if cursor_x < inner.x + inner.w {
        canvas.set_cursor(Some(crate::render::CursorView {
            x: cursor_x,
            y: qy,
            visible: true,
            shape: 5,
        }));
    }

    let footer_y = inner.y + inner.h - 1;
    let footer = match palette.kind {
        PaletteKind::All => "↑↓ move   ↵ run   esc close",
        PaletteKind::Windows => "↑↓ move   ↵ switch   esc close",
    };
    if inner.h < 4 {
        if footer_y > qy {
            canvas.put_str(inner.x, footer_y, inner.w, footer, muted);
        }
        return;
    }

    // Divider.
    let divider_y = qy + 1;
    for x in inner.x..inner.x + inner.w {
        canvas.set(x, divider_y, crate::term::Cell::ch('─', muted));
    }

    canvas.put_str(inner.x, footer_y, inner.w, footer, muted);
    let list_top = divider_y + 1;
    let list_h = footer_y.saturating_sub(list_top) as usize;
    let total = palette.filtered().len();
    if total == 0 {
        canvas.put_str(inner.x, list_top, inner.w, "No matching commands", muted);
        return;
    }
    let selected = palette.selected.min(total - 1);
    let mut scroll = palette.scroll;
    if selected < scroll {
        scroll = selected;
    } else if selected >= scroll + list_h.max(1) {
        scroll = selected + 1 - list_h.max(1);
    }
    let max_scroll = total.saturating_sub(list_h.max(1));
    scroll = scroll.min(max_scroll);
    let filtered = palette.filtered();

    for (row, item) in filtered.iter().enumerate().skip(scroll).take(list_h) {
        let y = list_top + (row - scroll) as u16;
        let is_selected = row == selected;
        if is_selected {
            canvas.fill(
                inner.x,
                y,
                inner.w,
                1,
                crate::term::Cell::blank(selected_style),
            );
            canvas.put_str(inner.x, y, 2, "▸ ", selected_accent);
        } else {
            canvas.put_str(inner.x, y, 2, "  ", text);
        }
        let hint_w = if item.hint.is_empty() {
            0
        } else {
            (text_width(&item.hint) as u16 + 2).min(inner.w / 3)
        };
        let label_w = inner.w.saturating_sub(2 + hint_w + 1);
        let label_style = if is_selected {
            selected_style
        } else {
            Style::default().fg(theme.status_fg).bg(theme.overlay_bg)
        };
        let label = truncate_display(&item.label, label_w as usize);
        canvas.put_str(inner.x + 2, y, label_w, &label, label_style);
        if hint_w > 0 {
            let hint = truncate_display(&item.hint, hint_w.saturating_sub(2) as usize);
            let hx = inner.x + inner.w.saturating_sub(hint_w);
            canvas.put_str(
                hx,
                y,
                hint_w,
                &hint,
                if is_selected { selected_style } else { muted },
            );
        }
    }
    if list_h > 0 && total > list_h {
        let pos = format!("{}–{}/{}", scroll + 1, (scroll + list_h).min(total), total);
        let w = text_width(&pos) as u16;
        if (text_width(footer) as u16)
            .saturating_add(w)
            .saturating_add(1)
            <= inner.w
        {
            canvas.put_str(
                inner.x + inner.w.saturating_sub(w),
                footer_y,
                w,
                &pos,
                muted,
            );
        }
    }
}

fn draw_help(canvas: &mut Canvas, theme: &crate::config::Theme, help: &HelpState) {
    let want_h = 26.min(canvas.rows.saturating_sub(2)).max(6);
    let rect = centered_rect(canvas.cols, canvas.rows, 76, want_h);
    scrim(canvas, rect);
    let title_style = Style::default()
        .fg(theme.accent)
        .bg(theme.overlay_bg)
        .bold();
    let inner = draw_panel(canvas, theme, rect, Some("Keys & shortcuts"), title_style);
    if inner.w < 6 || inner.h < 1 {
        return;
    }
    let muted = Style::default().fg(theme.muted).bg(theme.overlay_bg);
    let key_style = Style::default()
        .fg(theme.accent)
        .bg(theme.overlay_bg)
        .bold();
    let desc_style = Style::default().fg(theme.status_fg).bg(theme.overlay_bg);
    let group_style = Style::default().fg(theme.mauve).bg(theme.overlay_bg).bold();
    let footer_y = inner.y + inner.h - 1;

    // Flatten lines for scrolling.
    enum Line<'a> {
        Group(&'a str),
        Row(&'a str, &'a str),
        Blank,
    }
    let mut lines: Vec<Line> = Vec::new();
    for (index, section) in help.sections.iter().enumerate() {
        if index > 0 {
            lines.push(Line::Blank);
        }
        lines.push(Line::Group(section.title));
        for (key, desc) in &section.rows {
            lines.push(Line::Row(key, desc));
        }
    }
    let list_h = footer_y.saturating_sub(inner.y) as usize;
    help.viewport.set(list_h.max(1));
    let max_scroll = lines.len().saturating_sub(list_h);
    let scroll = help.scroll.min(max_scroll);
    let key_width: u16 = 18.min(inner.w.saturating_sub(8)).max(2);
    let desc_x = inner.x + 2 + key_width;
    let desc_w = inner.w.saturating_sub(2 + key_width);
    for (i, line) in lines.iter().skip(scroll).take(list_h).enumerate() {
        let y = inner.y + i as u16;
        match line {
            Line::Blank => {}
            Line::Group(g) => {
                canvas.put_str(inner.x, y, inner.w, g, group_style);
            }
            Line::Row(key, desc) => {
                let key = format!(
                    "{:<width$}",
                    truncate_display(key, key_width as usize),
                    width = key_width as usize
                );
                canvas.put_str(inner.x + 1, y, key_width, &key, key_style);
                canvas.put_str(
                    desc_x,
                    y,
                    desc_w,
                    &truncate_display(desc, desc_w as usize),
                    desc_style,
                );
            }
        }
    }
    let footer = "↑↓ scroll   q / esc close";
    canvas.put_str(inner.x, footer_y, inner.w, footer, muted);
    if max_scroll > 0 {
        let pos = format!(
            "{}–{}/{}",
            scroll + 1,
            (scroll + list_h).min(lines.len()),
            lines.len()
        );
        let w = text_width(&pos) as u16;
        if (text_width(footer) as u16)
            .saturating_add(w)
            .saturating_add(1)
            <= inner.w
        {
            canvas.put_str(
                inner.x + inner.w.saturating_sub(w),
                footer_y,
                w,
                &pos,
                muted,
            );
        }
    }
}

fn draw_confirm(canvas: &mut Canvas, theme: &crate::config::Theme, confirm: &ConfirmState) {
    let rect = centered_rect(canvas.cols, canvas.rows, 58, 7);
    scrim(canvas, rect);
    let title_style = Style::default()
        .fg(if confirm.danger {
            theme.red
        } else {
            theme.accent
        })
        .bg(theme.overlay_bg)
        .bold();
    let inner = draw_panel(canvas, theme, rect, Some("Confirm"), title_style);
    if inner.w < 10 || inner.h < 3 {
        return;
    }
    let text = Style::default().fg(theme.status_fg).bg(theme.overlay_bg);
    let muted = Style::default().fg(theme.muted).bg(theme.overlay_bg);
    let key_style = Style::default()
        .fg(theme.yellow)
        .bg(theme.overlay_bg)
        .bold();
    let msg = truncate_display(&confirm.message, inner.w as usize);
    canvas.put_str(inner.x, inner.y, inner.w, &msg, text);
    if !confirm.detail.is_empty() && inner.h >= 4 {
        let detail = truncate_display(&confirm.detail, inner.w as usize);
        canvas.put_str(inner.x, inner.y + 1, inner.w, &detail, muted);
    }
    let footer_y = inner.y + inner.h - 1;
    let mut x = inner.x;
    let mut left = inner.w;
    x = canvas.put_str(x, footer_y, left.min(3), "[y]", key_style);
    left = left.saturating_sub(x - inner.x);
    x = canvas.put_str(x, footer_y, left.min(10), " confirm   ", muted);
    left = left.saturating_sub(x - inner.x);
    x = canvas.put_str(x, footer_y, left.min(3), "[n]", key_style);
    left = left.saturating_sub(x - inner.x);
    let _ = canvas.put_str(x, footer_y, left.min(8), " cancel", muted);
}

fn draw_prompt(canvas: &mut Canvas, theme: &crate::config::Theme, prompt: &PromptState) {
    let rect = centered_rect(canvas.cols, canvas.rows, 64, 6);
    scrim(canvas, rect);
    let title_style = Style::default()
        .fg(theme.accent)
        .bg(theme.overlay_bg)
        .bold();
    let inner = draw_panel(canvas, theme, rect, Some(&prompt.title), title_style);
    if inner.w < 8 || inner.h < 3 {
        return;
    }
    let text = Style::default().fg(theme.status_fg).bg(theme.overlay_bg);
    let muted = Style::default().fg(theme.muted).bg(theme.overlay_bg);
    let accent = Style::default().fg(theme.accent).bg(theme.overlay_bg);
    canvas.put_str(inner.x, inner.y, 2, "❯ ", accent);
    let value = prompt.line.text();
    let max = inner.w.saturating_sub(2);
    let x = inner.x + 2;
    let cursor_x = if value.is_empty() {
        canvas.put_str(x, inner.y, max, &prompt.placeholder, muted);
        x
    } else {
        let (shown, offset) = input_window(&prompt.line, max as usize);
        canvas.put_str(x, inner.y, max, &shown, text);
        x + offset as u16
    };
    canvas.set_cursor(Some(crate::render::CursorView {
        x: cursor_x.min(inner.x + inner.w.saturating_sub(1)),
        y: inner.y,
        visible: true,
        shape: 5,
    }));
    let footer_y = inner.y + inner.h - 1;
    canvas.put_str(inner.x, footer_y, inner.w, "↵ confirm   esc cancel", muted);
}

/// Build the status bar view. Kept here so the session can focus on state.
pub struct StatusInput<'a> {
    pub session: &'a str,
    pub tabs: Vec<crate::render::WindowTab>,
    pub pane_text: Option<String>,
    pub badge: Option<(String, Color, Color)>,
    pub clock: String,
    pub clients: usize,
    pub status_rows: u16,
    pub hints: &'a [(String, String)],
    pub version: &'a str,
}

pub fn draw_status_bar(
    canvas: &mut Canvas,
    theme: &crate::config::Theme,
    input: &StatusInput<'_>,
) -> crate::render::StatusHits {
    let view = StatusView {
        session: input.session,
        tabs: input.tabs.clone(),
        pane_text: input.pane_text.clone(),
        badge: input.badge.clone(),
        clock: input.clock.clone(),
        clients: input.clients,
        status_rows: input.status_rows,
        hints: input.hints,
        version: input.version,
    };
    crate::render::draw_status(canvas, theme, &view)
}

pub fn centered_message(canvas_cols: u16, canvas_rows: u16, width: u16, height: u16) -> Rect {
    centered_rect(canvas_cols, canvas_rows, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Theme;
    use crate::term::Color;

    fn palette_items() -> Vec<PaletteItem> {
        vec![
            PaletteItem {
                label: "1  workspace".into(),
                hint: "2 panes".into(),
                action: PaletteAction::SelectWindow(1),
                category: PaletteCategory::Window,
            },
            PaletteItem {
                label: "New window".into(),
                hint: "C-a c".into(),
                action: PaletteAction::NewWindow,
                category: PaletteCategory::Command,
            },
        ]
    }

    #[test]
    fn input_line_is_bounded() {
        let mut line = InputLine::new("");
        line.insert_str(&"x".repeat(MAX_INPUT_CHARS + 100));
        assert_eq!(line.chars.len(), MAX_INPUT_CHARS);
        line.insert('y');
        assert_eq!(line.chars.len(), MAX_INPUT_CHARS);
    }

    #[test]
    fn input_window_keeps_cursor_visible() {
        let mut line = InputLine::new("");
        line.insert_str("0123456789abcdef");
        let (shown, offset) = input_window(&line, 8);
        assert!(shown.chars().count() <= 8);
        assert!(offset < 8);
        assert!(shown.contains('f'));
        line.home();
        let (shown, offset) = input_window(&line, 8);
        assert_eq!(offset, 0);
        assert!(shown.starts_with('0'));
    }

    #[test]
    fn input_line_editing() {
        let mut line = InputLine::new("hello");
        line.left();
        line.backspace();
        assert_eq!(line.text(), "helo");
        line.insert('l');
        assert_eq!(line.text(), "hello");
        line.home();
        line.kill_to_end();
        assert_eq!(line.text(), "");
        line.insert_str("abc def");
        line.left();
        line.home();
        line.insert_str("X ");
        assert_eq!(line.text(), "X abc def");
    }

    #[test]
    fn palette_filters_and_wraps_selection() {
        let mut palette = PaletteState::new(PaletteKind::All, palette_items());
        assert_eq!(palette.filtered().len(), 2);
        palette.query.insert_str("win");
        palette.query_changed();
        assert!(palette.filtered().len() <= 2);
        let filtered = palette.filtered();
        assert!(filtered.iter().any(|i| i.label.contains("window")));
        palette.move_selection(1);
        assert!(palette.selected < palette.filtered().len());
    }

    #[test]
    fn overlays_draw_at_any_size() {
        let theme = Theme::default();
        let mut items = palette_items();
        items.push(PaletteItem {
            label: "Split pane right".into(),
            hint: "C-a |".into(),
            action: PaletteAction::SplitRight,
            category: PaletteCategory::Command,
        });
        let mut ui = UiState::default();
        let confirm = ConfirmState {
            message: "Kill window 1 (\"workspace\")?".into(),
            detail: "This is the last window; the session will end.".into(),
            action: ConfirmAction::KillSession,
            danger: true,
        };
        let prompt = PromptState {
            title: "Rename window".into(),
            line: InputLine::new("workspace"),
            placeholder: "Window title".into(),
            action: PromptAction::RenameWindow { window_id: 1 },
        };
        let help = HelpState::new(vec![
            HelpSection {
                title: "SESSION",
                rows: vec![("C-a d".into(), "Detach from this session".into())],
            },
            HelpSection {
                title: "WINDOWS",
                rows: vec![("C-a c".into(), "Create a new window".into())],
            },
        ]);
        for (cols, rows) in [(1, 1), (8, 4), (20, 6), (80, 24), (160, 50)] {
            for overlay in [
                Overlay::None,
                Overlay::Palette(PaletteState::new(PaletteKind::All, items.clone())),
                Overlay::Help(HelpState {
                    scroll: help.scroll,
                    sections: help
                        .sections
                        .iter()
                        .map(|s| HelpSection {
                            title: s.title,
                            rows: s.rows.clone(),
                        })
                        .collect(),
                    viewport: Cell::new(help.viewport.get()),
                }),
                Overlay::Confirm(ConfirmState {
                    message: confirm.message.clone(),
                    detail: confirm.detail.clone(),
                    action: confirm.action.clone(),
                    danger: confirm.danger,
                }),
                Overlay::Prompt(PromptState {
                    title: prompt.title.clone(),
                    line: InputLine::new(&prompt.line.text()),
                    placeholder: prompt.placeholder.clone(),
                    action: prompt.action.clone(),
                }),
            ] {
                ui.overlay = overlay;
                let mut canvas = Canvas::new(cols, rows);
                draw_overlay(&mut canvas, &theme, &ui, if rows > 1 { 1 } else { 0 });
                canvas.normalize_wide();
                for cell in &canvas.cells {
                    assert_ne!(cell.ch, '\u{0}');
                }
            }
        }
        let _ = Color::Default;
    }

    #[test]
    fn toast_and_strip_render_without_panicking() {
        let theme = Theme::default();
        for cols in 1..20 {
            let mut canvas = Canvas::new(cols, 3);
            crate::render::draw_hint_strip(
                &mut canvas,
                &theme,
                0,
                "PREFIX",
                theme.yellow,
                "C-a",
                &[("c".into(), "new window".into())],
            );
            crate::render::draw_toast(
                &mut canvas,
                &theme,
                3,
                "A rather long toast message that must be clipped",
                theme.green,
                '✔',
            );
        }
    }
}
