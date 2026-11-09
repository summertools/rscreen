//! Configuration loading and the visual theme.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use serde::Deserialize;

use crate::paths;
use crate::term::{Color, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BellMode {
    #[default]
    Visual,
    Audible,
    Off,
}

impl BellMode {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "visual" | "flash" => Some(BellMode::Visual),
            "audible" | "sound" => Some(BellMode::Audible),
            "off" | "none" => Some(BellMode::Off),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub status_bg: Color,
    pub status_fg: Color,
    pub muted: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub border: Color,
    pub border_focus: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub mauve: Color,
    pub overlay_bg: Color,
    pub overlay_border: Color,
    pub shadow: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            status_bg: Color::Rgb(0x18, 0x18, 0x25),
            status_fg: Color::Rgb(0xcd, 0xd6, 0xf4),
            muted: Color::Rgb(0x6c, 0x70, 0x86),
            accent: Color::Rgb(0x89, 0xb4, 0xfa),
            accent_text: Color::Rgb(0x11, 0x11, 0x1b),
            surface: Color::Rgb(0x31, 0x32, 0x44),
            surface_alt: Color::Rgb(0x1e, 0x1e, 0x2e),
            border: Color::Rgb(0x45, 0x47, 0x5a),
            border_focus: Color::Rgb(0x6c, 0x8f, 0xc7),
            green: Color::Rgb(0xa6, 0xe3, 0xa1),
            yellow: Color::Rgb(0xf9, 0xe2, 0xaf),
            red: Color::Rgb(0xf3, 0x8b, 0xa8),
            mauve: Color::Rgb(0xcb, 0xa6, 0xf7),
            overlay_bg: Color::Rgb(0x11, 0x11, 0x1b),
            overlay_border: Color::Rgb(0x58, 0x5b, 0x70),
            shadow: Color::Rgb(0x0b, 0x0b, 0x12),
        }
    }
}

impl Theme {
    pub fn status_style(&self) -> Style {
        Style::default().fg(self.status_fg).bg(self.status_bg)
    }

    pub fn muted_style(&self) -> Style {
        Style::default().fg(self.muted).bg(self.status_bg)
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent_text).bg(self.accent)
    }

    pub fn surface_style(&self) -> Style {
        Style::default().fg(self.status_fg).bg(self.surface)
    }

    pub fn overlay_style(&self) -> Style {
        Style::default().fg(self.status_fg).bg(self.overlay_bg)
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub prefix: u8,
    pub shell: String,
    pub shell_args: Vec<String>,
    pub scrollback: usize,
    pub status_rows: u16,
    pub mouse: bool,
    pub mouse_select: bool,
    pub app_clipboard: bool,
    pub bell: BellMode,
    pub clock: bool,
    pub confirm_kill: bool,
    pub wheel_step: usize,
    pub theme: Theme,
    /// Raw `[keys]` overrides; validated by the input layer.
    pub key_overrides: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: 0x01,
            shell: String::new(),
            shell_args: Vec::new(),
            scrollback: 5000,
            status_rows: 1,
            mouse: true,
            mouse_select: true,
            app_clipboard: true,
            bell: BellMode::Visual,
            clock: true,
            confirm_kill: true,
            wheel_step: 3,
            theme: Theme::default(),
            key_overrides: HashMap::new(),
        }
    }
}

