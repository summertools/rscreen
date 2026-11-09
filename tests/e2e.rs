#![allow(clippy::field_reassign_with_default)]

//! End-to-end tests: spawn the session server, speak the real wire protocol,
//! send keys and verify what a terminal would actually display.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rscreen::config::Config;
use rscreen::protocol::{read_msg_timeout, write_msg, ClientMsg, ServerMsg, PROTOCOL_VERSION};
use rscreen::server::session::{run_server, ServerOptions};
use rscreen::term::Terminal;

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct TestServer {
    name: String,
    socket: PathBuf,
    _dir: tempfile::TempDir,
    handle: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

impl TestServer {
    fn start() -> Self {
        let mut config = Config::default();
        config.confirm_kill = false;
        Self::start_with(config)
    }

    fn start_with(mut config: Config) -> Self {
        if config.shell.is_empty() {
            config.shell = "/bin/sh".into();
        }
        if config.shell_args.is_empty() {
            config.shell_args = vec![
                "-c".into(),
                "printf 'READY\\n'; while IFS= read -r line; do printf 'GOT:%s\\n' \"$line\"; done"
                    .into(),
            ];
        }
        config.scrollback = 1000;
        let dir = tempfile::tempdir().unwrap();
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("test-{}", id);
        let socket = dir.path().join(format!("{}.sock", name));
        let cwd = std::env::current_dir().unwrap();
        let opts = ServerOptions {
            name: name.clone(),
            socket: socket.clone(),
            config,
            cwd,
        };
        let handle = std::thread::spawn(move || run_server(opts));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            if Instant::now() > deadline {
                panic!("server did not bind socket");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            name,
            socket,
            _dir: dir,
            handle: Some(handle),
        }
    }

    fn client(&self) -> TestClient {
        TestClient::connect(&self.socket)
    }

    fn info(&self) -> rscreen::protocol::SessionInfo {
        let mut stream = UnixStream::connect(&self.socket).unwrap();
        write_msg(&mut stream, &ClientMsg::Query).unwrap();
        loop {
            match read_msg_timeout::<ServerMsg>(&mut stream, Duration::from_secs(1)).unwrap() {
                Some(ServerMsg::Info(info)) => return info,
                Some(_) => continue,
                None => panic!("no info"),
            }
        }
    }

    fn shutdown(&mut self) {
        if let Ok(mut stream) = UnixStream::connect(&self.socket) {
            let _ = write_msg(&mut stream, &ClientMsg::KillSession);
            let _ = read_msg_timeout::<ServerMsg>(&mut stream, Duration::from_millis(500));
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct TestClient {
    stream: UnixStream,
    terminal: Terminal,
    width: u16,
    height: u16,
    raw: Vec<Vec<u8>>,
}

impl TestClient {
    fn connect(socket: &std::path::Path) -> Self {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        write_msg(
            &mut stream,
            &ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                cols: 80,
                rows: 24,
                caps: rscreen::protocol::Caps { truecolor: true },
            },
        )
        .unwrap();
        let mut terminal = Terminal::new(80, 24, 5000);
        // Welcome must arrive first.
        loop {
            match read_msg_timeout::<ServerMsg>(&mut stream, Duration::from_secs(2)).unwrap() {
                Some(ServerMsg::Welcome { .. }) => {
                    break;
                }
                Some(ServerMsg::Frame(frame)) => terminal.feed(&frame),
                Some(_) => continue,
                None => panic!("no welcome"),
            }
        }
        let mut client = Self {
            stream,
            terminal,
            width: 80,
            height: 24,
            raw: Vec::new(),
        };
        client.pump(300);
        client
    }

    fn pump(&mut self, millis: u64) {
        let deadline = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < deadline {
            match read_msg_timeout::<ServerMsg>(&mut self.stream, Duration::from_millis(30)) {
                Ok(Some(ServerMsg::Frame(frame))) => self.terminal.feed(&frame),
                Ok(Some(ServerMsg::Welcome { .. })) | Ok(Some(ServerMsg::Info(_))) => {}
                Ok(Some(ServerMsg::Raw(bytes))) => self.raw.push(bytes),
                Ok(Some(ServerMsg::Exit { .. })) => break,
                Ok(Some(ServerMsg::Error(err))) => panic!("server error: {}", err),
                Ok(None) => break,
                Err(_) => {}
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        write_msg(&mut self.stream, &ClientMsg::Input(bytes.to_vec())).unwrap();
    }

    fn send_str(&mut self, text: &str) {
        self.send(text.as_bytes());
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.width = cols;
        self.height = rows;
        write_msg(&mut self.stream, &ClientMsg::Resize { cols, rows }).unwrap();
        self.terminal.resize(cols as usize, rows as usize);
        self.pump(200);
    }

    fn text(&self) -> String {
        self.terminal.screen.dump()
    }

    fn has(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }

    fn wait_for(&mut self, needle: &str, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            self.pump(60);
            if self.has(needle) {
                return true;
            }
        }
        self.has(needle)
    }

    fn wait_until<F: Fn(&str) -> bool>(&mut self, f: F, timeout_ms: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            self.pump(60);
            if f(&self.text()) {
                return true;
            }
        }
        f(&self.text())
    }

    fn detach(&mut self) {
        self.send(b"\x01d");
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            match read_msg_timeout::<ServerMsg>(&mut self.stream, Duration::from_millis(100)) {
                Ok(Some(ServerMsg::Exit { .. })) => return,
                Ok(Some(ServerMsg::Frame(frame))) => self.terminal.feed(&frame),
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(_) => {}
            }
        }
    }
}

#[test]
fn frame_shows_shell_and_status_bar() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000), "screen:\n{}", client.text());
    assert!(
        client.has(&server.name),
        "session name missing:\n{}",
        client.text()
    );
    assert!(
        client.has("1 pane"),
        "pane count missing:\n{}",
        client.text()
    );
    assert!(client.has("READY"));
    client.send_str("ping\r");
    assert!(
        client.wait_for("GOT:ping", 3000),
        "echo missing:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn split_panes_and_zoom() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01|");
    assert!(
        client.wait_until(|t| t.contains("2 panes"), 3000),
        "no split:\n{}",
        client.text()
    );
    assert_eq!(server.info().panes, 2);
    assert!(client.has("│") || client.has("├") || client.has("┤"));
    client.send(b"\x01z");
    assert!(
        client.wait_until(|t| t.contains("·  zoom") || t.contains("ZOOM"), 2000),
        "zoom badge missing:\n{}",
        client.text()
    );
    client.send(b"\x01z");
    assert!(
        client.wait_until(|t| !t.contains("·  zoom") && !t.contains("ZOOM"), 2000),
        "zoom did not clear:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn command_palette_creates_split() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01:");
    assert!(
        client.wait_for("Command palette", 2000),
        "palette missing:\n{}",
        client.text()
    );
    client.send_str("split");
    client.pump(200);
    client.send(b"\r");
    assert!(
        client.wait_until(|t| t.contains("2 panes"), 3000),
        "palette action failed:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn rename_window_via_prompt() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01A");
    assert!(client.wait_for("Rename window", 2000), "{}", client.text());
    // Select all of the prefilled title via ctrl-u, then type a new title.
    client.send(b"\x15");
    client.send_str("my-shell\r");
    assert!(
        client.wait_for("my-shell", 2500),
        "rename missing:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn copy_mode_enters_and_exits() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01[");
    assert!(
        client.wait_until(|t| t.contains("COPY"), 2000),
        "copy badge missing:\n{}",
        client.text()
    );
    client.send(b"\x1b[5~"); // PageUp
    client.pump(150);
    client.send(b"q");
    assert!(
        client.wait_until(|t| !t.contains("COPY"), 2000),
        "copy mode did not exit:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn mouse_wheel_enters_copy_mode() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x1b[<64;10;5M");
    assert!(
        client.wait_until(|t| t.contains("COPY"), 2000),
        "wheel did not enter copy mode:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn resize_reflows_frame() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.resize(100, 30);
    assert!(client.wait_for("READY", 3000), "lost content after resize");
    assert!(client.has("100x30") || client.has("1 pane"));
    let info = server.info();
    assert_eq!(info.cols, 100);
    assert_eq!(info.rows, 30);
    client.detach();
}

