//! A connection: its TLS handshake, then requests one after another --
//! HTTP/1.1, kept alive -- each read whole, answered, and the next read,
//! until the client is done, says nothing for its idle time, or the server
//! stops. One task does both directions, with no poll and no select: a
//! read that waits a second at most, the answer put in the session's queue,
//! and the queue sent with a wait of its own -- so a client that stops
//! reading holds up its own connection and nothing else, and is let go when
//! it has taken nothing for its idle time.
//!
//! The routes, under /v1:
//!
//!     GET  /v1/host                      what the host is, and who is asking
//!     GET  /v1/providers                 the bus's providers and methods
//!     GET  /v1/providers/<name>          one's descriptor, with an ETag
//!     POST /v1/call/<provider>/<method>  a read or an action, the body its
//!                                        parameters
//!     POST /v1/jobs/<provider>/<method>  a job started: 202 and its number
//!     GET  /v1/jobs                      every job's record
//!     GET  /v1/jobs/<id>[?notes_since=N] one's record, log and result
//!     POST /v1/jobs/<id>/cancel          one told to stop
//!     GET  /v1/stream                    a WebSocket of metrics, the log
//!                                        and the jobs (stream.rs)
//!
//! What a client may call is the kind its method is registered as on the
//! bus -- never what the request says -- against the client's role: `read`
//! calls reads alone; `admin`, everything. Whatever changes something is a
//! line of the audit log.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use kcore::mgmt::{self as bus, Kind, Status};
use kcore::tcp::{TcpStream, RECV_TIMEOUT};
use kcore::time::boot_time_ns;
use tls::server::{Flush, Read, Recv, Refused, ServerStream, Socket, HANDSHAKE_MS};
use web::http::{Limits, Method, Parsed, Request};

use alloc::sync::Arc;

use super::files::Role;
use super::server::{ConnCtx, Counters, Phase, Shared, POLL_MS};

const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;
/// The most of one read of the connection.
const CHUNK: usize = 4096;
/// How long a read waits: how soon the connection sees a stop.
const READ_MS: u64 = 1000;
/// A request whole, at most: its line, its headers and its body.
const REQUEST_MAX: usize = Limits::API.line + Limits::API.headers + Limits::API.body + 4;
/// What waits encrypted to be sent, at most; written in pieces of this.
pub const MAX_PENDING: usize = 64 * 1024;
const PIECE: usize = 16 * 1024;
/// How long a flush waits at a time.
const FLUSH_MS: u64 = 5;
/// An answer's bytes, at most: the bus's limit.
const MAX_REPLY: usize = 256 * 1024;
/// How long the goodbye is given: the client's FIN after our close_notify.
const CLOSE_MS: u64 = 2000;

/// The kernel's TCP, as a TLS session runs over it -- and the server's
/// stop: a wait is never longer than a listener's poll, and once the server
/// stops every one fails, so a handshake under way ends at once rather than
/// hold an unload for its ten seconds.
pub struct Sock {
    pub tcp: TcpStream,
    shared: Arc<Shared>,
}

