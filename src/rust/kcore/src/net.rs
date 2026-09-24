//! The network layer, **as a loadable module reaches it**: a device by name,
//! a UDP port to listen on, frames to build and transmit.
//!
//! A module is linked on its own, so the C ABI is the only seam it shares
//! with the layer, and these are its wrappers. Nothing inside the kernel
//! image comes through here: a NIC's driver registers with the `net` crate
//! as a `net::NetDriver` and moves `net::Frame`s, and the layer's own
//! services hold a `net::Nic`. What is at the bottom -- the kernel's
//! command-line parameters and its log -- is C++'s, and the layer asks for
//! it here like any other kernel service.
//!
//! A module that serves a UDP port implements [`UdpHandler`] and hands it to
//! [`Nic::listen`]: frames arrive as a [`Lent`], the end of a receive batch
//! is told, and what only the receive path touches -- the replies a batch
//! gathers, a [`TxBatch`] -- sits in an [`RxOwned`], reached with the
//! [`RxContext`] each call is lent. No pointer is cast and no lock is taken
//! on the way, which is the point: this is the path a load test measures.

use alloc::sync::Arc;
use core::marker::PhantomData;

use ffi::net;

pub use ffi::net::RxStats;

/// A network device already in the kernel's table -- `eth0` -- for a service
/// that sends and receives over it rather than drives it. Devices live as
/// long as the kernel does, so a Nic holds nothing and copies freely.
#[derive(Clone, Copy)]
pub struct Nic {
    handle: usize,
}

/// Why a UDP listener was refused
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenError {
    /// Someone has the port already -- the UDP shell, DHCP, another server.
    PortTaken,
    /// The device's listener table is full.
    TableFull,
    /// Port 0.
    Invalid,
}

impl ListenError {
    /// What `kernel_net_udp_listen` answered, when it was not 0.
    fn of(code: i32) -> Self {
        match code {
            1 => ListenError::PortTaken,
            2 => ListenError::TableFull,
            _ => ListenError::Invalid,
        }
    }
}

/// What a module's UDP listener hands datagrams to. `Send + Sync`, because
/// the receive pass runs on whichever CPU the soft IRQ does while a task of
/// the module's reads the same handler's counters.
pub trait UdpHandler: Send + Sync + 'static {
    /// One datagram's frame, lent for the length of the call.
    fn on_frame(&self, frame: Lent<'_>, rx: &mut RxContext);

    /// The end of a receive batch, whether or not it had anything for this
    /// port: the moment to hand the NIC what was built.
    fn on_batch_end(&self, _rx: &mut RxContext) {}
}

/// A frame the receive path lends a listener for the length of one call.
pub struct Lent<'a> {
    handle: usize,
    _for_the_call: PhantomData<&'a [u8]>,
}

impl Lent<'_> {
    /// The frame's bytes, Ethernet header first.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        /* The receive path holds the frame for the whole call, which
         * outlives this borrow, and does not write it meanwhile. */
        unsafe { NetFrame::lent(self.handle) }
    }

    /// The frame, to keep past the call -- to answer in, where it lies. The
    /// receive path's own reference is then not the last, and it never looks
    /// at the bytes again.
    #[inline]
    pub fn retain(self) -> NetFrame {
        unsafe { NetFrame::retain(self.handle) }
    }
}

/// The kernel runs the receive soft IRQ on one CPU at a time, and a
/// listener is called from its pass and from nowhere else. Each call is lent
/// one of these, so holding it is what it means to be the only code on the
/// receive path right now -- which is what lets [`RxOwned`] hand out its
/// contents without a lock. Only the two functions the kernel calls a typed
/// listener through make one.
pub struct RxContext {
    _only_the_receive_pass_makes_one: (),
}

/// What only the receive path touches: the replies a listener gathers
/// during a batch. No lock, because the [`RxContext`] borrowed to reach in
/// is the proof nobody else is there.
pub struct RxOwned<T>(core::cell::UnsafeCell<T>);

