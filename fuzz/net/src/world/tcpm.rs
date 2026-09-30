//! A TCP of the world's: the other end of every connection the machine has,
//! written to be right rather than fast -- in-order delivery, no SACK, no
//! congestion control, a plain retransmit timer -- and to say, of every
//! segment the machine sends it, what the other end can know is wrong: an
//! acknowledgement of data never sent; data past the window, larger than
//! the segment size it was told, or not the bytes the machine's program
//! wrote at that place in the stream; a FIN anywhere but just after the
//! last byte the program wrote; a SYN on a connection that has one; a
//! SYN-ACK that acknowledges another SYN.
//!
//! Each connection's two streams are a function of its number and the
//! offset (`peer_byte`, `machine_byte`): what either end should have
//! received is known without keeping it.

use netwire::{tcp, IP_PROTO_TCP};

use super::frames::{self, Ip, Mac, Sum};
use super::Net;
use crate::machine::{sched, ETH0_MAC};

/// What the peer sends without an MSS option from the machine, and takes
/// without one of its own (RFC 9293).
pub const DEFAULT_MSS: usize = 536;
/// The peer's first retransmit timeout, and the most retransmits it makes.
const RTO: u64 = 300_000_000;
const MAX_RTO: u64 = 4_000_000_000;
const TRIES: u32 = 10;
/// TIME-WAIT, the peer's: short, since it has no old segments to fear.
const TIME_WAIT: u64 = 2_000_000_000;
/// The least time between two ACKs for segments from outside the window.
const OOW_GAP: u64 = 500_000_000;

/// Byte `off` of what the peer's program writes on connection `id`.
pub fn peer_byte(id: usize, off: u64) -> u8 {
    let x = (off ^ (id as u64) << 40).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (x >> 56) as u8
}

/// Byte `off` of what the machine's program writes on connection `id`.
pub fn machine_byte(id: usize, off: u64) -> u8 {
    let x = (off ^ (id as u64) << 40 ^ 0x5555).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    (x >> 56) as u8
}

pub fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

pub fn before_eq(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum S {
    SynSent,
    SynRcvd,
    Established,
    FinWait1,
    FinWait2,
    Closing,
    TimeWait,
    CloseWait,
    LastAck,
    Closed,
}

/// Who ended a connection other than by the FIN exchange.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reset {
    /// The machine reset it.
    ByMachine,
    /// The peer did, its program's abort.
    ByPeer,
    /// The peer's retransmits ran out.
    GaveUp,
}

/// A connection whose streams are a program's own bytes rather than the
/// functions of an offset: what a server of the world's -- HTTP, TLS --
/// reads and writes.
#[derive(Default)]
pub struct Data {
    /// What the machine sent, taken in order.
    pub rx: Vec<u8>,
    /// What the peer's program has written: all of it, the part sent
    /// included.
    pub tx: Vec<u8>,
}

/// One connection, the peer's end of it.
pub struct Ep {
    /// Its number: what its streams are made from.
    pub id: usize,
    pub ip: u32,
    pub port: u16,
    /// Where its frames come from: its own MAC, or the gateway's.
    pub mac: Mac,
    pub mip: u32,
    pub mport: u16,
    pub st: S,

    pub iss: u32,
    pub snd_una: u32,
    pub snd_nxt: u32,
    /// The machine's window, as it last said, and which segment said it.
    pub snd_wnd: u32,
    wl1: u32,
    wl2: u32,
    /// The most the peer puts in a segment: the machine's MSS.
    pub mss: usize,
    /// The MSS the peer told the machine: the most the machine may send
    /// in one.
    pub our_mss: usize,
    /// What the peer's program has written, and whether it has closed.
    pub queued: u64,
    pub fin_queued: bool,
    pub fin_seq: Option<u32>,

    pub irs: u32,
    pub rcv_nxt: u32,
    /// The peer's receive buffer, and what of it the program has not read.
    pub rcv_cap: u32,
    pub unread: u32,
    /// The machine's stream: bytes taken in order, and where its FIN was.
    pub got: u64,
    pub fin_got: Option<u64>,
    /// What came ahead of a hole, kept: ranges of the stream, and where a
    /// FIN ahead of one is.
    ooo: Vec<(u64, u64)>,
    ooo_fin: Option<u64>,
    /// The furthest the peer's window ever let the machine send.
    pub right_edge: u32,

