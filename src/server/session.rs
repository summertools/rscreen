//! The session server: owns PTYs and windows, serves frames to clients, and
//! persists while detached.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use nix::poll::{poll, PollFd, PollFlags};
use serde::Serialize;

use crate::config::{prefix_label, BellMode, Config, Theme};
use crate::layout::{
    border_glyph, Dir, PaneId, PanePlacement, Rect, SolvedLayout, MIN_PANE_H, MIN_PANE_W,
};
use crate::paths;
use crate::protocol::{Caps, ClientMsg, FrameReader, ServerMsg, SessionInfo, PROTOCOL_VERSION};
use crate::pty::ExitStatus;
use crate::render::{draw_hint_strip, draw_toast, encode_frame, CursorView, WindowTab};
use crate::server::copy::{CopyOutcome, CopySnapshot, CopyState};
use crate::server::input::{self, encode_key, InputEvent, InputParser, Key, MouseEvent};
use crate::server::pane::Pane;
use crate::server::ui::{
    self, ConfirmAction, ConfirmState, HelpSection, HelpState, InputLine, Overlay, PaletteAction,
    PaletteCategory, PaletteItem, PaletteKind, PaletteState, PromptAction, PromptState,
    StatusInput, UiState,
};
use crate::server::window::Window;
use crate::term::{attrs, Cell, Style};
use crate::util;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 500;

fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

pub struct ServerOptions {
    pub name: String,
    pub socket: PathBuf,
    pub config: Config,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    NewWindow,
    NextWindow,
    PrevWindow,
    WindowPicker,
    RenameWindow,
    KillWindow,
    SplitRight,
    SplitDown,
    NextPane,
    LastPane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    ZoomPane,
    ClosePane,
    CopyMode,
    PasteBuffer,
    Detach,
    DetachAll,
    Palette,
    Help,
    KillSession,
    ToggleStatus,
}

impl Action {
    fn description(self) -> &'static str {
        match self {
            Action::NewWindow => "New window",
            Action::NextWindow => "Next window",
            Action::PrevWindow => "Previous window",
            Action::WindowPicker => "Switch window…",
            Action::RenameWindow => "Rename window…",
            Action::KillWindow => "Kill window",
            Action::SplitRight => "Split pane right",
            Action::SplitDown => "Split pane down",
            Action::NextPane => "Focus next pane",
            Action::LastPane => "Focus previous pane",
            Action::FocusLeft => "Focus pane left",
            Action::FocusRight => "Focus pane right",
            Action::FocusUp => "Focus pane up",
            Action::FocusDown => "Focus pane down",
            Action::ResizeLeft => "Resize pane left",
            Action::ResizeRight => "Resize pane right",
            Action::ResizeUp => "Resize pane up",
            Action::ResizeDown => "Resize pane down",
            Action::ZoomPane => "Toggle pane zoom",
            Action::ClosePane => "Close pane",
            Action::CopyMode => "Copy mode",
            Action::PasteBuffer => "Paste buffer",
            Action::Detach => "Detach",
            Action::DetachAll => "Detach all clients",
            Action::Palette => "Command palette",
            Action::Help => "Keyboard help",
            Action::KillSession => "Kill session",
            Action::ToggleStatus => "Toggle status bar size",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "new_window" => Action::NewWindow,
            "next_window" => Action::NextWindow,
            "prev_window" => Action::PrevWindow,
            "window_picker" => Action::WindowPicker,
            "rename_window" => Action::RenameWindow,
            "kill_window" => Action::KillWindow,
            "split_right" => Action::SplitRight,
            "split_down" => Action::SplitDown,
            "next_pane" => Action::NextPane,
            "last_pane" => Action::LastPane,
            "focus_left" => Action::FocusLeft,
            "focus_right" => Action::FocusRight,
            "focus_up" => Action::FocusUp,
            "focus_down" => Action::FocusDown,
            "resize_left" => Action::ResizeLeft,
            "resize_right" => Action::ResizeRight,
            "resize_up" => Action::ResizeUp,
            "resize_down" => Action::ResizeDown,
            "zoom_pane" => Action::ZoomPane,
            "close_pane" => Action::ClosePane,
            "copy_mode" => Action::CopyMode,
            "paste_buffer" => Action::PasteBuffer,
            "detach" => Action::Detach,
            "detach_all" => Action::DetachAll,
            "command_palette" => Action::Palette,
            "help" => Action::Help,
            "kill_session" => Action::KillSession,
            "toggle_status" => Action::ToggleStatus,
            _ => return None,
        })
    }
}

struct KeyMap {
    bindings: Vec<(Key, Action)>,
}

impl KeyMap {
    fn new(config: &Config) -> (Self, Vec<String>) {
        let mut bindings = vec![
            (Key::Char('c'), Action::NewWindow),
            (Key::Char('n'), Action::NextWindow),
            (Key::Tab, Action::NextWindow),
            (Key::Char('p'), Action::PrevWindow),
            (Key::BackTab, Action::PrevWindow),
            (Key::Char('w'), Action::WindowPicker),
            (Key::Char('"'), Action::WindowPicker),
            (Key::Char('A'), Action::RenameWindow),
            (Key::Char('k'), Action::KillWindow),
            (Key::Char('|'), Action::SplitRight),
            (Key::Char('-'), Action::SplitDown),
            (Key::Char('S'), Action::SplitDown),
            (Key::Char('o'), Action::NextPane),
            (Key::Char(';'), Action::LastPane),
            (Key::Left, Action::FocusLeft),
            (Key::Right, Action::FocusRight),
            (Key::Up, Action::FocusUp),
            (Key::Down, Action::FocusDown),
            (Key::Char('H'), Action::ResizeLeft),
            (Key::Char('L'), Action::ResizeRight),
            (Key::Char('J'), Action::ResizeDown),
            (Key::Char('K'), Action::ResizeUp),
            (Key::Char('z'), Action::ZoomPane),
            (Key::Char('x'), Action::ClosePane),
            (Key::Char('['), Action::CopyMode),
            (Key::Char(']'), Action::PasteBuffer),
            (Key::Char('d'), Action::Detach),
            (Key::Char('D'), Action::DetachAll),
            (Key::Char(':'), Action::Palette),
            (Key::Char('?'), Action::Help),
            (Key::Char('X'), Action::KillSession),
            (Key::Char('t'), Action::ToggleStatus),
        ];
        let mut warnings = Vec::new();
        for (name, value) in &config.key_overrides {
            let Some(action) = Action::from_name(name) else {
                warnings.push(format!("unknown key action {:?}", name));
                continue;
            };
            if value.is_empty() || value == "none" {
                bindings.retain(|(_, a)| *a != action);
                continue;
            }
            match input::parse_key_name(value) {
                Some(key) => {
                    bindings.retain(|(_, a)| *a != action);
                    bindings.push((key, action));
                }
                None => warnings.push(format!("invalid key {:?} for {}", value, name)),
            }
        }
        (Self { bindings }, warnings)
    }

    fn lookup(&self, key: Key) -> Option<Action> {
        self.bindings
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, a)| *a)
    }

    fn keys_for(&self, action: Action) -> Vec<Key> {
        self.bindings
            .iter()
            .filter(|(_, a)| *a == action)
            .map(|(k, _)| *k)
            .collect()
    }

    fn key_for(&self, action: Action) -> Option<Key> {
        self.keys_for(action).into_iter().next()
    }

    fn short_label(key: Key) -> String {
        match key {
            Key::Char(c) => c.to_string(),
            Key::Tab => "Tab".into(),
            Key::BackTab => "S-Tab".into(),
            Key::Enter => "Enter".into(),
            Key::Esc => "Esc".into(),
            Key::Backspace => "Bksp".into(),
            Key::Up => "↑".into(),
            Key::Down => "↓".into(),
            Key::Left => "←".into(),
            Key::Right => "→".into(),
            Key::Home => "Home".into(),
            Key::End => "End".into(),
            Key::PageUp => "PgUp".into(),
            Key::PageDown => "PgDn".into(),
            Key::Insert => "Ins".into(),
            Key::Delete => "Del".into(),
            Key::F(n) => format!("F{}", n),
        }
    }

    fn label(&self, action: Action, prefix: &str) -> String {
        let keys = self.keys_for(action);
        if keys.is_empty() {
            return "—".to_string();
        }
        keys.iter()
            .map(|key| format!("{} {}", prefix, Self::short_label(*key)))
            .collect::<Vec<_>>()
            .join(" / ")
    }

    fn hint(&self, action: Action, prefix: &str) -> String {
        let keys = self.keys_for(action);
        if keys.is_empty() {
            String::new()
        } else {
            keys.iter()
                .map(|key| format!("{} {}", prefix, Self::short_label(*key)))
                .collect::<Vec<_>>()
                .join(" / ")
        }
    }
}

#[derive(Clone, Default)]
struct PaneHit {
    window_id: u64,
    pane_id: PaneId,
    content: Rect,
    header: Rect,
}

#[derive(Clone, Default)]
struct MouseHits {
    status_y: u16,
    tabs: Vec<(u16, u16, u64)>,
    panes: Vec<PaneHit>,
}

struct Client {
    stream: UnixStream,
    reader: FrameReader,
    out: Vec<u8>,
    out_off: usize,
    size: (u16, u16),
    caps: Caps,
    hello: bool,
    connected_at: Instant,
    last_frame: Option<crate::render::Canvas>,
    last_cursor: Option<CursorView>,
    input: InputParser,
    hits: MouseHits,
}

impl Client {
    fn queue_msg<T: Serialize>(&mut self, msg: &T) -> bool {
        match crate::protocol::encode(msg) {
            Ok(bytes) => self.queue_bytes(&bytes),
            Err(_) => false,
        }
    }

    fn queue_bytes(&mut self, bytes: &[u8]) -> bool {
        if self.out.len().saturating_sub(self.out_off) + bytes.len() > 64 * 1024 * 1024 {
            return false;
        }
        self.out.extend_from_slice(bytes);
        true
    }

    fn flush(&mut self) -> io::Result<()> {
        while self.out_off < self.out.len() {
            match self.stream.write(&self.out[self.out_off..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "client connection closed",
                    ))
                }
                Ok(n) => self.out_off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.out_off >= self.out.len() {
            self.out.clear();
            self.out_off = 0;
        } else if self.out_off > 256 * 1024 {
            self.out.drain(..self.out_off);
            self.out_off = 0;
        }
        Ok(())
    }

    fn has_output(&self) -> bool {
        self.out_off < self.out.len()
    }

    fn final_flush(&mut self) {
        let _ = self.stream.set_nonblocking(false);
        let _ = self
            .stream
            .set_write_timeout(Some(Duration::from_millis(200)));
        if self.out_off < self.out.len() {
            let _ = self.stream.write_all(&self.out[self.out_off..]);
        }
    }
}

struct DragState {
    client_id: u64,
    window_id: u64,
    pane_id: PaneId,
    snapshot: CopySnapshot,
    anchor_row: usize,
    anchor_col: usize,
    moved: bool,
    content: Rect,
}

pub struct Session {
    name: String,
    created: u64,
    config: Config,
    theme: Theme,
    listener: UnixListener,
    socket_path: PathBuf,
    windows: Vec<Window>,
    active_window: usize,
    next_window: u64,
    next_pane: u64,
    clients: HashMap<u64, Client>,
    next_client: u64,
    driver: Option<u64>,
    size: (u16, u16),
    session_cwd: PathBuf,
    ui: UiState,
    clipboard: String,
    prefix: u8,
    prefix_label: String,
    keymap: KeyMap,
    running: bool,
    exit_reason: Option<String>,
    dirty: bool,
    last_clock: String,
    version: String,
    dying: Vec<Pane>,
    drag: Option<DragState>,
    signals: Arc<AtomicBool>,
}

