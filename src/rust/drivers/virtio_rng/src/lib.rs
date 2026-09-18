//! virtio-rng: the host's entropy, handed to the kernel's pool.
//!
//! The device is asked for bytes and answers on a queue, and this driver
//! polls for the answer rather than taking an interrupt: a reseed is rare,
//! and the pool in front of it (kernel/random.cpp) is what makes the latency
//! nobody's problem. Because it polls, the queue is set up asking the device
//! not to interrupt at all -- otherwise a level-triggered line would stay
//! asserted after the first completion and storm whoever shares it.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;

use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::entropy;
use kcore::pci;
use kcore::sync::SpinLock;
use kcore::trace;
use virtio::mmio::{MmioTransport, Slot};
use virtio::{Buf, Queue, Transport};

/// Four of them, as the entropy table has room for (kernel/entropy.h).
const MAX_DEVICES: usize = 4;

/// How long a request is waited for before the device is given up on. The
/// C++ this replaces spun the same count; at a poll a nanosecond or two it
/// is seconds, and a device that has not answered by then never will.
const POLL_ROUNDS: u64 = 10_000_000;

/// The request queue, which is the only one a virtio-rng has.
const REQUEST_QUEUE: u16 = 0;

struct Rng {
    transport: Box<dyn Transport>,
    /// Everything below is touched only with the lock held: the queue's
    /// bookkeeping and the one buffer the device writes into.
    lock: SpinLock,
    inner: core::cell::UnsafeCell<Inner>,
    /// NUL-terminated, handed to the entropy table, which keeps it
    name: [u8; 8],
}

struct Inner {
    queue: Queue,
    dma: DmaBuffer,
    /// A request that timed out left its descriptor with the device, still
    /// pointing at the buffer. Posting another for the same buffer would
    /// credit whichever completion arrives first to the new request, so the
    /// stale one is reclaimed first or the ask is refused.
    stuck: bool,
}

/* The lock is what makes concurrent use of Inner safe, and the entropy table
 * only ever calls through the one registration. */
unsafe impl Sync for Rng {}
unsafe impl Send for Rng {}

static mut DEVICES: usize = 0;

impl Rng {
    /// Bring a device up on whatever bus found it, and take its queue.
    fn start(transport: Box<dyn Transport>, index: usize) -> Option<Box<Rng>> {
        /* virtio-rng has no features of its own to ask for. */
        virtio::negotiate(transport.as_ref(), 0)?;

        let mut queue = virtio::setup_queue(transport.as_ref(), REQUEST_QUEUE, None).or_else(|| {
            virtio::failed(transport.as_ref());
            None
        })?;
        queue.disable_interrupts();

        virtio::driver_ok(transport.as_ref());

        let dma = match DmaBuffer::new(1) {
            Some(dma) => dma,
            None => {
                virtio::failed(transport.as_ref());
                return None;
            }
        };

        let lock = match SpinLock::new() {
            Some(lock) => lock,
            None => {
                virtio::failed(transport.as_ref());
                return None;
            }
        };

        let mut name = [0u8; 8];
        name[..3].copy_from_slice(b"rng");
        name[3] = b'0' + index as u8;

        Some(Box::new(Rng {
            transport,
            lock,
            inner: core::cell::UnsafeCell::new(Inner { queue, dma, stuck: false }),
            name,
        }))
    }

    /// Fill buf from the device, a page at a time. False if the device
    /// stopped answering, which leaves the pool to its other sources.
    fn fill(&self, buf: &mut [u8]) -> bool {
        let _guard = self.lock.lock();
        /* The lock is held for the whole of this, so nothing else is looking
         * at the queue or the buffer. */
        let inner = unsafe { &mut *self.inner.get() };

        if inner.stuck {
            if inner.queue.take_used().is_none() {
                trace!(0, "virtio-rng: a request from before is still with the device");
                return false;
            }
            inner.stuck = false;
        }

        let mut filled = 0;
        while filled < buf.len() {
            let chunk = core::cmp::min(buf.len() - filled, PAGE_SIZE);

            let phys = inner.dma.phys();
            if inner.queue.add(&[Buf::write(phys, chunk as u32)]).is_none() {
                trace!(0, "virtio-rng: no descriptor for a request");
                return false;
            }
            self.transport.notify(REQUEST_QUEUE);

            let mut rounds = 0;
            while !inner.queue.has_used() && rounds < POLL_ROUNDS {
                core::hint::spin_loop();
                rounds += 1;
            }

            let (_id, len) = match inner.queue.take_used() {
                Some(used) => used,
                None => {
                    trace!(0, "virtio-rng: the device did not answer");
                    inner.stuck = true;
                    return false;
                }
            };

            /* The device may give less than it was asked for, but none at
             * all means it has nothing to give. */
            let got = core::cmp::min(len as usize, chunk);
            if got == 0 {
                trace!(0, "virtio-rng: the device returned nothing");
                return false;
            }

            buf[filled..filled + got].copy_from_slice(&inner.dma.as_slice()[..got]);
            filled += got;
        }

        true
    }
}

