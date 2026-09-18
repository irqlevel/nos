use ffi::net;

/// Handle to a registered net device.
/// Registration is permanent (boot-lifetime); there is no unregister, so the
/// handle is a plain value a driver may copy where it needs one.
#[derive(Clone, Copy)]
pub struct NetDeviceHandle {
    handle: usize,
}

/// Ops table passed to `register`. All function pointers must remain valid
/// for the lifetime of the kernel.
pub struct NetDeviceOps {
    /// Null-terminated ASCII device name (e.g. b"eth0\0").
    pub name: *const u8,
    pub mac: [u8; 6],
    /// Called by the C++ net stack while TxQueueLock is held.
    /// Use `NetDeviceHandle::tx_dequeue` to drain frames one by one.
    pub flush_tx: extern "C" fn(ctx: *mut u8),
    /// Called from the soft IRQ task to process received frames.
    pub process_rx: extern "C" fn(ctx: *mut u8),
    pub ctx: *mut u8,
}

/// Register a net device with the kernel net device table.
/// Returns `None` if the slot pool is full.
pub fn register(ops: &NetDeviceOps) -> Option<NetDeviceHandle> {
    let ffi_ops = net::NetDeviceOps {
        name: ops.name,
        mac: ops.mac,
        flush_tx: ops.flush_tx,
        process_rx: ops.process_rx,
        ctx: ops.ctx,
    };
    let h = unsafe { net::kernel_netdev_register(&ffi_ops) };
    if h == 0 { None } else { Some(NetDeviceHandle { handle: h }) }
}

impl NetDeviceHandle {
    /// Construct a null placeholder (handle == 0).
    /// Used as a field initialiser before the real handle is assigned.
    /// Calling any method on a placeholder is a no-op or returns None.
    pub fn placeholder() -> Self {
        Self { handle: 0 }
    }

    pub fn set_ip(&self, ip: u32) {
        unsafe { net::kernel_netdev_set_ip(self.handle, ip) }
    }

    pub fn set_mask(&self, mask: u32) {
        unsafe { net::kernel_netdev_set_mask(self.handle, mask) }
    }

    pub fn set_gw(&self, gw: u32) {
        unsafe { net::kernel_netdev_set_gw(self.handle, gw) }
    }

    /// Dequeue one pending TX frame. Call this from inside `flush_tx` callback.
    /// Returns `None` when the TX queue is empty.
    ///
    /// # Lifetime contract
    ///
    /// The returned `NetFrame` wraps a reference-counted DMA buffer. You must
    /// keep the `NetFrame` alive (i.e. stored in a TX slot) until the hardware
    /// signals completion of the DMA transfer. Dropping the frame early (while
    /// the hardware is still reading from `data_phys()`) is a use-after-free.
    /// Only drop (or explicitly `put`) the frame after the hardware completion
    /// interrupt fires and you have confirmed the descriptor is done.
    pub fn tx_dequeue(&self) -> Option<NetFrame> {
        let h = unsafe { net::kernel_netdev_tx_dequeue(self.handle) };
        if h == 0 { None } else { Some(NetFrame { handle: h }) }
    }

    /// Notify the device that TX descriptors have been submitted to hardware.
    pub fn tx_notify(&self) {
        unsafe { net::kernel_netdev_tx_notify(self.handle) }
    }

    /// Pass a received frame to the kernel net stack.
    /// Takes ownership of the frame (calls `Put` on drop or on queue-full).
    pub fn enqueue_rx(&self, frame: NetFrame) {
        let h = frame.handle;
        core::mem::forget(frame);
        unsafe { net::kernel_netdev_enqueue_rx(self.handle, h) }
    }

    /// Hand a whole harvest over at once.
    ///
    /// The receive queue's lock is taken once for the batch rather than once
    /// per frame, which at tens of thousands of packets a second is the
    /// difference between a lock acquisition being noise and being the top of
    /// the receive path in a profile.
    ///
    /// The handles are consumed: whatever the queue had no room for is
    /// released on the other side, so the caller must not touch them again.
    pub fn enqueue_rx_batch(&self, handles: &[usize]) {
        if handles.is_empty() {
            return;
        }
        unsafe {
            net::kernel_netdev_enqueue_rx_batch(
                self.handle,
                handles.as_ptr(),
                handles.len(),
            )
        };
    }

