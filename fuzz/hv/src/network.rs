//! What the guests' network reaches of the kernel: the net layer's devices,
//! frames, ARP and NAT singleton (as `net/src/nat.rs` reaches them), the
//! module's view of them (`kcore::net`, `kcore::vnic`, as the switch in
//! `modules/hv/src/net.rs` does), and the VM a port wakes.
//!
//! Wired as the kernel wires them. `hv0` is a virtual NIC: what the switch
//! hands the stack (`Vnic::receive`) goes first to NAT's `intercept`, as a
//! received frame does, and what is sent out of `hv0` -- NAT's answers to
//! the guests -- goes to the sink the switch attached. The uplink `eth0`
//! has a gateway, and keeps what is sent out of it for the target to look
//! at; so does `hv0` when no sink is attached. ARP knows the hops the
//! target says it does. Everything is reset for each input.

use std::sync::Mutex;

use netwire::Mac;

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub mod device {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    use netwire::Mac;

    /// A device of the stack's, as NAT reads one.
    pub struct Device {
        name: &'static [u8],
        mac: Mutex<Mac>,
        ip: AtomicU32,
        mask: AtomicU32,
        gw: AtomicU32,
        /// Frames sent out of it, for the target; and whether it refuses
        /// them (no room on the device, the pool dry).
        pub(crate) sent: Mutex<Vec<Vec<u8>>>,
        pub(crate) refuse: AtomicBool,
    }

    impl Device {
        const fn new(name: &'static [u8]) -> Device {
            Device { name, mac: Mutex::new([0; 6]), ip: AtomicU32::new(0), mask: AtomicU32::new(0),
                     gw: AtomicU32::new(0), sent: Mutex::new(Vec::new()), refuse: AtomicBool::new(false) }
        }

        pub fn name(&self) -> &[u8] {
            self.name
        }
        pub fn mac(&self) -> Mac {
            *super::locked(&self.mac)
        }
        pub fn ip(&self) -> u32 {
            self.ip.load(Ordering::Relaxed)
        }
        pub fn mask(&self) -> u32 {
            self.mask.load(Ordering::Relaxed)
        }
        pub fn gw(&self) -> u32 {
            self.gw.load(Ordering::Relaxed)
        }
        /// As the stack's: the gateway for what is off the subnet.
        pub fn route_ip(&self, dst: u32) -> u32 {
            let (mask, gw) = (self.mask(), self.gw());
            if mask != 0 && gw != 0 && (dst & mask) != (self.ip() & mask) { gw } else { dst }
        }
        pub fn handle(&self) -> usize {
            self as *const Device as usize
        }
        pub fn as_nic(&'static self) -> crate::nic::Nic {
            crate::nic::Nic { dev: self }
        }

        /// Set up for an input.
        pub fn set(&self, mac: Mac, ip: u32, mask: u32, gw: u32) {
            *super::locked(&self.mac) = mac;
            self.ip.store(ip, Ordering::Relaxed);
            self.mask.store(mask, Ordering::Relaxed);
            self.gw.store(gw, Ordering::Relaxed);
            super::locked(&self.sent).clear();
            self.refuse.store(false, Ordering::Relaxed);
        }

        /// What was sent out of it since last asked.
        pub fn take_sent(&self) -> Vec<Vec<u8>> {
            core::mem::take(&mut *super::locked(&self.sent))
        }
    }

    /// `hv0`, the guests' side; `eth0`, the way out; and a third, with no
    /// address, that NAT must not take for either.
    pub static HV0: Device = Device::new(b"hv0");
    pub static ETH0: Device = Device::new(b"eth0");
    pub static SPARE: Device = Device::new(b"eth1");

    pub struct DeviceTable {
        devices: [&'static Device; 3],
    }

    pub static DEVICES: DeviceTable = DeviceTable { devices: [&HV0, &ETH0, &SPARE] };

    impl DeviceTable {
        /// As the stack's: the first device with an address and a gateway,
        /// `not` aside.
        pub fn uplink(&'static self, not: &Device) -> Option<&'static Device> {
            self.devices.iter().copied().find(|d| !core::ptr::eq(*d, not) && d.ip() != 0 && d.gw() != 0)
        }
        pub fn is_local(&'static self, addr: u32) -> bool {
            addr != 0 && self.devices.iter().any(|d| d.ip() == addr)
        }
        /// A handle back to its device: one of the table's, or none.
        pub fn by_handle(&'static self, handle: usize) -> Option<&'static Device> {
            self.devices.iter().copied().find(|d| d.handle() == handle)
        }
    }
}

/// `hv::nic` (the virtio NIC's backend, for the switch) and the stack's
/// `crate::nic::Nic` (a device to transmit on, for NAT): two crates' names
/// that are one path here.
pub mod nic {
    pub use crate::devices::net::*;

    use crate::device::{Device, HV0};

    pub struct Nic {
        pub(crate) dev: &'static Device,
    }

    impl Nic {
        /// A frame out of the device: `hv0`'s to the switch's sink, as a
        /// virtual NIC's transmit path hands it -- on the sender's own
        /// thread, then and there -- and anything else kept to look at.
        /// False when the device refuses it.
        pub fn transmit(&self, frame: crate::frame::Frame) -> bool {
            if self.dev.refuse.load(core::sync::atomic::Ordering::Relaxed) {
                return false;
            }
            if core::ptr::eq(self.dev, &HV0) {
                if let Some(sink) = crate::vnic::sink() {
                    sink.on_frame(frame.bytes());
                    return true;
                }
            }
            super::locked(&self.dev.sent).push(frame.bytes().to_vec());
            true
        }
    }
}

pub mod frame {
    /// A frame of the pool's, as NAT takes one: room for its length, filled
    /// whole.
    pub struct Frame {
        data: Vec<u8>,
        len: usize,
    }

    impl Frame {
        pub fn alloc_tx(len: usize) -> Option<Frame> {
            (len <= netwire::MAX_FRAME).then(|| Frame { data: vec![0; netwire::MAX_FRAME], len })
        }
        pub fn fill(&mut self, data: &[u8]) -> bool {
            if data.len() > self.data.len() {
                return false;
            }
            self.data[..data.len()].copy_from_slice(data);
            self.len = data.len();
            true
        }
        pub fn data_mut(&mut self) -> &mut [u8] {
            &mut self.data[..self.len]
        }
        pub fn bytes(&self) -> &[u8] {
            &self.data[..self.len]
        }
    }
}

pub mod abi {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    use netwire::Mac;

    /// ARP as NAT asks it: the hops it knows, and the ones it was asked for.
    pub struct Arp {
        pub(crate) known: Mutex<HashMap<u32, Mac>>,
        pub(crate) asked: Mutex<Vec<u32>>,
    }

    impl Arp {
        pub fn lookup(&self, ip: u32) -> Option<Mac> {
            super::locked(&self.known).get(&ip).copied()
        }
        pub fn ask(&self, _nic: &crate::nic::Nic, ip: u32) {
            super::locked(&self.asked).push(ip);
        }
        pub fn lookup_or_ask(&self, nic: &crate::nic::Nic, ip: u32) -> Option<Mac> {
            let hit = self.lookup(ip);
            if hit.is_none() {
                self.ask(nic, ip);
            }
            hit
        }
    }

    static ARP: OnceLock<Arp> = OnceLock::new();
    static NAT: OnceLock<crate::nat::Nat> = OnceLock::new();

    pub fn arp_table() -> Option<&'static Arp> {
        Some(ARP.get_or_init(|| Arp { known: Mutex::new(HashMap::new()), asked: Mutex::new(Vec::new()) }))
    }

    pub fn nat() -> Option<&'static crate::nat::Nat> {
        Some(NAT.get_or_init(|| crate::nat::Nat::new().expect("NAT's lock")))
    }
}

pub mod dns {
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) static UPSTREAM: AtomicU32 = AtomicU32::new(0);

    /// The DNS server the machine was given, 0 for none.
    pub fn upstream() -> u32 {
        UPSTREAM.load(Ordering::Relaxed)
    }
}