    rto_at: u64,
    rto: u64,
    tries: u32,
    probe_at: u64,
    tw_until: u64,
    oow_at: u64,
    /// The peer sends nothing and hears nothing: a host gone.
    pub silent: bool,
    /// The attacker put something in the connection that its ends could
    /// not tell from their own: what it carries is no longer only theirs.
    pub tainted: bool,
    pub reset: Option<Reset>,
    /// The options the peer's SYN or SYN-ACK carries.
    pub syn_opts: Vec<u8>,
    /// What the machine's program has handed to its sends on this
    /// connection, the call under way included: the most of its stream
    /// the machine may have sent.
    pub offered: u64,
    /// What the machine's program had written when it closed: where its
    /// FIN must be.
    pub closed_at: Option<u64>,
    /// The streams as bytes, for a program's own protocol.
    pub data: Option<Data>,
}

impl Ep {
    /// What the peer's program wrote at `from..to` of its stream.
    fn peer_bytes(&self, from: u64, to: u64) -> Vec<u8> {
        match &self.data {
            Some(d) => d.tx[from as usize..to as usize].to_vec(),
            None => (from..to).map(|o| peer_byte(self.id, o)).collect(),
        }
    }

    /// The peer's program writes `bytes`.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        if let Some(d) = self.data.as_mut() {
            d.tx.extend_from_slice(bytes);
            self.queued = d.tx.len() as u64;
        }
    }

    fn window(&self) -> u32 {
        self.rcv_cap.saturating_sub(self.unread).min(0xFFFF)
    }

    /// The stream offset of the machine's sequence number `seq`.
    fn machine_offset(&self, seq: u32) -> u64 {
        u64::from(seq.wrapping_sub(self.irs.wrapping_add(1)))
    }

    /// The machine's stream taken, in order, up to offset `to`.
    fn take(&mut self, to: u64) {
        if to <= self.got {
            return;
        }
        let n = to - self.got;
        self.got = to;
        self.unread += n as u32;
        self.rcv_nxt = self.rcv_nxt.wrapping_add(n as u32);
    }

    /// What was kept ahead of a hole, taken as far as it now joins up.
    fn drain_ooo(&mut self) {
        loop {
            let room = u64::from(self.rcv_cap.saturating_sub(self.unread));
            let got = self.got;
            let next = self.ooo.iter().filter(|&&(a, b)| a <= got && b > got).map(|&(_, b)| b).max();
            match next {
                Some(to) if room > 0 => self.take(to.min(got + room)),
                _ => break,
            }
            if self.got == got {
                break;
            }
        }
        let got = self.got;
        self.ooo.retain(|&(_, b)| b > got);
    }

    /// Where the next byte the peer sends lies in its stream.
    fn sent_offset(&self) -> u64 {
        let sent = u64::from(self.snd_nxt.wrapping_sub(self.iss.wrapping_add(1)));
        if self.fin_seq.is_some() { sent.saturating_sub(1) } else { sent }
    }

    pub fn open(&self) -> bool {
        !matches!(self.st, S::Closed | S::TimeWait)
    }
}

/// The peer's side of every connection.
pub struct Tcp {
    pub eps: Vec<Ep>,
    /// Ports the peer listens on for the machine's connects, and the
    /// connection number each one's first connection is.
    pub listening: Vec<(u16, usize)>,
    /// Ports nothing answers on at all, not even with a reset: a host that
    /// is not there.
    pub ignored: Vec<u16>,
    /// Ports whose connections carry a program's own bytes (`Data`), and
    /// the numbers such connections are given, one each, as they come.
    pub serve: Vec<u16>,
    pub next_id: usize,
    /// Where the peer's frames come from, and what its SYN-ACKs are like.
    pub ip: u32,
    pub mac: Mac,
    pub accept_iss: u32,
    pub accept_mss: Option<u16>,
    pub accept_window: u32,
    /// Segments from the machine the peer took, for the statistics.
    pub segments: u64,
}

impl Tcp {
    pub fn new(ip: u32, mac: Mac) -> Tcp {
        Tcp { eps: Vec::new(), listening: Vec::new(), ignored: Vec::new(), serve: Vec::new(), next_id: 1000, ip, mac, accept_iss: 1, accept_mss: Some(1460),
              accept_window: 8192, segments: 0 }
    }

