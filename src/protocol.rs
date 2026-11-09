//! Length-prefixed binary protocol between the thin client and the session
//! server. Frames contain fully composed ANSI updates, so the client stays
//! deliberately dumb: it forwards input and paints bytes.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Caps {
    pub truecolor: bool,
}

impl Default for Caps {
    fn default() -> Self {
        Self { truecolor: true }
    }
}

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    Codec(String),
    TooLarge(usize),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(e) => write!(f, "{}", e),
            ProtocolError::Codec(e) => write!(f, "protocol decode error: {}", e),
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

impl From<bincode::Error> for ProtocolError {
    fn from(e: bincode::Error) -> Self {
        ProtocolError::Codec(e.to_string())
    }
}

/// Incremental decoder for length-prefixed messages.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) -> Result<(), ProtocolError> {
        if self.buf.len() + data.len() > MAX_FRAME + 4 {
            return Err(ProtocolError::TooLarge(self.buf.len() + data.len()));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(ProtocolError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let payload: Vec<u8> = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        let msg = bincode::deserialize(&payload)?;
        Ok(Some(msg))
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = bincode::serialize(msg)?;
    if payload.len() > MAX_FRAME {
        return Err(ProtocolError::TooLarge(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn write_msg<W: Write, T: Serialize>(writer: &mut W, msg: &T) -> io::Result<()> {
    let bytes =
        encode(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn read_msg<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<Option<T>, ProtocolError> {
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
    let msg = bincode::deserialize(&payload)?;
    Ok(Some(msg))
}

pub fn read_msg_timeout<T: DeserializeOwned>(
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

    #[test]
    fn roundtrip_messages() {
        let msg = ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            cols: 80,
            rows: 24,
            caps: Caps { truecolor: true },
        };
        let bytes = encode(&msg).unwrap();
        let mut reader = FrameReader::new();
        // Feed in tiny chunks to exercise partial frames.
        for chunk in bytes.chunks(3) {
            reader.push(chunk).unwrap();
        }
        match reader.next_message::<ClientMsg>().unwrap().unwrap() {
            ClientMsg::Hello { cols, rows, .. } => {
                assert_eq!(cols, 80);
                assert_eq!(rows, 24);
            }
            _ => panic!("wrong message"),
        }
        assert!(reader.next_message::<ClientMsg>().unwrap().is_none());
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut reader = FrameReader::new();
        reader.push(&u32::MAX.to_le_bytes()).unwrap();
        assert!(matches!(
            reader.next_message::<ClientMsg>(),
            Err(ProtocolError::TooLarge(_))
        ));
    }
}
