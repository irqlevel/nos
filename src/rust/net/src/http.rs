//! The HTTP client: a GET over plain TCP or over TLS, with the body handed
//! to a sink as it arrives.
//!
//! The client never holds a whole body. A twenty megabyte download passes
//! through the sink in receive-buffer sized pieces, so the peak cost of any
//! transfer is one header buffer plus whatever the sink itself keeps. What
//! the framing says -- chunked encoding, a content length, the hard cap --
//! is applied on the way there.

use alloc::vec::Vec;

use kcore::net::Nic;
use kcore::tcp::TcpSocket;
use kcore::trace;
use tls::TlsStream;

use crate::abi;
use crate::tcp::{Conn, TCP};

/// A body kept in memory rather than streamed. The heap cannot serve a
/// single allocation larger than half a megabyte, so anything bigger has to
/// stream.
pub const MAX_RESPONSE: usize = 32768;
/// The largest download the shell offers.
pub const MAX_BODY: usize = 20 * 1024 * 1024;
/// The status line and headers must fit here; the same buffer then carries
/// the body a receive at a time.
pub const MAX_HEADER: usize = 16384;

pub const DEFAULT_PORT: u16 = 80;
pub const TLS_PORT: u16 = 443;

pub const MAX_HOST_LEN: usize = 128;
/// A whole URL, path and query included. URLs run long: a release download
/// redirects to a signed link whose query carries a signature and a token,
/// some nine hundred characters of it. A URL that does not fit fails the
/// request rather than being cut short -- a truncated URL is a different
/// URL, and whether the server still answers it depends on what its cache
/// happens to hold.
pub const MAX_URL_LEN: usize = 2048;

const RECV_TIMEOUT_MS: u64 = 10_000;
const MAX_REDIRECTS: u32 = 5;


/// Where a body goes as it arrives: handed the bytes, it says how many of
/// them it took. A short count ends the transfer and marks the response cut
/// short -- and, being a count rather than a flag, it says exactly how much
/// the sink holds.
pub trait Sink {
    fn take(&mut self, data: &[u8]) -> usize;
}

/// What a GET turned out to be. The C++ side declares the same struct.
#[repr(C)]
pub struct Response {
    pub status: i32,
    pub content_length: usize,
    pub body_len: usize,
    pub ok: u32,
    /// The body was cut short: the cap, a sink that refused, an idle
    /// timeout, or a peer that closed early
    pub truncated: u32,
    /// The handshake was refused -- a certificate that did not verify, or
    /// no protocol in common
    pub tls_failed: u32,
    /// A URL, or a redirect's target, longer than MAX_URL_LEN
    pub url_too_long: u32,
}

impl Response {
    fn new() -> Response {
        Response {
            status: 0, content_length: 0, body_len: 0,
            ok: 0, truncated: 0, tls_failed: 0, url_too_long: 0,
        }
    }

    fn is_redirect(&self, location_len: usize) -> bool {
        matches!(self.status, 301 | 302 | 303 | 307 | 308) && location_len != 0
    }
}

/* ---- the byte pipe underneath ---- */

/// Plain TCP, or TLS on top of it. Everything above -- the request, the
/// header parsing, the chunked decoding, the redirects and the sink -- is
/// the same either way.
enum Transport {
    Plain(&'static Conn),
    /// The session, over a connection that stays this client's to close
    Tls(TlsStream),
}

impl Transport {
    fn send(&mut self, data: &[u8]) -> bool {
        match self {
            Transport::Plain(conn) => TCP.send(conn, data, 0) > 0,
            Transport::Tls(stream) => stream.send(data) == data.len() as isize,
        }
    }

    fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> isize {
        match self {
            Transport::Plain(conn) => TCP.recv(conn, buf, timeout_ms),
            /* The TLS side runs its own idle timeout on the socket below */
            Transport::Tls(stream) => stream.recv(buf),
        }
    }
}

/* ---- header scanning ---- */

fn lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() { b + 32 } else { b }
}

