//! `api`: the host API module -- its source, as the kernel builds it into
//! api.ko -- serving the management bus over TLS 1.3 to the world's clients
//! from a ramfs root: its api.toml and its clients written there first, its
//! host key found there or made on its first start. On the bus are the
//! kernel's system provider (its source too), the network layer's, the
//! module's own, and the fuzzer's -- a read, an action and a job, each
//! keeping the tag its caller gave it, so whose call did what is known.
//!
//! The clients (`tlsc`, speaking HTTP and WebSocket through `httpc`)
//! connect as the input says, a few at a time: with the admin's key, the
//! reader's, a stranger's, none, or the admin's certificate signed by
//! another, or breaking TLS itself. Once in, each asks for what its plan
//! says -- the host, the providers, a descriptor and then with its ETag,
//! reads, actions and jobs of each provider, a job's record, a cancel, the
//! stream -- and what the API has not got: a route, a method a route does
//! not take, a request its parser refuses, one past its size, HTTP/1.0; one
//! after another or all at once, a body after the server's 100 Continue,
//! the connection to close after one, or kept open and quiet. A stream
//! answers the server's pings or not, pings it, closes it, or sends what it
//! will not take. Meanwhile the console runs `api`'s commands: a key taken
//! off the list and put back, the server stopped and started.
//!
//! What must hold: no byte of HTTP for a client whose key the server was
//! not given; a reader never makes an action or a job happen -- the
//! provider holds the tags of what ran -- and every action an admin was
//! told ran did, once, with a line of the audit log each and no other; every
//! response well formed, of the status the model says for the route, the
//! method's kind and the role, its error the bus's code for it, and every
//! request answered over a good link; a key taken off the list let do
//! nothing more on a connection it holds; never more connections open at
//! once than api.toml allows, in all or of a key; a stream's messages JSON
//! of the channels asked for, its log lines each once and in order, its
//! quiet client let go; the server never closing first, so no slot of its
//! port in TIME-WAIT; and once every client has gone and the module is
//! unloaded, the machine holding nothing.

#[path = "../../../../src/crates/modules/api/src/lib.rs"]
#[allow(unused_attributes, dead_code, clippy::all)]
mod api;

#[path = "../../../../src/crates/kernel/src/system.rs"]
#[allow(dead_code, clippy::all)]
mod system;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};

use mgmtwire::json::{self, Value};

use crate::machine::{cmd, sched};
use crate::world::httpc::{self, Frame, Response, OP_BINARY, OP_CLOSE, OP_PING, OP_PONG, OP_TEXT};
use crate::world::lan::Lan;
use crate::world::tcpm::{Data, Tcp, S};
use crate::world::tlsc::{keys, Break, Client, Ending, Plan, Talk, Who};
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const PORT: u16 = 39234;
const CLIENT_IP: u32 = 0x0A00_0264;
const CLIENT_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
/// What api.toml gives a connection that says nothing: short, so that an
/// input sees it run out.
const IDLE_SECS: u64 = 10;
const IDLE: u64 = IDLE_SECS * SEC;
const HANDSHAKE: u64 = tls::server::HANDSHAKE_MS * MS;
/// How long the server may count a connection after its client is done
/// with it: until the connection's task has ended it.
const REAP: u64 = SEC;
/// Clients an input makes at most.
const CLIENTS: usize = 14;
/// How long after `api start` the port is bound: its listener's task binds
/// it when it first runs, and a SYN before is reset.
const BINDING: u64 = SEC;
/// How long `api stop` may take: a connection's task sees the stop within a
/// read's wait, and a handshake's socket fails at once.
const STOPPING: u64 = 3 * SEC;
/// What the API answers with at most (http.rs): an answer past it is a 413.
const MAX_REPLY: usize = 256 * 1024;
/// What a reply of the fuzzer's `look` has besides its padding, at most.
const LOOK_FRAME: usize = 64;
/// What the client's JSON reader takes: anything the API may answer.
const READ: mgmtwire::Limits = mgmtwire::Limits { bytes: 1 << 20, depth: 32, values: 1 << 20, members: 1 << 16,
                                                  string: 1 << 20 };

/* ---- the fuzzer's provider ---- */

const FUZZP_DESCRIPTOR: &str = r#"{
  "schema": 1,
  "provider": "fuzzp",
  "title": {"en": "The fuzzer's provider", "ru": "Провайдер фаззера"},
  "help": {"en": "A read, an action and a job, each keeping its tag.", "ru": "Чтение, действие и задача с метками."},
  "methods": {
    "look": {"kind": "read", "title": {"en": "Look", "ru": "Посмотреть"}},
    "poke": {"kind": "action", "title": {"en": "Poke", "ru": "Ткнуть"}},
    "work": {"kind": "job", "title": {"en": "Work", "ru": "Поработать"}}
  }
}"#;

static FUZZP_METHODS: [mgmt::Method; 3] = [
    mgmt::Method { name: "look", kind: mgmt::Kind::Read },
    mgmt::Method { name: "poke", kind: mgmt::Kind::Action },
    mgmt::Method { name: "work", kind: mgmt::Kind::Job },
];

/// The descriptors the bus has, as the API must hand them out: byte for
/// byte what each provider registered.
fn descriptor_of(name: &str) -> Option<&'static str> {
    match name {
        "fuzzp" => Some(FUZZP_DESCRIPTOR),
        "system" => Some(include_str!("../../../../src/crates/kernel/src/system.descriptor.json")),
        "net" => Some(include_str!("../../../../src/crates/net/src/net.descriptor.json")),
        "api" => Some(include_str!("../../../../src/crates/modules/api/src/api.descriptor.json")),
        _ => None,
    }
}

/// What the fuzzer's provider did, by its callers' tags.
struct Did {
    looks: Vec<u64>,
    pokes: Vec<u64>,
    works: Vec<u64>,
}

static DID: Mutex<Did> = Mutex::new(Did { looks: Vec::new(), pokes: Vec::new(), works: Vec::new() });

fn did() -> MutexGuard<'static, Did> {
    DID.lock().unwrap_or_else(|e| e.into_inner())
}

struct Fuzzp;
static FUZZP: Fuzzp = Fuzzp;

/// The most a `look` pads its answer with, and the longest a `work` works.
const PAD_MAX: u64 = 400_000;
const WORK_MS_MAX: u64 = 4_000;
/// How often a `work` looks at whether it was told to stop.
const WORK_STEP_MS: u64 = 10;

impl mgmt::Provider for Fuzzp {
    fn call(&self, method: &str, params: &[u8], call: &mut mgmt::Call) -> mgmt::Status {
        let value = match mgmt::parse_params(params) {
            Ok(v) => v,
            Err(s) => return call.refuse(s, "the parameters are not JSON", None),
        };
        let mut p = match mgmt::Params::new(&value) {
            Ok(p) => p,
            Err(e) => return call.invalid(e),
        };
        let tag = match p.uint("tag", 1, u64::MAX) {
            Ok(t) => t,
            Err(e) => return call.invalid(e),
        };
        match method {
            "look" => {
                let pad = match p.opt_uint("pad", 0, PAD_MAX) {
                    Ok(v) => v.unwrap_or(0) as usize,
                    Err(e) => return call.invalid(e),
                };
                if let Err(e) = p.finish() {
                    return call.invalid(e);
                }
                sched::harness(|| did().looks.push(tag));
                let text = "a".repeat(pad);
                call.answer(|w| {
                    w.begin_object();
                    w.field_uint("tag", tag);
                    w.field_str("pad", &text);
                    w.end_object();
                })
            }
            "poke" => {
                if let Err(e) = p.finish() {
                    return call.invalid(e);
                }
                sched::harness(|| did().pokes.push(tag));
                call.answer(|w| {
                    w.begin_object();
                    w.field_uint("poked", tag);
                    w.end_object();
                })
            }
            "work" => {
                let ms = match p.opt_uint("ms", 0, WORK_MS_MAX) {
                    Ok(v) => v.unwrap_or(0),
                    Err(e) => return call.invalid(e),
                };
                if let Err(e) = p.finish() {
                    return call.invalid(e);
                }
                sched::harness(|| did().works.push(tag));
                call.note("fuzzp: working");
                let mut done = 0;
                while done < ms {
                    if call.cancelled() {
                        return call.refuse(mgmt::Status::Cancelled, "told to stop", None);
                    }
                    kcore::task::sleep_ms(WORK_STEP_MS);
                    done += WORK_STEP_MS;
                    call.progress(done.min(ms), ms);
                }
                call.answer(|w| {
                    w.begin_object();
                    w.field_uint("worked", tag);
                    w.end_object();
                })
            }
            _ => mgmt::Status::NoMethod,
        }
    }
}

/* ---- what the clients ask ---- */

