//! `ipc`: the channel from the shell -- and so from SSH, a script, and the
//! boot's own tests. What it prints is copied out first and printed with no
//! lock held: whoever runs it may be slow to take it.
//!
//!     ipc                     the host's counters
//!     ipc names [prefix]      every name: its mailbox, scope and owner
//!     ipc boxes               every mailbox: owner, name, what it holds
//!     ipc echo [start|stop]   kernel.echo: whether it runs, what it did
//!     ipc peers               the router and the peers it has told of
//!     ipc ping <to> [count=N] [bytes=B] [window=W] [bulk] [timeout=MS]
//!                             messages to a name, here or `name@host`, and
//!                             their answers: each held to what was sent, the
//!                             round trips and MB/s said -- kernel.echo@host
//!                             a peer's echo through ipcnet

use crate::{boxes, counters, echo_start, echo_status, echo_stop, list, peers, router, Addr, Limits, Mailbox, Payload,
            To, TAG_BOUNCED, TAG_BULK, TAG_DOWN};
use alloc::vec::Vec;
use core::fmt::Write;
use kcore::time::{boot_time_ns, Duration};
use core::sync::atomic::{AtomicBool, Ordering};
use kcore::cmd::{Command, Output};
use kcore::klog;

const HELP: &str = "ipc [names [prefix] | boxes | peers | echo [start|stop] | ping <to> [count=N bytes=B ..]]";

pub(crate) fn init() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    match Command::register("ipc", HELP, run) {
        /* The channel is the kernel's own, and its command stays for good */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => klog!(0, "ipc: cannot register the ipc command"),
    }
}

/// The first word of `args`, and the rest.
fn word(args: &str) -> (&str, &str) {
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    (&args[..end], args[end..].trim())
}

fn run(args: &str, out: &mut Output) {
    let (what, rest) = word(args);
    match what {
        "" => summary(out),
        "names" => names(rest, out),
        "boxes" => mailboxes(out),
        "peers" => peer_list(out),
        "echo" => echo(rest, out),
        "ping" => ping(rest, out),
        _ => {
            let _ = writeln!(out, "usage: {}", HELP);
        }
    }
}

fn summary(out: &mut Output) {
    let c = counters();
    let _ = writeln!(out, "ipc: {} mailboxes, {} names, {} watches; {} bytes queued", c.mailboxes, c.names, c.watches,
                     c.queued_bytes);
    let _ = writeln!(out, "ipc: sent {}, received {}; refused full {}, gone {}, no memory {}", c.sent, c.received,
                     c.full, c.gone, c.no_memory);
    let _ = writeln!(out, "ipc: downs {}, watches lost {}, strays {}", c.downs, c.watch_lost, c.stray);
    let _ = writeln!(out, "ipc: to peers {}, from peers {} (refused {}); peers up {}, lost {}; bounced {} (dropped {})",
                     c.routed, c.delivered_remote, c.refused_remote, c.peers_up, c.peers_lost, c.bounced,
                     c.bounces_dropped);
}

fn peer_list(out: &mut Output) {
    match router() {
        Some(r) => {
            let _ = writeln!(out, "ipc: router {}", r);
        }
        None => {
            let _ = writeln!(out, "ipc: no router -- nothing carries messages to other hosts");
        }
    }
    for (number, name, up, sent) in peers() {
        let _ = writeln!(out, "{:>6} {:<24} {:<5} {} may be answered", number, name, if up { "up" } else { "down" },
                         sent);
    }
}

fn names(prefix: &str, out: &mut Output) {
    let all = list(prefix);
    if all.is_empty() {
        let _ = writeln!(out, "ipc: no names{}{}", if prefix.is_empty() { "" } else { " under " }, prefix);
        return;
    }
    for n in &all {
        let _ = writeln!(out, "{:<32} {:<12} {:<8} {}", n.name, n.addr, n.scope.name(), n.owner);
    }
}

fn mailboxes(out: &mut Output) {
    let all = boxes();
    let _ = writeln!(out, "{:<12} {:<16} {:<32} {:>11} {:>17} {:>5} {:>4} {:>4} {:>10} {:>7}", "address", "owner",
                     "name", "queued", "bytes", "kept", "mon", "watch", "received", "refused");
    for b in &all {
        let _ = writeln!(out, "{:<12} {:<16} {:<32} {:>5}/{:<5} {:>8}/{:<8} {:>5} {:>4} {:>4} {:>10} {:>7}",
                         b.addr, b.owner, b.name.as_deref().unwrap_or("-"), b.queued, b.limit, b.bytes,
                         b.byte_limit, b.kept, b.monitors, b.watches, b.received, b.refused);
    }
}

