//! The network layer: what is on the wire, and the protocols over it.
//!
//! [`wire`] is the frame formats -- Ethernet, ARP, IP, UDP, ICMP -- read and
//! written through byte slices, with the internet checksum: the `netwire`
//! crate, which has no kernel in it so that a loadable module can use it
//! too. The protocol modules sit on it, and reach the devices through
//! [`nic`].
//!
//! A NIC's driver depends on this crate and registers with [`register`]; the
//! C++ side calls in by the `rust_net_*` names, and a loadable module by the
//! `kernel_net_*` / `kernel_netframe_*` / `kernel_tcp_*` ones, which
//! `kcore::net` and `kcore::tcp` wrap for it.

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
pub mod nat;
pub mod netconsole;
pub mod nic;
pub mod provider;
pub mod selftest;
pub mod shell;
pub mod tcp;
pub mod udp;
pub mod udp_shell;
pub mod vnic;
pub mod wget;

/// The frame formats: a crate of their own, shared with the modules.
pub use netwire as wire;

/// The network's file log, `/logs/net.log` (docs/logs.md): a line a
/// connection -- TCP's, made and reset; sshd's sessions -- which a machine
/// on the internet has all day, and which would crowd the kernel log.
/// sshd's lines go to it by its own `kcore::logs::Log` of the same name.
pub static LOG: fs::logs::Log = fs::logs::Log::new("net");

pub use device::{NetDriver, RxQueue, TxQueue};
pub use frame::{Frame, FrameQueue};
pub use nic::Nic;

/// Put a NIC in the device table; from here on its driver is called. `tx`
/// and `rx` are the two halves only `flush_tx` and `process_rx` touch, and
/// become the device's. Registered last, once the hardware is ready to be
/// asked, and for good: a net device is never given back.
///
/// None when the table is full or the name will not do -- and then the
/// halves are leaked rather than dropped, because the hardware may be
/// running on them: quiesce it.
pub fn register<D: NetDriver>(
    name: &str, mac: [u8; 6], driver: &'static D, tx: D::Tx, rx: D::Rx,
) -> Option<Nic> {
    device::DEVICES.register(name, mac, driver, tx, rx).map(Nic::of)
}

/// The name the next NIC registers by: the lowest `ethN` no device has yet
/// (`DeviceTable::free_eth_name`).
pub fn free_eth_name(buf: &mut [u8; device::ETH_NAME_MAX]) -> Option<&str> {
    device::DEVICES.free_eth_name(buf)
}

/// Put the layer's commands in front of whoever runs one. Called from
/// `rust_init`, before the shell starts.
pub fn init() {
    shell::register_all();
    provider::init();
}
