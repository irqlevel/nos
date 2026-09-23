//! A virtual NIC that a loadable module drives: the stack's end of a network
//! the module makes -- the hypervisor's, whose guests sit behind it.
//!
//! A module cannot register a NIC: a driver registers with this layer as a
//! trait object from inside the image, and there is no C name for a module
//! to bind one by. So the device is made and registered here, the first time
//! a module asks for it by name, and stays -- a net device is never given
//! back -- for the module, or the next load of it, to find again. What the
//! module does is trade frames with it:
//!
//! - what the stack sends out of the device goes to the handler the module
//!   attached, from the transmit path -- under the device's transmit lock,
//!   interrupts off -- so the handler copies the frame and returns: nothing
//!   that sleeps, nothing that allocates. With none attached it is dropped,
//!   and counted.
//! - what the module hands in arrives as if received: into a frame from the
//!   pool, onto the device's backlog, and up the stack on the next receive
//!   pass, which the hand-in raises.
//!
//! The handler is the module's code. A detach takes it away under the lock
//! and then waits out any call of it still running, so the module can go
//! after -- the UDP listeners' way (`Device::unlisten_udp`).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcore::once::Once;
use kcore::sync::IrqSpinLock;
use kcore::trace;

use crate::device::{NetDriver, RxQueue, TxQueue};
use crate::frame::{Frame, FrameQueue};
use crate::nic::Nic;

/// The most virtual NICs there are.
const MAX_VNICS: usize = 4;
/// The most frames handed in and not yet taken by a receive pass: what is
/// past it is dropped, and counted.
const RX_BACKLOG: usize = 256;
/// An Ethernet frame, header and all, less the FCS: the longest handed in.
const MAX_FRAME: usize = 1514;
const MIN_FRAME: usize = 14;

/// What a module attaches: called with each frame the stack sends out of the
/// device, lent for the call.
pub type SinkFn = unsafe extern "C" fn(ctx: usize, frame: *const u8, len: usize);

#[derive(Clone, Copy)]
struct Sink {
    handler: SinkFn,
    ctx: usize,
}

/// One virtual NIC.
pub struct Vnic {
    sink: IrqSpinLock<Option<Sink>>,
    /// Calls of the handler under way: what a detach waits out.
    in_flight: AtomicUsize,
    /// Frames handed in, for the next receive pass, and how many.
    rx: IrqSpinLock<FrameQueue>,
    backlog: AtomicUsize,
    nic: Once<Nic>,
    /// What went out to the module, what went nowhere (nothing attached),
    /// what came in, and what could not.
    pub to_module: AtomicUsize,
    pub unattached: AtomicUsize,
    pub from_module: AtomicUsize,
    pub refused: AtomicUsize,
}

