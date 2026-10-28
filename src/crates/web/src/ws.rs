//! WebSocket (RFC 6455) as a server speaks it: the handshake a request
//! upgrades by, a client's frames read -- unmasked in place -- and a
//! server's frames written.
//!
//! A client's frames are held to the RFC and to limits: masked; no reserved
//! bit (no extension is offered); a known opcode; a length in its shortest
//! form, the 64-bit one's top bit clear, and no larger than the limit; a
//! control frame of 125 bytes at most and never fragmented; a continuation
//! only of a message begun, and a new message only when the last is done; a
//! text message valid UTF-8; a close frame's code one a peer may send. What
//! breaks any of that is an error naming the close code to answer with.

use crate::http::{has_token, Method, Refusal, Request};
use crate::sha1::Sha1;
use alloc::vec::Vec;
use base64ct::{Base64, Encoding};

/// RFC 6455's GUID, which the accept key hashes with the client's key.
const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key`: None for
/// a key that is not 16 bytes in base64.
pub fn accept_key(key: &[u8]) -> Option<[u8; 28]> {
    let mut raw = [0u8; 16];
    match Base64::decode(key, &mut raw) {
        Ok(decoded) if decoded.len() == 16 => {}
        _ => return None,
    }
    let mut h = Sha1::new();
    h.update(key);
    h.update(GUID);
    let digest = h.finish();
    let mut out = [0u8; 28];
    Base64::encode(&digest, &mut out).ok()?;
    Some(out)
}

/// Whether `req` asks to become a WebSocket as RFC 6455 4.2.1 has it -- a
/// GET over HTTP/1.1 with `Upgrade: websocket`, `Connection: upgrade`,
/// version 13 and a key -- and the accept key to answer with.
pub fn handshake(req: &Request) -> Result<[u8; 28], Refusal> {
    if req.method != Method::Get || req.minor != 1 {
        return Err(Refusal { status: 400, why: "a WebSocket is asked for with a GET over HTTP/1.1" });
    }
    let upgrade = req.header_str("upgrade").unwrap_or("");
    let connection = req.header_str("connection").unwrap_or("");
    if !has_token(upgrade, "websocket") || !has_token(connection, "upgrade") {
        return Err(Refusal { status: 426, why: "not a WebSocket upgrade" });
    }
    if req.header_str("sec-websocket-version") != Some("13") {
        return Err(Refusal { status: 426, why: "a WebSocket version other than 13" });
    }
    let key = req.header("sec-websocket-key").ok_or(Refusal { status: 400, why: "no Sec-WebSocket-Key" })?;
    accept_key(key).ok_or(Refusal { status: 400, why: "a Sec-WebSocket-Key that is not 16 bytes" })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    Continuation,
    Text,
    Binary,
    Close,
    Ping,
    Pong,
}

impl Opcode {
    fn from_bits(b: u8) -> Option<Opcode> {
        Some(match b {
            0 => Opcode::Continuation,
            1 => Opcode::Text,
            2 => Opcode::Binary,
            8 => Opcode::Close,
            9 => Opcode::Ping,
            10 => Opcode::Pong,
            _ => return None,
        })
    }

    fn bits(self) -> u8 {
        match self {
            Opcode::Continuation => 0,
            Opcode::Text => 1,
            Opcode::Binary => 2,
            Opcode::Close => 8,
            Opcode::Ping => 9,
            Opcode::Pong => 10,
        }
    }

    fn control(self) -> bool {
        matches!(self, Opcode::Close | Opcode::Ping | Opcode::Pong)
    }
}

/// Why a client's frames were refused: the close code to answer with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WsError {
    /// 1002: against the protocol.
    Protocol(&'static str),
    /// 1007: a text message, or a close's reason, not UTF-8.
    InvalidData,
    /// 1009: a frame or a message larger than it may be.
    TooBig,
    /// 1011: no memory to hold the message.
    NoMemory,
}

impl WsError {
    pub fn close_code(self) -> u16 {
        match self {
            WsError::Protocol(_) => 1002,
            WsError::InvalidData => 1007,
            WsError::TooBig => 1009,
            WsError::NoMemory => 1011,
        }
    }
}

/// The longest control frame's payload.
pub const CONTROL_MAX: usize = 125;

/// What a client sent: a whole message, or a control frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A message whole: the reader's to take before the next `feed`.
    Message { text: bool, data: &'a [u8] },
    /// To be answered by a pong with the same bytes.
    Ping(&'a [u8]),
    Pong(&'a [u8]),
    /// The client closes: its code, if it gave one, and its reason.
    Close { code: Option<u16>, reason: &'a str },
}

/// A client's frames read into its messages.
pub struct Decoder {
    /// The message being put together, and whether it is text.
    message: Vec<u8>,
    kind: Option<bool>,
    /// The last control frame's payload, for the `Event` that lends it.
    control: [u8; CONTROL_MAX],
    max_message: usize,
}

impl Decoder {
    pub fn new(max_message: usize) -> Decoder {
        Decoder { message: Vec::new(), kind: None, control: [0; CONTROL_MAX], max_message }
    }

    /// The next frame at the head of `buf`, unmasked where it lies: None
    /// while it is not there whole; else what it makes, if anything (a
    /// fragment of a message makes nothing until its last), and how many
    /// bytes of `buf` it took.
    pub fn feed<'s>(&'s mut self, buf: &mut [u8]) -> Result<Option<(Option<Event<'s>>, usize)>, WsError> {
        let Some(head) = frame_head(buf, self.max_message)? else { return Ok(None) };
        let total = head.len + head.payload;
        if buf.len() < total {
            return Ok(None);
        }
        let payload = &mut buf[head.len..total];
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= head.mask[i % 4];
        }
        let payload: &[u8] = payload;
        let event = match head.opcode {
            Opcode::Ping => {
                self.control[..payload.len()].copy_from_slice(payload);
                Some(Event::Ping(&self.control[..payload.len()]))
            }
            Opcode::Pong => {
                self.control[..payload.len()].copy_from_slice(payload);
                Some(Event::Pong(&self.control[..payload.len()]))
            }
            Opcode::Close => {
                self.control[..payload.len()].copy_from_slice(payload);
                Some(close_event(&self.control[..payload.len()])?)
            }
            Opcode::Text | Opcode::Binary | Opcode::Continuation => {
                match (head.opcode, self.kind) {
                    (Opcode::Continuation, None) => return Err(WsError::Protocol("a continuation of no message")),
                    (Opcode::Text | Opcode::Binary, Some(_)) => {
                        return Err(WsError::Protocol("a message begun inside another"))
                    }
                    (Opcode::Text | Opcode::Binary, None) => {
                        self.message.clear();
                        self.kind = Some(head.opcode == Opcode::Text);
                    }
                    _ => {}
                }
                if self.message.len().saturating_add(payload.len()) > self.max_message {
                    return Err(WsError::TooBig);
                }
                self.message.try_reserve(payload.len()).map_err(|_| WsError::NoMemory)?;
                self.message.extend_from_slice(payload);
                if !head.fin {
                    None
                } else {
                    let text = self.kind.take() == Some(true);
                    if text && core::str::from_utf8(&self.message).is_err() {
                        return Err(WsError::InvalidData);
                    }
                    Some(Event::Message { text, data: &self.message })
                }
            }
        };
        Ok(Some((event, total)))
    }
}

/// A close frame's payload: nothing, or a code a peer may send and a UTF-8
/// reason.
fn close_event(p: &[u8]) -> Result<Event<'_>, WsError> {
    match p.len() {
        0 => Ok(Event::Close { code: None, reason: "" }),
        1 => Err(WsError::Protocol("a close frame of one byte")),
        _ => {
            let code = u16::from_be_bytes([p[0], p[1]]);
            let sendable = matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999);
            if !sendable {
                return Err(WsError::Protocol("a close code no peer may send"));
            }
            let reason = core::str::from_utf8(&p[2..]).map_err(|_| WsError::InvalidData)?;
            Ok(Event::Close { code: Some(code), reason })
        }
    }
}

struct FrameHead {
    fin: bool,
    opcode: Opcode,
    mask: [u8; 4],
    /// The head's length, and the payload's.
    len: usize,
    payload: usize,
}

/// The head of the frame at the start of `buf`, checked: None while it is
/// not all there.
fn frame_head(buf: &[u8], max: usize) -> Result<Option<FrameHead>, WsError> {
    let (Some(&b0), Some(&b1)) = (buf.first(), buf.get(1)) else { return Ok(None) };
    if b0 & 0x70 != 0 {
        return Err(WsError::Protocol("a reserved bit set"));
    }
    let fin = b0 & 0x80 != 0;
    let opcode = Opcode::from_bits(b0 & 0x0F).ok_or(WsError::Protocol("an unknown opcode"))?;
    if b1 & 0x80 == 0 {
        return Err(WsError::Protocol("a client's frame not masked"));
    }
    let (payload, at) = match b1 & 0x7F {
        n @ 0..=125 => (u64::from(n), 2),
        126 => {
            let Some(b) = buf.get(2..4) else { return Ok(None) };
            let n = u64::from(u16::from_be_bytes([b[0], b[1]]));
            if n < 126 {
                return Err(WsError::Protocol("a length not in its shortest form"));
            }
            (n, 4)
        }
        _ => {
            let Some(b) = buf.get(2..10) else { return Ok(None) };
            let mut be = [0u8; 8];
            be.copy_from_slice(b);
            let n = u64::from_be_bytes(be);
            if n >> 63 != 0 {
                return Err(WsError::Protocol("a length with its top bit set"));
            }
            if n <= u64::from(u16::MAX) {
                return Err(WsError::Protocol("a length not in its shortest form"));
            }
            (n, 10)
        }
    };
    if opcode.control() {
        if !fin {
            return Err(WsError::Protocol("a control frame fragmented"));
        }
        if payload > CONTROL_MAX as u64 {
            return Err(WsError::Protocol("a control frame longer than 125 bytes"));
        }
    }
    if payload > max as u64 {
        return Err(WsError::TooBig);
    }
    let Some(m) = buf.get(at..at + 4) else { return Ok(None) };
    let mask = [m[0], m[1], m[2], m[3]];
    /* At most `max`, a usize: the conversion is exact */
    Ok(Some(FrameHead { fin, opcode, mask, len: at + 4, payload: payload as usize }))
}

/// A server's frame, whole and unmasked, appended to `out`; nothing is
/// appended without the memory for all of it.
pub fn write_frame(out: &mut Vec<u8>, opcode: Opcode, payload: &[u8]) -> Result<(), WsError> {
    if opcode.control() && payload.len() > CONTROL_MAX {
        return Err(WsError::Protocol("a control frame longer than 125 bytes"));
    }
    let mut head = [0u8; 10];
    head[0] = 0x80 | opcode.bits();
    let head_len = match payload.len() {
        n @ 0..=125 => {
            head[1] = n as u8;
            2
        }
        n if n <= usize::from(u16::MAX) => {
            head[1] = 126;
            head[2..4].copy_from_slice(&(n as u16).to_be_bytes());
            4
        }
        n => {
            head[1] = 127;
            head[2..10].copy_from_slice(&(n as u64).to_be_bytes());
            10
        }
    };
    out.try_reserve(head_len + payload.len()).map_err(|_| WsError::NoMemory)?;
    out.extend_from_slice(&head[..head_len]);
    out.extend_from_slice(payload);
    Ok(())
}

/// A close frame with `code` and `reason` (cut to fit 125 bytes).
pub fn write_close(out: &mut Vec<u8>, code: u16, reason: &str) -> Result<(), WsError> {
    let mut p = [0u8; CONTROL_MAX];
    p[..2].copy_from_slice(&code.to_be_bytes());
    let mut n = reason.len().min(CONTROL_MAX - 2);
    while !reason.is_char_boundary(n) {
        n -= 1;
    }
    p[2..2 + n].copy_from_slice(&reason.as_bytes()[..n]);
    write_frame(out, Opcode::Close, &p[..2 + n])
}
