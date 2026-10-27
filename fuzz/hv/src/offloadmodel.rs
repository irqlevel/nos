//! The `offload` target: a guest NIC's transmit offloads
//! (`hv/src/devices/offload.rs`) held to a model of their own, written from
//! virtio's and the RFCs' words and not from that code -- a send of TCP over
//! IPv4 the driver leaves to be cut, a frame whose checksum it leaves to be
//! completed, and bytes that are neither, each well made or not. What comes
//! out must be what the model says, byte for byte, or the same refusal; and
//! on its own terms: every frame at most 1514 bytes, every IPv4 and TCP
//! checksum in it right, the payloads in order adding up to the send's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::devices::offload::{self, Dropped, Offloads, Send, TxHeader};
use crate::targets::Input;

/// What the sends came to, for `HV_FUZZ_STATS`: sends cut and the frames
/// they made, frames whose checksum was completed, and each refusal.
static CUT: AtomicU64 = AtomicU64::new(0);
static CUT_FRAMES: AtomicU64 = AtomicU64::new(0);
static COMPLETED: AtomicU64 = AtomicU64::new(0);
static REFUSED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

pub fn report() {
    println!("  offload: {} sends cut into {} frames, {} checksums completed", CUT.load(Ordering::Relaxed),
             CUT_FRAMES.load(Ordering::Relaxed), COMPLETED.load(Ordering::Relaxed));
    for (why, n) in REFUSED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        println!("  offload: refused, {}: {}", why, n);
    }
}

fn refused(why: Dropped) {
    let mut r = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    let key = format!("{:?}", why);
    match r.iter_mut().find(|(k, _)| *k == key) {
        Some((_, n)) => *n += 1,
        None => r.push((key, 1)),
    }
}

const ETH: usize = 14;
const MAX_FRAME: usize = 1514;

/// The one's-complement sum of `data` as big-endian words, an odd last byte
/// padded with a zero (RFC 1071), folded -- the model's own.
fn fold_sum(data: &[u8], mut sum: u64) -> u16 {
    for pair in data.chunks(2) {
        let hi = u64::from(pair[0]) << 8;
        let lo = pair.get(1).map_or(0, |b| u64::from(*b));
        sum += hi | lo;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum as u16
}

fn be16(b: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([b[at], b[at + 1]])
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_be_bytes());
}

/// The pseudo-header's sum for TCP or UDP over IPv4, `len` its length.
fn pseudo(src: &[u8], dst: &[u8], proto: u8, len: usize) -> u64 {
    u64::from(be16(src, 0)) + u64::from(be16(src, 2)) + u64::from(be16(dst, 0)) + u64::from(be16(dst, 2))
        + u64::from(proto) + len as u64
}

/// Where the checksum of the UDP datagram `f` carries is, if it carries
/// one: over IPv4 past its header, or over IPv6 past its extension headers
/// -- the model's own walk of them (RFC 8200, 4): hop-by-hop, routing and
/// destination options eight bytes and eight more a unit, authentication
/// eight and four more a unit, a first fragment's eight; eight of them at
/// most.
fn udp_sum_place(f: &[u8]) -> Option<usize> {
    if f.len() < ETH {
        return None;
    }
    match be16(f, 12) {
        0x0800 if f.len() >= ETH + 20 => (f[ETH + 9] == 17).then(|| ETH + usize::from(f[ETH] & 0xF) * 4 + 6),
        0x86DD if f.len() >= ETH + 40 => {
            let (mut next, mut at) = (f[ETH + 6], ETH + 40);
            for _ in 0..8 {
                let len = match next {
                    17 => return Some(at + 6),
                    0 | 43 | 60 => 8 * (1 + usize::from(*f.get(at + 1)?)),
                    51 => 4 * (2 + usize::from(*f.get(at + 1)?)),
                    44 if f.len() >= at + 8 && be16(f, at + 2) >> 3 == 0 => 8,
                    _ => return None,
                };
                if at + len > f.len() {
                    return None;
                }
                next = f[at];
                at += len;
            }
            None
        }
        _ => None,
    }
}