/// Whether `haystack` at `at` begins with `needle`, ignoring case.
fn starts_with_ci(haystack: &[u8], at: usize, needle: &[u8]) -> bool {
    if at + needle.len() > haystack.len() {
        return false;
    }
    for i in 0..needle.len() {
        if lower(haystack[at + i]) != lower(needle[i]) {
            return false;
        }
    }
    true
}

/// The value of a header, as a range within the headers. Only a match at the
/// start of a line counts, so a name inside another header's value is not
/// one.
fn header_value(headers: &[u8], name: &[u8]) -> Option<(usize, usize)> {
    for at in 0..headers.len() {
        if at != 0 && headers[at - 1] != b'\n' {
            continue;
        }
        if !starts_with_ci(headers, at, name) {
            continue;
        }

        let mut value = at + name.len();
        while value < headers.len() && headers[value] == b' ' {
            value += 1;
        }
        let start = value;
        while value < headers.len() && headers[value] != b'\r' && headers[value] != b'\n' {
            value += 1;
        }
        return Some((start, value));
    }
    None
}

fn header_number(headers: &[u8], name: &[u8]) -> usize {
    let (start, end) = match header_value(headers, name) {
        Some(range) => range,
        None => return 0,
    };

    let mut value = 0usize;
    for &b in &headers[start..end] {
        if !b.is_ascii_digit() {
            break;
        }
        value = value.saturating_mul(10).saturating_add((b - b'0') as usize);
    }
    value
}

/// Whether the headers say the body is chunked.
fn is_chunked(headers: &[u8]) -> bool {
    let (start, end) = match header_value(headers, b"Transfer-Encoding:") {
        Some(range) => range,
        None => return false,
    };

    let value = &headers[start..end];
    for at in 0..value.len() {
        if starts_with_ci(value, at, b"chunked") {
            return true;
        }
    }
    false
}

/// Where the headers end: the offset just past the blank line, or None.
fn find_header_end(buf: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while at + 3 < buf.len() {
        if &buf[at..at + 4] == b"\r\n\r\n" {
            return Some(at + 4);
        }
        at += 1;
    }
    None
}

/* ---- chunked framing ---- */

#[derive(PartialEq, Eq)]
enum ChunkState {
    /// The chunk's size, in hex
    Size,
    /// A ";extension", and the line ending after it
    Ext,
    /// The payload
    Data,
    /// The line ending after the payload
    DataEnd,
    /// The zero-size chunk arrived
    Done,
}

/// The chunk framing (RFC 9112 7.1), decoded as the bytes arrive: the body
/// never exists in one piece. Trailers after the last chunk are ignored, and
/// a final chunk cut short keeps the bytes that did arrive.
struct ChunkDecoder {
    state: ChunkState,
    remaining: usize,
    saw_digit: bool,
}

impl ChunkDecoder {
    fn new() -> ChunkDecoder {
        ChunkDecoder { state: ChunkState::Size, remaining: 0, saw_digit: false }
    }

    fn done(&self) -> bool {
        self.state == ChunkState::Done
    }

