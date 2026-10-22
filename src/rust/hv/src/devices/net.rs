//! A virtio network device, legacy PCI: a guest's `eth0`, over whatever
//! carries its frames (`Backend`) -- the module's switch.
//!
//! Two queues, as legacy has them: 0 the driver fills with buffers for what
//! the device receives, 1 with what it sends. Every frame either way comes
//! after the 10-byte header legacy puts in front of it, which says nothing
//! here -- no checksum offload and no segmentation are offered, so the guest
//! sends whole, checksummed frames of at most 1514 bytes, and is given the
//! same. A frame the guest sends is copied out and handed to the backend
//! before the notify returns; what the backend has for the guest is put into
//! its buffers each time round the run loop (`poll`) -- one frame held by the
//! device when the driver has posted none, so nothing is lost for want of a
//! buffer that is on its way.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::pci::Identity;
use super::virtio::{self, Asked, Broken, Queue, Raise, Seg, Transport};
use crate::memory::GuestMemory;
use crate::{Error, Result};

/// What carries a guest's frames: where what it sends goes, and where what
/// it is to receive comes from.
pub trait Backend: Send {
    /// A frame the guest sent, Ethernet header first.
    fn send(&mut self, frame: &[u8]);
    /// The next frame for the guest, into `buf` (`MAX_FRAME` bytes): its
    /// length, or None when there is none.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// Until `until_ns` on the boot clock, a frame for the guest need not
    /// wake the CPU that does the device's work: the device looks for what
    /// came by then on its own (`Net::rx_due`), its receive interrupts held
    /// till then anyway (`Net::set_rx_moderation`). Nothing to do for a
    /// backend that wakes nobody.
    fn quiet_until(&mut self, _until_ns: u64) {}
}

/// Virtio's type for a network device.
const VIRTIO_TYPE: u16 = 1;
/// Network, Ethernet.
const CLASS: u32 = 0x02_00_00;
/// The header and the configuration below, rounded up.
pub const BAR_SIZE: u32 = 0x40;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const QUEUE_SIZE: u16 = 256;
/// The most chains taken off a queue in one pass: a queue's worth. A driver
/// cannot have more than that available at once, so such a pass takes all
/// that was there when it began; what is posted while it runs -- by the
/// guest's other CPUs, into descriptors the pass gave back -- is the next
/// pass's: the notify that comes with it, or the next round of the devices
/// (`poll`). Unbounded, a guest that kept a ring topped up from a second
/// CPU, by plain writes and no exit, held the CPU running the pass in the
/// host for good, the platform's lock with it (hv-fuzz's racing CPU).
const PASS: usize = QUEUE_SIZE as usize;

/// Features: a MAC address of the device's, and a link status.
const F_MAC: u32 = 1 << 5;
const F_STATUS: u32 = 1 << 16;
const LINK_UP: u16 = 1;

/// The configuration: the MAC, then the status.
const CFG_MAC: usize = 0;
const CFG_STATUS: usize = 6;
const CFG_BYTES: usize = 8;

/// Legacy's header, before every frame.
const HDR: usize = 10;
/// The longest frame either way: Ethernet's, less the FCS.
pub const MAX_FRAME: usize = 1514;
const MIN_FRAME: usize = 14;

/// What the device has done, for a report.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub sent: u64,
    pub received: u64,
    /// Sent frames that were not frames: too short, or longer than was
    /// offered.
    pub dropped: u64,
}

pub struct Net {
    transport: Transport,
    backend: Box<dyn Backend>,
    mac: [u8; 6],
    segs: Vec<Seg>,
    /// A frame on its way either way, header included.
    tx_buf: Vec<u8>,
    /// The header, all zero -- nothing to say -- and the frame behind it:
    /// what goes into the guest's buffers as it lies.
    rx_buf: Vec<u8>,
    /// The length of a frame in `rx_buf` still waiting for a buffer.
    held: Option<usize>,
    /// A transmit pass ended at its bound, with chains perhaps still on the
    /// ring: the next round of the devices goes on with them.
    tx_more: bool,
    /// Receive interrupts, moderated (`set_rx_moderation`): no closer than
    /// this after the last -- 0, each at once; the next allowed at
    /// `rx_quiet_until`; and frames delivered since the last with none
    /// raised for them yet, owed one then.
    rx_quiet_ns: u64,
    rx_quiet_until: u64,
    rx_owed: bool,
    broken: Option<Broken>,
    pub stats: Stats,
}

