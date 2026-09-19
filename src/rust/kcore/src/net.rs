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

use ffi::net;

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

    /// Every UDP datagram to `port`, handed to `cb(ctx, frame)` from the
    /// receive softirq: the frame itself, lent for the call -- `NetFrame::
    /// retain` keeps it, `NetFrame::lent` reads it. Refused for a port someone else has. The listener
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

/* A handle -- a device, a port and the word it was registered with -- and
 * the kernel's listener table is what it names. */
unsafe impl Send for UdpListener {}

impl Drop for UdpListener {
    fn drop(&mut self) {
        net::kernel_net_udp_unlisten(self.nic.handle, self.port, self.ctx as *mut u8)
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

    /// A frame to transmit, room for `data_len` bytes: from the frame pool --
    /// a per-CPU cache, no allocator -- whenever it fits one. `len()` is 0
    /// until `set_len`.
    #[inline]
    pub fn alloc_tx(data_len: usize) -> Option<Self> {
        let h = net::kernel_netframe_alloc_tx(data_len);
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
    net::kernel_param_dhcp_off() != 0
}

/// `dns=on`: a lease's DNS server is worth starting a resolver on.
pub fn dns_on() -> bool {
    net::kernel_param_dns_on() != 0
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
