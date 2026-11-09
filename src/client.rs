//! The thin terminal client. It owns the physical terminal, sends input and
//! resize events to the session server, and paints the ANSI frames it receives.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use nix::poll::{poll, PollFd, PollFlags};

use crate::config::Config;
use crate::protocol::{Caps, ClientMsg, FrameReader, ServerMsg, PROTOCOL_VERSION};

pub enum AttachOutcome {
    Detached(String),
    Ended(String),
    Error(String),
}

pub struct AttachOptions {
    pub socket: std::path::PathBuf,
    pub name: String,
    pub config: Config,
}

pub fn attach(options: AttachOptions) -> Result<AttachOutcome> {
    if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
        anyhow::bail!(
            "rscreen attach needs an interactive terminal (stdin and stdout must be a tty)"
        );
    }
    let size = terminal_size();
    let mut stream = UnixStream::connect(&options.socket)
        .with_context(|| format!("connecting to session {:?}", options.name))?;
    stream.set_nonblocking(true)?;

    let _flags = FdFlags::set_nonblocking(0)?;
    let guard = TerminalGuard::new(&options.name, options.config.mouse)?;

    let winch = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGWINCH, winch.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, interrupted.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGHUP, interrupted.clone());

    let caps = Caps {
        truecolor: detect_truecolor(),
    };
    let mut out: Vec<u8> = Vec::new();
    queue_msg(
        &mut out,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            cols: size.0,
            rows: size.1,
            caps,
        },
    )?;
    let mut out_off = 0usize;
    let mut reader = FrameReader::new();
    let outcome: Option<AttachOutcome>;
    let mut stdout = io::stdout();
    let mut socket_open = true;
    let connected_at = Instant::now();
    let mut welcomed = false;

    'outer: loop {
        if !welcomed && connected_at.elapsed() > std::time::Duration::from_secs(5) {
            outcome = Some(AttachOutcome::Error(format!(
                "session {} did not answer the handshake",
                options.name
            )));
            break;
        }
        if winch.swap(false, Ordering::Relaxed) {
            let (cols, rows) = terminal_size();
            stream.set_nonblocking(true)?;
            // Queue resize; flush below.
            queue_msg(&mut out, &ClientMsg::Resize { cols, rows })?;
        }
        if interrupted.swap(false, Ordering::Relaxed) {
            queue_msg(&mut out, &ClientMsg::Detach { all: false })?;
            // Try one last flush.
            let _ = write_some(&mut stream, &mut out, &mut out_off);
            outcome = Some(AttachOutcome::Detached(format!(
                "detached from {}",
                options.name
            )));
            break;
        }

        // Flush pending messages to the server.
        if out_off < out.len() {
            match write_some(&mut stream, &mut out, &mut out_off) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    outcome = Some(AttachOutcome::Ended(format!(
                        "disconnected from {}",
                        options.name
                    )));
                    break;
                }
            }
        }
        if out_off >= out.len() {
            out.clear();
            out_off = 0;
        }

        let stdin_fd = 0;
        let sock_fd = stream.as_raw_fd();
        let mut fds = vec![
            PollFd::new(
                unsafe { BorrowedFd::borrow_raw(stdin_fd) },
                PollFlags::POLLIN,
            ),
            PollFd::new(
                unsafe { BorrowedFd::borrow_raw(sock_fd) },
                if out_off < out.len() {
                    PollFlags::POLLIN | PollFlags::POLLOUT
                } else {
                    PollFlags::POLLIN
                },
            ),
        ];
        match poll(&mut fds, 60u16) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => return Err(err.into()),
        }

        let stdin_ready = fds[0]
            .revents()
            .map(|r| r.contains(PollFlags::POLLIN))
            .unwrap_or(false);
        let sock_ready = fds[1].revents().map(|r| !r.is_empty()).unwrap_or(false);
        let sock_error = fds[1]
            .revents()
            .map(|r| r.intersects(PollFlags::POLLERR | PollFlags::POLLNVAL))
            .unwrap_or(false);

        if stdin_ready {
            let mut buf = [0u8; 64 * 1024];
            match read_fd(0, &mut buf) {
                Ok(0) => {
                    let _ = queue_msg(&mut out, &ClientMsg::Detach { all: false });
                    let _ = write_some(&mut stream, &mut out, &mut out_off);
                    outcome = Some(AttachOutcome::Detached(format!(
                        "detached from {}",
                        options.name
                    )));
                    break;
                }
                Ok(n) => {
                    queue_msg(&mut out, &ClientMsg::Input(buf[..n].to_vec()))?;
                    // Flush immediately for low typing latency.
                    match write_some(&mut stream, &mut out, &mut out_off) {
                        Ok(()) => {}
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            outcome = Some(AttachOutcome::Ended(format!(
                                "disconnected from {}",
                                options.name
                            )));
                            break;
                        }
                    }
                    if out_off >= out.len() {
                        out.clear();
                        out_off = 0;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err.into()),
            }
        }

        if sock_ready || sock_error {
            let mut buf = [0u8; 64 * 1024];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => {
                        socket_open = false;
                        break;
                    }
                    Ok(n) => {
                        if let Err(err) = reader.push(&buf[..n]) {
                            outcome = Some(AttachOutcome::Error(err.to_string()));
                            break 'outer;
                        }
                        loop {
                            match reader.next_message::<ServerMsg>() {
                                Ok(Some(msg)) => match msg {
                                    ServerMsg::Welcome { .. } => {
                                        welcomed = true;
                                    }
                                    ServerMsg::Frame(frame) | ServerMsg::Raw(frame) => {
                                        if let Err(err) = stdout.write_all(&frame) {
                                            outcome = Some(AttachOutcome::Error(format!(
                                                "cannot write to terminal: {}",
                                                err
                                            )));
                                            break 'outer;
                                        }
                                        let _ = stdout.flush();
                                    }
                                    ServerMsg::Info(_) => {}
                                    ServerMsg::Exit { reason, .. } => {
                                        let message = if reason == "detached" {
                                            format!("detached from {}", options.name)
                                        } else {
                                            format!("{}: {}", options.name, reason)
                                        };
                                        outcome = Some(if reason == "detached" {
                                            AttachOutcome::Detached(message)
                                        } else {
                                            AttachOutcome::Ended(message)
                                        });
                                        break 'outer;
                                    }
                                    ServerMsg::Error(err) => {
                                        outcome = Some(AttachOutcome::Error(err));
                                        break 'outer;
                                    }
                                },
                                Ok(None) => break,
                                Err(err) => {
                                    outcome = Some(AttachOutcome::Error(err.to_string()));
                                    break 'outer;
                                }
                            }
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => {
                        outcome = Some(AttachOutcome::Error(format!("connection lost: {}", err)));
                        break 'outer;
                    }
                }
            }
            if !socket_open {
                outcome = Some(AttachOutcome::Ended(format!("{} ended", options.name)));
                break;
            }
        }
    }

    drop(guard);
    Ok(outcome.unwrap_or_else(|| AttachOutcome::Ended(format!("{} ended", options.name))))
}

