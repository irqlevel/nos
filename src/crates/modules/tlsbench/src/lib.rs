#![no_std]

//! tlsbench: what TLS 1.3 costs this machine's CPUs. Both pinned ends of
//! `tls::pinned` -- a server and a client, each with a key of its own made
//! for the run -- in one kernel, over a pipe in memory: nothing measured but
//! the handshake, the crypto and the session's own work. ipcnet's links
//! between hosts are such sessions (plans/ipc.md, I4), and the kernel has no
//! SSE: every suite runs on its portable backend, and which is cheapest here
//! is this command's to say.
//!
//!     insmod /tlsbench.ko
//!     tlsbench                    every suite, 16 MB each way
//!     tlsbench mb=64 suite=chacha
//!
//! For each suite: the handshake's time as the client waits it, and MB/s
//! each way -- the sender encrypting in one task, the receiver decrypting in
//! another, as two hosts would, each on a CPU of its own where there are
//! two. The suite the two agreed on is checked to be the one asked for.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::cmd::{Command, Output};
use kcore::sync::{Event, Mutex};
use kcore::time::{boot_time_ns, Duration};
use tls::pinned::{Client, Fingerprint, Flush, Read, Recv, Server, Socket, Suite, HANDSHAKE_MS};

const HELP: &str = "tlsbench [mb=16] [suite=aes128|aes256|chacha] - time TLS 1.3 here, both pinned ends over memory";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const MB: u64 = 1 << 20;
const DEFAULT_MB: u64 = 16;
const MAX_MB: u64 = 1024;
/// What one direction of the pipe holds at most: a heap block's worth, well
/// under the kernel's largest.
const PIPE_CAP: usize = 128 * 1024;
/// What a write hands the session at a time: a record's worth.
const CHUNK: usize = 16 * 1024;
/// What waits encrypted, at most, before the writer flushes.
const MAX_PENDING: usize = 64 * 1024;
/// How long any one wait of the run may take before it is called stuck.
const WAIT_MS: u64 = 10_000;
const ALPN: &[u8] = b"tlsbench";
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;

/// One direction of the pipe: bytes in order, a bounded number of them, one
/// reader and one writer -- each event has one waiter.
struct Pipe {
    q: Mutex<VecDeque<u8>>,
    /// The reader's: bytes have come, or the writer has gone
    data: Event,
    /// The writer's: room has come, or the reader has gone
    room: Event,
    closed: AtomicBool,
}

impl Pipe {
    fn new() -> Option<Arc<Pipe>> {
        let mut q = VecDeque::new();
        q.try_reserve_exact(PIPE_CAP).ok()?;
        Some(Arc::new(Pipe { q: Mutex::new(q)?, data: Event::new()?, room: Event::new()?, closed: AtomicBool::new(false) }))
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.data.signal();
        self.room.signal();
    }
}

/// One end of the pipe, as a session runs over it.
struct End {
    rx: Arc<Pipe>,
    tx: Arc<Pipe>,
}

