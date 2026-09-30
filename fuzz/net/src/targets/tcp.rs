//! `tcp`: connections between the machine's programs and a peer on the
//! wire -- opened either way, data both ways, closed or aborted either way
//! -- over a link that loses, repeats, delays and reorders what it carries,
//! with an attacker putting segments of its own into them, ICMP errors
//! quoting them, and time passing through every timer the machine has.
//!
//! What the machine's programs are handed by `recv` must be the peer's
//! stream, byte for byte and in order, and end only where the peer ended
//! it; what the peer is handed must be the programs'. Segments the attacker
//! puts in from outside every window must change nothing. And once every
//! program has closed what it had and the timers have run out, the machine
//! must hold nothing: no slot, no frame, no task still waiting.

use std::collections::BTreeMap;

use net::tcp::{Conn, TCP};
use netwire::{icmp, tcp as seg, IP_PROTO_ICMP, IP_PROTO_TCP};

use crate::machine::{sched, ETH0_MAC};
use crate::world::frames::{self, Ip, Sum};
use crate::world::lan::Lan;
use crate::world::tcpm::{self, S, Tcp};
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, GW_MAC, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// The peer: on the machine's LAN, or beyond the gateway.
const NEAR_IP: u32 = 0x0A00_0264;
const NEAR_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x64];
const FAR_IP: u32 = 0x5DB8_D822;

/// The ports the machine listens on, and where the peer's own listening
/// ports -- one for each connect of the machine's -- begin.
const LISTEN_PORTS: [u16; 3] = [7001, 7002, 22];
const PEER_PORT_BASE: u16 = 8000;
const PEER_SRC_BASE: u16 = 30000;

/// What the peer and the LAN do with what the machine sends.
struct Wire {
    lan: Lan,
    tcp: Tcp,
    /// Segments that are not the peer's: for the attacker's quotes.
    seen: Vec<Vec<u8>>,
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if self.lan.on_frame(net, frame) {
            return;
        }
        if let Some(p) = crate::world::check::ipv4_parts(frame) {
            if p.proto == IP_PROTO_TCP && self.seen.len() < 256 {
                self.seen.push(frame.to_vec());
            }
        }
        self.tcp.on_frame(net, frame);
    }

    fn next_timer(&self) -> u64 {
        self.tcp.next_timer()
    }

    fn on_timer(&mut self, net: &mut Net) {
        self.tcp.on_timer(net);
    }
}

/// What a program on the machine is doing with a connection.
enum Call {
    Connect(App<Option<usize>>),
    Accept(App<Option<usize>>),
    Send(App<isize>, u64),
    Recv(App<(isize, Vec<u8>)>),
}

/// A connection, as the target knows it: the machine's end, the peer's,
/// and what the machine's program has sent and read.
struct Link2 {
    id: usize,
    /// The machine's end, once its program has it.
    conn: Option<&'static Conn>,
    /// The peer's port.
    port: u16,
    sent: u64,
    got: u64,
    eof: bool,
    /// The program has closed or aborted it: never touched again.
    done: bool,
    aborted: bool,
    /// Something outside the two ends could have ended it: the peer went
    /// quiet, an ICMP error named it, the link lost a lot.
    fragile: bool,
    call: Option<Call>,
}

struct State {
    w: World<Wire>,
    links: Vec<Link2>,
    /// The machine's listeners, and the accepts waiting on them.
    listeners: Vec<(u16, Option<&'static Conn>)>,
    accepts: Vec<(usize, App<Option<usize>>)>,
    next_peer_port: u16,
    next_src_port: u16,
    nic: net::Nic,
    peer_ip: u32,
    /// Connections the machine accepted, by the peer's port: each once.
    accepted: BTreeMap<u16, usize>,
    lossy: bool,
}

fn conn_word(c: &'static Conn) -> usize {
    c as *const Conn as usize
}

fn conn_of(word: usize) -> &'static Conn {
    TCP.by_handle(word).expect("a connection the pool gave out")
}

impl State {
    fn ep(&self, l: &Link2) -> Option<usize> {
        self.w.peers.tcp.eps.iter().rposition(|e| e.id == l.id)
    }

