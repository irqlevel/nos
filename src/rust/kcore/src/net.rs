use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use ffi::net;

/* ---- the driver's side ---- */

/// What a device's name fits in, its terminator included.
const NAME_MAX: usize = 32;

/// A registered net device, as its driver knows it. Registration is for the
/// life of the kernel, so this is a plain value.
#[derive(Clone, Copy)]
pub struct NetDeviceHandle {
    handle: usize,
}

impl NetDeviceHandle {
    pub fn set_ip(&self, ip: u32) {
        unsafe { net::kernel_netdev_set_ip(self.handle, ip) }
    }

    pub fn set_mask(&self, mask: u32) {
        unsafe { net::kernel_netdev_set_mask(self.handle, mask) }
    }

    pub fn set_gw(&self, gw: u32) {
        unsafe { net::kernel_netdev_set_gw(self.handle, gw) }
    }
}

/// A network card, as the driver behind it. The net layer asks a driver two
/// things, and each comes with the state that only that call touches:
///
/// - `flush_tx`, with the device's transmit lock held around it, so one at a
///   time per device: it is handed the transmit side as `&mut Self::Tx`;
/// - `process_rx`, from the receive soft IRQ, which the kernel runs on one
///   CPU at a time: it is handed the receive side as `&mut Self::Rx`.
///
/// Everything else of the driver -- what an interrupt handler or a shell
/// command looks at -- is `&self`, shared between whatever runs: registers,
/// atomics, locks. So a driver has no `UnsafeCell` with a comment saying who
/// may touch it; who may touch it is who is handed it.
pub trait NetDriver: Sync + 'static {
    /// What only `flush_tx` touches: the transmit ring and what is on it.
    type Tx: Send + 'static;
    /// What only `process_rx` touches: the receive ring and what is posted.
    type Rx: Send + 'static;

    /// Give back what the hardware has finished sending and send what the
    /// stack has queued. Called under the device's transmit lock, interrupts
    /// off: no sleeping, no allocating -- and no *freeing*: a frame finished
    /// with goes to `queue.done`, never out of scope.
    fn flush_tx(&'static self, tx: &mut Self::Tx, queue: &mut TxQueue<'_>);

    /// Take what the hardware has received and hand it up.
    fn process_rx(&'static self, rx: &mut Self::Rx, queue: &mut RxQueue<'_>);
}

/// A driver and the two halves only its calls touch, together for good.
/// Made first, so that the driver is somewhere an interrupt handler can be
/// pointed at; registered last, once the hardware is ready to be asked.
pub struct NetBinding<D: NetDriver> {
    driver: D,
    tx: UnsafeCell<D::Tx>,
    rx: UnsafeCell<D::Rx>,
    registered: AtomicBool,
}

/* `tx` and `rx` are reached only by the two functions at the bottom of this
 * block, each under the exclusion the net layer promises a driver. */
unsafe impl<D: NetDriver> Sync for NetBinding<D> {}

impl<D: NetDriver> NetBinding<D> {
    /// For the life of the kernel: a net device is never given back.
    pub fn new(driver: D, tx: D::Tx, rx: D::Rx) -> &'static NetBinding<D> {
        Box::leak(Box::new(NetBinding {
            driver,
            tx: UnsafeCell::new(tx),
            rx: UnsafeCell::new(rx),
            registered: AtomicBool::new(false),
        }))
    }

    pub fn driver(&'static self) -> &'static D {
        &self.driver
    }

    /// Put the device in the net layer's table; from here on it is called.
    /// None when the table is full, the name will not do, or this binding is
    /// registered already -- one device to a binding, because one transmit
    /// lock is what stands behind its `&mut Tx`.
    pub fn register(&'static self, name: &str, mac: [u8; 6]) -> Option<NetDeviceHandle> {
        if name.is_empty() || name.len() >= NAME_MAX || name.as_bytes().contains(&0) {
            return None;
        }
        if self.registered.swap(true, Ordering::AcqRel) {
            return None;
        }

        /* The table copies the name: a C string for the length of the call. */
        let mut c_name = [0u8; NAME_MAX];
        c_name[..name.len()].copy_from_slice(name.as_bytes());

        let ops = net::NetDeviceOps {
            name: c_name.as_ptr(),
            mac,
            flush_tx: flush_tx::<D>,
            process_rx: process_rx::<D>,
            ctx: self as *const Self as *mut u8,
        };
        let h = unsafe { net::kernel_netdev_register(&ops) };
        if h == 0 {
            self.registered.store(false, Ordering::Release);
            None
        } else {
            Some(NetDeviceHandle { handle: h })
        }
    }
}