/// A checksum left at `start + offset` finished as the model has it: the
/// sum from `start` on, folded and complemented, all ones for UDP's 0.
pub fn finish(f: &mut [u8], start: usize, offset: usize) {
    let at = start + offset;
    if at + 2 > f.len() {
        return;
    }
    let mut sum = !fold_sum(&f[start..], 0);
    if sum == 0 && udp_sum_place(f) == Some(at) {
        sum = 0xFFFF;
    }
    put16(f, at, sum);
}

/// The checksum field at `at` set so that the sum from `start` comes to all
/// ones, and the checksum finished to 0 -- which UDP's goes as all ones.
pub fn sum_to_zero(f: &mut [u8], start: usize, at: usize) {
    if start > at || at + 2 > f.len() || (at - start) % 2 != 0 {
        return;
    }
    put16(f, at, 0);
    let rest = fold_sum(&f[start..], 0);
    put16(f, at, 0xFFFF - rest);
}

/// What the model says a send comes to: the frames that go out, or why none.
fn model(hdr: &TxHeader, frame: &[u8], offloads: Offloads) -> Result<Vec<Vec<u8>>, Dropped> {
    if hdr.gso_type & 0x80 != 0 {
        return Err(Dropped::NotOffered);
    }
    match hdr.gso_type {
        0 => {
            let mut f = frame.to_vec();
            if hdr.flags & 1 != 0 {
                if !offloads.csum {
                    return Err(Dropped::NotOffered);
                }
                let start = usize::from(hdr.csum_start);
                let at = start + usize::from(hdr.csum_offset);
                if at + 2 > f.len() {
                    return Err(Dropped::ChecksumPlace);
                }
                let mut sum = !fold_sum(&f[start..], 0);
                /* UDP, the place its checksum's: 0 goes as 0xFFFF. */
                if sum == 0 && udp_sum_place(&f) == Some(at) {
                    sum = 0xFFFF;
                }
                put16(&mut f, at, sum);
            }
            if f.len() > MAX_FRAME {
                return Err(Dropped::TooLong);
            }
            Ok(vec![f])
        }
        1 if offloads.tso4 => cut(frame, usize::from(hdr.gso_size)),
        _ => Err(Dropped::NotOffered),
    }
}

/// A TCP send over IPv4 cut as the model has it -- for the switch's model
/// too, which cuts what it does not carry whole.
pub fn cut(frame: &[u8], mss: usize) -> Result<Vec<Vec<u8>>, Dropped> {
    if frame.len() < ETH + 20 || be16(frame, 12) != 0x0800 {
        return Err(Dropped::NotTcp4);
    }
    let ip = &frame[ETH..];
    if ip[0] >> 4 != 4 || ip[9] != 6 || be16(ip, 6) & 0x3FFF != 0 {
        return Err(Dropped::NotTcp4);
    }
    let ihl = usize::from(ip[0] & 0xF) * 4;
    if ihl < 20 || ip.len() < ihl + 20 {
        return Err(Dropped::Headers);
    }
    let thl = usize::from(ip[ihl + 12] >> 4) * 4;
    if thl < 20 || ip.len() < ihl + thl {
        return Err(Dropped::Headers);
    }
    let hl = ETH + ihl + thl;
    if mss < offload::MIN_SEGMENT || hl + mss > MAX_FRAME {
        return Err(Dropped::SegmentSize);
    }
    let data = &frame[hl..];
    let pieces: Vec<&[u8]> = if data.is_empty() { vec![&data[..0]] } else { data.chunks(mss).collect() };
    let n = pieces.len();
    let id = be16(ip, 4);
    let seq = u32::from_be_bytes([ip[ihl + 4], ip[ihl + 5], ip[ihl + 6], ip[ihl + 7]]);
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let mut f = frame[..hl].to_vec();
        f.extend_from_slice(piece);
        put16(&mut f, ETH + 2, (ihl + thl + piece.len()) as u16);
        put16(&mut f, ETH + 4, id.wrapping_add(i as u16));
        put16(&mut f, ETH + 10, 0);
        let c = !fold_sum(&f[ETH..ETH + ihl], 0);
        put16(&mut f, ETH + 10, c);
        let t = ETH + ihl;
        let s = seq.wrapping_add((i * mss) as u32);
        f[t + 4..t + 8].copy_from_slice(&s.to_be_bytes());
        let mut flags = f[t + 13];
        if i != n - 1 {
            flags &= !0x09; /* FIN, PSH */
        }
        if i != 0 {
            flags &= !0x80; /* CWR */
        }
        f[t + 13] = flags;
        put16(&mut f, t + 16, 0);
        let len = f.len() - t;
        let c = !fold_sum(&f[t..], pseudo(&f[ETH + 12..ETH + 16], &f[ETH + 16..ETH + 20], 6, len));
        put16(&mut f, t + 16, c);
        out.push(f);
    }
    Ok(out)
}