    /// False means the writer took less than it was offered: stop reading.
    fn feed(&mut self, src: &[u8], out: &mut BodyWriter) -> bool {
        let mut at = 0;

        while at < src.len() && self.state != ChunkState::Done {
            match self.state {
                ChunkState::Size => {
                    let digit = match src[at] {
                        b'0'..=b'9' => (src[at] - b'0') as usize,
                        b'a'..=b'f' => (src[at] - b'a') as usize + 10,
                        b'A'..=b'F' => (src[at] - b'A') as usize + 10,
                        _ => {
                            /* A size line with no digits at all is framing
                             * garbage: stop rather than resynchronise on
                             * noise. */
                            self.state = if self.saw_digit {
                                ChunkState::Ext
                            } else {
                                ChunkState::Done
                            };
                            continue;
                        }
                    };
                    self.remaining = self.remaining * 16 + digit;
                    self.saw_digit = true;
                    at += 1;
                }
                ChunkState::Ext => {
                    let b = src[at];
                    at += 1;
                    if b == b'\n' {
                        self.saw_digit = false;
                        self.state = if self.remaining == 0 {
                            ChunkState::Done
                        } else {
                            ChunkState::Data
                        };
                    }
                }
                ChunkState::Data => {
                    let chunk = (src.len() - at).min(self.remaining);
                    let taken = out.write(&src[at..at + chunk]);
                    at += taken;
                    self.remaining -= taken;
                    if taken < chunk {
                        return false;
                    }
                    if self.remaining == 0 {
                        self.state = ChunkState::DataEnd;
                    }
                }
                ChunkState::DataEnd => {
                    let b = src[at];
                    at += 1;
                    if b == b'\n' {
                        self.state = ChunkState::Size;
                    }
                }
                ChunkState::Done => {}
            }
        }

        true
    }
}

/* ---- the body on its way to the sink ---- */

/// Everything the framing says about the body, applied on the way to the
/// caller's sink: the chunk decoding, the content length's cut-off and the
/// hard cap. It is itself what the decoder writes through, so the cap covers
/// decoded output too.
struct BodyWriter<'a> {
    sink: &'a mut dyn Sink,
    limit: usize,
    written: usize,
    overflow: bool,
    failed: bool,
}

impl<'a> BodyWriter<'a> {
    fn new(sink: &'a mut dyn Sink, limit: usize) -> BodyWriter<'a> {
        BodyWriter { sink, limit, written: 0, overflow: false, failed: false }
    }

    fn write(&mut self, data: &[u8]) -> usize {
        let mut len = data.len();
        if len > self.limit - self.written {
            /* What still fits under the cap goes over, then it stops */
            self.overflow = true;
            len = self.limit - self.written;
        }
        if len == 0 {
            return 0;
        }

        let taken = self.sink.take(&data[..len]);
        self.written += taken;
        if taken < len {
            self.failed = true;
        }
        taken
    }
}

/// The framing around the writer: what to do with raw bytes off the wire.
struct Body<'a> {
    writer: BodyWriter<'a>,
    decoder: Option<ChunkDecoder>,
    content_length: usize,
}

impl<'a> Body<'a> {
    fn new(sink: &'a mut dyn Sink, chunked: bool, content_length: usize) -> Body<'a> {
        Body {
            writer: BodyWriter::new(sink, MAX_BODY),
            decoder: if chunked { Some(ChunkDecoder::new()) } else { None },
            content_length,
        }
    }

    /// False means there is nothing more to read.
    fn feed(&mut self, data: &[u8]) -> bool {
        if let Some(decoder) = self.decoder.as_mut() {
            if !decoder.feed(data, &mut self.writer) {
                return false;
            }
            return !decoder.done();
        }

        if self.content_length != 0 {
            /* The content length is the authority when it is there: stop on
             * its last byte instead of waiting out the peer's FIN. */
            let remaining = self.content_length - self.writer.written;
            let take = data.len().min(remaining);
            if self.writer.write(&data[..take]) < take {
                return false;
            }
            return self.writer.written < self.content_length;
        }

        /* No framing but the close: read until the end of the stream */
        self.writer.write(data) == data.len()
    }

    /// Whether the body ended where the framing said it would.
    fn complete(&self) -> bool {
        if self.writer.overflow || self.writer.failed {
            return false;
        }
        match self.decoder.as_ref() {
            Some(decoder) => decoder.done(),
            None => self.content_length == 0 || self.writer.written >= self.content_length,
        }
    }
}

/* ---- the URL ---- */

struct Url {
    host: [u8; MAX_HOST_LEN],
    host_len: usize,
    port: u16,
    path: [u8; MAX_URL_LEN],
    path_len: usize,
    tls: bool,
}

impl Url {
    fn host(&self) -> &[u8] {
        &self.host[..self.host_len]
    }

