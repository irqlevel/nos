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
    /// Direction is set to Rx. Returns `None` on allocation failure.
    pub fn alloc_rx(data_len: usize) -> Option<Self> {
        let h = unsafe { net::kernel_netframe_alloc_rx(data_len) };
        if h == 0 { None } else { Some(Self { handle: h }) }
    }

    /// Virtual address of the frame data buffer.
    pub fn data(&self) -> &[u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.handle) };
        let len = unsafe { net::kernel_netframe_len(self.handle) };
        unsafe { core::slice::from_raw_parts(ptr, len) }
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let ptr = unsafe { net::kernel_netframe_data(self.handle) };
        let len = unsafe { net::kernel_netframe_len(self.handle) };
        unsafe { core::slice::from_raw_parts_mut(ptr, len) }
    }

    /// Physical address of the data buffer (for DMA descriptor programming).
    pub fn data_phys(&self) -> u64 {
        unsafe { net::kernel_netframe_data_phys(self.handle) }
    }

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
