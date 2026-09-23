//! The guests' network: a switch between the virtio NICs of the guests that
//! have one and the host's end of it, `hv0` -- a virtual NIC in the kernel's
//! stack (`kcore::vnic`), 10.0.100.1/24, which is how nos reaches the guests
//! and they reach it.
//!
//! A guest's NIC is a port here, and a port's number is its address: MAC
//! 02:00:00:00:64:NN and 10.0.100.NN, NN the port plus two, handed to the
//! guest's kernel on its command line (`ip=`). So the switch forwards by the
//! destination MAC without learning anything: to the port whose MAC it is,
//! to `hv0` for `hv0`'s, and to every port -- and `hv0`, when the frame did
//! not come from there -- for a broadcast, a multicast or a MAC that is no
//! port's.
//!
//! A port's inbox holds what waits for the guest's NIC. It is filled from any
//! CPU, the host's transmit path among them with interrupts off, so it is a
//! spin lock with interrupts off over storage taken when the switch was made:
//! nothing on the way allocates, and a full inbox drops, counted. A frame put
//! in wakes the VM it belongs to -- a halted guest's vCPU waits on the VM's
//! event -- by a signal made under the inbox's lock, which is also what
//! keeps the VM there while it is made: the port gives the VM up in task
//! context, the lock down.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kcore::sync::IrqSpinLock;
use kcore::vnic::{Attached, Vnic, VnicSink};

use crate::vms::Shared;

/// The most ports: a VM each.
pub const MAX_PORTS: usize = 16;
/// Frames an inbox holds.
const INBOX_FRAMES: usize = 64;
const FRAME: usize = hv::nic::MAX_FRAME;

/// The host's end: its name, MAC and address, and the subnet's mask.
const HOST_IF: &str = "hv0";
const HOST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0x64, 0x01];
const SUBNET: u32 = 0x0A00_6400;
pub const HOST_IP: u32 = SUBNET | 1;
const MASK: u32 = 0xFFFF_FF00;
/// A port's MAC is this and its number plus two; so is its address's last
/// byte.
const PORT_MAC_PREFIX: [u8; 5] = [0x02, 0, 0, 0, 0x64];
const PORT_BASE: usize = 2;

pub fn port_ip(port: usize) -> u32 {
    SUBNET | (port + PORT_BASE) as u32
}

pub fn port_mac(port: usize) -> [u8; 6] {
    let mut m = [0u8; 6];
    m[..5].copy_from_slice(&PORT_MAC_PREFIX);
    m[5] = (port + PORT_BASE) as u8;
    m
}

/// The port a MAC is, if it is one.
fn port_of(mac: &[u8]) -> Option<usize> {
    if mac[..5] != PORT_MAC_PREFIX {
        return None;
    }
    usize::from(mac[5]).checked_sub(PORT_BASE).filter(|p| *p < MAX_PORTS)
}

/// An address, dotted, for a command line.
pub fn dotted(ip: u32) -> String {
    alloc::format!("{}.{}.{}.{}", ip >> 24, (ip >> 16) & 0xFF, (ip >> 8) & 0xFF, ip & 0xFF)
}

/// The kernel command line's `ip=` for the guest on `port`: its address, the
/// host's as the gateway, the mask, `eth0`, and no autoconfiguration.
pub fn ip_param(port: usize) -> String {
    alloc::format!("ip={}::{}:{}::eth0:off", dotted(port_ip(port)), dotted(HOST_IP), dotted(MASK))
}

/// What waits for one guest's NIC.
struct Inbox {
    /// The VM on the port; None while it is free.
    owner: Option<Arc<Shared>>,
    frames: Vec<[u8; FRAME]>,
    lens: [u16; INBOX_FRAMES],
    head: usize,
    count: usize,
}

struct Port {
    inbox: IrqSpinLock<Inbox>,
    /// `count`, readable without the lock: the vCPU looks every time round
    /// its loop, and takes the lock only when there is something.
    waiting: AtomicUsize,
    dropped: AtomicU64,
}

/// The ports, shared by the switch, every guest's NIC and `hv0`'s sink.
pub struct Ports {
    ports: Vec<Port>,
    vnic: Vnic,
    to_host: AtomicU64,
    refused: AtomicU64,
}

