//! Pseudo-terminal creation and child process management.

use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use nix::pty::openpty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    pub success: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitStatus {
    pub fn describe(&self) -> String {
        if let Some(sig) = self.signal {
            format!("terminated by signal {}", sig)
        } else if let Some(code) = self.code {
            if code == 0 {
                "exited".to_string()
            } else {
                format!("exited with status {}", code)
            }
        } else {
            "exited".to_string()
        }
    }
}

pub struct Pty {
    master: OwnedFd,
    pid: libc::pid_t,
    pub eof: bool,
    pub status: Option<ExitStatus>,
    /// Set when the process group has been asked to terminate.
    pub terminating: bool,
}

impl std::fmt::Debug for Pty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pty")
            .field("pid", &self.pid)
            .field("eof", &self.eof)
            .field("status", &self.status)
            .finish()
    }
}

pub struct SpawnOptions<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: Option<&'a Path>,
    pub cols: u16,
    pub rows: u16,
    pub session: &'a str,
    pub login_shell: bool,
}

pub fn spawn(options: SpawnOptions<'_>) -> Result<Pty> {
    let winsize = libc::winsize {
        ws_row: options.rows.max(1),
        ws_col: options.cols.max(1),
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(None, None).context("opening a pseudo terminal")?;
    let master = pty.master;
    let slave = pty.slave;
    set_winsize_fd(slave.as_raw_fd(), &winsize);

    let mut cmd = Command::new(options.program);
    if options.login_shell && options.args.is_empty() {
        let base = Path::new(options.program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("sh");
        let mut arg0 = OsString::from("-");
        arg0.push(base);
        cmd.arg0(arg0);
    }
    cmd.args(options.args);
    if let Some(cwd) = options.cwd {
        cmd.current_dir(cwd);
    }

    // Nested multiplexers confuse applications (and can echo escape codes
    // twice), so hide the outer session markers.
    cmd.env_remove("TMUX");
    cmd.env_remove("TMUX_PANE");
    cmd.env_remove("STY");
    cmd.env_remove("WINDOW");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "rscreen");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("RSCREEN", "1");
    cmd.env("RSCREEN_SESSION", options.session);

    let stdin = Stdio::from(slave.try_clone()?);
    let stdout = Stdio::from(slave.try_clone()?);
    let stderr = Stdio::from(slave.try_clone()?);
    cmd.stdin(stdin);
    cmd.stdout(stdout);
    cmd.stderr(stderr);

    unsafe {
        cmd.pre_exec(|| {
            // New session and controlling terminal.
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            // Rust ignores SIGPIPE in the parent; children should get the
            // conventional default behavior. The server also ignores SIGHUP
            // so it survives its launching terminal; reset it for the shell.
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            Ok(())
        });
    }

    let child = cmd.spawn().with_context(|| {
        format!(
            "starting {} in {}",
            options.program,
            options.cwd.unwrap_or(Path::new("~")).display()
        )
    })?;
    drop(slave);

    let pid = child.id() as libc::pid_t;
    set_nonblocking(master.as_raw_fd())?;
    set_cloexec(master.as_raw_fd());

    Ok(Pty {
        master,
        pid,
        eof: false,
        status: None,
        terminating: false,
    })
}

impl Pty {
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    pub fn master_fd(&self) -> std::os::fd::RawFd {
        self.master.as_raw_fd()
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = unsafe {
                libc::read(
                    self.master.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "pty would block"))
                }
                Some(libc::EINTR) => continue,
                Some(libc::EIO) => return Ok(0),
                _ => return Err(err),
            }
        }
    }

    pub fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        loop {
            let n = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) => {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "pty would block"))
                }
                Some(libc::EINTR) => continue,
                _ => return Err(err),
            }
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let winsize = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        set_winsize_fd(self.master.as_raw_fd(), &winsize);
        // Tell the foreground process group. SIGWINCH is harmless if the app
        // does not care.
        unsafe {
            libc::kill(-self.pid, libc::SIGWINCH);
        }
    }

    /// Check for exit without blocking. Reaps the child at most once.
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        if let Some(status) = self.status {
            return Some(status);
        }
        let mut raw = 0;
        let pid = unsafe { libc::waitpid(self.pid, &mut raw, libc::WNOHANG) };
        if pid == self.pid {
            let status = parse_status(raw);
            self.status = Some(status);
            self.eof = true;
            Some(status)
        } else {
            None
        }
    }

    pub fn signal(&self, sig: libc::c_int) {
        unsafe {
            if self.pid > 0 {
                libc::kill(-self.pid, sig);
                libc::kill(self.pid, sig);
            }
        }
    }

    /// Politely terminate the process group, escalating elsewhere on a timer.
    pub fn terminate(&mut self) {
        if self.status.is_some() {
            return;
        }
        self.terminating = true;
        self.signal(libc::SIGHUP);
        self.signal(libc::SIGTERM);
        self.signal(libc::SIGCONT);
    }

    pub fn force_kill(&mut self) {
        if self.status.is_none() {
            self.signal(libc::SIGKILL);
        }
    }

    /// Reap without blocking, returning the status if it was already available.
    pub fn reap(&mut self) -> Option<ExitStatus> {
        self.try_wait()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Closing the master hangs up the slave. Also make sure we do not leave
        // a running process group behind when a pane is removed.
        if self.status.is_none() {
            self.force_kill();
            let mut raw = 0;
            unsafe {
                libc::waitpid(self.pid, &mut raw, libc::WNOHANG);
            }
        }
    }
}

fn parse_status(raw: i32) -> ExitStatus {
    if libc::WIFEXITED(raw) {
        let code = libc::WEXITSTATUS(raw);
        ExitStatus {
            success: code == 0,
            code: Some(code),
            signal: None,
        }
    } else if libc::WIFSIGNALED(raw) {
        let sig = libc::WTERMSIG(raw);
        ExitStatus {
            success: false,
            code: None,
            signal: Some(sig),
        }
    } else {
        ExitStatus {
            success: false,
            code: None,
            signal: None,
        }
    }
}

fn set_nonblocking(fd: libc::c_int) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn set_cloexec(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

fn set_winsize_fd(fd: libc::c_int, ws: &libc::winsize) {
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ as _, ws as *const libc::winsize);
    }
}

/// Return the current working directory of a process, if readable.
pub fn process_cwd(pid: libc::pid_t) -> Option<std::path::PathBuf> {
    if pid <= 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
}

/// A fallback used when /proc is unavailable.
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}