    /// A segment of the peer's on `ep`. Whatever carries an ACK advertises
    /// the window as far as it reaches: the machine may send up to there.
    fn emit(net: &mut Net, ep: &mut Ep, flags: u8, seq: u32, opts: &[u8], data: &[u8]) {
        if ep.silent {
            return;
        }
        if flags & tcp::ACK_FLAG != 0 {
            ep.right_edge = later(ep.right_edge, ep.rcv_nxt.wrapping_add(ep.window()));
        }
        let ack = if flags & tcp::ACK_FLAG != 0 { ep.rcv_nxt } else { 0 };
        let s = frames::tcp(ep.ip, ep.mip, ep.port, ep.mport, seq, ack, flags, ep.window() as u16, opts, data,
                            Sum::Right);
        net.send(frames::ipv4(ETH0_MAC, ep.mac, &Ip::new(ep.ip, ep.mip, IP_PROTO_TCP), &s));
    }

    fn ack_now(net: &mut Net, ep: &mut Ep) {
        let seq = ep.snd_nxt;
        Self::emit(net, ep, tcp::ACK_FLAG, seq, &[], &[]);
    }

    /// The peer opens a connection to the machine's `mport`: connection
    /// `id`, from `port`.
    #[allow(clippy::too_many_arguments)]
    pub fn connect(&mut self, net: &mut Net, id: usize, port: u16, mip: u32, mport: u16, iss: u32, opts: Vec<u8>,
                   window: u32) -> usize {
        let our_mss = told_mss(&opts);
        let ep = Ep {
            id, ip: self.ip, port, mac: self.mac, mip, mport, st: S::SynSent,
            iss, snd_una: iss, snd_nxt: iss.wrapping_add(1), snd_wnd: 0, wl1: 0, wl2: 0, mss: DEFAULT_MSS, our_mss,
            queued: 0, fin_queued: false, fin_seq: None,
            irs: 0, rcv_nxt: 0, rcv_cap: window, unread: 0, got: 0, fin_got: None, ooo: Vec::new(), ooo_fin: None,
            right_edge: 0,
            rto_at: sched::now() + RTO, rto: RTO, tries: 0, probe_at: 0, tw_until: 0, oow_at: 0,
            silent: false, tainted: false, reset: None, syn_opts: opts, offered: 0, closed_at: None, data: None,
        };
        let mut ep = ep;
        let opts = ep.syn_opts.clone();
        Self::emit(net, &mut ep, tcp::SYN, iss, &opts, &[]);
        self.eps.push(ep);
        self.eps.len() - 1
    }

    /// The peer's end of connection `id`, if it has one: the newest.
    pub fn find_id(&self, id: usize) -> Option<usize> {
        self.eps.iter().rposition(|e| e.id == id)
    }

    fn find(&self, port: u16, mport: u16) -> Option<usize> {
        /* The newest first: an old incarnation in TIME-WAIT with the same
         * ports is the one a new SYN does not belong to. */
        self.eps.iter().rposition(|e| e.port == port && e.mport == mport)
    }

    /// A frame the machine sent: true when it was a TCP segment for the
    /// peer's address.
    pub fn on_frame(&mut self, net: &mut Net, frame: &[u8]) -> bool {
        let Some(p) = super::check::ipv4_parts(frame) else { return false };
        if p.proto != IP_PROTO_TCP || p.dst != self.ip || p.l4.len() < tcp::HDR_LEN {
            return false;
        }
        let s = p.l4;
        let hlen = tcp::header_len(s).clamp(tcp::HDR_LEN, s.len());
        let (seq, ack, flags, wnd) = (tcp::seq(s), tcp::ack(s), tcp::flags(s), u32::from(tcp::window(s)));
        let (port, mport) = (tcp::dst_port(s), tcp::src_port(s));
        let opts = &s[tcp::HDR_LEN..hlen];
        let payload = &s[hlen..];
        self.segments += 1;

        let at = match self.find(port, mport) {
            Some(i) if self.eps[i].st != S::Closed || flags & tcp::SYN == 0 => Some(i),
            _ => None,
        };
        let i = match at {
            Some(i) => i,
            None => {
                if self.ignored.contains(&port) {
                    return true;
                }
                if flags & tcp::SYN != 0 && flags & tcp::ACK_FLAG == 0 {
                    if let Some(&(_, id)) = self.listening.iter().find(|(lp, _)| *lp == port) {
                        self.accept(net, id, port, p.src, mport, seq, wnd, opts);
                        return true;
                    }
                    if self.serve.contains(&port) {
                        let id = self.next_id;
                        self.next_id += 1;
                        self.accept(net, id, port, p.src, mport, seq, wnd, opts);
                        return true;
                    }
                }
                self.refuse(net, p.src, port, mport, seq, ack, flags, payload.len());
                return true;
            }
        };
        self.segment(net, i, seq, ack, flags, wnd, opts, payload);
        true
    }