fn queue_msg<T: serde::Serialize>(out: &mut Vec<u8>, msg: &T) -> Result<()> {
    let bytes = crate::protocol::encode(msg)
        .map_err(|e| anyhow::anyhow!("protocol encode error: {}", e))?;
    out.extend_from_slice(&bytes);
    Ok(())
}

fn write_some(stream: &mut UnixStream, out: &mut [u8], off: &mut usize) -> io::Result<()> {
    while *off < out.len() {
        match stream.write(&out[*off..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
            Ok(n) => *off += n,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn read_fd(fd: libc::c_int, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            _ => return Err(err),
        }
    }
}

fn terminal_size() -> (u16, u16) {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0;
    if !ok || ws.ws_col == 0 || ws.ws_row == 0 {
        (80, 24)
    } else {
        (ws.ws_col, ws.ws_row)
    }
}

fn detect_truecolor() -> bool {
    if let Ok(colorterm) = std::env::var("COLORTERM") {
        let lower = colorterm.to_ascii_lowercase();
        if lower.contains("truecolor") || lower.contains("24bit") {
            return true;
        }
    }
    if let Ok(term) = std::env::var("TERM") {
        let lower = term.to_ascii_lowercase();
        if [
            "direct",
            "truecolor",
            "kitty",
            "wezterm",
            "alacritty",
            "foot",
            "ghostty",
            "contour",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
        {
            return true;
        }
    }
    false
}

struct FdFlags {
    fd: libc::c_int,
    flags: libc::c_int,
}

impl FdFlags {
    fn set_nonblocking(fd: libc::c_int) -> io::Result<Self> {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if flags & libc::O_NONBLOCK == 0
                && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, flags })
        }
    }
}

impl Drop for FdFlags {
    fn drop(&mut self) {
        unsafe {
            libc::fcntl(self.fd, libc::F_SETFL, self.flags);
        }
    }
}

struct TerminalGuard {
    original: libc::termios,
    active: bool,
    mouse: bool,
}

impl TerminalGuard {
    fn new(name: &str, mouse: bool) -> Result<Self> {
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut term) != 0 {
                return Err(io::Error::last_os_error()).context("reading terminal mode");
            }
            let original = term;
            libc::cfmakeraw(&mut term);
            if libc::tcsetattr(0, libc::TCSANOW, &term) != 0 {
                return Err(io::Error::last_os_error()).context("setting raw terminal mode");
            }
            let mut guard = Self {
                original,
                active: true,
                mouse,
            };
            let title = sanitize_title(name);
            let mut seq = String::new();
            seq.push_str("\x1b[?1049h"); // alternate screen
            seq.push_str("\x1b[22;0t"); // save title
            seq.push_str("\x1b[H\x1b[2J");
            seq.push_str("\x1b[?25l");
            seq.push_str("\x1b[?2004h"); // bracketed paste
            if mouse {
                seq.push_str("\x1b[?1002h\x1b[?1006h");
            }
            seq.push_str(&format!("\x1b]2;rscreen: {}\x1b\\", title));
            if let Err(err) = io::stdout().write_all(seq.as_bytes()) {
                guard.restore_now();
                return Err(err).context("initializing terminal");
            }
            let _ = io::stdout().flush();
            guard.active = true;
            Ok(guard)
        }
    }

    fn restore_now(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut seq = String::new();
        seq.push_str("\x1b[0m");
        seq.push_str("\x1b[0 q");
        if self.mouse {
            seq.push_str("\x1b[?1002l\x1b[?1006l");
        }
        seq.push_str("\x1b[?2004l");
        seq.push_str("\x1b[?2026l");
        seq.push_str("\x1b[?25h");
        seq.push_str("\x1b[?1049l");
        seq.push_str("\x1b[23;0t");
        let mut out = io::stdout();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.original);
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore_now();
    }
}

fn sanitize_title(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(80).collect()
}
