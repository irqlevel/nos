//! A VM's firewall (plans/vnet.md, "The firewall"): the rules a flow's first
//! packet is judged by, what of a frame they look at, and the table of flows
//! a port keeps, so that the rest of a flow -- and its answers -- go by
//! without being judged again.
//!
//! A firewall is named and holds a policy for each direction and rules in
//! order: the first rule a flow matches decides it, and where none does, the
//! policy of its direction. Directions are the VM's: `in` is what comes to
//! it, `out` what it sends. A rule says what it does (`allow`, `deny`), which
//! way (`in`, `out`), and optionally a protocol (`tcp`, `udp`, `icmp`), the
//! ports of the service the flow is for -- its first packet's destination
//! port, whichever way it goes -- and the other end: `any`, `net` (the VM's
//! own network), or networks by address and mask:
//!
//! ```text
//! allow in tcp 22 from 198.51.100.0/24
//! allow in tcp 80,443
//! allow in from net
//! deny out tcp 25
//! ```
//!
//! The switch that carries a VM's frames reads each one (`packet::read`)
//! and judges an IPv4 packet by the VM's flows and rules (`judge::judge`);
//! IPv6, which only a host network's guests carry between themselves, by
//! the rules that name no address and no port (`Rules::judge6`). What makes
//! no sense to the firewall -- ARP, what the switch answers itself, the
//! host's own traffic -- the switch keeps from it.
//!
//! No kernel in it and nothing that allocates on the way: a table of flows
//! is taken whole when it is made, and the rules are a fixed array. So the
//! hypervisor's module judges by it with a spin lock held and interrupts off,
//! the cluster's state checks a rule by it in `apply`, and the hv fuzzer
//! holds every verdict to a model of its own.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod edit;
pub mod flows;
pub mod judge;
pub mod packet;
pub mod rules;

pub use flows::{Flows, Key};
pub use judge::{judge, still_allowed, Context, Verdict, Why, LIFETIMES};
pub use packet::{read, read_send, Icmp, Quoted, Read, V4};
pub use rules::{valid_name, Action, By, Dir, First, Judged, Peer, Ports, Proto, Rule, Rules, NAME_MAX,
                NETS_MAX, PORTS_MAX, RULES_MAX};
