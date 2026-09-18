//! What the boot self-test checks of the network layer: the packet formats.
//!
//! The protocols themselves are judged by using them -- a ping that comes
//! back has exercised ARP, the checksum and every header on the way out and
//! back. What that cannot reach is the receiving half: a machine under QEMU
//! user networking is never pinged and never asked for its address, so the
//! code that answers an echo request or an ARP request would go unexercised
//! until the day it ran on a real network.
//!
//! So the formats are checked here against packets written out by hand, on
//! every boot and on both architectures: the checksum against the worked
//! example in RFC 1071, and each header by writing one and reading it back.

use crate::wire::{self, arp, eth, icmp, ip, udp, ETH_HDR_LEN, ETH_TYPE_ARP, ETH_TYPE_IP,
                  ARP_LEN, ARP_OP_REPLY, ARP_OP_REQUEST, ICMP_HDR_LEN, IP_HDR_LEN,
                  IP_PROTO_ICMP, IP_PROTO_UDP, MAC_BROADCAST};
use kcore::trace;

const US: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
const THEM: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];
const OUR_IP: u32 = 0x0A00_020F; /* 10.0.2.15 */
const THEIR_IP: u32 = 0x0A00_0202; /* 10.0.2.2 */

fn check(what: &str, ok: bool) -> bool {
    if !ok {
        trace!(0, "net selftest: {} FAILED", what);
    }
    ok
}

/// The checksum, against the example worked through in RFC 1071 section 3:
/// the bytes 00 01 f2 03 f4 f5 f6 f7 sum to 0xddf2, and the checksum is that
/// sum complemented -- 0x220d, which is what goes in the packet.
fn checksum_vectors() -> bool {
    let rfc1071 = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
    let mut ok = check("the RFC 1071 example", wire::checksum(&rfc1071) == 0x220d);

    /* A checksum laid back into what it covers makes the whole sum zero,
     * which is how a receiver checks one. */
    let mut header = [0u8; IP_HDR_LEN];
    ip::write(&mut header, IP_PROTO_UDP, OUR_IP, THEIR_IP, 100, 0x1234);
    ok &= check("a header checks against its own checksum",
        wire::checksum(&header) == 0);

    /* An odd length pads with a zero byte rather than reading past the end */
    ok &= check("an odd length is padded",
        wire::checksum(&[0x00, 0x01, 0xf2]) == wire::checksum(&[0x00, 0x01, 0xf2, 0x00]));

    /* One bit flipped anywhere must show */
    header[9] ^= 0x01;
    ok &= check("a flipped bit breaks the checksum", wire::checksum(&header) != 0);

    ok
}

/// Every header written and read back, at the offsets the wire puts them.
fn headers() -> bool {
    let mut frame = [0u8; 128];

    eth::write(&mut frame, &THEM, &US, ETH_TYPE_IP);
    let mut ok = check("an ethernet header reads back",
        eth::dst(&frame) == THEM && eth::src(&frame) == US
            && eth::ether_type(&frame) == ETH_TYPE_IP);
    /* Big-endian on the wire, whatever the machine is */
    ok &= check("the ether type is big-endian",
        frame[12] == 0x08 && frame[13] == 0x00);

    ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_ICMP, OUR_IP, THEIR_IP, 40, 7);
    let packet = &frame[ETH_HDR_LEN..];
    ok &= check("an ip header reads back",
        ip::version(packet) == 4 && ip::header_len(packet) == IP_HDR_LEN
            && ip::protocol(packet) == IP_PROTO_ICMP
            && ip::src(packet) == OUR_IP && ip::dst(packet) == THEIR_IP
            && ip::total_len(packet) as usize == IP_HDR_LEN + 40);

    /* A header claiming fewer than five words is not one */
    let mut broken = [0u8; IP_HDR_LEN];
    broken[0] = 0x44;
    ok &= check("a short ihl is refused", ip::header_len(&broken) == 0);

    let mut datagram = [0u8; 64];
    udp::write(&mut datagram, 68, 67, 20);
    ok &= check("a udp header reads back",
        udp::src_port(&datagram) == 68 && udp::dst_port(&datagram) == 67
            && udp::length(&datagram) as usize == 8 + 20);

    let mut message = [0u8; 64];
    icmp::write(&mut message, icmp::ECHO_REQUEST, 0, 0x4142, 3, 64);
    ok &= check("an icmp header reads back",
        icmp::kind(&message) == icmp::ECHO_REQUEST && icmp::code(&message) == 0
            && icmp::id(&message) == 0x4142 && icmp::seq(&message) == 3);
    ok &= check("and carries a checksum over the whole message",
        wire::checksum(&message[..64]) == 0);

    ok
}

