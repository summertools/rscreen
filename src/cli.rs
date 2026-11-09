//! Command line handling and session management.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::client::{self, AttachOptions, AttachOutcome};
use crate::config::{Config, SAMPLE_CONFIG};
use crate::paths;
use crate::protocol::{read_msg_timeout, write_msg, ClientMsg, ServerMsg, SessionInfo};
use crate::server::session::{run_server, ServerOptions};
use crate::util;

#[derive(Parser, Debug)]
#[command(
    name = "rscreen",
    version,
    about = "A modern, elegant terminal multiplexer inspired by GNU Screen",
    long_about = "rscreen keeps your shells alive across disconnects. It adds a polished status bar, splits, copy mode, search and a command palette to the classic screen workflow.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Session name (create or attach, depending on the command)
    #[arg(short = 'S', long = "session", global = true)]
    pub session: Option<String>,

    /// Path to the config file (defaults to ~/.config/rscreen/config.toml)
    #[arg(long = "config", global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new session and attach to it
    New {
        /// Session name (a friendly name is generated when omitted)
        name: Option<String>,
        /// Create the session and return without attaching
        #[arg(short = 'd', long = "detached")]
        detached: bool,
    },
    /// Attach to an existing session
    #[command(alias = "resume")]
    Attach { name: Option<String> },
    /// List sessions
    #[command(alias = "list")]
    Ls,
    /// Kill a session and all of its processes
    Kill { name: Option<String> },
    /// Detach every client from a session, leaving it running
    Detach { name: Option<String> },
    /// Show or create the configuration file
    Config {
        /// Write the sample configuration file
        #[arg(long)]
        init: bool,
        /// Print the sample configuration to stdout
        #[arg(long)]
        print: bool,
        /// Overwrite an existing configuration with --init
        #[arg(long)]
        force: bool,
    },
    /// Internal: run the session server
    #[command(hide = true, name = "__server")]
    Server {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cwd: PathBuf,
    },
}

pub fn main_entry(cli: Cli) -> i32 {
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("rscreen: {}", util::terminal_safe(&format!("{:#}", err)));
            1
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    if let Some(path) = &cli.config {
        std::env::set_var("RSCREEN_CONFIG", path);
    }
    let command = match cli.command {
        Some(command) => command,
        None => return attach_or_create_bare(&cli),
    };
    paths::ensure_runtime_dir()?;
    match command {
        Command::Server { name, cwd } => {
            util::validate_session_name(&name).map_err(|e| anyhow!(e))?;
            paths::ensure_runtime_dir()?;
            let (config, warnings) = Config::load();
            for warning in warnings {
                eprintln!("rscreen: {}", util::terminal_safe(&warning));
            }
            let socket = paths::socket_path(&name);
            if !paths::socket_path_fits(&socket) {
                bail!(
                    "socket path is too long ({}); set RSCREEN_RUNTIME_DIR to a shorter path",
                    socket.display()
                );
            }
            run_server(ServerOptions {
                name,
                socket,
                config,
                cwd,
            })?;
            Ok(0)
        }
        Command::New { name, detached } => {
            if !detached && (unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1) {
                bail!("`rscreen new` attaches to the new session; use --detached in scripts");
            }
            let name = match name.or_else(|| cli.session.clone()) {
                Some(name) => {
                    util::validate_session_name(&name).map_err(|e| anyhow!(e))?;
                    name
                }
                None => choose_new_name(None)?,
            };
            paths::ensure_runtime_dir()?;
            if live_info(&name).is_some() {
                bail!(
                    "session {:?} already exists; use `rscreen attach {}`",
                    name,
                    name
                );
            }
            create_session(&name)?;
            if detached {
                println!("{}", name);
                Ok(0)
            } else {
                attach_named(&name)
            }
        }
        Command::Attach { name } => {
            let name = name.or_else(|| cli.session.clone());
            let name = match name {
                Some(name) => name,
                None => resolve_single_session()?,
            };
            attach_named(&name)
        }
        Command::Ls => {
            list_sessions()?;
            Ok(0)
        }
        Command::Kill { name } => {
            let name = match name.or_else(|| cli.session.clone()) {
                Some(name) => name,
                None => resolve_single_session()?,
            };
            let mut stream = connect_existing(&name)?;
            write_msg(&mut stream, &ClientMsg::KillSession)?;
            let _ = read_msg_timeout::<ServerMsg>(&mut stream, Duration::from_millis(3000));
            // Wait for the socket to disappear so scripts can rely on it.
            let socket = paths::socket_path(&name);
            let deadline = Instant::now() + Duration::from_secs(2);
            while socket.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            println!("session {:?} killed", name);
            Ok(0)
        }
        Command::Detach { name } => {
            let name = match name.or_else(|| cli.session.clone()) {
                Some(name) => name,
                None => resolve_single_session()?,
            };
            let mut stream = connect_existing(&name)?;
            write_msg(&mut stream, &ClientMsg::Detach { all: true })?;
            let _ = read_msg_timeout::<ServerMsg>(&mut stream, Duration::from_millis(3000));
            println!("detached all clients from {:?}", name);
            Ok(0)
        }
        Command::Config { init, print, force } => {
            let path = paths::config_path();
            if print {
                print!("{}", SAMPLE_CONFIG);
                return Ok(0);
            }
            if init {
                if path.exists() && !force {
                    bail!(
                        "config already exists at {} (use --force to overwrite)",
                        path.display()
                    );
                }
                Config::default().write_sample(&path, force)?;
                println!("wrote {}", util::terminal_safe(&path.display().to_string()));
                return Ok(0);
            }
            println!("{}", util::terminal_safe(&path.display().to_string()));
            if path.exists() {
                println!("(exists — use `rscreen config --print` to see the sample)");
            } else {
                println!("(not found — use `rscreen config --init` to create it)");
            }
            Ok(0)
        }
    }
}

