//! `stack`: frames of every kind the wire can carry, into the receive path,
//! with the machine's services up.

use netwire::{ETH_TYPE_ARP, ETH_TYPE_IP, IP_PROTO_ICMP, IP_PROTO_TCP, IP_PROTO_UDP, MAC_BROADCAST};

use crate::input::noise;
use crate::machine::ETH0_MAC;
use crate::world::frames::{self, Ip, Sum};
use crate::world::{Link, Net, Peers, World, ETH0_IP, GW_IP, GW_MAC, MASK};
use crate::Input;

/// Nobody answers the machine.
struct Quiet;

impl Peers for Quiet {
    fn on_frame(&mut self, _net: &mut Net, _frame: &[u8]) {}
}

fn random_frame(r: &mut Input) -> Vec<u8> {
    let src_ip = r.pick(&[GW_IP, 0x0A00_0263, 0xFFFF_FFFF, 0, ETH0_IP, 0xC0A8_0001]);
    let dst_ip = r.pick(&[ETH0_IP, ETH0_IP, ETH0_IP, 0xFFFF_FFFF, 0x0A00_02FF, 0x0808_0808, 0]);
    let dst_mac = if r.u8() < 200 { ETH0_MAC } else { MAC_BROADCAST };
    match r.u8() % 6 {
        0 => frames::arp(dst_mac, GW_MAC, r.pick(&[1, 2, 3]), GW_MAC, src_ip, [0; 6], dst_ip),
        1 => {
            let payload = noise(r.u32(), r.below(64) as usize);
            let m = frames::icmp(r.pick(&[8, 0, 3, 11, 5]), r.pick(&[0, 3, 2]), r.u16(), r.u16(), &payload, Sum::Right);
            frames::ipv4(dst_mac, GW_MAC, &Ip::new(src_ip, dst_ip, IP_PROTO_ICMP), &m)
        }
        2 => {
            let payload = noise(r.u32(), r.below(128) as usize);
            let any = r.u16();
            let d = frames::udp(src_ip, dst_ip, r.u16(), r.pick(&[53, 67, 68, 9000, 10053, any]), &payload,
                                Sum::Right);
            frames::ipv4(dst_mac, GW_MAC, &Ip::new(src_ip, dst_ip, IP_PROTO_UDP), &d)
        }
        3 => {
            let flags = r.u8() & 0x3F;
            let any = r.u16();
            let s = frames::tcp(src_ip, dst_ip, r.u16(), r.pick(&[22, 80, any]), r.u32(), r.u32(), flags, r.u16(),
                                &[], &noise(r.u32(), r.below(32) as usize), Sum::Right);
            frames::ipv4(dst_mac, GW_MAC, &Ip::new(src_ip, dst_ip, IP_PROTO_TCP), &s)
        }
        4 => {
            let n = r.below(1600) as usize;
            let mut f = noise(r.u32(), n);
            if f.len() >= 14 {
                f[..6].copy_from_slice(&dst_mac);
                netwire::set_be16(&mut f, 12, r.pick(&[ETH_TYPE_IP, ETH_TYPE_ARP, 0x86DD]));
            }
            f
        }
        _ => frames::eth(dst_mac, GW_MAC, 0x86DD, &noise(r.u32(), 40)),
    }
}

pub fn stack(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);
    let mut w = World::new(Link::perfect(), Quiet);
    let listener = net::tcp::TCP.listen(&nic, 22);

    while let Some(op) = r.op(5) {
        match op {
            0..=3 => {
                let f = random_frame(r);
                w.net.inject(f);
            }
            _ => w.run_for(r.below(2_000_000_000)),
        }
        w.pump();
    }

    if let Some(l) = listener {
        net::tcp::TCP.close(l);
    }
    /* Long enough for every connection to run out its retransmits and its
     * TIME-WAIT. */
    w.run_for(200_000_000_000);
    super::audit();
}
