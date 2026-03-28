use ffi::net;

/// Handle to a registered net device.
/// Registration is permanent (boot-lifetime); there is no unregister.
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
    pub fn data_raw_mut(&mut self, capacity: usize) -> &mut [u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.handle) };
        unsafe { core::slice::from_raw_parts_mut(ptr, capacity) }
    }

    /// Physical address of the data buffer (for DMA descriptor programming).
    pub fn data_phys(&self) -> u64 {
        unsafe { net::kernel_netframe_data_phys(self.handle) }
    }

    /// Current valid data length (0 for a freshly allocated RX frame).
    pub fn len(&self) -> usize {
        unsafe { net::kernel_netframe_len(self.handle) }
    }

    pub fn set_len(&mut self, len: usize) {
        unsafe { net::kernel_netframe_set_len(self.handle, len) }
    }
}

impl Drop for NetFrame {
    fn drop(&mut self) {
        unsafe { net::kernel_netframe_put(self.handle) }
    }
}