impl Socket for Sock {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv {
        if self.shared.stop.load(Ordering::Acquire) {
            return Recv::Failed;
        }
        match self.tcp.recv(buf, wait_ms.clamp(1, POLL_MS)) {
            n if n > 0 => Recv::Data(n as usize),
            0 => Recv::End,
            RECV_TIMEOUT => Recv::Idle,
            _ => Recv::Failed,
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        if self.shared.stop.load(Ordering::Acquire) {
            return None;
        }
        let n = self.tcp.send(buf, wait_ms.clamp(1, POLL_MS));
        if n >= 0 { Some(n as usize) } else { None }
    }
}

pub type Session = ServerStream<Sock>;

/// Who is at the other end: what a call is let do by, and what the audit
/// log says of it.
pub struct Who {
    pub name: String,
    pub role: Role,
    pub ip: u32,
    pub key: tls::server::Fingerprint,
}

/// How a connection ends.
pub enum Ending {
    /// The client is done, or was told it is: a close_notify and our FIN,
    /// after its own.
    Close,
    /// A reset: an error, its idle time out, or the stop.
    Reset,
}

pub fn conn_main(ctx: ConnCtx) {
    let ConnCtx { shared, stream, id } = ctx;
    let (ip, _) = stream.peer();
    let server = shared.tls.with(|t| t.clone());
    let started = boot_time_ns();
    match server.accept(Sock { tcp: stream, shared: shared.clone() }, HANDSHAKE_MS) {
        Err((sock, why)) => {
            Counters::add(&shared.counters.refused);
            if why == Refused::UnknownKey {
                Counters::add(&shared.counters.unknown_key);
            }
            kcore::trace!(1, "api: {} refused in its handshake: {}", Ip(ip), why.name());
            sock.tcp.abort();
        }
        Ok(mut session) => {
            let us = boot_time_ns().saturating_sub(started) / 1000;
            let c = &shared.counters;
            c.handshake_us_last.store(us, Ordering::Relaxed);
            c.handshake_us_max.fetch_max(us, Ordering::Relaxed);
            c.handshake_us_total.fetch_add(us, Ordering::Relaxed);
            Counters::add(&c.handshakes);
            let key = *session.client();
            let ending = match shared.client(&key) {
                /* Taken off the list since this connection's server was made */
                None => Ending::Reset,
                Some(client) => {
                    let max = shared.config.max_per_key;
                    let open = shared.conns.with(|v| {
                        if v.iter().filter(|c| c.key == Some(key)).count() >= max {
                            return false;
                        }
                        if let Some(c) = v.iter_mut().find(|c| c.id == id) {
                            c.phase = Phase::Open;
                            c.key = Some(key);
                            c.name = client.name.clone();
                            c.role = Some(client.role);
                        }
                        true
                    });
                    if open {
                        let who = Who { name: client.name, role: client.role, ip, key };
                        serve(&shared, &mut session, &who)
                    } else {
                        Counters::add(&shared.counters.busy);
                        Ending::Reset
                    }
                }
            };
            match ending {
                Ending::Close => match session.close(CLOSE_MS) {
                    (sock, true) => drop(sock),
                    (sock, false) => sock.tcp.abort(),
                },
                Ending::Reset => session.into_socket().tcp.abort(),
            }
        }
    }
    shared.conns.with(|v| v.retain(|c| c.id != id));
}

/// What a request comes to.
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    /// For a 405: the methods the route has.
    pub allow: Option<&'static str>,
    pub close: bool,
    /// The answer to a HEAD: its head alone is sent, its Content-Length
    /// the body's it would have had.
    pub head: bool,
}

impl Response {
    fn json(status: u16, body: Vec<u8>) -> Response {
        Response { status, body, etag: None, allow: None, close: false, head: false }
    }

    /// `{"error": {"code", "message"}}`, as the bus words its own.
    pub fn error(status: u16, code: Status, message: &str) -> Response {
        let mut body = Vec::new();
        let mut sink = |b: &[u8]| {
            body.extend_from_slice(b);
            true
        };
        let mut w = bus::Writer::new(&mut sink);
        code.write_error(&mut w, message, None);
        Response::json(status, body)
    }
}

/// What a route answers: a response, or the WebSocket it opens.
enum Answer {
    Response(Response),
    Stream(super::stream::Params),
}

/// The HTTP status a bus status is answered with.
pub fn http_status(s: Status) -> u16 {
    match s {
        Status::Ok => 200,
        Status::NoProvider | Status::NoMethod | Status::NotFound => 404,
        Status::Invalid => 400,
        Status::Busy => 429,
        Status::Failed => 500,
        Status::Cancelled | Status::Kind | Status::Conflict => 409,
        Status::TooLarge => 413,
        Status::Gone => 503,
        Status::Unsupported => 501,
        Status::Forbidden => 403,
    }
}