/// What a client asks, as its plan has it.
#[derive(Clone, Debug)]
enum Ask {
    Host,
    Providers,
    /// A descriptor; `tagged`, asked with the ETag the client has for it --
    /// or one made up, if it has none.
    Describe { name: &'static str, tagged: bool },
    Call { provider: &'static str, method: &'static str, body: Body },
    Start { provider: &'static str, method: &'static str, body: Body },
    Jobs,
    /// The last job the client started, or one nobody did.
    Job { mine: bool, notes_since: u64 },
    Cancel { mine: bool },
    Stream { channels: Option<&'static str>, log_since: Option<u64>, shake: Shake, ws: WsPlan },
    /// `WRONG[i]`: a route the API has not got, or a method it does not
    /// take there.
    Wrong(usize),
    /// HTTP/1.0: answered, and the connection closed after.
    Old,
    /// `BAD[i]`, or a header (`BAD.len()`) or a request line past their
    /// limits: refused by the parser, and the connection closed.
    Bad(usize),
    /// A body larger than the API takes.
    Big,
}

#[derive(Clone, Copy, Debug)]
enum Body {
    /// `{"tag": T}`, the request's own tag.
    Tag,
    /// And padding of the answer, of this many bytes.
    Pad(u64),
    /// And a job of this many milliseconds.
    Ms(u64),
    /// Not JSON.
    Junk,
    Empty,
}

/// How a stream's upgrade is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shake {
    Ok,
    /// No Sec-WebSocket-Key: 400.
    NoKey,
    /// Version 12: 426.
    Version,
    /// No Upgrade header: 426.
    NoUpgrade,
}

#[derive(Clone, Copy, Debug)]
struct WsPlan {
    /// Answers the server's pings.
    pongs: bool,
    /// Pings the server once it has had a message.
    ping: bool,
    /// What it does once it has had `after` messages.
    then: Then,
    after: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// Nothing: it stays until the end of the input.
    Stay,
    /// Closes the stream, code 1000.
    Close,
    /// A frame unmasked: 1002, and a reset.
    Unmasked,
    /// A frame past what the server takes: 1009, and a reset.
    TooBig,
    /// A message, which the server ignores.
    Text,
}

/// A route the API has not got, or a method one does not take: the status,
/// and what Allow must say.
const WRONG: [(&str, &str, u16, Option<&str>); 10] = [
    ("PUT", "/v1/host", 405, Some("GET")),
    ("DELETE", "/v1/providers", 405, Some("GET")),
    ("GET", "/v1/call/fuzzp/look", 405, Some("POST")),
    ("POST", "/v1/jobs/12", 405, Some("GET")),
    ("GET", "/v1/jobs/1/cancel", 405, Some("POST")),
    ("OPTIONS", "/v1/host", 405, Some("GET")),
    ("HEAD", "/v1/host", 405, Some("GET")),
    ("GET", "/v2/host", 404, None),
    ("GET", "/v1/nothing/here", 404, None),
    ("GET", "/v1/jobs/abc", 404, None),
];

/// Requests the parser refuses, and with what.
const BAD: [(&[u8], u16); 7] = [
    (b"GET /v1/host HTTP/2.0\r\nHost: nos\r\n\r\n", 505),
    (b"GET /v1/host\r\nHost: nos\r\n\r\n", 400),
    (b"GET /v1/host HTTP/1.1\r\n\r\n", 400),
    (b"POST /v1/call/fuzzp/look HTTP/1.1\r\nHost: nos\r\nTransfer-Encoding: chunked\r\n\r\n", 501),
    (b"GET /v1/host HTTP/1.1\r\nHost: nos\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n", 400),
    (b"GET /v1/host HTTP/1.1\nHost: nos\n\n", 400),
    (b"GET /v1/host HTTP/1.1\r\nHost: nos\r\nX-A: a\x01b\r\n\r\n", 400),
];
/// The two made past a limit, after BAD: a header of 9 KiB, a request line
/// of 3.
const BAD_HEADER: usize = BAD.len();
const BAD_LINE: usize = BAD.len() + 1;

/// The system provider's reads, which take no parameters.
const SYSTEM_READS: [&str; 6] = ["info", "cpus", "memory", "tasks", "modules", "interrupts"];

/// A request of a client's plan.
#[derive(Clone, Debug)]
struct Req {
    ask: Ask,
    /// `Connection: close`.
    close: bool,
    /// `Expect: 100-continue`: the head first, the body once the server
    /// asks for it.
    expect: bool,
}

/// What became of a request.
#[derive(Clone, Debug, Default)]
struct Went {
    /// When it went, and when its answer was whole.
    at: u64,
    answered: Option<u64>,
    tag: u64,
    /// The ETag it asked with, and whether the server had given it.
    etag: Option<(String, bool)>,
    /// The job it asked about.
    job: u64,
    /// Its body held back for the server's 100 Continue; and the 100s that
    /// came before its answer.
    held: bool,
    continues: usize,
    /// A stream's: the key it asked with.
    key: String,
}

/// A client's side of its conversation, which the target judges after.
struct Conv {
    id: usize,
    who: Who,
    reqs: Vec<Req>,
    pipelined: bool,
    /// Keeps the connection, quiet, once it is answered.
    linger: bool,
    went: Vec<Went>,
    /// The body of the request whose head went with Expect.
    held: Option<Vec<u8>>,
    /// The answers whole so far, and what came with them.
    answers: Vec<Response>,
    etags: Vec<(&'static str, String)>,
    jobs: Vec<u64>,
    ws: Option<Ws>,
    /// What the server sent that is no HTTP.
    broken: Option<String>,
    rng: u64,
    finish: Rc<Cell<bool>>,
    /// When it last sent anything: what a connection's idle time runs from.
    said: u64,
    /// How long it waits after an answer before its next request, as a
    /// client that polls does.
    pause: u64,
}

/// A stream, from its 101 on.
struct Ws {
    plan: WsPlan,
    /// Where its frames start in what the server sent, and how much of them
    /// has been read.
    start: usize,
    parsed: usize,
    frames: Vec<(u64, Frame)>,
    messages: usize,
    queue: VecDeque<Vec<u8>>,
    ping: Option<Vec<u8>>,
    then_done: bool,
    opened: u64,
    /// When the client last sent anything: what its idle time runs from.
    said: u64,
    /// The server's close frame's code.
    closed: Option<u16>,
    broken: Option<String>,
    rng: u64,
}

fn step(rng: &mut u64) -> u64 {
    *rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *rng >> 33
}

impl Ws {
    fn mask(&mut self) -> Option<[u8; 4]> {
        Some((step(&mut self.rng) as u32).to_le_bytes())
    }

    fn take_in(&mut self, got: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        match httpc::frames(&got[self.start + self.parsed..]) {
            Err(e) => self.broken = Some(e),
            Ok((frames, used)) => {
                self.parsed += used;
                let now = sched::now();
                for f in frames {
                    match f.opcode {
                        OP_TEXT => self.messages += 1,
                        OP_PING if self.plan.pongs && self.closed.is_none() => {
                            let mask = self.mask();
                            self.queue.push_back(httpc::client_frame(OP_PONG, &f.payload, mask));
                        }
                        OP_CLOSE => {
                            let code = (f.payload.len() >= 2).then(|| u16::from_be_bytes([f.payload[0], f.payload[1]]));
                            self.closed = Some(code.unwrap_or(1005));
                        }
                        _ => {}
                    }
                    self.frames.push((now, f));
                }
            }
        }
    }

    fn next(&mut self, finish: bool) -> Option<Vec<u8>> {
        if self.broken.is_some() || self.closed.is_some() || finish {
            return None;
        }
        let now = sched::now();
        if let Some(f) = self.queue.pop_front() {
            self.said = now;
            return Some(f);
        }
        if !self.then_done && self.messages >= self.plan.after {
            self.then_done = true;
            let mask = self.mask();
            let frame = match self.plan.then {
                Then::Stay => None,
                Then::Close => Some(httpc::client_frame(OP_CLOSE, &1000u16.to_be_bytes(), mask)),
                Then::Unmasked => Some(httpc::client_frame(OP_TEXT, b"{}", None)),
                Then::TooBig => Some(httpc::client_frame(OP_BINARY, &[0x5A; 5000], mask)),
                Then::Text => Some(httpc::client_frame(OP_TEXT, b"hello", mask)),
            };
            if frame.is_some() {
                self.said = now;
                return frame;
            }
        }
        if self.plan.ping && self.ping.is_none() && self.messages >= 1 {
            let payload = format!("p{}", step(&mut self.rng) % 1000).into_bytes();
            self.ping = Some(payload.clone());
            self.said = now;
            let mask = self.mask();
            return Some(httpc::client_frame(OP_PING, &payload, mask));
        }
        None
    }
}

/// `{"tag": ...}` and the rest of a body.
fn body_of(body: Body, tag: u64) -> Vec<u8> {
    match body {
        Body::Tag => format!("{{\"tag\":{}}}", tag),
        Body::Pad(n) => format!("{{\"tag\":{},\"pad\":{}}}", tag, n),
        Body::Ms(n) => format!("{{\"tag\":{},\"ms\":{}}}", tag, n),
        Body::Junk => format!("{{\"tag\":{},", tag),
        Body::Empty => String::new(),
    }
    .into_bytes()
}

impl Conv {
    fn tag(&self, k: usize) -> u64 {
        ((self.id as u64) << 16) | (k as u64 + 1)
    }

    /// Whether the answer to request `k` has no body: a HEAD's.
    fn bodiless(&self, k: usize) -> bool {
        matches!(self.reqs.get(k).map(|r| &r.ask), Some(Ask::Wrong(i)) if WRONG[*i].0 == "HEAD")
    }

    /// Whether request `k` ends the connection: after its answer the server
    /// closes it, or -- a stream -- speaks another protocol.
    fn closing(&self, k: usize) -> bool {
        let r = &self.reqs[k];
        r.close || matches!(r.ask, Ask::Old | Ask::Bad(_) | Ask::Big | Ask::Stream { shake: Shake::Ok, .. })
    }

