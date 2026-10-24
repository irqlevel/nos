//! A legacy virtio block device (virtio 0.9.5 over an I/O BAR, hv's disks:
//! src/rust/hv/src/devices/{virtio,blk}.rs), read a request at a time with
//! its interrupt never taken. The loader polls instead: each poll reads the
//! ISR, a port, which takes its CPU out of the guest -- and that is where
//! hv hands back what the disk has done. A request that is never answered
//! is a timeout said on the console, not a loader spinning for good.

use alloc::string::String;
use core::sync::atomic::{fence, Ordering};

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

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 0x80;

const F_SIZE_MAX: u32 = 1 << 1;
const F_SEG_MAX: u32 = 1 << 2;
const F_RO: u32 = 1 << 5;

const T_IN: u32 = 0;
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
pub const SECTOR: u64 = 512;
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
        let wanted = offered & (F_SIZE_MAX | F_SEG_MAX | F_RO);
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

    /// Read `buf.len()` bytes -- whole sectors -- from `sector` on.
    pub fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), String> {
        if buf.len() as u64 % SECTOR != 0 {
            return Err(alloc::format!("vd{}: a read of {} bytes, not whole sectors", self.letter, buf.len()));
        }
        let count = buf.len() as u64 / SECTOR;
        if sector.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(alloc::format!("vd{}: sectors {}..+{} past its end, {}", self.letter, sector, count,
                                      self.sectors));
        }
        let per_request = self.segment_bytes as usize * self.segments as usize;
        let mut done = 0usize;
        while done < buf.len() {
            let n = (buf.len() - done).min(per_request);
            self.request(sector + (done as u64 / SECTOR), &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// One request: its header, `buf` in segments, its status.
    fn request(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), String> {
        // SAFETY: the header block is this device's: 16 bytes of header and
        // the status byte after them, in a page this allocated.
        unsafe {
            core::ptr::write_volatile(self.header as *mut u32, T_IN);
            core::ptr::write_volatile(self.header.add(4) as *mut u32, 0);
            core::ptr::write_volatile(self.header.add(8) as *mut u64, sector);
            core::ptr::write_volatile(self.header.add(HEADER_BYTES), 0xFF);
        }
        let mut d = 0usize;
        self.descriptor(d, self.header as u64, HEADER_BYTES as u32, DESC_NEXT, 1);
        let seg = self.segment_bytes as usize;
        let mut at = 0usize;
        while at < buf.len() {
            let len = (buf.len() - at).min(seg);
            d += 1;
            let addr = buf[at..].as_mut_ptr() as u64;
            self.descriptor(d, addr, len as u32, DESC_WRITE | DESC_NEXT, (d + 1) as u16);
            at += len;
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
                return Err(alloc::format!("vd{}: no answer to a read of {} bytes at sector {} in {} s", self.letter,
                                          buf.len(), sector, TIMEOUT_SECS));
            }
            core::hint::spin_loop();
        }
        self.next_avail = want;
        fence(Ordering::SeqCst);
        // SAFETY: the status byte, which the device wrote before it put the
        // chain on the used ring.
        let status = unsafe { core::ptr::read_volatile(self.header.add(HEADER_BYTES)) };
        if status != S_OK {
            return Err(alloc::format!("vd{}: a read of {} bytes at sector {} failed, status {}", self.letter,
                                      buf.len(), sector, status));
        }
        Ok(())
    }

    /// Reset the device before the kernel takes over: it forgets its ring,
    /// and no write of its can reach memory the kernel has by then.
    pub fn reset(&self) {
        outb(self.io + STATUS, 0);
    }
}
