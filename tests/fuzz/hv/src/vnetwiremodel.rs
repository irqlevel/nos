//! vnet's tunnel's datagram (`common/net/vnetwire`, plans/vnet.md, "The
//! tunnel") against a model of its own: a key's window of counters held to
//! a set of every counter taken and the newest; a datagram sealed and
//! opened, its packet back as it went; any byte of it changed -- header,
//! sealed packet, tag -- or another key, refused as forged; a counter sent
//! twice refused as replayed; the messages keys go in, each back as it went,
//! and anything else refused. The cipher itself is RustCrypto's, held to its
//! RFC by its own tests: what is held here is what vnetwire makes of it.

use std::collections::BTreeSet;

use vnetwire::{Header, Kind, Message, OpenError, Window, HEADER, KEY, MESSAGE_MAX, TAG, WINDOW};

use crate::targets::Input;

/// The window as the model has it: every counter taken, the newest.
struct Seen {
    taken: BTreeSet<u64>,
    newest: Option<u64>,
}

impl Seen {
    /// Fresh: past the newest, or within WINDOW of it and not taken.
    fn fresh(&self, c: u64) -> bool {
        match self.newest {
            None => true,
            Some(top) if c > top => true,
            Some(top) => top - c < WINDOW && !self.taken.contains(&c),
        }
    }

    fn take(&mut self, c: u64) {
        if !self.fresh(c) {
            return;
        }
        self.taken.insert(c);
        if self.newest.is_none_or(|t| c > t) {
            self.newest = Some(c);
        }
        /* What is past the window is forgotten: it is never fresh again */
        let top = self.newest.unwrap_or(0);
        while self.taken.first().is_some_and(|&f| top - f >= WINDOW) {
            self.taken.pop_first();
        }
    }
}

const KINDS: [Kind; 5] = [Kind::PortToPort, Kind::PortToGateway, Kind::GatewayToPort, Kind::Echo, Kind::EchoAnswer];