    /// A SYN from the machine to a port the peer listens on.
    #[allow(clippy::too_many_arguments)]
    fn accept(&mut self, net: &mut Net, id: usize, port: u16, mip: u32, mport: u16, seq: u32, wnd: u32, opts: &[u8]) {
        let iss = self.accept_iss;
        let our_opts = self.accept_mss.map(frames::mss).unwrap_or_default();
        let ep = Ep {
            id, ip: self.ip, port, mac: self.mac, mip, mport, st: S::SynRcvd,
            iss, snd_una: iss, snd_nxt: iss.wrapping_add(1), snd_wnd: wnd, wl1: seq, wl2: 0,
            mss: mss_of(opts).map_or(DEFAULT_MSS, |m| usize::from(m.max(1))),
            our_mss: told_mss(&self.accept_mss.map(frames::mss).unwrap_or_default()),
            queued: 0, fin_queued: false, fin_seq: None,
            irs: seq, rcv_nxt: seq.wrapping_add(1), rcv_cap: self.accept_window, unread: 0, got: 0, fin_got: None,
            ooo: Vec::new(), ooo_fin: None,
            right_edge: seq.wrapping_add(1).wrapping_add(self.accept_window.min(0xFFFF)),
            rto_at: sched::now() + RTO, rto: RTO, tries: 0, probe_at: 0, tw_until: 0, oow_at: 0,
            silent: false, tainted: false, reset: None, syn_opts: our_opts, offered: 0, closed_at: None,
            data: if self.serve.contains(&port) { Some(Data::default()) } else { None },
        };
        let mut ep = ep;
        /* These ports had a connection before, which the peer has let go
         * of: this SYN is that one's, kept by the network past its end, or
         * the attacker's -- a stream that is no program's either way. */
        ep.tainted = self.eps.iter().any(|e| e.port == port && e.mport == mport);
        let opts = ep.syn_opts.clone();
        Self::emit(net, &mut ep, tcp::SYN | tcp::ACK_FLAG, iss, &opts, &[]);
        self.eps.push(ep);
    }

    /// What a host answers a segment for no connection of its: a reset,
    /// unless it is one.
    #[allow(clippy::too_many_arguments)]
    fn refuse(&self, net: &mut Net, mip: u32, port: u16, mport: u16, seq: u32, ack: u32, flags: u8, len: usize) {
        if flags & tcp::RST != 0 {
            return;
        }
        let mut rack = seq.wrapping_add(len as u32);
        if flags & tcp::SYN != 0 {
            rack = rack.wrapping_add(1);
        }
        if flags & tcp::FIN != 0 {
            rack = rack.wrapping_add(1);
        }
        let (rseq, rflags) = if flags & tcp::ACK_FLAG != 0 { (ack, tcp::RST) } else { (0, tcp::RST | tcp::ACK_FLAG) };
        let s = frames::tcp(self.ip, mip, port, mport, rseq, rack, rflags, 0, &[], &[], Sum::Right);
        net.send(frames::ipv4(ETH0_MAC, self.mac, &Ip::new(self.ip, mip, IP_PROTO_TCP), &s));
    }