    fn take_in(&mut self, got: &[u8]) {
        if self.broken.is_some() {
            return;
        }
        if self.ws.is_none() {
            let bodiless: Vec<bool> = (0..self.reqs.len()).map(|k| self.bodiless(k)).collect();
            let g = match httpc::responses(got, |k| bodiless.get(k).copied().unwrap_or(false)) {
                Ok(g) => g,
                Err(e) => {
                    self.broken = Some(e);
                    return;
                }
            };
            let now = sched::now();
            if g.responses.len() > self.went.len() {
                self.broken = Some(format!("{} answers to {} requests", g.responses.len(), self.went.len()));
                return;
            }
            for (k, n) in g.interim.iter().enumerate().take(self.went.len()) {
                self.went[k].continues = *n;
            }
            for k in self.answers.len()..g.responses.len() {
                let r = g.responses[k].clone();
                self.went[k].answered = Some(now);
                match (&self.reqs[k].ask, r.status) {
                    (Ask::Describe { name, .. }, 200) => {
                        if let Some(tag) = r.header("ETag") {
                            self.etags.retain(|(n, _)| n != name);
                            self.etags.push((name, tag.to_string()));
                        }
                    }
                    (Ask::Start { .. }, 202) => {
                        let id = json::parse(&r.body, &READ).ok().and_then(|v| v.get("id").and_then(Value::as_u64));
                        self.jobs.extend(id);
                    }
                    (Ask::Stream { ws, .. }, 101) => {
                        self.ws = Some(Ws { plan: *ws, start: g.used, parsed: 0, frames: Vec::new(), messages: 0,
                                            queue: VecDeque::new(), ping: None, then_done: false, opened: now,
                                            said: now, closed: None, broken: None, rng: self.rng ^ 0x5DEECE66D });
                    }
                    _ => {}
                }
                self.answers.push(r);
            }
        }
        if let Some(ws) = self.ws.as_mut() {
            ws.take_in(got);
        }
    }

    /// Request `k`, its head and its body; what went is noted.
    fn render(&mut self, k: usize) -> (Vec<u8>, Vec<u8>) {
        let tag = self.tag(k);
        let req = self.reqs[k].clone();
        let mut went = Went { at: sched::now(), tag, ..Went::default() };
        let mut extra: Vec<(&str, String)> = Vec::new();
        let (method, target, body): (&str, String, Vec<u8>) = match &req.ask {
            Ask::Host => ("GET", "/v1/host".to_string(), Vec::new()),
            Ask::Providers => ("GET", "/v1/providers".to_string(), Vec::new()),
            Ask::Describe { name, tagged } => {
                if *tagged {
                    let tag = self.etags.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone());
                    let given = tag.is_some();
                    let tag = tag.unwrap_or_else(|| "\"0123456789abcdef\"".to_string());
                    extra.push(("If-None-Match", tag.clone()));
                    went.etag = Some((tag, given));
                }
                ("GET", format!("/v1/providers/{}", name), Vec::new())
            }
            Ask::Call { provider, method, body } => {
                ("POST", format!("/v1/call/{}/{}", provider, method), body_of(*body, tag))
            }
            Ask::Start { provider, method, body } => {
                ("POST", format!("/v1/jobs/{}/{}", provider, method), body_of(*body, tag))
            }
            Ask::Jobs => ("GET", "/v1/jobs".to_string(), Vec::new()),
            Ask::Job { mine, notes_since } => {
                went.job = if *mine { self.jobs.last().copied().unwrap_or(999_999) } else { 999_999 };
                ("GET", format!("/v1/jobs/{}?notes_since={}", went.job, notes_since), Vec::new())
            }
            Ask::Cancel { mine } => {
                went.job = if *mine { self.jobs.last().copied().unwrap_or(999_999) } else { 999_999 };
                ("POST", format!("/v1/jobs/{}/cancel", went.job), Vec::new())
            }
            Ask::Stream { channels, log_since, shake, .. } => {
                let mut target = "/v1/stream".to_string();
                let mut q = Vec::new();
                if let Some(c) = channels {
                    q.push(format!("channels={}", c));
                }
                if let Some(s) = log_since {
                    q.push(format!("log_since={}", s));
                }
                if !q.is_empty() {
                    target = format!("{}?{}", target, q.join("&"));
                }
                let mut key = [0u8; 16];
                for b in key.iter_mut() {
                    *b = step(&mut self.rng) as u8;
                }
                went.key = httpc::base64(&key);
                if *shake != Shake::NoUpgrade {
                    extra.push(("Upgrade", "websocket".to_string()));
                }
                extra.push(("Connection", "Upgrade".to_string()));
                if *shake != Shake::NoKey {
                    extra.push(("Sec-WebSocket-Key", went.key.clone()));
                }
                extra.push(("Sec-WebSocket-Version", if *shake == Shake::Version { "12" } else { "13" }.to_string()));
                ("GET", target, Vec::new())
            }
            Ask::Wrong(i) => (WRONG[*i].0, WRONG[*i].1.to_string(), Vec::new()),
            Ask::Old => {
                self.went.push(went);
                return (b"GET /v1/host HTTP/1.0\r\n\r\n".to_vec(), Vec::new());
            }
            Ask::Bad(i) => {
                self.went.push(went);
                let bytes = match *i {
                    BAD_HEADER => format!("GET /v1/host HTTP/1.1\r\nHost: nos\r\nX-Long: {}\r\n\r\n", "b".repeat(9000))
                        .into_bytes(),
                    BAD_LINE => format!("GET /v1/{} HTTP/1.1\r\nHost: nos\r\n\r\n", "c".repeat(3000)).into_bytes(),
                    i => BAD[i].0.to_vec(),
                };
                return (bytes, Vec::new());
            }
            Ask::Big => {
                self.went.push(went);
                return (b"POST /v1/call/fuzzp/look HTTP/1.1\r\nHost: nos\r\nContent-Length: 70000\r\n\r\n".to_vec(),
                        Vec::new());
            }
        };
        let mut head = format!("{} {} HTTP/1.1\r\nHost: nos\r\n", method, target);
        for (k, v) in &extra {
            head.push_str(&format!("{}: {}\r\n", k, v));
        }
        if method == "POST" || !body.is_empty() {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        if req.close {
            head.push_str("Connection: close\r\n");
        }
        let held = req.expect && !body.is_empty() && !self.pipelined;
        if held {
            head.push_str("Expect: 100-continue\r\n");
        }
        head.push_str("\r\n");
        went.held = held;
        self.went.push(went);
        (head.into_bytes(), body)
    }

    fn next(&mut self, got: &[u8]) -> Option<Vec<u8>> {
        let said = self.say(got);
        if said.is_some() {
            self.said = sched::now();
        }
        said
    }

    fn say(&mut self, got: &[u8]) -> Option<Vec<u8>> {
        self.take_in(got);
        if self.broken.is_some() {
            return None;
        }
        if let Some(ws) = self.ws.as_mut() {
            return ws.next(self.finish.get());
        }
        if (0..self.went.len()).any(|k| self.closing(k)) && self.held.is_none() {
            return None;
        }
        let k = self.went.len();
        if let Some(body) = self.held.take() {
            let pending = k - 1;
            /* Answered without asking for it: the body is never sent */
            if self.answers.len() > pending {
                return None;
            }
            if self.went[pending].continues > 0 {
                return Some(body);
            }
            self.held = Some(body);
            return None;
        }
        if k == self.reqs.len() {
            return None;
        }
        if self.pipelined {
            let mut all = Vec::new();
            for k in k..self.reqs.len() {
                let (head, body) = self.render(k);
                all.extend_from_slice(&head);
                all.extend_from_slice(&body);
                if self.closing(k) {
                    break;
                }
            }
            return Some(all);
        }
        if self.answers.len() < k || sched::now() < self.wake() {
            return None;
        }
        let (head, body) = self.render(k);
        if self.went[k].held {
            self.held = Some(body);
            return Some(head);
        }
        Some([head, body].concat())
    }

    /// When it may ask its next request: its pause after the last answer.
    fn wake(&self) -> u64 {
        match self.went.len().checked_sub(1).and_then(|k| self.went[k].answered) {
            Some(at) if !self.pipelined && self.went.len() < self.reqs.len() && self.ws.is_none() => at + self.pause,
            _ => 0,
        }
    }

    /// Whether it has said all it will: its requests answered -- and, told
    /// to finish, a stream or a client that lingers.
    fn done(&self) -> bool {
        if self.broken.is_some() {
            return true;
        }
        if let Some(ws) = &self.ws {
            return ws.broken.is_some() || ws.closed.is_some() || self.finish.get();
        }
        if self.linger && !self.finish.get() {
            return false;
        }
        let sent = self.went.len();
        let closed = (0..sent).any(|k| self.closing(k));
        (sent == self.reqs.len() || closed) && self.answers.len() >= sent && self.held.is_none()
    }
}

/// What a client says: its conversation, which the target keeps a hold of.
struct Http(Rc<RefCell<Conv>>);

impl Talk for Http {
    fn next(&mut self, got: &[u8]) -> Option<Vec<u8>> {
        self.0.borrow_mut().next(got)
    }

    fn done(&self, _got: &[u8]) -> bool {
        self.0.borrow().done()
    }
}

/* ---- the world ---- */

/// A client, and what the world knew when it connected.
struct Peer {
    client: Client,
    conv: Option<Rc<RefCell<Conv>>>,
    at: u64,
    over_at: Option<u64>,
    /// Others connected when it connected: the server may turn it away past
    /// its limits.
    crowded: bool,
}

struct Wire {
    lan: Lan,
    tcp: Tcp,
    peers: Rc<RefCell<Vec<Peer>>>,
    /// When the timer last ran: a client's pause that ran out by then has
    /// had its turn.
    ticked: u64,
}

impl Wire {
    fn tend(&mut self, net: &mut Net) {
        for p in self.peers.borrow_mut().iter_mut() {
            let ep = p.client.ep;
            if matches!(self.tcp.eps[ep].st, S::Established | S::CloseWait) {
                p.client.start(&mut self.tcp, net);
            }
            p.client.pump(&mut self.tcp, net);
            if p.client.ending.is_none() && (!self.tcp.eps[ep].open() || self.tcp.eps[ep].reset.is_some()) {
                p.client.ending = Some(Ending::Gone);
            }
            /* Done: its FIN, as a client hangs up -- the server's close
             * waits for it */
            if p.client.over() && p.over_at.is_none() {
                p.over_at = Some(sched::now());
                if self.tcp.eps[ep].open() && self.tcp.eps[ep].reset.is_none() {
                    self.tcp.close(net, ep);
                }
            }
        }
    }
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if !self.lan.on_frame(net, frame) {
            self.tcp.on_frame(net, frame);
        }
        self.tend(net);
    }

