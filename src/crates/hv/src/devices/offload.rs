//! A guest NIC's transmit offloads, as virtio-net offers them: a checksum
//! the driver leaves for the device to complete, and TCP over IPv4 the
//! driver leaves for the device to cut into frames -- a send of up to 64
//! KiB and one notify, where it would take forty-odd frames and a notify,
//! an exit, each.
//!
//! Every frame on the switch, and so everything a guest, nos's own stack or
//! NAT is handed, stays a whole Ethernet frame with its checksums right: the
//! cutting and the completing are done here, before the frames leave the
//! device. What the driver says about either -- where a checksum goes, how
//! long a segment is, where the headers end -- is the guest's to say, so
//! every offset and length in it is checked against the frame before it is
//! used, and a send that does not hold up goes nowhere, counted.

use netwire::{self as wire, ETH_HDR_LEN, ETH_TYPE_IP, IP_PROTO_TCP, IP_PROTO_UDP};

/// The longest frame on the wire: Ethernet's, less the FCS.
pub const MAX_FRAME: usize = 1514;
/// The longest send a driver that may leave segmentation to the device
/// makes: as much IPv4 as its 16-bit length can say, the Ethernet header in
/// front of it.
pub const MAX_SEND: usize = 65_535 + ETH_HDR_LEN;
/// The least segment size a send may ask to be cut by: a send is then 256
/// frames at most, a ring's worth, where a byte a segment would make one
/// notify 65,000 frames on the switch -- free to the guest that asks, and
/// costing the host's CPU and every inbox on the way. TCP on the switch's
/// MTU cuts at 1,448.
pub const MIN_SEGMENT: usize = 256;

/// The header's flag: the checksum is the device's to complete.
pub const NEEDS_CSUM: u8 = 1;
/// The header's kinds of segmentation: none, TCP over IPv4, and the flag
/// that says the segments may carry ECN's CWR -- not offered.
pub const GSO_NONE: u8 = 0;
pub const GSO_TCPV4: u8 = 1;
pub const GSO_ECN: u8 = 0x80;

/// TCP's flags a segmentation hands out: FIN and PSH to the last segment
/// only, CWR to the first only.
const TCP_FIN: u8 = 0x01;
const TCP_PSH: u8 = 0x08;
const TCP_CWR: u8 = 0x80;

/// The fields of IPv4's header and TCP's a segmentation rewrites, by offset.
const IP_TOTAL_LEN: usize = 2;
const IP_ID: usize = 4;
const IP_FRAG: usize = 6;
const IP_PROTO: usize = 9;
const IP_CHECKSUM: usize = 10;
const IP_SRC: usize = 12;
const IP_DST: usize = 16;
const IP_MIN: usize = 20;
/// IPv4's flags and fragment offset, but for Don't Fragment: a fragment is
/// none of TCP's to cut.
const IP_FRAG_MASK: u16 = 0x3FFF;
const TCP_SEQ: usize = 4;
const TCP_OFFSET: usize = 12;
const TCP_FLAGS: usize = 13;
const TCP_CHECKSUM: usize = 16;
const TCP_MIN: usize = 20;
/// UDP's checksum: one computed as 0 is sent as all ones, since 0 says
/// there is none (RFC 768).
const UDP_CHECKSUM: usize = 6;

/// The internet checksum's sum (RFC 1071) over `data`, on top of `acc`:
/// eight bytes at a time, as big-endian words with the carry wrapped round,
/// which folds to the same as the 16-bit words do -- every frame a driver
/// leaves its checksum to is summed here, on the guest's own CPU, and two
/// bytes at a time made a frame of small writes 3-5% slower to send than the
/// guest's own checksum, which it takes as it copies.
fn sum(data: &[u8], mut acc: u64) -> u64 {
    let mut words = data.chunks_exact(8);
    for w in &mut words {
        let v = u64::from_be_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]);
        let (s, carry) = acc.overflowing_add(v);
        acc = s + u64::from(carry);
    }
    let rest = words.remainder();
    let mut pairs = rest.chunks_exact(2);
    for p in &mut pairs {
        let (s, carry) = acc.overflowing_add(u64::from(u16::from_be_bytes([p[0], p[1]])));
        acc = s + u64::from(carry);
    }
    if let [last] = pairs.remainder() {
        let (s, carry) = acc.overflowing_add(u64::from(*last) << 8);
        acc = s + u64::from(carry);
    }
    acc
}