    /// One segment from the machine, on the peer's end `i`.
    #[allow(clippy::too_many_arguments)]
    fn segment(&mut self, net: &mut Net, i: usize, seq: u32, ack: u32, flags: u8, wnd: u32, opts: &[u8],
               payload: &[u8]) {
        let ep = &mut self.eps[i];
        if ep.silent {
            return;
        }
        let (syn, ackf, fin, rst) = (flags & tcp::SYN != 0, flags & tcp::ACK_FLAG != 0, flags & tcp::FIN != 0,
                                     flags & tcp::RST != 0);
        let now = sched::now();

        if ep.st == S::SynSent {
            if rst {
                if ackf && ack == ep.iss.wrapping_add(1) {
                    ep.reset = Some(Reset::ByMachine);
                    ep.st = S::Closed;
                    ep.rto_at = 0;
                }
                return;
            }
            if syn && ackf {
                invariant!(ack == ep.iss.wrapping_add(1), "the machine's SYN-ACK acknowledges {} for a SYN of {}", ack,
                           ep.iss);
                invariant!(payload.is_empty(), "data on the machine's SYN-ACK");
                ep.irs = seq;
                ep.rcv_nxt = seq.wrapping_add(1);
                ep.right_edge = ep.rcv_nxt.wrapping_add(ep.window());
                ep.snd_una = ack;
                ep.snd_wnd = wnd;
                ep.wl1 = seq;
                ep.wl2 = ack;
                ep.mss = mss_of(opts).map_or(DEFAULT_MSS, |m| usize::from(m.max(1)));
                ep.st = S::Established;
                ep.rto_at = 0;
                ep.tries = 0;
                ep.rto = RTO;
                Self::ack_now(net, ep);
                self.output(net, i);
            }
            return;
        }
        if matches!(ep.st, S::Closed) {
            /* A host with no connection for it: a reset. */
            let (mip, port, mport) = (ep.mip, ep.port, ep.mport);
            self.refuse(net, mip, port, mport, seq, ack, flags, payload.len());
            return;
        }

        if rst {
            /* Acceptable when it is in the peer's window (RFC 9293). */
            let in_window = seq == ep.rcv_nxt || (!before(seq, ep.rcv_nxt) && before(seq, later(ep.right_edge,
                                                                                           ep.rcv_nxt.wrapping_add(1))));
            if in_window {
                ep.reset = Some(Reset::ByMachine);
                ep.st = S::Closed;
                ep.rto_at = 0;
            }
            return;
        }
        if syn {
            /* The machine's SYN or SYN-ACK again: the peer's answer to it was
             * lost. Anything else is a SYN on a connection that has one. */
            if seq != ep.irs {
                /* Another connection's SYN on these ports: the machine's
                 * answer to the attacker's, or its own from long ago that
                 * the network kept. Not this connection's; as RFC 5961
                 * answers one, an ACK saying where this end is. */
                Self::ack_now(net, ep);
                return;
            }
            match ep.st {
                S::SynRcvd => {
                    let (iss, opts) = (ep.iss, ep.syn_opts.clone());
                    Self::emit(net, ep, tcp::SYN | tcp::ACK_FLAG, iss, &opts, &[])
                }
                _ => Self::ack_now(net, ep),
            }
            return;
        }
        if ep.st == S::SynRcvd {
            if !ackf {
                return;
            }
            if ack != ep.iss.wrapping_add(1) {
                /* Not an answer to this SYN-ACK: an old one's, which the
                 * network kept. A reset, as RFC 9293 answers it. */
                let (mip, port, mport) = (ep.mip, ep.port, ep.mport);
                self.refuse(net, mip, port, mport, seq, ack, flags, payload.len());
                return;
            }
            ep.st = S::Established;
            ep.rto_at = 0;
            ep.tries = 0;
            ep.rto = RTO;
        }

        if ackf {
            invariant!(before_eq(ack, ep.snd_nxt) || ep.tainted,
                       "the machine acknowledged {} past everything the peer sent ({})", ack, ep.snd_nxt);
            if before(ep.snd_una, ack) {
                ep.snd_una = ack;
                ep.tries = 0;
                ep.rto = RTO;
                ep.rto_at = if ep.snd_una == ep.snd_nxt { 0 } else { now + ep.rto };
            }
            if before(ep.wl1, seq) || (ep.wl1 == seq && before_eq(ep.wl2, ack)) {
                ep.snd_wnd = wnd;
                ep.wl1 = seq;
                ep.wl2 = ack;
            }
            if let Some(f) = ep.fin_seq {
                if !before(ack, f.wrapping_add(1)) {
                    match ep.st {
                        S::FinWait1 => ep.st = S::FinWait2,
                        S::Closing => {
                            ep.st = S::TimeWait;
                            ep.tw_until = now + TIME_WAIT;
                        }
                        S::LastAck => ep.st = S::Closed,
                        _ => {}
                    }
                }
            }
        }

        if !payload.is_empty() {
            let end = seq.wrapping_add(payload.len() as u32);
            invariant!(payload.len() <= ep.our_mss, "a segment of {} bytes to a peer whose MSS is {}", payload.len(),
                       ep.our_mss);
            invariant!(before_eq(end, ep.right_edge) || ep.tainted,
                       "the machine sent [{}, {}) past the window, whose edge was never past {}", seq, end,
                       ep.right_edge);
            /* Every byte where the machine's program put it in the stream
             * -- as far as what it has written reaches. A program's own
             * protocol is its server's to judge. */
            if !ep.tainted && ep.data.is_none() {
                let from = ep.machine_offset(seq);
                invariant!(from < 1 << 31, "the machine sent data at {}, before its stream's start {}", seq,
                           ep.irs.wrapping_add(1));
                invariant!(from + payload.len() as u64 <= ep.offered,
                           "the machine sent stream bytes [{}, {}) of connection {}, its program having written {}",
                           from, from + payload.len() as u64, ep.id, ep.offered);
                for (k, &b) in payload.iter().enumerate() {
                    let off = from + k as u64;
                    invariant!(b == machine_byte(ep.id, off), "byte {} of connection {}'s stream sent as {:#04x}, \
                               its program wrote {:#04x}", off, ep.id, b, machine_byte(ep.id, off));
                }
            }
            if ep.fin_got.is_none() {
                /* In order, it is taken; ahead of a hole but in the window,
                 * it is kept for when the hole fills, as a receiver with a
                 * reassembly queue keeps it. */
                let from = ep.machine_offset(seq);
                let to = from + payload.len() as u64;
                if from <= ep.got && ep.got < to {
                    let room = u64::from(ep.rcv_cap.saturating_sub(ep.unread));
                    let (got, upto) = (ep.got, to.min(ep.got + room));
                    if let Some(d) = ep.data.as_mut() {
                        d.rx.extend_from_slice(&payload[(got - from) as usize..(upto - from) as usize]);
                    }
                    ep.take(upto);
                } else if from > ep.got && from < 1 << 31 && before_eq(end, ep.right_edge) && ep.data.is_none() {
                    ep.ooo.push((from, to));
                }
                ep.drain_ooo();
            }
            Self::ack_now(net, ep);
        }

        /* A segment outside the window -- an old one, a window probe --
         * is answered with an ACK saying where the peer is (RFC 9293
         * 3.10.7.4): what a probe is sent to hear. At most one a half
         * second, as Linux answers them, or two ends each outside the
         * other's window answer each other for good. */
        if payload.is_empty() && !fin && seq != ep.rcv_nxt && now >= ep.oow_at.saturating_add(OOW_GAP) {
            ep.oow_at = now;
            Self::ack_now(net, ep);
        }

        if fin && ep.fin_got.is_none() {
            let at = ep.machine_offset(seq.wrapping_add(payload.len() as u32));
            if at > ep.got && at < 1 << 31 {
                ep.ooo_fin = Some(at);
            }
        }
        let fin_now = ep.fin_got.is_none() && ep.ooo_fin == Some(ep.got);
        if fin || fin_now {
            let at = seq.wrapping_add(payload.len() as u32);
            if (at == ep.rcv_nxt || fin_now) && ep.fin_got.is_none() {
                if !ep.tainted && ep.data.is_none() {
                    if let Some(closed) = ep.closed_at {
                        invariant!(ep.got == closed, "the machine's FIN came after {} bytes of connection {}, its \
                                   program having written {} when it closed", ep.got, ep.id, closed);
                    } else {
                        panic!("invariant: the machine sent a FIN on connection {}, which its program has not closed",
                               ep.id);
                    }
                }
                ep.fin_got = Some(ep.got);
                ep.rcv_nxt = ep.rcv_nxt.wrapping_add(1);
                match ep.st {
                    S::Established | S::SynRcvd => ep.st = S::CloseWait,
                    S::FinWait1 => ep.st = S::Closing,
                    S::FinWait2 => {
                        ep.st = S::TimeWait;
                        ep.tw_until = now + TIME_WAIT;
                    }
                    _ => {}
                }
                Self::ack_now(net, ep);
            } else if ep.fin_got.is_some() {
                Self::ack_now(net, ep);
            }
        }
        self.output(net, i);
    }