extern "C" fn flush_tx<D: NetDriver>(ctx: *mut u8, dev: usize) {
    /* `ctx` is the binding `register` passed, which lives for good. */
    let binding = unsafe { &*(ctx as *const NetBinding<D>) };
    /* The device's transmit lock is held around this call -- the net layer's
     * contract with a driver -- so nothing else is in `tx`. The one exception
     * is a panic's report, which comes through with the lock stolen if its
     * holder is never going to let go: see `Device::submit_tx`. */
    let tx = unsafe { &mut *binding.tx.get() };
    binding.driver.flush_tx(tx, &mut TxQueue { dev, _held: PhantomData });
}

extern "C" fn process_rx<D: NetDriver>(ctx: *mut u8, dev: usize) {
    let binding = unsafe { &*(ctx as *const NetBinding<D>) };
    /* Called from the receive soft IRQ and nowhere else, and the kernel runs
     * a soft IRQ type on one CPU at a time. */
    let rx = unsafe { &mut *binding.rx.get() };
    binding.driver.process_rx(rx, &mut RxQueue { dev, _life: PhantomData });
}

/// The stack's transmit queue, for the length of one `flush_tx`. It exists
/// only there, which is what its two calls need: the queue is guarded by the
/// lock that is held around `flush_tx`.
pub struct TxQueue<'a> {
    dev: usize,
    _held: PhantomData<&'a mut ()>,
}

impl TxQueue<'_> {
    /// The next frame to send, or None when the queue is empty.
    ///
    /// The frame wraps a DMA buffer the hardware is about to read: keep it --
    /// in the ring's shadow of what is posted -- until the hardware says it
    /// is done, and then hand it to `done`. Dropped earlier, the buffer goes
    /// back to the pool and out again while the card is still reading it.
    pub fn dequeue(&mut self) -> Option<NetFrame> {
        /* Inside `flush_tx`, which is what the call requires. */
        let h = unsafe { net::kernel_netdev_tx_dequeue(self.dev) };
        core::num::NonZeroUsize::new(h).map(|handle| NetFrame { handle })
    }

    /// A transmitted frame, for release once the lock is down. Never dropped
    /// here instead: dropping frees, a free can reach the page allocator,
    /// which shoots down the TLB on every other CPU and waits for each -- and
    /// a CPU spinning on this lock has interrupts off and never answers.
    pub fn done(&mut self, frame: NetFrame) {
        let h = frame.into_raw();
        unsafe { net::kernel_netdev_tx_done(self.dev, h) }
    }
}

/// The stack's receive queue, for the length of one `process_rx`.
pub struct RxQueue<'a> {
    dev: usize,
    _life: PhantomData<&'a mut ()>,
}

impl RxQueue<'_> {
    /// One received frame, to the stack. What the queue has no room for is
    /// released on the far side.
    pub fn enqueue(&mut self, frame: NetFrame) {
        let h = frame.into_raw();
        unsafe { net::kernel_netdev_enqueue_rx(self.dev, h) }
    }

    /// A whole harvest at once: the receive queue's lock is taken once for
    /// the batch rather than once per frame, which at tens of thousands of
    /// packets a second is the difference between a lock acquisition being
    /// noise and being the top of the receive path in a profile. The batch
    /// is empty after.
    pub fn deliver<const N: usize>(&mut self, batch: &mut FrameBatch<N>) {
        let count = core::mem::replace(&mut batch.count, 0);
        if count == 0 {
            return;
        }
        /* Each is a frame `push` took ownership of and gives up here. */
        unsafe {
            net::kernel_netdev_enqueue_rx_batch(self.dev, batch.frames.as_ptr(), count);
        }
    }
}