fn echo(what: &str, out: &mut Output) {
    match what {
        "start" => match echo_start() {
            Ok(()) => {
                let _ = writeln!(out, "ipc echo: started");
            }
            Err(why) => {
                let _ = writeln!(out, "ipc echo: not started: {}", why);
            }
        },
        "stop" => {
            let _ = writeln!(out, "ipc echo: {}", if echo_stop() { "stopped" } else { "was not running" });
        }
        "" => {
            let s = echo_status();
            let _ = writeln!(out, "ipc echo: {}, echoed {}, downs {}, pestering {}, pester messages {}",
                             if s.running { "running" } else { "stopped" }, s.echoed, s.downs, s.pestering,
                             s.pester_sent);
        }
        _ => {
            let _ = writeln!(out, "usage: ipc echo [start|stop]");
        }
    }
}

/// What `ipc ping` sends: "PIN", bulk's bit clear -- `bulk` sets it.
const PING_TAG: u32 = 0x0050_494E;
/// Pings at most, and in flight at most: the round trips are kept to be
/// sorted, a u64 each, well under a block of the heap.
const PING_MAX: usize = 20_000;
const WINDOW_MAX: usize = 64;
const PING_LIMITS: Limits = Limits { messages: 256, bytes: 16 << 20 };
const NS_PER_US: u64 = 1_000;
/// A ping's bytes are made, and an answer's compared, a piece at a time:
/// a megabyte is more than one block of the heap holds.
const PING_PIECE: usize = 64 * 1024;

/// The byte a ping has at `i`.
fn ping_byte(i: usize) -> u8 {
    (i as u8).wrapping_mul(29) ^ 0x5A
}

/// Whether `p` holds `len` bytes of the ping's pattern, read a piece at a
/// time into `scratch`.
fn ping_matches(p: &Payload, len: usize, scratch: &mut [u8]) -> bool {
    if p.len() != len {
        return false;
    }
    let mut at = 0;
    while at < len {
        let n = p.read(at, &mut scratch[..PING_PIECE.min(len - at)]);
        if n == 0 || scratch[..n].iter().enumerate().any(|(k, &b)| b != ping_byte(at + k)) {
            return false;
        }
        at += n;
    }
    true
}

