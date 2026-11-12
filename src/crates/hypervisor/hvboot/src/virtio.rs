//! A legacy virtio block device (virtio 0.9.5 over an I/O BAR, hv's disks:
//! src/crates/hypervisor/hv/src/devices/{virtio,blk}.rs), read a request at a time with
//! its interrupt never taken. The loader polls instead: each poll reads the
//! ISR, a port, which takes its CPU out of the guest -- and that is where
//! hv hands back what the disk has done. A request that is never answered
//! is a timeout said on the console, not a loader spinning for good.
//!
//! A device whose blocks are larger than a sector says so (`F_BLK_SIZE`) --
//! hv's for a volume of NBS's, of 4 KiB -- and hv holds every request to
//! whole blocks of it: such a disk is read and written in whole blocks, the
//! part of one a read or a write takes at either end through a block's
//! buffer of the disk's own -- a write's read first, changed, and written
//! whole (grubenv's, a kilobyte at its block's start).

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

use crate::disk::{Device, SECTOR};
use crate::io::{inb, inl, inw, outb, outl, outw, rdtsc};

/* The legacy register block, in the I/O BAR. */
const DEVICE_FEATURES: u16 = 0x00;
const DRIVER_FEATURES: u16 = 0x04;
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SIZE: u16 = 0x0C;
const QUEUE_SELECT: u16 = 0x0E;
const QUEUE_NOTIFY: u16 = 0x10;
const STATUS: u16 = 0x12;
const ISR: u16 = 0x13;
/// The device's configuration, MSI-X being off: the capacity in sectors,
/// then the largest segment and the most segments a request may have.
const CONFIG_CAPACITY: u16 = 0x14;
const CONFIG_SIZE_MAX: u16 = 0x1C;
const CONFIG_SEG_MAX: u16 = 0x20;
/// The device's block, in bytes, after the geometry (virtio 1.x, 5.2.4).
const CONFIG_BLK_SIZE: u16 = 0x28;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 0x80;

const F_SIZE_MAX: u32 = 1 << 1;
const F_SEG_MAX: u32 = 1 << 2;
const F_RO: u32 = 1 << 5;
const F_BLK_SIZE: u32 = 1 << 6;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const S_OK: u8 = 0;

/* The ring (virtio 0.9.5, 2.3): descriptors, the available ring right after
 * them, the used ring on the next page boundary. */
const DESC_BYTES: usize = 16;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
const PAGE: usize = 4096;
const PFN_SHIFT: u32 = 12;
/// The largest queue this drives: hv's is 256.
const MAX_QUEUE: u16 = 256;

/// A request's header: type, reserved, sector.
const HEADER_BYTES: usize = 16;
/// The most data descriptors a request carries, whatever the device
/// allows, and the bytes each, when it says no limit.
const MAX_SEGMENTS: u32 = 64;
const DEFAULT_SEGMENT: u32 = 4096;

/// How long a request may take before the loader gives up on it.
const TIMEOUT_SECS: u64 = 30;
/// The same, in polls, when the TSC's rate is not known.
const POLL_LIMIT: u64 = 1 << 26;

pub struct Disk {
    io: u16,
    /// vda, vdb, ... -- for what is said about it.
    pub letter: char,
    pub sectors: u64,
    pub read_only: bool,
    queue: u16,
    ring: *mut u8,
    used_at: usize,
    /// The request's header and status byte, in memory the device reads
    /// and writes.
    header: *mut u8,
    segment_bytes: u32,
    segments: u32,
    next_avail: u16,
    tsc_khz: Option<u64>,
    /// A request went unanswered: the device has been reset and nothing
    /// more is asked of it. Its chain was the device's still, with the
    /// addresses of buffers the loader had since given to something else
    /// -- a stack frame that had returned -- and the next request reused
    /// the slot and the index, so the late answer was taken for that one's.
    given_up: bool,
    /// The device's block, in sectors: 1, or what `F_BLK_SIZE` says.
    block_sectors: u64,
    /// A block's buffer, for the part of one a read or a write takes: empty
    /// where a block is a sector.
    bounce: Vec<u8>,
}

fn round_up(v: usize, to: usize) -> usize {
    (v + to - 1) & !(to - 1)
}

/// `len` zeroed bytes at `align` from the heap, kept for good: a ring the
/// device holds the address of is never given back.
fn alloc_dma(len: usize, align: usize) -> Result<*mut u8, String> {
    let layout = core::alloc::Layout::from_size_align(len, align).map_err(|_| String::from("a bad DMA layout"))?;
    // SAFETY: a layout of non-zero size; the block is the caller's for good.
    let p = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if p.is_null() {
        return Err(String::from("out of memory for a virtio ring"));
    }
    Ok(p)
}

