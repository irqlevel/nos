//! A virtual NIC, as a module drives one (`net/src/vnic.rs`): the stack's end
//! of a network the module makes. Opened by name -- made and registered the
//! first time, found after, and never given back -- it hands what the stack
//! sends out of it to a `VnicSink` the module attaches, and takes frames the
//! module hands in as if they had been received.

use alloc::sync::Arc;

use ffi::net;

use crate::net::Nic;

/// What a module attaches to a virtual NIC. `on_frame` runs on the transmit
/// path of the device -- whichever CPU sends, interrupts off -- so it copies
/// the frame and returns: nothing that sleeps, allocates or takes long.
pub trait VnicSink: Send + Sync + 'static {
    /// A frame the stack sent out of the device, lent for the call.
    fn on_frame(&self, frame: &[u8]);
}

/// A virtual NIC: a handle into the kernel's table of them, looked up by
/// every call, so a stale one does nothing.
#[derive(Clone, Copy)]
pub struct Vnic {
    handle: usize,
}

impl Vnic {
    /// The virtual NIC called `name`; the first time, made with `mac` and
    /// given `ip` and `mask` (host byte order). None when the kernel's table
    /// is full, or another is being made at the moment.
    pub fn open(name: &str, mac: [u8; 6], ip: u32, mask: u32) -> Option<Vnic> {
        let handle = unsafe { net::kernel_vnic_open(name.as_ptr(), name.len(), mac.as_ptr(), ip, mask) };
        if handle == 0 { None } else { Some(Vnic { handle }) }
    }

    /// The net device it is: for TCP to what is behind it, and ARP.
    pub fn nic(&self) -> Option<Nic> {
        Nic::from_handle(net::kernel_vnic_device(self.handle))
    }

    /// Everything the stack sends out of it, to `sink`, until the returned
    /// attachment is dropped. None when something is attached already.
    pub fn attach<S: VnicSink>(&self, sink: Arc<S>) -> Option<Attached> {
        /* The attachment's hold on the sink, as the word the kernel hands
         * back: given up in the drop, after the detach -- or here, if the
         * kernel takes no attachment. */
        let ctx = Arc::into_raw(sink) as usize;
        if unsafe { net::kernel_vnic_attach(self.handle, on_frame::<S>, ctx) } != 0 {
            unsafe { release::<S>(ctx) };
            return None;
        }
        Some(Attached { handle: self.handle, ctx, release: release::<S> })
    }

    /// A frame into the stack as if the device had received it: false when
    /// it was not taken -- too short or too long, the backlog full, the frame
    /// pool dry.
    pub fn receive(&self, frame: &[u8]) -> bool {
        unsafe { net::kernel_vnic_receive(self.handle, frame.as_ptr(), frame.len()) == 0 }
    }
}

/// A sink attached to a virtual NIC; detached on drop, once no call of it is
/// still running -- so what it reaches may go right after. Task context: the
/// drop may wait.
pub struct Attached {
    handle: usize,
    ctx: usize,
    release: unsafe fn(usize),
}

impl Drop for Attached {
    fn drop(&mut self) {
        unsafe { net::kernel_vnic_detach(self.handle) };
        /* No call is running and none will start: the sink may go. */
        unsafe { (self.release)(self.ctx) };
    }
}

/// What the kernel calls the attached sink through.
///
/// # Safety
/// `ctx` is the hold `attach` took, alive until the detach has returned;
/// `frame` is `len` bytes lent for the call.
unsafe extern "C" fn on_frame<S: VnicSink>(ctx: usize, frame: *const u8, len: usize) {
    let sink = unsafe { &*(ctx as *const S) };
    sink.on_frame(unsafe { core::slice::from_raw_parts(frame, len) });
}

/// # Safety
/// `ctx` is a hold `attach` took, given up once, after nothing can call
/// `on_frame` with it.
unsafe fn release<S>(ctx: usize) {
    drop(unsafe { Arc::from_raw(ctx as *const S) });
}