/// `acc` folded to 16 bits and complemented: the checksum.
fn checksum(acc: u64) -> u16 {
    let mut s = (acc & 0xFFFF_FFFF) + (acc >> 32);
    while s > 0xFFFF {
        s = (s & 0xFFFF) + (s >> 16);
    }
    !(s as u16)
}

/// The pseudo-header of TCP or UDP over IPv4 (`len` its length), summed.
fn pseudo(src: u32, dst: u32, protocol: u8, len: usize) -> u64 {
    u64::from(src >> 16) + u64::from(src & 0xFFFF) + u64::from(dst >> 16) + u64::from(dst & 0xFFFF)
        + u64::from(protocol) + len as u64
}

/// The header in front of every frame the driver sends (virtio's
/// `virtio_net_hdr`, legacy's: little-endian, ten bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxHeader {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
}

impl TxHeader {
    pub const LEN: usize = 10;

    pub fn parse(b: &[u8; TxHeader::LEN]) -> TxHeader {
        TxHeader {
            flags: b[0],
            gso_type: b[1],
            hdr_len: u16::from_le_bytes([b[2], b[3]]),
            gso_size: u16::from_le_bytes([b[4], b[5]]),
            csum_start: u16::from_le_bytes([b[6], b[7]]),
            csum_offset: u16::from_le_bytes([b[8], b[9]]),
        }
    }

    /// As it lies in front of a frame the device hands the driver.
    pub fn bytes(&self) -> [u8; TxHeader::LEN] {
        let mut b = [0u8; TxHeader::LEN];
        b[0] = self.flags;
        b[1] = self.gso_type;
        b[2..4].copy_from_slice(&self.hdr_len.to_le_bytes());
        b[4..6].copy_from_slice(&self.gso_size.to_le_bytes());
        b[6..8].copy_from_slice(&self.csum_start.to_le_bytes());
        b[8..10].copy_from_slice(&self.csum_offset.to_le_bytes());
        b
    }
}

/// What the driver may leave to the device, by the features it accepted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Offloads {
    pub csum: bool,
    pub tso4: bool,
}

/// Why a send went nowhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// Something the driver was not offered, or did not accept: a checksum
    /// to complete, a segmentation, of a kind or with ECN.
    NotOffered,
    /// A checksum's place outside the frame.
    ChecksumPlace,
    /// Longer than a frame, and not to be cut.
    TooLong,
    /// To be cut, and not TCP over IPv4: another protocol, a fragment.
    NotTcp4,
    /// Headers that do not fit the frame, or say lengths below their own.
    Headers,
    /// A segment size under `MIN_SEGMENT`, or one that would not fit a
    /// frame.
    SegmentSize,
}

/// What a send comes to once its header is followed: a frame, its checksum
/// left to finish where the header asked (`Partial`) -- for the backend to
/// hand on so to a guest that takes checksums partial, and to finish for
/// anyone else; or TCP over IPv4 that held up (`check_tcp4`), for the
/// backend to carry whole to a guest that takes it so, or cut
/// (`segment_tcp4`).
pub enum Send<'a> {
    Frame(&'a mut [u8], Option<Partial>),
    Tcp4 { frame: &'a [u8], layout: Tcp4 },
}

/// What a frame for a guest is, by the header it goes in with: whole, its
/// checksums right; its checksum left to finish; or a send of TCP carried
/// whole, to be cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rx {
    Done,
    Partial(Partial),
    Whole(Tcp4),
}

impl Rx {
    pub fn header(&self) -> TxHeader {
        match self {
            Rx::Done => TxHeader::default(),
            Rx::Partial(p) => p.header(),
            Rx::Whole(t) => t.header(),
        }
    }
}

/// What a guest's driver takes in what it is handed: checksums left to
/// finish (`GUEST_CSUM`), and sends of TCP whole (`GUEST_TSO4`, which needs
/// the other).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Takes {
    pub partial: bool,
    pub whole: bool,
}

/// Where a frame's checksum is left to finish: summed from `start` to the
/// frame's end and put at `start + offset` -- checked against the frame it
/// came with, so a guest it is handed to is told nothing outside it. Made by
/// `Partial::check` alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partial {
    start: u16,
    offset: u16,
}