impl Ports {
    /// A frame into a port's inbox, and its VM woken.
    fn deliver(&self, port: usize, frame: &[u8]) {
        let Some(p) = self.ports.get(port) else { return };
        let mut inbox = p.inbox.lock();
        if inbox.owner.is_none() {
            return;
        }
        if inbox.count == INBOX_FRAMES || frame.len() > FRAME {
            p.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let slot = (inbox.head + inbox.count) % INBOX_FRAMES;
        inbox.frames[slot][..frame.len()].copy_from_slice(frame);
        inbox.lens[slot] = frame.len() as u16;
        inbox.count += 1;
        p.waiting.store(inbox.count, Ordering::Release);
        /* Under the lock: what keeps the VM from going meanwhile. */
        if let Some(owner) = &inbox.owner {
            owner.wake_up();
        }
    }

    /// A frame to wherever its destination says, from port `from` -- or
    /// from `hv0`, which a frame from there never goes back to.
    fn forward(&self, from: Option<usize>, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        let dst = &frame[..6];
        let broadcast = dst[0] & 1 != 0;
        if !broadcast {
            if dst == HOST_MAC {
                self.to_host(from, frame);
                return;
            }
            if let Some(p) = port_of(dst) {
                if Some(p) != from {
                    self.deliver(p, frame);
                }
                return;
            }
        }
        /* Everyone: a broadcast, a multicast, a MAC that is no port's. */
        for p in 0..self.ports.len() {
            if Some(p) != from {
                self.deliver(p, frame);
            }
        }
        self.to_host(from, frame);
    }

    fn to_host(&self, from: Option<usize>, frame: &[u8]) {
        if from.is_none() {
            return;
        }
        if self.vnic.receive(frame) {
            self.to_host.fetch_add(1, Ordering::Relaxed);
        } else {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The next frame in a port's inbox, into `buf`.
    fn take(&self, port: usize, buf: &mut [u8]) -> Option<usize> {
        let p = self.ports.get(port)?;
        if p.waiting.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut inbox = p.inbox.lock();
        if inbox.count == 0 {
            return None;
        }
        let slot = inbox.head;
        let len = usize::from(inbox.lens[slot]).min(buf.len());
        buf[..len].copy_from_slice(&inbox.frames[slot][..len]);
        inbox.head = (inbox.head + 1) % INBOX_FRAMES;
        inbox.count -= 1;
        p.waiting.store(inbox.count, Ordering::Release);
        Some(len)
    }
}

/// `hv0`'s sink: what the host sends to the guests.
struct Uplink(Arc<Ports>);

impl VnicSink for Uplink {
    fn on_frame(&self, frame: &[u8]) {
        self.0.forward(None, frame);
    }
}

/// What a guest's NIC is given: its port.
pub struct PortBackend {
    port: usize,
    ports: Arc<Ports>,
}

impl hv::nic::Backend for PortBackend {
    fn send(&mut self, frame: &[u8]) {
        self.ports.forward(Some(self.port), frame);
    }

    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.ports.take(self.port, buf)
    }
}

/// The switch: the ports, and `hv0`'s sink attached for as long as it lives.
pub struct Switch {
    /// Dropped first -- fields go in the order they are declared: `hv0`'s
    /// sink detached, and any call of it waited out, before the ports go.
    _uplink: Attached,
    ports: Arc<Ports>,
}

impl Switch {
    /// `hv0` opened -- made the first time any load of this module asks --
    /// and the ports made, their storage all taken now.
    pub fn new() -> Result<Switch, String> {
        let vnic = Vnic::open(HOST_IF, HOST_MAC, HOST_IP, MASK)
            .ok_or_else(|| String::from("no hv0: the kernel's virtual NICs are all taken, or one is being made"))?;
        let mut ports = Vec::new();
        ports.try_reserve_exact(MAX_PORTS).map_err(|_| String::from("out of memory for the switch"))?;
        for _ in 0..MAX_PORTS {
            let mut frames = Vec::new();
            frames.try_reserve_exact(INBOX_FRAMES).map_err(|_| String::from("out of memory for the switch"))?;
            frames.resize(INBOX_FRAMES, [0u8; FRAME]);
            ports.push(Port {
                inbox: IrqSpinLock::new(Inbox { owner: None, frames, lens: [0; INBOX_FRAMES], head: 0, count: 0 }),
                waiting: AtomicUsize::new(0),
                dropped: AtomicU64::new(0),
            });
        }
        let ports = Arc::new(Ports { ports, vnic, to_host: AtomicU64::new(0), refused: AtomicU64::new(0) });
        let uplink = vnic.attach(Arc::new(Uplink(ports.clone())))
            .ok_or_else(|| String::from("hv0 has a sink attached already -- another load of hv?"))?;
        Ok(Switch { _uplink: uplink, ports })
    }

    /// A free port for `vm`: its number, or None when all are taken.
    pub fn claim(&self, vm: &Arc<Shared>) -> Option<usize> {
        for (i, p) in self.ports.ports.iter().enumerate() {
            let mut inbox = p.inbox.lock();
            if inbox.owner.is_none() {
                /* An Arc clone under the lock: a count going up, nothing
                 * allocated. */
                inbox.owner = Some(vm.clone());
                inbox.head = 0;
                inbox.count = 0;
                p.waiting.store(0, Ordering::Release);
                return Some(i);
            }
        }
        None
    }

    /// The port given up, and its VM with it -- dropped here, the lock down.
    pub fn release(&self, port: usize) {
        let Some(p) = self.ports.ports.get(port) else { return };
        let owner = {
            let mut inbox = p.inbox.lock();
            inbox.count = 0;
            p.waiting.store(0, Ordering::Release);
            inbox.owner.take()
        };
        drop(owner);
    }

    /// The backend a guest's NIC on `port` is built with.
    pub fn backend(&self, port: usize) -> PortBackend {
        PortBackend { port, ports: self.ports.clone() }
    }

    /// The net device `hv0` is, for TCP to the guests.
    pub fn host_nic(&self) -> Option<kcore::net::Nic> {
        self.ports.vnic.nic()
    }

    /// Frames a port's inbox had no room for.
    pub fn dropped(&self, port: usize) -> u64 {
        self.ports.ports.get(port).map_or(0, |p| p.dropped.load(Ordering::Relaxed))
    }

    /// What went to the host, and what the host would not take.
    pub fn host_counts(&self) -> (u64, u64) {
        (self.ports.to_host.load(Ordering::Relaxed), self.ports.refused.load(Ordering::Relaxed))
    }
}
