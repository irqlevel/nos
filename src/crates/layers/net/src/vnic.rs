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

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcore::once::Once;
use kcore::sync::IrqSpinLock;
use kcore::klog;

use crate::device::{NetDriver, RxQueue, TxQueue};
use crate::frame::{Frame, FrameQueue};
use crate::nic::Nic;

/// The most virtual NICs there are.
const MAX_VNICS: usize = 4;
/// The most frames handed in and not yet taken by a receive pass: what is
/// past it is dropped, and counted. `kcore`'s number, which a module shares
/// out among its own senders by.
const RX_BACKLOG: usize = kcore::vnic::BACKLOG;
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
    /// Frames handed in, for the next receive pass, and how many; and how
    /// many passes have emptied it.
    rx: IrqSpinLock<FrameQueue>,
    backlog: AtomicUsize,
    drains: AtomicU64,
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
        self.drains.fetch_add(1, Ordering::AcqRel);
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

    /// No more calls of the handler attached with `ctx`: back once none is
    /// still running. A sink attached since -- after a sweep took this one
    /// off (`detach_range`) -- is not this caller's to take: the detach was
    /// by the device alone, and took whoever's sink was there.
    fn detach(&self, ctx: usize) {
        {
            let mut sink = self.sink.lock();
            if !sink.is_some_and(|s| s.ctx == ctx) {
                return;
            }
            *sink = None;
        }
        self.wait_out();
    }

    /// Back once no call of a handler taken away is still running: one
    /// that read the sink before it was taken counted itself under the
    /// lock.
    /// A sink that never returns made this a wait with no word: said every
    /// ten seconds it lasts, as a join is.
    fn wait_out(&self) {
        let from = kcore::time::boot_time_ns();
        let mut said_at = from;
        while self.in_flight.load(Ordering::Acquire) != 0 {
            kcore::task::yield_to_runnable();
            let now = kcore::time::boot_time_ns();
            if now.saturating_sub(said_at) >= WAIT_REPORT_NS {
                said_at = now;
                let name = self.nic().map(|nic| nic.device().name());
                klog!(0, "vnic: {}: a detach has waited {} s for its sink's call to return",
                      name.and_then(|n| core::str::from_utf8(n).ok()).unwrap_or("?"),
                      now.saturating_sub(from) / kcore::consts::NS_PER_SEC);
            }
        }
    }

    pub fn nic(&self) -> Option<Nic> {
        self.nic.get().copied()
    }
}

/// What a module whose image is `lo..hi` left attached as it went, detached
/// and said: a sink of its code, called from the transmit path for every
/// frame the stack sends out of the device, which the next frame would have
/// called into the pages the unload frees -- and the module's next load been
/// refused its attach ("attached already") until a reboot. Task context:
/// back once no call of one is still running. The loader's, through
/// `rust_net_close_range`.
pub(crate) fn detach_range(lo: usize, hi: usize, module: &str) {
    for v in VNICS.iter().filter_map(|slot| slot.get().copied()) {
        let left = {
            let mut sink = v.sink.lock();
            let left = sink.is_some_and(|s| (lo..hi).contains(&(s.handler as usize)));
            if left {
                *sink = None;
            }
            left
        };
        if left {
            v.wait_out();
            let name = v.nic().map(|nic| nic.device().name());
            klog!(0, "module: {} left its sink attached to {}: detached", module,
                name.and_then(|n| core::str::from_utf8(n).ok()).unwrap_or("a virtual NIC"));
        }
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

/// How long a wait for a sink's call to return goes before it is said, and
/// again: a join's ten seconds (`Task::wait`).
const WAIT_REPORT_NS: u64 = 10 * kcore::consts::NS_PER_SEC;

fn make(name: &str, mac: [u8; 6], ip: u32, mask: u32) -> Option<usize> {
    let slot = VNICS.iter().position(|s| s.get().is_none())?;
    /* Asked before the device is made `'static` -- for good, as a driver
     * registered is: refused after, as it was for a name a device had or a
     * table full, it was lost at every open that tried. Only a
     * registration racing this one can refuse it now, and that is said. */
    if !crate::device::DEVICES.would_take(name) {
        return None;
    }
    let vnic: &'static Vnic = alloc::boxed::Box::leak(alloc::boxed::Box::new(Vnic {
        sink: IrqSpinLock::new(None),
        in_flight: AtomicUsize::new(0),
        rx: IrqSpinLock::new(FrameQueue::new()),
        backlog: AtomicUsize::new(0),
        drains: AtomicU64::new(0),
        nic: Once::new(),
        to_module: AtomicUsize::new(0),
        unattached: AtomicUsize::new(0),
        from_module: AtomicUsize::new(0),
        refused: AtomicUsize::new(0),
    }));
    let Some(nic) = crate::register(name, mac, vnic, (), ()) else {
        klog!(0, "vnic: {} refused by a registration that came first: its {} bytes kept for good",
              name, core::mem::size_of::<Vnic>());
        return None;
    };
    nic.device().set_ip(ip);
    nic.device().set_mask(mask);
    let _ = vnic.nic.set(nic);
    if VNICS[slot].set(vnic).is_err() {
        return None;
    }
    klog!(0, "vnic: {} made for a module, {}.{}.{}.{}", name,
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

/// No more calls of the handler attached with `ctx`; back once none is
/// running. Task context.
#[no_mangle]
pub extern "C" fn kernel_vnic_detach(vnic: usize, ctx: usize) {
    if let Some(v) = by_handle(vnic) {
        v.detach(ctx);
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

/// How many receive passes have emptied a virtual NIC's backlog -- or 0 for
/// a bad handle: a module sharing the backlog out counts afresh at each.
#[no_mangle]
pub extern "C" fn kernel_vnic_drains(vnic: usize) -> u64 {
    by_handle(vnic).map_or(0, |v| v.drains.load(Ordering::Acquire))
}
