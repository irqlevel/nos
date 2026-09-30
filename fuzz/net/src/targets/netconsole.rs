//! `netconsole`: the kernel log over UDP -- on the Hetzner boxes the only
//! console there is -- to a collector that is on the LAN, beyond the
//! gateway, or not there yet; over a link that loses what it likes; from a
//! machine that has no address yet, whose NIC stalls, that logs a line, a
//! line too long for a record, and bursts that overrun the ring, from every
//! task and in the middle of a send.
//!
//! What the collector gets is numbered from 0, every datagram the next
//! number -- a gap is the network's, never the machine's -- and carries
//! whole records: each line logged at most once, in the order it was
//! logged, as it was logged (cut to a record's length); and every line
//! logged reaches it in the end but for those the ring says it dropped.

use std::cell::RefCell;
use std::rc::Rc;

use netwire::udp;

use crate::machine::{self, nic};
use crate::world::lan::Lan;
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const PORT: u16 = 6666;
const MAGIC: &[u8; 4] = b"NOSC";
const HDR_LEN: usize = 8;
const DGRAM_MAX: usize = 1400;
const MAX_RECORD: usize = 512;
/// A collector beyond the gateway, and one on the LAN that is not there
/// until it is.
const FAR: u32 = 0x5DB8_D822;
const LATE: u32 = 0x0A00_0266;
const LATE_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x66];
const TAG: &[u8] = b"<rec-";

/// What the collector got: every datagram, by the number it came with.
struct Collector {
    to: u32,
    datagrams: Vec<(u32, Vec<u8>)>,
}

struct Wire {
    lan: Lan,
    got: Rc<RefCell<Collector>>,
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if self.lan.on_frame(net, frame) {
            return;
        }
        let Some(d) = udp::parse(frame) else { return };
        let mut got = self.got.borrow_mut();
        if d.dst_port != PORT || d.dst_ip != got.to {
            return;
        }
        invariant!(d.src_ip == ETH0_IP && d.src_port == PORT, "a log datagram from {}:{}",
                   netwire::Ipv4(d.src_ip), d.src_port);
        let p = d.payload;
        invariant!(p.len() > HDR_LEN && p.len() <= DGRAM_MAX && p[..4] == *MAGIC,
                   "a log datagram of {} bytes starting {:02x?}", p.len(), &p[..p.len().min(4)]);
        let seq = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
        let next = got.datagrams.last().map_or(0, |d| d.0.wrapping_add(1));
        invariant!(seq == next, "a log datagram numbered {} after {}: the machine's own gap or repeat", seq,
                   got.datagrams.last().map_or(-1, |d| i64::from(d.0)));
        got.datagrams.push((seq, p[HDR_LEN..].to_vec()));
    }
}

/// Record `n`, as it is logged: a tag the collector finds it by, its
/// length's worth of letters, a newline.
fn record(n: u32, len: usize) -> Vec<u8> {
    let mut r = format!("<rec-{:07}>", n).into_bytes();
    for i in 0..len {
        r.push(b'a' + ((n as usize + i) % 26) as u8);
    }
    r.push(b'\n');
    r
}

/// A record of the input's length, logged as the tracer logs a line.
fn log_one(r: &mut Input, n: &mut u32, logged: &mut Vec<(u32, Vec<u8>)>) {
    let len = match r.u8() % 8 {
        0 => r.pick(&[MAX_RECORD - 14, MAX_RECORD - 13, MAX_RECORD - 12, 600, 2000]),
        _ => r.below(120) as usize,
    };
    let text = record(*n, len);
    logged.push((*n, text.clone()));
    *n += 1;
    net::netconsole::NETCONSOLE.log(&text);
}

/// What the collector got of each record, in the order it got them: the
/// record's number, and whether what followed its tag is the record (cut
/// to a record's length) -- the stream checked as it is read.
fn records(got: &Collector, logged: &[(u32, Vec<u8>)]) -> Vec<u32> {
    let stream: Vec<u8> = got.datagrams.iter().flat_map(|d| d.1.iter().copied()).collect();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(i) = stream[at..].windows(TAG.len()).position(|w| w == TAG) {
        let start = at + i;
        let digits = &stream[start + TAG.len()..(start + TAG.len() + 7).min(stream.len())];
        let n: u32 = std::str::from_utf8(digits).ok().and_then(|s| s.parse().ok()).unwrap_or(u32::MAX);
        let Some((_, text)) = logged.iter().find(|l| l.0 == n) else {
            invariant!(false, "the collector got a record {:?} nobody logged",
                       String::from_utf8_lossy(&stream[start..(start + 20).min(stream.len())]));
            break;
        };
        let want = &text[..text.len().min(MAX_RECORD)];
        let have = &stream[start..(start + want.len()).min(stream.len())];
        invariant!(have == want, "record {} came as {:?}..., logged as {:?}...", n,
                   String::from_utf8_lossy(&have[..have.len().min(40)]),
                   String::from_utf8_lossy(&want[..want.len().min(40)]));
        found.push(n);
        at = start + want.len();
    }
    found
}