#[test]
fn help_overlay_opens_and_closes() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01?");
    assert!(
        client.wait_for("Keys & shortcuts", 2000),
        "help missing:\n{}",
        client.text()
    );
    client.send(b"\x1b");
    client.pump(150);
    assert!(
        client.wait_until(|t| !t.contains("Keys & shortcuts"), 2000),
        "help did not close:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn copy_broadcasts_clipboard_without_corrupting_protocol() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01[");
    assert!(
        client.wait_until(|t| t.contains("COPY"), 2000),
        "copy mode missing:\n{}",
        client.text()
    );
    client.send(b"\x1b[A"); // move up from the empty prompt line to READY
    client.send(b"y");
    let copied = client.wait_for("Copied", 1500);
    let saw_clipboard = client
        .raw
        .iter()
        .any(|bytes| bytes.windows(4).any(|w| w == b"\x1b]52"));
    assert!(copied, "copy toast missing:\n{}", client.text());
    assert!(saw_clipboard, "clipboard OSC 52 was not broadcast");
    client.send_str("ping\r");
    assert!(
        client.wait_for("GOT:ping", 3000),
        "protocol corrupted after clipboard:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn mouse_drag_selects_and_copies_text() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    // Press at column 1, row 1 (READY), drag to column 5, release.
    client.send(b"\x1b[<0;1;1M");
    client.send(b"\x1b[<32;5;1M");
    client.send(b"\x1b[<0;5;1m");
    let copied = client.wait_for("Copied", 2000);
    let clipboard = client.raw.iter().any(|bytes| {
        // base64("READY") == UkVBRFk=
        bytes.windows(8).any(|w| w == b"UkVBRFk=")
    });
    assert!(copied, "drag did not copy:\n{}", client.text());
    assert!(clipboard, "READY was not placed on the clipboard");
    client.detach();
}

