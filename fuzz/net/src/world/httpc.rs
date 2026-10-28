//! The world's side of HTTP/1.1 and of a WebSocket, for the clients of the
//! host API (the `api` target): the responses in what the server has sent,
//! read strictly -- a status line, headers, a body of exactly the
//! Content-Length it says -- and, past a 101, its frames, as RFC 6455 has a
//! server's: so a response or a frame the server got wrong is a finding, not
//! something read some other way.

/// A response, whole.
#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

/// What a server has sent, read.
pub struct Got {
    /// The responses whole, in order: an interim one left out, a 101 the
    /// last there is.
    pub responses: Vec<Response>,
    /// The bytes they took: what follows is a response not whole yet -- or,
    /// past a 101, the other protocol's.
    pub used: usize,
    /// The interim responses (100 Continue) before each of them, and after
    /// the last: one more than there are responses.
    pub interim: Vec<usize>,
}

/// The responses whole in `got`, the `k`th of which has no body if
/// `bodiless(k)` -- the answer to a HEAD -- as a 304's has none. An error
/// for bytes that are no response a server may send.
pub fn responses(got: &[u8], bodiless: impl Fn(usize) -> bool) -> Result<Got, String> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut interim = vec![0];
    loop {
        let rest = &got[at..];
        let Some(head_len) = rest.windows(4).position(|w| w == b"\r\n\r\n") else {
            if rest.len() > 16 * 1024 {
                return Err(format!("a response head of {} bytes and no end", rest.len()));
            }
            return Ok(Got { responses: out, used: at, interim });
        };
        let head = std::str::from_utf8(&rest[..head_len]).map_err(|_| "a response head that is not text".to_string())?;
        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        let status = status_line
            .strip_prefix("HTTP/1.1 ")
            .and_then(|s| s.get(..3))
            .filter(|d| d.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|d| d.parse::<u16>().ok())
            .ok_or_else(|| format!("a status line {:?}", status_line))?;
        if status_line.len() > 4 + 9 && status_line.as_bytes()[12] != b' ' {
            return Err(format!("a status line {:?}", status_line));
        }
        let mut headers = Vec::new();
        for line in lines {
            let (name, value) = line.split_once(':').ok_or_else(|| format!("a header line {:?}", line))?;
            if name.is_empty() || !name.bytes().all(token) {
                return Err(format!("a header name {:?}", name));
            }
            let value = value.trim_matches(|c| c == ' ' || c == '\t');
            if !value.bytes().all(|c| c == b'\t' || (0x20..0x7F).contains(&c)) {
                return Err(format!("a header value {:?}", value));
            }
            headers.push((name.to_string(), value.to_string()));
        }
        let lengths: Vec<&str> =
            headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.as_str()).collect();
        let body_start = at + head_len + 4;
        /* An interim answer, and a switch to another protocol, have no body */
        if (100..200).contains(&status) {
            if !lengths.is_empty() {
                return Err(format!("a {} with a Content-Length", status));
            }
            if status == 101 {
                out.push(Response { status, headers, body: Vec::new() });
                interim.push(0);
                return Ok(Got { responses: out, used: body_start, interim });
            }
            *interim.last_mut().expect("one at least") += 1;
            at = body_start;
            continue;
        }
        let len = match lengths.as_slice() {
            _ if status == 304 || bodiless(out.len()) => 0,
            [one] => one.parse::<usize>().map_err(|_| format!("a Content-Length {:?}", one))?,
            [] => return Err(format!("a {} with no Content-Length", status)),
            _ => return Err(format!("a {} with {} Content-Lengths", status, lengths.len())),
        };
        if got.len() < body_start + len {
            return Ok(Got { responses: out, used: at, interim });
        }
        out.push(Response { status, headers, body: got[body_start..body_start + len].to_vec() });
        interim.push(0);
        at = body_start + len;
    }
}

/* ---- a WebSocket, from the client's end ---- */

pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;

/// A frame the server sent.
#[derive(Clone, Debug)]
pub struct Frame {
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// The frames whole in `bytes`, and the bytes they took. An error for one a
/// server may not send: masked, fragmented (the API sends every message
/// whole), with a reserved bit or opcode, a length not in its shortest
/// form, a control frame past 125 bytes, text that is not UTF-8.
pub fn frames(bytes: &[u8]) -> Result<(Vec<Frame>, usize), String> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        let b = &bytes[at..];
        if b.len() < 2 {
            return Ok((out, at));
        }
        let (fin, rsv, opcode) = (b[0] & 0x80 != 0, b[0] & 0x70, b[0] & 0x0F);
        if !fin || rsv != 0 {
            return Err(format!("a frame's first byte {:#04x}: unfinished, or a reserved bit", b[0]));
        }
        if ![OP_TEXT, OP_BINARY, OP_CLOSE, OP_PING, OP_PONG].contains(&opcode) {
            return Err(format!("an opcode {}", opcode));
        }
        if b[1] & 0x80 != 0 {
            return Err("a frame from the server, masked".to_string());
        }
        let (len, head) = match b[1] & 0x7F {
            126 => {
                if b.len() < 4 {
                    return Ok((out, at));
                }
                let n = u16::from_be_bytes([b[2], b[3]]) as usize;
                if n < 126 {
                    return Err(format!("a length of {} in 16 bits", n));
                }
                (n, 4)
            }
            127 => {
                if b.len() < 10 {
                    return Ok((out, at));
                }
                let n = u64::from_be_bytes(b[2..10].try_into().expect("8 bytes"));
                if n < 1 << 16 || n >> 63 != 0 {
                    return Err(format!("a length of {} in 64 bits", n));
                }
                (n as usize, 10)
            }
            n => (n as usize, 2),
        };
        if opcode >= OP_CLOSE && len > 125 {
            return Err(format!("a control frame of {} bytes", len));
        }
        if b.len() < head + len {
            return Ok((out, at));
        }
        let payload = b[head..head + len].to_vec();
        if opcode == OP_TEXT && std::str::from_utf8(&payload).is_err() {
            return Err("a text frame that is not UTF-8".to_string());
        }
        out.push(Frame { opcode, payload });
        at += head + len;
    }
}

/// A frame from the client: masked with `mask`, as RFC 6455 has a client's
/// -- or not, for a client that breaks the rule.
pub fn client_frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut f = vec![0x80 | opcode];
    let m = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        n if n < 126 => f.push(m | n as u8),
        n if n < 1 << 16 => {
            f.push(m | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(m | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(k) => {
            f.extend_from_slice(&k);
            f.extend(payload.iter().enumerate().map(|(i, b)| b ^ k[i % 4]));
        }
        None => f.extend_from_slice(payload),
    }
    f
}

/// What a server must answer the key `key` with (RFC 6455 4.2.2): the
/// SHA-1 of the key and the GUID, in base64.
pub fn accept_for(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = web::sha1::sha1(format!("{}{}", key, GUID).as_bytes());
    base64(&digest)
}

pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