/// The bus's status a refusal of HTTP's says as its code: the one that is
/// answered with that HTTP status, where there is one -- `invalid` for the
/// rest, which no status of the bus's is.
fn refused_as(status: u16) -> Status {
    match status {
        403 => Status::Forbidden,
        413 => Status::TooLarge,
        501 => Status::Unsupported,
        _ => Status::Invalid,
    }
}

/// What one turn of the request loop found in what has arrived.
enum Step {
    Partial { continue_wanted: bool },
    Refused(u16, &'static str),
    /// What the route answered, whether to keep the connection, whether
    /// the request was a HEAD, and its length.
    Answer(Answer, bool, bool, usize),
}

fn serve(shared: &Shared, s: &mut Session, who: &Who) -> Ending {
    let idle_ns = shared.config.idle_secs.saturating_mul(NS_PER_SEC);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; CHUNK];
    let mut last = boot_time_ns();
    let mut continued = false;
    loop {
        /* Every request already whole, answered -- by the list as it
         * stands: a key taken off it is a stranger's, nothing it sends is
         * parsed, and a role changed holds from the next request */
        loop {
            let current = match shared.client(&who.key) {
                Some(now) => Some(Who { name: now.name, role: now.role, ip: who.ip, key: who.key }),
                None if buf.is_empty() => break,
                None => None,
            };
            let step = match current {
                None => Step::Refused(403, "this client's key is let in no more"),
                Some(current) => match web::http::parse(&buf, &Limits::API) {
                    Parsed::Partial { continue_wanted } => Step::Partial { continue_wanted },
                    Parsed::Refused(r) => Step::Refused(r.status, r.why),
                    Parsed::Request(req, used) => {
                        Step::Answer(route(shared, &req, &current), req.keep_alive(), req.method == Method::Head, used)
                    }
                },
            };
            match step {
                Step::Partial { continue_wanted } => {
                    if continue_wanted && !continued {
                        continued = true;
                        if !send(s, b"HTTP/1.1 100 Continue\r\n\r\n", idle_ns) {
                            return Ending::Reset;
                        }
                    }
                    break;
                }
                Step::Refused(status, why) => {
                    let mut r = Response::error(status, refused_as(status), why);
                    r.close = true;
                    return if respond(s, &r, idle_ns) { Ending::Close } else { Ending::Reset };
                }
                Step::Answer(answer, keep_alive, head, used) => {
                    continued = false;
                    Counters::add(&shared.counters.requests);
                    buf.drain(..used);
                    match answer {
                        Answer::Response(mut r) => {
                            r.close |= !keep_alive;
                            r.head = head;
                            if !respond(s, &r, idle_ns) {
                                return Ending::Reset;
                            }
                            if r.close {
                                return Ending::Close;
                            }
                        }
                        Answer::Stream(params) => {
                            Counters::add(&shared.counters.streams);
                            return super::stream::run(shared, s, who, params, idle_ns);
                        }
                    }
                }
            }
        }
        if shared.stop.load(Ordering::Acquire) {
            return Ending::Reset;
        }
        match s.read(&mut chunk, READ_MS) {
            Read::Data(n) => {
                if buf.len() + n > REQUEST_MAX || buf.try_reserve(n).is_err() {
                    let mut r = Response::error(413, Status::TooLarge, "the request is larger than the API takes");
                    r.close = true;
                    return if respond(s, &r, idle_ns) { Ending::Close } else { Ending::Reset };
                }
                buf.extend_from_slice(&chunk[..n]);
                last = boot_time_ns();
            }
            Read::Idle => {
                if boot_time_ns().saturating_sub(last) >= idle_ns {
                    return Ending::Reset;
                }
            }
            Read::End => return Ending::Close,
            Read::Failed => return Ending::Reset,
        }
    }
}

