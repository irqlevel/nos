//! The network layer: what is on the wire, and the protocols over it.
//!
//! [`wire`] is the frame formats -- Ethernet, ARP, IP, UDP, ICMP -- read and
//! written through byte slices, with the internet checksum. The protocol
//! modules sit on it, and reach the devices through `kcore::net`.
//!
//! The C++ side calls in by the names at the bottom of each module; the
//! device layer and the protocols above TCP are still there and move next.

#![no_std]

extern crate alloc;

pub mod abi;
pub mod arp;
pub mod device;
pub mod dhcp;
pub mod dns;
pub mod frame;
pub mod http;
pub mod icmp;
pub mod net_load;
pub mod netconsole;
pub mod selftest;
pub mod shell;
pub mod tcp;
pub mod udp;
pub mod udp_shell;
pub mod wget;
pub mod wire;

/// Put the layer's commands in front of whoever runs one. Called from
/// `rust_init`, before the shell starts.
pub fn init() {
    shell::register_all();
}