/// An ARP request written and read back, and the reply to it: what answers a
/// host asking where this machine is.
fn arp_packets() -> bool {
    let mut frame = [0u8; ETH_HDR_LEN + ARP_LEN];

    /* A request for us, as another host would put it on the wire */
    eth::write(&mut frame, &MAC_BROADCAST, &THEM, ETH_TYPE_ARP);
    arp::write(&mut frame[ETH_HDR_LEN..], ARP_OP_REQUEST,
        &THEM, THEIR_IP, &[0; 6], OUR_IP);

    let packet = &frame[ETH_HDR_LEN..];
    let mut ok = check("an arp request reads back",
        arp::opcode(packet) == ARP_OP_REQUEST
            && arp::sender_mac(packet) == THEM
            && arp::sender_ip(packet) == THEIR_IP
            && arp::target_ip(packet) == OUR_IP);

    /* The reply to it: our address, to the host that asked */
    let mut reply = [0u8; ETH_HDR_LEN + ARP_LEN];
    eth::write(&mut reply, &THEM, &US, ETH_TYPE_ARP);
    arp::write(&mut reply[ETH_HDR_LEN..], ARP_OP_REPLY, &US, OUR_IP, &THEM, THEIR_IP);

    let answer = &reply[ETH_HDR_LEN..];
    ok &= check("and the reply names us to the host that asked",
        eth::dst(&reply) == THEM && eth::ether_type(&reply) == ETH_TYPE_ARP
            && arp::opcode(answer) == ARP_OP_REPLY
            && arp::sender_mac(answer) == US && arp::sender_ip(answer) == OUR_IP
            && arp::target_ip(answer) == THEIR_IP);

    /* The hardware and protocol types are what an Ethernet/IPv4 ARP says */
    ok &= check("with the hardware and protocol types ARP asks for",
        answer[0] == 0 && answer[1] == 1 && answer[2] == 0x08 && answer[3] == 0x00
            && answer[4] == 6 && answer[5] == 4);

    ok
}

/// An echo request as a host sends one, and the reply built from it -- the
/// path a machine under QEMU user networking never takes.
fn echo_exchange() -> bool {
    const PAYLOAD: usize = 16;
    let msg_len = ICMP_HDR_LEN + PAYLOAD;
    let frame_len = ETH_HDR_LEN + IP_HDR_LEN + msg_len;

    let mut request = [0u8; 64];
    eth::write(&mut request, &US, &THEM, ETH_TYPE_IP);
    ip::write(&mut request[ETH_HDR_LEN..], IP_PROTO_ICMP, THEIR_IP, OUR_IP, msg_len, 9);
    let at = ETH_HDR_LEN + IP_HDR_LEN;
    for i in 0..PAYLOAD {
        request[at + ICMP_HDR_LEN + i] = (0xA0 + i) as u8;
    }
    icmp::write(&mut request[at..], icmp::ECHO_REQUEST, 0, 0x1234, 5, msg_len);

    /* What `process` checks before it answers: the message is whole, its
     * checksum holds, and it is addressed to us. */
    let packet = &request[ETH_HDR_LEN..];
    let total = ip::total_len(packet) as usize;
    let msg = &request[at..at + total - IP_HDR_LEN];
    let mut ok = check("an echo request arrives whole",
        total == IP_HDR_LEN + msg_len && ETH_HDR_LEN + total == frame_len
            && ip::dst(packet) == OUR_IP);
    ok &= check("and its checksum holds", wire::checksum(msg) == 0);

    /* The reply: the message back with its type changed, our address as the
     * source, and the requester's as the destination. */
    let mut reply = [0u8; 64];
    eth::write(&mut reply, &eth::src(&request), &US, ETH_TYPE_IP);
    ip::write(&mut reply[ETH_HDR_LEN..], IP_PROTO_ICMP,
        ip::dst(packet), ip::src(packet), msg.len(), 0);
    reply[at..at + msg.len()].copy_from_slice(msg);
    icmp::write(&mut reply[at..], icmp::ECHO_REPLY, 0,
        icmp::id(msg), icmp::seq(msg), msg.len());

    let answer = &reply[ETH_HDR_LEN..];
    let answer_msg = &reply[at..at + msg.len()];
    ok &= check("the reply goes back to where the request came from",
        eth::dst(&reply) == THEM && ip::src(answer) == OUR_IP
            && ip::dst(answer) == THEIR_IP);
    ok &= check("carries the request's id, sequence and payload",
        icmp::kind(answer_msg) == icmp::ECHO_REPLY
            && icmp::id(answer_msg) == 0x1234 && icmp::seq(answer_msg) == 5
            && answer_msg[ICMP_HDR_LEN..] == msg[ICMP_HDR_LEN..]);
    ok &= check("and checksums that a sender would accept",
        wire::checksum(answer_msg) == 0 && wire::checksum(&answer[..IP_HDR_LEN]) == 0);

    ok
}

/// Everything above. 0 when every check passed.
#[no_mangle]
pub extern "C" fn rust_net_selftest() -> i32 {
    let ok = checksum_vectors() & headers() & arp_packets() & echo_exchange();

    if ok {
        trace!(0, "net selftest: passed");
        0
    } else {
        -1
    }
}