    /// What the peer can send on `i` now: data as far as the machine's
    /// window lets it, then its FIN.
    pub fn output(&mut self, net: &mut Net, i: usize) {
        let now = sched::now();
        let ep = &mut self.eps[i];
        if ep.silent || !matches!(ep.st, S::Established | S::CloseWait | S::FinWait1 | S::Closing | S::LastAck) {
            return;
        }
        loop {
            let off = ep.sent_offset();
            if off < ep.queued && ep.fin_seq.is_none() {
                let in_flight = ep.snd_nxt.wrapping_sub(ep.snd_una);
                let room = ep.snd_wnd.saturating_sub(in_flight) as u64;
                if room == 0 {
                    if ep.probe_at == 0 && ep.rto_at == 0 {
                        ep.probe_at = now + ep.rto;
                    }
                    return;
                }
                let n = (ep.queued - off).min(ep.mss as u64).min(room);
                let data = ep.peer_bytes(off, off + n);
                let seq = ep.snd_nxt;
                Self::emit(net, ep, tcp::ACK_FLAG | tcp::PSH, seq, &[], &data);
                ep.snd_nxt = ep.snd_nxt.wrapping_add(n as u32);
                if ep.rto_at == 0 {
                    ep.rto_at = now + ep.rto;
                }
                continue;
            }
            if ep.fin_queued && ep.fin_seq.is_none() {
                let seq = ep.snd_nxt;
                ep.fin_seq = Some(seq);
                Self::emit(net, ep, tcp::FIN | tcp::ACK_FLAG, seq, &[], &[]);
                ep.snd_nxt = ep.snd_nxt.wrapping_add(1);
                ep.st = match ep.st {
                    S::CloseWait => S::LastAck,
                    _ => S::FinWait1,
                };
                if ep.rto_at == 0 {
                    ep.rto_at = now + ep.rto;
                }
            }
            return;
        }
    }

