//! Client input parsing.
//!
//! Keyboard and mouse bytes arrive in arbitrary chunks. This state machine
//! handles prefix keys, escape-sequence boundaries, bracketed paste and SGR
//! mouse reports, and translates cursor keys according to the focused pane's
//! DECCKM mode.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Backspace,
    Enter,
    Tab,
    BackTab,
    Esc,
    F(u8),
    Char(char),
}

impl Key {
    pub fn label(&self) -> String {
        match self {
            Key::Up => "↑".into(),
            Key::Down => "↓".into(),
            Key::Left => "←".into(),
            Key::Right => "→".into(),
            Key::Home => "home".into(),
            Key::End => "end".into(),
            Key::PageUp => "pgup".into(),
            Key::PageDown => "pgdn".into(),
            Key::Insert => "ins".into(),
            Key::Delete => "del".into(),
            Key::Backspace => "bspace".into(),
            Key::Enter => "enter".into(),
            Key::Tab => "tab".into(),
            Key::BackTab => "backtab".into(),
            Key::Esc => "esc".into(),
            Key::F(n) => format!("f{}", n),
            Key::Char(c) => c.to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MouseEvent {
    /// Zero-based screen coordinates.
    pub x: u16,
    pub y: u16,
    /// Raw xterm button/modifier bits (without the 32 offset).
    pub button: u8,
    pub pressed: bool,
    pub motion: bool,
}

impl MouseEvent {
    pub fn is_wheel(&self) -> bool {
        self.button & 64 != 0
    }

    /// Some(up/down) for wheel events.
    pub fn wheel_direction(&self) -> Option<bool> {
        if self.is_wheel() {
            Some(self.button & 1 == 0) // true = up
        } else {
            None
        }
    }

    pub fn is_left(&self) -> bool {
        self.button & 3 == 0
    }

    pub fn is_release(&self) -> bool {
        !self.pressed
    }
}

#[derive(Clone, Debug)]
pub enum InputEvent {
    Data(Vec<u8>),
    Key(Key),
    PrefixKey(Key),
    PrefixUnknown(String),
    PrefixCancel,
    PrefixPending,
    Mouse(MouseEvent),
    Paste(Vec<u8>),
    PasteTooLarge,
    /// A complete escape/chord sequence (modified arrow, Alt+key, terminal
    /// response...). It is forwarded to applications verbatim, but overlays and
    /// copy mode ignore it instead of misreading the leading ESC as a cancel.
    RawKey(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Ss3,
    Str,
    Paste,
}

pub struct InputParser {
    state: State,
    seq: Vec<u8>,
    pub prefix_pending: bool,
    since: Option<Instant>,
    paste: Vec<u8>,
    paste_tail: Vec<u8>,
    paste_discard: bool,
    prefix_utf8_remaining: u8,
    prefix_utf8_buf: Vec<u8>,
    /// Set when the user pressed the prefix and then Esc alone.
    prefix_esc: bool,
}

impl Default for InputParser {
    fn default() -> Self {
        Self::new()
    }
}

impl InputParser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            seq: Vec::new(),
            prefix_pending: false,
            since: None,
            paste: Vec::new(),
            paste_tail: Vec::new(),
            paste_discard: false,
            prefix_utf8_remaining: 0,
            prefix_utf8_buf: Vec::new(),
            prefix_esc: false,
        }
    }

    pub fn feed(&mut self, bytes: &[u8], prefix: u8) -> Vec<InputEvent> {
        self.feed_with(bytes, prefix, true)
    }

    /// `allow_prefix` is false while a modal prompt is open, so Ctrl-A can be
    /// used for "beginning of line" instead of arming the session prefix.
    pub fn feed_with(&mut self, bytes: &[u8], prefix: u8, allow_prefix: bool) -> Vec<InputEvent> {
        let mut events = Vec::new();
        let mut data: Vec<u8> = Vec::new();
        let flush = |data: &mut Vec<u8>, events: &mut Vec<InputEvent>| {
            if !data.is_empty() {
                events.push(InputEvent::Data(std::mem::take(data)));
            }
        };

        for &byte in bytes {
            if self.prefix_utf8_remaining > 0 {
                self.prefix_utf8_buf.push(byte);
                self.prefix_utf8_remaining -= 1;
                if self.prefix_utf8_remaining == 0 {
                    self.prefix_pending = false;
                    let decoded = std::str::from_utf8(&self.prefix_utf8_buf)
                        .ok()
                        .and_then(|s| s.chars().next());
                    match decoded {
                        Some(ch) if !ch.is_control() => {
                            events.push(InputEvent::PrefixKey(Key::Char(ch)));
                        }
                        _ => events.push(InputEvent::PrefixCancel),
                    }
                    self.prefix_utf8_buf.clear();
                }
                continue;
            }
            if self.state == State::Paste {
                const LIMIT: usize = 32 * 1024 * 1024;
                if self.paste_discard {
                    self.paste_tail.push(byte);
                    if self.paste_tail.len() > 6 {
                        self.paste_tail.remove(0);
                    }
                    if self.paste_tail == b"\x1b[201~" {
                        self.paste_tail.clear();
                        self.paste_discard = false;
                        self.state = State::Ground;
                        self.since = None;
                        events.push(InputEvent::PasteTooLarge);
                    }
                    continue;
                }
                self.paste.push(byte);
                if self.paste.len() > LIMIT {
                    // Do not deliver a silently truncated paste. Keep only a
                    // short tail so the closing marker can still be found.
                    self.paste_discard = true;
                    let start = self.paste.len().saturating_sub(6);
                    self.paste_tail = self.paste[start..].to_vec();
                    self.paste.clear();
                    if self.paste_tail == b"\x1b[201~" {
                        self.paste_tail.clear();
                        self.paste_discard = false;
                        self.state = State::Ground;
                        self.since = None;
                        events.push(InputEvent::PasteTooLarge);
                    }
                    continue;
                }
                if self.paste.ends_with(b"\x1b[201~") {
                    let len = self.paste.len() - 6;
                    let payload = self.paste[..len].to_vec();
                    self.paste.clear();
                    self.paste_discard = false;
                    self.state = State::Ground;
                    self.since = None;
                    events.push(InputEvent::Paste(payload));
                }
                continue;
            }

            match self.state {
                State::Ground => {
                    if byte == 0x1b && !self.prefix_pending {
                        flush(&mut data, &mut events);
                        self.state = State::Esc;
                        self.seq.clear();
                        self.seq.push(0x1b);
                        self.since = Some(Instant::now());
                        continue;
                    }
                    if self.prefix_pending {
                        if byte == prefix {
                            self.prefix_pending = false;
                            flush(&mut data, &mut events);
                            // Sending the prefix literally is the standard
                            // escape hatch for applications that need ^A.
                            events.push(InputEvent::Data(vec![prefix]));
                            continue;
                        }
                        match byte {
                            0x1b => {
                                self.prefix_esc = true;
                                self.state = State::Esc;
                                self.seq.clear();
                                self.seq.push(0x1b);
                                self.since = Some(Instant::now());
                                continue;
                            }
                            0x09 => {
                                self.prefix_pending = false;
                                events.push(InputEvent::PrefixKey(Key::Tab));
                                continue;
                            }
                            0x0d | 0x0a => {
                                self.prefix_pending = false;
                                events.push(InputEvent::PrefixKey(Key::Enter));
                                continue;
                            }
                            0x7f | 0x08 => {
                                self.prefix_pending = false;
                                events.push(InputEvent::PrefixKey(Key::Backspace));
                                continue;
                            }
                            0x00..=0x1f => {
                                self.prefix_pending = false;
                                events.push(InputEvent::PrefixKey(Key::Char(byte as char)));
                                continue;
                            }
                            0x20..=0x7e => {
                                self.prefix_pending = false;
                                events.push(InputEvent::PrefixKey(Key::Char(byte as char)));
                                continue;
                            }
                            _ => {
                                let need = utf8_continuations(byte);
                                if need == 0 {
                                    self.prefix_pending = false;
                                    events.push(InputEvent::PrefixCancel);
                                } else {
                                    // Decode the complete character; pressing
                                    // prefix + é should either match a bound
                                    // unicode key or report an unknown key,
                                    // never silently eat it.
                                    self.prefix_utf8_remaining = need;
                                    self.prefix_utf8_buf.clear();
                                    self.prefix_utf8_buf.push(byte);
                                }
                                continue;
                            }
                        }
                    }
                    if allow_prefix && byte == prefix {
                        flush(&mut data, &mut events);
                        if self.prefix_pending {
                            self.prefix_pending = false;
                            events.push(InputEvent::Data(vec![prefix]));
                        } else {
                            self.prefix_pending = true;
                            events.push(InputEvent::PrefixPending);
                        }
                        continue;
                    }
                    data.push(byte);
                }
                State::Esc => {
                    self.since = Some(Instant::now());
                    match byte {
                        b'[' => {
                            self.state = State::Csi;
                            self.seq.push(byte);
                        }
                        b'O' => {
                            self.state = State::Ss3;
                            self.seq.push(byte);
                        }
                        b']' | b'P' | b'X' | b'^' | b'_' => {
                            self.state = State::Str;
                            self.seq.push(byte);
                        }
                        _ => {
                            // Alt+key or a lone escape followed by a quick key.
                            self.state = State::Ground;
                            self.seq.push(byte);
                            self.since = None;
                            if self.prefix_pending || self.prefix_esc {
                                self.prefix_pending = false;
                                self.prefix_esc = false;
                                flush(&mut data, &mut events);
                                events.push(InputEvent::PrefixKey(Key::Char(byte as char)));
                            } else {
                                flush(&mut data, &mut events);
                                events.push(InputEvent::RawKey(std::mem::take(&mut self.seq)));
                            }
                            self.seq.clear();
                        }
                    }
                }
                State::Csi => {
                    self.since = Some(Instant::now());
                    if self.seq.len() < 128 {
                        self.seq.push(byte);
                    }
                    if (0x40..=0x7e).contains(&byte) {
                        let seq = std::mem::take(&mut self.seq);
                        self.state = State::Ground;
                        self.since = None;
                        self.handle_csi(&seq, &mut events);
                    }
                }
                State::Ss3 => {
                    self.since = Some(Instant::now());
                    self.seq.push(byte);
                    self.state = State::Ground;
                    self.since = None;
                    self.handle_ss3(&mut events);
                }
                State::Str => {
                    self.since = Some(Instant::now());
                    self.seq.push(byte);
                    let done = byte == 0x07
                        || (self.seq.len() >= 2
                            && self.seq[self.seq.len() - 2] == 0x1b
                            && byte == b'\\');
                    if done || self.seq.len() > 16 * 1024 {
                        let seq = std::mem::take(&mut self.seq);
                        self.state = State::Ground;
                        self.since = None;
                        if self.prefix_esc {
                            self.prefix_esc = false;
                            self.prefix_pending = false;
                            events.push(InputEvent::PrefixCancel);
                        } else {
                            flush(&mut data, &mut events);
                            events.push(InputEvent::RawKey(seq));
                        }
                    }
                }
                State::Paste => unreachable!(),
            }
        }
        flush(&mut data, &mut events);
        events
    }

    fn handle_csi(&mut self, seq: &[u8], events: &mut Vec<InputEvent>) {
        // seq includes ESC and '['.
        let body = &seq[2..];
        if body.is_empty() {
            return;
        }
        // SGR mouse: ESC [ < b ; x ; y M/m
        if body[0] == b'<' {
            if let Some(mouse) = parse_sgr_mouse(body) {
                if self.prefix_pending {
                    self.prefix_pending = false;
                    events.push(InputEvent::PrefixCancel);
                }
                events.push(InputEvent::Mouse(mouse));
                return;
            }
        }
        if body == b"200~" {
            if self.prefix_pending || self.prefix_esc {
                self.prefix_pending = false;
                self.prefix_esc = false;
                events.push(InputEvent::PrefixCancel);
            }
            self.state = State::Paste;
            self.paste.clear();
            self.paste_tail.clear();
            self.paste_discard = false;
            return;
        }
        if body == b"I" || body == b"O" {
            // Focus reports from the outer terminal are not needed; ignoring
            // them avoids leaking them into applications.
            return;
        }
        if self.prefix_pending || self.prefix_esc {
            self.prefix_esc = false;
            self.prefix_pending = false;
            if let Some(key) = parse_csi_key(body) {
                events.push(InputEvent::PrefixKey(key));
            } else {
                events.push(InputEvent::PrefixUnknown(format!(
                    "ESC [ {}",
                    String::from_utf8_lossy(body)
                )));
            }
            return;
        }
        if let Some(key) = parse_csi_key(body) {
            events.push(InputEvent::Key(key));
        } else {
            let mut raw = Vec::with_capacity(seq.len());
            raw.extend_from_slice(seq);
            events.push(InputEvent::RawKey(raw));
        }
    }

    fn handle_ss3(&mut self, events: &mut Vec<InputEvent>) {
        let last = self.seq.last().copied().unwrap_or(0);
        let key = match last {
            b'A' => Some(Key::Up),
            b'B' => Some(Key::Down),
            b'C' => Some(Key::Right),
            b'D' => Some(Key::Left),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            b'P' => Some(Key::F(1)),
            b'Q' => Some(Key::F(2)),
            b'R' => Some(Key::F(3)),
            b'S' => Some(Key::F(4)),
            _ => None,
        };
        if self.prefix_pending || self.prefix_esc {
            self.prefix_esc = false;
            self.prefix_pending = false;
            match key {
                Some(key) => events.push(InputEvent::PrefixKey(key)),
                None => events.push(InputEvent::PrefixUnknown(format!("ESC O {}", last as char))),
            }
            self.seq.clear();
            return;
        }
        match key {
            Some(key) => events.push(InputEvent::Key(key)),
            None => {
                let mut raw = Vec::new();
                raw.extend_from_slice(&self.seq);
                events.push(InputEvent::RawKey(raw));
            }
        }
        self.seq.clear();
    }

    /// Flush an incomplete escape sequence (notably a lone ESC) so that vim
    /// receives Escape promptly. Returns events to deliver.
    pub fn flush_timeout(&mut self, now: Instant) -> Vec<InputEvent> {
        let Some(since) = self.since else {
            return Vec::new();
        };
        let timeout = if self.state == State::Esc {
            Duration::from_millis(25)
        } else {
            Duration::from_millis(300)
        };
        if now.duration_since(since) < timeout {
            return Vec::new();
        }
        let mut events = Vec::new();
        if self.state == State::Esc {
            self.state = State::Ground;
            self.since = None;
            if self.prefix_pending || self.prefix_esc {
                self.prefix_pending = false;
                self.prefix_esc = false;
                events.push(InputEvent::PrefixCancel);
            } else {
                events.push(InputEvent::Data(vec![0x1b]));
            }
            self.seq.clear();
        } else if matches!(self.state, State::Csi | State::Ss3 | State::Str) {
            let seq = std::mem::take(&mut self.seq);
            self.state = State::Ground;
            self.since = None;
            self.prefix_pending = false;
            self.prefix_esc = false;
            events.push(InputEvent::RawKey(seq));
        }
        events
    }

    pub fn is_mid_sequence(&self) -> bool {
        self.state != State::Ground || self.prefix_pending
    }

    pub fn reset(&mut self) {
        self.state = State::Ground;
        self.seq.clear();
        self.paste.clear();
        self.paste_tail.clear();
        self.paste_discard = false;
        self.prefix_utf8_remaining = 0;
        self.prefix_utf8_buf.clear();
        self.prefix_pending = false;
        self.prefix_esc = false;
        self.since = None;
    }
}

fn utf8_continuations(lead: u8) -> u8 {
    if (0xC2..=0xDF).contains(&lead) {
        1
    } else if (0xE0..=0xEF).contains(&lead) {
        2
    } else if (0xF0..=0xF4).contains(&lead) {
        3
    } else {
        0
    }
}

fn parse_csi_key(body: &[u8]) -> Option<Key> {
    // Do not translate sequences with modifiers; applications expect them
    // verbatim (e.g. Shift+Arrow = CSI 1;2A).
    if body.contains(&b';') {
        return None;
    }
    match body {
        b"A" => Some(Key::Up),
        b"B" => Some(Key::Down),
        b"C" => Some(Key::Right),
        b"D" => Some(Key::Left),
        b"H" => Some(Key::Home),
        b"F" => Some(Key::End),
        b"Z" => Some(Key::BackTab),
        b"2~" => Some(Key::Insert),
        b"3~" => Some(Key::Delete),
        b"5~" => Some(Key::PageUp),
        b"6~" => Some(Key::PageDown),
        b"1~" | b"7~" => Some(Key::Home),
        b"4~" | b"8~" => Some(Key::End),
        b"11~" => Some(Key::F(1)),
        b"12~" => Some(Key::F(2)),
        b"13~" => Some(Key::F(3)),
        b"14~" => Some(Key::F(4)),
        b"15~" => Some(Key::F(5)),
        b"17~" => Some(Key::F(6)),
        b"18~" => Some(Key::F(7)),
        b"19~" => Some(Key::F(8)),
        b"20~" => Some(Key::F(9)),
        b"21~" => Some(Key::F(10)),
        b"23~" => Some(Key::F(11)),
        b"24~" => Some(Key::F(12)),
        _ => None,
    }
}

fn parse_sgr_mouse(body: &[u8]) -> Option<MouseEvent> {
    let (payload, pressed) = match body.strip_suffix(b"M") {
        Some(p) => (p, true),
        None => (body.strip_suffix(b"m")?, false),
    };
    let payload = payload.strip_prefix(b"<")?;
    let mut parts = payload.split(|b| *b == b';');
    let button: u8 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    let x: u16 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    let y: u16 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(MouseEvent {
        x: x.saturating_sub(1),
        y: y.saturating_sub(1),
        button,
        pressed,
        motion: button & 32 != 0,
    })
}

/// Encode a special key for a pane, honoring application cursor mode.
pub fn encode_key(key: Key, app_cursor: bool) -> Vec<u8> {
    let arrow = |c: char| -> Vec<u8> {
        let prefix = if app_cursor { "O" } else { "[" };
        format!("\x1b{}{}", prefix, c).into_bytes()
    };
    match key {
        Key::Up => arrow('A'),
        Key::Down => arrow('B'),
        Key::Right => arrow('C'),
        Key::Left => arrow('D'),
        Key::Home => arrow('H'),
        Key::End => arrow('F'),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Backspace => vec![0x7f],
        Key::Enter => vec![0x0d],
        Key::Tab => vec![0x09],
        Key::BackTab => b"\x1b[Z".to_vec(),
        Key::Esc => vec![0x1b],
        Key::F(1) => b"\x1bOP".to_vec(),
        Key::F(2) => b"\x1bOQ".to_vec(),
        Key::F(3) => b"\x1bOR".to_vec(),
        Key::F(4) => b"\x1bOS".to_vec(),
        Key::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => 15,
            };
            format!("\x1b[{}~", code).into_bytes()
        }
        Key::Char(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
    }
}