    /// Hand a transmitted frame back for release, instead of dropping it.
    ///
    /// A driver reaps its TX ring from `flush_tx`, which the C++ net stack
    /// calls under TxQueueLock with interrupts off. Dropping a `NetFrame`
    /// there calls `kernel_netframe_put` -> `Mm::Free`, which shoots down the
    /// TLB on every other CPU and waits for each to acknowledge -- and a CPU
    /// spinning on TxQueueLock has interrupts off and never will. The two
    /// then wait for each other forever. This queues the frame instead; the
    /// C++ side releases it once the lock is down.
    pub fn tx_done(&self, frame: NetFrame) {
        let h = frame.into_raw();
        unsafe { net::kernel_netdev_tx_done(self.handle, h) }
    }
}

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

/// Frames gathered to go out together: one transmit lock and one doorbell
/// for the lot, rather than one of each per frame.
pub struct TxBatch<const N: usize> {
    frames: [usize; N],
    count: usize,
}

impl<const N: usize> TxBatch<N> {
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

impl<const N: usize> Drop for TxBatch<N> {
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
    handle: usize,
}

impl NetFrame {
    /// Allocate a new RX frame with `data_len` bytes of DMA-backed buffer.
    /// Direction is set to Rx. `Length` is initialised to 0; call `set_len`
    /// after the hardware fills the buffer.
    /// Returns `None` on allocation failure.
    pub fn alloc_rx(data_len: usize) -> Option<Self> {
        let h = unsafe { net::kernel_netframe_alloc_rx(data_len) };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    /// A frame to transmit, room for `data_len` bytes: from the frame pool --
    /// a per-CPU cache, no allocator -- whenever it fits one. `len()` is 0
    /// until `set_len`.
    #[inline]
    pub fn alloc_tx(data_len: usize) -> Option<Self> {
        let h = unsafe { net::kernel_netframe_alloc_tx(data_len) };
        if h == 0 { None } else { Some(Self { handle: h }) }
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
        Self { handle }
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
        let ptr = unsafe { net::kernel_netframe_data(self.handle) };
        let len = unsafe { net::kernel_netframe_len(self.handle) };
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    /// Mutable slice of the received/transmitted data (length = `self.len()`).
    /// See `data()` for the note on freshly allocated RX frames.
    pub fn data_mut(&mut self) -> &mut [u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.handle) };
        let len = unsafe { net::kernel_netframe_len(self.handle) };
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Mutable slice of the full allocated buffer up to `capacity` bytes.
    ///
    /// Use this when you need to write into a freshly allocated RX frame
    /// before calling `set_len`. `capacity` must not exceed the value passed
    /// to `alloc_rx`; the caller is responsible for not exceeding it.
    #[inline]
    pub fn data_raw_mut(&mut self, capacity: usize) -> &mut [u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.handle) };
        unsafe { core::slice::from_raw_parts_mut(ptr, capacity) }
    }

    /// Physical address of the data buffer (for DMA descriptor programming).
    #[inline]
    pub fn data_phys(&self) -> u64 {
        unsafe { net::kernel_netframe_data_phys(self.handle) }
    }

    /// Current valid data length (0 for a freshly allocated RX frame).
    #[inline]
    pub fn len(&self) -> usize {
        unsafe { net::kernel_netframe_len(self.handle) }
    }

    #[inline]
    pub fn set_len(&mut self, len: usize) {
        unsafe { net::kernel_netframe_set_len(self.handle, len) }
    }

    /// Consume the frame, returning the raw handle without decrementing
    /// the refcount.  The caller must eventually call `from_raw()` or
    /// invoke `kernel_netframe_put(handle)` directly (e.g. from an ISR).
    #[inline]
    pub fn into_raw(self) -> usize {
        let h = self.handle;
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
        Self { handle }
    }
}

impl Drop for NetFrame {
    fn drop(&mut self) {
        unsafe { net::kernel_netframe_put(self.handle) }
    }
}

/// What the recycled frame pool has been doing: allocations it could not
/// serve, and frames a driver is holding.
pub fn frame_pool_stats() -> (usize, usize) {
    let (mut misses, mut in_flight) = (0, 0);
    unsafe { net::kernel_netframe_pool_stats(&mut misses, &mut in_flight) };
    (misses, in_flight)
}

/// Receive polls, polls that found work, and polls that found work with no
/// interrupt-driven pass since the last one -- the third being the evidence
/// of a lost wakeup.
pub fn rx_poll_stats() -> (usize, usize, usize) {
    let (mut polls, mut work, mut stalls) = (0, 0, 0);
    unsafe { net::kernel_net_rx_poll_stats(&mut polls, &mut work, &mut stalls) };
    (polls, work, stalls)
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
