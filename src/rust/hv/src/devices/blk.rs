//! A virtio block device, legacy PCI: a guest's `/dev/vda`, over whatever
//! stores its bytes (`Backend`) -- a file of nos's, in the module.
//!
//! A request is a chain of buffers: a 16-byte header the driver wrote (the
//! type and, for a read or a write, the sector), the data, and a status byte
//! the device writes last. It is handled whatever the layout -- the header,
//! the data and the status taken as streams across the chain's readable and
//! writable buffers -- and served before the notify that made it available
//! returns: the vCPU waits for its disk, as a guest's does for a device with
//! no queue of its own. The data crosses through a buffer of this device's,
//! `BOUNCE_BYTES` at a time, so a request of any size needs nothing
//! allocated on the way.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::pci::Identity;
use super::virtio::{self, Asked, Broken, Queue, Seg, Transport};
use crate::memory::GuestMemory;
use crate::{Error, Result};

/// What stores a disk's bytes. Offsets and lengths are the device's to keep
/// within `size`, which is a whole number of sectors.
pub trait Backend: Send {
    fn size(&self) -> u64;
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> bool;
    fn write(&mut self, offset: u64, data: &[u8]) -> bool;
    /// Everything written, on stable storage.
    fn flush(&mut self) -> bool;
    /// A disk the guest may only read: the device says so to the driver,
    /// and refuses a write itself rather than hand it here.
    fn read_only(&self) -> bool {
        false
    }
}

/// Virtio's type for a block device.
const VIRTIO_TYPE: u16 = 2;
/// Mass storage, SCSI -- what a legacy virtio disk has always said it is.
const CLASS: u32 = 0x01_00_00;
/// Its I/O BAR: the header and the configuration below, rounded up.
pub const BAR_SIZE: u32 = 0x40;

pub const SECTOR: u64 = 512;
/// The queue's size.
const QUEUE_SIZE: u16 = 128;

/// Features: flush, a limit on segments a request may have, and a disk
/// that is read-only.
const F_SEG_MAX: u32 = 1 << 2;
const F_RO: u32 = 1 << 5;
const F_FLUSH: u32 = 1 << 9;
/// The header, the status, and the data between.
const SEG_MAX: u32 = QUEUE_SIZE as u32 - 2;

/// Request types, and statuses.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;
const HEADER: usize = 16;
/// What GET_ID answers with, at most: the id's length in the spec.
const ID_BYTES: usize = 20;

/// The configuration, by offset past the header.
const CFG_CAPACITY: u16 = 0;
const CFG_SEG_MAX: u16 = 12;
const CFG_BYTES: usize = 24;

/// How much data crosses at a time.
const BOUNCE_BYTES: usize = 64 * 1024;

/// What the device has done, for a report.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub errors: u64,
}

pub struct Blk {
    transport: Transport,
    backend: Box<dyn Backend>,
    segs: Vec<Seg>,
    bounce: Vec<u8>,
    id: [u8; ID_BYTES],
    /// Set when the driver made a ring this device will not follow: no more
    /// requests are taken until it resets the device.
    broken: Option<Broken>,
    pub stats: Stats,
}