/// `bytes` put in the session's queue a piece at a time, and the queue
/// sent: false once the session has failed, or the client has taken nothing
/// for `stall_ns`.
pub fn send(s: &mut Session, bytes: &[u8], stall_ns: u64) -> bool {
    let mut at = 0;
    let mut moved = boot_time_ns();
    while at < bytes.len() {
        let piece = &bytes[at..(at + PIECE).min(bytes.len())];
        if s.write(piece, MAX_PENDING) {
            at += piece.len();
            continue;
        }
        if !flush_some(s, &mut moved, stall_ns) {
            return false;
        }
    }
    while s.pending() != 0 {
        if !flush_some(s, &mut moved, stall_ns) {
            return false;
        }
    }
    true
}

/// One flush: false once the session failed, or nothing has gone for
/// `stall_ns`.
fn flush_some(s: &mut Session, moved: &mut u64, stall_ns: u64) -> bool {
    let before = s.pending();
    match s.flush(FLUSH_MS) {
        Flush::Failed => false,
        Flush::Done => true,
        Flush::More => {
            let now = boot_time_ns();
            if s.pending() < before {
                *moved = now;
            }
            now.saturating_sub(*moved) < stall_ns
        }
    }
}

fn respond(s: &mut Session, r: &Response, stall_ns: u64) -> bool {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    headers.push(("Server", "nos"));
    headers.push(("Cache-Control", "no-store"));
    headers.push(("X-Content-Type-Options", "nosniff"));
    if !r.body.is_empty() {
        headers.push(("Content-Type", "application/json"));
    }
    if let Some(tag) = &r.etag {
        headers.push(("ETag", tag));
    }
    if let Some(allow) = r.allow {
        headers.push(("Allow", allow));
    }
    if r.status == 429 {
        headers.push(("Retry-After", "1"));
    }
    if r.close {
        headers.push(("Connection", "close"));
    }
    /* A HEAD's answer is its head alone: a client that sent one reads no
     * body, and would read this one as the next answer */
    let body: &[u8] = if r.head { &[] } else { &r.body };
    let mut out = Vec::new();
    if web::http::write_head(&mut out, r.status, &headers, Some(r.body.len())).is_err()
        || out.try_reserve(body.len()).is_err() {
        return false;
    }
    out.extend_from_slice(body);
    send(s, &out, stall_ns)
}

/// What the bus answered, into a buffer of its own.
fn answered(f: impl FnOnce(&mut dyn FnMut(&[u8]) -> bool) -> Status) -> (Status, Vec<u8>) {
    let mut got: Vec<u8> = Vec::new();
    let status = f(&mut |b| {
        got.try_reserve(b.len()).is_ok() && {
            got.extend_from_slice(b);
            true
        }
    });
    (status, got)
}

fn not_found() -> Answer {
    Answer::Response(Response::error(404, Status::NotFound, "no such route: GET /v1/providers lists what there is"))
}

fn not_allowed(allow: &'static str) -> Answer {
    let mut r = Response::error(405, Status::Invalid, "not a method this route takes");
    r.allow = Some(allow);
    Answer::Response(r)
}

fn forbidden() -> Answer {
    Answer::Response(Response::error(403, Status::Forbidden, "a client of the read role calls reads alone"))
}