pub mod tcp {
    /// Where the stack's own ephemeral ports begin, above NAT's.
    pub const EPHEMERAL_BASE: u16 = 49152;
}

/// `kcore::net` as a module sees it -- and `ffi::net`, whose `NatOn` the
/// net crate's export answers with: a device by handle, NAT turned on and
/// off through the net crate's own exports, as the kernel's does it.
pub mod net {
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct NatOn {
        pub code: i32,
        pub outer: usize,
    }

    #[derive(Clone, Copy)]
    pub struct Nic {
        handle: usize,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum NatError {
        NoUplink,
        Device,
        Busy,
        NoMemory,
    }

    pub struct Nat {
        inner: usize,
        outer: Nic,
    }

    impl Nic {
        pub fn from_handle(handle: usize) -> Option<Nic> {
            if handle == 0 { None } else { Some(Nic { handle }) }
        }
        pub fn ip(&self) -> u32 {
            crate::device::DEVICES.by_handle(self.handle).map_or(0, |d| d.ip())
        }
        pub fn nat(&self) -> core::result::Result<Nat, NatError> {
            let on = crate::nat::kernel_net_nat_enable(self.handle);
            match on.code {
                0 => Ok(Nat { inner: self.handle, outer: Nic { handle: on.outer } }),
                1 => Err(NatError::NoUplink),
                3 => Err(NatError::Busy),
                4 => Err(NatError::NoMemory),
                _ => Err(NatError::Device),
            }
        }
    }

    impl Nat {
        pub fn outer(&self) -> Nic {
            self.outer
        }
    }