    /// The next of TCP's timers, or a client's pause running out.
    fn next_timer(&self) -> u64 {
        let wakes = self.peers.borrow().iter().filter(|p| !p.client.over()).filter_map(|p| p.conv.as_ref())
            .map(|c| c.borrow().wake()).filter(|t| *t > self.ticked).min().unwrap_or(u64::MAX);
        self.tcp.next_timer().min(wakes)
    }

    fn on_timer(&mut self, net: &mut Net) {
        self.ticked = sched::now();
        self.tcp.on_timer(net);
        self.tend(net);
    }
}

/* ---- the plan ---- */

/// The limits api.toml says.
#[derive(Clone, Copy, Debug)]
struct Limits {
    conns: usize,
    per_key: usize,
    handshakes: usize,
}

fn body(r: &mut Input) -> Body {
    match r.u8() % 16 {
        0 => Body::Junk,
        1 => Body::Empty,
        _ => Body::Tag,
    }
}

fn ask(r: &mut Input) -> Ask {
    match r.u8() % 64 {
        0..=4 => Ask::Host,
        5..=7 => Ask::Providers,
        8..=11 => Ask::Describe { name: r.pick(&["fuzzp", "fuzzp", "system", "net", "api", "nope"]), tagged: r.bool() },
        12..=17 => {
            let body = match r.u8() % 8 {
                0..=2 => Body::Pad(r.pick(&[0u64, 10, 1000, 20_000, 100_000, 262_000, 300_000])),
                _ => body(r),
            };
            Ask::Call { provider: "fuzzp", method: "look", body }
        }
        18..=25 => Ask::Call { provider: "fuzzp", method: "poke", body: body(r) },
        26 => Ask::Call { provider: "fuzzp", method: "work", body: Body::Tag },
        27..=29 => Ask::Call { provider: "system", method: r.pick(&SYSTEM_READS), body: Body::Empty },
        30 => {
            let provider = r.pick(&["nope", "fuzzp", "Bad"]);
            Ask::Call { provider, method: r.pick(&["look", "nope"]), body: Body::Tag }
        }
        31..=36 => {
            let body = if r.u8() < 32 { body(r) } else { Body::Ms(r.pick(&[0u64, 10, 200, 1500, 4000])) };
            Ask::Start { provider: "fuzzp", method: "work", body }
        }
        37 => Ask::Start { provider: "fuzzp", method: r.pick(&["look", "nope"]), body: Body::Tag },
        38..=39 => Ask::Jobs,
        40..=42 => Ask::Job { mine: r.u8() < 200, notes_since: r.pick(&[0u64, 1, 100]) },
        43..=45 => Ask::Cancel { mine: r.u8() < 200 },
        46..=51 => {
            let channels = r.pick(&[None, None, Some("metrics"), Some("log"), Some("jobs"), Some("metrics,jobs"),
                                    Some("log,jobs"), Some("nothing")]);
            let log_since = r.pick(&[None, None, Some(0u64), Some(5), Some(1 << 40)]);
            let shake = match r.u8() % 16 {
                0 => Shake::NoKey,
                1 => Shake::Version,
                2 => Shake::NoUpgrade,
                _ => Shake::Ok,
            };
            let then = match r.u8() % 8 {
                0 | 1 => Then::Close,
                2 => Then::Unmasked,
                3 => Then::TooBig,
                4 => Then::Text,
                _ => Then::Stay,
            };
            let ws = WsPlan { pongs: r.u8() < 192, ping: r.u8() < 64, then, after: r.pick(&[0usize, 1, 3, 8]) };
            Ask::Stream { channels, log_since, shake, ws }
        }
        52..=55 => Ask::Wrong(r.below(WRONG.len() as u64) as usize),
        56 => Ask::Old,
        57..=60 => Ask::Bad(r.below(BAD.len() as u64 + 2) as usize),
        61 => Ask::Big,
        _ => Ask::Host,
    }
}

fn plan(r: &mut Input, id: usize, finish: &Rc<Cell<bool>>) -> (Plan, Option<Rc<RefCell<Conv>>>) {
    /* Zeros -- an input's end -- are the ordinary client: the admin, as the
     * panel is, keeping to the protocol */
    let who = match r.u8() % 16 {
        0..=7 => Who::Known,
        8..=12 => Who::Reader,
        13 => Who::Stranger,
        14 => Who::Nobody,
        _ => Who::Impostor,
    };
    let brk = match r.u8() % 32 {
        27 => Break::Garbage,
        28 => Break::Silent,
        29 => Break::Cut,
        30 => Break::Tls12,
        31 => Break::Alpn,
        _ => Break::None,
    };
    let alpn: Vec<&'static [u8]> = match (brk, r.u8() % 5) {
        (Break::Alpn, _) => vec![b"h2"],
        (_, 0) => vec![],
        (_, 1) => vec![b"http/1.1"],
        (_, 2) => vec![b"h2", b"http/1.1", b"nos-api/1"],
        _ => vec![b"nos-api/1"],
    };
    let plan = Plan { who, brk, alpn, lines: Vec::new(), pin: None };
    let mut reqs = Vec::new();
    for _ in 0..1 + r.below(6) {
        let ask = ask(r);
        /* An ETag asked with is mostly one the server gave: the descriptor
         * asked for first, on the same connection */
        if let Ask::Describe { name, tagged: true } = ask {
            let asked = reqs.iter().any(|q: &Req| matches!(q.ask, Ask::Describe { name: n, .. } if n == name));
            if !asked && r.u8() < 192 {
                reqs.push(Req { ask: Ask::Describe { name, tagged: false }, close: false, expect: false });
            }
        }
        /* And a job asked about is mostly one it started */
        if matches!(ask, Ask::Job { mine: true, .. } | Ask::Cancel { mine: true })
           && !reqs.iter().any(|q: &Req| matches!(q.ask, Ask::Start { .. })) && r.u8() < 192 {
            let body = Body::Ms(r.pick(&[0u64, 200, 1500]));
            reqs.push(Req { ask: Ask::Start { provider: "fuzzp", method: "work", body }, close: false, expect: false });
        }
        let stream = matches!(ask, Ask::Stream { shake: Shake::Ok, .. });
        reqs.push(Req { ask, close: r.u8() < 16, expect: r.u8() < 48 });
        /* Nothing is asked of a connection after it became a stream */
        if stream {
            break;
        }
    }
    let conv = Conv { id, who, reqs, pipelined: r.u8() < 32, linger: r.u8() < 16, went: Vec::new(), held: None,
                      answers: Vec::new(), etags: Vec::new(), jobs: Vec::new(), ws: None, broken: None,
                      rng: r.u64() | 1, finish: finish.clone(), said: 0,
                      pause: r.pick(&[0u64, 0, 0, 100 * MS, SEC, 3 * SEC]) };
    (plan, Some(Rc::new(RefCell::new(conv))))
}

/* ---- the console ---- */

/// When the world changed under the clients: a key off the list, the server
/// stopped -- from before the command ran to after it returned.
#[derive(Default)]
struct Timeline {
    off: Vec<(Who, u64, u64)>,
    down: Vec<(u64, u64)>,
}

impl Timeline {
    /// Whether `who`'s key was on the list all of `from..=to` (Some(true)),
    /// off it all of it (Some(false)), or both.
    fn key(&self, who: Who, from: u64, to: u64) -> Option<bool> {
        let mut touched = false;
        for &(w, a, b) in &self.off {
            if w != who || b < from || a > to {
                continue;
            }
            if a <= from && to <= b {
                return Some(false);
            }
            touched = true;
        }
        if touched { None } else { Some(true) }
    }

    fn down_within(&self, from: u64, to: u64) -> bool {
        self.down.iter().any(|&(a, b)| !(b < from || a > to))
    }

    fn off_within(&self, who: Who, from: u64, to: u64) -> bool {
        self.key(who, from, to) != Some(true)
    }
}

/// The server's own count of its connections, as `api.status` gives it on
/// the bus, held to api.toml's limits: in all, in their handshake, open of
/// one client's key.
fn held_to_limits(w: &mut World<Wire>, limits: &Limits) {
    let app = App::spawn("status", 1, || {
        let mut got = Vec::new();
        let status = mgmt::call("api", "status", b"", 1 << 20, &mut |b| {
            got.extend_from_slice(b);
            true
        });
        (status, got)
    });
    invariant!(w.wait_app(&app, 30 * SEC), "api.status never answered");
    let (status, got) = app.take().expect("done");
    invariant!(status == mgmt::Status::Ok, "api.status answered {}", status.name());
    let v = json::parse(&got, &READ).unwrap_or(Value::Null);
    let conns = v.get("connections").and_then(Value::as_list).unwrap_or(&[]);
    let open = |c: &&Value| c.get("open").and_then(Value::as_bool) == Some(true);
    let handshaking = conns.iter().filter(|c| !open(c)).count();
    invariant!(conns.len() <= limits.conns && handshaking <= limits.handshakes,
               "the server holds {} connections, {} in their handshake, api.toml allowing {} and {}", conns.len(),
               handshaking, limits.conns, limits.handshakes);
    for name in ["panel", "viewer"] {
        let n = conns.iter().filter(|c| open(c) && c.get("client").and_then(Value::as_str) == Some(name)).count();
        invariant!(n <= limits.per_key, "the server holds {} connections of {}'s key, api.toml allowing {}", n, name,
                   limits.per_key);
    }
}

/// The clients file, every key on it but those `off`.
fn clients_file(off: &[(Who, u64)]) -> String {
    let mut text = String::new();
    for who in [Who::Known, Who::Reader] {
        if off.iter().any(|(w, _)| *w == who) {
            continue;
        }
        let cert = if who == Who::Known { &keys().known.cert } else { &keys().reader.cert };
        let (name, role) = role_of(who);
        text.push_str(&format!("{} {} {}\n", hex(&tls::server::fingerprint(cert).expect("a certificate")), role, name));
    }
    text
}