/* ---- the consuming side ---- */

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
    pub fn ip(&self) -> u32 {
        unsafe { net::kernel_net_ip(self.handle) }
    }

    pub fn mac(&self) -> [u8; 6] {
        let mut mac = [0u8; 6];
        unsafe { net::kernel_net_mac(self.handle, mac.as_mut_ptr()) };
        mac
    }

    /// Give the device the addresses a lease granted it. Host byte order.
    pub fn set_ip(&self, ip: u32) {
        unsafe { net::kernel_net_set_ip(self.handle, ip) }
    }

    pub fn set_mask(&self, mask: u32) {
        unsafe { net::kernel_net_set_mask(self.handle, mask) }
    }

    pub fn set_gw(&self, gw: u32) {
        unsafe { net::kernel_net_set_gw(self.handle, gw) }
    }

    /// What to ask ARP for to reach `dst`: the gateway when `dst` is off the
    /// subnet, `dst` itself when it is on it. Host byte order both ways.
    pub fn route_ip(&self, dst: u32) -> u32 {
        unsafe { net::kernel_net_route_ip(self.handle, dst) }
    }

    /// A frame the caller built whole -- Ethernet header and all -- copied
    /// into a frame of the device's and queued. False when it was dropped.
    ///
    /// `transmit` is the way to send something built in a frame already;
    /// this is for a packet assembled on the stack.
    pub fn send_raw(&self, data: &[u8]) -> bool {
        !data.is_empty()
            && unsafe { net::kernel_net_send_raw(self.handle, data.as_ptr(), data.len()) } == 0
    }

    /// The device, for the wrappers of other kernel calls that take one
    /// (`tcp::TcpListener::bind`).
    pub(crate) fn handle(&self) -> usize {
        self.handle
    }

    /// Every UDP datagram to `port`, handed to `cb(ctx, frame)` from the
    /// receive softirq: the frame itself, lent for the call -- `NetFrame::
    /// retain` keeps it. Refused for a port someone else has. The listener
    /// goes with the returned handle, once any call still running returns.
    ///
    /// cb runs on the receive path of every packet the machine gets: nothing
    /// that sleeps, and nothing long. `ctx` has to stay valid until the
    /// UdpListener is dropped.
    pub fn listen_udp(
        &self,
        port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        ctx: *mut u8,
    ) -> core::result::Result<UdpListener, ListenError> {
        match unsafe { net::kernel_net_udp_listen(self.handle, port, cb, ctx) } {
            0 => Ok(UdpListener { nic: *self, port, ctx: ctx as usize }),
            1 => Err(ListenError::PortTaken),
            2 => Err(ListenError::TableFull),
            _ => Err(ListenError::Invalid),
        }
    }

    /// Every UDP datagram to `port`, handed to `handler` -- something that
    /// lives for good, a service's one instance. No raw context at the call
    /// site: `'static` is what says the handler outlives the listener, and
    /// `Sync` what says the receive path may call it from any CPU.
    ///
    /// The handler runs on the receive path of every packet the machine gets:
    /// nothing that sleeps, and nothing long.
    pub fn listen<H: UdpHandler>(
        &self, port: u16, handler: &'static H,
    ) -> core::result::Result<UdpListener, ListenError> {
        self.listen_udp(port, on_frame::<H>, handler as *const H as *mut u8)
    }

    /// As `listen`, with `UdpHandler::on_batch_end` called at the end of each
    /// receive batch -- see `listen_udp_batched`.
    pub fn listen_batched<H: UdpHandler>(
        &self, port: u16, handler: &'static H,
    ) -> core::result::Result<UdpListener, ListenError> {
        self.listen_udp_batched(port, on_frame::<H>, on_batch_end::<H>,
            handler as *const H as *mut u8)
    }

    /// Every UDP datagram to `port`, as `listen_udp`, and a call at the end
    /// of each receive batch. A listener that answers from the receive path
    /// builds its replies as the frames arrive and hands them to the NIC in
    /// `batch_end` -- one lock and one doorbell for the batch, rather than
    /// one of each per packet.
    pub fn listen_udp_batched(
        &self,
        port: u16,
        cb: extern "C" fn(ctx: *mut u8, frame: usize),
        batch_end: extern "C" fn(ctx: *mut u8),
        ctx: *mut u8,
    ) -> core::result::Result<UdpListener, ListenError> {
        match unsafe {
            net::kernel_net_udp_listen_batch(self.handle, port, cb, ctx, batch_end)
        } {
            0 => Ok(UdpListener { nic: *self, port, ctx: ctx as usize }),
            1 => Err(ListenError::PortTaken),
            2 => Err(ListenError::TableFull),
            _ => Err(ListenError::Invalid),
        }
    }

    /// Queues a frame to transmit; false when the queue had no room and it
    /// was dropped.
    pub fn transmit(&self, frame: NetFrame) -> bool {
        let handle = frame.into_raw();
        unsafe { net::kernel_net_submit_tx(self.handle, &handle, 1) == 1 }
    }

    /// Queues a run of frames -- one lock and one doorbell for the lot --
    /// from any context. Takes every one; returns how many were queued, the
    /// rest dropped.
    ///
    /// # Safety
    /// Each is a frame handle the caller owns (`NetFrame::into_raw`) and
    /// gives up here.
    #[inline]
    pub unsafe fn transmit_raw(&self, frames: &[usize]) -> usize {
        if frames.is_empty() {
            return 0;
        }
        unsafe { net::kernel_net_submit_tx(self.handle, frames.as_ptr(), frames.len()) }
    }
}