/* The table: set once each, the device leaked with it. */
static VNICS: [Once<&'static Vnic>; MAX_VNICS] = [const { Once::new() }; MAX_VNICS];
/* One `open` at a time makes a device: registration is not made for two at
 * once. A second waits its turn by being refused -- it is a module's load,
 * not a datapath. */
static MAKING: AtomicBool = AtomicBool::new(false);

impl NetDriver for Vnic {
    type Tx = ();
    type Rx = ();

    fn flush_tx(&'static self, _tx: &mut (), queue: &mut TxQueue<'_>) {
        while let Some(frame) = queue.dequeue() {
            self.to_sink(frame.bytes());
            /* Given back, never dropped here: see `TxQueue::done`. */
            queue.done(frame);
        }
    }

    fn process_rx(&'static self, _rx: &mut (), queue: &mut RxQueue<'_>) {
        let mut frames = self.rx.lock().take();
        self.backlog.fetch_sub(frames.len(), Ordering::AcqRel);
        queue.deliver(&mut frames);
    }
}

impl Vnic {
    /// A frame the stack sent, to the module's handler if one is attached.
    fn to_sink(&self, bytes: &[u8]) {
        let sink = {
            let guard = self.sink.lock();
            let sink = *guard;
            if sink.is_some() {
                /* Counted under the lock, so that a detach, which takes the
                 * sink away under it, then sees every call it has to wait
                 * for. */
                self.in_flight.fetch_add(1, Ordering::AcqRel);
            }
            sink
        };
        match sink {
            Some(s) => {
                /* The module's handler, with the frame lent for the call. */
                unsafe { (s.handler)(s.ctx, bytes.as_ptr(), bytes.len()) };
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                self.to_module.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                self.unattached.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A frame into the stack, as if received on the device: true when it
    /// went on the backlog.
    fn receive(&self, bytes: &[u8]) -> bool {
        if bytes.len() < MIN_FRAME || bytes.len() > MAX_FRAME {
            self.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        /* A place on the backlog first, given back if the frame is not had. */
        if self.backlog.fetch_add(1, Ordering::AcqRel) >= RX_BACKLOG {
            self.backlog.fetch_sub(1, Ordering::AcqRel);
            self.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        /* Dropped when it will not take the bytes: no lock is held here, so
         * the frame goes back to the pool. */
        let frame = Frame::alloc_rx(bytes.len()).and_then(|mut f| f.fill(bytes).then_some(f));
        let Some(frame) = frame else {
            self.backlog.fetch_sub(1, Ordering::AcqRel);
            self.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        self.rx.lock().push(frame);
        self.from_module.fetch_add(1, Ordering::Relaxed);
        kcore::softirq::raise(kcore::softirq::TYPE_NET_RX);
        true
    }

    fn attach(&self, sink: Sink) -> bool {
        let mut guard = self.sink.lock();
        if guard.is_some() {
            return false;
        }
        *guard = Some(sink);
        true
    }

    /// No more calls of the handler: back once none is still running.
    fn detach(&self) {
        *self.sink.lock() = None;
        while self.in_flight.load(Ordering::Acquire) != 0 {
            kcore::task::yield_to_runnable();
        }
    }

    pub fn nic(&self) -> Option<Nic> {
        self.nic.get().copied()
    }
}

/// The virtual NIC called `name`: found, or -- the first time -- made with
/// `mac`, registered with the stack and given `ip` and `mask`. Its handle is
/// its place in the table plus one.
fn open(name: &str, mac: [u8; 6], ip: u32, mask: u32) -> Option<usize> {
    for (i, slot) in VNICS.iter().enumerate() {
        if let Some(v) = slot.get() {
            if v.nic().map_or(false, |nic| nic.device().name() == name.as_bytes()) {
                return Some(i + 1);
            }
        }
    }

    if MAKING.swap(true, Ordering::AcqRel) {
        return None;
    }
    let made = make(name, mac, ip, mask);
    MAKING.store(false, Ordering::Release);
    made
}

fn make(name: &str, mac: [u8; 6], ip: u32, mask: u32) -> Option<usize> {
    let slot = VNICS.iter().position(|s| s.get().is_none())?;
    let vnic: &'static Vnic = alloc::boxed::Box::leak(alloc::boxed::Box::new(Vnic {
        sink: IrqSpinLock::new(None),
        in_flight: AtomicUsize::new(0),
        rx: IrqSpinLock::new(FrameQueue::new()),
        backlog: AtomicUsize::new(0),
        nic: Once::new(),
        to_module: AtomicUsize::new(0),
        unattached: AtomicUsize::new(0),
        from_module: AtomicUsize::new(0),
        refused: AtomicUsize::new(0),
    }));
    let nic = crate::register(name, mac, vnic, (), ())?;
    nic.device().set_ip(ip);
    nic.device().set_mask(mask);
    let _ = vnic.nic.set(nic);
    if VNICS[slot].set(vnic).is_err() {
        return None;
    }
    trace!(0, "vnic: {} made for a module, {}.{}.{}.{}", name,
           ip >> 24, (ip >> 16) & 0xFF, (ip >> 8) & 0xFF, ip & 0xFF);
    Some(slot + 1)
}

/// The device a handle names: looked up, never dereferenced, so a bad one
/// is None.
fn by_handle(handle: usize) -> Option<&'static Vnic> {
    VNICS.get(handle.checked_sub(1)?)?.get().copied()
}

/* ---- the C ABI a module binds (kcore::vnic) ---- */

/// The virtual NIC named `name`, made -- with `mac`, `ip` and `mask`, host
/// byte order -- and registered with the stack the first time it is asked
/// for, found after: a handle, or 0 when the table is full, the name will not
/// do, or another is being made at the moment.
///
/// # Safety
/// `name` points at `name_len` bytes, `mac` at six.
#[no_mangle]
pub unsafe extern "C" fn kernel_vnic_open(
    name: *const u8, name_len: usize, mac: *const u8, ip: u32, mask: u32,
) -> usize {
    if name.is_null() || mac.is_null() {
        return 0;
    }
    let name = match core::str::from_utf8(unsafe { core::slice::from_raw_parts(name, name_len) }) {
        Ok(name) => name,
        Err(_) => return 0,
    };
    let mut m = [0u8; 6];
    m.copy_from_slice(unsafe { core::slice::from_raw_parts(mac, 6) });
    open(name, m, ip, mask).unwrap_or(0)
}

/// Frames the stack sends out of the device go to `handler(ctx, frame, len)`
/// from here on, until `kernel_vnic_detach`. 0, or -1 for a bad handle or
/// one attached already.
///
/// # Safety
/// `handler` is sound to call with `ctx` and a lent frame from the transmit
/// path, interrupts off, until `kernel_vnic_detach` has returned.
#[no_mangle]
pub unsafe extern "C" fn kernel_vnic_attach(vnic: usize, handler: SinkFn, ctx: usize) -> i32 {
    match by_handle(vnic) {
        Some(v) if v.attach(Sink { handler, ctx }) => 0,
        _ => -1,
    }
}

/// No more calls of the attached handler; back once none is running. Task
/// context.
#[no_mangle]
pub extern "C" fn kernel_vnic_detach(vnic: usize) {
    if let Some(v) = by_handle(vnic) {
        v.detach();
    }
}

/// A frame into the stack, as if the device had received it: 0, or -1 when
/// it would not fit a frame, the backlog is full, the pool is dry, or the
/// handle is bad.
///
/// # Safety
/// `frame` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_vnic_receive(vnic: usize, frame: *const u8, len: usize) -> i32 {
    if frame.is_null() {
        return -1;
    }
    match by_handle(vnic) {
        Some(v) if v.receive(unsafe { core::slice::from_raw_parts(frame, len) }) => 0,
        _ => -1,
    }
}

/// The net device a virtual NIC is -- a `kernel_net_find` handle, for the
/// module's TCP and ARP -- or 0.
#[no_mangle]
pub extern "C" fn kernel_vnic_device(vnic: usize) -> usize {
    by_handle(vnic).and_then(|v| v.nic()).map_or(0, |nic| nic.device().handle())
}
