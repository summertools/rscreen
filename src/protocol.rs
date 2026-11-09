//! Length-prefixed binary protocol between the thin client and the session
//! server. Frames contain fully composed ANSI updates, so the client stays
//! deliberately dumb: it forwards input and paints bytes.
//!
//! The wire format is intentionally small and explicit instead of relying on a
//! generic serialization format. Every length is checked against the remaining
//! frame and hard limits before allocating, and unknown tags/trailing bytes are
//! rejected. This keeps malformed clients from turning the server into an
//! allocation or parsing toy.

use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME: usize = 32 * 1024 * 1024;
const MAX_STRING: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    pub truecolor: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self { truecolor: true }
    }
}

#[derive(Debug)]
pub enum ClientMsg {
    Hello {
        version: u32,
        cols: u16,
        rows: u16,
        caps: Caps,
    },
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    Detach {
        all: bool,
    },
    Query,
    KillSession,
}

#[derive(Debug)]
pub enum ServerMsg {
    Welcome {
        name: String,
        version: u32,
        cols: u16,
        rows: u16,
        status_rows: u16,
        prefix_label: String,
    },
    Frame(Vec<u8>),
    /// Raw terminal control bytes that are not part of the composed frame
    /// (clipboard OSC 52, audible bell). Still framed on the socket.
    Raw(Vec<u8>),
    Info(SessionInfo),
    Exit {
        reason: String,
        code: i32,
    },
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub pid: i32,
    pub created: u64,
    pub windows: usize,
    pub panes: usize,
    pub clients: usize,
    pub cols: u16,
    pub rows: u16,
    pub bell: bool,
}