/* Reached on whichever CPU the receive soft IRQ runs on, one at a time. */
unsafe impl<T: Send> Sync for RxOwned<T> {}

impl<T> RxOwned<T> {
    pub const fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    #[inline]
    pub fn get<'a>(&'a self, _rx: &'a mut RxContext) -> &'a mut T {
        /* There is one `RxContext` at a time and it is borrowed for as long
         * as what is returned here lives. */
        unsafe { &mut *self.0.get() }
    }
}

/* What the kernel calls a typed listener through. `ctx` is the hold
 * `Nic::listen` took on the handler, which the listener's drop gives up only
 * after `kernel_net_udp_unlisten` has waited out every call of these. */

extern "C" fn on_frame<H: UdpHandler>(ctx: *mut u8, frame: usize) {
    let handler = unsafe { &*(ctx as *const H) };
    let mut rx = RxContext { _only_the_receive_pass_makes_one: () };
    handler.on_frame(Lent { handle: frame, _for_the_call: PhantomData }, &mut rx);
}

extern "C" fn on_batch_end<H: UdpHandler>(ctx: *mut u8) {
    let handler = unsafe { &*(ctx as *const H) };
    let mut rx = RxContext { _only_the_receive_pass_makes_one: () };
    handler.on_batch_end(&mut rx);
}

/// # Safety
/// `ctx` is an `Arc<H>` turned into a word by `Nic::listen`, given up once.
unsafe fn release<H>(ctx: usize) {
    drop(unsafe { Arc::from_raw(ctx as *const H) });
}

impl Nic {
    pub fn find(name: &str) -> Option<Self> {
        let handle = unsafe { net::kernel_net_find(name.as_ptr(), name.len()) };
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    /// The device a handle names: what a call arriving from outside
    /// carries, `kernel_net_find`'s answer passed on. Any word will do: the
    /// device table looks up every handle it is given, and one it never gave
    /// out names no device -- whatever is asked of it answers nothing.
    pub fn from_handle(handle: usize) -> Option<Self> {
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    /// Its address, host byte order; 0 until it has one.
    #[inline]
    pub fn ip(&self) -> u32 {
        net::kernel_net_ip(self.handle)
    }

    pub fn mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        unsafe { net::kernel_net_mac(self.handle, mac.as_mut_ptr()) };
        mac
    }

    /// The device, for the wrappers of other kernel calls that take one
    /// (`tcp::TcpListener::bind`).
    pub(crate) fn handle(&self) -> usize {
        self.handle
    }

    /// Every UDP datagram to `port`, handed to `handler` from the receive
    /// softirq, and the end of every receive batch with it. Refused for a
    /// port someone else has. The listener holds the handler for as long as
    /// it lives, and goes -- with its hold -- when the returned handle is
    /// dropped, once any call still running has returned.
    ///
    /// The handler runs on the receive path of every packet the machine
    /// gets: nothing that sleeps, and nothing long.
    pub fn listen<H: UdpHandler>(
        &self, port: u16, handler: Arc<H>,
    ) -> core::result::Result<UdpListener, ListenError> {
        /* The listener's own hold on the handler, as the word the kernel
         * hands back to the two functions below. `release` is what gives it
         * up: here, if the kernel takes no listener, or in the drop. */
        let ctx = Arc::into_raw(handler) as *mut u8;

        let code = unsafe {
            net::kernel_net_udp_listen(
                self.handle, port, on_frame::<H>, Some(on_batch_end::<H>), ctx)
        };
        if code != 0 {
            unsafe { release::<H>(ctx as usize) };
            return Err(ListenError::of(code));
        }
        Ok(UdpListener { nic: *self, port, ctx: ctx as usize, release: release::<H> })
    }

    /// How many more frames the transmit queue has room for right now.
    /// `transmit` and `TxBatch::send` release what finds no room; a sender
    /// that asks first, and builds no more than the answer, loses nothing
    /// that way while it is the only one sending.
    #[inline]
    pub fn tx_room(&self) -> usize {
        net::kernel_net_tx_room(self.handle)
    }

    /// Where a frame to `ip` (host byte order) goes on the wire: the address
    /// itself on this device's subnet, the gateway off it, as ARP answers.
    /// What a sender that builds its own frames asks once, before the first.
    /// Task context: a cache miss sends a request and sleeps for the answer,
    /// up to three seconds. None when nothing answered.
    pub fn resolve(&self, ip: u32) -> Option<[u8; 6]> {
        let answer = net::kernel_net_resolve(self.handle, ip);
        if answer.found != 0 { Some(answer.mac) } else { None }
    }

    /// Queues a frame to transmit; false when the queue had no room and it
    /// was dropped.
    #[inline]
    pub fn transmit(&self, frame: NetFrame) -> bool {
        let handle = frame.into_raw();
        unsafe { net::kernel_net_submit_tx(self.handle, &handle, 1) == 1 }
    }

    /// Queues a run of frames -- one lock and one doorbell for the lot --
    /// from any context. Takes every one; returns how many were queued, the
    /// rest dropped. `TxBatch::send` is how anything outside this file
    /// reaches it.
    ///
    /// # Safety
    /// Each is a frame handle the caller owns (`NetFrame::into_raw`) and
    /// gives up here.
    #[inline]
    unsafe fn transmit_raw(&self, frames: &[usize]) -> usize {
        if frames.is_empty() {
            return 0;
        }
        unsafe { net::kernel_net_submit_tx(self.handle, frames.as_ptr(), frames.len()) }
    }
}

/// Why NAT would not go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatError {
    /// No device has a gateway to go out through: no lease yet, or none of
    /// them routes anywhere.
    NoUplink,
    /// This device will not do: it has no address, or names none.
    Device,
    /// It is on already: there is one at a time.
    Busy,
    NoMemory,
}