/// Everything the collector got so far, against what was logged.
fn judge(w: &World<Wire>, logged: &[(u32, Vec<u8>)], finished: bool) {
    let got = w.peers.got.borrow();
    let found = records(&got, logged);
    for pair in found.windows(2) {
        invariant!(pair[0] < pair[1], "the collector got record {} after record {}: {}", pair[1], pair[0],
                   if pair[0] == pair[1] { "twice" } else { "out of order" });
    }
    if finished {
        let st = net::netconsole::NETCONSOLE.stats();
        let missing: Vec<u32> = logged.iter().map(|l| l.0).filter(|k| !found.contains(k)).collect();
        invariant!(missing.len() <= st.dropped, "{} records never reached the collector (the first {:?}), the ring \
                   having dropped {} -- {} bytes still in it, {} datagrams sent, {} refused, next number {}",
                   missing.len(), &missing[..missing.len().min(5)], st.dropped, st.used, st.sent, st.tx_failed,
                   st.seq);
    }
}

pub fn netconsole(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    let has_ip = r.u8() < 200;
    nic.set_ip(if has_ip { ETH0_IP } else { 0 });
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);
    let to = r.pick(&[GW_IP, GW_IP, FAR, LATE]);
    let tail_kb = r.pick(&[0usize, 0, 1, 64, 2048]);
    machine::set_params(machine::Params { netconsole: Some((to, PORT, tail_kb)), ..machine::params() });

    let link = if r.u8() < 216 { Link::perfect() } else { Link::from_input(r) };
    let got = Rc::new(RefCell::new(Collector { to, datagrams: Vec::new() }));
    let mut w = World::new(link, Wire { lan: Lan::new(), got: got.clone() });
    let nc = &net::netconsole::NETCONSOLE;

    /* Lines from before the network: in the ring, and replayed from the
     * kernel log by setup. */
    let mut logged: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut n = 0u32;

    invariant!(nc.setup(), "netconsole would not set up");
    for _ in 0..r.below(4) {
        log_one(r, &mut n, &mut logged);
    }
    invariant!(nc.start(nic), "netconsole would not start");
    let mut running = true;

    while let Some(op) = r.op(10) {
        match op {
            0..=2 => log_one(r, &mut n, &mut logged),
            3 => {
                /* A burst: past what the ring holds, sometimes -- the ring
                 * full, the oldest going as the newest come. */
                if r.u8() < 64 {
                    for _ in 0..r.pick(&[2000u32, 2200, 4000]) {
                        let text = record(n, 480);
                        logged.push((n, text.clone()));
                        n += 1;
                        nc.log(&text);
                    }
                } else {
                    for _ in 0..r.pick(&[10u32, 100, 1000, 2500]) {
                        log_one(r, &mut n, &mut logged);
                    }
                }
            }
            4 => {
                /* Lines from a task of the machine's, while the drain
                 * sends. */
                let first = n;
                let count = r.below(200) as u32;
                n += count;
                let texts: Vec<(u32, Vec<u8>)> = (first..first + count).map(|k| (k, record(k, 40))).collect();
                logged.extend(texts.iter().cloned());
                let app = App::spawn("logger", machine::next_cpu(), move || {
                    for (_, t) in &texts {
                        nc.log(t);
                        kcore::task::yield_to_runnable();
                    }
                });
                invariant!(w.wait_app(&app, 60 * SEC), "a logger task never finished");
            }
            5 => {
                /* The address comes, goes, and comes back. */
                nic.set_ip(if nic.ip() == 0 || r.u8() < 128 { ETH0_IP } else { 0 });
            }
            6 => {
                /* The late collector turns up; or the NIC stalls a while. */
                if r.bool() {
                    w.peers.lan.add(LATE, LATE_MAC);
                } else {
                    nic::stall(0, true);
                    w.run_for(r.below(2000) * MS);
                    nic::stall(0, false);
                }
            }
            7 if running && r.u8() < 64 => {
                let app = App::spawn("nc-stop", 1, || nc.stop());
                invariant!(w.wait_app(&app, 30 * SEC), "netconsole took half a minute to stop");
                running = false;
            }
            8 if !running => {
                invariant!(nc.start(nic), "netconsole would not start again");
                running = true;
            }
            _ => w.run_for(r.below(2000) * MS),
        }
        judge(&w, &logged, false);
    }

    /* The collector there, the link good, the address on: everything logged
     * reaches it but what the ring dropped. */
    w.net.link = Link::perfect();
    w.peers.lan.add(LATE, LATE_MAC);
    nic.set_ip(ETH0_IP);
    nic::stall(0, false);
    if !running {
        invariant!(nc.start(nic), "netconsole would not start again");
    }
    w.run_for(60 * SEC);
    judge(&w, &logged, true);
    let app = App::spawn("nc-stop", 1, || nc.stop());
    invariant!(w.wait_app(&app, 30 * SEC), "netconsole took half a minute to stop");
    w.run_for(10 * SEC);
    super::audit();
}