pub fn vnetwire_target(r: &mut Input) {
    let mut key = [0u8; KEY];
    for b in key.iter_mut() {
        *b = r.u8();
    }
    let mut other = key;
    other[r.below(KEY as u64) as usize] ^= 1 << r.below(8);
    let mut window = Window::new();
    let mut seen = Seen { taken: BTreeSet::new(), newest: None };
    let mut counter: u64 = r.pick(&[0, 1, 2047, 2048, u64::MAX - 3000]);
    let mut sent: Vec<(Vec<u8>, Header, Vec<u8>)> = Vec::new();

    while let Some(op) = r.op(8) {
        match op {
            0 | 1 => {
                /* A datagram sealed at the next counter, or one ahead */
                counter = counter.saturating_add(if r.u8() < 32 { 1 + r.below(3000) } else { 1 });
                let packet: Vec<u8> = (0..r.below(200)).map(|_| r.u8()).collect();
                let header = Header { kind: r.pick(&KINDS), network: r.u32() & 0xFF_FFFF, key: r.u32(), counter };
                let mut out = vec![0u8; HEADER + packet.len() + TAG + r.below(4) as usize];
                let n = vnetwire::seal(&key, &header, &packet, &mut out).expect("room for it");
                invariant!(n == HEADER + packet.len() + TAG, "a datagram of {} bytes for a packet of {}", n, packet.len());
                invariant!(Header::read(&out[..n]) == Some(header), "a header not read back as written");
                /* No nonce twice under a key: the same packet at another
                 * counter enciphered to other bytes -- the tag differs
                 * anyway, the header being in it; of 16 bytes or more, two
                 * keystreams alike by chance are no matter */
                let mut again = vec![0u8; n];
                let next = Header { counter: counter.wrapping_add(1 + r.below(4)), ..header };
                let m = vnetwire::seal(&key, &next, &packet, &mut again).expect("room for it");
                let body = HEADER..HEADER + packet.len();
                invariant!(m == n && (packet.len() < 16 || again[body.clone()] != out[body]),
                           "a packet of {} bytes enciphered alike at counters {} and {}", packet.len(), counter,
                           next.counter);
                sent.push((out[..n].to_vec(), header, packet));
            }
            2 => {
                /* One sent opened -- and held to the window as hv holds it:
                 * fresh before, taken after */
                if sent.is_empty() {
                    continue;
                }
                let (d, header, packet) = sent[r.below(sent.len() as u64) as usize].clone();
                let fresh = window.fresh(header.counter);
                invariant!(fresh == seen.fresh(header.counter), "counter {} fresh {}, the model's {}", header.counter,
                           fresh, seen.fresh(header.counter));
                let mut buf = d.clone();
                match vnetwire::open(&key, &mut buf) {
                    Ok((h, p)) => {
                        invariant!(h == header && p == packet.as_slice(), "a datagram opened to another packet");
                    }
                    Err(e) => panic!("invariant: a datagram sealed with the key not opened: {:?}", e),
                }
                if fresh {
                    window.take(header.counter);
                    seen.take(header.counter);
                }
                invariant!(!window.fresh(header.counter), "counter {} fresh after it was taken", header.counter);
                invariant!(window.newest() == seen.newest, "newest {:?}, the model's {:?}", window.newest(), seen.newest);
            }
            3 => {
                /* Any byte changed, or another key: forged */
                if sent.is_empty() {
                    continue;
                }
                let (d, _, _) = sent[r.below(sent.len() as u64) as usize].clone();
                let mut buf = d.clone();
                if r.bool() {
                    let at = r.below(buf.len() as u64) as usize;
                    buf[at] ^= 1 << r.below(8);
                    let res = vnetwire::open(&key, &mut buf);
                    /* A kind changed to none there is reads as no header */
                    invariant!(matches!(res, Err(OpenError::Forged) | Err(OpenError::Malformed)),
                               "a datagram changed at byte {} opened: {:?}", at, res.map(|(h, _)| h));
                } else {
                    invariant!(vnetwire::open(&other, &mut buf) == Err(OpenError::Forged), "opened with another key");
                }
            }
            4 => {
                /* Too short for a header and a tag */
                let mut buf: Vec<u8> = (0..r.below((HEADER + TAG) as u64)).map(|_| r.u8()).collect();
                invariant!(vnetwire::open(&key, &mut buf) == Err(OpenError::Malformed), "a scrap opened");
            }
            5 => {
                /* Counters around the window's edges, taken by the window
                 * and the model alike */
                let base = seen.newest.unwrap_or(0);
                let c = match r.u8() % 4 {
                    0 => base.saturating_sub(WINDOW),
                    1 => base.saturating_sub(WINDOW - 1),
                    2 => base.saturating_add(r.below(WINDOW * 2)),
                    _ => base.saturating_sub(r.below(WINDOW + 10)),
                };
                invariant!(window.fresh(c) == seen.fresh(c), "counter {} near {}: fresh {}, the model's {}", c, base,
                           window.fresh(c), seen.fresh(c));
                window.take(c);
                seen.take(c);
            }
            6 => {
                /* A message, back as it went */
                let m = match r.u8() % 3 {
                    0 => Message::Hello { instance: r.u64() },
                    1 => Message::Gone { instance: r.u64() },
                    _ => {
                        let mut k = [0u8; KEY];
                        for b in k.iter_mut() {
                            *b = r.u8();
                        }
                        Message::Key { made_by: r.u64(), made_for: r.u64(), number: r.u32(), key: k }
                    }
                };
                let mut out = [0u8; MESSAGE_MAX];
                let n = m.encode(&mut out);
                invariant!(Message::decode(&out[..n]) == Some(m.clone()), "a message not read back: {:?}", m);
                /* One byte more or less is no message */
                invariant!(Message::decode(&out[..n - 1]).is_none(), "a message cut short read");
                let mut longer = out[..n].to_vec();
                longer.push(r.u8());
                invariant!(Message::decode(&longer).is_none(), "a message with a byte past its end read");
            }
            _ => {
                /* Bytes that are no message */
                let bytes: Vec<u8> = (0..r.below(64)).map(|_| r.u8()).collect();
                if let Some(m) = Message::decode(&bytes) {
                    let mut out = [0u8; MESSAGE_MAX];
                    let n = m.encode(&mut out);
                    invariant!(out[..n] == bytes[..], "bytes read as a message that is not them");
                }
            }
        }
    }
}