pub fn run_server(options: ServerOptions) -> Result<()> {
    // The server is intentionally immune to the terminal that launched it.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let terminate = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, terminate.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, terminate.clone());

    if options.socket.exists() {
        paths::remove_stale_socket(&options.socket);
    }
    let listener = UnixListener::bind(&options.socket).with_context(|| {
        format!(
            "binding session socket {} (is another rscreen session using this name?)",
            options.socket.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&options.socket, std::fs::Permissions::from_mode(0o600));
    }
    listener.set_nonblocking(true)?;

    let (keymap, warnings) = KeyMap::new(&options.config);
    for warning in warnings {
        log(&format!("config: {}", warning));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let prefix_label = prefix_label(options.config.prefix);
    let mut session = Session {
        name: options.name.clone(),
        created: now,
        theme: options.config.theme,
        config: options.config,
        listener,
        socket_path: options.socket,
        windows: Vec::new(),
        active_window: 0,
        next_window: 1,
        next_pane: 1,
        clients: HashMap::new(),
        next_client: 1,
        driver: None,
        size: (80, 24),
        session_cwd: options.cwd,
        ui: UiState::default(),
        clipboard: String::new(),
        prefix: 0,
        prefix_label,
        keymap,
        running: true,
        exit_reason: None,
        dirty: true,
        last_clock: clock_string(),
        version: format!("rscreen {}", VERSION),
        dying: Vec::new(),
        drag: None,
        signals: terminate,
    };
    session.prefix = session.config.prefix;
    if let Err(err) = session.startup_window() {
        log(&format!("startup failed: {:#}", err));
        let _ = std::fs::remove_file(&session.socket_path);
        return Err(err).context("creating the initial shell window");
    }
    log(&format!(
        "session {:?} started (pid {}, socket {})",
        session.name,
        std::process::id(),
        session.socket_path.display()
    ));
    let result = session.run();
    session.cleanup();
    result
}

impl Session {
    fn startup_window(&mut self) -> Result<()> {
        let area = self.content_area(self.size.0, self.size.1);
        let pane = self.spawn_pane(
            self.session_cwd.clone(),
            area.w.max(1),
            area.h.max(1),
            "shell",
        )?;
        let id = self.next_window;
        self.next_window += 1;
        self.windows.push(Window::new(id, pane));
        self.active_window = 0;
        Ok(())
    }

    fn spawn_pane(&mut self, cwd: PathBuf, cols: u16, rows: u16, title: &str) -> Result<Pane> {
        let id = self.next_pane;
        self.next_pane += 1;
        let program = self.config.shell_path();
        let args = self.config.shell_args.clone();
        let login_shell = args.is_empty();
        let scrollback = self.config.scrollback;
        let mut pane = Pane::spawn(
            id,
            &program,
            &args,
            if cwd.as_os_str().is_empty() {
                None
            } else {
                Some(cwd.as_path())
            },
            cols.max(1),
            rows.max(1),
            &self.name,
            login_shell,
            scrollback,
        )?;
        if pane.title.is_empty() {
            pane.title = title.to_string();
        }
        Ok(pane)
    }

    fn content_area(&self, cols: u16, rows: u16) -> Rect {
        let status = if rows <= 1 {
            0
        } else {
            self.config.status_rows.min(rows - 1)
        };
        let h = rows.saturating_sub(status).max(1);
        Rect::new(0, 0, cols.max(1), h)
    }

    fn active_window(&self) -> Option<&Window> {
        self.windows.get(self.active_window)
    }

    fn active_window_mut(&mut self) -> Option<&mut Window> {
        self.windows.get_mut(self.active_window)
    }

    fn focused(&self) -> Option<PaneId> {
        self.active_window().map(|w| w.active)
    }

    fn find_pane(&self, id: PaneId) -> Option<(&Window, &Pane)> {
        for window in &self.windows {
            if let Some(pane) = window.pane(id) {
                return Some((window, pane));
            }
        }
        None
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        for window in &mut self.windows {
            if let Some(pane) = window.pane_mut(id) {
                return Some(pane);
            }
        }
        None
    }

    fn window_index_of_pane(&self, id: PaneId) -> Option<usize> {
        self.windows.iter().position(|w| w.pane(id).is_some())
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.active_window().and_then(|w| w.active_pane())
    }

    fn focused_pane_mut(&mut self) -> Option<&mut Pane> {
        let win = self.active_window_mut()?;
        let id = win.active;
        win.pane_mut(id)
    }

    fn resize_all(&mut self) {
        let area = self.content_area(self.size.0, self.size.1);
        for window in &mut self.windows {
            window.apply_layout(area);
        }
        self.dirty = true;
    }

    fn resize_window(&mut self, index: usize) {
        let area = self.content_area(self.size.0, self.size.1);
        if let Some(window) = self.windows.get_mut(index) {
            window.apply_layout(area);
        }
    }

    fn notify_focus_change(&mut self, old: Option<PaneId>, new: Option<PaneId>) {
        if old == new {
            return;
        }
        if let Some(id) = old {
            if let Some(pane) = self.pane_mut(id) {
                if pane.focus_reporting() {
                    pane.write_input(b"\x1b[O");
                }
            }
        }
        if let Some(id) = new {
            if let Some(pane) = self.pane_mut(id) {
                if pane.focus_reporting() {
                    pane.write_input(b"\x1b[I");
                }
            }
        }
    }

    fn focus_pane(&mut self, window_id: u64, pane_id: PaneId) -> bool {
        let Some(index) = self.windows.iter().position(|w| w.id == window_id) else {
            return false;
        };
        if self.windows[index].pane(pane_id).is_none() {
            return false;
        }
        let old = self.focused();
        if let Some(copy) = &self.ui.copy {
            if copy.pane_id != pane_id {
                self.ui.copy = None;
            }
        }
        if self.active_window != index {
            self.switch_window(index);
        }
        let changed = self.windows[index].set_active(pane_id);
        if changed {
            if let Some(pane) = self.windows[index].pane_mut(pane_id) {
                pane.bell = false;
            }
            self.clear_window_bell(index);
            let new = self.focused();
            self.notify_focus_change(old, new);
            self.dirty = true;
        }
        changed
    }

    fn clear_window_bell(&mut self, index: usize) {
        if let Some(window) = self.windows.get_mut(index) {
            for pane in window.panes.values_mut() {
                pane.bell = false;
            }
        }
    }

    fn switch_window(&mut self, index: usize) {
        if index >= self.windows.len() || index == self.active_window {
            return;
        }
        let old = self.focused();
        self.drag = None;
        self.active_window = index;
        self.clear_window_bell(index);
        self.resize_window(index);
        if let Some(copy) = &self.ui.copy {
            if copy.window_id != self.windows[index].id {
                self.ui.copy = None;
            }
        }
        let new = self.focused();
        self.notify_focus_change(old, new);
        self.dirty = true;
    }

    fn select_window_id(&mut self, id: u64) {
        if let Some(index) = self.windows.iter().position(|w| w.id == id) {
            self.switch_window(index);
        }
    }

    fn select_window_number(&mut self, number: usize) {
        if number >= 1 && number <= self.windows.len() {
            self.switch_window(number - 1);
        } else {
            self.ui
                .toast_error(&self.theme, format!("No window {}", number));
        }
        self.dirty = true;
    }

    fn next_window(&mut self, forward: bool) {
        let len = self.windows.len();
        if len == 0 {
            return;
        }
        let next = if forward {
            (self.active_window + 1) % len
        } else {
            (self.active_window + len - 1) % len
        };
        self.switch_window(next);
    }

    fn create_window(&mut self) {
        let area = self.content_area(self.size.0, self.size.1);
        let cwd = self
            .focused_pane()
            .and_then(|p| p.cwd.clone())
            .or_else(|| {
                self.focused_pane()
                    .and_then(|p| crate::pty::process_cwd(p.pid()))
            })
            .unwrap_or_else(|| self.session_cwd.clone());
        match self.spawn_pane(cwd, area.w, area.h, "shell") {
            Ok(pane) => {
                let id = self.next_window;
                self.next_window += 1;
                self.windows.push(Window::new(id, pane));
                self.active_window = self.windows.len() - 1;
                self.ui.copy = None;
                self.resize_window(self.active_window);
                self.ui.toast(&self.theme, "New window");
                self.dirty = true;
            }
            Err(err) => {
                self.ui
                    .toast_error(&self.theme, format!("Cannot open window: {}", err));
                self.dirty = true;
            }
        }
    }

    fn split_pane(&mut self, dir: Dir) {
        if self.windows.is_empty() {
            return;
        }
        self.ui.copy = None;
        let area = self.content_area(self.size.0, self.size.1);
        let window_index = self.active_window;
        let solved = self.windows[window_index]
            .layout
            .solve(area, self.windows[window_index].pane_count() > 1);
        let target_content = solved
            .panes
            .iter()
            .find(|p| p.pane == self.windows[window_index].active)
            .map(|p| p.content)
            .unwrap_or(area);
        let fits = match dir {
            Dir::SideBySide => target_content.w > 2 * MIN_PANE_W && target_content.h > MIN_PANE_H,
            Dir::Stacked => {
                target_content.h > 2 * (MIN_PANE_H + 2) && target_content.w >= MIN_PANE_W
            }
        };
        if !fits {
            self.ui
                .toast_error(&self.theme, "Not enough space to split this pane");
            self.dirty = true;
            return;
        }
        let (target, target_pid, cwd) = {
            let window = &self.windows[window_index];
            let target = window.active;
            let Some(pane) = window.pane(target) else {
                return;
            };
            (target, pane.pid(), pane.cwd.clone())
        };
        let cwd = cwd
            .or_else(|| crate::pty::process_cwd(target_pid))
            .unwrap_or_else(|| self.session_cwd.clone());
        let content = solved
            .panes
            .iter()
            .find(|p| p.pane == target)
            .map(|p| p.content)
            .unwrap_or(area);
        let cols = (content.w / 2).max(1);
        let rows = (content.h / 2).max(1);
        match self.spawn_pane(cwd, cols, rows, "shell") {
            Ok(pane) => {
                let new_id = pane.id;
                self.windows[window_index].zoom = false;
                if let Err(err) = self.windows[window_index].split(pane, dir) {
                    self.ui.toast_error(&self.theme, err);
                    self.dirty = true;
                    return;
                }
                self.resize_window(window_index);
                self.notify_focus_change(Some(target), Some(new_id));
                self.ui.toast(&self.theme, "Pane created");
                self.dirty = true;
            }
            Err(err) => {
                self.ui
                    .toast_error(&self.theme, format!("Cannot split: {}", err));
                self.dirty = true;
            }
        }
    }

    fn close_pane_now(&mut self, window_id: u64, pane_id: PaneId) {
        let Some(index) = self.windows.iter().position(|w| w.id == window_id) else {
            return;
        };
        if self.windows[index].pane(pane_id).is_none() {
            return;
        }
        let old = self.focused();
        if let Some(copy) = &self.ui.copy {
            if copy.pane_id == pane_id {
                self.ui.copy = None;
            }
        }
        if self
            .drag
            .as_ref()
            .map(|d| d.pane_id == pane_id)
            .unwrap_or(false)
        {
            self.drag = None;
        }
        let mut removed = self.windows[index].remove_pane(pane_id);
        if let Some(pane) = removed.as_mut() {
            pane.terminate();
        }
        if let Some(pane) = removed {
            self.dying.push(pane);
        }
        if self.windows[index].is_empty() {
            self.remove_window_at(index, "window closed");
            return;
        }
        self.resize_window(index);
        let new = self.focused();
        self.notify_focus_change(old, new);
        self.ui.toast(&self.theme, "Pane closed");
        self.dirty = true;
    }

    fn remove_window_at(&mut self, index: usize, reason: &str) {
        if index >= self.windows.len() {
            return;
        }
        let old = self.focused();
        if let Some(copy) = &self.ui.copy {
            if Some(copy.window_id) == self.windows.get(index).map(|w| w.id) {
                self.ui.copy = None;
            }
        }
        let window = self.windows.remove(index);
        for (_, mut pane) in window.panes {
            pane.terminate();
            self.dying.push(pane);
        }
        if self.windows.is_empty() {
            self.active_window = 0;
            self.exit_reason = Some(reason.to_string());
            self.running = false;
            return;
        }
        if index < self.active_window {
            self.active_window -= 1;
        } else if self.active_window >= self.windows.len() {
            self.active_window = self.windows.len() - 1;
        }
        self.resize_window(self.active_window);
        let new = self.focused();
        self.notify_focus_change(old, new);
        self.ui.toast(&self.theme, reason);
        self.dirty = true;
    }

    fn close_window_now(&mut self, window_id: u64) {
        if let Some(index) = self.windows.iter().position(|w| w.id == window_id) {
            self.remove_window_at(index, "Window closed");
        }
    }

    fn handle_pane_exit(&mut self, pane_id: PaneId, status: ExitStatus) {
        let Some(index) = self.window_index_of_pane(pane_id) else {
            return;
        };
        let old = self.focused();
        if let Some(copy) = &self.ui.copy {
            if copy.pane_id == pane_id {
                self.ui.copy = None;
            }
        }
        self.windows[index].remove_pane(pane_id);
        if self.windows[index].is_empty() {
            let reason = if self.windows.len() == 1 {
                format!("session ended ({})", status.describe())
            } else {
                "window ended".to_string()
            };
            self.remove_window_at(index, &reason);
            if !self.running {
                self.exit_reason = Some(reason);
            }
            return;
        }
        self.resize_window(index);
        let new = self.focused();
        self.notify_focus_change(old, new);
        self.ui
            .toast_info(&self.theme, format!("A pane {}", status.describe()));
        self.dirty = true;
    }

    fn toggle_zoom(&mut self) {
        self.ui.copy = None;
        let Some(window) = self.active_window_mut() else {
            return;
        };
        if window.pane_count() <= 1 {
            self.ui
                .toast_info(&self.theme, "Only one pane in this window");
            self.dirty = true;
            return;
        }
        window.zoom = !window.zoom;
        let zoomed = window.zoom;
        let index = self.active_window;
        self.resize_window(index);
        self.ui.toast_info(
            &self.theme,
            if zoomed {
                "Pane zoomed"
            } else {
                "Pane unzoomed"
            },
        );
        self.dirty = true;
    }

    fn adjust_resize(&mut self, dir: Dir, delta: f32) {
        let Some(window) = self.active_window_mut() else {
            return;
        };
        let pane = window.active;
        if window.layout.resize(pane, dir, delta) {
            let index = self.active_window;
            self.resize_window(index);
            self.dirty = true;
        } else {
            self.ui
                .toast_info(&self.theme, "No split in that direction");
            self.dirty = true;
        }
    }

    fn focus_direction(&mut self, movement: Dir, forward: bool) {
        let (window_index, area) = (
            self.active_window,
            self.content_area(self.size.0, self.size.1),
        );
        let Some(window) = self.windows.get(window_index) else {
            return;
        };
        let framed = window.pane_count() > 1;
        let solved = window.layout.solve(area, framed);
        let Some(cur) = solved.panes.iter().find(|p| p.pane == window.active) else {
            return;
        };
        let cur_rect = cur.content;
        let cur_cx = cur_rect.x as i32 + cur_rect.w as i32 / 2;
        let cur_cy = cur_rect.y as i32 + cur_rect.h as i32 / 2;
        let mut best: Option<(i32, PaneId)> = None;
        for p in &solved.panes {
            if p.pane == window.active {
                continue;
            }
            let cx = p.content.x as i32 + p.content.w as i32 / 2;
            let cy = p.content.y as i32 + p.content.h as i32 / 2;
            let (dx, dy) = (cx - cur_cx, cy - cur_cy);
            let ok = match (movement, forward) {
                (Dir::SideBySide, true) => p.content.x >= cur_rect.right(),
                (Dir::SideBySide, false) => p.content.right() <= cur_rect.x,
                (Dir::Stacked, true) => p.content.y >= cur_rect.bottom(),
                (Dir::Stacked, false) => p.content.bottom() <= cur_rect.y,
            };
            if !ok {
                continue;
            }
            let score = match movement {
                Dir::SideBySide => dx.abs() * 2 + dy.abs(),
                Dir::Stacked => dy.abs() * 2 + dx.abs(),
            };
            if best.map(|(s, _)| score < s).unwrap_or(true) {
                best = Some((score, p.pane));
            }
        }
        if let Some((_, pane_id)) = best {
            let window_id = self.windows[window_index].id;
            self.focus_pane(window_id, pane_id);
        }
    }

    fn focus_next_pane(&mut self, forward: bool) {
        self.ui.copy = None;
        let Some(window) = self.active_window_mut() else {
            return;
        };
        if window.pane_count() <= 1 {
            return;
        }
        let old = window.active;
        window.focus_next(forward);
        let new = window.active;
        self.notify_focus_change(Some(old), Some(new));
        self.dirty = true;
    }

    fn focus_previous_pane(&mut self) {
        self.ui.copy = None;
        let (old, new) = {
            let Some(window) = self.active_window_mut() else {
                return;
            };
            let old = window.active;
            window.focus_previous();
            (old, window.active)
        };
        self.notify_focus_change(Some(old), Some(new));
        self.dirty = true;
    }

    fn open_palette(&mut self, kind: PaletteKind) {
        let items = self.build_palette_items(kind);
        let mut palette = PaletteState::new(kind, items);
        if kind == PaletteKind::Windows {
            palette.selected = self
                .active_window
                .min(palette.items.len().saturating_sub(1));
            palette.scroll = palette.selected;
        }
        self.ui.overlay = Overlay::Palette(palette);
        self.dirty = true;
    }

    fn build_palette_items(&self, kind: PaletteKind) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        if matches!(kind, PaletteKind::All | PaletteKind::Windows) {
            for (i, window) in self.windows.iter().enumerate() {
                let panes = window.pane_count();
                let marker = if i == self.active_window { "●" } else { " " };
                items.push(PaletteItem {
                    label: format!("{} {}  {}", marker, i + 1, window.display_title()),
                    hint: format!("{} pane{}", panes, if panes == 1 { "" } else { "s" }),
                    action: PaletteAction::SelectWindow(window.id),
                    category: PaletteCategory::Window,
                });
            }
        }
        if kind == PaletteKind::All {
            let actions = [
                Action::NewWindow,
                Action::SplitRight,
                Action::SplitDown,
                Action::NextPane,
                Action::LastPane,
                Action::ZoomPane,
                Action::ClosePane,
                Action::NextWindow,
                Action::PrevWindow,
                Action::WindowPicker,
                Action::RenameWindow,
                Action::CopyMode,
                Action::PasteBuffer,
                Action::Help,
                Action::ToggleStatus,
                Action::Detach,
                Action::KillWindow,
                Action::KillSession,
            ];
            for action in actions {
                let hint = self.keymap.hint(action, &self.prefix_label);
                items.push(PaletteItem {
                    label: action.description().to_string(),
                    hint,
                    action: palette_action_for(action),
                    category: PaletteCategory::Command,
                });
            }
        }
        items
    }

    fn open_help(&mut self) {
        self.ui.overlay = Overlay::Help(self.build_help());
        self.dirty = true;
    }

    fn build_help(&self) -> HelpState {
        let p = &self.prefix_label;
        let key = |action: Action| self.keymap.label(action, p);
        let sections = vec![
            HelpSection {
                title: "SESSION",
                rows: vec![
                    (key(Action::Detach), "Detach from this session".into()),
                    (key(Action::Palette), "Command palette".into()),
                    (key(Action::Help), "This help screen".into()),
                    (key(Action::KillSession), "Kill the whole session".into()),
                ],
            },
            HelpSection {
                title: "WINDOWS",
                rows: vec![
                    (key(Action::NewWindow), "Create a new window".into()),
                    (key(Action::NextWindow), "Next window".into()),
                    (key(Action::PrevWindow), "Previous window".into()),
                    (
                        format!("{} 1..9 / 0", p),
                        "Jump to a window number (0 = 10)".into(),
                    ),
                    (key(Action::WindowPicker), "Window picker".into()),
                    (key(Action::RenameWindow), "Rename this window".into()),
                    (key(Action::KillWindow), "Kill this window".into()),
                ],
            },
            HelpSection {
                title: "PANES",
                rows: vec![
                    (key(Action::SplitRight), "Split right".into()),
                    (key(Action::SplitDown), "Split down".into()),
                    (key(Action::NextPane), "Focus the next pane".into()),
                    (
                        format!(
                            "{} {}/{}/{}/{}",
                            p,
                            KeyMap::short_label(
                                self.keymap.key_for(Action::FocusLeft).unwrap_or(Key::Left)
                            ),
                            KeyMap::short_label(
                                self.keymap.key_for(Action::FocusUp).unwrap_or(Key::Up)
                            ),
                            KeyMap::short_label(
                                self.keymap.key_for(Action::FocusDown).unwrap_or(Key::Down)
                            ),
                            KeyMap::short_label(
                                self.keymap
                                    .key_for(Action::FocusRight)
                                    .unwrap_or(Key::Right)
                            ),
                        ),
                        "Focus a pane by direction".into(),
                    ),
                    (key(Action::ZoomPane), "Zoom / unzoom the pane".into()),
                    (key(Action::ClosePane), "Close the focused pane".into()),
                    (
                        format!(
                            "{} {}/{}/{}/{}",
                            p,
                            KeyMap::short_label(
                                self.keymap
                                    .key_for(Action::ResizeLeft)
                                    .unwrap_or(Key::Char('H'))
                            ),
                            KeyMap::short_label(
                                self.keymap
                                    .key_for(Action::ResizeDown)
                                    .unwrap_or(Key::Char('J'))
                            ),
                            KeyMap::short_label(
                                self.keymap
                                    .key_for(Action::ResizeUp)
                                    .unwrap_or(Key::Char('K'))
                            ),
                            KeyMap::short_label(
                                self.keymap
                                    .key_for(Action::ResizeRight)
                                    .unwrap_or(Key::Char('L'))
                            ),
                        ),
                        "Resize the focused pane".into(),
                    ),
                ],
            },
            HelpSection {
                title: "COPY MODE",
                rows: vec![
                    (format!("{} [", p), "Enter copy mode".into()),
                    ("hjkl / arrows".into(), "Move the cursor".into()),
                    ("space / v".into(), "Start or clear a selection".into()),
                    ("/ ?".into(), "Search forward / backward".into()),
                    ("n / N".into(), "Next / previous match".into()),
                    ("y / enter".into(), "Copy and leave copy mode".into()),
                    (format!("{} ]", p), "Paste the copy buffer".into()),
                    ("q / esc".into(), "Leave copy mode".into()),
                ],
            },
            HelpSection {
                title: "MOUSE",
                rows: vec![
                    ("wheel".into(), "Scroll the focused pane".into()),
                    ("drag".into(), "Select and copy text".into()),
                    ("click".into(), "Focus a pane or window tab".into()),
                ],
            },
        ];
        HelpState::new(sections)
    }

    fn prefix_hints(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for (action, desc) in [
            (Action::NewWindow, "new"),
            (Action::WindowPicker, "windows"),
            (Action::SplitRight, "split →"),
            (Action::SplitDown, "split ↓"),
            (Action::NextPane, "pane"),
            (Action::CopyMode, "copy"),
            (Action::Help, "help"),
            (Action::Detach, "detach"),
        ] {
            if let Some(key) = self.keymap.key_for(action) {
                out.push((KeyMap::short_label(key), desc.to_string()));
            }
        }
        out
    }

    fn status_hints(&self) -> Vec<(String, String)> {
        if self.ui.copy.is_some() {
            return vec![
                ("space".into(), "select".into()),
                ("y".into(), "copy".into()),
                ("/".into(), "search".into()),
                ("q".into(), "quit".into()),
            ];
        }
        let mut out = Vec::new();
        for (action, desc) in [
            (Action::NewWindow, "new"),
            (Action::WindowPicker, "windows"),
            (Action::Help, "help"),
            (Action::Detach, "detach"),
        ] {
            if let Some(key) = self.keymap.key_for(action) {
                out.push((
                    format!("{} {}", self.prefix_label, KeyMap::short_label(key)),
                    desc.to_string(),
                ));
            }
        }
        out
    }
}

fn palette_action_for(action: Action) -> PaletteAction {
    match action {
        Action::NewWindow => PaletteAction::NewWindow,
        Action::SplitRight => PaletteAction::SplitRight,
        Action::SplitDown => PaletteAction::SplitDown,
        Action::NextPane => PaletteAction::NextPane,
        Action::LastPane => PaletteAction::PrevPane,
        Action::ZoomPane => PaletteAction::ZoomPane,
        Action::ClosePane => PaletteAction::ClosePane,
        Action::NextWindow => PaletteAction::NextWindow,
        Action::PrevWindow => PaletteAction::PrevWindow,
        Action::WindowPicker => PaletteAction::WindowPicker,
        Action::RenameWindow => PaletteAction::RenameWindow,
        Action::CopyMode => PaletteAction::CopyMode,
        Action::PasteBuffer => PaletteAction::PasteBuffer,
        Action::Help => PaletteAction::Help,
        Action::ToggleStatus => PaletteAction::ToggleStatusRows,
        Action::Detach => PaletteAction::Detach,
        Action::KillWindow => PaletteAction::KillWindow,
        Action::KillSession => PaletteAction::KillSession,
        _ => PaletteAction::NewWindow,
    }
}

fn clock_string() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

fn log(message: &str) {
    eprintln!("[{}] {}", chrono::Local::now().format("%H:%M:%S"), message);
}

// ---------------------------------------------------------------------------
// Client protocol and session lifecycle
// ---------------------------------------------------------------------------

impl Session {
    fn handle_client_message(&mut self, id: u64, msg: ClientMsg) {
        match msg {
            ClientMsg::Hello {
                version,
                cols,
                rows,
                caps,
            } => {
                if version != PROTOCOL_VERSION {
                    self.send_error_and_close(
                        id,
                        format!(
                            "protocol version mismatch: server {}, client {}",
                            PROTOCOL_VERSION, version
                        ),
                    );
                    return;
                }
                let first = self.clients.values().filter(|c| c.hello).count() == 0;
                let size = clamp_size(cols, rows);
                let name = self.name.clone();
                let status_rows = if size.1 <= 1 {
                    0
                } else {
                    self.config.status_rows.min(size.1 - 1)
                };
                let prefix = self.prefix_label.clone();
                if let Some(client) = self.clients.get_mut(&id) {
                    client.hello = true;
                    client.size = size;
                    client.caps = caps;
                    client.last_frame = None;
                    client.last_cursor = None;
                    client.queue_msg(&ServerMsg::Welcome {
                        name,
                        version: PROTOCOL_VERSION,
                        cols: size.0,
                        rows: size.1,
                        status_rows,
                        prefix_label: prefix,
                    });
                }
                if first || self.driver.is_none() {
                    self.driver = Some(id);
                    self.size = size;
                    self.resize_all();
                    let focus = self.focused();
                    self.notify_focus_change(None, focus);
                }
                self.dirty = true;
            }
            ClientMsg::Input(data) => {
                let Some(client) = self.clients.get_mut(&id) else {
                    return;
                };
                if !client.hello {
                    return;
                }
                let allow_prefix = !matches!(self.ui.overlay, Overlay::Prompt(_));
                let events = client.input.feed_with(&data, self.prefix, allow_prefix);
                self.handle_input_events(id, events);
            }
            ClientMsg::Resize { cols, rows } => {
                let Some(client) = self.clients.get_mut(&id) else {
                    return;
                };
                client.size = clamp_size(cols, rows);
                let resize_session = self.driver == Some(id) || self.driver.is_none();
                if resize_session {
                    self.driver = Some(id);
                    self.size = client.size;
                    self.resize_all();
                } else {
                    self.dirty = true;
                }
            }
            ClientMsg::Detach { all } => {
                self.detach_client(id, all);
            }
            ClientMsg::Query => {
                self.send_info(id);
                if let Some(mut client) = self.clients.remove(&id) {
                    client.final_flush();
                }
                self.after_client_removed(id);
            }
            ClientMsg::KillSession => {
                self.exit_reason = Some("session killed".into());
                self.running = false;
            }
        }
    }

    fn send_info(&mut self, id: u64) {
        let panes: usize = self.windows.iter().map(|w| w.pane_count()).sum();
        let clients = self.clients.values().filter(|c| c.hello).count();
        let bell = self
            .windows
            .iter()
            .any(|w| w.panes.values().any(|p| p.bell));
        let info = SessionInfo {
            name: self.name.clone(),
            pid: std::process::id() as i32,
            created: self.created,
            windows: self.windows.len(),
            panes,
            clients,
            cols: self.size.0,
            rows: self.size.1,
            bell,
        };
        if let Some(client) = self.clients.get_mut(&id) {
            client.queue_msg(&ServerMsg::Info(info));
        }
    }

    fn send_error_and_close(&mut self, id: u64, message: String) {
        if let Some(mut client) = self.clients.remove(&id) {
            client.queue_msg(&ServerMsg::Error(message));
            client.final_flush();
        }
        self.after_client_removed(id);
    }

    fn detach_client(&mut self, id: u64, all: bool) {
        let ids: Vec<u64> = if all {
            self.clients.keys().copied().collect()
        } else {
            vec![id]
        };
        let old_focus = self.focused();
        if let Some(drag) = &self.drag {
            if ids.contains(&drag.client_id) {
                self.drag = None;
            }
        }
        for id in ids {
            if let Some(mut client) = self.clients.remove(&id) {
                client.queue_msg(&ServerMsg::Exit {
                    reason: "detached".into(),
                    code: 0,
                });
                client.final_flush();
            }
        }
        if self.clients.is_empty() {
            self.driver = None;
            self.notify_focus_change(old_focus, None);
            self.ui.copy = None;
            self.drag = None;
        } else if self
            .driver
            .map(|d| !self.clients.contains_key(&d))
            .unwrap_or(false)
        {
            self.choose_new_driver();
        }
        self.dirty = true;
    }

    fn after_client_removed(&mut self, id: u64) {
        if self
            .drag
            .as_ref()
            .map(|d| d.client_id == id)
            .unwrap_or(false)
        {
            self.drag = None;
        }
        if self.driver == Some(id) {
            self.choose_new_driver();
        }
        if self.clients.values().all(|c| !c.hello) {
            self.driver = None;
            let focus = self.focused();
            self.notify_focus_change(focus, None);
            self.ui.overlay = Overlay::None;
            self.ui.copy = None;
            self.drag = None;
        }
        self.dirty = true;
    }

    fn remove_client(&mut self, id: u64) {
        self.clients.remove(&id);
        self.after_client_removed(id);
    }

    fn choose_new_driver(&mut self) {
        let next = self
            .clients
            .iter()
            .find(|(_, c)| c.hello)
            .map(|(id, c)| (*id, c.size));
        if let Some((id, size)) = next {
            self.driver = Some(id);
            if size != self.size {
                self.size = size;
                self.resize_all();
            }
        } else {
            self.driver = None;
        }
    }

    fn send_exits(&mut self, reason: &str) {
        let ids: Vec<u64> = self.clients.keys().copied().collect();
        for id in ids {
            if let Some(mut client) = self.clients.remove(&id) {
                client.queue_msg(&ServerMsg::Exit {
                    reason: reason.to_string(),
                    code: 0,
                });
                client.final_flush();
            }
        }
        self.driver = None;
    }

    fn cleanup(&mut self) {
        for window in &mut self.windows {
            for (_, mut pane) in window.panes.drain() {
                pane.force_kill();
            }
        }
        for mut pane in self.dying.drain(..) {
            pane.force_kill();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        log(&format!("session {:?} stopped", self.name));
    }
}

// ---------------------------------------------------------------------------
// Input dispatch
// ---------------------------------------------------------------------------

impl Session {
    fn set_driver(&mut self, client_id: u64) {
        if self.driver == Some(client_id) {
            return;
        }
        let Some(client) = self.clients.get(&client_id) else {
            return;
        };
        if !client.hello {
            return;
        }
        self.driver = Some(client_id);
        if client.size != self.size {
            self.size = client.size;
            self.resize_all();
        }
    }

    fn handle_input_events(&mut self, client_id: u64, events: Vec<InputEvent>) {
        for event in events {
            if !self.clients.contains_key(&client_id) {
                break;
            }
            match event {
                InputEvent::PrefixPending => {
                    self.dirty = true;
                }
                InputEvent::PrefixCancel => {
                    self.dirty = true;
                }
                InputEvent::PrefixUnknown(key) => {
                    self.ui
                        .toast_error(&self.theme, format!("Unknown prefix key: {}", key));
                    self.dirty = true;
                }
                InputEvent::PrefixKey(key) => {
                    self.handle_prefix_key(client_id, key);
                }
                InputEvent::Key(key) => {
                    self.handle_key_event(client_id, key);
                }
                InputEvent::Data(data) => {
                    self.handle_data_event(client_id, &data);
                }
                InputEvent::Mouse(event) => {
                    self.handle_mouse(client_id, event);
                }
                InputEvent::Paste(data) => {
                    self.handle_paste(client_id, data);
                }
            }
        }
    }

    fn handle_prefix_key(&mut self, client_id: u64, key: Key) {
        if let Key::Char(c) = key {
            if c.is_ascii_digit() {
                if let Some(n) = c.to_digit(10) {
                    self.ui.overlay = Overlay::None;
                    let number = if n == 0 { 10 } else { n as usize };
                    self.select_window_number(number);
                    return;
                }
            }
        }
        match self.keymap.lookup(key) {
            Some(action) => self.execute_action(client_id, action),
            None => {
                self.ui.toast_error(
                    &self.theme,
                    format!("Unknown prefix key: {}", KeyMap::short_label(key)),
                );
                self.dirty = true;
            }
        }
    }

    fn handle_key_event(&mut self, client_id: u64, key: Key) {
        if self.ui.overlay_open() {
            self.handle_overlay_key(client_id, key);
            return;
        }
        if self.ui.copy.is_some() {
            self.handle_copy_key(key);
            return;
        }
        self.set_driver(client_id);
        if let Some(pane) = self.focused_pane_mut() {
            let bytes = encode_key(key, pane.app_cursor());
            pane.write_input(&bytes);
        }
    }

    fn handle_data_event(&mut self, client_id: u64, data: &[u8]) {
        if self.ui.overlay_open() {
            self.handle_overlay_data(client_id, data);
            return;
        }
        if self.ui.copy.is_some() {
            self.handle_copy_data(data);
            return;
        }
        self.set_driver(client_id);
        if let Some(pane) = self.focused_pane_mut() {
            pane.write_input(data);
        }
    }

    fn handle_paste(&mut self, client_id: u64, data: Vec<u8>) {
        if self.ui.overlay_open() {
            self.handle_overlay_paste(&data);
            return;
        }
        if self.ui.copy.is_some() {
            self.ui
                .toast_info(&self.theme, "Leave copy mode before pasting");
            self.dirty = true;
            return;
        }
        self.set_driver(client_id);
        let bracketed = self
            .focused_pane()
            .map(|p| p.bracketed_paste())
            .unwrap_or(false);
        let mut out = Vec::with_capacity(data.len() + 12);
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(&data);
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        if let Some(pane) = self.focused_pane_mut() {
            if !pane.write_input(&out) {
                self.ui
                    .toast_error(&self.theme, "Paste is too large for this pane");
                self.dirty = true;
            }
        }
    }

    fn handle_overlay_key(&mut self, client_id: u64, key: Key) {
        let mut overlay = std::mem::replace(&mut self.ui.overlay, Overlay::None);
        let mut close = false;
        let mut deferred_palette: Option<PaletteAction> = None;
        let mut deferred_confirm: Option<ConfirmAction> = None;
        let mut deferred_prompt = false;

        match &mut overlay {
            Overlay::None => close = true,
            Overlay::Palette(palette) => match key {
                Key::Esc => close = true,
                Key::Up | Key::BackTab => palette.move_selection(-1),
                Key::Down | Key::Tab => palette.move_selection(1),
                Key::PageUp => palette.move_selection(-6),
                Key::PageDown => palette.move_selection(6),
                Key::Home => palette.selected = 0,
                Key::End => {
                    palette.selected = palette.filtered().len().saturating_sub(1);
                }
                Key::Enter => deferred_palette = palette.selected_action(),
                Key::Backspace => {
                    palette.query.backspace();
                    palette.query_changed();
                }
                Key::Delete => {
                    palette.query.delete();
                    palette.query_changed();
                }
                Key::Left => palette.query.left(),
                Key::Right => palette.query.right(),
                Key::Char(c) if !c.is_control() => {
                    palette.query.insert(c);
                    palette.query_changed();
                }
                _ => {}
            },
            Overlay::Help(help) => match key {
                Key::Esc | Key::Char('q') => close = true,
                Key::Down | Key::Char('j') => help.scroll += 1,
                Key::Up | Key::Char('k') => help.scroll = help.scroll.saturating_sub(1),
                Key::PageDown => {
                    help.scroll = (help.scroll + 8).min(help.line_count().saturating_sub(1))
                }
                Key::PageUp => help.scroll = help.scroll.saturating_sub(8),
                Key::Home | Key::Char('g') => help.scroll = 0,
                Key::End | Key::Char('G') => help.scroll = help.line_count().saturating_sub(1),
                _ => {}
            },
            Overlay::Confirm(confirm) => match key {
                Key::Char('y') | Key::Char('Y') | Key::Enter => {
                    deferred_confirm = Some(confirm.action.clone());
                    close = true;
                }
                Key::Char('n') | Key::Char('N') | Key::Esc => close = true,
                _ => {}
            },
            Overlay::Prompt(prompt) => match key {
                Key::Esc => close = true,
                Key::Enter => deferred_prompt = true,
                Key::Backspace => prompt.line.backspace(),
                Key::Delete => prompt.line.delete(),
                Key::Left => prompt.line.left(),
                Key::Right => prompt.line.right(),
                Key::Home => prompt.line.home(),
                Key::End => prompt.line.end(),
                Key::Char(c) if !c.is_control() => prompt.line.insert(c),
                _ => {}
            },
        }

        if deferred_prompt {
            // `confirm_prompt` consumes the overlay; put it back first.
            self.ui.overlay = overlay;
            self.confirm_prompt();
        } else if close {
            self.ui.overlay = Overlay::None;
        } else {
            self.ui.overlay = overlay;
        }
        self.dirty = true;

        if let Some(action) = deferred_palette {
            self.ui.overlay = Overlay::None;
            self.execute_palette_action(client_id, action);
        }
        if let Some(action) = deferred_confirm {
            self.ui.overlay = Overlay::None;
            self.execute_confirm_action(action);
        }
    }

    fn handle_overlay_data(&mut self, client_id: u64, data: &[u8]) {
        let mut text: Vec<u8> = Vec::new();
        for &byte in data {
            if byte < 0x20 || byte == 0x7f {
                self.flush_overlay_text(&text, client_id);
                text.clear();
                match byte {
                    0x1b | 0x03 => self.handle_overlay_key(client_id, Key::Esc),
                    0x0d | 0x0a => self.handle_overlay_key(client_id, Key::Enter),
                    0x09 => self.handle_overlay_key(client_id, Key::Tab),
                    0x7f | 0x08 => self.handle_overlay_key(client_id, Key::Backspace),
                    0x15 => self.overlay_line_edit(|line| line.kill_to_start()),
                    0x17 => self.overlay_line_edit(kill_word_before),
                    0x01 => self.overlay_line_edit(|line| line.home()),
                    0x05 => self.overlay_line_edit(|line| line.end()),
                    0x0b => self.overlay_line_edit(|line| line.kill_to_end()),
                    0x04 => {
                        if let Overlay::Help(help) = &mut self.ui.overlay {
                            help.scroll += 6;
                            self.dirty = true;
                        }
                    }
                    _ => {}
                }
            } else {
                text.push(byte);
            }
        }
        self.flush_overlay_text(&text, client_id);
    }

    fn flush_overlay_text(&mut self, bytes: &[u8], client_id: u64) {
        if bytes.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        match &mut self.ui.overlay {
            Overlay::Palette(palette) => {
                palette.query.insert_str(&text);
                palette.query_changed();
                self.dirty = true;
            }
            Overlay::Prompt(prompt) => {
                for c in text.chars() {
                    if !c.is_control() {
                        prompt.line.insert(c);
                    }
                }
                self.dirty = true;
            }
            Overlay::Help(_) | Overlay::Confirm(_) => {
                let chars: Vec<char> = text.chars().collect();
                for c in chars {
                    if c.is_control() {
                        continue;
                    }
                    self.handle_overlay_key(client_id, Key::Char(c));
                }
            }
            Overlay::None => {}
        }
    }

    fn overlay_line_edit<F: FnOnce(&mut InputLine)>(&mut self, f: F) {
        match &mut self.ui.overlay {
            Overlay::Palette(palette) => {
                f(&mut palette.query);
                palette.query_changed();
                self.dirty = true;
            }
            Overlay::Prompt(prompt) => {
                f(&mut prompt.line);
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn handle_overlay_paste(&mut self, data: &[u8]) {
        let text = paste_text(data);
        if text.is_empty() {
            return;
        }
        match &mut self.ui.overlay {
            Overlay::Palette(palette) => {
                palette.query.insert_str(&text);
                palette.query_changed();
            }
            Overlay::Prompt(prompt) => prompt.line.insert_str(&text),
            _ => {}
        }
        self.dirty = true;
    }

    fn confirm_prompt(&mut self) {
        let Overlay::Prompt(prompt) = std::mem::replace(&mut self.ui.overlay, Overlay::None) else {
            return;
        };
        match prompt.action {
            PromptAction::RenameWindow { window_id } => {
                if let Some(window) = self.windows.iter_mut().find(|w| w.id == window_id) {
                    let title = prompt.line.text().trim().to_string();
                    if title.is_empty() {
                        window.title_locked = false;
                        window.title = window.display_title();
                        self.ui.toast(&self.theme, "Window title reset");
                    } else {
                        window.title = util::truncate_display(&title, 80);
                        window.title_locked = true;
                        self.ui.toast(&self.theme, "Window renamed");
                    }
                }
            }
            PromptAction::CopySearch { forward } => {
                let height = self.copy_content_height();
                let query = prompt.line.text();
                if let Some(copy) = self.ui.copy.as_mut() {
                    copy.set_search(&query, forward, height);
                    if copy.matches.is_empty() && !query.is_empty() {
                        self.ui.toast_info(&self.theme, "No matches");
                    }
                }
            }
        }
        self.ui.overlay = Overlay::None;
        self.dirty = true;
    }

    fn execute_palette_action(&mut self, client_id: u64, action: PaletteAction) {
        match action {
            PaletteAction::SelectWindow(id) => self.select_window_id(id),
            PaletteAction::NewWindow => self.create_window(),
            PaletteAction::SplitRight => self.split_pane(Dir::SideBySide),
            PaletteAction::SplitDown => self.split_pane(Dir::Stacked),
            PaletteAction::ClosePane => self.request_close_pane(),
            PaletteAction::NextPane => self.focus_next_pane(true),
            PaletteAction::PrevPane => self.focus_previous_pane(),
            PaletteAction::ZoomPane => self.toggle_zoom(),
            PaletteAction::RenameWindow => self.request_rename(),
            PaletteAction::KillWindow => self.request_kill_window(),
            PaletteAction::NextWindow => self.next_window(true),
            PaletteAction::PrevWindow => self.next_window(false),
            PaletteAction::WindowPicker => self.open_palette(PaletteKind::Windows),
            PaletteAction::CopyMode => self.enter_copy_mode(),
            PaletteAction::PasteBuffer => self.paste_buffer(),
            PaletteAction::Help => self.open_help(),
            PaletteAction::Detach => self.detach_client(client_id, false),
            PaletteAction::KillSession => self.request_kill_session(),
            PaletteAction::ToggleStatusRows => self.toggle_status_rows(),
            PaletteAction::SendPrefix => {
                let prefix = self.prefix;
                if let Some(pane) = self.focused_pane_mut() {
                    pane.write_input(&[prefix]);
                }
            }
        }
    }

    fn execute_action(&mut self, client_id: u64, action: Action) {
        // Prefix commands are explicit; close any modal that would otherwise
        // show stale state.
        self.ui.overlay = Overlay::None;
        match action {
            Action::NewWindow => self.create_window(),
            Action::NextWindow => self.next_window(true),
            Action::PrevWindow => self.next_window(false),
            Action::WindowPicker => self.open_palette(PaletteKind::Windows),
            Action::RenameWindow => self.request_rename(),
            Action::KillWindow => self.request_kill_window(),
            Action::SplitRight => self.split_pane(Dir::SideBySide),
            Action::SplitDown => self.split_pane(Dir::Stacked),
            Action::NextPane => self.focus_next_pane(true),
            Action::LastPane => self.focus_previous_pane(),
            Action::FocusLeft => self.focus_direction(Dir::SideBySide, false),
            Action::FocusRight => self.focus_direction(Dir::SideBySide, true),
            Action::FocusUp => self.focus_direction(Dir::Stacked, false),
            Action::FocusDown => self.focus_direction(Dir::Stacked, true),
            Action::ResizeLeft => self.adjust_resize(Dir::SideBySide, -0.04),
            Action::ResizeRight => self.adjust_resize(Dir::SideBySide, 0.04),
            Action::ResizeUp => self.adjust_resize(Dir::Stacked, -0.04),
            Action::ResizeDown => self.adjust_resize(Dir::Stacked, 0.04),
            Action::ZoomPane => self.toggle_zoom(),
            Action::ClosePane => self.request_close_pane(),
            Action::CopyMode => self.enter_copy_mode(),
            Action::PasteBuffer => self.paste_buffer(),
            Action::Detach => self.detach_client(client_id, false),
            Action::DetachAll => self.detach_client(client_id, true),
            Action::Palette => self.open_palette(PaletteKind::All),
            Action::Help => self.open_help(),
            Action::KillSession => self.request_kill_session(),
            Action::ToggleStatus => self.toggle_status_rows(),
        }
        self.dirty = true;
    }

    fn request_rename(&mut self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let window_id = window.id;
        let current = window.display_title();
        self.ui.overlay = Overlay::Prompt(PromptState {
            title: "Rename window".into(),
            line: InputLine::new(&current),
            placeholder: "Window title".into(),
            action: PromptAction::RenameWindow { window_id },
        });
        self.dirty = true;
    }

    fn request_close_pane(&mut self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let window_id = window.id;
        let window_number = self.active_window + 1;
        let pane_id = window.active;
        let title = window
            .active_pane()
            .map(|p| p.title.clone())
            .unwrap_or_else(|| "pane".into());
        let panes = window.pane_count();
        if !self.config.confirm_kill {
            self.close_pane_now(window_id, pane_id);
            return;
        }
        let last_window = self.windows.len() == 1;
        let (message, detail) = if panes == 1 {
            (
                format!("Close window {} (\"{}\")?", window_number, title),
                if last_window {
                    "It is the last window; the session will end.".to_string()
                } else {
                    "The shell in this window will be terminated.".to_string()
                },
            )
        } else {
            (
                format!("Close pane \"{}\"?", title),
                "The process in this pane will be terminated.".to_string(),
            )
        };
        self.ui.overlay = Overlay::Confirm(ConfirmState {
            message,
            detail,
            action: ConfirmAction::ClosePane {
                window_id,
                pane: pane_id,
            },
            danger: last_window,
        });
        self.dirty = true;
    }

    fn request_kill_window(&mut self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let window_id = window.id;
        let number = self.active_window + 1;
        let title = window.display_title();
        if !self.config.confirm_kill {
            self.close_window_now(window_id);
            return;
        }
        let last = self.windows.len() == 1;
        self.ui.overlay = Overlay::Confirm(ConfirmState {
            message: format!("Kill window {} (\"{}\")?", number, title),
            detail: if last {
                "This is the last window; the session will end.".into()
            } else {
                "Every process in this window will be terminated.".into()
            },
            action: ConfirmAction::CloseWindow { window_id },
            danger: last,
        });
        self.dirty = true;
    }

    fn request_kill_session(&mut self) {
        if !self.config.confirm_kill {
            self.exit_reason = Some("session killed".into());
            self.running = false;
            return;
        }
        let panes: usize = self.windows.iter().map(|w| w.pane_count()).sum();
        self.ui.overlay = Overlay::Confirm(ConfirmState {
            message: format!("Kill session \"{}\"?", self.name),
            detail: format!(
                "{} window(s), {} pane(s) and all their processes will be terminated.",
                self.windows.len(),
                panes
            ),
            action: ConfirmAction::KillSession,
            danger: true,
        });
        self.dirty = true;
    }

    fn execute_confirm_action(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::ClosePane { window_id, pane } => self.close_pane_now(window_id, pane),
            ConfirmAction::CloseWindow { window_id } => self.close_window_now(window_id),
            ConfirmAction::KillSession => {
                self.exit_reason = Some("session killed".into());
                self.running = false;
            }
        }
        self.dirty = true;
    }

    fn toggle_status_rows(&mut self) {
        self.config.status_rows = if self.config.status_rows >= 2 { 1 } else { 2 };
        self.resize_all();
        self.ui.toast_info(
            &self.theme,
            format!("Status bar: {} row(s)", self.config.status_rows),
        );
        self.dirty = true;
    }

    fn enter_copy_mode(&mut self) {
        let Some(window) = self.active_window() else {
            return;
        };
        let window_id = window.id;
        let pane_id = window.active;
        if let Some(copy) = &self.ui.copy {
            if copy.pane_id == pane_id {
                self.ui.copy = None;
                self.ui.toast_info(&self.theme, "Copy mode off");
                self.dirty = true;
                return;
            }
        }
        let Some((_, pane)) = self.find_pane(pane_id) else {
            return;
        };
        let snapshot = pane.snapshot();
        self.ui.copy = Some(CopyState::new(window_id, pane_id, snapshot));
        self.ui.overlay = Overlay::None;
        self.dirty = true;
    }

    fn paste_buffer(&mut self) {
        if self.clipboard.is_empty() {
            self.ui.toast_info(&self.theme, "Clipboard is empty");
            self.dirty = true;
            return;
        }
        let text = self.clipboard.clone();
        let Some(pane) = self.focused_pane() else {
            return;
        };
        let bracketed = pane.bracketed_paste();
        let mut payload = Vec::with_capacity(text.len() + 12);
        if bracketed {
            payload.extend_from_slice(b"\x1b[200~");
        }
        payload.extend_from_slice(text.as_bytes());
        if bracketed {
            payload.extend_from_slice(b"\x1b[201~");
        }
        if let Some(pane) = self.focused_pane_mut() {
            if !pane.write_input(&payload) {
                self.ui
                    .toast_error(&self.theme, "Buffer is too large for this pane");
            }
        }
        self.ui.toast(&self.theme, "Pasted buffer");
        self.dirty = true;
    }

    fn copy_content_height(&self) -> u16 {
        let Some(copy) = &self.ui.copy else {
            return 20;
        };
        let Some(window) = self.windows.iter().find(|w| w.id == copy.window_id) else {
            return 20;
        };
        let area = self.content_area(self.size.0, self.size.1);
        let zoom = window.zoom && window.pane_count() > 1;
        if zoom {
            return area.h;
        }
        let solved = window.layout.solve(area, window.pane_count() > 1);
        solved
            .panes
            .iter()
            .find(|p| p.pane == copy.pane_id)
            .map(|p| p.content.h)
            .unwrap_or(20)
    }

    fn handle_copy_key(&mut self, key: Key) {
        let height = self.copy_content_height();
        let outcome = {
            let Some(copy) = self.ui.copy.as_mut() else {
                return;
            };
            copy.handle_key(key, height)
        };
        self.handle_copy_outcome(outcome);
    }

    fn handle_copy_data(&mut self, data: &[u8]) {
        let height = self.copy_content_height();
        let mut keys: Vec<Key> = Vec::new();
        if data == [0x1b] {
            keys.push(Key::Esc);
        } else {
            for byte in data {
                match byte {
                    b'\r' | b'\n' => keys.push(Key::Enter),
                    b'\t' => keys.push(Key::Tab),
                    0x7f | 0x08 => keys.push(Key::Backspace),
                    0x03 => keys.push(Key::Char('q')),
                    0x02 => keys.push(Key::Char('\u{2}')),
                    0x04 => keys.push(Key::Char('\u{4}')),
                    0x06 => keys.push(Key::Char('\u{6}')),
                    0x15 => keys.push(Key::Char('\u{15}')),
                    b if *b >= 0x20 => {}
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(data);
            for c in text.chars() {
                if !c.is_control() {
                    keys.push(Key::Char(c));
                }
            }
        }
        for key in keys {
            let outcome = {
                let Some(copy) = self.ui.copy.as_mut() else {
                    return;
                };
                copy.handle_key(key, height)
            };
            self.handle_copy_outcome(outcome);
            if self.ui.copy.is_none() {
                break;
            }
        }
    }

    fn handle_copy_outcome(&mut self, outcome: CopyOutcome) {
        match outcome {
            CopyOutcome::Continue => self.dirty = true,
            CopyOutcome::Exit => {
                self.ui.copy = None;
                self.dirty = true;
            }
            CopyOutcome::Copy(text) => {
                self.ui.copy = None;
                self.finish_copy(text);
            }
            CopyOutcome::SearchPrompt { forward } => {
                self.ui.overlay = Overlay::Prompt(PromptState {
                    title: if forward {
                        "Search forward"
                    } else {
                        "Search backward"
                    }
                    .into(),
                    line: InputLine::new(""),
                    placeholder: "Type a search term…".into(),
                    action: PromptAction::CopySearch { forward },
                });
                self.dirty = true;
            }
            CopyOutcome::Toast(message) => {
                self.ui.toast_info(&self.theme, message);
                self.dirty = true;
            }
        }
    }

    fn finish_copy(&mut self, text: String) {
        let lines = text.lines().count().max(1);
        self.set_clipboard(text.clone());
        self.ui.toast(
            &self.theme,
            format!("Copied {} line{}", lines, if lines == 1 { "" } else { "s" }),
        );
        self.dirty = true;
    }

    fn set_clipboard(&mut self, text: String) {
        self.clipboard = text.clone();
        self.broadcast_clipboard(&text);
    }

    fn broadcast_clipboard(&mut self, text: &str) {
        if text.len() > 512 * 1024 {
            self.ui.toast_info(
                &self.theme,
                "Clipboard is too large to sync to the terminal",
            );
            return;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let mut seq = Vec::with_capacity(encoded.len() + 16);
        seq.extend_from_slice(b"\x1b]52;c;");
        seq.extend_from_slice(encoded.as_bytes());
        seq.extend_from_slice(b"\x1b\\");
        for client in self.clients.values_mut() {
            client.queue_msg(&ServerMsg::Raw(seq.clone()));
        }
    }
}

fn kill_word_before(line: &mut InputLine) {
    while line.cursor > 0
        && line
            .chars
            .get(line.cursor - 1)
            .map(|c| c.is_whitespace())
            .unwrap_or(false)
    {
        line.backspace();
    }
    while line.cursor > 0
        && line
            .chars
            .get(line.cursor - 1)
            .map(|c| !c.is_whitespace())
            .unwrap_or(false)
    {
        line.backspace();
    }
}

fn paste_text(data: &[u8]) -> String {
    String::from_utf8_lossy(data)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

impl Session {
    fn handle_mouse(&mut self, client_id: u64, event: MouseEvent) {
        if self.ui.overlay_open() {
            if let Some(up) = event.wheel_direction() {
                match &mut self.ui.overlay {
                    Overlay::Palette(palette) => palette.move_selection(if up { -1 } else { 1 }),
                    Overlay::Help(help) => {
                        if up {
                            help.scroll = help.scroll.saturating_sub(3);
                        } else {
                            help.scroll = (help.scroll + 3).min(help.max_scroll());
                        }
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            return;
        }
        if self
            .drag
            .as_ref()
            .map(|d| d.client_id == client_id)
            .unwrap_or(false)
        {
            if !event.pressed {
                self.finish_drag(client_id);
                return;
            }
            if event.motion {
                self.update_drag(client_id, event);
                return;
            }
        }
        let Some(hits) = self.clients.get(&client_id).map(|c| c.hits.clone()) else {
            return;
        };
        if hits.status_y > 0 && event.y >= hits.status_y {
            if event.pressed && event.y == hits.status_y {
                for (x, w, window_id) in &hits.tabs {
                    if event.x >= *x && event.x < x.saturating_add(*w) {
                        self.select_window_id(*window_id);
                        return;
                    }
                }
            }
            return;
        }
        let Some(hit) = hits
            .panes
            .iter()
            .find(|p| p.content.contains(event.x, event.y) || p.header.contains(event.x, event.y))
            .cloned()
        else {
            return;
        };
        self.set_driver(client_id);
        if hit.header.contains(event.x, event.y) {
            self.focus_pane(hit.window_id, hit.pane_id);
            return;
        }
        let app_mouse = self
            .find_pane(hit.pane_id)
            .map(|(_, p)| {
                (
                    p.mouse_mode(),
                    p.terminal.screen.modes.mouse_sgr,
                    p.terminal.screen.modes.mouse_utf8,
                )
            })
            .unwrap_or((crate::term::MouseMode::Off, false, false));
        self.focus_pane(hit.window_id, hit.pane_id);
        if app_mouse.0 != crate::term::MouseMode::Off {
            if mouse_wanted(app_mouse.0, event) {
                self.forward_mouse(hit.pane_id, event, hit.content, app_mouse.1, app_mouse.2);
            }
            return;
        }
        if event.is_wheel() {
            if self.alternate_scroll(hit.pane_id) {
                self.send_alternate_scroll(hit.pane_id, event);
            } else {
                self.wheel_scroll(Some(hit.window_id), hit.pane_id, hit.content, event);
            }
            return;
        }
        if !event.is_left() || !self.config.mouse_select {
            return;
        }
        if event.pressed && !event.motion {
            self.begin_drag(client_id, hit, event);
        } else if event.motion && event.pressed {
            self.update_drag(client_id, event);
        } else if !event.pressed {
            self.finish_drag(client_id);
        }
    }

    fn alternate_scroll(&self, pane_id: PaneId) -> bool {
        self.find_pane(pane_id)
            .map(|(_, p)| {
                p.terminal.screen.alt_active
                    && p.alternate_scroll()
                    && p.mouse_mode() == crate::term::MouseMode::Off
            })
            .unwrap_or(false)
    }

    fn send_alternate_scroll(&mut self, pane_id: PaneId, event: MouseEvent) {
        let Some(up) = event.wheel_direction() else {
            return;
        };
        let (app_cursor, count) = match self.find_pane(pane_id) {
            Some((_, p)) => (p.app_cursor(), self.config.wheel_step),
            None => return,
        };
        let key = if up { Key::Up } else { Key::Down };
        let bytes = encode_key(key, app_cursor);
        if let Some(pane) = self.pane_mut(pane_id) {
            for _ in 0..count {
                pane.write_input(&bytes);
            }
        }
    }

    fn forward_mouse(
        &mut self,
        pane_id: PaneId,
        event: MouseEvent,
        content: Rect,
        sgr: bool,
        utf8: bool,
    ) {
        if event.x < content.x || event.y < content.y {
            return;
        }
        let x = event.x - content.x + 1;
        let y = event.y - content.y + 1;
        let local = MouseEvent { x, y, ..event };
        let bytes = encode_mouse(local, sgr, utf8);
        if let Some(pane) = self.pane_mut(pane_id) {
            pane.write_input(&bytes);
        }
    }

    fn wheel_scroll(
        &mut self,
        window_id: Option<u64>,
        pane_id: PaneId,
        content: Rect,
        event: MouseEvent,
    ) {
        let Some(up) = event.wheel_direction() else {
            return;
        };
        let step = self.config.wheel_step as isize;
        if let Some(copy) = &self.ui.copy {
            if copy.pane_id == pane_id {
                let height = content.h.max(1);
                let delta = if up { step } else { -step };
                if let Some(copy) = self.ui.copy.as_mut() {
                    copy.scroll_view(delta, height);
                }
                if !up
                    && self
                        .ui
                        .copy
                        .as_ref()
                        .map(|c| c.scroll == 0)
                        .unwrap_or(false)
                {
                    self.ui.copy = None;
                }
                self.dirty = true;
                return;
            }
        }
        let Some(window_id) = window_id else {
            return;
        };
        let Some((_, pane)) = self.find_pane(pane_id) else {
            return;
        };
        let snapshot = pane.snapshot();
        let mut copy = CopyState::new(window_id, pane_id, snapshot);
        copy.scroll_view(if up { step } else { -step }, content.h.max(1));
        self.ui.copy = Some(copy);
        self.dirty = true;
    }

    fn begin_drag(&mut self, client_id: u64, hit: PaneHit, event: MouseEvent) {
        if let Some(copy) = &self.ui.copy {
            if copy.pane_id != hit.pane_id {
                self.ui.copy = None;
                self.dirty = true;
            }
        }
        let Some((_, pane)) = self.find_pane(hit.pane_id) else {
            return;
        };
        let snapshot = pane.snapshot();
        let (row, col) = if let Some(copy) = &self.ui.copy {
            if copy.pane_id == hit.pane_id {
                point_to_copy(copy, hit.content, event)
            } else {
                point_to_snapshot(&snapshot, hit.content, event)
            }
        } else {
            point_to_snapshot(&snapshot, hit.content, event)
        };
        if let Some(copy) = self.ui.copy.as_mut() {
            if copy.pane_id == hit.pane_id {
                copy.cursor_row = row.min(copy.total_rows().saturating_sub(1));
                copy.cursor_col = col;
                copy.anchor = Some((copy.cursor_row, copy.cursor_col));
                copy.selecting = true;
                copy.ensure_visible(hit.content.h.max(1));
            }
        }
        self.drag = Some(DragState {
            client_id,
            window_id: hit.window_id,
            pane_id: hit.pane_id,
            snapshot,
            anchor_row: row,
            anchor_col: col,
            moved: self.ui.copy.is_some(),
            content: hit.content,
        });
    }

    fn update_drag(&mut self, client_id: u64, event: MouseEvent) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if drag.client_id != client_id {
            return;
        }
        if !drag.moved {
            drag.moved = true;
            let mut copy = CopyState::new(drag.window_id, drag.pane_id, drag.snapshot.clone());
            copy.cursor_row = drag.anchor_row.min(copy.total_rows().saturating_sub(1));
            copy.cursor_col = drag.anchor_col;
            copy.anchor = Some((copy.cursor_row, copy.cursor_col));
            copy.selecting = true;
            self.ui.copy = Some(copy);
            self.ui.overlay = Overlay::None;
        }
        let content = drag.content;
        let Some(copy) = self.ui.copy.as_mut() else {
            return;
        };
        if copy.pane_id != drag.pane_id {
            return;
        }
        let (row, col) = point_to_copy(copy, content, event);
        copy.cursor_row = row.min(copy.total_rows().saturating_sub(1));
        copy.cursor_col = col;
        copy.ensure_visible(content.h.max(1));
        self.dirty = true;
    }

    fn finish_drag(&mut self, client_id: u64) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.client_id != client_id {
            return;
        }
        if drag.moved {
            let text = self
                .ui
                .copy
                .as_ref()
                .map(|c| c.selection_text())
                .unwrap_or_default();
            self.ui.copy = None;
            if !text.is_empty() {
                self.finish_copy(text);
            }
        }
        self.dirty = true;
    }
}

fn mouse_wanted(mode: crate::term::MouseMode, event: MouseEvent) -> bool {
    use crate::term::MouseMode;
    if event.is_wheel() {
        return true;
    }
    match mode {
        MouseMode::Off => false,
        MouseMode::X10 => event.pressed,
        MouseMode::Normal => !event.motion,
        MouseMode::Button => !event.motion || event.button & 32 != 0,
        MouseMode::Any => true,
    }
}

fn encode_mouse(event: MouseEvent, sgr: bool, utf8: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if sgr {
        let final_byte = if event.pressed { 'M' } else { 'm' };
        out.extend_from_slice(
            format!(
                "\x1b[<{};{};{}{}",
                event.button,
                event.x.max(1),
                event.y.max(1),
                final_byte
            )
            .as_bytes(),
        );
        return out;
    }
    let button = if event.pressed {
        event.button
    } else {
        3 | (event.button & !3)
    };
    out.extend_from_slice(b"\x1b[M");
    out.push(32u8.saturating_add(button));
    push_x10_coord(&mut out, event.x, utf8);
    push_x10_coord(&mut out, event.y, utf8);
    out
}

fn push_x10_coord(out: &mut Vec<u8>, value: u16, utf8: bool) {
    let value = value.max(1).saturating_add(32);
    if utf8 && value > 127 {
        if let Some(c) = char::from_u32(value as u32) {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            return;
        }
    }
    out.push(value.min(255) as u8);
}

fn point_to_copy(copy: &CopyState, content: Rect, event: MouseEvent) -> (usize, usize) {
    let (top, _bottom) = copy.viewport(content.h.max(1));
    let dy = event.y.saturating_sub(content.y) as usize;
    let dx = event.x.saturating_sub(content.x) as usize;
    let row = (top + dy).min(copy.total_rows().saturating_sub(1));
    let col = dx.min(copy.cols().saturating_sub(1));
    (row, col)
}

fn point_to_snapshot(snapshot: &CopySnapshot, content: Rect, event: MouseEvent) -> (usize, usize) {
    let total = snapshot.rows.len();
    let top = total.saturating_sub(content.h.max(1) as usize);
    let dy = event.y.saturating_sub(content.y) as usize;
    let dx = event.x.saturating_sub(content.x) as usize;
    let row = (top + dy).min(total.saturating_sub(1));
    let col = dx.min(snapshot.cols.saturating_sub(1));
    (row, col)
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum FdKind {
    Listener,
    Client(u64),
    Pane(PaneId),
}

impl Session {
    fn run(&mut self) -> Result<()> {
        while self.running {
            if self.signals.load(Ordering::Relaxed) {
                self.exit_reason = Some("session terminated".into());
                self.running = false;
                break;
            }
            // Render before reaping children: a command that prints and exits
            // must not lose its final screen update.
            if self.dirty {
                self.render_clients();
            }
            self.flush_clients();
            self.tick();
            if !self.running {
                break;
            }

            let timeout: u16 = if self.clients.is_empty() { 200 } else { 40 };
            let mut fds: Vec<PollFd> = Vec::new();
            let mut kinds: Vec<FdKind> = Vec::new();
            let listener_fd = self.listener.as_raw_fd();
            fds.push(PollFd::new(
                unsafe { BorrowedFd::borrow_raw(listener_fd) },
                PollFlags::POLLIN,
            ));
            kinds.push(FdKind::Listener);
            for (id, client) in &self.clients {
                let mut flags = PollFlags::POLLIN;
                if client.has_output() {
                    flags |= PollFlags::POLLOUT;
                }
                let fd = client.stream.as_raw_fd();
                fds.push(PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, flags));
                kinds.push(FdKind::Client(*id));
            }
            for window in &self.windows {
                for (pane_id, pane) in &window.panes {
                    let mut flags = PollFlags::empty();
                    if !pane.pty.eof {
                        flags |= PollFlags::POLLIN;
                    }
                    if pane.has_pending_input() {
                        flags |= PollFlags::POLLOUT;
                    }
                    if flags.is_empty() {
                        continue;
                    }
                    let fd = pane.master_fd();
                    fds.push(PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, flags));
                    kinds.push(FdKind::Pane(*pane_id));
                }
            }

            match poll(&mut fds, timeout) {
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => continue,
                Err(err) => return Err(err.into()),
            }

            // Accept first: new clients should not wait behind pane output.
            if fds
                .first()
                .and_then(|fd| fd.revents())
                .map(|r| r.contains(PollFlags::POLLIN))
                .unwrap_or(false)
            {
                self.accept_clients();
            }

            let mut actions: Vec<(FdKind, PollFlags)> = Vec::new();
            for (fd, kind) in fds.iter().zip(kinds.iter()).skip(1) {
                if let Some(revents) = fd.revents() {
                    if !revents.is_empty() {
                        actions.push((*kind, revents));
                    }
                }
            }
            for (kind, revents) in actions {
                if !self.running {
                    break;
                }
                match kind {
                    FdKind::Listener => {}
                    FdKind::Client(id) => {
                        if !self.clients.contains_key(&id) {
                            continue;
                        }
                        if revents.intersects(
                            PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL,
                        ) {
                            self.read_client(id);
                            self.flush_client(id);
                            continue;
                        }
                        if revents.contains(PollFlags::POLLIN) {
                            self.read_client(id);
                        }
                        if revents.contains(PollFlags::POLLOUT) {
                            self.flush_client(id);
                        }
                    }
                    FdKind::Pane(id) => {
                        if self.find_pane(id).is_none() {
                            continue;
                        }
                        if revents.contains(PollFlags::POLLIN)
                            || revents.intersects(
                                PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL,
                            )
                        {
                            self.read_pane(id);
                        }
                        if revents.contains(PollFlags::POLLOUT) {
                            self.flush_pane(id);
                        }
                    }
                }
            }
        }
        let reason = self
            .exit_reason
            .clone()
            .unwrap_or_else(|| "session ended".into());
        self.send_exits(&reason);
        Ok(())
    }

    fn tick(&mut self) {
        if self.signals.load(Ordering::Relaxed) {
            self.exit_reason = Some("session terminated".into());
            self.running = false;
            return;
        }
        let now = Instant::now();
        let client_ids: Vec<u64> = self.clients.keys().copied().collect();
        for id in client_ids {
            let (timeout, events) = match self.clients.get_mut(&id) {
                Some(client) => {
                    if !client.hello
                        && now.duration_since(client.connected_at) > Duration::from_secs(5)
                    {
                        (true, Vec::new())
                    } else {
                        (false, client.input.flush_timeout(now))
                    }
                }
                None => continue,
            };
            if timeout {
                self.remove_client(id);
                continue;
            }
            if !events.is_empty() {
                self.handle_input_events(id, events);
            }
        }

        let pane_ids: Vec<PaneId> = self
            .windows
            .iter()
            .flat_map(|w| w.panes.keys().copied())
            .collect();
        for id in pane_ids {
            let status = self.pane_mut(id).and_then(|pane| pane.try_wait());
            if let Some(status) = status {
                // The child may already be gone while its final bytes are
                // still buffered in the PTY. Drain and show them before the
                // pane is removed.
                self.read_pane(id);
                if self.dirty {
                    self.render_clients();
                    self.flush_clients();
                }
                self.handle_pane_exit(id, status);
                if !self.running {
                    break;
                }
            }
        }

        let mut keep = Vec::new();
        for mut pane in self.dying.drain(..) {
            pane.check_kill_deadline();
            if pane.try_wait().is_none() {
                keep.push(pane);
            }
        }
        self.dying = keep;

        let had_toast = self.ui.toast.is_some();
        self.ui.tick();
        if had_toast && self.ui.toast.is_none() {
            self.dirty = true;
        }
        if self.config.clock {
            let clock = clock_string();
            if clock != self.last_clock {
                self.last_clock = clock;
                self.dirty = true;
            }
        }

        if let Some(copy) = &self.ui.copy {
            let Some(window) = self.windows.iter().find(|w| w.id == copy.window_id) else {
                self.ui.copy = None;
                self.dirty = true;
                return;
            };
            if window.pane(copy.pane_id).is_none() {
                self.ui.copy = None;
                self.dirty = true;
            }
        }
        if self.windows.is_empty() && self.running {
            self.exit_reason = Some("session ended".into());
            self.running = false;
        }
    }

    fn accept_clients(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let id = self.next_client;
                    self.next_client += 1;
                    let client = Client {
                        stream,
                        reader: FrameReader::new(),
                        out: Vec::new(),
                        out_off: 0,
                        size: (80, 24),
                        caps: Caps::default(),
                        hello: false,
                        connected_at: Instant::now(),
                        last_frame: None,
                        last_cursor: None,
                        input: InputParser::new(),
                        hits: MouseHits::default(),
                    };
                    self.clients.insert(id, client);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    log(&format!("accept failed: {}", err));
                    break;
                }
            }
        }
    }

    fn read_client(&mut self, id: u64) {
        let mut messages = Vec::new();
        let mut protocol_error: Option<String> = None;
        let mut eof = false;
        {
            let Some(client) = self.clients.get_mut(&id) else {
                return;
            };
            let mut buf = [0u8; 64 * 1024];
            for _ in 0..16 {
                match client.stream.read(&mut buf) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        if let Err(err) = client.reader.push(&buf[..n]) {
                            protocol_error = Some(err.to_string());
                            break;
                        }
                        loop {
                            match client.reader.next_message::<ClientMsg>() {
                                Ok(Some(msg)) => messages.push(msg),
                                Ok(None) => break,
                                Err(err) => {
                                    protocol_error = Some(err.to_string());
                                    break;
                                }
                            }
                        }
                        if protocol_error.is_some() {
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        log(&format!("client {} read error: {}", id, err));
                        eof = true;
                        break;
                    }
                }
            }
        }
        if let Some(err) = protocol_error {
            self.send_error_and_close(id, err);
            return;
        }
        for msg in messages {
            if !self.clients.contains_key(&id) {
                break;
            }
            self.handle_client_message(id, msg);
            if !self.running {
                break;
            }
        }
        if eof && self.clients.contains_key(&id) {
            self.remove_client(id);
        }
    }

    fn flush_client(&mut self, id: u64) {
        let result = match self.clients.get_mut(&id) {
            Some(client) => client.flush(),
            None => return,
        };
        if let Err(err) = result {
            if err.kind() != io::ErrorKind::WouldBlock {
                self.remove_client(id);
            }
        }
    }

    fn flush_clients(&mut self) {
        let ids: Vec<u64> = self.clients.keys().copied().collect();
        for id in ids {
            self.flush_client(id);
        }
    }

    fn read_pane(&mut self, id: PaneId) {
        let mut events = Vec::new();
        let mut saw_data = false;
        let mut eof = false;
        {
            let Some(pane) = self.pane_mut(id) else {
                return;
            };
            let mut buf = [0u8; 64 * 1024];
            loop {
                match pane.pty.read(&mut buf) {
                    Ok(0) => {
                        eof = true;
                        pane.pty.eof = true;
                        break;
                    }
                    Ok(n) => {
                        saw_data = true;
                        pane.feed(&buf[..n]);
                        events.extend(pane.take_events());
                        if n < buf.len() {
                            // Keep draining until EAGAIN, but avoid an
                            // unbounded loop for a very chatty process.
                            if events.len() > 512 {
                                break;
                            }
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        log(&format!("pane {} read error: {}", id, err));
                        eof = true;
                        pane.pty.eof = true;
                        break;
                    }
                }
            }
            let _ = pane.flush_input();
        }
        if saw_data {
            self.dirty = true;
        }
        for event in events {
            self.handle_terminal_event(id, event);
        }
        if eof {
            // Exit is reaped by the tick loop after SIGCHLD/waitpid.
            self.dirty = true;
        }
    }

    fn flush_pane(&mut self, id: PaneId) {
        if let Some(pane) = self.pane_mut(id) {
            let _ = pane.flush_input();
        }
    }

    fn handle_terminal_event(&mut self, pane_id: PaneId, event: crate::term::Event) {
        use crate::term::Event;
        match event {
            Event::Title(title) => {
                if let Some(pane) = self.pane_mut(pane_id) {
                    let title = util::truncate_display(&title, 120);
                    if !title.is_empty() {
                        pane.title = title;
                    }
                }
                self.dirty = true;
            }
            Event::Cwd(path) => {
                if let Some(pane) = self.pane_mut(pane_id) {
                    pane.cwd = Some(PathBuf::from(path));
                }
            }
            Event::Clipboard(text) => {
                self.set_clipboard(text);
                self.dirty = true;
            }
            Event::Notify(message) => {
                self.ui.toast_info(&self.theme, message);
                self.dirty = true;
            }
            Event::Bell => {
                let window_index = self.window_index_of_pane(pane_id);
                let focused = self.focused() == Some(pane_id);
                if let Some(index) = window_index {
                    if let Some(pane) = self.windows[index].pane_mut(pane_id) {
                        pane.bell = !focused;
                    }
                }
                if focused {
                    match self.config.bell {
                        BellMode::Audible => {
                            for client in self.clients.values_mut() {
                                client.queue_msg(&ServerMsg::Raw(b"\x07".to_vec()));
                            }
                        }
                        BellMode::Visual => {
                            let number = window_index.map(|i| i + 1).unwrap_or(1);
                            self.ui
                                .toast_info(&self.theme, format!("Bell — window {}", number));
                        }
                        BellMode::Off => {}
                    }
                }
                self.dirty = true;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frame composition
// ---------------------------------------------------------------------------

impl Session {
    fn render_clients(&mut self) {
        let ids: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, c)| c.hello)
            .map(|(id, _)| *id)
            .collect();
        let mut overflow = Vec::new();
        for id in ids {
            let (canvas, hits, bytes) = {
                let Some(client) = self.clients.get(&id) else {
                    continue;
                };
                let (canvas, hits) = self.compose(client);
                let bytes = encode_frame(
                    client.last_frame.as_ref(),
                    client.last_cursor,
                    &canvas,
                    client.caps,
                );
                (canvas, hits, bytes)
            };
            if let Some(client) = self.clients.get_mut(&id) {
                let cursor = canvas.cursor;
                if !bytes.is_empty() && !client.queue_msg(&ServerMsg::Frame(bytes)) {
                    overflow.push(id);
                }
                client.last_cursor = cursor;
                client.last_frame = Some(canvas);
                client.hits = hits;
            }
        }
        for id in overflow {
            self.ui
                .toast_error(&self.theme, "A client is too slow; disconnecting");
            self.remove_client(id);
        }
        self.dirty = false;
    }

    fn compose(&self, client: &Client) -> (crate::render::Canvas, MouseHits) {
        let cols = client.size.0.max(1);
        let rows = client.size.1.max(1);
        let status_rows = if rows <= 1 {
            0
        } else {
            self.config.status_rows.min(rows - 1)
        };
        let status_y = rows.saturating_sub(status_rows);
        let content_h = if status_rows == 0 { rows } else { status_y };
        let area = Rect::new(0, 0, cols, content_h.max(1));
        let mut canvas = crate::render::Canvas::new(cols, rows);
        let mut hits = MouseHits {
            status_y,
            ..Default::default()
        };

        let mut copy_for_window: Option<&CopyState> = None;
        if let Some(copy) = &self.ui.copy {
            if let Some(window) = self.windows.get(self.active_window) {
                if window.id == copy.window_id {
                    copy_for_window = Some(copy);
                }
            }
        }

        if let Some(window) = self.windows.get(self.active_window) {
            let zoom = window.zoom && window.pane_count() > 1;
            let solved = if zoom {
                SolvedLayout {
                    panes: vec![PanePlacement {
                        pane: window.active,
                        rect: area,
                        content: area,
                        header: false,
                    }],
                    border: vec![false; area.w as usize * area.h as usize],
                    border_w: area.w,
                    border_h: area.h,
                    framed: false,
                }
            } else {
                window.layout.solve(area, window.pane_count() > 1)
            };
            let total = solved.panes.len().max(1);
            for (idx, placement) in solved.panes.iter().enumerate() {
                let active = placement.pane == window.active;
                let copy = copy_for_window.filter(|c| c.pane_id == placement.pane);
                self.compose_pane(
                    &mut canvas,
                    window,
                    placement,
                    active,
                    (idx + 1, total),
                    copy,
                );
                let header = if placement.header {
                    Rect::new(placement.rect.x, placement.rect.y, placement.rect.w, 1)
                } else {
                    Rect::new(placement.content.x, placement.content.y, 0, 0)
                };
                hits.panes.push(PaneHit {
                    window_id: window.id,
                    pane_id: placement.pane,
                    content: placement.content,
                    header,
                });
            }
            if !zoom && window.pane_count() > 1 {
                let active_rect = solved
                    .panes
                    .iter()
                    .find(|p| p.pane == window.active)
                    .map(|p| p.rect);
                let border_style = Style::default().fg(self.theme.border);
                let focus_style = Style::default().fg(self.theme.border_focus);
                for y in 0..area.h {
                    for x in 0..area.w {
                        let pos = y as usize * area.w as usize + x as usize;
                        if solved.border.get(pos).copied().unwrap_or(false) {
                            let glyph = border_glyph(&solved, x, y);
                            let touching = active_rect
                                .map(|r| {
                                    let ax = area.x + x;
                                    let ay = area.y + y;
                                    (ax > 0 && r.contains(ax - 1, ay))
                                        || (ax + 1 < cols && r.contains(ax + 1, ay))
                                        || (ay > 0 && r.contains(ax, ay - 1))
                                        || (ay + 1 < rows && r.contains(ax, ay + 1))
                                })
                                .unwrap_or(false);
                            let style = if touching { focus_style } else { border_style };
                            canvas.set(area.x + x, area.y + y, Cell::ch(glyph, style));
                        }
                    }
                }
            }
        }

        // Status bar.
        if status_rows > 0 {
            let tabs: Vec<WindowTab> = self
                .windows
                .iter()
                .enumerate()
                .map(|(i, window)| WindowTab {
                    index: i + 1,
                    title: window.display_title(),
                    bell: window.panes.values().any(|p| p.bell),
                    active: i == self.active_window,
                })
                .collect();
            let pane_count = self
                .windows
                .get(self.active_window)
                .map(|w| w.pane_count())
                .unwrap_or(0);
            let zoom = self
                .windows
                .get(self.active_window)
                .map(|w| w.zoom && w.pane_count() > 1)
                .unwrap_or(false);
            let copy = self.ui.copy.as_ref();
            let mut pane_text = format!(
                "{} pane{}",
                pane_count,
                if pane_count == 1 { "" } else { "s" }
            );
            if let Some(copy) = copy {
                if let Some(status) = copy.search_status() {
                    pane_text = status;
                }
            }
            let badge = if let Some(copy) = copy.filter(|_| status_rows >= 2) {
                Some((
                    format!("COPY {}%", copy.scroll_percent()),
                    self.theme.accent_text,
                    self.theme.mauve,
                ))
            } else if zoom {
                Some((
                    "ZOOM".to_string(),
                    self.theme.accent_text,
                    self.theme.surface,
                ))
            } else {
                None
            };
            let hints = self.status_hints();
            let inputs = StatusInput {
                session: &self.name,
                tabs,
                pane_text: Some(pane_text),
                badge,
                clock: if self.config.clock {
                    self.last_clock.clone()
                } else {
                    String::new()
                },
                clients: self.clients.values().filter(|c| c.hello).count(),
                status_rows,
                hints: &hints,
                version: &self.version,
            };
            let status_hits = ui::draw_status_bar(&mut canvas, &self.theme, &inputs);
            for (x, w, number) in status_hits.tabs {
                if let Some(window) = self.windows.get(number.saturating_sub(1)) {
                    hits.tabs.push((x, w, window.id));
                }
            }
        }

        // Transient hint strip (prefix pending, or copy mode in compact
        // layout) and toast, above the status bar.
        let prefix_pending = client.input.prefix_pending;
        let copy_active = self.ui.copy.is_some();
        let mut strip_y = None;
        if status_rows > 0 && status_y > 1 {
            if prefix_pending {
                let hints = self.prefix_hints();
                let y = status_y - 1;
                draw_hint_strip(
                    &mut canvas,
                    &self.theme,
                    y,
                    "PREFIX",
                    self.theme.yellow,
                    &self.prefix_label,
                    &hints,
                );
                strip_y = Some(y);
            } else if copy_active && status_rows < 2 {
                let y = status_y - 1;
                let pct = self
                    .ui
                    .copy
                    .as_ref()
                    .map(|c| c.scroll_percent())
                    .unwrap_or(0);
                let hints = vec![
                    ("space".to_string(), "select".to_string()),
                    ("y".to_string(), "copy".to_string()),
                    ("/".to_string(), "search".to_string()),
                    ("q".to_string(), "quit".to_string()),
                ];
                draw_hint_strip(
                    &mut canvas,
                    &self.theme,
                    y,
                    "COPY",
                    self.theme.mauve,
                    &format!("{}%", pct),
                    &hints,
                );
                strip_y = Some(y);
            }
        }
        if let Some(toast) = &self.ui.toast {
            let bottom = strip_y.unwrap_or_else(|| status_y.saturating_sub(1).max(1));
            draw_toast(
                &mut canvas,
                &self.theme,
                bottom,
                &toast.text,
                toast.tone,
                toast.icon,
            );
        }

        // Modal overlays sit above everything, including the status bar.
        if self.ui.overlay_open() {
            ui::draw_overlay(&mut canvas, &self.theme, &self.ui, status_rows);
        }

        canvas.normalize_wide();
        (canvas, hits)
    }

    fn compose_pane(
        &self,
        canvas: &mut crate::render::Canvas,
        window: &Window,
        placement: &PanePlacement,
        active: bool,
        order: (usize, usize),
        copy: Option<&CopyState>,
    ) {
        let Some(pane) = window.pane(placement.pane) else {
            return;
        };
        let content = placement.content;
        if content.w == 0 || content.h == 0 {
            return;
        }

        if placement.header {
            let style = if active {
                Style::default()
                    .fg(self.theme.status_fg)
                    .bg(self.theme.surface)
                    .bold()
            } else {
                Style::default()
                    .fg(self.theme.muted)
                    .bg(self.theme.surface_alt)
            };
            canvas.fill(
                placement.rect.x,
                placement.rect.y,
                placement.rect.w,
                1,
                Cell::blank(style),
            );
            let marker_style = Style::default()
                .fg(if active {
                    self.theme.accent
                } else {
                    self.theme.muted
                })
                .bg(if active {
                    self.theme.surface
                } else {
                    self.theme.surface_alt
                });
            canvas.put_str(placement.rect.x, placement.rect.y, 2, "▎", marker_style);
            let bell = if pane.bell { "● " } else { "" };
            let title = format!("{}{}", bell, pane.title);
            let right = format!("{}/{}", order.0, order.1);
            let right_w = util::width(&right) as u16 + 1;
            let title_max = placement.rect.w.saturating_sub(4 + right_w);
            if title_max > 2 {
                canvas.put_str(
                    placement.rect.x + 2,
                    placement.rect.y,
                    title_max,
                    &util::truncate_display(&title, title_max as usize),
                    style,
                );
            }
            if placement.rect.w > right_w + 2 {
                canvas.put_str(
                    placement.rect.x + placement.rect.w - right_w,
                    placement.rect.y,
                    right_w,
                    &right,
                    Style::default().fg(self.theme.muted).bg(if active {
                        self.theme.surface
                    } else {
                        self.theme.surface_alt
                    }),
                );
            }
        }

        if let Some(copy) = copy {
            self.compose_copy(canvas, content, copy, active);
            return;
        }

        let screen = &pane.terminal.screen;
        let buffer = screen.active();
        let off_y = buffer.rows.saturating_sub(content.h as usize);
        let reverse = screen.modes.reverse_video;
        for dy in 0..content.h {
            let sy = off_y + dy as usize;
            if sy >= buffer.rows {
                break;
            }
            let row = buffer.row(sy);
            for dx in 0..content.w {
                let sx = dx as usize;
                if sx >= row.len() {
                    break;
                }
                let cell = &row[sx];
                if cell.cont {
                    continue;
                }
                if cell.wide && dx + 2 > content.w {
                    let mut blank = Cell::blank(cell.style);
                    if reverse {
                        std::mem::swap(&mut blank.style.fg, &mut blank.style.bg);
                    }
                    canvas.set(content.x + dx, content.y + dy, blank);
                    continue;
                }
                canvas.set_pane_cell(content.x + dx, content.y + dy, cell, reverse);
            }
        }

        if active && screen.modes.cursor_visible {
            let cursor = buffer.cursor;
            if cursor.row >= off_y && cursor.row < off_y + content.h as usize {
                let mut col = cursor.col;
                if col > 0 && buffer.get(cursor.row, col).cont {
                    col -= 1;
                }
                if col < content.w as usize {
                    canvas.set_cursor(Some(CursorView {
                        x: content.x + col as u16,
                        y: content.y + (cursor.row - off_y) as u16,
                        visible: true,
                        shape: screen.cursor_style,
                    }));
                }
            }
        }
    }

    fn compose_copy(
        &self,
        canvas: &mut crate::render::Canvas,
        content: Rect,
        copy: &CopyState,
        _active: bool,
    ) {
        let (top, bottom) = copy.viewport(content.h);
        for (dy, row_index) in (top..bottom).enumerate() {
            let Some(line) = copy.snapshot.rows.get(row_index) else {
                continue;
            };
            for dx in 0..content.w {
                let col = dx as usize;
                if col >= copy.snapshot.cols {
                    break;
                }
                let Some(cell) = line.cells.get(col) else {
                    continue;
                };
                if cell.cont {
                    continue;
                }
                let mut style = cell.style;
                if copy.snapshot.reverse {
                    std::mem::swap(&mut style.fg, &mut style.bg);
                }
                if copy.is_selected(row_index, col) {
                    style.attrs ^= attrs::REVERSE;
                }
                if let Some(current) = copy.match_highlight(row_index, col) {
                    style.bg = if current {
                        self.theme.mauve
                    } else {
                        self.theme.yellow
                    };
                    style.fg = self.theme.accent_text;
                    style.attrs &= !attrs::REVERSE;
                }
                if cell.wide && dx + 2 > content.w {
                    canvas.set(content.x + dx, content.y + dy as u16, Cell::blank(style));
                    continue;
                }
                let out = Cell {
                    ch: cell.ch,
                    extra: cell.extra.clone(),
                    style,
                    link: cell.link.clone(),
                    wide: cell.wide,
                    cont: false,
                };
                canvas.set(content.x + dx, content.y + dy as u16, out);
            }
        }
        if let Some((col, row)) = copy.cursor_position(content.h) {
            canvas.set_cursor(Some(CursorView {
                x: content.x + col.min(content.w.saturating_sub(1)),
                y: content.y + row.min(content.h.saturating_sub(1)),
                visible: true,
                shape: 2,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_has_screen_compatible_aliases() {
        let (map, warnings) = KeyMap::new(&Config::default());
        assert!(warnings.is_empty());
        assert_eq!(map.lookup(Key::Char('c')), Some(Action::NewWindow));
        assert_eq!(map.lookup(Key::Char('"')), Some(Action::WindowPicker));
        assert_eq!(map.lookup(Key::Char('S')), Some(Action::SplitDown));
        assert_eq!(map.lookup(Key::Char('K')), Some(Action::ResizeUp));
        assert_eq!(map.lookup(Key::Char('X')), Some(Action::KillSession));
    }

    #[test]
    fn sgr_and_x10_mouse_encoding() {
        let press = MouseEvent {
            x: 12,
            y: 5,
            button: 0,
            pressed: true,
            motion: false,
        };
        assert_eq!(encode_mouse(press, true, false), b"\x1b[<0;12;5M");
        let release = MouseEvent {
            pressed: false,
            ..press
        };
        assert_eq!(encode_mouse(release, true, false), b"\x1b[<0;12;5m");
        assert_eq!(
            encode_mouse(press, false, false),
            vec![0x1b, b'[', b'M', 32, 32 + 12, 32 + 5]
        );
    }

    #[test]
    fn mouse_mode_filtering() {
        use crate::term::MouseMode;
        let motion = MouseEvent {
            x: 1,
            y: 1,
            button: 32,
            pressed: true,
            motion: true,
        };
        assert!(!mouse_wanted(MouseMode::Normal, motion));
        assert!(mouse_wanted(MouseMode::Button, motion));
        assert!(mouse_wanted(MouseMode::Any, motion));
        let wheel = MouseEvent {
            x: 1,
            y: 1,
            button: 64,
            pressed: true,
            motion: false,
        };
        assert!(mouse_wanted(MouseMode::Normal, wheel));
        assert!(mouse_wanted(MouseMode::X10, wheel));
    }

    #[test]
    fn palette_exposes_previous_pane() {
        // Ensures the palette does not accidentally route "Focus previous
        // pane" to the next-pane action.
        assert_eq!(
            palette_action_for(Action::LastPane),
            PaletteAction::PrevPane
        );
    }
}
