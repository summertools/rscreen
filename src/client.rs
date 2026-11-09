//! The thin terminal client. It owns the physical terminal, sends input and
//! resize events to the session server, and paints the ANSI frames it receives.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

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

    let winch = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGWINCH, winch.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGTERM, interrupted.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGHUP, interrupted.clone());
    let _ = signal_hook::flag::register(signal_hook::consts::SIGQUIT, interrupted.clone());
    let guard = TerminalGuard::new(&options.name, options.config.mouse)?;

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
                                        if let Err(err) = write_terminal(&frame) {
                                            outcome = Some(AttachOutcome::Error(format!(
                                                "cannot write to terminal: {}",
                                                err
                                            )));
                                            break 'outer;
                                        }
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

/// Write a complete frame to the terminal.
///
/// The terminal may be non-blocking because a parent multiplexer or a previous
/// tool set O_NONBLOCK on the shared terminal description. A large frame (for
/// example when rtop redraws) can then fill the PTY buffer and return EAGAIN.
/// Wait for writability instead of treating that as a fatal error, which used
/// to make the client flash and exit.
fn write_terminal(bytes: &[u8]) -> io::Result<()> {
    write_all_fd(libc::STDOUT_FILENO, bytes)
}

fn write_all_fd(fd: libc::c_int, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let written =
            unsafe { libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len()) };
        if written > 0 {
            bytes = &bytes[written as usize..];
            continue;
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "terminal write returned zero bytes",
            ));
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                let mut poll_fd =
                    PollFd::new(unsafe { BorrowedFd::borrow_raw(fd) }, PollFlags::POLLOUT);
                loop {
                    match poll(std::slice::from_mut(&mut poll_fd), PollTimeout::NONE) {
                        Ok(_) => break,
                        Err(nix::errno::Errno::EINTR) => continue,
                        Err(err) => {
                            return Err(io::Error::from_raw_os_error(err as i32));
                        }
                    }
                }
                let revents = poll_fd.revents().unwrap_or(PollFlags::empty());
                if revents.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "terminal output is no longer writable",
                    ));
                }
            }
            _ => return Err(err),
        }
    }
    Ok(())
}

fn queue_msg<T: crate::protocol::WireMessage>(out: &mut Vec<u8>, msg: &T) -> Result<()> {
    const MAX_QUEUE: usize = 16 * 1024 * 1024;
    let bytes = crate::protocol::encode(msg)
        .map_err(|e| anyhow::anyhow!("protocol encode error: {}", e))?;
    if out.len().saturating_add(bytes.len()) > MAX_QUEUE {
        anyhow::bail!("input queue is full; the session server is not keeping up");
    }
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
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n >= 0 {
        return Ok(n as usize);
    }
    // Return EINTR to the event loop instead of retrying here. signal-hook
    // only sets flags; the loop must get a chance to process SIGTERM/winch.
    Err(io::Error::last_os_error())
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
            seq.push_str(&format!(
                "\x1b[2;1H\x1b[2m  rscreen · connecting to {}…\x1b[0m",
                title
            ));
            seq.push_str("\x1b[?2004h"); // bracketed paste
            if mouse {
                seq.push_str("\x1b[?1003h\x1b[?1006h");
            }
            seq.push_str(&format!("\x1b]2;rscreen: {}\x1b\\", title));
            if let Err(err) = write_terminal(seq.as_bytes()) {
                guard.restore_now();
                return Err(err).context("initializing terminal");
            }
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
            seq.push_str("\x1b[?1003l\x1b[?1006l");
        }
        seq.push_str("\x1b[?2004l");
        seq.push_str("\x1b[?2026l");
        seq.push_str("\x1b[?25h");
        seq.push_str("\x1b[?1049l");
        seq.push_str("\x1b[23;0t");
        let _ = write_terminal(seq.as_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Seek as _, SeekFrom};
    use std::os::fd::AsRawFd;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn write_all_fd_writes_every_byte() {
        let mut file = tempfile::tempfile().expect("temporary file");
        let payload = b"terminal frame".repeat(4096);
        write_all_fd(file.as_raw_fd(), &payload).expect("write frame");
        file.seek(SeekFrom::Start(0)).expect("rewind");
        let mut read = Vec::new();
        file.read_to_end(&mut read).expect("read back");
        assert_eq!(read, payload);
    }

    #[test]
    fn write_all_fd_waits_for_a_full_nonblocking_pipe() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        let write_file = File::from(write_fd);
        let mut read_file = File::from(read_fd);
        let fd = write_file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);

        let filler = [b'x'; 4096];
        let mut filled = 0usize;
        loop {
            let written =
                unsafe { libc::write(fd, filler.as_ptr().cast::<libc::c_void>(), filler.len()) };
            if written > 0 {
                filled += written as usize;
                continue;
            }
            match io::Error::last_os_error().kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break,
                other => panic!("unexpected pipe write error: {other:?}"),
            }
        }
        assert!(filled > 0, "pipe did not fill");

        let reader = thread::spawn(move || {
            let mut data = Vec::new();
            read_file.read_to_end(&mut data).expect("drain pipe");
            data
        });
        // Give the reader a chance to block on the pipe, then write through
        // the same non-blocking descriptor. The helper must poll and retry
        // rather than surfacing EAGAIN.
        thread::sleep(Duration::from_millis(10));
        write_all_fd(fd, b"payload").expect("write after drain");
        drop(write_file);
        let data = reader.join().expect("reader thread");
        assert_eq!(data.len(), filled + b"payload".len());
        assert!(data.ends_with(b"payload"));
    }
}
