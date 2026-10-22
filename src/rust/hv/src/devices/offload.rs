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
/// makes: 64 KiB of IPv4, the Ethernet header in front of it.
pub const MAX_SEND: usize = 65_536 + ETH_HDR_LEN;
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

/// `frame`, sent with `hdr`: handed to `out` as the frame it is, with its
/// checksum completed where the header asks, or cut into frames of at most
/// `MAX_FRAME` (`segment_tcp4`, through `seg`, a frame's room) -- how many
/// frames went out, or why none did.
pub fn transmit(hdr: &TxHeader, frame: &mut [u8], offloads: Offloads, seg: &mut [u8], out: &mut dyn FnMut(&[u8]))
    -> Result<usize, Dropped>
{
    if hdr.gso_type & GSO_ECN != 0 {
        return Err(Dropped::NotOffered);
    }
    match hdr.gso_type {
        GSO_NONE => {
            if hdr.flags & NEEDS_CSUM != 0 {
                if !offloads.csum {
                    return Err(Dropped::NotOffered);
                }
                complete_checksum(frame, usize::from(hdr.csum_start), usize::from(hdr.csum_offset))?;
            }
            if frame.len() > MAX_FRAME {
                return Err(Dropped::TooLong);
            }
            out(frame);
            Ok(1)
        }
        GSO_TCPV4 if offloads.tso4 => segment_tcp4(frame, usize::from(hdr.gso_size), seg, out),
        _ => Err(Dropped::NotOffered),
    }
}

/// The checksum at `start + offset`, over the frame from `start` to its end:
/// the driver left there the sum of the pseudo-header, which the sum over
/// the rest takes in.
fn complete_checksum(frame: &mut [u8], start: usize, offset: usize) -> Result<(), Dropped> {
    let at = start.checked_add(offset).ok_or(Dropped::ChecksumPlace)?;
    if at.checked_add(2).map_or(true, |end| end > frame.len()) {
        return Err(Dropped::ChecksumPlace);
    }
    let mut c = checksum(sum(&frame[start..], 0));
    if c == 0 && udp4_checksum_at(frame, at) {
        c = 0xFFFF;
    }
    wire::set_be16(frame, at, c);
    Ok(())
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

/// A send of TCP over IPv4 cut into frames whose payloads are `mss` bytes
/// but the last, each through `seg` and handed to `out`: the headers of the
/// send in front of each, with its IP length, ID and checksum, its sequence
/// number and its TCP checksum its own, FIN and PSH the last one's, CWR the
/// first's. How many frames went out, or why none did -- the headers checked
/// whole first, so a send goes out entire or not at all.
pub fn segment_tcp4(frame: &[u8], mss: usize, seg: &mut [u8], out: &mut dyn FnMut(&[u8])) -> Result<usize, Dropped> {
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
    if mss < MIN_SEGMENT || headers + mss > MAX_FRAME || seg.len() < MAX_FRAME {
        return Err(Dropped::SegmentSize);
    }

    let payload = &frame[headers..];
    let count = payload.len().div_ceil(mss).max(1);
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
    Ok(count)
}