    fn path(&self) -> &[u8] {
        &self.path[..self.path_len]
    }
}

/// `http[s]://host[:port][/path]`, taken apart. None when it is not one, or
/// a piece of it does not fit.
fn parse_url(url: &[u8]) -> Option<Url> {
    let mut out = Url {
        host: [0; MAX_HOST_LEN], host_len: 0,
        port: DEFAULT_PORT,
        path: [0; MAX_URL_LEN], path_len: 0,
        tls: false,
    };

    let mut at = 0;
    if url.starts_with(b"https://") {
        out.tls = true;
        out.port = TLS_PORT;
        at = 8;
    } else if url.starts_with(b"http://") {
        at = 7;
    }

    let host_start = at;
    while at < url.len() && url[at] != b'/' && url[at] != b':' {
        at += 1;
    }
    let host_len = at - host_start;
    if host_len == 0 || host_len >= MAX_HOST_LEN {
        return None;
    }
    out.host[..host_len].copy_from_slice(&url[host_start..at]);
    out.host_len = host_len;

    if at < url.len() && url[at] == b':' {
        at += 1;
        let mut port = 0u32;
        while at < url.len() && url[at].is_ascii_digit() {
            port = port * 10 + (url[at] - b'0') as u32;
            at += 1;
        }
        if port == 0 || port > 65535 {
            return None;
        }
        out.port = port as u16;
    }

    /* The path, query included. Never cut short: a truncated path is a
     * different resource. */
    if at < url.len() && url[at] == b'/' {
        let len = url.len() - at;
        if len >= MAX_URL_LEN {
            return None;
        }
        out.path[..len].copy_from_slice(&url[at..]);
        out.path_len = len;
    } else {
        out.path[0] = b'/';
        out.path_len = 1;
    }

    Some(out)
}

/// A dotted-quad, or None.
fn parse_ipv4(text: &[u8]) -> Option<u32> {
    let mut parts = [0u32; 4];
    let mut part = 0;
    let mut digits = 0;

    for &b in text {
        if b == b'.' {
            if digits == 0 || part == 3 {
                return None;
            }
            part += 1;
            digits = 0;
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        parts[part] = parts[part] * 10 + (b - b'0') as u32;
        if parts[part] > 255 {
            return None;
        }
        digits += 1;
    }

    if part != 3 || digits == 0 {
        return None;
    }
    Some((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3])
}

/// The address of a host: a literal, or what the resolver says.
fn resolve(host: &[u8]) -> Option<u32> {
    if let Some(ip) = parse_ipv4(host) {
        return Some(ip);
    }

    let dns = abi::dns()?;
    if !dns.is_ready() {
        return None;
    }
    dns.resolve(host, crate::dns::DEFAULT_TIMEOUT_MS)
}

/* ---- one exchange ---- */

/// The request line, the host header and the framing around them.
fn send_request(transport: &mut Transport, url: &Url) -> bool {
    let mut request = Vec::new();
    if request.try_reserve_exact(MAX_URL_LEN + MAX_HOST_LEN + 64).is_err() {
        return false;
    }

    request.extend_from_slice(b"GET ");
    request.extend_from_slice(url.path());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(url.host());
    request.extend_from_slice(b"\r\nConnection: close\r\n\r\n");

    transport.send(&request)
}

/// The status code off the status line.
fn parse_status(buf: &[u8]) -> i32 {
    let mut at = 0;
    while at < buf.len() && buf[at] != b' ' {
        at += 1;
    }
    if at < buf.len() {
        at += 1;
    }

    let mut status = 0;
    while at < buf.len() && buf[at].is_ascii_digit() {
        status = status * 10 + (buf[at] - b'0') as i32;
        at += 1;
    }
    status
}

/// The response, with the body handed to the sink as it arrives. `location`
/// takes a redirect's target, and its length comes back in the result.
fn recv_response(
    transport: &mut Transport, sink: &mut dyn Sink, resp: &mut Response,
    location: &mut [u8; MAX_URL_LEN],
) -> Option<usize> {
    /* One buffer for the whole exchange: it holds the headers first, then
     * carries the body a receive at a time. A twenty megabyte download costs
     * no more memory than a two hundred byte one. */
    let mut buf = Vec::new();
    if buf.try_reserve_exact(MAX_HEADER).is_err() {
        return None;
    }
    buf.resize(MAX_HEADER, 0);

    let mut total = 0;
    let mut searched = 0;
    let mut header_end;
    let mut eof = false;

    loop {
        header_end = find_header_end(&buf[..total], searched);
        if header_end.is_some() || eof {
            break;
        }

        if total >= MAX_HEADER {
            trace!(0, "http: headers larger than {} bytes", MAX_HEADER);
            return None;
        }

        /* A boundary can straddle two receives, so the last three bytes are
         * looked at again. */
        searched = total.saturating_sub(3);

        let got = transport.recv(&mut buf[total..MAX_HEADER], RECV_TIMEOUT_MS);
        if got > 0 {
            total += got as usize;
        } else if got == 0 {
            eof = true;
        } else {
            trace!(0, "http: nothing more arrived while waiting for the headers");
            return None;
        }
    }

    if total == 0 {
        return None;
    }

    resp.status = parse_status(&buf[..total]);

    let header_end = match header_end {
        Some(end) => end,
        None => {
            /* The peer hung up before the headers ended: what arrived is
             * all there is, and it is the body. */
            resp.body_len = sink.take(&buf[..total]);
            resp.content_length = resp.body_len;
            resp.truncated = 1;
            resp.ok = 1;
            return Some(0);
        }
    };

    let headers = &buf[..header_end];
    resp.content_length = header_number(headers, b"Content-Length:");
    let chunked = is_chunked(headers);

    let mut location_len = 0;
    if let Some((start, end)) = header_value(headers, b"Location:") {
        let len = end - start;
        if len >= MAX_URL_LEN {
            /* Following a cut-down target would fetch some other URL */
            if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
                trace!(0, "http: the {} redirect's target is longer than {} characters",
                    resp.status, MAX_URL_LEN - 1);
                resp.url_too_long = 1;
                resp.ok = 1;
                return Some(0);
            }
        } else {
            location[..len].copy_from_slice(&headers[start..end]);
            location_len = len;
        }
    }

    if resp.is_redirect(location_len) {
        /* A redirect's body is of no interest to anyone, and the connection
         * closes right after: do not read it at all. */
        resp.body_len = 0;
        resp.ok = 1;
        return Some(location_len);
    }

    if !chunked && resp.content_length > MAX_BODY {
        /* Refused before the transfer rather than after twenty megabytes */
        trace!(0, "http: a body of {} bytes is over the {} byte limit",
            resp.content_length, MAX_BODY);
        resp.body_len = 0;
        resp.truncated = 1;
        resp.ok = 1;
        return Some(0);
    }

    let mut body = Body::new(sink, chunked, resp.content_length);

    let mut keep_reading = body.feed(&buf[header_end..total]);
    let mut recv_failed = false;

    while keep_reading && !eof {
        let got = transport.recv(&mut buf[..MAX_HEADER], RECV_TIMEOUT_MS);
        if got > 0 {
            keep_reading = body.feed(&buf[..got as usize]);
        } else if got == 0 {
            eof = true;
        } else {
            trace!(0, "http: nothing more arrived after {} bytes of body",
                body.writer.written);
            recv_failed = true;
            break;
        }
    }

    resp.body_len = body.writer.written;
    if resp.content_length == 0 || chunked {
        resp.content_length = resp.body_len;
    }
    resp.truncated = (recv_failed || !body.complete()) as u32;
    resp.ok = 1;
    Some(0)
}

/// One request and its answer, over a connection this opens and closes.
fn exchange(
    nic: &Nic, url: &[u8], sink: &mut dyn Sink, resp: &mut Response,
    location: &mut [u8; MAX_URL_LEN],
) -> usize {
    let parsed = match parse_url(url) {
        Some(parsed) => parsed,
        None => {
            trace!(0, "http: that is not a url this client can fetch");
            return 0;
        }
    };

    let ip = match resolve(parsed.host()) {
        Some(ip) => ip,
        None => {
            trace!(0, "http: the host could not be resolved");
            return 0;
        }
    };

    let conn = match TCP.connect(nic, ip, parsed.port, 0) {
        Some(conn) => conn,
        None => {
            trace!(0, "http: the connection could not be made");
            return 0;
        }
    };

    /* TLS goes on top of that connection; the connection below is closed
     * here either way. */
    let mut transport = if parsed.tls {
        let socket = TcpSocket::from_raw(conn as *const Conn as *mut core::ffi::c_void);
        let session = core::str::from_utf8(parsed.host())
            .ok()
            .and_then(|host| TlsStream::connect(socket, host));
        match session {
            Some(stream) => Transport::Tls(stream),
            None => {
                trace!(0, "http: the tls handshake was refused");
                resp.tls_failed = 1;
                TCP.close(conn);
                return 0;
            }
        }
    } else {
        Transport::Plain(conn)
    };

    let mut redirect_len = 0;
    if send_request(&mut transport, &parsed) {
        match recv_response(&mut transport, sink, resp, location) {
            Some(len) => redirect_len = len,
            None => trace!(0, "http: no answer to the request"),
        }
    } else {
        trace!(0, "http: the request could not be sent");
    }

    /* The session says goodbye before the connection under it goes. */
    drop(transport);
    TCP.close(conn);
    redirect_len
}

/// A GET, following redirects, with the final body going to the sink. What
/// comes back in `last_location` is the target of a redirect that was not
/// followed -- one that does not lead to http, or one too many.
pub fn get_with_location(
    nic: &Nic, url: &[u8], sink: &mut dyn Sink, last_location: &mut [u8],
) -> Response {
    let mut resp = Response::new();

    if url.is_empty() || url.len() >= MAX_URL_LEN {
        trace!(0, "http: a url longer than {} characters", MAX_URL_LEN - 1);
        resp.url_too_long = 1;
        return resp;
    }

    let mut current = [0u8; MAX_URL_LEN];
    let mut current_len = url.len();
    current[..current_len].copy_from_slice(url);

    let mut location = [0u8; MAX_URL_LEN];

    for attempt in 0..=MAX_REDIRECTS {
        resp = Response::new();
        let redirect_len = exchange(nic, &current[..current_len], &mut *sink,
            &mut resp, &mut location);

        if resp.ok == 0 || !resp.is_redirect(redirect_len) {
            break;
        }

        /* What the caller is told about, when it is not followed */
        let keep = redirect_len.min(last_location.len().saturating_sub(1));
        last_location[..keep].copy_from_slice(&location[..keep]);
        if keep < last_location.len() {
            last_location[keep] = 0;
        }

        if attempt == MAX_REDIRECTS {
            trace!(0, "http: too many redirects");
            resp = Response::new();
            break;
        }

        /* Only an absolute http:// or https:// target is followed */
        let target = &location[..redirect_len];
        if !target.starts_with(b"http://") && !target.starts_with(b"https://") {
            trace!(0, "http: the {} redirect does not lead to http", resp.status);
            break;
        }

        /* Followed after all: nothing for the caller to be told */
        if !last_location.is_empty() {
            last_location[0] = 0;
        }

        current[..redirect_len].copy_from_slice(target);
        current_len = redirect_len;
    }

    resp
}

/* ---- what the kernel calls ---- */

