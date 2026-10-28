//! HTTP/1.1 requests read where they lie, and responses' heads written.
//!
//! A request is read in place from the connection's buffer: its line, its
//! headers and its body borrowed, nothing allocated, and every part held to
//! a limit (`Limits`) before anything else is done with it. It is read more
//! strictly than RFC 9112 asks of a server -- what a client of an API never
//! sends, and what makes two readers disagree about where a request ends,
//! is refused: a line ended by LF alone, a header folded over lines,
//! white space before a header's colon, a target that is not a path, two
//! Content-Lengths, any Transfer-Encoding. The status a refusal answers
//! with is the one RFC 9110 gives it.

use alloc::vec::Vec;

/// Headers a request may have, at most.
pub const MAX_HEADERS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The request line, its CRLF not counted.
    pub line: usize,
    /// The header lines, their CRLFs counted.
    pub headers: usize,
    /// How many headers, at most MAX_HEADERS.
    pub count: usize,
    /// The body.
    pub body: usize,
}

impl Limits {
    /// What the host API takes.
    pub const API: Limits = Limits { line: 2048, headers: 8192, count: MAX_HEADERS, body: 64 * 1024 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    /// A method of another name: the caller answers 405 or 501.
    Other,
}

/// A request refused: the status to answer with, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub why: &'static str,
}

const fn refuse(status: u16, why: &'static str) -> Refusal {
    Refusal { status, why }
}

/// What the bytes so far make.
#[derive(Debug)]
pub enum Parsed<'a> {
    /// Not a whole request yet; `continue_wanted` once its head is whole
    /// and it asks for `100 Continue` before it sends its body.
    Partial { continue_wanted: bool },
    /// A request, and how many bytes of the buffer it took: what follows
    /// is the next one's.
    Request(Request<'a>, usize),
    Refused(Refusal),
}

#[derive(Debug)]
pub struct Request<'a> {
    pub method: Method,
    /// The method's name as sent.
    pub method_name: &'a str,
    /// The path and query, as sent: visible ASCII, starting with `/`.
    pub target: &'a str,
    /// 1 for HTTP/1.1, 0 for HTTP/1.0.
    pub minor: u8,
    headers: [(&'a str, &'a [u8]); MAX_HEADERS],
    count: usize,
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// The value of the first header named `name`, any case.
    pub fn header(&self, name: &str) -> Option<&'a [u8]> {
        self.headers[..self.count].iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
    }

    /// The header's value, if it is ASCII.
    pub fn header_str(&self, name: &str) -> Option<&'a str> {
        self.header(name).and_then(|v| core::str::from_utf8(v).ok()).filter(|v| v.is_ascii())
    }

    pub fn headers(&self) -> &[(&'a str, &'a [u8])] {
        &self.headers[..self.count]
    }

    /// The target's path: up to its `?`.
    pub fn path(&self) -> &'a str {
        self.target.split_once('?').map_or(self.target, |(p, _)| p)
    }

    /// The query's value for `key`, as sent -- not decoded: what the API
    /// takes this way is a number or a name.
    pub fn query(&self, key: &str) -> Option<&'a str> {
        let (_, q) = self.target.split_once('?')?;
        q.split('&').find_map(|pair| match pair.split_once('=') {
            Some((k, v)) if k == key => Some(v),
            _ => None,
        })
    }

    /// Whether the list header `name` has `token` in it, any case: on any
    /// of its lines, since a list may be given on as many as the sender
    /// likes -- `Connection: upgrade` and `Connection: close` are
    /// `Connection: upgrade, close` (RFC 9110 5.3).
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.headers[..self.count].iter().filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .any(|(_, v)| core::str::from_utf8(v).is_ok_and(|v| has_token(v, token)))
    }

    /// Whether the connection stays open after the response: HTTP/1.1's
    /// unless it says `close`, HTTP/1.0's only if it says `keep-alive`.
    pub fn keep_alive(&self) -> bool {
        if self.minor >= 1 {
            !self.has_token("connection", "close")
        } else {
            self.has_token("connection", "keep-alive")
        }
    }
}