impl Disk {
    /// Bring up the device at `io`: reset, acknowledged, features agreed,
    /// queue 0 given a ring, driver OK.
    pub fn new(io: u16, letter: char, tsc_khz: Option<u64>) -> Result<Disk, String> {
        outb(io + STATUS, 0);
        outb(io + STATUS, STATUS_ACKNOWLEDGE);
        outb(io + STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        let offered = inl(io + DEVICE_FEATURES);
        let wanted = offered & (F_SIZE_MAX | F_SEG_MAX | F_RO | F_BLK_SIZE);
        outl(io + DRIVER_FEATURES, wanted);

        outw(io + QUEUE_SELECT, 0);
        let queue = inw(io + QUEUE_SIZE);
        if queue < 4 || queue > MAX_QUEUE || !queue.is_power_of_two() {
            outb(io + STATUS, STATUS_FAILED);
            return Err(alloc::format!("vd{}: a queue of {} entries", letter, queue));
        }
        let n = usize::from(queue);
        let used_at = round_up(n * DESC_BYTES + 2 * (3 + n), PAGE);
        let ring_bytes = round_up(used_at + 6 + 8 * n, PAGE);
        let ring = alloc_dma(ring_bytes, PAGE)?;
        let header = alloc_dma(PAGE, PAGE)?;
        let segment_bytes = if wanted & F_SIZE_MAX != 0 { inl(io + CONFIG_SIZE_MAX) } else { DEFAULT_SEGMENT };
        let segments = if wanted & F_SEG_MAX != 0 { inl(io + CONFIG_SEG_MAX) } else { MAX_SEGMENTS };
        let sectors = u64::from(inl(io + CONFIG_CAPACITY)) | (u64::from(inl(io + CONFIG_CAPACITY + 4)) << 32);
        if segment_bytes < SECTOR as u32 || segments == 0 {
            outb(io + STATUS, STATUS_FAILED);
            return Err(alloc::format!("vd{}: requests of {} segments of {} bytes", letter, segments, segment_bytes));
        }
        /* A block of a power of two of sectors, up to a page -- what hv's
         * device says -- and a buffer of one where it is more than one. */
        let block = if wanted & F_BLK_SIZE != 0 { u64::from(inl(io + CONFIG_BLK_SIZE)) } else { SECTOR };
        if !block.is_power_of_two() || !(SECTOR..=PAGE as u64).contains(&block) {
            outb(io + STATUS, STATUS_FAILED);
            return Err(alloc::format!("vd{}: blocks of {} bytes", letter, block));
        }
        let mut bounce = Vec::new();
        if block > SECTOR {
            if bounce.try_reserve_exact(block as usize).is_err() {
                outb(io + STATUS, STATUS_FAILED);
                return Err(alloc::format!("vd{}: no memory for a block", letter));
            }
            bounce.resize(block as usize, 0);
        }

        let disk = Disk {
            io,
            letter,
            sectors,
            read_only: wanted & F_RO != 0,
            queue,
            ring,
            used_at,
            header,
            /* Whole sectors a segment, and no more segments than the ring
             * leaves room for beside the header's and the status's. */
            segment_bytes: (segment_bytes.min(1 << 20) / SECTOR as u32) * SECTOR as u32,
            segments: segments.min(MAX_SEGMENTS).min(u32::from(queue) - 2),
            next_avail: 0,
            tsc_khz,
            given_up: false,
            block_sectors: block / SECTOR,
            bounce,
        };
        disk.write16(n * DESC_BYTES, AVAIL_NO_INTERRUPT);
        outl(io + QUEUE_PFN, (ring as usize >> PFN_SHIFT) as u32);
        outb(io + STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);
        Ok(disk)
    }

    /* The ring's fields, written and read volatile: the device reads what
     * is written after the notify and writes the used ring at any time. */
    fn write16(&self, at: usize, v: u16) {
        // SAFETY: `at` is inside the ring this allocated (callers index it
        // by the queue's size), and the ring is this device's alone.
        unsafe { core::ptr::write_volatile(self.ring.add(at) as *mut u16, v) }
    }
    fn write32(&self, at: usize, v: u32) {
        // SAFETY: as `write16`.
        unsafe { core::ptr::write_volatile(self.ring.add(at) as *mut u32, v) }
    }
    fn write64(&self, at: usize, v: u64) {
        // SAFETY: as `write16`.
        unsafe { core::ptr::write_volatile(self.ring.add(at) as *mut u64, v) }
    }
    fn read16(&self, at: usize) -> u16 {
        // SAFETY: as `write16`.
        unsafe { core::ptr::read_volatile(self.ring.add(at) as *const u16) }
    }

    fn descriptor(&self, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
        let at = i * DESC_BYTES;
        self.write64(at, addr);
        self.write32(at + 8, len);
        self.write16(at + 12, flags);
        self.write16(at + 14, next);
    }

    /// `len` bytes at `addr` -- a buffer the caller holds for the call --
    /// read into from `sector` on, or written there: whole blocks of the
    /// device's, the first's first sector on, in requests of as many whole
    /// blocks as the device takes at once.
    fn transfer(&mut self, kind: u32, sector: u64, addr: u64, len: usize) -> Result<(), String> {
        let what = if kind == T_IN { "read" } else { "write" };
        if self.given_up {
            return Err(alloc::format!("vd{}: given up after a request it did not answer", self.letter));
        }
        if len as u64 % SECTOR != 0 {
            return Err(alloc::format!("vd{}: a {} of {} bytes, not whole sectors", self.letter, what, len));
        }
        let count = len as u64 / SECTOR;
        if sector.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(alloc::format!("vd{}: sectors {}..+{} past its end, {}", self.letter, sector, count,
                                      self.sectors));
        }
        let block = (self.block_sectors * SECTOR) as usize;
        if len % block != 0 || sector % self.block_sectors != 0 {
            return Err(alloc::format!("vd{}: a {} of {} bytes at sector {}, not whole blocks of {}", self.letter,
                                      what, len, sector, block));
        }
        let per_request = (self.segment_bytes as usize * self.segments as usize) / block * block;
        if per_request == 0 {
            return Err(alloc::format!("vd{}: a request holds less than a block of {}", self.letter, block));
        }
        let mut done = 0usize;
        while done < len {
            let n = (len - done).min(per_request);
            self.request(kind, sector + (done as u64 / SECTOR), addr + done as u64, n)?;
            done += n;
        }
        Ok(())
    }