    /// The machine's slot for link `k`, as `tcpstat` shows it.
    fn slot(&self, k: usize) -> Option<net::tcp::ConnInfo> {
        let port = self.links[k].port;
        (0..net::tcp::MAX_CONNECTIONS).filter_map(|i| TCP.snapshot(i))
            .find(|c| c.remote_port == port && c.remote_ip == self.peer_ip)
    }

    /* ---- what the machine's programs do ---- */

    fn connect(&mut self, r: &mut Input) {
        let id = self.links.len();
        let port = self.next_peer_port;
        self.next_peer_port += 1;
        /* The peer listens there, mostly; sometimes nobody does, and it
         * answers a reset; sometimes it is not there at all. */
        match r.u8() % 8 {
            0 => {}
            1 => self.w.peers.tcp.ignored.push(port),
            _ => self.w.peers.tcp.listening.push((port, id)),
        }
        self.w.peers.tcp.accept_iss = iss(r);
        self.w.peers.tcp.accept_mss = match r.u8() % 8 {
            0 => None,
            1 => Some(r.pick(&[0u16, 1, 88, 535, 65535])),
            _ => Some(r.pick(&[1460u16, 536, 1200])),
        };
        self.w.peers.tcp.accept_window = r.pick(&[8192u32, 65535, 1024, 100, 0, 3000]);
        let nic = self.nic;
        let ip = self.peer_ip;
        let app = App::spawn("connect", r.below(4) as u32, move || TCP.connect(&nic, ip, port, 0).map(conn_word));
        self.links.push(Link2 { id, conn: None, port, sent: 0, got: 0, eof: false, done: false, aborted: false,
                                fragile: self.lossy, call: Some(Call::Connect(app)) });
    }

    fn peer_connect(&mut self, r: &mut Input) {
        let id = self.links.len();
        let port = self.next_src_port;
        self.next_src_port += 1;
        let mport = r.pick(&LISTEN_PORTS);
        let opts = match r.u8() % 8 {
            0 => Vec::new(),
            1 => {
                /* Options of every kind and length, well made or not. */
                let n = r.below(12) as usize;
                let mut o = r.bytes(n);
                o.extend_from_slice(&frames::mss(r.pick(&[0u16, 1, 1460, 65535])));
                o
            }
            _ => frames::mss(r.pick(&[1460u16, 536, 1400, 100])),
        };
        let window = r.pick(&[8192u32, 65535, 1024, 0, 600]);
        let s = iss(r);
        self.w.peers.tcp.connect(&mut self.w.net, id, port, ETH0_IP, mport, s, opts, window);
        self.links.push(Link2 { id, conn: None, port, sent: 0, got: 0, eof: false, done: false, aborted: false,
                                fragile: self.lossy, call: None });
    }

    fn accept(&mut self, r: &mut Input) {
        if self.accepts.len() >= 4 {
            return;
        }
        let k = r.below(self.listeners.len() as u64) as usize;
        let Some(l) = self.listeners[k].1 else { return };
        let timeout = if r.u8() < 16 { 0 } else { r.pick(&[1u64, 50, 1000, 5000]) };
        let app = App::spawn("accept", r.below(4) as u32, move || TCP.accept(l, timeout).map(conn_word));
        self.accepts.push((k, app));
    }

