//! UDP: one datagram out of a device, and what a received one is made of.
//!
//! Everything above UDP in this kernel -- DHCP, DNS, the shell, netconsole
//! -- builds its payload and hands it to [`send`], or is handed a received
//! frame and takes it apart with [`parse`]. (The load target, a module,
//! builds its own frames: the formats are `netwire`'s, which both share.)

use crate::nic::Nic;
use kcore::trace;

use crate::arp::ArpTable;
use crate::wire::{self, udp, Mac};

pub use crate::wire::udp::{parse, Datagram, MAX_PAYLOAD};
pub use crate::wire::MAX_FRAME;

/// A datagram to a host whose Ethernet address is already known -- from the
/// frame it arrived in, or from the ARP cache. Any context: nothing here
/// sleeps. Not from 0.0.0.0, which is no address: the device's may have gone
/// between the sender reading it and sending -- DHCP letting a lease go --
/// and only a DHCP client, which builds its own, sends from nowhere.
pub fn send_to(
    nic: &Nic, dst_mac: &Mac, dst_ip: u32, dst_port: u16, src_ip: u32, src_port: u16,
    payload: &[u8],
) -> bool {
    if src_ip == 0 {
        return false;
    }
    let route = udp::Route {
        src_mac: nic.mac(), dst_mac: *dst_mac, src_ip, dst_ip, src_port, dst_port,
        dont_fragment: false,
    };

    let mut frame = [0u8; MAX_FRAME];
    let frame_len = match udp::write_frame(&mut frame, &route, payload.len()) {
        Some(len) => len,
        None => return false,
    };
    frame[udp::PAYLOAD_AT..frame_len].copy_from_slice(payload);

    nic.send_raw(&frame[..frame_len])
}

/// A datagram, its destination's Ethernet address from ARP -- the gateway's
/// for an address off the subnet (`ArpTable::destination`). Task context: the
/// resolution sleeps while it waits for an answer. False when nothing
/// answers: the datagram is not handed to everyone on the link instead.
pub fn send(
    nic: &Nic, arp: &ArpTable, dst_ip: u32, dst_port: u16, src_ip: u32, src_port: u16,
    payload: &[u8],
) -> bool {
    let dst_mac = match arp.destination(nic, dst_ip) {
        Some(mac) => mac,
        None => {
            trace!(0, "udp: nothing answered for {}, not sent", wire::Ipv4(dst_ip));
            return false;
        }
    };

    send_to(nic, &dst_mac, dst_ip, dst_port, src_ip, src_port, payload)
}

/* Keeps the checksum in the module's use for the headers written above */
pub use wire::checksum;