    impl Drop for Nat {
        fn drop(&mut self) {
            crate::nat::kernel_net_nat_disable(self.inner);
        }
    }

    /// The DNS server the machine was given.
    pub fn dns_server() -> Option<u32> {
        let ip = crate::dns::upstream();
        if ip == 0 { None } else { Some(ip) }
    }
}

/// `kcore::vnic`: the one virtual NIC, `hv0`.
pub mod vnic {
    use std::sync::{Arc, Mutex};

    pub trait VnicSink: Send + Sync + 'static {
        fn on_frame(&self, frame: &[u8]);
    }

    /// What the stack's end of the virtual NIC is: the sink attached, and
    /// whether it has been opened.
    struct State {
        sink: Option<Arc<dyn VnicSink>>,
        refuse: bool,
    }

    static STATE: Mutex<State> = Mutex::new(State { sink: None, refuse: false });

    #[derive(Clone, Copy)]
    pub struct Vnic {
        _handle: usize,
    }

    impl Vnic {
        pub fn open(_name: &str, mac: [u8; 6], ip: u32, mask: u32) -> Option<Vnic> {
            crate::device::HV0.set(mac, ip, mask, 0);
            Some(Vnic { _handle: 1 })
        }
        pub fn nic(&self) -> Option<crate::net::Nic> {
            crate::net::Nic::from_handle(crate::device::HV0.handle())
        }
        pub fn attach<S: VnicSink>(&self, sink: Arc<S>) -> Option<Attached> {
            let mut s = super::locked(&STATE);
            if s.sink.is_some() {
                return None;
            }
            s.sink = Some(sink);
            Some(Attached { _private: () })
        }
        /// A frame into the stack as `hv0` received it, kept for the target:
        /// an IPv4 packet to NAT first, as the receive path hands it
        /// (`net/src/device.rs`), and else -- and anything else -- the
        /// stack's own, kept too. False when the stack would not take it.
        pub fn receive(&self, frame: &[u8]) -> bool {
            use netwire::{eth, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN, MAX_FRAME};
            if super::locked(&STATE).refuse || frame.len() < ETH_HDR_LEN || frame.len() > MAX_FRAME {
                return false;
            }
            super::locked(&super::HV0_IN).push(frame.to_vec());
            let ipv4 = eth::ether_type(frame) == ETH_TYPE_IP && frame.len() >= ETH_HDR_LEN + IP_HDR_LEN;
            if !(ipv4 && crate::nat::intercept(&crate::device::HV0, frame)) {
                super::locked(&super::STACK).push(frame.to_vec());
            }
            true
        }
    }

    pub struct Attached {
        _private: (),
    }

    impl Drop for Attached {
        fn drop(&mut self) {
            super::locked(&STATE).sink = None;
        }
    }

    /// The sink attached to `hv0`, if any.
    pub fn sink() -> Option<Arc<dyn VnicSink>> {
        super::locked(&STATE).sink.clone()
    }

    pub fn set_refuse(refuse: bool) {
        super::locked(&STATE).refuse = refuse;
    }

    pub(crate) fn reset() {
        let mut s = super::locked(&STATE);
        s.sink = None;
        s.refuse = false;
    }
}

/// The VM a switch port belongs to, as the switch wakes it.
pub mod vms {
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    pub struct Shared {
        pub woken: AtomicU64,
    }

    impl Shared {
        pub fn wake_up(&self) {
            self.woken.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What reached the stack's own protocols through `hv0` -- not NAT's.
static STACK: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// Everything `hv0` took in: the stack's and NAT's.
static HV0_IN: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Frames that reached the stack's own protocols since last asked.
pub fn take_stack() -> Vec<Vec<u8>> {
    core::mem::take(&mut *locked(&STACK))
}

/// Frames `hv0` took in since last asked, NAT's among them.
pub fn take_hv0_in() -> Vec<Vec<u8>> {
    core::mem::take(&mut *locked(&HV0_IN))
}

/// Everything back as a boot left it: NAT off, no sink, no frames, ARP
/// knowing nothing, the devices unset.
pub fn reset() {
    if let Some(nat) = abi::nat() {
        nat.disable(&device::HV0);
    }
    vnic::reset();
    for d in [&device::HV0, &device::ETH0, &device::SPARE] {
        d.set([0; 6], 0, 0, 0);
    }
    locked(&STACK).clear();
    locked(&HV0_IN).clear();
    if let Some(arp) = abi::arp_table() {
        locked(&arp.known).clear();
        locked(&arp.asked).clear();
    }
    dns::UPSTREAM.store(0, std::sync::atomic::Ordering::Relaxed);
}

/// ARP told `ip` is at `mac`.
pub fn arp_learn(ip: u32, mac: Mac) {
    if let Some(arp) = abi::arp_table() {
        locked(&arp.known).insert(ip, mac);
    }
}