    /// The link `k`'s machine end, when the program has it and is not
    /// busy with it.
    fn idle(&self, k: usize) -> Option<&'static Conn> {
        let l = &self.links[k];
        if l.done || l.call.is_some() {
            return None;
        }
        l.conn
    }

    fn send(&mut self, r: &mut Input, k: usize) {
        let Some(c) = self.idle(k) else { return };
        let n = match r.u8() % 8 {
            0 => r.below(20_000) as u64,
            1 => 0,
            _ => r.below(3000) as u64,
        };
        let timeout = if r.u8() < 16 { 0 } else { r.pick(&[1u64, 10, 200, 3000]) };
        let (id, from) = (self.links[k].id, self.links[k].sent);
        if let Some(e) = self.ep(&self.links[k]) {
            let ep = &mut self.w.peers.tcp.eps[e];
            ep.offered = ep.offered.max(from + n);
        }
        let app = App::spawn("send", r.below(4) as u32, move || {
            let data: Vec<u8> = (from..from + n).map(|o| tcpm::machine_byte(id, o)).collect();
            TCP.send(c, &data, timeout)
        });
        self.links[k].call = Some(Call::Send(app, n));
    }

    fn recv(&mut self, r: &mut Input, k: usize) {
        let Some(c) = self.idle(k) else { return };
        let n = r.pick(&[1usize, 16, 1460, 8192, 20000]);
        let timeout = if r.u8() < 16 { 0 } else { r.pick(&[1u64, 10, 200, 3000]) };
        let app = App::spawn("recv", r.below(4) as u32, move || {
            let mut buf = vec![0u8; n];
            let got = TCP.recv(c, &mut buf, timeout);
            buf.truncate(got.max(0) as usize);
            (got, buf)
        });
        self.links[k].call = Some(Call::Recv(app));
    }

    fn close(&mut self, k: usize, abort: bool) {
        let Some(c) = self.idle(k) else { return };
        let l = &mut self.links[k];
        l.done = true;
        l.aborted = abort;
        let (id, sent) = (l.id, l.sent);
        if let Some(e) = self.w.peers.tcp.eps.iter().rposition(|e| e.id == id) {
            self.w.peers.tcp.eps[e].closed_at = Some(sent);
        }
        if abort {
            TCP.abort(c);
        } else {
            TCP.close(c);
        }
    }

    /* ---- what comes back ---- */

    /// Every call that has returned, held to what it had to return.
    fn harvest(&mut self) {
        for k in 0..self.links.len() {
            let Some(call) = self.links[k].call.take() else { continue };
            match call {
                Call::Connect(app) => match app.take() {
                    None => self.links[k].call = Some(Call::Connect(app)),
                    Some(Some(word)) => {
                        /* The machine had a SYN-ACK: the peer's, which it
                         * sent when the SYN came -- or the attacker's. */
                        let e = self.ep(&self.links[k]);
                        let tainted = self.w.peers.tcp.eps.iter().any(|e| e.tainted);
                        invariant!(tainted || e.is_some(), "connection {} connected to a port nobody answered on", k);
                        self.links[k].conn = Some(conn_of(word));
                    }
                    Some(None) => self.links[k].done = true,
                },
                Call::Accept(_) => {}
                Call::Send(app, asked) => match app.take() {
                    None => self.links[k].call = Some(Call::Send(app, asked)),
                    Some(n) => {
                        invariant!(n >= -1 && n <= asked as isize, "a send of {} bytes answered {}", asked, n);
                        if n > 0 {
                            self.links[k].sent += n as u64;
                        }
                    }
                },
                Call::Recv(app) => match app.take() {
                    None => self.links[k].call = Some(Call::Recv(app)),
                    Some((n, data)) => self.received(k, n, &data),
                },
            }
        }
        let mut still = Vec::new();
        for (lk, app) in std::mem::take(&mut self.accepts) {
            match app.take() {
                None => still.push((lk, app)),
                Some(None) => {}
                Some(Some(word)) => self.accepted_one(conn_of(word)),
            }
        }
        self.accepts = still;
    }

    fn accepted_one(&mut self, c: &'static Conn) {
        let (ip, port) = TCP.peer(c);
        invariant!(ip == self.peer_ip, "accepted a connection from {}, not the peer", netwire::Ipv4(ip));
        let k = self.links.iter().position(|l| l.port == port && l.conn.is_none());
        let Some(k) = k else {
            panic!("invariant: accepted a connection from port {} the peer never opened, or twice", port);
        };
        invariant!(self.accepted.insert(port, k).is_none(), "the connection from port {} accepted twice", port);
        let e = self.ep(&self.links[k]);
        let ok = e.is_some_and(|e| {
            let ep = &self.w.peers.tcp.eps[e];
            ep.tainted || ep.st != S::SynSent || ep.reset.is_some()
        });
        invariant!(ok, "accepted connection {} before the peer had the machine's SYN-ACK", k);
        self.links[k].conn = Some(c);
    }

    fn received(&mut self, k: usize, n: isize, data: &[u8]) {
        if crate::machine::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            let slot = self.slot(k).map(|c| format!("{} rcv {}", c.state.name(), c.recv_used));
            eprintln!("[{:>12}] connection {} (port {}): recv answered {} after {} bytes; the machine's end {:?}",
                      sched::now() / 1000, k, self.links[k].port, n, self.links[k].got, slot);
        }
        let e = self.ep(&self.links[k]);
        let (tainted, peer_fin, reset, silent) = match e {
            Some(e) => {
                let ep = &self.w.peers.tcp.eps[e];
                (ep.tainted, ep.fin_seq.map(|_| ep.queued), ep.reset, ep.silent)
            }
            None => (false, None, None, false),
        };
        let l = &mut self.links[k];
        invariant!(n >= 0 || n == net::tcp::RECV_TIMEOUT, "recv answered {}", n);
        if n > 0 {
            invariant!(!l.eof, "connection {} read {} bytes after the end of its stream", k, n);
            if !tainted {
                for (i, &b) in data.iter().enumerate() {
                    let off = l.got + i as u64;
                    invariant!(b == tcpm::peer_byte(l.id, off), "byte {} of connection {}'s stream read as {:#04x}, \
                               the peer wrote {:#04x}", off, l.id, b, tcpm::peer_byte(l.id, off));
                }
                let queued = e.map_or(0, |e| self.w.peers.tcp.eps[e].queued);
                invariant!(l.got + n as u64 <= queued, "connection {} read past what the peer wrote: {} of {}", l.id,
                           l.got + n as u64, queued);
            }
            l.got += n as u64;
        } else if n == 0 {
            /* The end of the stream: where the peer ended it -- or the
             * connection gone, reset or given up on. */
            l.eof = true;
            if !tainted && !l.fragile && !silent && reset.is_none() {
                match peer_fin {
                    Some(at) => invariant!(l.got == at, "connection {} (the peer's port {}) read the end of its stream \
                                           after {} bytes, the peer's FIN coming after {}", l.id, l.port, l.got, at),
                    None => panic!("invariant: connection {} (the peer's port {}) read the end of its stream after {} \
                                    bytes, the peer having sent no FIN nor reset it", l.id, l.port, l.got),
                }
            }
        }
    }

    /* ---- the attacker ---- */

    /// A segment from outside the connection: the peer's addresses and
    /// ports, and whatever else the attacker picks -- from outside every
    /// window, which must change nothing, or anywhere, which may.
    fn inject(&mut self, r: &mut Input) {
        let eps = &self.w.peers.tcp.eps;
        if eps.is_empty() {
            return;
        }
        let e = r.below(eps.len() as u64) as usize;
        let ep = &eps[e];
        if ep.st == S::SynSent {
            return;
        }
        let far = r.u8() < 160;
        let any = r.u8() & 0x3F;
        let flags = r.pick(&[seg::ACK_FLAG, seg::ACK_FLAG | seg::PSH, seg::RST, seg::RST | seg::ACK_FLAG, seg::SYN,
                             seg::FIN | seg::ACK_FLAG, seg::SYN | seg::ACK_FLAG, any]);
        /* Far: a sequence number and an acknowledgement at least a
         * megabyte from anything either end has sent -- and with the
         * window the machine offers never more than 64 KiB, out of it. */
        let (seq, ack) = if far {
            let away = 0x0010_0000 + r.below(0x3000_0000) as u32;
            let seq = if r.bool() { ep.snd_nxt.wrapping_add(away) } else { ep.snd_una.wrapping_sub(away) };
            let away = 0x0010_0000 + r.below(0x3000_0000) as u32;
            let ack = if r.bool() { ep.rcv_nxt.wrapping_add(away) } else { ep.rcv_nxt.wrapping_sub(away) };
            (seq, ack)
        } else {
            let seq = ep.snd_nxt.wrapping_add(r.below(20000) as u32).wrapping_sub(10000);
            let ack = ep.rcv_nxt.wrapping_add(r.below(20000) as u32).wrapping_sub(10000);
            (seq, ack)
        };
        let window = r.pick(&[0u16, 1, 65535, 8192]);
        let data = crate::input::noise(r.u32(), r.pick(&[0usize, 0, 1, 100, 1400]));
        let s = frames::tcp(ep.ip, ep.mip, ep.port, ep.mport, seq, ack, flags, window, &[], &data, Sum::Right);
        let f = frames::ipv4(ETH0_MAC, ep.mac, &Ip::new(ep.ip, ep.mip, IP_PROTO_TCP), &s);
        /* From outside every window it changes nothing -- unless it is a
         * SYN, and the machine has let the connection go: then it is a new
         * one's first segment, wherever it is in the sequence space. */
        if !far || flags & seg::SYN != 0 {
            self.w.peers.tcp.eps[e].tainted = true;
        }
        self.w.net.inject(f);
    }

    /// An ICMP error from a router, quoting a segment the machine sent: the
    /// peer unreachable, or a lie.
    fn icmp_error(&mut self, r: &mut Input) {
        if self.w.peers.seen.is_empty() {
            return;
        }
        let q = self.w.peers.seen[r.below(self.w.peers.seen.len() as u64) as usize].clone();
        let mut quoted = q[14..].to_vec();
        quoted.truncate(r.pick(&[28usize, 40, 60, 20, 8]));
        if r.u8() < 64 && quoted.len() >= 28 {
            /* A quote whose sequence number was never sent */
            let s = netwire::be32(&quoted, 24).wrapping_add(r.u32() | 0x1000_0000);
            netwire::set_be32(&mut quoted, 24, s);
        }
        let code = r.pick(&[icmp::PORT_UNREACH, icmp::PROTO_UNREACH, 1, 0, 4]);
        let m = frames::icmp(icmp::DEST_UNREACH, code, 0, 0, &quoted, if r.u8() < 16 { Sum::Wrong } else { Sum::Right });
        let router = r.pick(&[GW_IP, self.peer_ip]);
        let f = frames::ipv4(ETH0_MAC, GW_MAC, &Ip::new(router, ETH0_IP, IP_PROTO_ICMP), &m);
        /* An error that quotes a live connection's unacknowledged data ends
         * it, as RFC 1122 asks: whatever it names is fragile from here. */
        for l in self.links.iter_mut() {
            l.fragile = true;
        }
        self.w.net.inject(f);
    }
}