/// A clients file the API must refuse whole: a key twice -- the admin's,
/// once as a reader --, a role it has not got, a fingerprint that is not
/// one, a name it does not take, a line of four words.
fn bad_clients(which: u8) -> String {
    let known = hex(&tls::server::fingerprint(&keys().known.cert).expect("a certificate"));
    let reader = hex(&tls::server::fingerprint(&keys().reader.cert).expect("a certificate"));
    match which % 5 {
        0 => format!("{} admin panel\n{} read viewer\n{} read other\n", known, reader, known),
        1 => format!("{} admin panel\n{} root viewer\n", known, reader),
        2 => format!("{} admin panel\n{}x read viewer\n", known, &reader[..63]),
        3 => format!("{} admin panel\n{} read view/er\n", known, reader),
        _ => format!("{} admin panel\n{} read viewer extra\n", known, reader),
    }
}

/// `who`'s key put back on the list, as it was.
fn add_back(w: &mut World<Wire>, who: Who) {
    let (name, role) = role_of(who);
    let cert = if who == Who::Known { &keys().known.cert } else { &keys().reader.cert };
    let fp = tls::server::fingerprint(cert).expect("a certificate");
    let out = console(w, &format!("api client add {} {} {}", name, role, hex(&fp)));
    invariant!(out.contains("lets in"), "`api client add {}`: {:?}", name, out);
}

fn console(w: &mut World<Wire>, line: &str) -> String {
    let l = line.to_string();
    let app = App::spawn("console", 1, move || cmd::run(&l));
    invariant!(w.wait_app(&app, 120 * SEC), "`{}` never returned", line);
    app.take().unwrap_or_default()
}

/* ---- judging ---- */

/// What the run knew, for the judge.
struct Ctx {
    pin_hex: String,
    limits: Limits,
    lossless: bool,
    /// The client's address is let in by eth0's allowlist.
    allowed: bool,
}

fn role_of(who: Who) -> (&'static str, &'static str) {
    match who {
        Who::Known => ("panel", "admin"),
        Who::Reader => ("viewer", "read"),
        _ => ("", ""),
    }
}

/// Whether an error's code is the bus's for the HTTP status it came with:
/// the status the bus's own maps the code to -- or `invalid`, for a refusal
/// of HTTP's that no status of the bus's is.
fn code_fits(code: &str, status: u16) -> bool {
    let mapped = match code {
        "invalid" => 400,
        "forbidden" => 403,
        "no_provider" | "no_method" | "not_found" => 404,
        "kind" | "conflict" | "cancelled" => 409,
        "too_large" => 413,
        "busy" => 429,
        "failed" => 500,
        "unsupported" => 501,
        "gone" => 503,
        _ => return false,
    };
    mapped == status || (code == "invalid" && [405, 414, 426, 431, 505].contains(&status))
}

/// What every response must be, whatever it answers -- the answer to a
/// HEAD, `bodiless`, its head alone.
fn well_formed(r: &Response, bodiless: bool, what: &dyn Fn() -> String) {
    invariant!(r.header("Server") == Some("nos"), "{}: a {} without Server: nos", what(), r.status);
    if r.status == 101 {
        return;
    }
    invariant!(r.header("Cache-Control") == Some("no-store") && r.header("X-Content-Type-Options") == Some("nosniff"),
               "{}: a {} without no-store and nosniff: {:?}", what(), r.status, r.headers);
    if !r.body.is_empty() {
        invariant!(r.header("Content-Type") == Some("application/json"), "{}: a body without its type", what());
        let v = json::parse(&r.body, &READ);
        invariant!(v.is_ok(), "{}: a {} whose body is not JSON: {:?}", what(), r.status,
                   String::from_utf8_lossy(&r.body[..r.body.len().min(200)]));
    }
    if r.status >= 400 && !bodiless {
        let v = json::parse(&r.body, &READ).ok();
        let code = v.as_ref().and_then(|v| v.get("error")).and_then(|e| e.get("code")).and_then(Value::as_str);
        let message = v.as_ref().and_then(|v| v.get("error")).and_then(|e| e.get("message")).and_then(Value::as_str);
        invariant!(code.is_some_and(|c| code_fits(c, r.status)) && message.is_some_and(|m| !m.is_empty()),
                   "{}: a {} whose error is {:?}", what(), r.status, String::from_utf8_lossy(&r.body));
    }
    if r.status == 429 {
        invariant!(r.header("Retry-After") == Some("1"), "{}: a 429 without Retry-After", what());
    }
    if r.status == 405 {
        invariant!(r.header("Allow").is_some(), "{}: a 405 without Allow", what());
    }
}

/// The statuses the model has for request `k` of a client of `who`'s role,
/// its key on the list.
fn model(c: &Conv, k: usize) -> Vec<u16> {
    let admin = c.who == Who::Known;
    let went = &c.went[k];
    match &c.reqs[k].ask {
        Ask::Host | Ask::Providers | Ask::Jobs | Ask::Old => vec![200],
        Ask::Describe { name, tagged } => match descriptor_of(name) {
            None => vec![404],
            Some(_) if *tagged && went.etag.as_ref().is_some_and(|(_, given)| *given) => vec![304],
            Some(_) => vec![200],
        },
        Ask::Call { provider: "fuzzp", method: "look", body } => match body {
            Body::Tag => vec![200],
            Body::Pad(n) if *n as usize + LOOK_FRAME > MAX_REPLY => vec![413],
            Body::Pad(_) => vec![200],
            _ => vec![400],
        },
        Ask::Call { provider: "fuzzp", method: "poke", body } => match (admin, body) {
            (false, _) => vec![403],
            (true, Body::Tag) => vec![200],
            _ => vec![400],
        },
        Ask::Call { provider: "fuzzp", method: "work", .. } => vec![409],
        Ask::Call { provider: "system", .. } => vec![200],
        Ask::Call { .. } => vec![404],
        Ask::Start { provider: "fuzzp", method, .. } => match (admin, *method) {
            (false, _) => vec![403],
            (true, "work") => vec![202, 429],
            (true, "look") => vec![409],
            _ => vec![404],
        },
        Ask::Start { .. } => if admin { vec![404] } else { vec![403] },
        Ask::Job { .. } => if went.job == 999_999 { vec![404] } else { vec![200, 404] },
        Ask::Cancel { .. } => if admin { vec![200, 404] } else { vec![403] },
        Ask::Stream { shake, .. } => match shake {
            Shake::Ok => vec![101],
            Shake::NoKey => vec![400],
            Shake::Version | Shake::NoUpgrade => vec![426],
        },
        Ask::Wrong(i) => vec![WRONG[*i].2],
        Ask::Bad(i) => match *i {
            BAD_HEADER => vec![431],
            BAD_LINE => vec![414],
            i => vec![BAD[i].1],
        },
        Ask::Big => vec![413],
    }
}

