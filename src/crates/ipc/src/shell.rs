//! `ipc`: the channel from the shell -- and so from SSH, a script, and the
//! boot's own tests. What it prints is copied out first and printed with no
//! lock held: whoever runs it may be slow to take it.
//!
//!     ipc                     the host's counters
//!     ipc names [prefix]      every name: its mailbox, scope and owner
//!     ipc boxes               every mailbox: owner, name, what it holds
//!     ipc echo [start|stop]   kernel.echo: whether it runs, what it did
//!     ipc peers               the router and the peers it has told of

use crate::{boxes, counters, echo_start, echo_status, echo_stop, list, peers, router};
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use kcore::cmd::{Command, Output};
use kcore::klog;

const HELP: &str = "ipc [names [prefix] | boxes | peers | echo [start|stop]]";

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