/// A TCP or UDP frame over IPv6 as a driver builds one: the extension
/// headers `chain` names between its header and the transport's --
/// hop-by-hop, routing and destination options of eight bytes, a first
/// fragment's eight, authentication's twelve -- and `payload` bytes after
/// it; and where the transport's header is.
pub fn build6(r: &mut Input, proto: u8, chain: &[u8], payload: usize) -> (Vec<u8>, usize) {
    let l4 = if proto == 17 { 8 } else { 20 };
    let ext: usize = chain.iter().map(|&h| if h == 51 { 12 } else { 8 }).sum();
    let mut f = vec![0u8; ETH + 40 + ext + l4 + payload];
    let mut x = u64::from(r.u32()) | 1;
    for b in f.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    put16(&mut f, 12, 0x86DD);
    f[ETH..ETH + 4].copy_from_slice(&[0x60, 0, 0, 0]);
    put16(&mut f, ETH + 4, (ext + l4 + payload).min(0xFFFF) as u16);
    f[ETH + 6] = chain.first().copied().unwrap_or(proto);
    f[ETH + 7] = 64;
    let mut at = ETH + 40;
    for (i, &h) in chain.iter().enumerate() {
        f[at] = chain.get(i + 1).copied().unwrap_or(proto);
        f[at + 1] = u8::from(h == 51);
        if h == 44 {
            /* The first fragment, more to come */
            f[at + 1] = 0;
            put16(&mut f, at + 2, 1);
        }
        at += if h == 51 { 12 } else { 8 };
    }
    if proto == 17 {
        put16(&mut f, at + 4, (l4 + payload).min(0xFFFF) as u16);
    } else {
        f[at + 12] = 0x50;
        f[at + 13] = 0x18;
    }
    (f, at)
}

/// A TCP or UDP frame over IPv4 as a driver builds one: headers of the
/// lengths given, `payload` bytes after them, its checksum field holding the
/// pseudo-header's sum when the device is to complete it.
#[allow(clippy::too_many_arguments)]
pub fn build(r: &mut Input, proto: u8, ihl: usize, thl: usize, payload: usize, ethertype: u16, frag: u16,
             flags: u8, partial: bool) -> Vec<u8> {
    let l4 = if proto == 17 { 8 } else { thl };
    let mut f = vec![0u8; ETH + ihl + l4 + payload];
    /* Its bytes from a generator the input seeds, not from the input: a
     * send of 64 KiB would take all of it, and none were left for the next. */
    let mut x = u64::from(r.u32()) | 1;
    for b in f.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    put16(&mut f, 12, ethertype);
    f[ETH] = 0x40 | ((ihl / 4) as u8 & 0xF);
    let total = (ihl + l4 + payload).min(0xFFFF) as u16;
    put16(&mut f, ETH + 2, total);
    put16(&mut f, ETH + 6, frag);
    f[ETH + 9] = proto;
    let t = ETH + ihl;
    if proto == 17 {
        put16(&mut f, t + 4, (l4 + payload) as u16);
    } else {
        f[t + 12] = ((thl / 4) as u8) << 4;
        f[t + 13] = flags;
    }
    /* No checksum to leave where a header made too short for one has no
     * room for it -- a send the device must refuse anyway. */
    let at = t + if proto == 17 { 6 } else { 16 };
    if partial && at + 2 <= f.len() {
        let len = f.len() - t;
        let p = pseudo(&f[ETH + 12..ETH + 16], &f[ETH + 16..ETH + 20], proto, len);
        let mut s = p;
        while s > 0xFFFF {
            s = (s & 0xFFFF) + (s >> 16);
        }
        put16(&mut f, at, s as u16);
    }
    f
}

