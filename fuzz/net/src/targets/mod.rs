//! The targets: each turns its input into what the world does to the
//! machine and what the machine's programs ask of it, and holds what comes
//! of it to what the protocol -- and this stack, of itself -- says.

mod api;
mod dhcp;
mod dns;
mod http;
mod icmp;
mod netconsole;
mod ssh;
mod stack;
mod tcp;
mod terminal;
mod tls_client;
mod tls_server;
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
    Target { name: "tls_server", run: tls_server::tls_server, max_len: 1024, gate: 300 },
    Target { name: "tls_client", run: tls_client::tls_client, max_len: 1024, gate: 300 },
    Target { name: "terminal", run: terminal::terminal, max_len: 4096, gate: 20000 },
    Target { name: "api", run: api::api, max_len: 1024, gate: 500 },
];

/// Everything the machine still holds of what an input did, once it has
/// had time to let go: every TCP connection, every frame, every task. What
/// is still there is held for good -- a slot, a frame or a task a peer can
/// take and keep.
pub fn audit() {
    audit_but(&[], &[]);
}

/// The TCP timer's promise (the timer's task in net/src/tcp.rs): no
/// connection's retransmission is left past its deadline by more than the
/// task's wakeup can be late -- a tick of the machine's, which rounds every
/// sleep up to one -- with one to spare: the pass the task runs then sends
/// again or sets a deadline anew. A closed connection's is nobody's: the
/// pass skips it, and the slot's cleanup is all that is left of it.
pub fn check_tcp_timer() {
    let now_ms = crate::machine::sched::now() / 1_000_000;
    let late = 2 * crate::machine::sched::TICK / 1_000_000 + 1;
    for i in 0..net::tcp::MAX_CONNECTIONS {
        if let Some(c) = net::tcp::TCP.snapshot(i) {
            /* Its timeout held to its floor, and the floor to what a
             * connection may be given (`set_rto_floor`) */
            invariant!((net::tcp::LINK_MIN_RTO_MS..=net::tcp::MIN_RTO_MS).contains(&c.min_rto_ms),
                       "TCP slot {} has a floor of {} ms for its timeout", i, c.min_rto_ms);
            invariant!((c.min_rto_ms..=net::tcp::MAX_RTO_MS).contains(&c.rto_ms), "TCP slot {} ({}, port {} -> {}): \
                       its timeout is {} ms, its floor {} ms", i, c.state.name(), c.local_port, c.remote_port,
                       c.rto_ms, c.min_rto_ms);
            if c.retransmit_at != 0 && c.state != net::tcp::State::Closed {
                invariant!(now_ms <= c.retransmit_at + late, "TCP slot {} ({}, port {} -> {}): its retransmission was \
                           due at {} ms and is still not looked at, at {} ms", i, c.state.name(), c.local_port,
                           c.remote_port, c.retransmit_at, now_ms);
            }
            /* And a connection half open is given up after its time --
             * looked at by a retransmission's pass or a sweep */
            if c.state == net::tcp::State::SynReceived {
                let by = c.opened_ms + net::tcp::HALF_OPEN_MS + net::tcp::SWEEP_MS + late;
                invariant!(now_ms <= by, "TCP slot {} (port {} -> {}): half open since {} ms, and still at {} ms",
                           i, c.local_port, c.remote_port, c.opened_ms, now_ms);
            }
        }
    }
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
    /* TCP's timer's task waits for good, as it does in the kernel: stopped
     * here, where nothing is left for it to do */
    net::tcp::TCP.stop_timer();
    for (id, name, kind, st, holds) in crate::machine::sched::tasks() {
        if waiting.contains(&id) {
            continue;
        }
        invariant!(st == crate::machine::sched::St::Done, "task {} '{}' ({:?}) is still {:?} at the end", id, name,
                   kind, st);
        invariant!(holds.is_empty(), "task {} '{}' ended holding {:?}", id, name, holds);
    }
}