impl SessionInfo {
    pub fn attached(&self) -> bool {
        self.clients > 0
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    Malformed(&'static str),
    TooLarge(usize),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(e) => write!(f, "{}", e),
            ProtocolError::Malformed(reason) => write!(f, "protocol error: {}", reason),
            ProtocolError::TooLarge(n) => write!(f, "frame too large ({} bytes)", n),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        ProtocolError::Io(e)
    }
}

/// A message that can be encoded into / decoded from one protocol frame.
pub trait WireMessage: Sized {
    fn encode_into(&self, out: &mut Vec<u8>);
    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError>;
}

// ---------------------------------------------------------------------------
// Low-level encoding
// ---------------------------------------------------------------------------

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bool(out: &mut Vec<u8>, value: bool) {
    put_u8(out, value as u8);
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    let mut len = bytes.len().min(MAX_STRING);
    while !value.is_char_boundary(len) {
        len -= 1;
    }
    put_u32(out, len as u32);
    out.extend_from_slice(&bytes[..len]);
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    let len = value.len().min(u32::MAX as usize);
    put_u32(out, len as u32);
    out.extend_from_slice(&value[..len]);
}

struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if n > self.remaining() {
            return Err(ProtocolError::Malformed("truncated frame"));
        }
        let start = self.pos;
        self.pos += n;
        Ok(&self.bytes[start..self.pos])
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ProtocolError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn bool(&mut self) -> Result<bool, ProtocolError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtocolError::Malformed("invalid boolean")),
        }
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        let len = self.u32()? as usize;
        if len > MAX_STRING {
            return Err(ProtocolError::Malformed("string too large"));
        }
        let bytes = self.take(len)?;
        let text = std::str::from_utf8(bytes)
            .map_err(|_| ProtocolError::Malformed("invalid UTF-8 string"))?;
        Ok(text.to_owned())
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ProtocolError> {
        let len = self.u32()? as usize;
        if len > MAX_FRAME {
            return Err(ProtocolError::Malformed("byte string too large"));
        }
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> Result<(), ProtocolError> {
        if self.pos != self.bytes.len() {
            Err(ProtocolError::Malformed("trailing bytes"))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Client messages
// ---------------------------------------------------------------------------

impl WireMessage for ClientMsg {
    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            ClientMsg::Hello {
                version,
                cols,
                rows,
                caps,
            } => {
                put_u8(out, 0);
                put_u32(out, *version);
                put_u16(out, *cols);
                put_u16(out, *rows);
                put_bool(out, caps.truecolor);
            }
            ClientMsg::Input(data) => {
                put_u8(out, 1);
                put_bytes(out, data);
            }
            ClientMsg::Resize { cols, rows } => {
                put_u8(out, 2);
                put_u16(out, *cols);
                put_u16(out, *rows);
            }
            ClientMsg::Detach { all } => {
                put_u8(out, 3);
                put_bool(out, *all);
            }
            ClientMsg::Query => put_u8(out, 4),
            ClientMsg::KillSession => put_u8(out, 5),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut d = Decoder::new(bytes);
        let msg = match d.u8()? {
            0 => ClientMsg::Hello {
                version: d.u32()?,
                cols: d.u16()?,
                rows: d.u16()?,
                caps: Caps {
                    truecolor: d.bool()?,
                },
            },
            1 => ClientMsg::Input(d.bytes()?),
            2 => ClientMsg::Resize {
                cols: d.u16()?,
                rows: d.u16()?,
            },
            3 => ClientMsg::Detach { all: d.bool()? },
            4 => ClientMsg::Query,
            5 => ClientMsg::KillSession,
            _ => return Err(ProtocolError::Malformed("unknown client message")),
        };
        d.finish()?;
        Ok(msg)
    }
}

// ---------------------------------------------------------------------------
// Server messages
// ---------------------------------------------------------------------------

impl WireMessage for ServerMsg {
    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            ServerMsg::Welcome {
                name,
                version,
                cols,
                rows,
                status_rows,
                prefix_label,
            } => {
                put_u8(out, 0);
                put_string(out, name);
                put_u32(out, *version);
                put_u16(out, *cols);
                put_u16(out, *rows);
                put_u16(out, *status_rows);
                put_string(out, prefix_label);
            }
            ServerMsg::Frame(data) => {
                put_u8(out, 1);
                put_bytes(out, data);
            }
            ServerMsg::Raw(data) => {
                put_u8(out, 2);
                put_bytes(out, data);
            }
            ServerMsg::Info(info) => {
                put_u8(out, 3);
                put_string(out, &info.name);
                put_i32(out, info.pid);
                put_u64(out, info.created);
                put_u64(out, info.windows as u64);
                put_u64(out, info.panes as u64);
                put_u64(out, info.clients as u64);
                put_u16(out, info.cols);
                put_u16(out, info.rows);
                put_bool(out, info.bell);
            }
            ServerMsg::Exit { reason, code } => {
                put_u8(out, 4);
                put_string(out, reason);
                put_i32(out, *code);
            }
            ServerMsg::Error(message) => {
                put_u8(out, 5);
                put_string(out, message);
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut d = Decoder::new(bytes);
        let msg = match d.u8()? {
            0 => ServerMsg::Welcome {
                name: d.string()?,
                version: d.u32()?,
                cols: d.u16()?,
                rows: d.u16()?,
                status_rows: d.u16()?,
                prefix_label: d.string()?,
            },
            1 => ServerMsg::Frame(d.bytes()?),
            2 => ServerMsg::Raw(d.bytes()?),
            3 => ServerMsg::Info(SessionInfo {
                name: d.string()?,
                pid: d.i32()?,
                created: d.u64()?,
                windows: usize::try_from(d.u64()?)
                    .map_err(|_| ProtocolError::Malformed("count out of range"))?,
                panes: usize::try_from(d.u64()?)
                    .map_err(|_| ProtocolError::Malformed("count out of range"))?,
                clients: usize::try_from(d.u64()?)
                    .map_err(|_| ProtocolError::Malformed("count out of range"))?,
                cols: d.u16()?,
                rows: d.u16()?,
                bell: d.bool()?,
            }),
            4 => ServerMsg::Exit {
                reason: d.string()?,
                code: d.i32()?,
            },
            5 => ServerMsg::Error(d.string()?),
            _ => return Err(ProtocolError::Malformed("unknown server message")),
        };
        d.finish()?;
        Ok(msg)
    }
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Incremental decoder for length-prefixed messages.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
    off: usize,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        if self.buffered().saturating_add(data.len()) > MAX_FRAME + 4 {
            return Err(ProtocolError::TooLarge(self.buffered() + data.len()));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    pub fn next_message<T: WireMessage>(&mut self) -> Result<Option<T>, ProtocolError> {
        if self.buffered() < 4 {
            return Ok(None);
        }
        let start = self.off;
        let len = u32::from_le_bytes([
            self.buf[start],
            self.buf[start + 1],
            self.buf[start + 2],
            self.buf[start + 3],
        ]) as usize;
        if len > MAX_FRAME {
            return Err(ProtocolError::TooLarge(len));
        }
        if self.buffered() < 4 + len {
            return Ok(None);
        }
        let msg = T::decode(&self.buf[start + 4..start + 4 + len])?;
        self.off += 4 + len;
        if self.off == self.buf.len() {
            self.buf.clear();
            self.off = 0;
        } else if self.off >= 256 * 1024 {
            self.buf.drain(..self.off);
            self.off = 0;
        }
        Ok(Some(msg))
    }

    pub fn buffered(&self) -> usize {
        self.buf.len().saturating_sub(self.off)
    }
}

pub fn encode<T: WireMessage>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut payload = Vec::new();
    msg.encode_into(&mut payload);
    if payload.len() > MAX_FRAME {
        return Err(ProtocolError::TooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn write_msg<W: Write, T: WireMessage>(writer: &mut W, msg: &T) -> io::Result<()> {
    let bytes =
        encode(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn read_msg<R: Read, T: WireMessage>(reader: &mut R) -> Result<Option<T>, ProtocolError> {
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtocolError::Io(e)),
    }
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME {
        return Err(ProtocolError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(T::decode(&payload)?))
}

pub fn read_msg_timeout<T: WireMessage>(
    stream: &mut std::os::unix::net::UnixStream,
    timeout: std::time::Duration,
) -> Result<Option<T>, ProtocolError> {
    stream.set_read_timeout(Some(timeout))?;
    let result = read_msg(stream);
    let _ = stream.set_read_timeout(None);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_roundtrip(msg: ClientMsg) -> ClientMsg {
        let bytes = encode(&msg).unwrap();
        let mut reader = FrameReader::new();
        for chunk in bytes.chunks(3) {
            reader.push(chunk).unwrap();
        }
        match reader.next_message::<ClientMsg>().unwrap().unwrap() {
            ClientMsg::Hello {
                version,
                cols,
                rows,
                caps,
            } => ClientMsg::Hello {
                version,
                cols,
                rows,
                caps,
            },
            ClientMsg::Input(data) => ClientMsg::Input(data),
            ClientMsg::Resize { cols, rows } => ClientMsg::Resize { cols, rows },
            ClientMsg::Detach { all } => ClientMsg::Detach { all },
            ClientMsg::Query => ClientMsg::Query,
            ClientMsg::KillSession => ClientMsg::KillSession,
        }
    }

    #[test]
    fn roundtrip_client_messages() {
        let messages = [
            ClientMsg::Hello {
                version: PROTOCOL_VERSION,
                cols: 120,
                rows: 40,
                caps: Caps { truecolor: true },
            },
            ClientMsg::Input(b"hello\x1b[A".to_vec()),
            ClientMsg::Resize { cols: 99, rows: 31 },
            ClientMsg::Detach { all: true },
            ClientMsg::Query,
            ClientMsg::KillSession,
        ];
        for msg in messages {
            let expected = format!("{:?}", msg);
            let got = client_roundtrip(msg);
            assert_eq!(format!("{:?}", got), expected);
        }
    }

    #[test]
    fn roundtrip_server_messages() {
        let info = SessionInfo {
            name: "calm-fox".into(),
            pid: 42,
            created: 1_700_000_000,
            windows: 3,
            panes: 5,
            clients: 2,
            cols: 100,
            rows: 30,
            bell: true,
        };
        let messages = [
            ServerMsg::Welcome {
                name: "calm-fox".into(),
                version: PROTOCOL_VERSION,
                cols: 100,
                rows: 30,
                status_rows: 1,
                prefix_label: "C-a".into(),
            },
            ServerMsg::Frame(b"\x1b[31mframe\x1b[0m".to_vec()),
            ServerMsg::Raw(b"\x07".to_vec()),
            ServerMsg::Info(info.clone()),
            ServerMsg::Exit {
                reason: "detached".into(),
                code: 0,
            },
            ServerMsg::Error("bad frame".into()),
        ];
        for msg in messages {
            let bytes = encode(&msg).unwrap();
            let decoded = ServerMsg::decode(&bytes[4..]).unwrap();
            assert_eq!(format!("{:?}", decoded), format!("{:?}", msg));
        }
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(ClientMsg::decode(&[]).is_err());
        assert!(ClientMsg::decode(&[99]).is_err());
        assert!(ClientMsg::decode(&[0, 0, 0]).is_err());
        // Trailing bytes.
        assert!(ClientMsg::decode(&[4, 0]).is_err());
        // Invalid boolean for Detach.
        assert!(ClientMsg::decode(&[3, 2]).is_err());
        // Declared input length exceeds the payload.
        assert!(ClientMsg::decode(&[1, 0xff, 0xff, 0xff, 0xff]).is_err());
        // Invalid UTF-8.
        assert!(ServerMsg::decode(&[5, 1, 0, 0, 0, 0xff]).is_err());
        // Declared string length exceeds the payload.
        assert!(ServerMsg::decode(&[5, 5, 0, 0, 0, b'h']).is_err());
    }

    #[test]
    fn many_small_frames_are_consumed_without_quadratic_drain() {
        let mut bytes = Vec::new();
        for _ in 0..5000 {
            bytes.extend_from_slice(&encode(&ClientMsg::Query).unwrap());
        }
        let mut reader = FrameReader::new();
        reader.push(&bytes).unwrap();
        for _ in 0..5000 {
            assert!(matches!(
                reader.next_message::<ClientMsg>().unwrap(),
                Some(ClientMsg::Query)
            ));
        }
        assert_eq!(reader.buffered(), 0);
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut reader = FrameReader::new();
        reader.push(&u32::MAX.to_le_bytes()).unwrap();
        assert!(matches!(
            reader.next_message::<ClientMsg>(),
            Err(ProtocolError::TooLarge(_))
        ));
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut seed = 0x9e37_79b9u32;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..2000 {
            let len = (rng() % 64) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng() as u8).collect();
            let _ = ClientMsg::decode(&bytes);
            let _ = ServerMsg::decode(&bytes);
        }
    }
}