/// A device that can be set and cleared without a lock: what a receive path
/// reads once a packet. Only a `Nic` ever goes in, so only a `Nic` comes out.
pub struct AtomicNic(core::sync::atomic::AtomicUsize);

impl AtomicNic {
    pub const fn none() -> Self {
        Self(core::sync::atomic::AtomicUsize::new(0))
    }

    pub fn set(&self, nic: Option<Nic>) {
        self.0.store(nic.map_or(0, |nic| nic.handle), core::sync::atomic::Ordering::Release);
    }

    #[inline]
    pub fn get(&self) -> Option<Nic> {
        match self.0.load(core::sync::atomic::Ordering::Acquire) {
            0 => None,
            handle => Some(Nic { handle }),
        }
    }
}

/// Frames gathered to be handed over together -- to a device to transmit,
/// one lock and one doorbell for the lot, or by a driver to the stack, one
/// lock for the harvest -- rather than one of each per frame.
pub struct FrameBatch<const N: usize> {
    frames: [usize; N],
    count: usize,
}

/// What a listener that answers from the receive path gathers its replies in.
pub type TxBatch<const N: usize> = FrameBatch<N>;

impl<const N: usize> FrameBatch<N> {
    pub const fn new() -> Self {
        Self { frames: [0; N], count: 0 }
    }