fn route(shared: &Shared, req: &Request, who: &Who) -> Answer {
    let Some(rest) = req.path().strip_prefix("/v1/") else { return not_found() };
    let parts: Vec<&str> = rest.split('/').collect();
    let get = req.method == Method::Get;
    let post = req.method == Method::Post;
    match parts.as_slice() {
        ["host"] if get => Answer::Response(host(shared, who)),
        ["host"] => not_allowed("GET"),
        ["providers"] if get => {
            let kinds = if who.role == Role::Admin { bus::KINDS_ALL } else { bus::KINDS_READ };
            let (s, body) = answered(|sink| bus::providers(kinds, sink));
            Answer::Response(from_bus(s, body))
        }
        ["providers"] => not_allowed("GET"),
        ["providers", name] if get => Answer::Response(descriptor(req, name)),
        ["providers", _] => not_allowed("GET"),
        ["call", provider, method] if post => Answer::Response(call(shared, req, who, provider, method)),
        ["call", _, _] => not_allowed("POST"),
        ["jobs"] if get => {
            let (s, body) = answered(|sink| bus::jobs(sink));
            Answer::Response(from_bus(s, body))
        }
        ["jobs"] => not_allowed("GET"),
        ["jobs", id] if get => {
            let Ok(id) = id.parse::<u64>() else { return not_found() };
            let since = req.query("notes_since").and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let (s, body) = answered(|sink| bus::job(id, since, sink));
            Answer::Response(from_bus(s, body))
        }
        /* A job's number and `cancel`; or a provider and its method -- one
         * a provider may call `cancel` too */
        ["jobs", id, "cancel"] if post && id.parse::<u64>().is_ok() => {
            let id = id.parse::<u64>().unwrap_or(0);
            if who.role != Role::Admin {
                return forbidden();
            }
            let s = bus::job_cancel(id);
            audit(shared, who, &alloc::format!("cancel job {}", id), s);
            Answer::Response(from_bus(s, Vec::new()))
        }
        ["jobs", provider, method] if post => Answer::Response(start(shared, req, who, provider, method)),
        ["jobs", _] => not_allowed("GET"),
        ["jobs", _, _] => not_allowed("POST"),
        ["stream"] if get => match web::ws::handshake(req) {
            Ok(accept) => Answer::Stream(super::stream::Params::of(req, accept)),
            Err(r) => Answer::Response(Response::error(r.status, Status::Invalid, r.why)),
        },
        ["stream"] => not_allowed("GET"),
        _ => not_found(),
    }
}

/// What a status says to a person, when what answered it said nothing.
pub fn said(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::NoProvider => "no such provider: GET /v1/providers lists them",
        Status::NoMethod => "no such method: GET /v1/providers lists them",
        Status::Invalid => "the parameters were refused",
        Status::Busy => "too much at once: try again",
        Status::Failed => "the method failed",
        Status::Cancelled => "cancelled",
        Status::TooLarge => "larger than it may be",
        Status::Gone => "gone: the provider, or the bus, is stopping",
        Status::Kind => "not of the kind asked for",
        Status::Unsupported => "not on this host",
        Status::Conflict => "refused for the state of what it is about",
        Status::NotFound => "no such thing: a job, or what the method is about",
        Status::Forbidden => "not for this client",
    }
}

/// An answer from the bus: the body as it is, or -- for a failure that
/// said nothing -- one saying what it was.
fn from_bus(s: Status, body: Vec<u8>) -> Response {
    if s == Status::Ok {
        return Response::json(200, if body.is_empty() { b"{}".to_vec() } else { body });
    }
    if body.is_empty() {
        return Response::error(http_status(s), s, said(s));
    }
    Response::json(http_status(s), body)
}

/// A method's name and its provider's, as the bus takes them: anything else
/// is no route.
fn names<'a>(provider: &'a str, method: &'a str) -> Option<(&'a str, &'a str)> {
    (mgmtwire::valid_provider(provider) && mgmtwire::valid_method(method)).then_some((provider, method))
}

fn call(shared: &Shared, req: &Request, who: &Who, provider: &str, method: &str) -> Response {
    let Some((p, m)) = names(provider, method) else { return Response::error(404, Status::NotFound, "no such method") };
    let kind = match bus::kind(p, m) {
        Ok(k) => k,
        Err(s) => return Response::error(http_status(s), s, said(s)),
    };
    match kind {
        Kind::Job => {
            return Response::error(409, Status::Kind, "a job: POST /v1/jobs/<provider>/<method> starts it");
        }
        Kind::Action if who.role != Role::Admin => {
            return Response::error(403, Status::Forbidden, "a client of the read role calls reads alone");
        }
        _ => {}
    }
    let (s, body) = answered(|sink| bus::call(p, m, req.body, MAX_REPLY, sink));
    if kind == Kind::Action {
        audit(shared, who, &alloc::format!("call {}.{}", p, m), s);
    }
    from_bus(s, body)
}