impl Config {
    pub fn load() -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let path = paths::config_path();
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return (Self::default(), warnings)
            }
            Err(err) => {
                warnings.push(format!("cannot read {}: {}", path.display(), err));
                return (Self::default(), warnings);
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                warnings.push(format!("cannot inspect {}: {}", path.display(), err));
                return (Self::default(), warnings);
            }
        };
        if !metadata.file_type().is_file() {
            warnings.push(format!(
                "config {} is not a regular file; using defaults",
                path.display()
            ));
            return (Self::default(), warnings);
        }
        if metadata.uid() != unsafe { libc::geteuid() } {
            warnings.push(format!(
                "config {} is not owned by the current user; using defaults",
                path.display()
            ));
            return (Self::default(), warnings);
        }
        if metadata.mode() & 0o022 != 0 {
            warnings.push(format!(
                "config {} is group/world-writable; using defaults",
                path.display()
            ));
            return (Self::default(), warnings);
        }
        if metadata.len() > 1024 * 1024 {
            warnings.push(format!(
                "config {} is too large; using defaults",
                path.display()
            ));
            return (Self::default(), warnings);
        }
        let mut text = String::new();
        let mut limited = file.take(1024 * 1024 + 1);
        if let Err(err) = limited.read_to_string(&mut text) {
            warnings.push(format!("cannot read {}: {}", path.display(), err));
            return (Self::default(), warnings);
        }
        if text.len() > 1024 * 1024 {
            warnings.push(format!(
                "config {} is too large; using defaults",
                path.display()
            ));
            return (Self::default(), warnings);
        }
        match Self::parse(&text) {
            Ok(cfg) => (cfg, warnings),
            Err(err) => {
                warnings.push(format!(
                    "config {}: {} (using defaults)",
                    path.display(),
                    err
                ));
                (Self::default(), warnings)
            }
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let file: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut cfg = Self::default();
        if let Some(prefix) = file.general.prefix.as_deref() {
            match parse_prefix(prefix) {
                Some(0x1b) => {
                    return Err("ESC cannot be used as the prefix key".into());
                }
                Some(byte) => cfg.prefix = byte,
                None => {
                    return Err(format!(
                        "invalid prefix {:?}; use e.g. \"C-a\" or a single ASCII character",
                        prefix
                    ))
                }
            }
        }
        if let Some(shell) = file.general.shell {
            cfg.shell = shell;
        }
        if let Some(args) = file.general.shell_args {
            cfg.shell_args = args;
        }
        if let Some(n) = file.general.scrollback {
            cfg.scrollback = n.min(100_000);
        }
        if let Some(n) = file.general.status_rows {
            cfg.status_rows = n.clamp(1, 2);
        }
        if let Some(v) = file.general.mouse {
            cfg.mouse = v;
        }
        if let Some(v) = file.general.mouse_select {
            cfg.mouse_select = v;
        }
        if let Some(v) = file.general.app_clipboard {
            cfg.app_clipboard = v;
        }
        if let Some(v) = file.general.bell.as_deref() {
            cfg.bell = BellMode::parse(v).ok_or_else(|| format!("invalid bell mode {:?}", v))?;
        }
        if let Some(v) = file.general.clock {
            cfg.clock = v;
        }
        if let Some(v) = file.general.confirm_kill {
            cfg.confirm_kill = v;
        }
        if let Some(v) = file.general.wheel_step {
            cfg.wheel_step = v.clamp(1, 50);
        }
        cfg.theme = apply_theme(file.theme);
        cfg.key_overrides = file.keys;
        Ok(cfg)
    }

    pub fn shell_path(&self) -> String {
        if !self.shell.is_empty() {
            return self.shell.clone();
        }
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }

    pub fn sample() -> &'static str {
        SAMPLE_CONFIG
    }

    pub fn write_sample(&self, path: &Path, overwrite: bool) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        if overwrite {
            options.create(true).truncate(true);
        } else {
            options.create_new(true);
        }
        let mut file = options.open(path)?;
        file.write_all(SAMPLE_CONFIG.as_bytes())?;
        file.flush()
    }
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    general: GeneralFile,
    #[serde(default)]
    theme: ThemeFile,
    #[serde(default)]
    keys: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct GeneralFile {
    prefix: Option<String>,
    shell: Option<String>,
    shell_args: Option<Vec<String>>,
    scrollback: Option<usize>,
    status_rows: Option<u16>,
    mouse: Option<bool>,
    mouse_select: Option<bool>,
    app_clipboard: Option<bool>,
    bell: Option<String>,
    clock: Option<bool>,
    confirm_kill: Option<bool>,
    wheel_step: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct ThemeFile {
    status_bg: Option<String>,
    status_fg: Option<String>,
    muted: Option<String>,
    accent: Option<String>,
    accent_text: Option<String>,
    surface: Option<String>,
    surface_alt: Option<String>,
    border: Option<String>,
    border_focus: Option<String>,
    green: Option<String>,
    yellow: Option<String>,
    red: Option<String>,
    mauve: Option<String>,
    overlay_bg: Option<String>,
    overlay_border: Option<String>,
    shadow: Option<String>,
}

fn apply_theme(file: ThemeFile) -> Theme {
    let mut theme = Theme::default();
    fn set(dst: &mut Color, src: Option<String>) {
        if let Some(text) = src {
            if let Some(c) = Color::parse(&text) {
                *dst = c;
            }
        }
    }
    set(&mut theme.status_bg, file.status_bg);
    set(&mut theme.status_fg, file.status_fg);
    set(&mut theme.muted, file.muted);
    set(&mut theme.accent, file.accent);
    set(&mut theme.accent_text, file.accent_text);
    set(&mut theme.surface, file.surface);
    set(&mut theme.surface_alt, file.surface_alt);
    set(&mut theme.border, file.border);
    set(&mut theme.border_focus, file.border_focus);
    set(&mut theme.green, file.green);
    set(&mut theme.yellow, file.yellow);
    set(&mut theme.red, file.red);
    set(&mut theme.mauve, file.mauve);
    set(&mut theme.overlay_bg, file.overlay_bg);
    set(&mut theme.overlay_border, file.overlay_border);
    set(&mut theme.shadow, file.shadow);
    theme
}

/// Parse a prefix key such as `C-a`, `C-Space`, or a single printable ASCII
/// character. Only single-byte prefixes are supported, which keeps input
/// handling deterministic.
pub fn parse_prefix(s: &str) -> Option<u8> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    match lower.as_str() {
        "" => return None,
        "c-space" | "c-@" => return Some(0x00),
        "c-[" | "esc" => return Some(0x1b),
        "c-\\" => return Some(0x1c),
        "c-]" => return Some(0x1d),
        "c-^" => return Some(0x1e),
        "c-_" => return Some(0x1f),
        _ => {}
    }
    if let Some(rest) = lower.strip_prefix("c-") {
        let mut chars = rest.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        if c.is_ascii_lowercase() {
            return Some((c as u8) & 0x1f);
        }
        if c.is_ascii_uppercase() {
            return Some((c.to_ascii_lowercase() as u8) & 0x1f);
        }
        return None;
    }
    let bytes = t.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii() {
        Some(bytes[0])
    } else {
        None
    }
}