/// NAT from a device out through the machine's default route, for as long
/// as this lives: `Nic::nat`.
pub struct Nat {
    inner: usize,
    outer: Nic,
}

impl Nic {
    /// NAT on: what is behind this device -- a virtual NIC's guests --
    /// reaches the world through the device the default route is on, from
    /// that device's address, until the returned value is dropped. One at a
    /// time in the whole kernel. Task context: it allocates its table.
    pub fn nat(&self) -> core::result::Result<Nat, NatError> {
        let on = net::kernel_net_nat_enable(self.handle);
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
    /// The device it goes out through.
    pub fn outer(&self) -> Nic {
        self.outer
    }
}

impl Drop for Nat {
    fn drop(&mut self) {
        net::kernel_net_nat_disable(self.inner);
    }
}

/// A UDP port listened on, from `Nic::listen`; given back on drop, once
/// no call of its callback is still running -- so what the callback reaches
/// may go right after. Task context: the drop may wait.
pub struct UdpListener {
    nic: Nic,
    port: u16,
    /* what it was registered with, which is what takes away this listener
       and nobody else's on the port */
    ctx: usize,
    /* what gives up the listener's hold on its handler: last of all */
    release: unsafe fn(usize),
}

/* A handle -- a device, a port and the word it was registered with -- and
 * the kernel's listener table is what it names. */
unsafe impl Send for UdpListener {}

impl Drop for UdpListener {
    fn drop(&mut self) {
        net::kernel_net_udp_unlisten(self.nic.handle, self.port, self.ctx as *mut u8);
        /* No call is running and none will start: the handler may go. */
        unsafe { (self.release)(self.ctx) };
    }
}

/// Frames gathered to go to the NIC together -- at most `N`, with one lock
/// and one doorbell for the lot: the replies a listener builds during a
/// receive batch, or what a sender has made since it last handed over.
pub struct TxBatch<const N: usize> {
    /* each a frame reference this holds, as the word the kernel takes */
    frames: [usize; N],
    len: usize,
}

impl<const N: usize> TxBatch<N> {
    pub const fn new() -> Self {
        Self { frames: [0; N], len: 0 }
    }