    /// One request: its header, the `len` bytes at `addr` in segments --
    /// written by the device for a read, read by it for a write -- its
    /// status.
    fn request(&mut self, kind: u32, sector: u64, addr: u64, len: usize) -> Result<(), String> {
        let what = if kind == T_IN { "read" } else { "write" };
        // SAFETY: the header block is this device's: 16 bytes of header and
        // the status byte after them, in a page this allocated.
        unsafe {
            core::ptr::write_volatile(self.header as *mut u32, kind);
            core::ptr::write_volatile(self.header.add(4) as *mut u32, 0);
            core::ptr::write_volatile(self.header.add(8) as *mut u64, sector);
            core::ptr::write_volatile(self.header.add(HEADER_BYTES), 0xFF);
        }
        let mut d = 0usize;
        self.descriptor(d, self.header as u64, HEADER_BYTES as u32, DESC_NEXT, 1);
        let seg = self.segment_bytes as usize;
        let data = if kind == T_IN { DESC_WRITE | DESC_NEXT } else { DESC_NEXT };
        let mut at = 0usize;
        while at < len {
            let n = (len - at).min(seg);
            d += 1;
            self.descriptor(d, addr + at as u64, n as u32, data, (d + 1) as u16);
            at += n;
        }
        d += 1;
        self.descriptor(d, self.header as u64 + HEADER_BYTES as u64, 1, DESC_WRITE, 0);

        let n = usize::from(self.queue);
        let avail = n * DESC_BYTES;
        let slot = usize::from(self.next_avail % self.queue);
        self.write16(avail + 4 + 2 * slot, 0);
        fence(Ordering::SeqCst);
        let want = self.next_avail.wrapping_add(1);
        self.write16(avail + 2, want);
        fence(Ordering::SeqCst);
        outw(self.io + QUEUE_NOTIFY, 0);

        let start = rdtsc();
        let limit = self.tsc_khz.map(|khz| khz.saturating_mul(1000).saturating_mul(TIMEOUT_SECS));
        let mut polls = 0u64;
        loop {
            /* The exit this takes is where hv gives back what is done. */
            let _ = inb(self.io + ISR);
            fence(Ordering::SeqCst);
            if self.read16(self.used_at + 2) == want {
                break;
            }
            polls += 1;
            let late = match limit {
                Some(cycles) => rdtsc().wrapping_sub(start) > cycles,
                None => polls > POLL_LIMIT,
            };
            if late {
                /* Reset, which takes the chain back from the device -- hv
                 * gives nothing back for one a reset found -- before `buf`
                 * goes back to its owner; and given up for good. */
                self.reset();
                self.given_up = true;
                let waited = match limit {
                    Some(_) => alloc::format!("{} s", TIMEOUT_SECS),
                    None => alloc::format!("{} polls, the TSC's rate not given", POLL_LIMIT),
                };
                return Err(alloc::format!("vd{}: no answer to a {} of {} bytes at sector {} in {} -- reset, and \
                                           nothing more asked of it", self.letter, what, len, sector, waited));
            }
            core::hint::spin_loop();
        }
        self.next_avail = want;
        fence(Ordering::SeqCst);
        // SAFETY: the status byte, which the device wrote before it put the
        // chain on the used ring.
        let status = unsafe { core::ptr::read_volatile(self.header.add(HEADER_BYTES)) };
        if status != S_OK {
            return Err(alloc::format!("vd{}: a {} of {} bytes at sector {} failed, status {}", self.letter, what,
                                      len, sector, status));
        }
        Ok(())
    }