impl Socket for End {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv {
        let until = boot_time_ns().saturating_add(wait_ms.saturating_mul(NS_PER_MS));
        loop {
            let got = {
                let mut q = self.rx.q.lock();
                let n = q.len().min(buf.len());
                for (to, from) in buf.iter_mut().zip(q.drain(..n)) {
                    *to = from;
                }
                n
            };
            if got > 0 {
                self.rx.room.signal();
                return Recv::Data(got);
            }
            if self.rx.closed.load(Ordering::Acquire) {
                return Recv::End;
            }
            let now = boot_time_ns();
            if now >= until {
                return Recv::Idle;
            }
            self.rx.data.wait_for(Duration::from_nanos(until - now));
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        let until = boot_time_ns().saturating_add(wait_ms.saturating_mul(NS_PER_MS));
        loop {
            if self.tx.closed.load(Ordering::Acquire) {
                return None;
            }
            let put = {
                let mut q = self.tx.q.lock();
                let n = (PIPE_CAP - q.len().min(PIPE_CAP)).min(buf.len());
                q.extend(buf[..n].iter().copied());
                n
            };
            if put > 0 {
                self.tx.data.signal();
                return Some(put);
            }
            let now = boot_time_ns();
            if now >= until {
                return Some(0);
            }
            self.tx.room.wait_for(Duration::from_nanos(until - now));
        }
    }
}

/// Both directions closed once an end is done with them: what a FIN and
/// the reader going would be.
impl Drop for End {
    fn drop(&mut self) {
        self.tx.close();
        self.rx.close();
    }
}

/// What the server's task saw, for the client's side to read once it ends.
struct Seen {
    /// When the last of the client's bytes had come (boot ns)
    read_done: AtomicU64,
    /// When the server began sending its own
    write_start: AtomicU64,
    ok: AtomicBool,
}

/// The server's side of a run: let the client in, take `total` bytes, send
/// `total` back, say goodbye.
fn serve((server, end, total, seen): (Arc<Server>, End, u64, Arc<Seen>)) {
    let Ok(mut s) = server.accept(end, HANDSHAKE_MS) else { return };
    let mut buf = vec![0u8; CHUNK];
    let mut got = 0u64;
    while got < total {
        match s.read(&mut buf, WAIT_MS) {
            Read::Data(n) => got += n as u64,
            _ => return,
        }
    }
    seen.read_done.store(boot_time_ns(), Ordering::Release);
    seen.write_start.store(boot_time_ns(), Ordering::Release);
    if !send_all(&mut s, total) {
        return;
    }
    seen.ok.store(true, Ordering::Release);
    /* The client says goodbye first; its FIN, then the server's */
    let mut tail = [0u8; 64];
    while let Read::Data(_) = s.read(&mut tail, WAIT_MS) {}
    let _ = s.close(WAIT_MS);
}

/// `total` bytes through the session `s`, a chunk at a time, all of it out.
fn send_all<E: tls::pinned::End>(s: &mut tls::pinned::Session<End, E>, total: u64) -> bool {
    let chunk = [0x5Au8; CHUNK];
    let mut sent = 0u64;
    while sent < total {
        let n = CHUNK.min((total - sent) as usize);
        while !s.write(&chunk[..n], MAX_PENDING) {
            if s.flush(WAIT_MS) == Flush::Failed {
                return false;
            }
        }
        sent += n as u64;
    }
    s.flush(WAIT_MS) == Flush::Done
}

/// The two ends' keys, made for the run.
struct Ids {
    server: web::x509::Identity,
    client: web::x509::Identity,
    server_fp: Fingerprint,
    client_fp: Fingerprint,
}

fn identity(name: &str, usage: web::x509::Usage) -> Result<web::x509::Identity, String> {
    let mut serial = [0u8; 16];
    if !kcore::random::fill_random(&mut serial) {
        return Err(String::from("no randomness: the kernel's pool has no source"));
    }
    serial[0] &= 0x7F;
    /* Nothing judges the dates -- the pinned ends take the key alone */
    let req = web::x509::Request { name, ips: &[], dns: &[], serial, not_before: 1_767_225_600,
                                   not_after: 2_524_608_000, usage };
    let mut secret = [0u8; 32];
    for _ in 0..8 {
        if !kcore::random::fill_random(&mut secret) {
            break;
        }
        match web::x509::self_signed(&secret, &req) {
            Ok(id) => return Ok(id),
            Err(web::x509::Error::BadKey) => continue,
            Err(e) => return Err(alloc::format!("{}'s certificate: {:?}", name, e)),
        }
    }
    Err(alloc::format!("no key for {}", name))
}

fn ids() -> Result<Ids, String> {
    let server = identity("tlsbench server", web::x509::Usage::Server)?;
    let client = identity("tlsbench client", web::x509::Usage::Client)?;
    let server_fp = tls::pinned::fingerprint(&server.cert).ok_or("the server's certificate unreadable")?;
    let client_fp = tls::pinned::fingerprint(&client.cert).ok_or("the client's certificate unreadable")?;
    Ok(Ids { server, client, server_fp, client_fp })
}

/// MB/s of `bytes` in `ns`, as tenths.
fn rate(bytes: u64, ns: u64) -> (u64, u64) {
    let tenths = bytes.saturating_mul(10).saturating_mul(NS_PER_SEC) / ns.max(1) / MB;
    (tenths / 10, tenths % 10)
}

fn one(suite: Suite, mb: u64, ids: &Ids, out: &mut Output) -> Result<(), String> {
    let total = mb * MB;
    let server = Server::with_suites(&ids.server.cert, &ids.server.key, &[ids.client_fp], &[ALPN], &[suite])
        .map_err(|e| alloc::format!("server: {:?}", e))?;
    let client = Client::with_suites(&ids.client.cert, &ids.client.key, &[ids.server_fp], &[ALPN], &[suite])
        .map_err(|e| alloc::format!("client: {:?}", e))?;
    let (a, b) = (Pipe::new().ok_or("no memory for a pipe")?, Pipe::new().ok_or("no memory for a pipe")?);
    let seen = Arc::new(Seen { read_done: AtomicU64::new(0), write_start: AtomicU64::new(0), ok: AtomicBool::new(false) });
    let server_end = End { rx: a.clone(), tx: b.clone() };
    let task = kcore::task::spawn_with("tlsbench/srv", (Arc::new(server), server_end, total, seen.clone()), serve)
        .ok_or("no task for the server")?;

    let t0 = boot_time_ns();
    let mut s = match client.connect(End { rx: b, tx: a }, 0x7F00_0001, HANDSHAKE_MS) {
        Ok(s) => s,
        Err((_, why)) => {
            drop(task);
            return Err(alloc::format!("{}: the handshake refused: {}", suite.name(), why.name()));
        }
    };
    let handshake = boot_time_ns() - t0;
    let agreed = s.suite();
    let t1 = boot_time_ns();
    if !send_all(&mut s, total) {
        drop(s);
        drop(task);
        return Err(alloc::format!("{}: the client's sending failed", suite.name()));
    }
    let mut buf = vec![0u8; CHUNK];
    let mut got = 0u64;
    while got < total {
        match s.read(&mut buf, WAIT_MS) {
            Read::Data(n) => got += n as u64,
            other => {
                drop(s);
                drop(task);
                return Err(alloc::format!("{}: the client's reading ended {:?} at {} of {} bytes", suite.name(), other,
                                          got, total));
            }
        }
    }
    let t3 = boot_time_ns();
    let _ = s.close(WAIT_MS);
    /* The server's task ends with the session: waited for here */
    drop(task);
    if !seen.ok.load(Ordering::Acquire) {
        return Err(alloc::format!("{}: the server's side did not finish", suite.name()));
    }
    if agreed != Some(suite) {
        return Err(alloc::format!("{}: the two agreed on {:?}", suite.name(), agreed.map(|s| s.name())));
    }
    let up = rate(total, seen.read_done.load(Ordering::Acquire).saturating_sub(t1));
    let down = rate(total, t3.saturating_sub(seen.write_start.load(Ordering::Acquire)));
    let _ = writeln!(out, "{:<18} handshake {}.{} ms; {} MB client->server {}.{} MB/s, server->client {}.{} MB/s",
                     suite.name(), handshake / NS_PER_MS, (handshake % NS_PER_MS) / 100_000, mb, up.0, up.1, down.0,
                     down.1);
    Ok(())
}

fn run(args: &str, out: &mut Output) -> Result<(), String> {
    let mut mb = DEFAULT_MB;
    let mut suites: Vec<Suite> = Suite::ALL.to_vec();
    for word in args.split_whitespace() {
        if let Some(v) = word.strip_prefix("mb=") {
            mb = v.parse().ok().filter(|m| (1..=MAX_MB).contains(m)).ok_or("mb is 1 to 1024")?;
        } else if let Some(v) = word.strip_prefix("suite=") {
            suites = match v {
                "aes128" => vec![Suite::Aes128Gcm],
                "aes256" => vec![Suite::Aes256Gcm],
                "chacha" => vec![Suite::ChaCha20Poly1305],
                "all" => Suite::ALL.to_vec(),
                _ => return Err(String::from("suite is aes128, aes256, chacha or all")),
            };
        } else {
            return Err(String::from(HELP));
        }
    }
    let ids = ids()?;
    let _ = writeln!(out, "tlsbench: {} CPUs, both pinned ends over memory", kcore::cpu::count());
    for suite in suites {
        one(suite, mb, &ids, out)?;
    }
    Ok(())
}

struct TlsBench {
    _cmd: Command,
}

impl kmod::Module for TlsBench {}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let cmd = Command::register("tlsbench", HELP, |args, out| {
        if let Err(problem) = run(args, out) {
            let _ = writeln!(out, "tlsbench: {}", problem);
        }
    })?;
    Ok(Box::new(TlsBench { _cmd: cmd }))
}

kmod::module!(name: "tlsbench", init: init);