impl Partial {
    /// The place a driver gave, if it lies in a frame of `len` bytes.
    pub fn check(len: usize, start: u16, offset: u16) -> Result<Partial, Dropped> {
        let at = usize::from(start) + usize::from(offset);
        if at + 2 > len {
            return Err(Dropped::ChecksumPlace);
        }
        Ok(Partial { start, offset })
    }

    /// The checksum finished in `frame`, the one it was checked against:
    /// the driver left there the sum of the pseudo-header, which the sum
    /// over the rest takes in. Nothing for a frame too short for its place.
    pub fn complete(&self, frame: &mut [u8]) {
        let (start, at) = (usize::from(self.start), usize::from(self.start) + usize::from(self.offset));
        if at + 2 > frame.len() {
            return;
        }
        let mut c = checksum(sum(&frame[start..], 0));
        if c == 0 && udp4_checksum_at(frame, at) {
            c = 0xFFFF;
        }
        wire::set_be16(frame, at, c);
    }

    /// The header a driver that takes checksums partial is handed it with.
    pub fn header(&self) -> TxHeader {
        TxHeader { flags: NEEDS_CSUM, gso_type: GSO_NONE, hdr_len: 0, gso_size: 0,
                   csum_start: self.start, csum_offset: self.offset }
    }
}

/// A send of TCP over IPv4 that held up: its headers' lengths, and the
/// segment size it is cut by. Made by `check_tcp4` alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tcp4 {
    ihl: usize,
    thl: usize,
    mss: usize,
}

impl Tcp4 {
    /// The segment size it is cut by.
    pub fn mss(&self) -> usize {
        self.mss
    }

    /// Where its headers end: the Ethernet header's, IPv4's and TCP's.
    pub fn headers(&self) -> usize {
        ETH_HDR_LEN + self.ihl + self.thl
    }

    /// The frames it is cut into, `payload` bytes of TCP behind its headers.
    pub fn frames(&self, payload: usize) -> usize {
        payload.div_ceil(self.mss).max(1)
    }

    /// The header a driver that takes sends whole is handed it with: to be
    /// cut by the segment size, its checksum left to finish at TCP's -- from
    /// the send as checked here, not from what its sender's header said.
    pub fn header(&self) -> TxHeader {
        TxHeader {
            flags: NEEDS_CSUM,
            gso_type: GSO_TCPV4,
            hdr_len: self.headers() as u16,
            gso_size: self.mss as u16,
            csum_start: (ETH_HDR_LEN + self.ihl) as u16,
            csum_offset: TCP_CHECKSUM as u16,
        }
    }
}

/// `frame`, sent with `hdr`: what it comes to (`Send`), or why it goes
/// nowhere.
pub fn transmit<'a>(hdr: &TxHeader, frame: &'a mut [u8], offloads: Offloads) -> Result<Send<'a>, Dropped> {
    if hdr.gso_type & GSO_ECN != 0 {
        return Err(Dropped::NotOffered);
    }
    match hdr.gso_type {
        GSO_NONE => {
            let partial = if hdr.flags & NEEDS_CSUM != 0 {
                if !offloads.csum {
                    return Err(Dropped::NotOffered);
                }
                Some(Partial::check(frame.len(), hdr.csum_start, hdr.csum_offset)?)
            } else {
                None
            };
            if frame.len() > MAX_FRAME {
                return Err(Dropped::TooLong);
            }
            Ok(Send::Frame(frame, partial))
        }
        GSO_TCPV4 if offloads.tso4 => {
            let layout = check_tcp4(frame, usize::from(hdr.gso_size))?;
            Ok(Send::Tcp4 { frame, layout })
        }
        _ => Err(Dropped::NotOffered),
    }
}

/// Whether `at` is the checksum of the UDP datagram an IPv4 frame carries.
fn udp4_checksum_at(frame: &[u8], at: usize) -> bool {
    if frame.len() < ETH_HDR_LEN + IP_MIN || wire::be16(frame, 12) != ETH_TYPE_IP {
        return false;
    }
    let ip = &frame[ETH_HDR_LEN..];
    let ihl = usize::from(ip[0] & 0xF) * 4;
    ip[IP_PROTO] == IP_PROTO_UDP && at == ETH_HDR_LEN + ihl + UDP_CHECKSUM
}