#[test]
fn create_switch_and_close_windows() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01c");
    assert!(
        client.wait_until(|t| t.contains("2 windows") || t.contains("2 panes"), 2000)
            || server.info().windows == 2,
        "second window was not created"
    );
    assert_eq!(server.info().windows, 2);
    client.send(b"\x01n");
    client.pump(200);
    client.send(b"\x01k"); // confirm_kill is disabled in tests
    assert!(
        client.wait_until(|_| server.info().windows == 1, 3000),
        "window was not killed"
    );
    client.detach();
}

#[test]
fn close_pane_keeps_window_alive() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01|");
    assert!(client.wait_until(|t| t.contains("2 panes"), 3000));
    client.send(b"\x01x");
    assert!(
        client.wait_until(|t| t.contains("1 pane") && !t.contains("2 panes"), 3000),
        "pane was not closed:\n{}",
        client.text()
    );
    assert_eq!(server.info().windows, 1);
    assert_eq!(server.info().panes, 1);
    client.detach();
}

#[test]
fn two_row_status_bar_shows_hints() {
    let mut config = Config::default();
    config.status_rows = 2;
    let server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    assert!(
        client.wait_for("new", 2000),
        "hint row missing:\n{}",
        client.text()
    );
    // The status bar occupies the last two rows.
    let text = client.text();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 23);
    client.detach();
}

#[test]
fn prefix_strip_appears_while_waiting() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01");
    assert!(
        client.wait_until(|t| t.contains("PREFIX"), 2000),
        "prefix hint strip missing:\n{}",
        client.text()
    );
    client.send(b"c"); // prefix + c -> new window, consumes prefix
    assert!(
        client.wait_until(|_| server.info().windows == 2, 3000),
        "prefix action not executed"
    );
    client.detach();
}

