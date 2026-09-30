//! The targets: each turns its input into what the world does to the
//! machine and what the machine's programs ask of it, and holds what comes
//! of it to what the protocol -- and this stack, of itself -- says.

mod dhcp;
mod dns;
mod http;
mod icmp;
mod netconsole;
mod ssh;
mod stack;
mod tcp;
mod udpshell;

use crate::Target;

pub static ALL: &[Target] = &[
    Target { name: "stack", run: stack::stack, max_len: 4096, gate: 5000 },
    Target { name: "tcp", run: tcp::tcp, max_len: 4096, gate: 2000 },
    Target { name: "http", run: http::http, max_len: 2048, gate: 5000 },
    Target { name: "https", run: http::https, max_len: 2048, gate: 3000 },
    Target { name: "dns", run: dns::dns, max_len: 1024, gate: 500 },
    Target { name: "dhcp", run: dhcp::dhcp, max_len: 1024, gate: 500 },
    Target { name: "icmp", run: icmp::icmp_target, max_len: 1024, gate: 5000 },
    Target { name: "udpshell", run: udpshell::udpshell, max_len: 1024, gate: 2000 },
    Target { name: "netconsole", run: netconsole::netconsole, max_len: 1024, gate: 1000 },
    Target { name: "ssh", run: ssh::ssh, max_len: 1024, gate: 300 },
];

/// Everything the machine still holds of what an input did, once it has
/// had time to let go: every TCP connection, every frame, every task. What
/// is still there is held for good -- a slot, a frame or a task a peer can
/// take and keep.
pub fn audit() {
    audit_but(&[], &[]);
}

/// `audit`, but for the TCP connections on `held`'s ports -- the machine's
/// and the other end's -- and the tasks in `waiting`: what an attacker may
/// hold open on purpose, as any peer may.
pub fn audit_but(held: &[(u16, u16)], waiting: &[usize]) {
    for i in 0..net::tcp::MAX_CONNECTIONS {
        if let Some(c) = net::tcp::TCP.snapshot(i) {
            if held.contains(&(c.local_port, c.remote_port)) {
                continue;
            }
            panic!("invariant: TCP slot {} is still {} ({}:{} -> {}:{}, {} bytes to send, {} to read) after \
                    everything closed and the timers ran out", i, c.state.name(), netwire::Ipv4(c.local_ip),
                   c.local_port, netwire::Ipv4(c.remote_ip), c.remote_port, c.send_used, c.recv_used);
        }
    }
    let busy = (0..net::tcp::MAX_CONNECTIONS).filter(|&i| net::tcp::TCP.snapshot(i).is_some()).count();
    let conns = net::tcp::TCP.stats().conns;
    invariant!(conns <= busy, "TCP counts {} connections with {} slots in use", conns, busy);
    if busy == 0 {
        let pool = net::frame::POOL.stats();
        invariant!(pool.in_flight == 0, "{} frames out of the pool with nothing left to hold one", pool.in_flight);
    }
    for (id, name, kind, st, holds) in crate::machine::sched::tasks() {
        if waiting.contains(&id) {
            continue;
        }
        invariant!(st == crate::machine::sched::St::Done, "task {} '{}' ({:?}) is still {:?} at the end", id, name,
                   kind, st);
        invariant!(holds.is_empty(), "task {} '{}' ended holding {:?}", id, name, holds);
    }
}