    /// The peer's program writes `n` more bytes on `i`.
    pub fn write(&mut self, net: &mut Net, i: usize, n: u64) {
        let ep = &mut self.eps[i];
        if ep.fin_queued || !ep.open() {
            return;
        }
        ep.queued += n;
        if crate::machine::ECHO_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("[{:>12}] peer writes {} on connection {} ({:?}, port {}), {} queued", sched::now() / 1000, n,
                      ep.id, ep.st, ep.port, ep.queued);
        }
        self.output(net, i);
    }

    /// The peer's program reads up to `n` bytes on `i`, and tells the
    /// machine its window is open again when it was near shut.
    pub fn read(&mut self, net: &mut Net, i: usize, n: u32) {
        let ep = &mut self.eps[i];
        let before_wnd = ep.window();
        ep.unread -= n.min(ep.unread);
        /* A window that was shut, or too small for a segment, and is not
         * now: said at once, as a receiver that has read does. */
        if ep.window() > before_wnd && before_wnd < ep.mss as u32 && ep.open() && ep.st != S::SynSent {
            Self::ack_now(net, ep);
        }
    }

    /// The peer's program closes its end of `i`: a FIN after what it wrote.
    pub fn close(&mut self, net: &mut Net, i: usize) {
        self.eps[i].fin_queued = true;
        self.output(net, i);
    }

    /// The peer's program aborts `i`: a reset.
    pub fn abort(&mut self, net: &mut Net, i: usize) {
        let ep = &mut self.eps[i];
        if !ep.open() {
            return;
        }
        if ep.st != S::SynSent {
            let seq = ep.snd_nxt;
            Self::emit(net, ep, tcp::RST | tcp::ACK_FLAG, seq, &[], &[]);
        }
        ep.reset = Some(Reset::ByPeer);
        ep.st = S::Closed;
        ep.rto_at = 0;
    }

    pub fn next_timer(&self) -> u64 {
        self.eps.iter().filter(|e| !e.silent).flat_map(|e| [e.rto_at, e.probe_at, e.tw_until])
            .filter(|&t| t != 0).min().unwrap_or(u64::MAX)
    }

    /// The peer's timers: retransmits, window probes, TIME-WAIT.
    pub fn on_timer(&mut self, net: &mut Net) {
        let now = sched::now();
        for i in 0..self.eps.len() {
            let ep = &mut self.eps[i];
            if ep.silent {
                continue;
            }
            if ep.tw_until != 0 && now >= ep.tw_until {
                ep.tw_until = 0;
                ep.st = S::Closed;
            }
            if ep.probe_at != 0 && now >= ep.probe_at {
                ep.probe_at = 0;
                /* A probe of the shut window, as Linux sends one: an ACK
                 * from below it, for the machine to answer with the window
                 * it has now -- and again, backing off, while it is shut. */
                let off = ep.sent_offset();
                if ep.snd_wnd == 0 && off < ep.queued && ep.fin_seq.is_none() && ep.open() {
                    let seq = ep.snd_una.wrapping_sub(1);
                    Self::emit(net, ep, tcp::ACK_FLAG, seq, &[], &[]);
                    ep.rto = (ep.rto * 2).min(MAX_RTO);
                    ep.probe_at = now + ep.rto;
                }
            }
            if ep.rto_at != 0 && now >= ep.rto_at {
                ep.tries += 1;
                if ep.tries > TRIES {
                    ep.reset = Some(Reset::GaveUp);
                    ep.st = S::Closed;
                    ep.rto_at = 0;
                    continue;
                }
                ep.rto = (ep.rto * 2).min(MAX_RTO);
                ep.rto_at = now + ep.rto;
                let (iss, opts) = (ep.iss, ep.syn_opts.clone());
                match ep.st {
                    S::SynSent => Self::emit(net, ep, tcp::SYN, iss, &opts, &[]),
                    S::SynRcvd => Self::emit(net, ep, tcp::SYN | tcp::ACK_FLAG, iss, &opts, &[]),
                    S::Closed | S::TimeWait => ep.rto_at = 0,
                    _ => {
                        /* From the first unacknowledged byte: data, or the
                         * FIN when that is all there is left. */
                        let una_off = u64::from(ep.snd_una.wrapping_sub(ep.iss.wrapping_add(1)));
                        let data_end = match ep.fin_seq {
                            Some(f) => u64::from(f.wrapping_sub(ep.iss.wrapping_add(1))),
                            None => u64::from(ep.snd_nxt.wrapping_sub(ep.iss.wrapping_add(1))),
                        };
                        if una_off < data_end {
                            let n = (data_end - una_off).min(ep.mss as u64);
                            let data = ep.peer_bytes(una_off, una_off + n);
                            let seq = ep.snd_una;
                            Self::emit(net, ep, tcp::ACK_FLAG | tcp::PSH, seq, &[], &data);
                        } else if let Some(f) = ep.fin_seq {
                            Self::emit(net, ep, tcp::FIN | tcp::ACK_FLAG, f, &[], &[]);
                        } else {
                            ep.rto_at = 0;
                        }
                    }
                }
            }
        }
    }
}

/// `b` if it is later than `a`, wrap-safe.
fn later(a: u32, b: u32) -> u32 {
    if before(a, b) { b } else { a }
}

/// The MSS option's value, when `opts` carries a well-formed one.
pub fn mss_of(opts: &[u8]) -> Option<u16> {
    let mut at = 0;
    while at < opts.len() {
        match opts[at] {
            tcp::OPT_END => return None,
            tcp::OPT_NOP => at += 1,
            kind => {
                let len = *opts.get(at + 1)? as usize;
                if len < 2 || at + len > opts.len() {
                    return None;
                }
                if kind == tcp::OPT_MSS && len == 4 {
                    return Some(u16::from_be_bytes([opts[at + 2], opts[at + 3]]));
                }
                at += len;
            }
        }
    }
    None
}

/// The most the machine may put in a segment to a peer whose SYN carried
/// `opts`: its MSS -- 536 when it names none, or names 0, as the machine
/// reads it -- and never more than the machine's own 1460.
fn told_mss(opts: &[u8]) -> usize {
    match mss_of(opts) {
        None | Some(0) => DEFAULT_MSS,
        Some(m) => usize::from(m).min(1460),
    }
}