/// What came out, held on its own terms: frames of at most 1514 bytes, the
/// IPv4 and TCP checksums of a cut one right, and a cut send's payloads in
/// order adding up to its own.
fn check_frames(sent: &[u8], hdr: &TxHeader, frames: &[Vec<u8>]) {
    for f in frames {
        invariant!(f.len() <= MAX_FRAME, "a frame of {} bytes out of a send", f.len());
    }
    if hdr.gso_type != 1 || frames.is_empty() {
        return;
    }
    let ihl = usize::from(sent[ETH] & 0xF) * 4;
    let thl = usize::from(sent[ETH + ihl + 12] >> 4) * 4;
    let hl = ETH + ihl + thl;
    let mut joined = Vec::new();
    for f in frames {
        invariant!(fold_sum(&f[ETH..ETH + ihl], 0) == 0xFFFF, "a cut frame's IPv4 checksum wrong");
        let t = ETH + ihl;
        let sum = fold_sum(&f[t..], pseudo(&f[ETH + 12..ETH + 16], &f[ETH + 16..ETH + 20], 6, f.len() - t));
        invariant!(sum == 0xFFFF, "a cut frame's TCP checksum wrong");
        invariant!(usize::from(be16(f, ETH + 2)) == f.len() - ETH, "a cut frame's IPv4 length is not its own");
        joined.extend_from_slice(&f[hl..]);
    }
    invariant!(joined == sent[hl..], "the cut frames' payloads are not the send's, in order");
}