impl Net {
    pub fn new(backend: Box<dyn Backend>, mac: [u8; 6]) -> Result<Net> {
        let mut queues = Vec::new();
        queues.try_reserve_exact(2).map_err(|_| Error::NoMemory)?;
        queues.push(Queue::new(QUEUE_SIZE));
        queues.push(Queue::new(QUEUE_SIZE));
        let mut segs = Vec::new();
        segs.try_reserve_exact(usize::from(QUEUE_SIZE)).map_err(|_| Error::NoMemory)?;
        let mut tx_buf = Vec::new();
        tx_buf.try_reserve_exact(HDR + MAX_FRAME).map_err(|_| Error::NoMemory)?;
        tx_buf.resize(HDR + MAX_FRAME, 0);
        let mut rx_buf = Vec::new();
        rx_buf.try_reserve_exact(HDR + MAX_FRAME).map_err(|_| Error::NoMemory)?;
        rx_buf.resize(HDR + MAX_FRAME, 0);
        Ok(Net {
            transport: Transport::new(F_MAC | F_STATUS, queues),
            backend,
            mac,
            segs,
            tx_buf,
            rx_buf,
            held: None,
            tx_more: false,
            rx_quiet_ns: 0,
            rx_quiet_until: 0,
            rx_owed: false,
            broken: None,
            stats: Stats::default(),
        })
    }

    pub fn identity() -> Identity {
        Identity {
            vendor: virtio::VENDOR,
            device: virtio::LEGACY_DEVICE_BASE + VIRTIO_TYPE - 1,
            revision: 0,
            class: CLASS,
            subsystem_vendor: virtio::VENDOR,
            subsystem: VIRTIO_TYPE,
        }
    }

    pub fn broken(&self) -> Option<Broken> {
        self.broken
    }

    /// Receive interrupts no closer than `ns` apart -- 0, as each frame
    /// comes -- as a real NIC's interrupt throttling has them. What arrives
    /// goes into the guest's buffers at once; its interrupt waits until the
    /// time since the last is up, and then one takes all that came. A frame
    /// after a quiet spell is interrupted for at once. Without it a guest
    /// that drains its ring faster than frames come takes an interrupt for
    /// every frame and acknowledges every one or two: two guests sending
    /// TCP through the switch exchanged five times the ACKs they did when
    /// halt polling held frames back by accident (docs/hypervisor.md).
    pub fn set_rx_moderation(&mut self, ns: u64) {
        self.rx_quiet_ns = ns;
    }

    /// When the device must be looked at again by its own round, if it is
    /// to keep what it promised its backend (`Backend::quiet_until`): the
    /// interrupt owed for what it delivered, and frames that came meanwhile
    /// and woke nobody. None when nothing is promised past `now`.
    pub fn rx_due(&self, now: u64) -> Option<u64> {
        (self.rx_quiet_until > now || self.rx_owed).then_some(self.rx_quiet_until)
    }

    fn config(&self) -> [u8; CFG_BYTES] {
        let mut c = [0u8; CFG_BYTES];
        c[CFG_MAC..CFG_MAC + 6].copy_from_slice(&self.mac);
        c[CFG_STATUS..CFG_STATUS + 2].copy_from_slice(&LINK_UP.to_le_bytes());
        c
    }

    /// Offer MSI-X, with a table of `entries`.
    pub fn offer_msix(&mut self, entries: u16) -> Option<()> {
        self.transport.offer_msix(entries)
    }

    /// Whether the function has MSI-X on, as its capability says now.
    pub fn set_msix_enabled(&mut self, on: bool) {
        self.transport.set_msix_enabled(on);
    }