/// Whether the comma-separated list `list` has `token` in it, any case.
pub fn has_token(list: &str, token: &str) -> bool {
    list.split(',').any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// RFC 9110's token characters: a method's or a header's name.
fn tchar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

/// What a header's value may hold: visible characters, spaces and tabs, and
/// bytes past ASCII (obs-text) -- never a control character.
fn value_char(c: u8) -> bool {
    c == b'\t' || (0x20..0x7F).contains(&c) || c >= 0x80
}

/// The offset of the first CRLF in `b`, refusing a bare LF or CR before it.
fn line_end(b: &[u8]) -> Result<Option<usize>, Refusal> {
    for (i, &c) in b.iter().enumerate() {
        if c == b'\n' {
            return Err(refuse(400, "a line ended by LF alone"));
        }
        if c == b'\r' {
            return match b.get(i + 1) {
                Some(b'\n') => Ok(Some(i)),
                Some(_) => Err(refuse(400, "a CR inside a line")),
                None => Ok(None),
            };
        }
    }
    Ok(None)
}

/// The request at the head of `buf`, if it is there whole.
pub fn parse<'a>(buf: &'a [u8], limits: &Limits) -> Parsed<'a> {
    match parse_inner(buf, limits) {
        Ok(p) => p,
        Err(r) => Parsed::Refused(r),
    }
}

fn parse_inner<'a>(buf: &'a [u8], limits: &Limits) -> Result<Parsed<'a>, Refusal> {
    const PARTIAL: Parsed<'static> = Parsed::Partial { continue_wanted: false };
    /* The request line */
    let window = &buf[..buf.len().min(limits.line + 2)];
    let Some(line_len) = line_end(window)? else {
        if buf.len() >= limits.line + 2 {
            return Err(refuse(414, "a request line longer than it may be"));
        }
        return Ok(PARTIAL);
    };
    let line = core::str::from_utf8(&buf[..line_len]).map_err(|_| refuse(400, "a request line not ASCII"))?;
    let mut words = line.split(' ');
    let (Some(method_name), Some(target), Some(version), None) = (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(refuse(400, "a request line not of three words"));
    };
    if method_name.is_empty() || !method_name.bytes().all(tchar) {
        return Err(refuse(400, "a method that is not a token"));
    }
    let method = match method_name {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "DELETE" => Method::Delete,
        _ => Method::Other,
    };
    if !target.starts_with('/') || !target.bytes().all(|c| (0x21..0x7F).contains(&c)) {
        return Err(refuse(400, "a target that is not a path"));
    }
    let minor = match version {
        "HTTP/1.1" => 1,
        "HTTP/1.0" => 0,
        v if v.starts_with("HTTP/") => return Err(refuse(505, "an HTTP version other than 1.1 and 1.0")),
        _ => return Err(refuse(400, "a request line without its version")),
    };

    /* The headers, to the empty line */
    let mut headers: [(&'a str, &'a [u8]); MAX_HEADERS] = [("", &[]); MAX_HEADERS];
    let mut count = 0;
    let head_start = line_len + 2;
    let mut at = head_start;
    let head_end = loop {
        let rest = &buf[at..];
        let window = &rest[..rest.len().min(limits.headers.saturating_sub(at - head_start) + 2)];
        let Some(n) = line_end(window)? else {
            if at - head_start + rest.len() >= limits.headers + 2 {
                return Err(refuse(431, "headers longer than they may be"));
            }
            return Ok(PARTIAL);
        };
        if n == 0 {
            break at + 2;
        }
        if at - head_start + n + 2 > limits.headers {
            return Err(refuse(431, "headers longer than they may be"));
        }
        if count == limits.count.min(MAX_HEADERS) {
            return Err(refuse(431, "more headers than it may have"));
        }
        let field = &rest[..n];
        if field[0] == b' ' || field[0] == b'\t' {
            return Err(refuse(400, "a header folded over lines"));
        }
        let colon = field.iter().position(|&c| c == b':').ok_or(refuse(400, "a header without a colon"))?;
        let name = &field[..colon];
        if name.is_empty() || !name.iter().all(|&c| tchar(c)) {
            return Err(refuse(400, "a header's name that is not a token"));
        }
        let value = trim(&field[colon + 1..]);
        if !value.iter().all(|&c| value_char(c)) {
            return Err(refuse(400, "a control character in a header's value"));
        }
        /* A token is ASCII */
        let name = core::str::from_utf8(name).map_err(|_| refuse(400, "a header's name not ASCII"))?;
        headers[count] = (name, value);
        count += 1;
        at += n + 2;
    };

    let mut req = Request { method, method_name, target, minor, headers, count, body: &[] };
    if minor == 1 {
        let hosts = req.headers().iter().filter(|(n, _)| n.eq_ignore_ascii_case("host")).count();
        if hosts != 1 {
            return Err(refuse(400, "an HTTP/1.1 request without one Host"));
        }
    }
    if req.header("transfer-encoding").is_some() {
        return Err(refuse(501, "a Transfer-Encoding: bodies are taken by Content-Length only"));
    }
    let mut lengths = req.headers().iter().filter(|(n, _)| n.eq_ignore_ascii_case("content-length"));
    let length = match (lengths.next(), lengths.next()) {
        (None, _) => 0,
        (Some(_), Some(_)) => return Err(refuse(400, "two Content-Lengths")),
        (Some((_, v)), None) => {
            if v.is_empty() || v.len() > 19 || !v.iter().all(u8::is_ascii_digit) {
                return Err(refuse(400, "a Content-Length that is not a number"));
            }
            let n = v.iter().fold(0u64, |n, d| n * 10 + u64::from(d - b'0'));
            if n > limits.body as u64 {
                return Err(refuse(413, "a body larger than it may be"));
            }
            n as usize
        }
    };
    let Some(end) = head_end.checked_add(length).filter(|&e| e <= buf.len()) else {
        let continue_wanted = length > 0
            && req.header_str("expect").is_some_and(|e| e.eq_ignore_ascii_case("100-continue"));
        return Ok(Parsed::Partial { continue_wanted });
    };
    req.body = &buf[head_end..end];
    Ok(Parsed::Request(req, end))
}