/// Parse a key name from configuration (`c`, `tab`, `pageup`, ...).
pub fn parse_key_name(name: &str) -> Option<Key> {
    let n = name.trim();
    match n.to_ascii_lowercase().as_str() {
        "tab" => return Some(Key::Tab),
        "space" => return Some(Key::Char(' ')),
        "enter" | "return" => return Some(Key::Enter),
        "esc" | "escape" => return Some(Key::Esc),
        "up" => return Some(Key::Up),
        "down" => return Some(Key::Down),
        "left" => return Some(Key::Left),
        "right" => return Some(Key::Right),
        "home" => return Some(Key::Home),
        "end" => return Some(Key::End),
        "pageup" | "pgup" => return Some(Key::PageUp),
        "pagedown" | "pgdn" => return Some(Key::PageDown),
        "backspace" | "bspace" => return Some(Key::Backspace),
        "insert" | "ins" => return Some(Key::Insert),
        "delete" | "del" => return Some(Key::Delete),
        "backtab" => return Some(Key::BackTab),
        _ => {}
    }
    if let Some(num) = n.to_ascii_lowercase().strip_prefix('f') {
        if let Ok(f) = num.parse::<u8>() {
            if (1..=12).contains(&f) {
                return Some(Key::F(f));
            }
        }
    }
    let mut chars = n.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if c.is_control() {
        return None;
    }
    Some(Key::Char(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(parser: &mut InputParser, bytes: &[u8]) -> Vec<InputEvent> {
        parser.feed(bytes, 0x01)
    }

    fn data(events: &[InputEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for event in events {
            if let InputEvent::Data(d) = event {
                out.extend_from_slice(d);
            }
        }
        out
    }

    #[test]
    fn plain_text_passes_through() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"hello");
        assert_eq!(data(&events), b"hello");
    }

    #[test]
    fn prefix_then_char() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x01c");
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::PrefixPending)));
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::PrefixKey(Key::Char('c')))));
    }

    #[test]
    fn prefix_can_be_disabled_for_prompts() {
        let mut p = InputParser::new();
        let events = p.feed_with(b"abc\x01def", 0x01, false);
        assert_eq!(data(&events), b"abc\x01def");
        assert!(!events
            .iter()
            .any(|e| matches!(e, InputEvent::PrefixPending)));
    }

    #[test]
    fn prefix_prefix_sends_literal() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x01\x01");
        assert_eq!(data(&events), vec![0x01]);
    }

    #[test]
    fn arrows_are_split_across_reads() {
        let mut p = InputParser::new();
        assert!(collect(&mut p, b"\x1b").is_empty());
        assert!(collect(&mut p, b"[").is_empty());
        let events = collect(&mut p, b"A");
        assert!(events.iter().any(|e| matches!(e, InputEvent::Key(Key::Up))));
    }

    #[test]
    fn lone_escape_flushes_after_timeout() {
        let mut p = InputParser::new();
        assert!(collect(&mut p, b"\x1b").is_empty());
        let events = p.flush_timeout(Instant::now() + Duration::from_millis(40));
        assert_eq!(data(&events), vec![0x1b]);
    }

    #[test]
    fn prefix_arrow() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x01\x1b[A");
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::PrefixKey(Key::Up))));
    }

    #[test]
    fn bracketed_paste_is_collected() {
        let mut p = InputParser::new();
        collect(&mut p, b"\x1b[200~");
        let events = collect(&mut p, b"hello\x01world");
        assert!(events.is_empty());
        let events = collect(&mut p, b"\x1b[201~");
        assert!(matches!(&events[0], InputEvent::Paste(p) if p == b"hello\x01world"));
    }

    #[test]
    fn prefix_before_paste_is_cancelled() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x01\x1b[200~hi\x1b[201~");
        assert!(events.iter().any(|e| matches!(e, InputEvent::PrefixCancel)));
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::Paste(data) if data == b"hi")));
    }

    #[test]
    fn sgr_mouse_is_parsed() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x1b[<0;12;5M");
        match &events[0] {
            InputEvent::Mouse(m) => {
                assert_eq!(m.x, 11);
                assert_eq!(m.y, 4);
                assert!(m.pressed);
                assert!(m.is_left());
            }
            _ => panic!("expected mouse"),
        }
    }

    #[test]
    fn wheel_is_parsed() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x1b[<64;3;3M");
        match &events[0] {
            InputEvent::Mouse(m) => assert_eq!(m.wheel_direction(), Some(true)),
            _ => panic!("expected mouse"),
        }
    }

    #[test]
    fn modified_arrows_are_raw_key_events() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x1b[1;2A");
        assert_eq!(data(&events), b"");
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::RawKey(bytes) if bytes == b"\x1b[1;2A")));
    }

    #[test]
    fn alt_key_is_a_raw_key_event() {
        let mut p = InputParser::new();
        let events = collect(&mut p, b"\x1bx");
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::RawKey(bytes) if bytes == b"\x1bx")));
    }

    #[test]
    fn prefix_accepts_unicode_keys() {
        let mut p = InputParser::new();
        let events = collect(&mut p, "\x01é".as_bytes());
        assert!(events
            .iter()
            .any(|e| matches!(e, InputEvent::PrefixKey(Key::Char('é')))));
    }

    #[test]
    fn encode_app_cursor() {
        assert_eq!(encode_key(Key::Up, false), b"\x1b[A");
        assert_eq!(encode_key(Key::Up, true), b"\x1bOA");
        assert_eq!(encode_key(Key::F(5), false), b"\x1b[15~");
    }

    #[test]
    fn key_names() {
        assert_eq!(parse_key_name("c"), Some(Key::Char('c')));
        assert_eq!(parse_key_name("pageup"), Some(Key::PageUp));
        assert_eq!(parse_key_name("f12"), Some(Key::F(12)));
        assert_eq!(parse_key_name("nope!"), None);
    }
}