    /// Whether it asserts its INTx line now: a level, for an IO-APIC's pin.
    pub fn line(&self) -> bool {
        self.transport.line()
    }

    /// A read of `size` bytes at `offset` in the BAR.
    pub fn io_read(&mut self, offset: u16, size: u8) -> u32 {
        let config = self.transport.config_offset();
        if offset < config {
            return self.transport.read(offset, size);
        }
        let c = self.config();
        let mut v = 0u32;
        for i in 0..u16::from(size) {
            if let Some(b) = c.get(usize::from(offset - config + i)) {
                v |= u32::from(*b) << (8 * i);
            }
        }
        v
    }

    /// A write of `size` bytes at `offset` in the BAR, at `now`, and the
    /// interrupts it leaves the device with.
    pub fn io_write(&mut self, offset: u16, size: u8, value: u32, mem: &GuestMemory, now: u64) -> Raise {
        if offset >= self.transport.config_offset() {
            return Raise::default();
        }
        match self.transport.write(offset, size, value) {
            Asked::Notify(TX_QUEUE) => self.send(mem),
            /* Buffers posted for receiving: whatever is waiting can go. */
            Asked::Notify(RX_QUEUE) => self.deliver(mem, now),
            Asked::Reset => {
                self.broken = None;
                self.held = None;
                self.tx_more = false;
                self.rx_owed = false;
                Raise::default()
            }
            _ => Raise::default(),
        }
    }

    /// What the driver has queued to send, to the backend: a pass's worth.
    fn send(&mut self, mem: &GuestMemory) -> Raise {
        self.tx_more = false;
        if self.broken.is_some() {
            return Raise::default();
        }
        let mut sent = false;
        let mut left = PASS;
        loop {
            if left == 0 {
                self.tx_more = true;
                break;
            }
            left -= 1;
            let mut segs = core::mem::take(&mut self.segs);
            let popped = match self.transport.queue(TX_QUEUE) {
                Some(q) if q.ready() => q.pop(mem, &mut segs),
                _ => Ok(None),
            };
            let result = match popped {
                Ok(Some(head)) => {
                    self.transmit(mem, &segs);
                    self.transport.queue(TX_QUEUE).ok_or(Broken::Index).and_then(|q| q.push(mem, head, 0))
                }
                Ok(None) => {
                    self.segs = segs;
                    break;
                }
                Err(e) => Err(e),
            };
            self.segs = segs;
            match result {
                Ok(()) => sent = true,
                Err(e) => {
                    self.broken = Some(e);
                    break;
                }
            }
        }
        let wants = sent && self.transport.queue(TX_QUEUE).map_or(false, |q| q.wants_interrupt(mem));
        if wants { self.transport.interrupt(TX_QUEUE) } else { Raise::default() }
    }

    /// One chain the driver sent: the header, then the frame, taken whole.
    fn transmit(&mut self, mem: &GuestMemory, segs: &[Seg]) {
        let total: u64 = segs.iter().filter(|s| !s.write).map(|s| u64::from(s.len)).sum();
        let len = total as usize;
        if total < (HDR + MIN_FRAME) as u64 || total > (HDR + MAX_FRAME) as u64 {
            self.stats.dropped += 1;
            return;
        }
        let mut done = 0usize;
        for s in segs.iter().filter(|s| !s.write) {
            let n = s.len as usize;
            /* A buffer of no bytes has none to take, wherever it says it is. */
            let Some(at) = s.at(0) else { continue };
            if mem.read(at, &mut self.tx_buf[done..done + n]).is_err() {
                self.stats.dropped += 1;
                return;
            }
            done += n;
        }
        self.backend.send(&self.tx_buf[HDR..len]);
        self.stats.sent += 1;
    }

    /// A round of the device at `now`, each time round the run loop: what a
    /// transmit pass left at its bound sent, and what the backend has for
    /// the guest delivered -- and the interrupts that leaves the device
    /// with, a receive interrupt owed among them once its time is up.
    pub fn poll(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        let sent = if self.tx_more { self.send(mem) } else { Raise::default() };
        sent.or(self.deliver(mem, now))
    }

