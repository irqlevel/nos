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

    /// A write of `size` bytes at `offset` in the BAR, and the interrupts it
    /// leaves the device with.
    pub fn io_write(&mut self, offset: u16, size: u8, value: u32, mem: &GuestMemory) -> Raise {
        if offset >= self.transport.config_offset() {
            return Raise::default();
        }
        match self.transport.write(offset, size, value) {
            Asked::Notify(TX_QUEUE) => self.send(mem),
            /* Buffers posted for receiving: whatever is waiting can go. */
            Asked::Notify(RX_QUEUE) => self.poll(mem),
            Asked::Reset => {
                self.broken = None;
                self.held = None;
                Raise::default()
            }
            _ => Raise::default(),
        }
    }

    /// Everything the driver has queued to send, to the backend.
    fn send(&mut self, mem: &GuestMemory) -> Raise {
        if self.broken.is_some() {
            return Raise::default();
        }
        let mut sent = false;
        loop {
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
            if mem.read(s.addr, &mut self.tx_buf[done..done + n]).is_err() {
                self.stats.dropped += 1;
                return;
            }
            done += n;
        }
        self.backend.send(&self.tx_buf[HDR..len]);
        self.stats.sent += 1;
    }

    /// What the backend has for the guest, into the buffers the driver has
    /// posted, and the interrupts that leaves the device with.
    pub fn poll(&mut self, mem: &GuestMemory) -> Raise {
        if self.broken.is_some() || !self.transport.queue(RX_QUEUE).map_or(false, |q| q.ready()) {
            return Raise::default();
        }
        let mut delivered = false;
        loop {
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
        let wants = delivered && self.transport.queue(RX_QUEUE).map_or(false, |q| q.wants_interrupt(mem));
        if wants { self.transport.interrupt(RX_QUEUE) } else { Raise::default() }
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
            if mem.write(s.addr, &self.rx_buf[done..done + n]).is_err() {
                self.stats.dropped += 1;
                return 0;
            }
            done += n;
        }
        self.stats.received += 1;
        total
    }
}