    pub fn is_full(&self) -> bool {
        self.count == N
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Takes the frame. False, and the frame released, when there is no room.
    pub fn push(&mut self, frame: NetFrame) -> bool {
        if self.count == N {
            return false;
        }
        self.frames[self.count] = frame.into_raw();
        self.count += 1;
        true
    }

    /// Everything gathered, released unsent.
    pub fn clear(&mut self) {
        let count = core::mem::replace(&mut self.count, 0);
        for handle in &self.frames[..count] {
            /* A frame `push` took ownership of, given up here. */
            drop(unsafe { NetFrame::from_raw(*handle) });
        }
    }

    /// Everything gathered, to the device: how many it queued. The rest it
    /// releases, and the batch is empty either way.
    pub fn send(&mut self, nic: &Nic) -> usize {
        let count = core::mem::replace(&mut self.count, 0);
        /* Each is a frame `push` took ownership of and gives up here. */
        unsafe { nic.transmit_raw(&self.frames[..count]) }
    }
}

impl<const N: usize> Drop for FrameBatch<N> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Being inside the receive dispatch, as a value.
///
/// The kernel runs the receive soft IRQ on one CPU at a time, and the
/// dispatch in it is what calls every listener. So there is one of these at
/// a time, a handler is lent it for the length of a call, and holding it is
/// what it means to be the only code on the receive path right now -- which
/// is what lets `RxOwned` hand out its contents without a lock.
pub struct RxContext {
    _only_the_dispatch_makes_one: (),
}

/// What only the receive path touches: the replies a listener gathers during
/// a batch, say. No lock, because the `RxContext` borrowed to reach in is
/// the proof nobody else is there.
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

/// What `Nic::listen` hands datagrams to.
pub trait UdpHandler: Sync + 'static {
    /// One datagram's frame, lent for the length of the call.
    fn on_frame(&'static self, frame: Lent<'_>, rx: &mut RxContext);

    /// The end of a receive batch, for a listener registered with
    /// `listen_batched`: the moment to hand the NIC what was built.
    fn on_batch_end(&'static self, _rx: &mut RxContext) {}
}

/// A frame the receive path lends a listener for the length of one call.
pub struct Lent<'a> {
    handle: usize,
    _life: core::marker::PhantomData<&'a ()>,
}

impl Lent<'_> {
    /// The frame's bytes, Ethernet header first.
    pub fn bytes(&self) -> &[u8] {
        /* Alive and unwritten for as long as the borrow: that is what being
         * lent it for the call means, and `retain` -- the way to a frame
         * that can be written -- ends the loan. */
        unsafe { NetFrame::lent(self.handle) }
    }

    /// The frame, to keep past the call -- to answer in, where it lies. The
    /// receive path's own reference is then not the last, and it never looks
    /// at the bytes again.
    pub fn retain(self) -> NetFrame {
        unsafe { NetFrame::retain(self.handle) }
    }
}

extern "C" fn on_frame<H: UdpHandler>(ctx: *mut u8, frame: usize) {
    /* `ctx` is the `&'static H` that `Nic::listen` registered. */
    let handler = unsafe { &*(ctx as *const H) };
    /* This is the one place an `RxContext` comes from, and what makes it
     * true: nothing but the receive dispatch is ever given this function,
     * and the kernel runs that dispatch on one CPU at a time. */
    let mut rx = RxContext { _only_the_dispatch_makes_one: () };
    handler.on_frame(Lent { handle: frame, _life: core::marker::PhantomData }, &mut rx);
}

extern "C" fn on_batch_end<H: UdpHandler>(ctx: *mut u8) {
    let handler = unsafe { &*(ctx as *const H) };
    /* As in `on_frame`. */
    let mut rx = RxContext { _only_the_dispatch_makes_one: () };
    handler.on_batch_end(&mut rx);
}

/// A UDP port listened on, from `Nic::listen_udp`; given back on drop, once
/// no call of its callback is still running -- so what the callback reaches
/// may go right after. Task context: the drop may wait.
pub struct UdpListener {
    nic: Nic,
    port: u16,
    /* what it was registered with, which is what takes away this listener
       and nobody else's on the port */
    ctx: usize,
}

/* A handle -- a device, a port and the word it was registered with -- and
 * the kernel's listener table is what it names. */
unsafe impl Send for UdpListener {}

