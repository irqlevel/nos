//! `hv forward`: a port of nos's, relayed to a port of a guest's -- how a
//! server in a guest is reached from outside, the guests having addresses
//! only on their switch (10.0.100.0/24, behind `hv0`).
//!
//! Each forward is a task that listens on nos's port and accepts; for each
//! connection it opens one to the guest's port through `hv0`, and hands the
//! two to a relay task of their own, which carries what each side sends to
//! the other until either ends. The relay alternates between the two with
//! short waits rather than blocking on one, so it needs no second task and
//! nothing shared between two. A forward is to the address its guest had
//! when it was made -- a guest restarted keeps it; one stopped takes it away,
//! and connections to it then fail.
//!
//! The kernel has 64 TCP connections in all and a forwarded one takes two,
//! so a forward relays at most `MAX_CONNS` at once and refuses the rest.
//! Taking a forward away closes its port, and waits out its relays.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcore::cmd::Output;
use kcore::net::Nic;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::tcp::{TcpListener, TcpStream, RECV_TIMEOUT};

use crate::net;

const MAX_FORWARDS: usize = 8;
const MAX_CONNS: usize = 8;
/// How long the listener waits for a connection before it looks at its stop
/// flag; and a relay's wait on each side before it looks at the other.
const ACCEPT_WAIT_MS: u64 = 500;
const RELAY_WAIT_MS: u64 = 5;
const RELAY_BUF: usize = 4096;
/// The device a forward listens through: nos's own.
const PUBLIC_IF: &str = "eth0";

/// What a forward's tasks share.
struct Shared {
    stop: AtomicBool,
    /// Connections being relayed, and all ever accepted, refused and failed.
    active: AtomicUsize,
    accepted: AtomicU64,
    refused: AtomicU64,
    failed: AtomicU64,
    /// Where to: the guest's address and port, and the device it is behind.
    guest_ip: u32,
    guest_port: u16,
    host: Nic,
    /// The relays' tasks, joined when the forward goes -- and the finished
    /// ones as new ones start.
    relays: Mutex<Vec<(Arc<AtomicBool>, TaskHandle)>>,
}

struct Forward {
    port: u16,
    vm: u32,
    shared: Arc<Shared>,
    task: Option<TaskHandle>,
}

/// Every forward.
pub struct Forwards {
    table: Mutex<Vec<Forward>>,
}

/// The listener task: accept, connect to the guest, hand the pair to a relay.
struct Listen {
    shared: Arc<Shared>,
    listener: TcpListener,
}

fn listen(start: Listen) {
    let Listen { shared, listener } = start;
    while !shared.stop.load(Ordering::Acquire) {
        let Some(client) = listener.accept(ACCEPT_WAIT_MS) else { continue };
        reap(&shared);
        if shared.active.load(Ordering::Acquire) >= MAX_CONNS {
            shared.refused.fetch_add(1, Ordering::Relaxed);
            client.abort();
            continue;
        }
        let Some(guest) = TcpStream::connect(&shared.host, shared.guest_ip, shared.guest_port) else {
            shared.failed.fetch_add(1, Ordering::Relaxed);
            client.abort();
            continue;
        };
        shared.accepted.fetch_add(1, Ordering::Relaxed);
        let done = Arc::new(AtomicBool::new(false));
        shared.active.fetch_add(1, Ordering::AcqRel);
        let relay = Relay { shared: shared.clone(), done: done.clone(), a: client, b: guest };
        match kcore::task::spawn_with("hv/relay", relay, relay_task) {
            /* The relay's hold on the pair is gone with the spawn that
             * failed: both are closed. */
            None => {
                shared.active.fetch_sub(1, Ordering::AcqRel);
                shared.failed.fetch_add(1, Ordering::Relaxed);
            }
            Some(task) => {
                let mut relays = shared.relays.lock();
                if relays.try_reserve(1).is_ok() {
                    relays.push((done, task));
                } else {
                    /* No room to keep it: joined now, which waits out the
                     * connection -- out of the lock. */
                    drop(relays);
                    drop(task);
                }
            }
        }
    }
    /* The listener closes with this task: its port answers no more. */
}

/// Join the relays that have finished, out of the lock.
fn reap(shared: &Shared) {
    let mut finished = Vec::new();
    {
        let mut relays = shared.relays.lock();
        let mut i = 0;
        while i < relays.len() {
            if relays[i].0.load(Ordering::Acquire) && finished.try_reserve(1).is_ok() {
                finished.push(relays.swap_remove(i));
            } else {
                i += 1;
            }
        }
    }
    drop(finished);
}

/// One connection's two sides.
struct Relay {
    shared: Arc<Shared>,
    done: Arc<AtomicBool>,
    a: TcpStream,
    b: TcpStream,
}

