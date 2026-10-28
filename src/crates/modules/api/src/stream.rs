//! /v1/stream: a WebSocket a client keeps open for what changes --
//!
//! - `metrics`, once a second: each CPU's idle time, the memory, every
//!   interface's counters, as the system and net providers answer them (a
//!   client makes its rates from two of them; its history is its own);
//! - `log`, the kernel's log from where the client asks (`log_since`) -- or
//!   from now -- by its messages' numbers, a gap in them what the log lost;
//! - `jobs`, every job's record, whenever one has changed.
//!
//! `?channels=metrics,log` picks; all three unless asked. Each message is
//! a text frame of JSON, `{"channel": ..., ...}`. The client says nothing
//! but its pongs and its close; the server pings it every half of its idle
//! time, and one that answers nothing for the whole of it is let go. Every
//! channel is a read: a client of either role may open it.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use kcore::mgmt::{self as bus, Status, Writer};
use kcore::time::boot_time_ns;
use mgmtwire::{json, Limits};
use tls::server::Read;
use web::http::Request;
use web::ws::{Decoder, Event, Opcode};

use crate::http::{send, Ending, Session, Who};
use crate::server::Shared;

const NS_PER_MS: u64 = 1_000_000;
const METRICS_NS: u64 = 1_000 * NS_PER_MS;
const LOG_NS: u64 = 500 * NS_PER_MS;
const JOBS_NS: u64 = 1_000 * NS_PER_MS;
/// The longest a read waits: how soon the stream sees what is due.
const READ_MS: u64 = 250;
/// What a client's message may be: it sends none but control frames.
const MESSAGE_MAX: usize = 4096;
/// The log's bytes one message carries, at most.
const LOG_BYTES: u64 = 16 * 1024;
/// What a provider's answer is read as before it is handed on as JSON.
const ANSWER_LIMITS: Limits = Limits { bytes: 256 * 1024, depth: 16, values: 1 << 16, members: 4096, string: 64 * 1024 };
/// Going away: the server stops.
const CLOSE_GOING_AWAY: u16 = 1001;
const CLOSE_NORMAL: u16 = 1000;

pub struct Params {
    accept: [u8; 28],
    metrics: bool,
    log: bool,
    jobs: bool,
    log_since: Option<u64>,
}

impl Params {
    pub fn of(req: &Request, accept: [u8; 28]) -> Params {
        let channels = req.query("channels").unwrap_or("metrics,log,jobs");
        let has = |c: &str| channels.split(',').any(|x| x == c);
        Params {
            accept,
            metrics: has("metrics"),
            log: has("log"),
            jobs: has("jobs"),
            log_since: req.query("log_since").and_then(|v| v.parse::<u64>().ok()),
        }
    }
}

/// A provider's read, its answer kept if it is JSON as the bus reads it.
fn read(provider: &str, method: &str, params: &[u8]) -> Option<Vec<u8>> {
    let mut got = Vec::new();
    let status = bus::call(provider, method, params, 256 * 1024, &mut |b| {
        got.try_reserve(b.len()).is_ok() && {
            got.extend_from_slice(b);
            true
        }
    });
    (status == Status::Ok && json::parse(&got, &ANSWER_LIMITS).is_ok()).then_some(got)
}

/// `{"channel": <name>, <fields>...}` as a text frame, sent.
fn message(s: &mut Session, stall_ns: u64, write: impl FnOnce(&mut Writer)) -> bool {
    let mut body = Vec::new();
    let mut sink = |b: &[u8]| {
        body.try_reserve(b.len()).is_ok() && {
            body.extend_from_slice(b);
            true
        }
    };
    let mut w = Writer::new(&mut sink);
    write(&mut w);
    if !w.ok() {
        return true;
    }
    let mut frame = Vec::new();
    if web::ws::write_frame(&mut frame, Opcode::Text, &body).is_err() {
        return false;
    }
    send(s, &frame, stall_ns)
}

fn raw_or_null(w: &mut Writer, key: &str, value: &Option<Vec<u8>>) {
    w.key(key);
    match value {
        Some(v) => w.raw_value(v),
        None => w.null(),
    }
}