/// What the kernel's entropy pool calls (kernel/entropy.h). Runs in task
/// context, from a reseed.
extern "C" fn get_random(ctx: *mut u8, buf: *mut u8, len: usize) -> i32 {
    if ctx.is_null() || buf.is_null() || len == 0 {
        return -1;
    }

    let rng = unsafe { &*(ctx as *const Rng) };
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    if rng.fill(out) {
        0
    } else {
        -1
    }
}

fn register(rng: Box<Rng>) {
    /* Taken before the box becomes a raw pointer: forming a reference to a
       field through that pointer is exactly what must not be done to memory
       the kernel is about to share. */
    let label = rng.name;
    let raw = Box::into_raw(rng);
    let name = unsafe { core::ptr::addr_of!((*raw).name) as *const u8 };

    /* The device is the kernel's for good: an entropy source cannot be
     * taken back, so neither the box nor the registration is ever dropped. */
    match entropy::register(name, get_random, raw as *mut u8) {
        Some(source) => {
            core::mem::forget(source);
            unsafe { DEVICES += 1 };
            trace!(0, "virtio-rng: {} is an entropy source",
                core::str::from_utf8(&label[..4]).unwrap_or("?"));
        }
        None => {
            trace!(0, "virtio-rng: the entropy table would not take another source");
            unsafe { drop(Box::from_raw(raw)) };
        }
    }
}

/// Find the virtio-rng devices on the PCI bus and give their entropy to the
/// pool. Called from rust_init.
#[cfg(target_arch = "x86_64")]
pub fn init() {
    use virtio::pci::PciTransport;

    for device in [pci::device::VIRTIO_RNG, pci::device::VIRTIO_RNG_MODERN] {
        let mut start = 0;
        while unsafe { DEVICES } < MAX_DEVICES {
            let (index, dev) = match pci::find_device_from(pci::vendor::VIRTIO, device, start) {
                Some(found) => found,
                None => break,
            };
            start = index + 1;

            dev.enable_bus_mastering();
            let transport = match PciTransport::probe(&dev) {
                Some(transport) => transport,
                None => {
                    trace!(0, "virtio-rng: {:02x}:{:02x}.{} is not a bus this speaks",
                        dev.bus, dev.slot, dev.func);
                    continue;
                }
            };

            trace!(0, "virtio-rng: {} virtio-pci at {:02x}:{:02x}.{}",
                if transport.is_legacy() { "legacy" } else { "modern" },
                dev.bus, dev.slot, dev.func);

            if let Some(rng) = Rng::start(Box::new(transport), unsafe { DEVICES }) {
                register(rng);
            }
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn init() {}

/// The same for the virtio-mmio windows the device tree described, which is
/// how arm64 has its devices. Called from the arm64 boot path with the slots
/// it has already looked at.
///
/// # Safety
/// `slots` points at `count` slots, each naming a mapped register window.
#[no_mangle]
pub unsafe extern "C" fn rust_virtio_rng_init_mmio(slots: *const Slot, count: usize) {
    if slots.is_null() {
        return;
    }

    let slots = unsafe { core::slice::from_raw_parts(slots, count) };
    for slot in slots {
        if unsafe { DEVICES } >= MAX_DEVICES {
            break;
        }
        if slot.device_id != virtio::device::RNG {
            continue;
        }

        let transport = match MmioTransport::probe(slot) {
            Some(transport) => transport,
            None => continue,
        };

        trace!(0, "virtio-rng: virtio-mmio at {:#x}", slot.base);

        if let Some(rng) = Rng::start(Box::new(transport), unsafe { DEVICES }) {
            register(rng);
        }
    }
}
