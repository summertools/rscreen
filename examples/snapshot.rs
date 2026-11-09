//! Developer tool: attach to a session, replay a few key presses, and dump the
//! final rendered grid as JSON for the screenshot renderer.
//!
//! Usage:
//!   cargo run --example snapshot -- --socket /tmp/... --cols 100 --rows 30 \
//!       --send 017c --send 013a --out /tmp/frame.json

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use rscreen::protocol::{
    read_msg_timeout, write_msg, Caps, ClientMsg, ServerMsg, PROTOCOL_VERSION,
};
use rscreen::term::{Color, Terminal};

fn main() {
    let mut socket = None;
    let mut cols = 100u16;
    let mut rows = 30u16;
    let mut sends: Vec<Vec<u8>> = Vec::new();
    let mut out = String::from("/tmp/frame.json");
    let mut wait_ms = 900u64;
    let mut settle_ms = 0u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next(),
            "--cols" => cols = args.next().unwrap().parse().unwrap(),
            "--rows" => rows = args.next().unwrap().parse().unwrap(),
            "--send" => {
                let hex = args.next().unwrap();
                sends.push(decode_hex(&hex));
            }
            "--out" => out = args.next().unwrap(),
            "--wait" => wait_ms = args.next().unwrap().parse().unwrap(),
            "--settle" => settle_ms = args.next().unwrap().parse().unwrap(),
            _ => panic!("unknown argument {}", arg),
        }
    }
    let socket = socket.expect("--socket required");
    let mut stream = UnixStream::connect(&socket).unwrap();
    write_msg(
        &mut stream,
        &ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            cols,
            rows,
            caps: Caps { truecolor: true },
        },
    )
    .unwrap();
    let mut terminal = Terminal::new(cols as usize, rows as usize, 10_000);
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    while Instant::now() < deadline {
        pump(&mut stream, &mut terminal, 80);
    }
    for send in sends {
        write_msg(&mut stream, &ClientMsg::Input(send)).unwrap();
        let deadline = Instant::now() + Duration::from_millis(350);
        while Instant::now() < deadline {
            pump(&mut stream, &mut terminal, 80);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(settle_ms);
    while Instant::now() < deadline {
        pump(&mut stream, &mut terminal, 80);
    }
    let _ = write_msg(&mut stream, &ClientMsg::Detach { all: false });
    std::fs::write(&out, to_json(&terminal)).unwrap();
    eprintln!("wrote {}", out);
}

fn pump(stream: &mut UnixStream, terminal: &mut Terminal, millis: u64) {
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        match read_msg_timeout::<ServerMsg>(stream, Duration::from_millis(25)) {
            Ok(Some(ServerMsg::Frame(frame))) => terminal.feed(&frame),
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {}
        }
    }
}

fn decode_hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    s.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn color(c: Color) -> String {
    match c {
        Color::Default => "null".to_string(),
        Color::Indexed(n) => format!("\"idx:{}\"", n),
        Color::Rgb(r, g, b) => format!("\"#{:02x}{:02x}{:02x}\"", r, g, b),
    }
}

fn to_json(terminal: &Terminal) -> String {
    let screen = &terminal.screen;
    let buffer = screen.active();
    let mut out = String::new();
    out.push_str(&format!(
        "{{\"cols\":{},\"rows\":{},\"cursor\":",
        buffer.cols, buffer.rows
    ));
    if screen.modes.cursor_visible {
        out.push_str(&format!("[{},{}],", buffer.cursor.col, buffer.cursor.row));
    } else {
        out.push_str("null,");
    }
    out.push_str("\"cells\":[");
    for row in 0..buffer.rows {
        if row > 0 {
            out.push(',');
        }
        out.push('[');
        for col in 0..buffer.cols {
            if col > 0 {
                out.push(',');
            }
            let cell = buffer.get(row, col);
            if cell.cont {
                out.push('0');
                continue;
            }
            let mut extra = String::new();
            if let Some(e) = &cell.extra {
                for c in e.chars() {
                    extra.push_str(&format!("\\u{{{:x}}}", c as u32));
                }
            }
            out.push_str(&format!(
                "{{\"c\":\"{}\",\"x\":\"{}\",\"fg\":{},\"bg\":{},\"a\":{},\"w\":{}}}",
                escape(cell.ch),
                extra,
                color(cell.style.fg),
                color(cell.style.bg),
                cell.style.attrs,
                if cell.wide { 1 } else { 0 }
            ));
        }
        out.push(']');
    }
    out.push_str("]}");
    out
}

fn escape(c: char) -> String {
    match c {
        '"' => "\\\"".into(),
        '\\' => "\\\\".into(),
        c if (c as u32) < 0x20 => format!("\\u{{{:x}}}", c as u32),
        c => c.to_string(),
    }
}