#[test]
fn copy_search_finds_a_match() {
    let mut config = Config::default();
    config.confirm_kill = false;
    config.shell = "/bin/sh".into();
    config.shell_args = vec![
        "-c".into(),
        "printf 'alpha\nbeta\nNEEDLE in the haystack\ngamma\n'; while IFS= read -r line; do :; done"
            .into(),
    ];
    let server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(client.wait_for("NEEDLE", 3000));
    client.send(b"\x01[");
    assert!(client.wait_until(|t| t.contains("COPY"), 2000));
    client.send(b"/");
    assert!(
        client.wait_for("Search forward", 2000),
        "search prompt missing:\n{}",
        client.text()
    );
    client.send_str("needle\r");
    assert!(
        client.wait_until(|t| t.contains("/needle") && t.contains("1/1"), 3000),
        "search did not find the match:\n{}",
        client.text()
    );
    client.send(b"q");
    client.pump(100);
    client.detach();
}

#[test]
fn confirm_dialog_cancel_and_confirm() {
    let mut config = Config::default();
    config.confirm_kill = true;
    let server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01c"); // second window
    assert!(client.wait_until(|_| server.info().windows == 2, 2000));
    client.send(b"\x01k");
    assert!(client.wait_for("Confirm", 2000), "{}", client.text());
    client.send(b"n");
    assert!(
        client.wait_until(|t| !t.contains("Confirm"), 2000),
        "cancel did not close dialog"
    );
    assert_eq!(server.info().windows, 2);
    client.send(b"\x01k");
    assert!(client.wait_for("Confirm", 2000));
    client.send(b"y");
    assert!(
        client.wait_until(|_| server.info().windows == 1, 3000),
        "confirm did not close the window"
    );
    client.detach();
}

#[test]
fn app_clipboard_osc52_is_forwarded() {
    let mut config = Config::default();
    config.confirm_kill = false;
    config.shell = "/bin/sh".into();
    config.shell_args = vec![
        "-c".into(),
        "printf 'READY\\n'; while IFS= read -r line; do printf '\\033]52;c;aGVsbG8=\\007hello\\n'; done".into(),
    ];
    let server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send_str("go\r");
    assert!(client.wait_for("hello", 3000));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw = false;
    while Instant::now() < deadline && !saw {
        client.pump(100);
        saw = client
            .raw
            .iter()
            .any(|bytes| bytes.windows(4).any(|w| w == b"\x1b]52"));
    }
    assert!(saw, "OSC 52 from the app was not forwarded");
    client.detach();
}

#[test]
fn tiny_resizes_do_not_crash_or_disconnect() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    for (cols, rows) in [(20, 4), (10, 2), (1, 1), (80, 24)] {
        client.resize(cols, rows);
        client.pump(100);
        let _ = server.info();
    }
    assert!(
        client.wait_until(|t| t.contains(&server.name), 2000),
        "client stopped receiving valid frames after tiny resizes"
    );
    client.detach();
}

#[test]
fn detach_and_reattach_keeps_screen_history() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send_str("remember-me\r");
    assert!(client.wait_for("GOT:remember-me", 3000));
    client.detach();
    let mut again = server.client();
    assert!(
        again.wait_for("GOT:remember-me", 3000),
        "screen history lost after reattach:\n{}",
        again.text()
    );
    again.detach();
}