pub fn run(shared: &Shared, s: &mut Session, _who: &Who, p: Params, idle_ns: u64) -> Ending {
    let accept = core::str::from_utf8(&p.accept).unwrap_or("");
    let mut head = Vec::new();
    let headers = [("Upgrade", "websocket"), ("Connection", "Upgrade"), ("Sec-WebSocket-Accept", accept), ("Server", "nos")];
    if web::http::write_head(&mut head, 101, &headers, None).is_err() || !send(s, &head, idle_ns) {
        return Ending::Reset;
    }
    let mut decoder = Decoder::new(MESSAGE_MAX);
    let mut incoming: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    let mut since = p.log_since.unwrap_or_else(|| kcore::system::log_next_seq().saturating_sub(1));
    let start = boot_time_ns();
    let (mut next_metrics, mut next_log, mut next_jobs) = (start, start, start);
    let mut next_ping = start + idle_ns / 2;
    let mut heard = start;
    let mut last_jobs: Vec<u8> = Vec::new();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            close(s, CLOSE_GOING_AWAY, "the server stops", idle_ns);
            return Ending::Reset;
        }
        let now = boot_time_ns();
        if p.metrics && now >= next_metrics {
            next_metrics = now + METRICS_NS;
            let cpus = read("system", "cpus", b"");
            let memory = read("system", "memory", b"");
            let interfaces = read("net", "interfaces", b"");
            if !message(s, idle_ns, |w| {
                w.begin_object();
                w.field_str("channel", "metrics");
                w.field_uint("uptime_ms", now / NS_PER_MS);
                raw_or_null(w, "cpus", &cpus);
                raw_or_null(w, "memory", &memory);
                raw_or_null(w, "interfaces", &interfaces);
                w.end_object();
            }) {
                return Ending::Reset;
            }
        }
        if p.log && now >= next_log {
            next_log = now + LOG_NS;
            let params = alloc::format!("{{\"since\":{},\"max_bytes\":{}}}", since, LOG_BYTES);
            if let Some(answer) = read("system", "log", params.as_bytes()) {
                let parsed = json::parse(&answer, &ANSWER_LIMITS).ok();
                let last = parsed.as_ref().and_then(|v| v.get("last_seq")).and_then(|v| v.as_u64()).unwrap_or(since);
                let lines = parsed.as_ref().and_then(|v| v.get("lines")).and_then(|v| v.as_list()).map_or(0, |l| l.len());
                if lines != 0 {
                    since = last;
                    let data = Some(answer);
                    if !message(s, idle_ns, |w| {
                        w.begin_object();
                        w.field_str("channel", "log");
                        raw_or_null(w, "data", &data);
                        w.end_object();
                    }) {
                        return Ending::Reset;
                    }
                }
            }
        }
        if p.jobs && now >= next_jobs {
            next_jobs = now + JOBS_NS;
            let mut jobs = Vec::new();
            let status = bus::jobs(&mut |b| {
                jobs.try_reserve(b.len()).is_ok() && {
                    jobs.extend_from_slice(b);
                    true
                }
            });
            if status == Status::Ok && jobs != last_jobs && json::parse(&jobs, &ANSWER_LIMITS).is_ok() {
                let data = Some(jobs.clone());
                last_jobs = jobs;
                if !message(s, idle_ns, |w| {
                    w.begin_object();
                    w.field_str("channel", "jobs");
                    raw_or_null(w, "data", &data);
                    w.end_object();
                }) {
                    return Ending::Reset;
                }
            }
        }
        if now >= next_ping {
            next_ping = now + idle_ns / 2;
            let mut frame = Vec::new();
            if web::ws::write_frame(&mut frame, Opcode::Ping, b"nos").is_err() || !send(s, &frame, idle_ns) {
                return Ending::Reset;
            }
        }
        if boot_time_ns().saturating_sub(heard) >= idle_ns {
            return Ending::Reset;
        }
        match s.read(&mut chunk, READ_MS) {
            Read::Data(n) => {
                if incoming.len() + n > 2 * MESSAGE_MAX + 16 || incoming.try_reserve(n).is_err() {
                    close(s, web::ws::WsError::TooBig.close_code(), "more than a stream's client sends", idle_ns);
                    return Ending::Reset;
                }
                incoming.extend_from_slice(&chunk[..n]);
                heard = boot_time_ns();
                loop {
                    match decoder.feed(&mut incoming) {
                        Ok(None) => break,
                        Ok(Some((event, used))) => {
                            let mut reply = Vec::new();
                            let mut done = false;
                            match event {
                                Some(Event::Ping(data)) => {
                                    if web::ws::write_frame(&mut reply, Opcode::Pong, data).is_err() {
                                        return Ending::Reset;
                                    }
                                }
                                Some(Event::Close { .. }) => {
                                    let _ = web::ws::write_close(&mut reply, CLOSE_NORMAL, "");
                                    done = true;
                                }
                                Some(Event::Pong(_)) | Some(Event::Message { .. }) | None => {}
                            }
                            incoming.drain(..used);
                            if !reply.is_empty() && !send(s, &reply, idle_ns) {
                                return Ending::Reset;
                            }
                            if done {
                                return Ending::Close;
                            }
                        }
                        Err(e) => {
                            close(s, e.close_code(), "a frame the server will not take", idle_ns);
                            return Ending::Reset;
                        }
                    }
                }
            }
            Read::Idle => {}
            Read::End => return Ending::Close,
            Read::Failed => return Ending::Reset,
        }
    }
}

/// A close frame, sent if it can be: what the client is told before the
/// reset.
fn close(s: &mut Session, code: u16, reason: &str, stall_ns: u64) {
    let mut frame = Vec::new();
    if web::ws::write_close(&mut frame, code, reason).is_ok() {
        let _ = send(s, &frame, stall_ns.min(1_000 * NS_PER_MS));
    }
}