    #[inline]
    pub fn is_full(&self) -> bool {
        self.len >= N
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Takes the frame. False, and the frame released, when there is no room.
    #[inline]
    pub fn push(&mut self, frame: NetFrame) -> bool {
        if self.is_full() {
            return false;
        }
        self.frames[self.len] = frame.into_raw();
        self.len += 1;
        true
    }

    /// Everything gathered, released unsent.
    pub fn clear(&mut self) {
        for &handle in &self.frames[..self.len] {
            drop(unsafe { NetFrame::from_raw(handle) });
        }
        self.len = 0;
    }

    /// Everything gathered, to the device: how many it queued. The rest it
    /// releases, and the batch is empty either way.
    #[inline]
    pub fn send(&mut self, nic: &Nic) -> usize {
        /* Each is a reference `push` took over, given up here. */
        let queued = unsafe { nic.transmit_raw(&self.frames[..self.len]) };
        self.len = 0;
        queued
    }
}

impl<const N: usize> Drop for TxBatch<N> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Reference-counted network frame buffer.
/// `Drop` calls `kernel_netframe_put`, which frees the frame when the
/// refcount reaches zero.
pub struct NetFrame {
    /// Never zero, so that a slot that may hold a frame -- a ring's shadow of
    /// what is posted -- is no bigger than the frame's own word
    handle: core::num::NonZeroUsize,
}

impl NetFrame {
    #[inline]
    fn raw(&self) -> usize {
        self.handle.get()
    }

    /// A frame to transmit, room for `data_len` bytes: from the frame pool --
    /// a per-CPU cache, no allocator -- whenever it fits one. `len()` is 0
    /// until `set_len`.
    #[inline]
    pub fn alloc_tx(data_len: usize) -> Option<Self> {
        let h = net::kernel_netframe_alloc_tx(data_len);
        core::num::NonZeroUsize::new(h).map(|handle| Self { handle })
    }

    /// A reference of the caller's own to a frame the kernel lent: what
    /// `Lent::retain` is made of.
    ///
    /// # Safety
    /// `handle` must be a frame alive for the call -- the one a listener was
    /// handed.
    #[inline]
    unsafe fn retain(handle: usize) -> Self {
        unsafe { net::kernel_netframe_get(handle) };
        Self { handle: Self::word(handle) }
    }

    /// The bytes of a frame the kernel lent, without taking it: what
    /// `Lent::bytes` is made of.
    ///
    /// # Safety
    /// `handle` must be a frame that outlives the slice and that nobody
    /// writes meanwhile.
    #[inline]
    unsafe fn lent<'a>(handle: usize) -> &'a [u8] {
        let ptr = unsafe { net::kernel_netframe_data(handle) };
        let len = unsafe { net::kernel_netframe_len(handle) };
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    /// Slice of the received/transmitted data (length = `self.len()`).
    ///
    /// Note: for a freshly allocated RX frame `len()` is 0 until `set_len` is
    /// called. Use `data_raw_mut(capacity)` to access the full buffer before
    /// the length is known (e.g. for memcpy-based drivers).
    #[inline]
    pub fn data(&self) -> &[u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.raw()) };
        let len = unsafe { net::kernel_netframe_len(self.raw()) };
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    /// Mutable slice of the received/transmitted data (length = `self.len()`).
    /// See `data()` for the note on freshly allocated RX frames.
    #[inline]
    pub fn data_mut(&mut self) -> &mut [u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.raw()) };
        let len = unsafe { net::kernel_netframe_len(self.raw()) };
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Mutable slice of the allocated buffer, up to `capacity` bytes of it --
    /// fewer if the frame has room for fewer.
    ///
    /// Use this when you need to write into a freshly allocated frame before
    /// calling `set_len`.
    #[inline]
    pub fn data_raw_mut(&mut self, capacity: usize) -> &mut [u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.raw()) };
        let room = unsafe { net::kernel_netframe_capacity(self.raw()) };
        unsafe { core::slice::from_raw_parts_mut(ptr, capacity.min(room)) }
    }

    /// Physical address of the data buffer (for DMA descriptor programming).
    #[inline]
    pub fn data_phys(&self) -> u64 {
        unsafe { net::kernel_netframe_data_phys(self.raw()) }
    }

    /// Current valid data length (0 for a freshly allocated RX frame).
    #[inline]
    pub fn len(&self) -> usize {
        unsafe { net::kernel_netframe_len(self.raw()) }
    }

    #[inline]
    pub fn set_len(&mut self, len: usize) {
        unsafe { net::kernel_netframe_set_len(self.raw(), len) }
    }

    /// The frame as the word the kernel takes one by, its reference with it:
    /// given to `kernel_net_submit_tx`, or taken back by `from_raw`. Nothing
    /// outside this file needs the word -- a frame goes out through
    /// `Nic::transmit` or a `TxBatch`.
    #[inline]
    fn into_raw(self) -> usize {
        let h = self.raw();
        core::mem::forget(self);
        h
    }

    /// The frame `into_raw` made a word of.
    ///
    /// # Safety
    /// `handle` came from `into_raw` and is not used again.
    #[inline]
    unsafe fn from_raw(handle: usize) -> Self {
        Self { handle: Self::word(handle) }
    }

    /// A frame's word. Zero is the kernel's "no frame" and never a frame:
    /// a caller that passes it has broken what `retain` and `from_raw` ask,
    /// and is told so rather than left holding a frame that is not one.
    #[inline]
    fn word(handle: usize) -> core::num::NonZeroUsize {
        core::num::NonZeroUsize::new(handle).expect("a frame handle is never 0")
    }
}