/// `ipc ping <to> [count=N] [bytes=B] [window=W] [bulk] [timeout=MS]`: N
/// messages of B bytes to `to`, W of them in flight at once, each answer
/// held to what was sent. The first goes by name; its answer says the
/// mailbox's address, which the rest go to, monitored -- a Down ends the
/// run. A bounce is counted, and so is an answer that is not the bytes
/// sent. One line, `key=value`s, for whoever reads it.
fn ping(args: &str, out: &mut Output) {
    const USAGE: &str = "usage: ipc ping <name | name@host> [count=N] [bytes=B] [window=W] [bulk] [timeout=MS]";
    let mut words = args.split_whitespace();
    let Some(name) = words.next() else {
        let _ = writeln!(out, "{}", USAGE);
        return;
    };
    let (mut count, mut bytes, mut window, mut bulk, mut timeout_ms) = (10usize, 64usize, 1usize, false, 2000u64);
    for w in words {
        let (k, v) = w.split_once('=').unwrap_or((w, ""));
        let ok = match k {
            "count" => v.parse().ok().filter(|&n| (1..=PING_MAX).contains(&n)).map(|n| count = n).is_some(),
            "bytes" => v.parse().ok().filter(|&n| n <= crate::MAX_PAYLOAD).map(|n| bytes = n).is_some(),
            "window" => v.parse().ok().filter(|&n| (1..=WINDOW_MAX).contains(&n)).map(|n| window = n).is_some(),
            "timeout" => v.parse().ok().filter(|&n| (1..=60_000).contains(&n)).map(|n| timeout_ms = n).is_some(),
            "bulk" if v.is_empty() => {
                bulk = true;
                true
            }
            _ => false,
        };
        if !ok {
            let _ = writeln!(out, "ipc ping: \"{}\" -- {}", w, USAGE);
            return;
        }
    }
    let Ok(mb) = Mailbox::new(PING_LIMITS) else {
        let _ = writeln!(out, "ipc ping: no mailbox");
        return;
    };
    /* The bytes in pieces, and room to read an answer's back a piece at a
     * time */
    let mut pieces: Vec<Vec<u8>> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut made = 0;
    let mut room = scratch.try_reserve_exact(PING_PIECE.min(bytes.max(1))).is_ok();
    while room && made < bytes {
        let n = PING_PIECE.min(bytes - made);
        let mut p = Vec::new();
        room = p.try_reserve_exact(n).is_ok() && pieces.try_reserve(1).is_ok();
        if room {
            p.extend((made..made + n).map(ping_byte));
            pieces.push(p);
            made += n;
        }
    }
    if !room {
        let _ = writeln!(out, "ipc ping: no memory for {} bytes", bytes);
        return;
    }
    scratch.resize(PING_PIECE.min(bytes.max(1)), 0);
    let parts: Vec<&[u8]> = pieces.iter().map(|p| &p[..]).collect();
    let mut rtts: Vec<u64> = Vec::new();
    if rtts.try_reserve_exact(count).is_err() {
        return;
    }
    let tag = if bulk { PING_TAG | TAG_BULK } else { PING_TAG };
    let (mut sent, mut wrong, mut bounced, mut timeouts, mut errors, mut down) = (0usize, 0usize, 0usize, 0usize, 0usize, false);
    let mut target: Option<Addr> = None;
    let mut monitor: Option<u64> = None;
    /* Answers come in the order their messages went, the references
     * rising: one that comes before an earlier one's is counted */
    let mut last_answered = 0u64;
    let mut reordered = 0usize;
    /* What is in flight: each one's reference and when it went */
    let mut flight: Vec<(u64, u64)> = Vec::new();
    let begun = boot_time_ns();
    while (sent < count || !flight.is_empty()) && !down {
        /* Into flight while there is room -- the first alone, by name,
         * until its answer says where to */
        while sent < count && flight.len() < if target.is_some() { window } else { 1 } {
            let Ok(payload) = Payload::gather(&parts) else {
                errors += 1;
                sent += 1;
                continue;
            };
            let reference = mb.next_reference();
            let to = match target {
                Some(a) => To::Addr(a),
                None => To::Name(name),
            };
            sent += 1;
            match mb.send(to, tag, reference, payload) {
                Ok(()) => flight.push((reference, boot_time_ns())),
                Err(_) => errors += 1,
            }
        }
        if flight.is_empty() {
            continue;
        }
        let oldest = flight.iter().map(|f| f.1).min().unwrap_or(0);
        let deadline = oldest + timeout_ms * 1_000_000;
        let now = boot_time_ns();
        if now >= deadline {
            /* The oldest given up on */
            if let Some(i) = flight.iter().position(|f| f.1 == oldest) {
                flight.swap_remove(i);
            }
            timeouts += 1;
            continue;
        }
        let Some(m) = mb.recv(Duration::from_nanos(deadline - now)) else { continue };
        if m.tag() == TAG_DOWN && Some(m.reference()) == monitor {
            down = true;
            continue;
        }
        let Some(i) = flight.iter().position(|f| f.0 == m.reference()) else { continue };
        let (_, at) = flight.swap_remove(i);
        if m.tag() == TAG_BOUNCED {
            bounced += 1;
            continue;
        }
        if m.is_system() {
            continue;
        }
        if m.tag() != tag || !ping_matches(m.payload(), bytes, &mut scratch) {
            wrong += 1;
            continue;
        }
        rtts.push(boot_time_ns().saturating_sub(at) / NS_PER_US);
        if m.reference() < last_answered {
            reordered += 1;
        }
        last_answered = last_answered.max(m.reference());
        if target.is_none() {
            target = Some(m.from());
            monitor = mb.monitor(m.from()).ok();
        }
    }
    let took_ns = boot_time_ns().saturating_sub(begun).max(1);
    rtts.sort_unstable();
    let at = |q: usize| rtts.get((rtts.len().saturating_sub(1)) * q / 100).copied().unwrap_or(0);
    let mb_s = (rtts.len() as u64).saturating_mul(bytes as u64).saturating_mul(1000) / took_ns;
    let _ = writeln!(out, "ipc ping: to={} sent={} answered={} wrong={} bounced={} timeouts={} errors={} down={} \
                           reordered={} min_us={} p50_us={} p99_us={} max_us={} mb_per_s={} ms={}", name, sent,
                     rtts.len(), wrong, bounced, timeouts, errors, down, reordered, rtts.first().copied().unwrap_or(0),
                     at(50), at(99), rtts.last().copied().unwrap_or(0), mb_s, took_ns / 1_000_000);
}