/// `b` without the spaces and tabs at either end.
fn trim(mut b: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = b {
        b = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = b {
        b = rest;
    }
    b
}

/// A response's head could not be written: a header's name or value that
/// would break it, a status that is none, or no memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadError {
    BadStatus,
    BadHeader,
    NoMemory,
}

/// The phrase RFC 9110 gives a status, for those the API answers with.
pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        414 => "URI Too Long",
        421 => "Misdirected Request",
        422 => "Unprocessable Content",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// A response's head, appended to `out`: the status line, the headers, the
/// body's length when there is one (`Some`), and the empty line. Nothing is
/// appended when anything is refused.
pub fn write_head(out: &mut Vec<u8>, status: u16, headers: &[(&str, &str)], body_len: Option<usize>)
    -> Result<(), HeadError> {
    if !(100..=599).contains(&status) {
        return Err(HeadError::BadStatus);
    }
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(tchar) || !value.bytes().all(|c| c == b'\t' || (0x20..0x7F).contains(&c))
        {
            return Err(HeadError::BadHeader);
        }
    }
    let start = out.len();
    let mut digits = [0u8; 20];
    let result = (|| {
        put(out, b"HTTP/1.1 ")?;
        put(out, decimal(u64::from(status), &mut digits))?;
        put(out, b" ")?;
        put(out, reason(status).as_bytes())?;
        put(out, b"\r\n")?;
        for (name, value) in headers {
            put(out, name.as_bytes())?;
            put(out, b": ")?;
            put(out, value.as_bytes())?;
            put(out, b"\r\n")?;
        }
        if let Some(n) = body_len {
            put(out, b"Content-Length: ")?;
            put(out, decimal(n as u64, &mut digits))?;
            put(out, b"\r\n")?;
        }
        put(out, b"\r\n")
    })();
    if result.is_err() {
        out.truncate(start);
    }
    result
}

fn put(out: &mut Vec<u8>, b: &[u8]) -> Result<(), HeadError> {
    out.try_reserve(b.len()).map_err(|_| HeadError::NoMemory)?;
    out.extend_from_slice(b);
    Ok(())
}

fn decimal(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}