fn attach_or_create_bare(cli: &Cli) -> Result<i32> {
    if let Some(name) = &cli.session {
        util::validate_session_name(name).map_err(|e| anyhow!(e))?;
        if live_info(name).is_some() {
            return attach_named(name);
        }
        if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
            bail!("no live session {:?}; use `rscreen new --detached` to create one without a terminal", name);
        }
        create_session(name)?;
        return attach_named(name);
    }
    let sessions = live_sessions();
    match sessions.len() {
        0 => {
            if unsafe { libc::isatty(0) } != 1 || unsafe { libc::isatty(1) } != 1 {
                bail!("no sessions running (create one with `rscreen new --detached`)");
            }
            let name = choose_new_name(None)?;
            create_session(&name)?;
            attach_named(&name)
        }
        1 => attach_named(&sessions[0].name),
        _ => {
            eprintln!("multiple sessions are running; choose one:");
            print_table(&sessions);
            bail!(
                "specify a session name, e.g. `rscreen attach {}`",
                sessions[0].name
            )
        }
    }
}

fn choose_new_name(preferred: Option<String>) -> Result<String> {
    if let Some(name) = preferred {
        return Ok(name);
    }
    for _ in 0..32 {
        let name = util::random_session_name();
        if live_info(&name).is_none() && !paths::socket_path(&name).exists() {
            return Ok(name);
        }
    }
    Ok(format!(
        "session-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ))
}

fn attach_named(name: &str) -> Result<i32> {
    util::validate_session_name(name).map_err(|e| anyhow!(e))?;
    let socket = paths::socket_path(name);
    if !paths::socket_path_fits(&socket) {
        bail!("session socket path is too long: {}", socket.display());
    }
    if !socket.exists() {
        bail!("no such session {:?} (try `rscreen ls`)", name);
    }
    let (config, warnings) = Config::load();
    for warning in warnings {
        eprintln!("rscreen: {}", util::terminal_safe(&warning));
    }
    let outcome = client::attach(AttachOptions {
        socket,
        name: name.to_string(),
        config,
    })?;
    match outcome {
        AttachOutcome::Detached(message) => {
            println!("[{}]", util::terminal_safe(&message));
            Ok(0)
        }
        AttachOutcome::Ended(message) => {
            println!("[{}]", util::terminal_safe(&message));
            Ok(0)
        }
        AttachOutcome::Error(message) => {
            eprintln!("rscreen: {}", util::terminal_safe(&message));
            Ok(1)
        }
    }
}

fn resolve_single_session() -> Result<String> {
    let sessions = live_sessions();
    match sessions.len() {
        0 => bail!("no sessions running (create one with `rscreen new`)"),
        1 => Ok(sessions[0].name.clone()),
        _ => {
            eprintln!("multiple sessions are running; choose one:");
            print_table(&sessions);
            bail!(
                "specify a session name, e.g. `rscreen attach {}`",
                sessions[0].name
            )
        }
    }
}

fn live_sessions() -> Vec<SessionInfo> {
    let mut out = Vec::new();
    for name in paths::session_names() {
        if let Some(info) = live_info(&name) {
            out.push(info);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn live_info(name: &str) -> Option<SessionInfo> {
    query_session(name).ok().flatten()
}

fn query_session(name: &str) -> Result<Option<SessionInfo>> {
    if util::validate_session_name(name).is_err() {
        return Ok(None);
    }
    let socket = paths::socket_path(name);
    if !socket.exists() {
        return Ok(None);
    }
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            let _ = std::fs::remove_file(&socket);
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };
    write_msg(&mut stream, &ClientMsg::Query)?;
    loop {
        match read_msg_timeout::<ServerMsg>(&mut stream, Duration::from_millis(3000))? {
            Some(ServerMsg::Info(info)) => return Ok(Some(info)),
            Some(ServerMsg::Error(err)) => bail!(err),
            Some(_) => continue,
            None => return Ok(None),
        }
    }
}

fn connect_existing(name: &str) -> Result<UnixStream> {
    util::validate_session_name(name).map_err(|e| anyhow!(e))?;
    let socket = paths::socket_path(name);
    match UnixStream::connect(&socket) {
        Ok(stream) => Ok(stream),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            let _ = std::fs::remove_file(&socket);
            bail!("no live session {:?}", name)
        }
        Err(err) => Err(err.into()),
    }
}

/// A create lock prevents two clients from racing to spawn two servers for the
/// same session name.
struct CreationLock {
    _file: std::fs::File,
}

impl CreationLock {
    fn acquire(name: &str) -> Result<Self> {
        let path = paths::lock_path(name);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening lock file {}", path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            bail!(
                "session {:?} is already being created (or another rscreen process holds the lock)",
                name
            );
        }
        Ok(Self { _file: file })
    }
}

fn create_session(name: &str) -> Result<()> {
    let socket = paths::socket_path(name);
    if !paths::socket_path_fits(&socket) {
        bail!(
            "socket path is too long ({}); set RSCREEN_RUNTIME_DIR to a shorter path",
            socket.display()
        );
    }
    let _lock = CreationLock::acquire(name)?;
    // Re-check after taking the lock in case another client just won.
    if live_info(name).is_some() {
        bail!("session {:?} already exists", name);
    }
    // A timeout is not proof of death. Only remove a socket when the kernel
    // explicitly refuses the connection.
    if socket.exists() {
        match UnixStream::connect(&socket) {
            Ok(_) => bail!(
                "session {:?} is alive but did not answer a query; refusing to replace it",
                name
            ),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) =>
            {
                let _ = std::fs::remove_file(&socket);
            }
            Err(err) => bail!(
                "cannot verify existing session {:?} at {}: {}",
                name,
                socket.display(),
                err
            ),
        }
    }

    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| crate::pty::home_dir().unwrap_or_else(|| PathBuf::from("/")));
    let (mut child, log_path) = spawn_daemon(name, &cwd)?;

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(_info) = live_info(name) {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let tail = read_log_tail(&log_path);
            bail!(
                "session server exited during startup ({}){}\nlog: {}",
                status,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", tail)
                },
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            let tail = read_log_tail(&log_path);
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&socket);
            bail!(
                "timed out waiting for session {:?} to start{}\nlog: {}",
                name,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", tail)
                },
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn spawn_daemon(name: &str, cwd: &Path) -> Result<(Child, PathBuf)> {
    let exe = std::env::current_exe().context("locating the rscreen executable")?;
    let log_path = paths::log_path(name);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening log {}", log_path.display()))?;
    let mut command = ProcessCommand::new(exe);
    command
        .arg("__server")
        .arg("--name")
        .arg(name)
        .arg("--cwd")
        .arg(cwd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .env("RSCREEN_DAEMON", "1");
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().context("spawning the session server")?;
    Ok((child, log_path))
}

fn read_log_tail(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = file.take(8192).read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let lines: Vec<&str> = text.lines().rev().take(6).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

fn list_sessions() -> Result<()> {
    let sessions = live_sessions();
    if sessions.is_empty() {
        println!("No rscreen sessions.");
        return Ok(());
    }
    print_table(&sessions);
    Ok(())
}

fn print_table(sessions: &[SessionInfo]) {
    let tty = unsafe { libc::isatty(1) } == 1 && std::env::var_os("NO_COLOR").is_none();
    let (bold, dim, accent, reset) = if tty {
        ("\x1b[1m", "\x1b[2m", "\x1b[38;5;111m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };
    println!(
        "{bold}{:<22} {:>7} {:>6} {:>8} {:>9}  {:<10} STATUS{reset}",
        "NAME",
        "WINDOWS",
        "PANES",
        "CLIENTS",
        "SIZE",
        "CREATED",
        bold = bold,
        reset = reset
    );
    for info in sessions {
        let created = now_unix().saturating_sub(info.created);
        let status = if info.attached() {
            format!("attached ({})", info.clients)
        } else {
            "detached".to_string()
        };
        let status_colored = if info.attached() {
            format!("{}{}{}", accent, status, reset)
        } else {
            format!("{}{}{}", dim, status, reset)
        };
        println!(
            "{accent}{:<22}{reset} {:>7} {:>6} {:>8} {:>9}  {:<10} {}{}",
            util::truncate_display(&info.name, 21),
            info.windows,
            info.panes,
            info.clients,
            format!("{}x{}", info.cols, info.rows),
            util::time_ago(created),
            status_colored,
            if info.bell { "  ●" } else { "" },
            accent = accent,
            reset = reset
        );
    }
    println!(
        "{}Tip: attach with `rscreen attach NAME` · detach with the prefix key + d{}",
        dim, reset
    );
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