fn one(r: &mut Input, offloads: Offloads, seg: &mut [u8; offload::MAX_FRAME]) {
    let kind = r.u8();
    let (hdr, frame) = if kind < 128 {
        /* TCP to be cut: well made, mostly. */
        let ihl = if r.u8() < 220 { 20 } else { r.pick(&[16usize, 24, 40, 60]) };
        let thl = if r.u8() < 200 { r.pick(&[20usize, 32]) } else { r.pick(&[16usize, 40, 60]) };
        let payload = if r.bool() {
            r.pick(&[0usize, 1, 255, 256, 1448, 1449, 2896, 30_000, 64_000])
        } else {
            r.below(65_000) as usize
        };
        let payload = payload.min(offload::MAX_SEND - ETH - ihl - thl);
        let ethertype = if r.u8() < 248 { 0x0800 } else { 0x86DD };
        let frag = r.pick(&[0u16, 0x4000, 0x4000, 0x2000, 0x0001]);
        let proto = if r.u8() < 248 { 6 } else { 17 };
        let flags = r.u8();
        let frame = build(r, proto, ihl, thl, payload, ethertype, frag, flags, true);
        let any = r.u16();
        let mss = if r.bool() { r.pick(&[1448u16, 1460, 536, 256]) } else { r.pick(&[0u16, 1, 255, 1461, 1500, 4096, any]) };
        let gso = if r.u8() < 240 { 1 } else { r.pick(&[0x81u8, 3, 4, 0x80]) };
        let hdr = TxHeader { flags: 1, gso_type: gso, hdr_len: (ETH + ihl + thl) as u16, gso_size: mss,
                             csum_start: (ETH + ihl) as u16, csum_offset: 16 };
        (hdr, frame)
    } else if kind < 220 {
        /* A frame whose checksum is left to the device: TCP or UDP, over
         * IPv4 or over IPv6 and its extension headers, its place given
         * right, mostly -- and now and then its sum made to come to 0,
         * which UDP's goes as all ones. */
        let proto = r.pick(&[6u8, 17]);
        let payload = if r.u8() < 240 { r.below(1460) as usize } else { 1460 + r.below(200) as usize };
        let (mut frame, l4) = if r.u8() < 64 {
            let chain: &[u8] = r.pick(&[&[][..], &[0], &[60], &[44], &[0, 44], &[51], &[43, 60]]);
            build6(r, proto, chain, payload)
        } else {
            let ihl = if r.u8() < 230 { 20 } else { 24 };
            (build(r, proto, ihl, 20, payload, 0x0800, 0x4000, 0x18, true), ETH + ihl)
        };
        let (start, offset) = if r.u8() < 230 {
            (l4 as u16, if proto == 17 { 6 } else { 16 })
        } else {
            (r.u16(), r.u16())
        };
        if r.u8() < 32 {
            sum_to_zero(&mut frame, usize::from(start), usize::from(start) + usize::from(offset));
        }
        let hdr = TxHeader { flags: if r.u8() < 240 { 1 } else { 0 }, gso_type: 0, hdr_len: 0, gso_size: 0,
                             csum_start: start, csum_offset: offset };
        (hdr, frame)
    } else {
        /* Anything. */
        let len = r.below(1600) as usize;
        let frame: Vec<u8> = (0..len).map(|_| r.u8()).collect();
        let hdr = TxHeader { flags: r.u8(), gso_type: r.u8(), hdr_len: r.u16(), gso_size: r.u16(),
                             csum_start: r.u16(), csum_offset: r.u16() };
        (hdr, frame)
    };

    let want = model(&hdr, &frame, offloads);
    let mut got_frames: Vec<Vec<u8>> = Vec::new();
    let mut work = frame.clone();
    let got = match offload::transmit(&hdr, &mut work, offloads) {
        Ok(Send::Frame(f, partial)) => {
            /* Finished as a backend finishes it for a guest that takes none
             * partial -- the place it was given, the one the header said. */
            if let Some(p) = partial {
                let h = p.header();
                invariant!(h.csum_start == hdr.csum_start && h.csum_offset == hdr.csum_offset,
                           "a checksum to finish at {}+{}, its header said {}+{}", h.csum_start, h.csum_offset,
                           hdr.csum_start, hdr.csum_offset);
                p.complete(f);
            }
            got_frames.push(f.to_vec());
            Ok(1)
        }
        Ok(Send::Tcp4 { frame: f, layout }) => {
            invariant!(layout.mss() == usize::from(hdr.gso_size), "a send to cut by {}, its header said {}",
                       layout.mss(), hdr.gso_size);
            Ok(offload::segment_tcp4(f, layout, seg, &mut |piece: &[u8]| got_frames.push(piece.to_vec())))
        }
        Err(e) => Err(e),
    };
    match (&want, got) {
        (Ok(frames), Ok(n)) => {
            if hdr.gso_type == 1 {
                CUT.fetch_add(1, Ordering::Relaxed);
                CUT_FRAMES.fetch_add(n as u64, Ordering::Relaxed);
            } else if hdr.flags & 1 != 0 {
                COMPLETED.fetch_add(1, Ordering::Relaxed);
            }
            invariant!(n == frames.len() && got_frames.len() == n,
                       "{} frames out of a send, {} said, the model {}", got_frames.len(), n, frames.len());
            for (i, (g, w)) in got_frames.iter().zip(frames.iter()).enumerate() {
                if g != w {
                    let at = g.iter().zip(w.iter()).position(|(a, b)| a != b).unwrap_or(g.len().min(w.len()));
                    panic!("invariant: frame {} of a send ({:?}) differs from the model's at byte {} ({} and {} bytes)",
                           i, hdr, at, g.len(), w.len());
                }
            }
            check_frames(&frame, &hdr, &got_frames);
        }
        (Err(w), Err(g)) => {
            refused(g);
            invariant!(*w == g, "a send ({:?}, {} bytes) dropped as {:?}, the model says {:?}", hdr, frame.len(), g, w);
            invariant!(got_frames.is_empty(), "{} frames out of a send dropped as {:?}", got_frames.len(), g);
        }
        (w, g) => panic!("invariant: a send ({:?}, {} bytes) came to {:?}, the model says {:?}",
                         hdr, frame.len(), g, w.as_ref().map(|f| f.len())),
    }
}

pub fn offload_target(r: &mut Input) {
    let offloads = Offloads { csum: r.u8() < 232, tso4: r.u8() < 232 };
    let mut seg = [0u8; offload::MAX_FRAME];
    while r.op(1).is_some() {
        one(r, offloads, &mut seg);
    }
}