#[test]
fn clicking_status_tabs_switches_windows() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01c");
    assert!(client.wait_until(|_| server.info().windows == 2, 2000));
    client.send_str("WINDOW_TWO\r");
    assert!(client.wait_for("GOT:WINDOW_TWO", 3000));
    // Click the first tab: brand " ◆ " + "test-N" + two spaces = tab starts at 11.
    client.send(b"\x1b[<0;13;24M");
    assert!(
        client.wait_until(|t| !t.contains("GOT:WINDOW_TWO"), 2500),
        "clicking tab 1 did not switch windows:\n{}",
        client.text()
    );
    client.send(b"\x1b[<0;19;24M");
    assert!(
        client.wait_until(|t| t.contains("GOT:WINDOW_TWO"), 2500),
        "clicking tab 2 did not switch back:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn final_output_is_rendered_before_session_exit() {
    let mut config = Config::default();
    config.confirm_kill = false;
    config.shell = "/bin/sh".into();
    config.shell_args = vec![
        "-c".into(),
        "printf 'READY\n'; sleep 0.4; printf 'FINAL_OUTPUT\n'".into(),
    ];
    let server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_output = client.text().contains("FINAL_OUTPUT");
    let mut saw_exit = false;
    while Instant::now() < deadline {
        match read_msg_timeout::<ServerMsg>(&mut client.stream, Duration::from_millis(100)) {
            Ok(Some(ServerMsg::Frame(frame))) => {
                client.terminal.feed(&frame);
                if client.text().contains("FINAL_OUTPUT") {
                    saw_output = true;
                }
            }
            Ok(Some(ServerMsg::Exit { .. })) => {
                saw_exit = true;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }
    assert!(saw_output, "final output was lost:\n{}", client.text());
    assert!(saw_exit, "session did not exit");
}

#[test]
fn window_picker_switches() {
    let server = TestServer::start();
    let mut client = server.client();
    assert!(client.wait_for("READY", 3000));
    client.send(b"\x01c");
    assert!(client.wait_until(|_| server.info().windows == 2, 2000));
    client.send_str("WINDOW_TWO\r");
    assert!(client.wait_for("GOT:WINDOW_TWO", 3000));
    client.send(b"\x01w");
    assert!(
        client.wait_for("Switch window", 2000),
        "picker missing:\n{}",
        client.text()
    );
    // The picker preselects the active window; move up to window 1.
    client.send(b"\x1b[A");
    client.send(b"\r");
    assert!(
        client.wait_until(|t| !t.contains("GOT:WINDOW_TWO"), 2500),
        "picker did not switch to the first window:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn osc_title_updates_the_window_tab() {
    let mut config = Config::default();
    config.confirm_kill = false;
    config.shell = "/bin/sh".into();
    config.shell_args = vec![
        "-c".into(),
        "printf '\\033]2;My Window Title\\007READY\\n'; sleep 2".into(),
    ];
    let server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(
        client.wait_for("My Window Title", 3000),
        "title missing:\n{}",
        client.text()
    );
    client.detach();
}

#[test]
fn many_clients_attach_and_detach() {
    let server = TestServer::start();
    let mut a = server.client();
    assert!(a.wait_for("READY", 3000));
    let mut b = server.client();
    assert!(b.wait_for("READY", 3000));
    assert_eq!(server.info().clients, 2);
    b.detach();
    assert_eq!(server.info().clients, 1);
    a.detach();
}

#[test]
fn process_exit_ends_session() {
    let mut config = Config::default();
    config.shell = "/bin/sh".into();
    config.shell_args = vec!["-c".into(), "printf 'BYE\\n'; sleep 1.0".into()];
    let mut server = TestServer::start_with(config);
    let mut client = server.client();
    assert!(client.wait_for("BYE", 3000));
    // The last pane exits, so the client should receive Exit.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut exited = false;
    while Instant::now() < deadline {
        match read_msg_timeout::<ServerMsg>(&mut client.stream, Duration::from_millis(100)) {
            Ok(Some(ServerMsg::Exit { reason, .. })) => {
                assert!(reason.contains("ended") || reason.contains("exited"));
                exited = true;
                break;
            }
            Ok(Some(ServerMsg::Frame(frame))) => client.terminal.feed(&frame),
            Ok(Some(ServerMsg::Raw(bytes))) => client.raw.push(bytes),
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }
    assert!(exited, "no exit message; screen:\n{}", client.text());
    server.shutdown();
}

#[allow(dead_code)]
fn _unused() {
    let _ = Command::new("true").status();
}