/// What else an answer of the model's status must say, for its route.
fn judge_body(c: &Conv, k: usize, r: &Response, ctx: &Ctx, what: &dyn Fn() -> String) {
    let v = json::parse(&r.body, &READ).ok();
    let field = |key: &str| v.as_ref().and_then(|v| v.get(key));
    let went = &c.went[k];
    match (&c.reqs[k].ask, r.status) {
        (Ask::Host, 200) | (Ask::Old, 200) => {
            let client = field("client");
            let (name, role) = role_of(c.who);
            invariant!(client.and_then(|c| c.get("name")).and_then(Value::as_str) == Some(name)
                       && client.and_then(|c| c.get("role")).and_then(Value::as_str) == Some(role),
                       "{}: /v1/host says the client is {:?}", what(), client);
            invariant!(field("key").and_then(Value::as_str) == Some(ctx.pin_hex.as_str())
                       && field("module_abi").and_then(Value::as_str) == Some(kmod::ABI),
                       "{}: /v1/host's key or ABI is not the host's: {:?}", what(), String::from_utf8_lossy(&r.body));
        }
        (Ask::Providers, 200) => {
            let list = field("providers").and_then(Value::as_list).unwrap_or(&[]);
            let fuzzp = list.iter().find(|p| p.get("name").and_then(Value::as_str) == Some("fuzzp"));
            let methods: Vec<(&str, &str)> = fuzzp.and_then(|p| p.get("methods")).and_then(Value::as_list)
                .unwrap_or(&[]).iter().filter_map(|m| Some((m.get("name")?.as_str()?, m.get("kind")?.as_str()?)))
                .collect();
            let want: &[(&str, &str)] = if c.who == Who::Known {
                &[("look", "read"), ("poke", "action"), ("work", "job")]
            } else {
                &[("look", "read")]
            };
            invariant!(methods == want, "{}: fuzzp's methods listed as {:?}", what(), methods);
            if c.who != Who::Known {
                for p in list {
                    for m in p.get("methods").and_then(Value::as_list).unwrap_or(&[]) {
                        invariant!(m.get("kind").and_then(Value::as_str) == Some("read"),
                                   "{}: a reader shown {:?} of {:?}", what(), m.get("name"), p.get("name"));
                    }
                }
            }
        }
        (Ask::Describe { name, .. }, 200) => {
            let want = descriptor_of(name).unwrap_or("");
            invariant!(r.body == want.as_bytes(), "{}: {}'s descriptor came back {} bytes, not its {}", what(), name,
                       r.body.len(), want.len());
            let tag = r.header("ETag").unwrap_or("");
            invariant!(tag.len() == 18 && tag.starts_with('"') && tag.ends_with('"'), "{}: an ETag {:?}", what(), tag);
            if let Some((sent, true)) = &went.etag {
                invariant!(false, "{}: asked with its own ETag {}, it was answered 200 with {}", what(), sent, tag);
            }
        }
        (Ask::Describe { .. }, 304) => {
            let sent = went.etag.as_ref().map(|(t, _)| t.as_str());
            invariant!(r.header("ETag") == sent, "{}: a 304 with ETag {:?}, asked with {:?}", what(), r.header("ETag"),
                       sent);
        }
        (Ask::Call { method: "look", body, .. }, 200) => {
            let pad = match body {
                Body::Pad(n) => *n as usize,
                _ => 0,
            };
            invariant!(field("tag").and_then(Value::as_u64) == Some(went.tag)
                       && field("pad").and_then(Value::as_str).map(str::len) == Some(pad),
                       "{}: look's answer is not its own: {} bytes", what(), r.body.len());
        }
        (Ask::Call { method: "poke", .. }, 200) => {
            invariant!(field("poked").and_then(Value::as_u64) == Some(went.tag), "{}: poke answered {:?}", what(),
                       String::from_utf8_lossy(&r.body));
        }
        (Ask::Call { provider: "system", .. }, 200) => {
            invariant!(v.as_ref().and_then(Value::as_object).is_some(), "{}: a system read not an object", what());
        }
        (Ask::Start { .. }, 202) => {
            invariant!(field("id").and_then(Value::as_u64).is_some_and(|id| id > 0), "{}: a job started as {:?}",
                       what(), String::from_utf8_lossy(&r.body));
        }
        (Ask::Job { .. }, 200) => {
            invariant!(field("id").and_then(Value::as_u64) == Some(went.job)
                       && field("provider").and_then(Value::as_str) == Some("fuzzp"),
                       "{}: job {}'s record is {:?}", what(), went.job, String::from_utf8_lossy(&r.body));
        }
        (Ask::Stream { .. }, 101) => {
            let accept = httpc::accept_for(&went.key);
            invariant!(r.header("Upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
                       && r.header("Sec-WebSocket-Accept") == Some(accept.as_str()),
                       "{}: a 101 of {:?}, the key's accept {}", what(), r.headers, accept);
        }
        (Ask::Wrong(i), 405) => {
            invariant!(r.header("Allow") == WRONG[*i].3, "{}: Allow {:?} for {} {}", what(), r.header("Allow"),
                       WRONG[*i].0, WRONG[*i].1);
        }
        _ => {}
    }
    /* Closed after it exactly when the request said so, or the server must */
    let closes = c.closing(k) && !matches!(c.reqs[k].ask, Ask::Stream { shake: Shake::Ok, .. });
    invariant!((r.header("Connection") == Some("close")) == closes, "{}: Connection {:?} for a request that {}", what(),
               r.header("Connection"), if closes { "ends it" } else { "keeps it" });
}

/// How soon a stream sees its key off the list: within a turn of its loop.
const KEY_SEEN: u64 = SEC;

/// A stream's frames, held to what the server must send.
fn judge_stream(c: &Conv, ws: &Ws, k: usize, end: u64, tl: &Timeline, what: &dyn Fn() -> String) {
    invariant!(ws.broken.is_none(), "{}: the stream sent what is no WebSocket: {:?}", what(), ws.broken);
    let Ask::Stream { channels, log_since, .. } = &c.reqs[k].ask else { return };
    let channels = channels.unwrap_or("metrics,log,jobs");
    let asked = |ch: &str| channels.split(',').any(|x| x == ch);
    let mut last_seq: Option<u64> = log_since.map(|s| s.min(u64::MAX));
    let mut last_jobs: Option<Vec<u8>> = None;
    let mut metrics = 0;
    let mut pong = false;
    for (at, f) in &ws.frames {
        match f.opcode {
            OP_TEXT => {
                invariant!(tl.key(c.who, at.saturating_sub(KEY_SEEN), *at) != Some(false),
                           "{}: a message at {} ms, its key off the list since {} s before", what(),
                           (at - sched::START) / MS, KEY_SEEN / SEC);
                let v = json::parse(&f.payload, &READ);
                invariant!(v.is_ok(), "{}: a stream's message that is not JSON: {:?}", what(),
                           String::from_utf8_lossy(&f.payload[..f.payload.len().min(200)]));
                let v = v.unwrap_or(Value::Null);
                let ch = v.get("channel").and_then(Value::as_str).unwrap_or("");
                invariant!(asked(ch), "{}: a message of channel {:?}, {} asked for", what(), ch, channels);
                match ch {
                    "metrics" => {
                        metrics += 1;
                        for key in ["cpus", "memory", "interfaces"] {
                            invariant!(v.get(key).is_some_and(|x| x.as_object().is_some()),
                                       "{}: metrics without {} at {} s", what(), key, (at - sched::START) / SEC);
                        }
                    }
                    "log" => {
                        let lines = v.get("data").and_then(|d| d.get("lines")).and_then(Value::as_list).unwrap_or(&[]);
                        invariant!(!lines.is_empty(), "{}: a log message with no lines", what());
                        for l in lines {
                            let seq = l.get("seq").and_then(Value::as_u64).unwrap_or(0);
                            invariant!(last_seq.is_none_or(|s| seq > s), "{}: log line {} after {:?}: a line twice, or \
                                       out of order", what(), seq, last_seq);
                            last_seq = Some(seq);
                        }
                    }
                    "jobs" => {
                        let data = v.get("data").map(|d| format!("{:?}", d).into_bytes());
                        invariant!(data.is_some() && data != last_jobs, "{}: a jobs message that changed nothing",
                                   what());
                        last_jobs = data;
                    }
                    _ => {}
                }
            }
            OP_PING => invariant!(f.payload == b"nos", "{}: a ping of {:?}", what(), f.payload),
            OP_PONG => {
                invariant!(ws.ping.as_ref() == Some(&f.payload) && !pong, "{}: a pong of {:?}, pinged with {:?}",
                           what(), f.payload, ws.ping);
                pong = true;
            }
            OP_CLOSE => {
                let code = (f.payload.len() >= 2).then(|| u16::from_be_bytes([f.payload[0], f.payload[1]]));
                invariant!(code != Some(1008) || tl.key(c.who, ws.opened, *at) != Some(true),
                           "{}: closed 1008 with its key on the list", what());
            }
            _ => invariant!(false, "{}: a frame of opcode {} from the server", what(), f.opcode),
        }
    }
    let lasted = end.saturating_sub(ws.opened);
    if asked("metrics") && lasted >= 3 * SEC {
        invariant!(metrics >= 1, "{}: a stream of {} s with no metrics", what(), lasted / SEC);
    }
}

/// Everything a client's conversation must have come to.
fn judge(p: &Peer, tl: &Timeline, ctx: &Ctx, done: bool) {
    let c = &p.client;
    let Some(conv) = p.conv.as_ref() else { return };
    let conv = conv.borrow();
    let who = c.plan.who;
    let what = || format!("client {} ({:?} {:?}, from {} s)", conv.id, who, c.plan.brk, (p.at - sched::START) / SEC);
    /* Never a byte of HTTP for a key the server was not given, a client that
     * broke TLS itself -- or one the allowlist keeps out, which is reset
     * before a byte of TLS */
    if !ctx.allowed {
        invariant!(c.got.is_empty() && !c.up, "{}: let in past an allowlist that has not its address", what());
        return;
    }
    if !matches!(who, Who::Known | Who::Reader) || c.plan.brk != Break::None {
        invariant!(c.got.is_empty(), "{}: answered {} bytes", what(), c.got.len());
        return;
    }
    invariant!(conv.broken.is_none(), "{}: the server sent what is no HTTP: {:?}", what(), conv.broken);
    let end = p.over_at.unwrap_or(sched::now());
    let mut let_go = false;
    for (k, r) in conv.answers.iter().enumerate() {
        let went = &conv.went[k];
        let ask = &conv.reqs[k].ask;
        let what = || format!("{}, request {} {:?}", what(), k, ask);
        well_formed(r, conv.bodiless(k), &what);
        invariant!(!let_go, "{}: answered after the server let its key go", what());
        let answered = went.answered.unwrap_or(end);
        let model = model(&conv, k);
        /* A key off the list is let do nothing more: 403, and closed -- and
         * its message, but for a HEAD's, whose answer the client reads no
         * body of (the server, parsing nothing of a stranger's, sent one) */
        let refused = r.status == 403 && r.header("Connection") == Some("close")
            && (conv.bodiless(k) || json::parse(&r.body, &READ).ok()
                .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str)
                    .map(|m| m.contains("no more"))).unwrap_or(false));
        match tl.key(who, went.at, answered) {
            Some(true) => invariant!(model.contains(&r.status) && !refused, "{}: answered {}, the model {:?}", what(),
                                     r.status, model),
            Some(false) => invariant!(refused, "{}: answered {} with the key off the list", what(), r.status),
            None => invariant!(model.contains(&r.status) || refused, "{}: answered {}, the model {:?} or refused",
                               what(), r.status, model),
        }
        if refused {
            let_go = true;
            continue;
        }
        judge_body(&conv, k, r, ctx, &what);
        if went.held && model.contains(&r.status) && r.status < 400 {
            invariant!(went.continues == 1, "{}: its body held back for {} 100 Continues", what(), went.continues);
        }
        if !went.held {
            invariant!(went.continues == 0, "{}: {} 100 Continues to a request that asked for none", what(),
                       went.continues);
        }
        if let (Some(ws), 101) = (&conv.ws, r.status) {
            judge_stream(&conv, ws, k, end, tl, &what);
        }
    }

    /* Over a good link, with nothing taken from under it, every request is
     * answered and the client said goodbye */
    let touched = tl.down_within(p.at, end) || tl.off_within(who, p.at, end) || p.crowded || !ctx.lossless
                  || !ctx.allowed;
    if !done || touched {
        return;
    }
    let expected = (0..conv.reqs.len()).position(|k| conv.closing(k)).map_or(conv.reqs.len(), |k| k + 1);
    let expected = if let_go { conv.answers.len() } else { expected };
    invariant!(conv.answers.len() == expected, "{}: {} answers of {} ({:?}), ended {:?}", what(), conv.answers.len(),
               expected, conv.reqs.iter().map(|r| &r.ask).collect::<Vec<_>>(), c.ending);
    let stream = conv.ws.as_ref();
    /* What the stream did, if it came to it, and the server's close */
    match stream.map(|ws| (if ws.then_done { ws.plan.then } else { Then::Stay }, ws.closed)) {
        Some((Then::Unmasked, code)) => invariant!(code == Some(1002) && c.ending == Some(Ending::Gone),
                                                   "{}: an unmasked frame ended {:?}, closed {:?}", what(), c.ending,
                                                   code),
        Some((Then::TooBig, code)) => invariant!(code == Some(1009) && c.ending == Some(Ending::Gone),
                                                 "{}: a frame too big ended {:?}, closed {:?}", what(), c.ending, code),
        Some((_, Some(code))) => invariant!(code == 1000 && c.ending == Some(Ending::Closed),
                                            "{}: a stream closed {} and ended {:?}", what(), code, c.ending),
        /* Quiet for its idle time: let go then, and not before -- or it
         * said goodbye at the end first */
        Some((_, None)) if stream.is_some_and(|ws| !ws.plan.pongs) => {
            let said = stream.map_or(0, |ws| ws.said);
            let_go_idle(c, said, end, &what);
        }
        None if conv.linger => let_go_idle(c, conv.said, end, &what),
        _ => invariant!(c.ending == Some(Ending::Closed), "{}: ended {:?} after every answer", what(), c.ending),
    }
}