impl Drop for NetFrame {
    fn drop(&mut self) {
        unsafe { net::kernel_netframe_put(self.raw()) }
    }
}

/// Whether the receive path is keeping up, summed over every device: frames
/// the pool could not supply, frames out of it now, and how often the tick
/// had to find the frames an interrupt should have announced.
pub fn rx_stats() -> RxStats {
    net::kernel_net_rx_stats()
}

/// The DNS server this machine was given -- its resolver's, or its DHCP
/// lease's -- host byte order. None when it was given none.
pub fn dns_server() -> Option<u32> {
    let ip = net::kernel_net_dns_server();
    if ip == 0 { None } else { Some(ip) }
}

/// `dhcp=off`: the kernel was told not to run a DHCP client.
pub fn dhcp_off() -> bool {
    net::kernel_param_dhcp_off() != 0
}

/// `dns=on`: a lease's DNS server is worth starting a resolver on.
pub fn dns_on() -> bool {
    net::kernel_param_dns_on() != 0
}

/// `rxpoll=on`: the tick is to look at the receive path as well as the
/// NIC's own interrupt. Off unless asked for.
pub fn rx_poll_on() -> bool {
    net::kernel_param_rxpoll_on() != 0
}

/// `netconsole=ip:port` and `nctail=N`, off the kernel command line: the
/// collector's address (host byte order), its port, and the backlog cap in
/// KiB. None when no netconsole was asked for.
pub fn netconsole_params() -> Option<(u32, u16, usize)> {
    let (mut ip, mut port, mut tail_kb) = (0u32, 0u16, 0usize);
    if unsafe { net::kernel_netconsole_params(&mut ip, &mut port, &mut tail_kb) } == 0 {
        None
    } else {
        Some((ip, port, tail_kb))
    }
}

/// Every message the kernel log already holds, oldest first, handed to
/// `line` one at a time.
pub fn replay_kernel_log(line: &mut dyn FnMut(&[u8])) {
    extern "C" fn each(ctx: *mut u8, s: *const u8, len: usize) {
        if s.is_null() || len == 0 {
            return;
        }
        /* `ctx` is the `&mut dyn FnMut` below, alive for the whole replay,
         * and the kernel calls back on this same stack. */
        let line = unsafe { &mut *(ctx as *mut &mut dyn FnMut(&[u8])) };
        line(unsafe { core::slice::from_raw_parts(s, len) });
    }

    let mut line = line;
    let ctx = &mut line as *mut &mut dyn FnMut(&[u8]) as *mut u8;
    unsafe { net::kernel_dmesg_replay(each, ctx) };
}
