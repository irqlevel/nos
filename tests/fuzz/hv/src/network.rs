//! What the guests' network reaches of the kernel: the net layer's devices,
//! frames, ARP and NAT singleton (as `layers/net/src/nat.rs` reaches them), the
//! module's view of them (`kcore::net`, `kcore::vnic`, as the switch in
//! `modules/hypervisor/hv/src/net.rs` does), and the VM a port wakes.
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
        /// The public addresses NAT said were at the outer device, as it
        /// mapped them (`announce`).
        pub(crate) announced: Mutex<Vec<u32>>,
    }

    impl Arp {
        pub fn announce(&self, _nic: &crate::nic::Nic, ip: u32) {
            super::locked(&self.announced).push(ip);
        }
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
        Some(ARP.get_or_init(|| Arp { known: Mutex::new(HashMap::new()), asked: Mutex::new(Vec::new()),
                                      announced: Mutex::new(Vec::new()) }))
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
/// off through the net crate's own exports, as the kernel's does it; and
/// what the tunnel takes of it -- a host's uplink (`crate::underlay`), a
/// frame of the pool, a listener and the frames it is lent.
pub mod net {
    use std::sync::Arc;

    /// hv's switch, and what it carries by, as `tunnel.rs` names them:
    /// `crate::net` is the switch in the module, and `kcore::net` here --
    /// one path for the two.
    pub use crate::switch::{Carrier, Switch};

    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct NatOn {
        pub code: i32,
        pub outer: usize,
    }

    /// What `kernel_net_nat_enable` answers in its `code`: ffi's numbers,
    /// which the net crate's export reads from `ffi::net` (here, this).
    pub const NAT_ON: i32 = 0;
    pub const NAT_NO_UPLINK: i32 = 1;
    pub const NAT_DEVICES: i32 = 2;
    pub const NAT_BUSY: i32 = 3;
    pub const NAT_NO_MEMORY: i32 = 4;

    /// What `kernel_net_nat_map` and `kernel_net_nat_inside` answer: ffi's.
    pub const NAT_MAPPED: i32 = 0;
    pub const NAT_MAP_OFF: i32 = 1;
    pub const NAT_MAP_TAKEN: i32 = 2;
    pub const NAT_MAP_BAD: i32 = 3;
    pub const NAT_MAP_FULL: i32 = 4;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum MapError {
        Off,
        Taken,
        Bad,
        Full,
    }

    impl MapError {
        fn of(code: i32) -> MapError {
            match code {
                NAT_MAP_OFF => MapError::Off,
                NAT_MAP_TAKEN => MapError::Taken,
                NAT_MAP_FULL => MapError::Full,
                _ => MapError::Bad,
            }
        }

        pub fn name(self) -> &'static str {
            match self {
                MapError::Off => "NAT is off",
                MapError::Taken => "the address is mapped already",
                MapError::Bad => "not an address that may be mapped",
                MapError::Full => "as many are mapped as may be",
            }
        }
    }

    /// `kcore::net::OneToOne`: unmapped when dropped.
    pub struct OneToOne {
        inner: usize,
        public: u32,
    }

    impl OneToOne {
        pub fn public(&self) -> u32 {
            self.public
        }
    }

    impl Drop for OneToOne {
        fn drop(&mut self) {
            crate::nat::kernel_net_nat_unmap(self.inner, self.public);
        }
    }

    /// `kcore::net::Inside`: behind the inner device no longer when dropped.
    pub struct Inside {
        inner: usize,
        net: u32,
        mask: u32,
    }

    impl Drop for Inside {
        fn drop(&mut self) {
            crate::nat::kernel_net_nat_outside(self.inner, self.net, self.mask);
        }
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
            if let Some(u) = crate::underlay::get(self.handle) {
                return u.ip;
            }
            crate::device::DEVICES.by_handle(self.handle).map_or(0, |d| d.ip())
        }
        pub fn mac(&self) -> [u8; 6] {
            if let Some(u) = crate::underlay::get(self.handle) {
                return u.mac;
            }
            crate::device::DEVICES.by_handle(self.handle).map_or([0; 6], |d| d.mac())
        }

        /// The room its transmit queue has.
        pub fn tx_room(&self) -> usize {
            crate::underlay::get(self.handle).map_or(0, |u| u.room.load(std::sync::atomic::Ordering::Relaxed))
        }

        /// ARP's answer for `ip`: a look that waits, so never with a lock.
        pub fn resolve(&self, ip: u32) -> Option<[u8; 6]> {
            invariant!(crate::sync::held() == 0, "an ARP look, which waits, with {} of the kernel's locks held",
                       crate::sync::held());
            crate::underlay::hop(ip)
        }

        /// A listener on `port`, which the target lends the datagrams to.
        pub fn listen<H: UdpHandler>(&self, port: u16, handler: Arc<H>) -> Result<UdpListener, ListenError> {
            let Some(u) = crate::underlay::get(self.handle) else { return Err(ListenError::Invalid) };
            let mut l = u.listener.lock().unwrap_or_else(|e| e.into_inner());
            if l.is_some() {
                return Err(ListenError::PortTaken);
            }
            let id = crate::underlay::next_listener();
            *l = Some((port, id, handler));
            Ok(UdpListener { handle: self.handle, id })
        }

        /// A frame out of the uplink: refused where it has no room. The
        /// tunnel transmits with no lock of its own held.
        pub fn transmit(&self, frame: NetFrame) -> bool {
            invariant!(crate::sync::held() == 0, "a transmit with {} of the kernel's locks held",
                       crate::sync::held());
            let Some(u) = crate::underlay::get(self.handle) else { return false };
            if u.room.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                return false;
            }
            u.sent.lock().unwrap_or_else(|e| e.into_inner()).push(frame.data().to_vec());
            true
        }
        pub fn nat(&self) -> core::result::Result<Nat, NatError> {
            let on = crate::nat::kernel_net_nat_enable(0, self.handle);
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
        pub fn forget(&self, ip: u32) -> usize {
            crate::nat::kernel_net_nat_forget(self.inner, ip) as usize
        }
        pub fn map(&self, public: u32, inner_ip: u32, mac: [u8; 6], mtu: u16) -> Result<OneToOne, MapError> {
            let word = u64::from_be_bytes([0, 0, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]]);
            match crate::nat::kernel_net_nat_map(self.inner, public, inner_ip, word, u32::from(mtu)) {
                NAT_MAPPED => Ok(OneToOne { inner: self.inner, public }),
                code => Err(MapError::of(code)),
            }
        }
        pub fn inside(&self, net: u32, mask: u32, gw: u32) -> Result<Inside, MapError> {
            match crate::nat::kernel_net_nat_inside(self.inner, net, mask, gw) {
                NAT_MAPPED => Ok(Inside { inner: self.inner, net: net & mask, mask }),
                code => Err(MapError::of(code)),
            }
        }
    }

    impl Drop for Nat {
        fn drop(&mut self) {
            crate::nat::kernel_net_nat_disable(self.inner);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ListenError {
        PortTaken,
        TableFull,
        Invalid,
    }

    /// What a listener is lent a datagram's frame by.
    pub trait UdpHandler: Send + Sync + 'static {
        fn on_frame(&self, frame: Lent<'_>, rx: &mut RxContext);
        fn on_batch_end(&self, _rx: &mut RxContext) {}
    }

    /// A frame lent for one call.
    pub struct Lent<'a> {
        bytes: &'a [u8],
    }

    impl<'a> Lent<'a> {
        /// The target's: `bytes` lent, as the receive path lends a frame.
        pub fn lend(bytes: &'a [u8]) -> Lent<'a> {
            Lent { bytes }
        }
        pub fn bytes(&self) -> &[u8] {
            self.bytes
        }
        /// The frame kept past the call: its bytes, to open where they lie.
        pub fn retain(self) -> NetFrame {
            NetFrame { buf: self.bytes.to_vec(), len: self.bytes.len() }
        }
    }

    pub struct RxContext {
        _only_the_receive_pass: (),
    }

    impl RxContext {
        /// The target's: the receive pass's own, for one call.
        pub fn for_the_target() -> RxContext {
            RxContext { _only_the_receive_pass: () }
        }
    }

    /// A port listened on: given back when dropped -- which waits out a
    /// receive still running, so never with a lock.
    pub struct UdpListener {
        handle: usize,
        id: u64,
    }

    impl Drop for UdpListener {
        fn drop(&mut self) {
            invariant!(crate::sync::held() == 0, "a listener dropped -- which waits -- with {} of the kernel's locks \
                       held", crate::sync::held());
            if let Some(u) = crate::underlay::get(self.handle) {
                let mut l = u.listener.lock().unwrap_or_else(|e| e.into_inner());
                if l.as_ref().is_some_and(|(_, id, _)| *id == self.id) {
                    let gone = l.take();
                    drop(l);
                    drop(gone);
                }
            }
        }
    }

    /// A frame of the pool: room for what was asked, 2048 bytes at most,
    /// none while the pool is dry -- and never taken with a lock held.
    pub struct NetFrame {
        buf: Vec<u8>,
        len: usize,
    }

    impl NetFrame {
        pub fn alloc_tx(data_len: usize) -> Option<NetFrame> {
            invariant!(crate::sync::held() == 0, "a frame taken from the pool with {} of the kernel's locks held",
                       crate::sync::held());
            if crate::underlay::dry() || data_len > 2048 {
                return None;
            }
            Some(NetFrame { buf: vec![0; data_len], len: 0 })
        }
        pub fn data(&self) -> &[u8] {
            &self.buf[..self.len]
        }
        pub fn data_mut(&mut self) -> &mut [u8] {
            &mut self.buf[..self.len]
        }
        pub fn data_raw_mut(&mut self, capacity: usize) -> &mut [u8] {
            let n = capacity.min(self.buf.len());
            &mut self.buf[..n]
        }
        pub fn len(&self) -> usize {
            self.len
        }
        pub fn set_len(&mut self, len: usize) {
            self.len = len.min(self.buf.len());
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// `kcore::vnic::BACKLOG`, as the switch shares it out.
    pub const BACKLOG: usize = 256;

    /// The stack's passes over `hv0`'s backlog: one each time the target
    /// takes what `hv0` was handed (`take_hv0_in`).
    pub static DRAINS: AtomicU64 = AtomicU64::new(0);

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
        pub fn drains(&self) -> u64 {
            DRAINS.load(Ordering::Relaxed)
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
        /// (`layers/net/src/device.rs`), and else -- and anything else -- the
        /// stack's own, kept too. False when the stack would not take it.
        pub fn receive(&self, frame: &[u8]) -> bool {
            use netwire::{eth, ETH_HDR_LEN, ETH_TYPE_IP, IP_HDR_LEN, MAX_FRAME};
            if super::locked(&STATE).refuse || frame.len() < ETH_HDR_LEN || frame.len() > MAX_FRAME {
                return false;
            }
            super::locked(&super::HV0_IN).push(frame.to_vec());
            if super::HANDED_KEEP.load(std::sync::atomic::Ordering::Relaxed) {
                super::locked(&super::HANDED).push((crate::ipc::current(), frame.to_vec()));
            }
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

    impl crate::switch::Wake for Shared {
        fn wake_up(&self) {
            Shared::wake_up(self);
        }
    }
}

/// What reached the stack's own protocols through `hv0` -- not NAT's.
static STACK: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// Everything `hv0` took in: the stack's and NAT's.
static HV0_IN: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// What was handed in to `hv0`, by the host whose code handed it
/// (`crate::ipc::current`), while a target keeps it: one `hv0` stands for
/// every host's here, and who handed a frame in is what tells which host's
/// way out it took.
static HANDED: Mutex<Vec<(usize, Vec<u8>)>> = Mutex::new(Vec::new());
static HANDED_KEEP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn keep_handed(on: bool) {
    HANDED_KEEP.store(on, std::sync::atomic::Ordering::Relaxed);
    locked(&HANDED).clear();
}

pub fn take_handed() -> Vec<(usize, Vec<u8>)> {
    core::mem::take(&mut *locked(&HANDED))
}

/// Frames that reached the stack's own protocols since last asked.
pub fn take_stack() -> Vec<Vec<u8>> {
    core::mem::take(&mut *locked(&STACK))
}

/// Frames `hv0` took in since last asked, NAT's among them: a pass of the
/// stack's over its backlog, which the switch's shares count from.
pub fn take_hv0_in() -> Vec<Vec<u8>> {
    vnic::DRAINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    core::mem::take(&mut *locked(&HV0_IN))
}

/// Everything back as a boot left it: NAT off, no sink, no frames, ARP
/// knowing nothing, the devices unset.
pub fn reset() {
    if let Some(nat) = abi::nat() {
        nat.disable(&device::HV0);
        /* What `nat allow` let through outlives NAT going off, as the
         * host's word does: taken back, for the next input to start as
         * the machine does. */
        let mut nets = [(0, 0); crate::nat::ALLOWED_MAX];
        let n = nat.allowed(&mut nets);
        for &(net, mask) in &nets[..n] {
            nat.disallow(net, mask);
        }
    }
    vnic::reset();
    for d in [&device::HV0, &device::ETH0, &device::SPARE] {
        d.set([0; 6], 0, 0, 0);
    }
    locked(&STACK).clear();
    keep_handed(false);
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
