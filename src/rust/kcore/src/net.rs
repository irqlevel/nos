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

    /// Its address, host byte order; 0 until it has one.
    pub fn ip(&self) -> u32 {
        unsafe { net::kernel_net_ip(self.handle) }
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