    /// What the backend has for the guest, into the buffers the driver has
    /// posted -- a pass's worth -- and the interrupts that leaves the device
    /// with at `now`. What is left waits with the backend for the next round.
    fn deliver(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        if self.broken.is_some() || !self.transport.queue(RX_QUEUE).map_or(false, |q| q.ready()) {
            /* A queue taken down, or a ring the device stopped following,
             * is owed nothing: kept owing, it would be due for good, and
             * the round that looks at it run without a pause (hv-fuzz
             * found it). */
            self.rx_owed = false;
            return Raise::default();
        }
        let mut delivered = false;
        for _ in 0..PASS {
            let len = match self.held {
                Some(len) => len,
                None => match self.backend.recv(&mut self.rx_buf[HDR..]) {
                    Some(len) if (MIN_FRAME..=MAX_FRAME).contains(&len) => len,
                    Some(_) => continue,
                    None => break,
                },
            };
            let mut segs = core::mem::take(&mut self.segs);
            let popped = match self.transport.queue(RX_QUEUE) {
                Some(q) => q.pop(mem, &mut segs),
                None => Ok(None),
            };
            let result = match popped {
                Ok(Some(head)) => {
                    let written = self.receive(mem, &segs, len);
                    self.transport.queue(RX_QUEUE).ok_or(Broken::Index)
                        .and_then(|q| q.push(mem, head, written as u32))
                }
                Ok(None) => {
                    /* No buffer yet: the frame waits here for one. */
                    self.held = Some(len);
                    self.segs = segs;
                    break;
                }
                Err(e) => Err(e),
            };
            self.segs = segs;
            self.held = None;
            match result {
                Ok(()) => delivered = true,
                Err(e) => {
                    self.broken = Some(e);
                    break;
                }
            }
        }
        if delivered {
            self.rx_owed = true;
        }
        self.rx_interrupt(mem, now)
    }

    /// The receive interrupt owed for what was delivered, if its time is up
    /// at `now` -- and the next not before `rx_quiet_ns` from now, frames
    /// for the guest until then waking nobody (`Backend::quiet_until`): the
    /// device's round comes for them by then (`rx_due`). One the driver has
    /// said it does not want -- it is taking them already -- is not owed.
    fn rx_interrupt(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        if !self.rx_owed || now < self.rx_quiet_until {
            return Raise::default();
        }
        self.rx_owed = false;
        if !self.transport.queue(RX_QUEUE).map_or(false, |q| q.wants_interrupt(mem)) {
            return Raise::default();
        }
        if self.rx_quiet_ns != 0 {
            self.rx_quiet_until = now.saturating_add(self.rx_quiet_ns);
            self.backend.quiet_until(self.rx_quiet_until);
        }
        self.transport.interrupt(RX_QUEUE)
    }

    /// The header and the frame, as they lie in `rx_buf`, into the chain's
    /// writable buffers -- one copy a buffer, `GuestMemory` taking it a page
    /// at a time: what was written, or 0 when they are too short for it --
    /// the frame then dropped. It went a byte at a time once, through a
    /// 256-byte bounce buffer whose every piece was mapped on its own -- host
    /// work on the vCPU's own CPU, for every frame, while the guest waited.
    fn receive(&mut self, mem: &GuestMemory, segs: &[Seg], len: usize) -> usize {
        let total = HDR + len;
        let room: u64 = segs.iter().filter(|s| s.write).map(|s| u64::from(s.len)).sum();
        if room < total as u64 {
            self.stats.dropped += 1;
            return 0;
        }
        let mut done = 0usize;
        for s in segs.iter().filter(|s| s.write) {
            if done == total {
                break;
            }
            let n = (total - done).min(s.len as usize);
            let Some(at) = s.at(0) else { continue };
            if mem.write(at, &self.rx_buf[done..done + n]).is_err() {
                self.stats.dropped += 1;
                return 0;
            }
            done += n;
        }
        self.stats.received += 1;
        total
    }
}
