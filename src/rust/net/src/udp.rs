//! UDP: one datagram out of a device, and what a received one is made of.
//!
//! Everything above UDP in this kernel -- DHCP, DNS, the shell, netconsole,
//! the load target -- builds its payload and hands it to [`send`], or is
//! handed a received frame and takes it apart with [`parse`].

use kcore::net::Nic;
use kcore::trace;

use crate::arp::ArpTable;
use crate::wire::{self, eth, ip, udp, Mac, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN,
                  IP_PROTO_UDP, MAC_BROADCAST, UDP_HDR_LEN};

/// An Ethernet frame, headers included.
pub const MAX_FRAME: usize = 1514;

/// What fits in one datagram out of this stack.
pub const MAX_PAYLOAD: usize = MAX_FRAME - ETH_HDR_LEN - IP_HDR_LEN - UDP_HDR_LEN;

/// A received datagram: who it is from, and what it carries.
pub struct Datagram<'a> {
    pub src_ip: u32,
    pub dst_ip: u32,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

/// What a frame off the wire holds, if it holds a whole UDP datagram at all.
///
/// The IP header's own length is honoured, so a packet carrying options puts
/// its UDP header where this looks for it; a length that disagrees with the
/// frame is a None rather than a read past the end.
pub fn parse(frame: &[u8]) -> Option<Datagram<'_>> {
    if frame.len() < ETH_HDR_LEN + IP_HDR_LEN + UDP_HDR_LEN {
        return None;
    }

    let packet = &frame[ETH_HDR_LEN..];
    let ip_len = ip::header_len(packet);
    if ip_len == 0 || frame.len() < ETH_HDR_LEN + ip_len + UDP_HDR_LEN {
        return None;
    }
    if ip::protocol(packet) != IP_PROTO_UDP {
        return None;
    }

    let datagram = &frame[ETH_HDR_LEN + ip_len..];
    let length = udp::length(datagram) as usize;
    if length < UDP_HDR_LEN || length > datagram.len() {
        return None;
    }

    Some(Datagram {
        src_ip: ip::src(packet),
        dst_ip: ip::dst(packet),
        src_port: udp::src_port(datagram),
        dst_port: udp::dst_port(datagram),
        payload: &datagram[UDP_HDR_LEN..length],
    })
}

/// A datagram to a host whose Ethernet address is already known -- from the
/// frame it arrived in, or from the ARP cache. Any context: nothing here
/// sleeps.
pub fn send_to(
    nic: &Nic, dst_mac: &Mac, dst_ip: u32, dst_port: u16, src_ip: u32, src_port: u16,
    payload: &[u8],
) -> bool {
    if payload.len() > MAX_PAYLOAD {
        return false;
    }

    let datagram_len = UDP_HDR_LEN + payload.len();
    let frame_len = ETH_HDR_LEN + IP_HDR_LEN + datagram_len;

    let mut frame = [0u8; MAX_FRAME];
    eth::write(&mut frame, dst_mac, &nic.mac(), ETH_TYPE_IP);
    ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_UDP, src_ip, dst_ip, datagram_len, 0);

    let at = ETH_HDR_LEN + IP_HDR_LEN;
    udp::write(&mut frame[at..], src_port, dst_port, payload.len());
    frame[at + UDP_HDR_LEN..at + datagram_len].copy_from_slice(payload);

    nic.send_raw(&frame[..frame_len])
}

/// A datagram, resolving the destination through ARP -- the gateway for an
/// address off the subnet. Task context: the resolution sleeps while it waits
/// for an answer, and a destination nothing answers for falls back to
/// broadcast, as this stack always has.
pub fn send(
    nic: &Nic, arp: &ArpTable, dst_ip: u32, dst_port: u16, src_ip: u32, src_port: u16,
    payload: &[u8],
) -> bool {
    let target = nic.route_ip(dst_ip);
    let dst_mac = match arp.resolve(nic, target) {
        Some(mac) => mac,
        None => {
            trace!(0, "udp: nothing answered for {:#x}, broadcasting", dst_ip);
            MAC_BROADCAST
        }
    };

    send_to(nic, &dst_mac, dst_ip, dst_port, src_ip, src_port, payload)
}

/* Keeps the checksum in the module's use for the headers written above */
pub use wire::checksum;