fn relay_task(relay: Relay) {
    let Relay { shared, done, mut a, mut b } = relay;
    let mut buf = [0u8; RELAY_BUF];
    'relay: while !shared.stop.load(Ordering::Acquire) {
        for dir in 0..2 {
            let (from, to) = if dir == 0 { (&mut a, &mut b) } else { (&mut b, &mut a) };
            let n = from.recv(&mut buf, RELAY_WAIT_MS);
            if n == RECV_TIMEOUT {
                continue;
            }
            if n <= 0 || !to.send_all(&buf[..n as usize]) {
                /* An end, or a failure, on either side: both close. */
                break 'relay;
            }
        }
    }
    drop(a);
    drop(b);
    shared.active.fetch_sub(1, Ordering::AcqRel);
    done.store(true, Ordering::Release);
}

impl Forwards {
    pub fn new() -> Option<Forwards> {
        let mut table = Vec::new();
        table.try_reserve_exact(MAX_FORWARDS).ok()?;
        Some(Forwards { table: Mutex::new(table)? })
    }

    /// `hv forward add <port> <vm> <guest-port>`.
    pub fn add(&self, port: u16, vm: u32, guest_port: u16, guest_ip: u32, host: Nic, out: &mut Output) {
        let Some(public) = Nic::find(PUBLIC_IF) else {
            let _ = writeln!(out, "hv: no {} to listen through", PUBLIC_IF);
            return;
        };
        {
            let table = self.table.lock();
            if table.len() >= MAX_FORWARDS {
                let _ = writeln!(out, "hv: {} forwards already", MAX_FORWARDS);
                return;
            }
            if table.iter().any(|f| f.port == port) {
                let _ = writeln!(out, "hv: port {} is forwarded already", port);
                return;
            }
        }
        let listener = match TcpListener::bind(&public, port) {
            Ok(l) => l,
            Err(e) => {
                let _ = writeln!(out, "hv: port {} would not listen -- {}", port, e);
                return;
            }
        };
        let relays = match Mutex::new(Vec::new()) {
            Some(m) => m,
            None => {
                let _ = writeln!(out, "hv: out of memory");
                return;
            }
        };
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            accepted: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            guest_ip,
            guest_port,
            host,
            relays,
        });
        let Some(task) = kcore::task::spawn_with("hv/forward", Listen { shared: shared.clone(), listener }, listen) else {
            let _ = writeln!(out, "hv: no task for the forward");
            return;
        };
        let forward = Forward { port, vm, shared, task: Some(task) };
        let refused = {
            let mut table = self.table.lock();
            if table.len() >= MAX_FORWARDS || table.iter().any(|f| f.port == port) {
                Some(forward)
            } else {
                /* Into the room taken at `new`. */
                table.push(forward);
                None
            }
        };
        if let Some(f) = refused {
            end(f);
            let _ = writeln!(out, "hv: port {} was forwarded meanwhile", port);
            return;
        }
        let _ = writeln!(out, "hv: port {} forwarded to vm {}, {}:{}", port, vm, net::dotted(guest_ip), guest_port);
    }

    /// `hv forward del <port>`.
    pub fn del(&self, port: u16, out: &mut Output) {
        let forward = {
            let mut table = self.table.lock();
            match table.iter().position(|f| f.port == port) {
                Some(i) => table.swap_remove(i),
                None => {
                    let _ = writeln!(out, "hv: port {} is not forwarded", port);
                    return;
                }
            }
        };
        end(forward);
        let _ = writeln!(out, "hv: port {} no longer forwarded", port);
    }

    /// `hv forward`.
    pub fn list(&self, out: &mut Output) {
        let table = self.table.lock();
        if table.is_empty() {
            let _ = writeln!(out, "hv: no forwards");
            return;
        }
        for f in table.iter() {
            let s = &f.shared;
            let _ = writeln!(out, "port {} -> vm {} {}:{}  {} active, {} accepted, {} refused, {} failed",
                f.port, f.vm, net::dotted(s.guest_ip), s.guest_port, s.active.load(Ordering::Relaxed),
                s.accepted.load(Ordering::Relaxed), s.refused.load(Ordering::Relaxed),
                s.failed.load(Ordering::Relaxed));
        }
    }

    /// Every forward taken away: the module is going.
    pub fn close(&self) {
        loop {
            let forward = self.table.lock().pop();
            match forward {
                Some(f) => end(f),
                None => return,
            }
        }
    }
}

/// A forward taken off the table, ended: its listener and relays told to
/// stop, and waited for.
fn end(mut f: Forward) {
    f.shared.stop.store(true, Ordering::Release);
    drop(f.task.take());
    let relays = core::mem::take(&mut *f.shared.relays.lock());
    drop(relays);
}

/// `hv forward ...` as typed: the words after `forward`.
pub fn usage() -> String {
    String::from("hv forward [add <port> <vm> <guest-port> | del <port>]")
}