/// A connection quiet since `said`, which ended at `end`: let go by the
/// server when its idle time ran out -- within a read's wait of it -- or
/// gone with its goodbye before then.
fn let_go_idle(c: &Client, said: u64, end: u64, what: &dyn Fn() -> String) {
    let due = said + IDLE;
    match c.ending {
        Some(Ending::Gone) => invariant!(end + 100 * MS >= due && end <= due + 2 * SEC,
                                         "{}: quiet from {} ms, let go at {} ms, its idle time {} s", what(),
                                         (said - sched::START) / MS, (end - sched::START) / MS, IDLE_SECS),
        Some(Ending::Closed) => invariant!(end <= due + 2 * SEC, "{}: quiet from {} ms, never let go: it said \
                                           goodbye at {} ms", what(), (said - sched::START) / MS,
                                           (end - sched::START) / MS),
        _ => invariant!(false, "{}: a quiet connection ended {:?}", what(), c.ending),
    }
}

/* ---- the run ---- */

struct Setup {
    mounted: bool,
    written: bool,
    registered: bool,
    loaded: String,
    started: String,
}

static MODULE: Mutex<Option<Box<dyn kmod::Module>>> = Mutex::new(None);

fn module() -> MutexGuard<'static, Option<Box<dyn kmod::Module>>> {
    MODULE.lock().unwrap_or_else(|e| e.into_inner())
}

fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{:02x}", b)).collect()
}

/// The root, the providers, api.toml and the clients, the module and its
/// start: as /etc/rc brings them up.
fn setup(limits: Limits, allow: Option<bool>, identity: bool) -> Setup {
    let keys = keys();
    let mounted = fs::ramfs::mount_at("/", false);
    fs::init();
    system::init();
    let registered = mgmt::register("fuzzp", &FUZZP_METHODS, FUZZP_DESCRIPTOR, &FUZZP).map(core::mem::forget).is_ok();
    let mut toml = format!("port = {}\nlisten = [\"eth0\"]\nmax_connections = {}\nmax_per_key = {}\n\
                            max_handshakes = {}\nidle_secs = {}\n", PORT, limits.conns, limits.per_key,
                           limits.handshakes, IDLE_SECS);
    match allow {
        Some(true) => toml.push_str("allow_eth0 = [\"10.0.2.0/24\"]\n"),
        Some(false) => toml.push_str("allow_eth0 = [\"192.168.0.0/16\", \"10.0.2.1\"]\n"),
        None => {}
    }
    let known = tls::server::fingerprint(&keys.known.cert).expect("the known key's certificate");
    let reader = tls::server::fingerprint(&keys.reader.cert).expect("the reader's certificate");
    let clients = format!("{} admin panel\n{} read viewer\n", hex(&known), hex(&reader));
    let mut written = kcore::fs::create_dir("/etc").and_then(|_| kcore::fs::create_dir("/etc/api"))
        .and_then(|_| kcore::fs::create("/etc/api/api.toml", toml.as_bytes()))
        .and_then(|_| kcore::fs::create("/etc/api/clients", clients.as_bytes())).is_ok();
    if identity {
        let key = web::x509::pem("PRIVATE KEY", &keys.server.key).expect("a PEM");
        let cert = web::x509::pem("CERTIFICATE", &keys.server.cert).expect("a PEM");
        written &= kcore::fs::create_dir("/etc/api/tls")
            .and_then(|_| kcore::fs::create("/etc/api/tls/key.pem", key.as_bytes()))
            .and_then(|_| kcore::fs::create("/etc/api/tls/cert.pem", cert.as_bytes()))
            .and_then(|_| kcore::fs::create("/etc/api/host", b"5f1d0a5e-0000-4000-8000-00000000f022\n")).is_ok();
    }
    /* insmod, as the loader runs a module's init: what it says on the
     * command's output */
    let loader = kcore::cmd::Command::register("insmod-api", "insmod-api - the api module's source, loaded",
                                               |args, out| {
        if let Ok(m) = api::insmod_with(args, out) {
            sched::harness(|| *module() = Some(m));
        }
    });
    let loaded = match loader {
        Ok(_) => cmd::run("insmod-api"),
        Err(_) => "no command".to_string(),
    };
    drop(loader);
    let started = if module().is_some() { cmd::run("api start") } else { String::new() };
    Setup { mounted, written, registered, loaded, started }
}

/// The host key `api start` said.
fn pin_of(out: &str) -> Option<[u8; 32]> {
    let at = out.find("host key ")? + "host key ".len();
    let hex = out.get(at..at + 64)?;
    let mut key = [0u8; 32];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(key)
}

