//! The hosts on the machine's LAN, as ARP knows them: the gateway, and
//! whoever else a target puts there. They answer the machine's ARP requests
//! for their addresses -- and for an address off the subnet nobody does,
//! the machine asking the gateway for it instead.

use netwire::{arp, eth, ARP_LEN, ETH_HDR_LEN, ETH_TYPE_ARP};

use super::frames;
use super::{Net, GW_IP, GW_MAC};

pub struct Lan {
    /// Address and MAC of every host that answers for itself.
    pub hosts: Vec<(u32, [u8; 6])>,
    /// Of 256 requests, how many go unanswered: a host that is not there
    /// yet, a reply lost.
    pub deaf: u8,
    rng: u64,
    /// What the machine asked for, in order.
    pub asked: Vec<u32>,
}

impl Lan {
    pub fn new() -> Lan {
        Lan { hosts: vec![(GW_IP, GW_MAC)], deaf: 0, rng: 0x5DEE_CE66, asked: Vec::new() }
    }

    pub fn add(&mut self, ip: u32, mac: [u8; 6]) {
        self.hosts.push((ip, mac));
    }

    pub fn mac_of(&self, ip: u32) -> Option<[u8; 6]> {
        self.hosts.iter().find(|h| h.0 == ip).map(|h| h.1)
    }

    /// A frame the machine sent: an ARP request is answered by the host
    /// asked for, if it is there; true when it was ARP at all.
    pub fn on_frame(&mut self, net: &mut Net, frame: &[u8]) -> bool {
        if frame.len() < ETH_HDR_LEN + ARP_LEN || eth::ether_type(frame) != ETH_TYPE_ARP {
            return false;
        }
        let a = &frame[ETH_HDR_LEN..];
        if arp::opcode(a) != 1 {
            return true;
        }
        let want = arp::target_ip(a);
        self.asked.push(want);
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        if (self.rng % 256) < u64::from(self.deaf) {
            return true;
        }
        if let Some(mac) = self.mac_of(want) {
            let reply = frames::arp(arp::sender_mac(a), mac, 2, mac, want, arp::sender_mac(a), arp::sender_ip(a));
            net.send(reply);
        }
        true
    }
}