/// Human readable prefix label, e.g. `C-a`.
pub fn prefix_label(prefix: u8) -> String {
    match prefix {
        0x00 => "C-Space".into(),
        0x1b => "C-[".into(),
        0x1c => "C-\\".into(),
        0x1d => "C-]".into(),
        0x1e => "C-^".into(),
        0x1f => "C-_".into(),
        1..=26 => format!("C-{}", (b'a' + prefix - 1) as char),
        b if b.is_ascii_graphic() => (b as char).to_string(),
        b => format!("0x{:02x}", b),
    }
}

pub const SAMPLE_CONFIG: &str = r##"# rscreen configuration
# Location: ~/.config/rscreen/config.toml

[general]
prefix = "C-a"          # prefix key; also accepts "C-b", "C-Space", ...
shell = ""              # empty = $SHELL
shell_args = []         # extra arguments for the shell
scrollback = 5000       # lines of history per pane
status_rows = 1         # 1 = compact bar, 2 = bar + key hints
mouse = true            # wheel scrolling and click-to-focus
mouse_select = true     # drag to select text (auto-copies)
app_clipboard = true    # allow applications to set the system clipboard via OSC 52
bell = "visual"         # visual | audible | off
clock = true            # show the time in the status bar
confirm_kill = true     # ask before killing a pane/window
wheel_step = 3          # lines per wheel notch

[theme]
# Catppuccin Mocha by default. Any #rrggbb color works.
status_bg = "#181825"
status_fg = "#cdd6f4"
muted = "#6c7086"
accent = "#89b4fa"
accent_text = "#11111b"
surface = "#313244"
surface_alt = "#1e1e2e"
border = "#45475a"
border_focus = "#6c8fc7"
green = "#a6e3a1"
yellow = "#f9e2af"
red = "#f38ba8"
mauve = "#cba6f7"
overlay_bg = "#11111b"
overlay_border = "#585b70"
shadow = "#0b0b12"

[keys]
# Prefix-key bindings. Values are one of:
#   a single character, "tab", "space", "enter", "esc", "up"/"down"/"left"/"right",
#   "home", "end", "pageup", "pagedown", "backspace", "insert", "delete",
#   "backtab", "f1".."f12"
# Use "none" to remove a binding.
#
# Available actions:
#   new_window, next_window, prev_window, window_picker, rename_window,
#   kill_window, split_right, split_down, next_pane, last_pane,
#   focus_left, focus_right, focus_up, focus_down,
#   resize_left, resize_right, resize_up, resize_down,
#   zoom_pane, close_pane, copy_mode, paste_buffer,
#   detach, detach_all, command_palette, help, kill_session, toggle_status
# new_window = "c"
# next_window = "n"
# prev_window = "p"
# window_picker = "w"
# split_right = "|"
# split_down = "-"
# command_palette = ":"
# help = "?"
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefixes() {
        assert_eq!(parse_prefix("C-a"), Some(1));
        assert_eq!(parse_prefix("C-Space"), Some(0));
        assert_eq!(parse_prefix("C-A"), Some(1));
        assert_eq!(parse_prefix("`"), Some(b'`'));
        assert_eq!(parse_prefix("C-1"), None);
        assert_eq!(prefix_label(1), "C-a");
    }

    #[test]
    fn default_config_roundtrip() {
        let cfg = Config::parse(SAMPLE_CONFIG).unwrap();
        assert_eq!(cfg.prefix, 1);
        assert_eq!(cfg.theme.accent, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(cfg.scrollback, 5000);
    }

    #[test]
    fn invalid_prefix_is_rejected() {
        assert!(Config::parse("[general]\nprefix = \"nope\"\n").is_err());
    }

    #[test]
    fn unknown_keys_are_preserved() {
        let cfg = Config::parse("[keys]\nnew_window = \"x\"\n").unwrap();
        assert_eq!(
            cfg.key_overrides.get("new_window").map(|s| s.as_str()),
            Some("x")
        );
    }
}