/// Whether `frame` is TCP over IPv4 that can be cut by `mss`: its headers,
/// the lengths they say and the segment size against a frame's room -- all
/// of it before a byte is cut, so a send goes out entire or not at all.
pub fn check_tcp4(frame: &[u8], mss: usize) -> Result<Tcp4, Dropped> {
    if frame.len() < ETH_HDR_LEN + IP_MIN || wire::be16(frame, 12) != ETH_TYPE_IP {
        return Err(Dropped::NotTcp4);
    }
    let ip = &frame[ETH_HDR_LEN..];
    if ip[0] >> 4 != 4 || ip[IP_PROTO] != IP_PROTO_TCP {
        return Err(Dropped::NotTcp4);
    }
    if wire::be16(ip, IP_FRAG) & IP_FRAG_MASK != 0 {
        return Err(Dropped::NotTcp4);
    }
    let ihl = usize::from(ip[0] & 0xF) * 4;
    if ihl < IP_MIN || ip.len() < ihl + TCP_MIN {
        return Err(Dropped::Headers);
    }
    let thl = usize::from(ip[ihl + TCP_OFFSET] >> 4) * 4;
    if thl < TCP_MIN || ip.len() < ihl + thl {
        return Err(Dropped::Headers);
    }
    let headers = ETH_HDR_LEN + ihl + thl;
    if mss < MIN_SEGMENT || headers + mss > MAX_FRAME {
        return Err(Dropped::SegmentSize);
    }
    Ok(Tcp4 { ihl, thl, mss })
}

/// A send `check_tcp4` passed, cut into frames whose payloads are `mss`
/// bytes but the last, each through `seg` and handed to `out`: the headers
/// of the send in front of each, with its IP length, ID and checksum, its
/// sequence number and its TCP checksum its own, FIN and PSH the last
/// one's, CWR the first's. How many frames went out.
pub fn segment_tcp4(frame: &[u8], t: Tcp4, seg: &mut [u8; MAX_FRAME], out: &mut dyn FnMut(&[u8])) -> usize {
    let (ihl, thl, mss) = (t.ihl, t.thl, t.mss);
    let headers = t.headers();
    /* A layout checked against another frame than this one: nothing cut,
     * rather than a slice past its end. */
    if frame.len() < headers || frame.len() > MAX_SEND {
        return 0;
    }
    let ip = &frame[ETH_HDR_LEN..];
    let payload = &frame[headers..];
    let count = t.frames(payload.len());
    let id = wire::be16(ip, IP_ID);
    let tcp = &ip[ihl..];
    let seq = wire::be32(tcp, TCP_SEQ);
    let flags = tcp[TCP_FLAGS];
    let (src, dst) = (wire::be32(ip, IP_SRC), wire::be32(ip, IP_DST));
    let (ip_at, tcp_at) = (ETH_HDR_LEN, ETH_HDR_LEN + ihl);

    for i in 0..count {
        /* Under 64 KiB of payload in all, cut at 256 bytes at least: the
         * offset fits a sequence number's step, and the count an ID's. */
        let from = i * mss;
        let len = mss.min(payload.len() - from);
        let total = headers + len;
        seg[..headers].copy_from_slice(&frame[..headers]);
        seg[headers..total].copy_from_slice(&payload[from..from + len]);

        wire::set_be16(seg, ip_at + IP_TOTAL_LEN, (ihl + thl + len) as u16);
        wire::set_be16(seg, ip_at + IP_ID, id.wrapping_add(i as u16));
        wire::set_be16(seg, ip_at + IP_CHECKSUM, 0);
        let c = checksum(sum(&seg[ip_at..ip_at + ihl], 0));
        wire::set_be16(seg, ip_at + IP_CHECKSUM, c);

        let mut f = flags;
        if i + 1 < count {
            f &= !(TCP_FIN | TCP_PSH);
        }
        if i > 0 {
            f &= !TCP_CWR;
        }
        seg[tcp_at + TCP_FLAGS] = f;
        wire::set_be32(seg, tcp_at + TCP_SEQ, seq.wrapping_add(from as u32));
        wire::set_be16(seg, tcp_at + TCP_CHECKSUM, 0);
        let c = checksum(sum(&seg[tcp_at..total], pseudo(src, dst, IP_PROTO_TCP, total - tcp_at)));
        wire::set_be16(seg, tcp_at + TCP_CHECKSUM, c);

        out(&seg[..total]);
    }
    count
}
