//! A pane: one pseudo-terminal, its emulator and a small amount of session
//! metadata.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::layout::PaneId;
use crate::pty::{self, ExitStatus, Pty, SpawnOptions};
use crate::term::{MouseMode, Terminal};

pub struct Pane {
    pub id: PaneId,
    pub terminal: Terminal,
    pub pty: Pty,
    pub title: String,
    pub bell: bool,
    pub dead: bool,
    pub exit: Option<ExitStatus>,
    pub cwd: Option<PathBuf>,
    /// Data waiting to be written to the child.
    pending_input: Vec<u8>,
    input_offset: usize,
    pub kill_deadline: Option<Instant>,
}

impl Pane {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: PaneId,
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        cols: u16,
        rows: u16,
        session: &str,
        login_shell: bool,
        scrollback: usize,
    ) -> Result<Self> {
        let pty = pty::spawn(SpawnOptions {
            program,
            args,
            cwd,
            cols,
            rows,
            session,
            login_shell,
        })?;
        let title = std::path::Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("shell")
            .to_string();
        Ok(Self {
            id,
            terminal: Terminal::new(cols as usize, rows as usize, scrollback),
            pty,
            title,
            bell: false,
            dead: false,
            exit: None,
            cwd: cwd.map(|p| p.to_path_buf()),
            pending_input: Vec::new(),
            input_offset: 0,
            kill_deadline: None,
        })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pty.pid()
    }

    pub fn master_fd(&self) -> std::os::fd::RawFd {
        self.pty.master_fd()
    }

    pub fn size(&self) -> (u16, u16) {
        (
            self.terminal.screen.cols() as u16,
            self.terminal.screen.rows() as u16,
        )
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let (old_cols, old_rows) = self.size();
        if old_cols == cols && old_rows == rows {
            return;
        }
        self.terminal.resize(cols as usize, rows as usize);
        self.pty.resize(cols, rows);
    }

    pub fn feed(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.terminal.feed(data);
        let responses = self.terminal.take_responses();
        if !responses.is_empty() {
            self.queue_input(&responses);
        }
    }

    pub fn take_events(&mut self) -> Vec<crate::term::Event> {
        self.terminal.take_events()
    }

    /// Queue bytes for the child. Returns false if the queue is unreasonably
    /// large (the caller should tell the user rather than growing forever).
    pub fn queue_input(&mut self, bytes: &[u8]) -> bool {
        const LIMIT: usize = 8 * 1024 * 1024;
        if self.pending_input.len().saturating_sub(self.input_offset) + bytes.len() > LIMIT {
            return false;
        }
        self.pending_input.extend_from_slice(bytes);
        true
    }

    pub fn has_pending_input(&self) -> bool {
        self.input_offset < self.pending_input.len()
    }

    /// Try to flush queued input. Returns whether anything remains.
    pub fn flush_input(&mut self) -> io::Result<bool> {
        while self.input_offset < self.pending_input.len() {
            match self.pty.write(&self.pending_input[self.input_offset..]) {
                Ok(0) => break,
                Ok(n) => self.input_offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.input_offset >= self.pending_input.len() {
            self.pending_input.clear();
            self.input_offset = 0;
        } else if self.input_offset > 64 * 1024 {
            self.pending_input.drain(..self.input_offset);
            self.input_offset = 0;
        }
        Ok(self.input_offset < self.pending_input.len())
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> bool {
        if !self.queue_input(bytes) {
            return false;
        }
        let _ = self.flush_input();
        true
    }

    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        if self.exit.is_some() {
            return self.exit;
        }
        let status = self.pty.try_wait();
        if let Some(status) = status {
            self.exit = Some(status);
            self.dead = true;
        }
        status
    }

    pub fn terminate(&mut self) {
        if self.exit.is_some() {
            return;
        }
        if self.kill_deadline.is_none() {
            self.kill_deadline = Some(Instant::now() + Duration::from_millis(1200));
        }
        self.pty.terminate();
    }

    pub fn force_kill(&mut self) {
        self.pty.force_kill();
    }

    pub fn check_kill_deadline(&mut self) {
        if let Some(deadline) = self.kill_deadline {
            if Instant::now() >= deadline && self.exit.is_none() {
                self.pty.force_kill();
                self.kill_deadline = Some(Instant::now() + Duration::from_millis(600));
            }
        }
    }

    pub fn mouse_mode(&self) -> MouseMode {
        self.terminal.screen.modes.mouse
    }

    pub fn app_cursor(&self) -> bool {
        self.terminal.screen.modes.app_cursor
    }

    pub fn app_keypad(&self) -> bool {
        self.terminal.screen.modes.app_keypad
    }

    pub fn bracketed_paste(&self) -> bool {
        self.terminal.screen.modes.bracketed_paste
    }

    pub fn focus_reporting(&self) -> bool {
        self.terminal.screen.modes.focus_events
    }

    pub fn alternate_scroll(&self) -> bool {
        self.terminal.screen.modes.alternate_scroll
    }

    pub fn scrollback_len(&self) -> usize {
        self.terminal.screen.active().scrollback.len()
    }

    pub fn live_cursor(&self) -> (usize, usize) {
        let b = self.terminal.screen.active();
        (b.cursor.row, b.cursor.col)
    }

    /// Clone the visible rows + scrollback for copy mode, freezing the view.
    pub fn snapshot(&self) -> crate::server::copy::CopySnapshot {
        let b = self.terminal.screen.active();
        let mut rows = Vec::with_capacity(b.total_rows());
        for i in 0..b.total_rows() {
            if let Some(cells) = b.virtual_row(i) {
                rows.push(crate::term::Row {
                    cells: cells.to_vec(),
                    wrapped: b.virtual_wrapped(i),
                });
            }
        }
        crate::server::copy::CopySnapshot {
            cols: b.cols,
            screen_rows: b.rows,
            rows,
            cursor: b.cursor,
            title: self.title.clone(),
            reverse: self.terminal.screen.modes.reverse_video,
        }
    }
}