impl Drop for UdpListener {
    fn drop(&mut self) {
        unsafe { net::kernel_net_udp_unlisten(self.nic.handle, self.port, self.ctx as *mut u8) }
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
    fn raw(&self) -> usize {
        self.handle.get()
    }

    /// Allocate a new RX frame with `data_len` bytes of DMA-backed buffer.
    /// Direction is set to Rx. `Length` is initialised to 0; call `set_len`
    /// after the hardware fills the buffer.
    /// Returns `None` on allocation failure.
    pub fn alloc_rx(data_len: usize) -> Option<Self> {
        let h = unsafe { net::kernel_netframe_alloc_rx(data_len) };
        core::num::NonZeroUsize::new(h).map(|handle| Self { handle })
    }

    /// A frame to transmit, room for `data_len` bytes: from the frame pool --
    /// a per-CPU cache, no allocator -- whenever it fits one. `len()` is 0
    /// until `set_len`.
    #[inline]
    pub fn alloc_tx(data_len: usize) -> Option<Self> {
        let h = unsafe { net::kernel_netframe_alloc_tx(data_len) };
        core::num::NonZeroUsize::new(h).map(|handle| Self { handle })
    }

    /// A reference of the caller's own to a frame the kernel lent: how a UDP
    /// frame listener keeps the frame it was handed past its return.
    ///
    /// # Safety
    /// `handle` must be a frame alive for the call -- the one a listener was
    /// handed, say.
    #[inline]
    pub unsafe fn retain(handle: usize) -> Self {
        unsafe { net::kernel_netframe_get(handle) };
        /* A frame alive for the call is not the null one. */
        Self { handle: unsafe { core::num::NonZeroUsize::new_unchecked(handle) } }
    }

    /// The bytes of a frame the kernel lent, without taking it.
    ///
    /// # Safety
    /// `handle` must be a frame that outlives the slice and that nobody
    /// writes meanwhile.
    #[inline]
    pub unsafe fn lent<'a>(handle: usize) -> &'a [u8] {
        let ptr = unsafe { net::kernel_netframe_data(handle) };
        let len = unsafe { net::kernel_netframe_len(handle) };
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    /// Slice of the received/transmitted data (length = `self.len()`).
    ///
    /// Note: for a freshly allocated RX frame `len()` is 0 until `set_len` is
    /// called. Use `data_raw_mut(capacity)` to access the full buffer before
    /// the length is known (e.g. for memcpy-based drivers).
    pub fn data(&self) -> &[u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.raw()) };
        let len = unsafe { net::kernel_netframe_len(self.raw()) };
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    /// Mutable slice of the received/transmitted data (length = `self.len()`).
    /// See `data()` for the note on freshly allocated RX frames.
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

    /// Consume the frame, returning the raw handle without decrementing
    /// the refcount.  The caller must eventually call `from_raw()` or
    /// invoke `kernel_netframe_put(handle)` directly (e.g. from an ISR).
    #[inline]
    pub fn into_raw(self) -> usize {
        let h = self.raw();
        core::mem::forget(self);
        h
    }

    /// Reconstruct a `NetFrame` from a raw handle returned by `into_raw()`.
    ///
    /// # Safety
    /// `handle` must be a valid non-zero handle previously obtained from
    /// `into_raw()`.  The caller must not use the original raw handle after
    /// this call.
    #[inline]
    pub unsafe fn from_raw(handle: usize) -> Self {
        Self { handle: unsafe { core::num::NonZeroUsize::new_unchecked(handle) } }
    }
}

impl Drop for NetFrame {
    fn drop(&mut self) {
        unsafe { net::kernel_netframe_put(self.raw()) }
    }
}

/// `dhcp=off`: the kernel was told not to run a DHCP client.
pub fn dhcp_off() -> bool {
    unsafe { net::kernel_param_dhcp_off() != 0 }
}

/// `dns=on`: a lease's DNS server is worth starting a resolver on.
pub fn dns_on() -> bool {
    unsafe { net::kernel_param_dns_on() != 0 }
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