impl Blk {
    /// A disk of `backend`'s size -- whole sectors of it -- named `id` for
    /// the driver's GET_ID.
    pub fn new(backend: Box<dyn Backend>, id: &str) -> Result<Blk> {
        let mut queues = Vec::new();
        queues.try_reserve_exact(1).map_err(|_| Error::NoMemory)?;
        queues.push(Queue::new(QUEUE_SIZE));
        let mut segs = Vec::new();
        segs.try_reserve_exact(usize::from(QUEUE_SIZE)).map_err(|_| Error::NoMemory)?;
        let mut bounce = Vec::new();
        bounce.try_reserve_exact(BOUNCE_BYTES).map_err(|_| Error::NoMemory)?;
        bounce.resize(BOUNCE_BYTES, 0);
        let mut name = [0u8; ID_BYTES];
        let n = id.len().min(ID_BYTES);
        name[..n].copy_from_slice(&id.as_bytes()[..n]);
        let ro = if backend.read_only() { F_RO } else { 0 };
        Ok(Blk {
            transport: Transport::new(F_SEG_MAX | F_FLUSH | ro, queues),
            backend,
            segs,
            bounce,
            id: name,
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

    /// Its size, in sectors.
    pub fn sectors(&self) -> u64 {
        self.backend.size() / SECTOR
    }

    /// What was wrong with the ring, if the device stopped over one.
    pub fn broken(&self) -> Option<Broken> {
        self.broken
    }

    fn config(&self) -> [u8; CFG_BYTES] {
        let mut c = [0u8; CFG_BYTES];
        let at = usize::from(CFG_CAPACITY);
        c[at..at + 8].copy_from_slice(&self.sectors().to_le_bytes());
        let at = usize::from(CFG_SEG_MAX);
        c[at..at + 4].copy_from_slice(&SEG_MAX.to_le_bytes());
        c
    }

    /// A read of `size` bytes at `offset` in the BAR.
    pub fn io_read(&mut self, offset: u16, size: u8) -> u32 {
        if offset < virtio::DEVICE_CONFIG {
            return self.transport.read(offset, size);
        }
        let c = self.config();
        let mut v = 0u32;
        for i in 0..u16::from(size) {
            let at = usize::from(offset - virtio::DEVICE_CONFIG + i);
            if let Some(b) = c.get(at) {
                v |= u32::from(*b) << (8 * i);
            }
        }
        v
    }

    /// A write of `size` bytes at `offset` in the BAR; true when the
    /// device's interrupt line is to go up. The configuration is read-only.
    pub fn io_write(&mut self, offset: u16, size: u8, value: u32, mem: &mut GuestMemory) -> bool {
        if offset >= virtio::DEVICE_CONFIG {
            return false;
        }
        match self.transport.write(offset, size, value) {
            Asked::Notify(0) => self.serve(mem),
            Asked::Reset => {
                self.broken = None;
                false
            }
            _ => false,
        }
    }

    /// Every request the driver has made available: served, given back, and
    /// -- if the driver wants one and none is waiting already -- an interrupt.
    fn serve(&mut self, mem: &mut GuestMemory) -> bool {
        if self.broken.is_some() {
            return false;
        }
        let mut served = false;
        loop {
            let mut segs = core::mem::take(&mut self.segs);
            let popped = match self.transport.queue(0) {
                Some(q) if q.ready() => q.pop(mem, &mut segs),
                _ => Ok(None),
            };
            let result = match popped {
                Ok(Some(head)) => {
                    let (status, written) = self.request(mem, &segs);
                    self.finish(mem, &segs, head, status, written)
                }
                Ok(None) => {
                    self.segs = segs;
                    break;
                }
                Err(e) => Err(e),
            };
            self.segs = segs;
            match result {
                Ok(()) => served = true,
                Err(e) => {
                    self.broken = Some(e);
                    break;
                }
            }
        }
        let wants = served && self.transport.queue(0).map_or(false, |q| q.wants_interrupt(mem));
        wants && self.transport.interrupt()
    }

    /// Write the status -- the last byte the device may write -- and give
    /// the chain back with what was written.
    fn finish(&mut self, mem: &mut GuestMemory, segs: &[Seg], head: u16, status: u8, written: u32) -> core::result::Result<(), Broken> {
        let last = segs.iter().rev().find(|s| s.write && s.len != 0).ok_or(Broken::Memory)?;
        mem.write(last.addr + u64::from(last.len) - 1, &[status]).map_err(|_| Broken::Memory)?;
        let q = self.transport.queue(0).ok_or(Broken::Index)?;
        q.push(mem, head, written.saturating_add(1))
    }

    /// Serve one request: its status, and how many data bytes it wrote into
    /// the chain.
    fn request(&mut self, mem: &mut GuestMemory, segs: &[Seg]) -> (u8, u32) {
        let mut header = [0u8; HEADER];
        if !copy_out(mem, segs, 0, &mut header) {
            self.stats.errors += 1;
            return (S_IOERR, 0);
        }
        let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let sector = u64::from_le_bytes([header[8], header[9], header[10], header[11],
                                         header[12], header[13], header[14], header[15]]);
        /* Everything the device may write but the status byte. */
        let writable: u64 = segs.iter().filter(|s| s.write).map(|s| u64::from(s.len)).sum();
        let room = writable.saturating_sub(1);
        let readable: u64 = segs.iter().filter(|s| !s.write).map(|s| u64::from(s.len)).sum();

        match kind {
            T_IN => {
                let Some(offset) = self.range(sector, room) else {
                    self.stats.errors += 1;
                    return (S_IOERR, 0);
                };
                let mut done = 0u64;
                while done < room {
                    let n = (room - done).min(BOUNCE_BYTES as u64) as usize;
                    if !self.backend.read(offset + done, &mut self.bounce[..n])
                        || !copy_in(mem, segs, done, &self.bounce[..n])
                    {
                        self.stats.errors += 1;
                        return (S_IOERR, clamp32(done));
                    }
                    done += n as u64;
                }
                self.stats.reads += 1;
                self.stats.read_bytes += room;
                (S_OK, clamp32(room))
            }
            /* A driver that was told the disk is read-only does not write
             * to it; one that does anyway is told no. */
            T_OUT if self.backend.read_only() => {
                self.stats.errors += 1;
                (S_IOERR, 0)
            }
            T_OUT => {
                let data = readable.saturating_sub(HEADER as u64);
                let Some(offset) = self.range(sector, data) else {
                    self.stats.errors += 1;
                    return (S_IOERR, 0);
                };
                let mut done = 0u64;
                while done < data {
                    let n = (data - done).min(BOUNCE_BYTES as u64) as usize;
                    if !copy_out(mem, segs, HEADER as u64 + done, &mut self.bounce[..n])
                        || !self.backend.write(offset + done, &self.bounce[..n])
                    {
                        self.stats.errors += 1;
                        return (S_IOERR, 0);
                    }
                    done += n as u64;
                }
                self.stats.writes += 1;
                self.stats.written_bytes += data;
                (S_OK, 0)
            }
            T_FLUSH => {
                self.stats.flushes += 1;
                if self.transport.driver_features() & F_FLUSH != 0 && self.backend.flush() {
                    (S_OK, 0)
                } else {
                    self.stats.errors += 1;
                    (S_IOERR, 0)
                }
            }
            T_GET_ID => {
                let n = (room as usize).min(ID_BYTES);
                if copy_in(mem, segs, 0, &self.id[..n]) { (S_OK, n as u32) } else { (S_IOERR, 0) }
            }
            _ => (S_UNSUPP, 0),
        }
    }

    /// The byte offset of `sector`, when `len` bytes from there are whole
    /// sectors within the disk.
    fn range(&self, sector: u64, len: u64) -> Option<u64> {
        let offset = sector.checked_mul(SECTOR)?;
        let end = offset.checked_add(len)?;
        (len % SECTOR == 0 && end <= self.backend.size()).then_some(offset)
    }
}

/// A count of bytes, as the used ring's 32 bits hold it.
fn clamp32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Copy `buf.len()` bytes of the chain's readable stream, from `skip` bytes
/// into it, out of guest memory.
fn copy_out(mem: &GuestMemory, segs: &[Seg], skip: u64, buf: &mut [u8]) -> bool {
    let mut skip = skip;
    let mut done = 0usize;
    for s in segs.iter().filter(|s| !s.write) {
        if done == buf.len() {
            break;
        }
        let len = u64::from(s.len);
        if skip >= len {
            skip -= len;
            continue;
        }
        let n = ((len - skip) as usize).min(buf.len() - done);
        if mem.read(s.addr + skip, &mut buf[done..done + n]).is_err() {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == buf.len()
}

/// Copy `data` into the chain's writable stream, from `skip` bytes into it.
fn copy_in(mem: &mut GuestMemory, segs: &[Seg], skip: u64, data: &[u8]) -> bool {
    let mut skip = skip;
    let mut done = 0usize;
    for s in segs.iter().filter(|s| s.write) {
        if done == data.len() {
            break;
        }
        let len = u64::from(s.len);
        if skip >= len {
            skip -= len;
            continue;
        }
        let n = ((len - skip) as usize).min(data.len() - done);
        if mem.write(s.addr + skip, &data[done..done + n]).is_err() {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == data.len()
}