fn start(shared: &Shared, req: &Request, who: &Who, provider: &str, method: &str) -> Response {
    let Some((p, m)) = names(provider, method) else { return Response::error(404, Status::NotFound, "no such method") };
    /* A job changes something, whatever it is: an admin's alone */
    if who.role != Role::Admin {
        return Response::error(403, Status::Forbidden, "a client of the read role starts no job");
    }
    let started = bus::job_start(p, m, req.body);
    let status = match started {
        Ok(_) => Status::Ok,
        Err(s) => s,
    };
    audit(shared, who, &alloc::format!("start {}.{}", p, m), status);
    match started {
        Ok(id) => {
            let mut body = Vec::new();
            let _ = write!(Text(&mut body), "{{\"id\":{}}}", id);
            Response::json(202, body)
        }
        Err(s) => Response::error(http_status(s), s, said(s)),
    }
}

fn descriptor(req: &Request, name: &str) -> Response {
    if !mgmtwire::valid_provider(name) {
        return Response::error(404, Status::NoProvider, "no such provider");
    }
    let (s, body) = answered(|sink| bus::describe(name, sink));
    if s != Status::Ok {
        return Response::error(http_status(s), s, said(s));
    }
    let tag = etag(&body);
    if req.header_str("If-None-Match").is_some_and(|v| v.split(',').any(|t| t.trim() == tag)) {
        let mut r = Response::json(304, Vec::new());
        r.etag = Some(tag);
        return r;
    }
    let mut r = Response::json(200, body);
    r.etag = Some(tag);
    r
}

/// A descriptor's tag: FNV-1a over its bytes -- a client caches it by this,
/// and a provider whose descriptor changed has another.
fn etag(body: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let h = body.iter().fold(OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME));
    alloc::format!("\"{:016x}\"", h)
}

#[cfg(target_arch = "x86_64")]
const ARCH: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
const ARCH: &str = "aarch64";

fn host(shared: &Shared, who: &Who) -> Response {
    let mut version = [0u8; 64];
    let version_len = kcore::system::version(&mut version);
    let mut revision = [0u8; 64];
    let revision_len = kcore::system::revision(&mut revision);
    let mut body = Vec::new();
    let mut sink = |b: &[u8]| {
        body.extend_from_slice(b);
        true
    };
    let mut w = bus::Writer::new(&mut sink);
    w.begin_object();
    w.field_str("id", &shared.identity.id);
    w.field_str("name", "nos");
    w.field_str_lossy("version", &version[..version_len]);
    w.field_str_lossy("revision", &revision[..revision_len]);
    w.field_str("arch", ARCH);
    w.field_str("module_abi", kmod::ABI);
    w.field_uint("api", 1);
    w.key("capabilities");
    w.begin_list();
    for c in ["call", "jobs", "stream"] {
        w.str(c);
    }
    w.end_list();
    w.key("time_s");
    match kcore::time::wall_clock_secs() {
        0 => w.null(),
        t => w.uint(t),
    }
    w.field_uint("uptime_ms", boot_time_ns() / NS_PER_MS);
    w.field_str("key", &super::files::hex(&shared.identity.key_fingerprint));
    w.key("client");
    w.begin_object();
    w.field_str("name", &who.name);
    w.field_str("role", who.role.name());
    w.end_object();
    w.end_object();
    Response::json(200, body)
}

/// A line of the audit log, and of the kernel's.
fn audit(shared: &Shared, who: &Who, what: &str, s: Status) {
    let line = alloc::format!("{} {} {} {}", who.name, Ip(who.ip), what, s.name());
    kcore::trace!(0, "api: {}", line);
    shared.audit.with(|_| super::files::audit(&shared.dir, &line));
}

/// `core::fmt::Write` into a byte buffer.
pub struct Text<'a>(pub &'a mut Vec<u8>);

impl Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// An address, dotted.
pub struct Ip(pub u32);

impl core::fmt::Display for Ip {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}