/// An initial sequence number: anything, and often one about to wrap.
fn iss(r: &mut Input) -> u32 {
    match r.u8() % 4 {
        0 => r.pick(&[0u32, 1, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF, 0xFFFF_F000, 0xFFFF_FFF0]),
        _ => r.u32(),
    }
}

pub fn tcp(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);

    let far = r.u8() < 64;
    let (peer_ip, peer_mac) = if far { (FAR_IP, GW_MAC) } else { (NEAR_IP, NEAR_MAC) };
    let link = Link::from_input(r);
    let lossy = link.loss > 8;
    let mut lan = Lan::new();
    lan.add(NEAR_IP, NEAR_MAC);
    let mut w = World::new(link, Wire { lan, tcp: Tcp::new(peer_ip, peer_mac), seen: Vec::new() });
    w.net.expect.ips = vec![ETH0_IP];
    /* The machine's own sequence numbers, when the input says: where they
     * wrap. */
    if r.u8() < 64 {
        let v = iss(r);
        crate::machine::random_queue(&v.to_le_bytes());
    }

    let listeners = LISTEN_PORTS.iter().map(|&p| (p, TCP.listen(&nic, p))).collect();
    let mut st = State { w, links: Vec::new(), listeners, accepts: Vec::new(), next_peer_port: PEER_PORT_BASE,
                         next_src_port: PEER_SRC_BASE, nic, peer_ip, accepted: BTreeMap::new(), lossy };

    while let Some(op) = r.op(20) {
        let k = if st.links.is_empty() { 0 } else { r.below(st.links.len() as u64) as usize };
        let some = !st.links.is_empty();
        match op {
            0 | 1 if st.links.len() >= 24 => {}
            0 => st.connect(r),
            1 => st.peer_connect(r),
            2 => st.accept(r),
            3..=7 if !some => {}
            3 | 4 => st.send(r, k),
            5 | 6 => st.recv(r, k),
            7 => {
                if r.u8() < 64 {
                    st.close(k, r.u8() < 64);
                }
            }
            8 | 9 => {
                if let Some(e) = st.links.get(k).and_then(|l| st.ep(l)) {
                    let n = r.pick(&[1u64, 100, 1460, 5000, 30000]);
                    st.w.peers.tcp.write(&mut st.w.net, e, n);
                }
            }
            10 => {
                if let Some(e) = st.links.get(k).and_then(|l| st.ep(l)) {
                    let n = r.pick(&[1u32, 500, 8192, 65535]);
                    st.w.peers.tcp.read(&mut st.w.net, e, n);
                }
            }
            11 => {
                if r.u8() >= 64 {
                    continue;
                }
                if let Some(e) = st.links.get(k).and_then(|l| st.ep(l)) {
                    if r.u8() < 32 {
                        st.w.peers.tcp.abort(&mut st.w.net, e);
                    } else {
                        st.w.peers.tcp.close(&mut st.w.net, e);
                    }
                }
            }
            12 => {
                /* The peer goes quiet, or comes back. */
                if r.u8() >= 32 {
                    continue;
                }
                if let Some(e) = st.links.get(k).and_then(|l| st.ep(l)) {
                    let ep = &mut st.w.peers.tcp.eps[e];
                    ep.silent = !ep.silent;
                    st.links[k].fragile = true;
                    st.w.peers.tcp.output(&mut st.w.net, e);
                }
            }
            13 => st.inject(r),
            14 => {
                if r.u8() < 64 {
                    st.icmp_error(r);
                }
            }
            15 => {
                /* A listener closed, with an accept perhaps waiting on it. */
                if r.u8() >= 16 {
                    continue;
                }
                let i = r.below(st.listeners.len() as u64) as usize;
                if let Some(l) = st.listeners[i].1.take() {
                    TCP.close(l);
                }
            }
            16 => {
                /* The NIC's ring fills, or drains. */
                if r.u8() >= 16 {
                    continue;
                }
                let stalled = crate::machine::nic::wire().stalled[0];
                crate::machine::nic::stall(0, !stalled);
                /* Nothing the machine sends leaves meanwhile: any of its
                 * connections may give up on its peer. */
                for l in st.links.iter_mut() {
                    l.fragile = true;
                }
            }
            _ => {
                /* Time: mostly a little, now and then long enough for a
                 * timer to run out -- a connect's, TIME-WAIT, the
                 * retransmits' give-up. */
                let step = match r.u8() % 16 {
                    0 => 0,
                    1..=9 => r.below(300) * MS,
                    10..=14 => r.below(5000) * MS,
                    _ => r.below(70) * SEC,
                };
                st.w.run_for(step);
            }
        }
        st.w.pump();
        st.harvest();
    }

    wind_down(&mut st);
}