pub fn api(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);
    *did() = Did { looks: Vec::new(), pokes: Vec::new(), works: Vec::new() };
    let keys = keys();

    let limits = Limits { conns: r.pick(&[8usize, 8, 3, 32]), per_key: r.pick(&[4usize, 4, 1, 32]),
                          handshakes: r.pick(&[2usize, 2, 1, 32]) };
    let allow = match r.u8() % 32 {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    };
    let identity = r.bool();
    let setup = App::spawn("setup", 1, move || setup(limits, allow, identity));
    let link = if r.u8() < 200 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let mut lan = Lan::new();
    lan.add(CLIENT_IP, CLIENT_MAC);
    let peers: Rc<RefCell<Vec<Peer>>> = Rc::new(RefCell::new(Vec::new()));
    let mut w = World::new(link, Wire { lan, tcp: Tcp::new(CLIENT_IP, CLIENT_MAC), peers: peers.clone(), ticked: 0 });
    invariant!(w.wait_app(&setup, 60 * SEC), "the setup never finished");
    let s = setup.take().expect("done");
    let pin = pin_of(&s.started);
    invariant!(s.mounted && s.written && s.registered && module().is_some() && pin.is_some(),
               "api would not start: mounted {} written {} fuzzp {} -- {:?} {:?}", s.mounted, s.written, s.registered,
               s.loaded, s.started);
    let pin = pin.expect("checked");
    if identity {
        invariant!(Some(pin) == tls::server::fingerprint(&keys.server.cert), "a host key other than the one in its \
                   folder: {}", hex(&pin));
    }
    let ctx = Ctx { pin_hex: hex(&pin), limits, lossless, allowed: allow != Some(false) };
    w.run_for(BINDING);

    let finish = Rc::new(Cell::new(false));
    let mut tl = Timeline::default();
    let mut off: Vec<(Who, u64)> = Vec::new();
    let mut next_id = 1usize;
    while let Some(op) = r.op(10) {
        match op {
            0..=4 if peers.borrow().len() < CLIENTS => {
                let id = next_id;
                next_id += 1;
                let (mut plan, conv) = plan(r, id, &finish);
                plan.pin = Some(pin);
                let now = sched::now();
                let (live, handshaking, same) = {
                    let v = peers.borrow();
                    let live: Vec<&Peer> = v.iter().filter(|p| p.over_at.is_none_or(|t| now < t + REAP)).collect();
                    let answered = |p: &&&Peer| p.conv.as_ref().is_some_and(|c| !c.borrow().answers.is_empty());
                    (live.len(), live.iter().filter(|p| !answered(p)).count(),
                     live.iter().filter(|p| p.client.plan.who == plan.who).count())
                };
                let crowded = live >= limits.conns || handshaking >= limits.handshakes || same >= limits.per_key;
                /* A key's count is had as each handshake ends, not in the
                 * order they began: one of its clients still unanswered may
                 * lose to this one */
                if same >= limits.per_key {
                    for p in peers.borrow_mut().iter_mut() {
                        let unanswered = p.conv.as_ref().is_some_and(|c| c.borrow().answers.is_empty());
                        if p.client.plan.who == plan.who && p.over_at.is_none_or(|t| now < t + REAP) && unanswered {
                            p.crowded = true;
                        }
                    }
                }
                let port = 40000 + id as u16;
                let iss = r.u32();
                let window = r.pick(&[65535u32, 65535, 8192, 1024]);
                let ep = w.peers.tcp.connect(&mut w.net, 50_000 + id, port, ETH0_IP, PORT, iss,
                                             crate::world::frames::mss(1460), window);
                w.peers.tcp.eps[ep].data = Some(Data::default());
                let client = match &conv {
                    Some(c) => Client::talking(ep, plan, Box::new(Http(c.clone()))),
                    None => Client::new(ep, plan),
                };
                peers.borrow_mut().push(Peer { client, conv, at: now, over_at: None, crowded });
                w.pump();
            }
            5 => {
                /* The console, at the API */
                let now = sched::now();
                match r.u8() % 9 {
                    8 => {
                        /* A list it must refuse, read again: the one it has
                         * stays -- and then the good one, back */
                        let bad = bad_clients(r.u8());
                        let app = App::spawn("write", 1, move || kcore::fs::write("/etc/api/clients", bad.as_bytes()));
                        invariant!(w.wait_app(&app, 30 * SEC) && app.take() == Some(Ok(())),
                                   "the clients file not written");
                        let out = console(&mut w, "api reload");
                        invariant!(!out.contains("clients let in"), "a clients file it should refuse taken: {:?}", out);
                        let good = clients_file(&off);
                        let app = App::spawn("write", 1, move || kcore::fs::write("/etc/api/clients", good.as_bytes()));
                        invariant!(w.wait_app(&app, 30 * SEC) && app.take() == Some(Ok(())),
                                   "the clients file not written");
                        let out = console(&mut w, "api reload");
                        invariant!(out.contains("clients let in"), "the clients file back: {:?}", out);
                    }
                    0 => {
                        let out = console(&mut w, "api");
                        invariant!(out.contains("serving port"), "`api` says {:?}", out);
                    }
                    1 | 2 => {
                        /* A key off the list, under the connections that
                         * hold it, a while, and on it again -- or left off
                         * until it is put back */
                        let who = if r.bool() { Who::Reader } else { Who::Known };
                        if let Some(i) = off.iter().position(|(w, _)| *w == who) {
                            let (_, from) = off.remove(i);
                            add_back(&mut w, who);
                            tl.off.push((who, from, sched::now()));
                        } else {
                            let name = role_of(who).0;
                            let out = console(&mut w, &format!("api client del {}", name));
                            invariant!(out.contains("is let in no more"), "`api client del {}`: {:?}", name, out);
                            if r.u8() < 192 {
                                w.run_for(r.below(4000) * MS);
                                add_back(&mut w, who);
                                tl.off.push((who, now, sched::now()));
                            } else {
                                off.push((who, now));
                            }
                        }
                    }
                    3 => {
                        /* Stopped under whoever is connected, a while with
                         * the port closed, and started again */
                        let out = console(&mut w, "api stop");
                        invariant!(out.contains("api: stopped"), "`api stop`: {:?}", out);
                        invariant!(sched::now() - now <= STOPPING, "`api stop` took {} ms", (sched::now() - now) / MS);
                        let out = console(&mut w, "api");
                        invariant!(out.contains("not serving"), "`api` after the stop: {:?}", out);
                        w.run_for(r.below(4000) * MS);
                        let out = console(&mut w, "api start");
                        invariant!(pin_of(&out) == Some(pin), "`api start` again: {:?}, the host key {}", out,
                                   hex(&pin));
                        tl.down.push((now, sched::now() + BINDING));
                    }
                    4 => {
                        let out = console(&mut w, "api reload");
                        invariant!(out.contains("clients let in"), "`api reload`: {:?}", out);
                    }
                    5 => {
                        let out = console(&mut w, "api cert");
                        invariant!(out.contains("BEGIN CERTIFICATE") && out.contains(&ctx.pin_hex), "`api cert`: {:?}",
                                   out);
                    }
                    _ => {
                        let out = console(&mut w, "api clients");
                        let keys = out.lines().filter(|l| l.split(' ').next().is_some_and(|k| k.len() == 64)).count();
                        invariant!(keys == 2 - off.len(), "`api clients`, {} off: {:?}", off.len(), out);
                    }
                }
            }
            _ => {
                let step = match r.u8() % 8 {
                    0..=5 => r.below(2000) * MS,
                    6 => r.below(8) * SEC,
                    _ => r.pick(&[11u64, 25]) * SEC,
                };
                w.run_for(step);
            }
        }
        held_to_limits(&mut w, &limits);
        if lossless {
            /* Never more open at once than api.toml allows: a connection
             * answered and not over is one the server counts */
            let v = peers.borrow();
            let open: Vec<Who> = v.iter().filter(|p| p.client.ending.is_none()
                && p.conv.as_ref().is_some_and(|c| !c.borrow().answers.is_empty()))
                .map(|p| p.client.plan.who).collect();
            invariant!(open.len() <= limits.conns, "{} connections open at once, api.toml allowing {}", open.len(),
                       limits.conns);
            for who in [Who::Known, Who::Reader] {
                let n = open.iter().filter(|w| **w == who).count();
                invariant!(n <= limits.per_key, "{} connections of {:?}'s key open at once, api.toml allowing {}", n,
                           who, limits.per_key);
            }
        }
    }
    /* The world as it was: every key on the list */
    for (who, from) in std::mem::take(&mut off) {
        add_back(&mut w, who);
        tl.off.push((who, from, sched::now()));
    }

    /* Every client finishes, over a good link: a stream, a client that
     * lingers, says goodbye; the rest have their answers, or their refusal */
    w.net.link = Link::perfect();
    finish.set(true);
    let all_over = |peers: &Rc<RefCell<Vec<Peer>>>| peers.borrow().iter().all(|p| p.client.over());
    {
        /* A turn for every client, to say its goodbye now that it is to */
        let mut v = peers.borrow_mut();
        let wire = &mut w.peers;
        for p in v.iter_mut() {
            p.client.pump(&mut wire.tcp, &mut w.net);
        }
    }
    w.run_until(sched::now() + 2 * HANDSHAKE + 2 * IDLE + 30 * SEC, || all_over(&peers));
    w.run_for(3 * SEC);
    for p in peers.borrow().iter() {
        if lossless {
            invariant!(p.client.over(), "a client from {} s never done: {:?} {:?}, up {}, got {} bytes",
                       (p.at - sched::START) / SEC, p.client.plan.who, p.client.plan.brk, p.client.up,
                       p.client.got.len());
        }
        judge(p, &tl, &ctx, p.client.over());
    }

    /* The server never closes first: no slot of its port in TIME-WAIT */
    for i in 0..net::tcp::MAX_CONNECTIONS {
        if let Some(c) = net::tcp::TCP.snapshot(i) {
            invariant!(!(c.local_port == PORT && c.state == net::tcp::State::TimeWait),
                       "the server's side of a connection from port {} is in TIME-WAIT", c.remote_port);
        }
    }

    /* What ran is what admins asked for, and the audit log says it all */
    w.run_for((WORK_MS_MAX + 1000) * MS);
    let (pokes, works) = {
        let d = did();
        (d.pokes.clone(), d.works.clone())
    };
    let asked = |tag: u64, f: &dyn Fn(&Ask) -> bool| {
        peers.borrow().iter().filter_map(|p| p.conv.clone()).any(|c| {
            let c = c.borrow();
            c.who == Who::Known && c.went.iter().enumerate().any(|(k, w)| w.tag == tag && f(&c.reqs[k].ask))
        })
    };
    for (i, t) in pokes.iter().enumerate() {
        invariant!(asked(*t, &|a| matches!(a, Ask::Call { method: "poke", body: Body::Tag, .. })),
                   "a poke ran with tag {:#x}, which no admin's request had", t);
        invariant!(!pokes[..i].contains(t), "a poke ran twice with tag {:#x}", t);
    }
    for (i, t) in works.iter().enumerate() {
        invariant!(asked(*t, &|a| matches!(a, Ask::Start { method: "work", .. })),
                   "a job ran with tag {:#x}, which no admin's request had", t);
        invariant!(!works[..i].contains(t), "a job ran twice with tag {:#x}", t);
    }
    for p in peers.borrow().iter() {
        let Some(c) = p.conv.as_ref() else { continue };
        let c = c.borrow();
        for (k, r) in c.answers.iter().enumerate() {
            if let (Ask::Call { method: "poke", .. }, 200) = (&c.reqs[k].ask, r.status) {
                invariant!(pokes.contains(&c.went[k].tag), "client {}'s poke {:#x} answered 200, and never ran", c.id,
                           c.went[k].tag);
            }
        }
    }
    let app = App::spawn("audit", 1, || kcore::fs::read("/etc/api/audit.log", 1 << 20).unwrap_or_default());
    invariant!(w.wait_app(&app, 60 * SEC), "the audit log never read");
    let log = String::from_utf8_lossy(&app.take().unwrap_or_default()).into_owned();
    let poked = log.lines().filter(|l| l.ends_with(" call fuzzp.poke ok")).count();
    invariant!(poked == pokes.len(), "{} pokes ran, the audit log says {}: {:?}", pokes.len(), poked, log);
    invariant!(!log.lines().any(|l| l.split(' ').nth(1) != Some("panel")),
               "the audit log names another than the admin: {:?}", log);
    invariant!(crate::machine::ffi::POWER_REQUESTS.load(std::sync::atomic::Ordering::Relaxed) == 0,
               "a power-off or a reboot was asked for");

    /* The server stops, the module goes, and nothing is left behind */
    let out = console(&mut w, "api stop");
    invariant!(out.contains("api: stopped"), "`api stop` at the end: {:?}", out);
    let m = module().take();
    let app = App::spawn("rmmod", 1, move || drop(m));
    invariant!(w.wait_app(&app, 120 * SEC), "rmmod api never returned");
    for e in 0..w.peers.tcp.eps.len() {
        if w.peers.tcp.eps[e].open() {
            w.peers.tcp.close(&mut w.net, e);
        }
    }
    w.run_for(200 * SEC);
    super::audit();
}