    /// Reset the device before the kernel takes over: it forgets its ring,
    /// and no write of its can reach memory the kernel has by then.
    pub fn reset(&self) {
        outb(self.io + STATUS, 0);
    }

    /// The block from `sector` on -- a block's first -- read into the
    /// block's buffer, or written from it.
    fn through_bounce(&mut self, kind: u32, sector: u64) -> Result<(), String> {
        let (addr, len) = (self.bounce.as_mut_ptr() as u64, self.bounce.len());
        self.transfer(kind, sector, addr, len)
    }
}

impl Device for Disk {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    /// Whole blocks of the device's straight into `buf`, the part of one at
    /// either end through the block's buffer.
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), String> {
        let per = self.block_sectors;
        if per == 1 {
            return self.transfer(T_IN, sector, buf.as_mut_ptr() as u64, buf.len());
        }
        if buf.len() as u64 % SECTOR != 0 {
            return Err(alloc::format!("vd{}: a read of {} bytes, not whole sectors", self.letter, buf.len()));
        }
        let (mut at, mut s) = (0usize, sector);
        while at < buf.len() {
            let into = s % per;
            let left = (buf.len() - at) as u64 / SECTOR;
            if into == 0 && left >= per {
                let len = ((left / per) * per * SECTOR) as usize;
                self.transfer(T_IN, s, buf[at..].as_mut_ptr() as u64, len)?;
                at += len;
                s += len as u64 / SECTOR;
            } else {
                /* Its block whole into the buffer, the part asked copied out */
                self.through_bounce(T_IN, s - into)?;
                let n = (per - into).min(left);
                let (from, len) = ((into * SECTOR) as usize, (n * SECTOR) as usize);
                buf[at..at + len].copy_from_slice(&self.bounce[from..from + len]);
                at += len;
                s += n;
            }
        }
        Ok(())
    }

    /// Refused for a disk its device says is read-only. grubenv's, as GRUB
    /// writes it: whole blocks of the device's straight from `buf`, the
    /// part of one at either end read into the block's buffer, changed and
    /// written whole.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), String> {
        if self.read_only {
            return Err(alloc::format!("vd{}: read-only", self.letter));
        }
        let per = self.block_sectors;
        if per == 1 {
            return self.transfer(T_OUT, sector, buf.as_ptr() as u64, buf.len());
        }
        if buf.len() as u64 % SECTOR != 0 {
            return Err(alloc::format!("vd{}: a write of {} bytes, not whole sectors", self.letter, buf.len()));
        }
        let (mut at, mut s) = (0usize, sector);
        while at < buf.len() {
            let into = s % per;
            let left = (buf.len() - at) as u64 / SECTOR;
            if into == 0 && left >= per {
                let len = ((left / per) * per * SECTOR) as usize;
                self.transfer(T_OUT, s, buf[at..].as_ptr() as u64, len)?;
                at += len;
                s += len as u64 / SECTOR;
            } else {
                /* Its block read whole, the part given put over it, the block
                   written back whole */
                self.through_bounce(T_IN, s - into)?;
                let n = (per - into).min(left);
                let (to, len) = ((into * SECTOR) as usize, (n * SECTOR) as usize);
                self.bounce[to..to + len].copy_from_slice(&buf[at..at + len]);
                self.through_bounce(T_OUT, s - into)?;
                at += len;
                s += n;
            }
        }
        Ok(())
    }
}