/// The input is over: everyone finishes what they have, over a good link,
/// and then the machine must hold nothing -- and every stream that ended
/// the ordinary way must have arrived whole.
fn wind_down(st: &mut State) {
    st.w.net.link = Link::perfect();
    st.w.peers.lan.deaf = 0;
    crate::machine::nic::stall(0, false);
    for e in 0..st.w.peers.tcp.eps.len() {
        if st.w.peers.tcp.eps[e].silent {
            st.w.peers.tcp.eps[e].silent = false;
            for l in st.links.iter_mut().filter(|l| l.id == st.w.peers.tcp.eps[e].id) {
                l.fragile = true;
            }
            /* Back: what it held while it was away goes out now. */
            st.w.peers.tcp.output(&mut st.w.net, e);
        }
    }

    /* The calls under way end: the peer reads everything and closes, so a
     * send or a recv waiting on it returns; the listeners close, so an
     * accept does. */
    /* A peer that offered no room at all offers some now: a call blocked
     * on its window would otherwise be blocked for good, as it is meant
     * to be. */
    for ep in st.w.peers.tcp.eps.iter_mut() {
        ep.rcv_cap = ep.rcv_cap.max(8192);
    }
    /* A peer that has let its end go -- reset it, or given up on it --
     * while the machine's reset or the peer's never arrived: its host
     * resets the machine's end now, as it would the next segment it had
     * from it. Without it a machine that neither sends nor is sent
     * anything keeps a connection to nobody for good, as TCP does with no
     * keepalive -- which is what it is meant to do. */
    for e in 0..st.w.peers.tcp.eps.len() {
        let ep = &st.w.peers.tcp.eps[e];
        if ep.st == S::Closed && matches!(ep.reset, Some(tcpm::Reset::ByPeer) | Some(tcpm::Reset::GaveUp)) {
            /* At the machine's own edge: what its program has read and
             * what it holds unread is all it has taken of the stream --
             * more than the peer knows was taken, when the machine's
             * acknowledgements were lost. */
            let id = ep.id;
            let Some(k) = st.links.iter().position(|l| l.id == id) else { continue };
            let held = st.slot(k).map_or(0, |c| c.recv_used as u64);
            let edge = ep.iss.wrapping_add(1).wrapping_add((st.links[k].got + held) as u32);
            let s = frames::tcp(ep.ip, ep.mip, ep.port, ep.mport, edge, 0, seg::RST, 0, &[], &[], Sum::Right);
            let f = frames::ipv4(ETH0_MAC, ep.mac, &Ip::new(ep.ip, ep.mip, IP_PROTO_TCP), &s);
            st.w.net.inject(f);
            st.links[k].fragile = true;
        }
    }
    /* Sixty seconds, and on for as long as the calls still waiting are
     * getting somewhere: a transfer to a peer that takes a hundred bytes
     * at a time, one retransmit a second, is slow, not stuck. */
    let mut end = sched::now() + 60 * SEC;
    let give_up = sched::now() + 3600 * SEC;
    let progress = |st: &State| -> u64 {
        TCP.stats().tx as u64 + st.w.peers.tcp.eps.iter().map(|e| e.got).sum::<u64>()
    };
    let mut seen = progress(st);
    loop {
        for e in 0..st.w.peers.tcp.eps.len() {
            let unread = st.w.peers.tcp.eps[e].unread;
            st.w.peers.tcp.read(&mut st.w.net, e, unread);
            if st.w.peers.tcp.eps[e].open() && !st.w.peers.tcp.eps[e].fin_queued {
                st.w.peers.tcp.close(&mut st.w.net, e);
            }
        }
        for (_, l) in st.listeners.iter_mut() {
            if let Some(l) = l.take() {
                TCP.close(l);
            }
        }
        st.w.run_for(100 * MS);
        st.harvest();
        let busy = st.links.iter().any(|l| l.call.is_some()) || !st.accepts.is_empty();
        if !busy {
            break;
        }
        if sched::now() >= end {
            let now_seen = progress(st);
            if now_seen == seen || sched::now() >= give_up {
                break;
            }
            seen = now_seen;
            end = sched::now() + 60 * SEC;
        }
    }
    for k in 0..st.links.len() {
        let l = &st.links[k];
        /* A connection the attacker had a hand in may be one it holds open:
         * its program's call may wait on it for good, as it would on a real
         * peer that never sends -- and its slot is kept as long. */
        if st.ep(l).is_some_and(|e| st.w.peers.tcp.eps[e].tainted) {
            continue;
        }
        if let Some(call) = &l.call {
            let what = match call {
                Call::Connect(_) => "connect",
                Call::Accept(_) => "accept",
                Call::Send(..) => "send",
                Call::Recv(_) => "recv",
            };
            let slot = st.slot(k).map(|c| format!("{} with {} bytes to send, {} in flight, {} to read",
                                                  c.state.name(), c.send_used, c.in_flight, c.recv_used))
                .unwrap_or_else(|| "no slot".into());
            let peer = st.ep(l).map(|e| {
                let ep = &st.w.peers.tcp.eps[e];
                format!("{:?} {:?}, window {} of {}, got {}, queued {}, una {} nxt {} its window {}{}{}", ep.st,
                        ep.reset, ep.rcv_cap.saturating_sub(ep.unread), ep.rcv_cap, ep.got, ep.queued, ep.snd_una,
                        ep.snd_nxt, ep.snd_wnd, if ep.silent { ", silent" } else { "" },
                        if ep.tainted { ", tainted" } else { "" })
            }).unwrap_or_else(|| "none".into());
            panic!("invariant: connection {}'s {} (the peer's port {}) never returned, with the peer closed and \
                    everything read: the machine's end {}; the peer's {}", k, what, l.port, slot, peer);
        }
    }
    invariant!(st.accepts.is_empty(), "an accept never returned, with its listener closed");

    /* Each program reads what is left, to the end of the stream, and
     * closes. */
    for k in 0..st.links.len() {
        let Some(c) = st.idle(k) else { continue };
        if st.ep(&st.links[k]).is_some_and(|e| st.w.peers.tcp.eps[e].tainted) {
            /* A connection the attacker had a hand in: its program gives
             * up on it. */
            st.close(k, true);
            continue;
        }
        if !st.links[k].eof {
            let app = App::spawn("drain", 1, move || {
                let mut all = Vec::new();
                let mut buf = vec![0u8; 4096];
                loop {
                    let n = TCP.recv(c, &mut buf, 30_000);
                    if n <= 0 {
                        return (n, all);
                    }
                    all.extend_from_slice(&buf[..n as usize]);
                }
            });
            st.w.wait_app(&app, 60 * SEC);
            let (n, data) = app.take().unwrap_or_else(|| panic!("invariant: connection {} never read the end of its \
                                                                 stream", k));
            if !data.is_empty() {
                st.received(k, data.len() as isize, &data);
            }
            st.received(k, n, &[]);
        }
        st.close(k, false);
    }

    /* Long enough for every retransmit and TIME-WAIT to run out -- and
     * then for as long as the machine is still getting somewhere: a
     * connection that ends slowly, a segment of one byte a second to a
     * peer that asked for no more, is not one kept for good. The peers
     * read everything meanwhile. */
    let mut sent = TCP.stats().tx;
    let give_up = sched::now() + 4 * 3600 * SEC;
    for round in 0.. {
        for e in 0..st.w.peers.tcp.eps.len() {
            let unread = st.w.peers.tcp.eps[e].unread;
            st.w.peers.tcp.read(&mut st.w.net, e, unread);
        }
        st.w.run_for(if round == 0 { 180 * SEC } else { 60 * SEC });
        let busy = (0..net::tcp::MAX_CONNECTIONS).any(|i| TCP.snapshot(i).is_some());
        let now_sent = TCP.stats().tx;
        if !busy || now_sent == sent || sched::now() >= give_up {
            break;
        }
        sent = now_sent;
    }

    for l in &st.links {
        let Some(e) = st.ep(l) else { continue };
        let ep = &st.w.peers.tcp.eps[e];
        if ep.tainted || l.fragile || l.aborted || ep.reset.is_some() || l.conn.is_none() {
            continue;
        }
        /* Closed the ordinary way at both ends: every byte the program
         * sent reached the peer, and every byte the peer sent the
         * program. */
        invariant!(ep.got == l.sent, "connection {}: the machine's program sent {} bytes, the peer got {} before the \
                   FIN", l.id, l.sent, ep.got);
        if l.eof {
            invariant!(l.got == ep.queued, "connection {}: the peer sent {} bytes, the machine's program read {} to the \
                       end of the stream", l.id, ep.queued, l.got);
        }
    }
    /* What the attacker may be holding open: the machine's end of a
     * connection it had a hand in, and a program's call waiting on one. */
    let held: Vec<(u16, u16)> = st.w.peers.tcp.eps.iter().filter(|e| e.tainted).map(|e| (e.mport, e.port)).collect();
    let waiting: Vec<usize> = st.links.iter()
        .filter(|l| st.ep(l).is_some_and(|e| st.w.peers.tcp.eps[e].tainted))
        .filter_map(|l| match &l.call {
            Some(Call::Send(app, _)) => Some(app.task()),
            Some(Call::Recv(app)) => Some(app.task()),
            Some(Call::Connect(app)) => Some(app.task()),
            _ => None,
        })
        .collect();
    super::audit_but(&held, &waiting);
}
