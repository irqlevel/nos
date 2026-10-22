//! Virtio's legacy PCI transport, and its split virtqueues: the device side of
//! what `drivers/virtio_*` are the driver side of.
//!
//! Legacy, because it keeps every register in an I/O BAR -- so a guest
//! reaches the device with `in` and `out` and nothing to decode -- and every
//! Linux has the driver for it (`virtio_pci_legacy`). Its header, by offset
//! in the BAR: the device's features, the driver's, the selected queue's page
//! frame number, its size, the queue select, the notify register, the device
//! status and the interrupt status; the device's own configuration follows at
//! 0x14 -- or, once the driver has turned MSI-X on for the function, two
//! registers more come first, the vector for configuration changes and the
//! selected queue's, and the configuration follows at 0x18. With MSI-X a
//! queue's interrupt is a message, its table entry's, and the interrupt
//! status is not used.
//!
//! A queue is three rings in guest memory, laid out from its page frame
//! number: the descriptors, the driver's available ring, and -- on the next
//! 4 KiB boundary -- the device's used ring. Everything in them is the
//! guest's to write at any moment, so every index, address and length read
//! from them is checked before it is used: a descriptor past the queue's
//! size, a chain longer than the queue (a loop), a buffer outside guest
//! memory make the ring `Broken`, and a device that finds one stops using the
//! queue until the driver resets it -- rather than the host trusting it.

use alloc::vec::Vec;

use crate::memory::{GuestMemory, Mapping};

/// The legacy header, by offset in the BAR.
const DEVICE_FEATURES: u16 = 0x00;
const DRIVER_FEATURES: u16 = 0x04;
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SIZE: u16 = 0x0C;
const QUEUE_SELECT: u16 = 0x0E;
const QUEUE_NOTIFY: u16 = 0x10;
const DEVICE_STATUS: u16 = 0x12;
const ISR_STATUS: u16 = 0x13;
/// Where the device's own configuration begins, with no MSI-X.
pub const DEVICE_CONFIG: u16 = 0x14;
/// With MSI-X on: the vector for configuration changes, the selected queue's,
/// and where the configuration begins then.
const CONFIG_VECTOR: u16 = 0x14;
const QUEUE_VECTOR: u16 = 0x16;
const DEVICE_CONFIG_MSIX: u16 = 0x18;
/// A vector register's "none": no interrupt for it -- and what reads back
/// from one a driver set to an entry the table has not got.
pub const NO_VECTOR: u16 = 0xFFFF;

/// The PCI vendor every virtio device has.
pub const VENDOR: u16 = 0x1AF4;
/// A legacy device's PCI device id is this plus its virtio type, less one.
pub const LEGACY_DEVICE_BASE: u16 = 0x1000;

/// The interrupt status's bit for "a queue has used buffers".
const ISR_QUEUE: u8 = 1;
/// A queue's used ring starts on this boundary, and its page frame number is
/// in pages of this size.
const QUEUE_ALIGN: usize = 4096;
const PFN_SHIFT: u32 = 12;

/// Descriptor flags.
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const DESC_INDIRECT: u16 = 4;
/// A descriptor's size in the table, and a used element's.
const DESC_SIZE: usize = 16;
const USED_ELEM_SIZE: usize = 8;
/// The available ring's flag: the driver wants no interrupt.
const AVAIL_NO_INTERRUPT: u16 = 1;

/// A driver's buffer, from a descriptor: where it is, how long, and whether
/// the device writes it (or reads it). Where it is is the guest's to say --
/// up to the top of the address space and past it -- so it is reached only
/// through `at`, which does the sum checked: `addr + offset` done by hand
/// was a panic in a kernel that checks overflow and a wrap to page 0 in one
/// that does not, for any guest that asked for either (hv-fuzz found it).
#[derive(Clone, Copy)]
pub struct Seg {
    addr: u64,
    pub len: u32,
    pub write: bool,
}

impl Seg {
    /// The guest physical address `offset` bytes into the buffer: None at or
    /// past its end, and past the end of the address space.
    #[inline]
    pub fn at(&self, offset: u64) -> Option<u64> {
        if offset < u64::from(self.len) { self.addr.checked_add(offset) } else { None }
    }
}

/// What the driver has made available for a frame (`Queue::room_ahead`):
/// enough, in so many chains; not yet; or not in as many as it may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Room {
    Enough(u16),
    Wait,
    Never,
}

/// A ring the driver made that the device will not follow.
#[derive(Clone, Copy, Debug)]
pub enum Broken {
    /// A descriptor index past the queue's size.
    Index,
    /// A chain longer than the queue: a loop.
    Loop,
    /// More chains available than the queue holds.
    Overrun,
    /// An indirect descriptor, which was not offered.
    Indirect,
    /// A ring, or a buffer, outside guest memory.
    Memory,
}

/// One split virtqueue, by what the device keeps of it.
pub struct Queue {
    /// Its size: the device's, fixed, a power of two.
    size: u16,
    /// Its page frame number; 0 while the driver has not set it up.
    pfn: u32,
    /// The next available entry to take, and the next used one to fill.
    next_avail: u16,
    next_used: u16,
    /// Its rings mapped into the host's space, from when the driver gives
    /// their page frame number until it takes the queue down
    /// (`GuestMemory::map`): read and written at every chain, where a copy
    /// through the kernel's window maps and unmaps a page -- a TLB flush --
    /// each time. None while the queue is down, and when no mapping was to
    /// be had (`Maps`): the rings are copied as they always were then.
    mapped: Option<Mapping>,
}

impl Queue {
    pub fn new(size: u16) -> Queue {
        Queue { size, pfn: 0, next_avail: 0, next_used: 0, mapped: None }
    }

    fn reset(&mut self) {
        self.pfn = 0;
        self.next_avail = 0;
        self.next_used = 0;
        self.mapped = None;
    }

    /// Set up by the driver at `pfn`: its rings mapped from there when `map`
    /// says they may be, and when they are all in the guest's memory.
    fn set_pfn(&mut self, pfn: u32, mem: &GuestMemory, map: bool) {
        self.pfn = pfn;
        self.next_avail = 0;
        self.next_used = 0;
        /* The old rings' mapping undone before a new one is made. */
        self.mapped = None;
        if map {
            self.mapped = mem.map(self.base(), self.ring_bytes()).ok();
        }
    }

    pub fn ready(&self) -> bool {
        self.pfn != 0
    }


    fn base(&self) -> u64 {
        u64::from(self.pfn) << PFN_SHIFT
    }

    /// Where each ring is, in bytes from the descriptors: the available
    /// ring right after them, the used ring on the next 4 KiB boundary past
    /// the available one's flags, index, entries and used_event -- and the
    /// used ring's end, past its avail_event.
    fn avail_at(&self) -> usize {
        DESC_SIZE * usize::from(self.size)
    }

    fn used_at(&self) -> usize {
        (self.avail_at() + 6 + 2 * usize::from(self.size)).next_multiple_of(QUEUE_ALIGN)
    }

    fn ring_bytes(&self) -> usize {
        self.used_at() + 6 + USED_ELEM_SIZE * usize::from(self.size)
    }

    fn rings<'a>(&'a self, mem: &'a GuestMemory) -> Rings<'a> {
        Rings { mapped: self.mapped.as_ref(), mem, base: self.base() }
    }

    /// The next chain the driver has made available: its head, its buffers
    /// onto `segs` (cleared first; its capacity is the queue's size, so
    /// nothing here allocates) -- or `Ok(None)` when there is none.
    pub fn pop(&mut self, mem: &GuestMemory, segs: &mut Vec<Seg>) -> Result<Option<u16>, Broken> {
        if self.waiting(mem)? == 0 {
            segs.clear();
            return Ok(None);
        }
        let head = self.chain(mem, self.next_avail, segs)?;
        self.next_avail = self.next_avail.wrapping_add(1);
        Ok(Some(head))
    }

    /// How many chains the driver has made available and the device not
    /// taken: never more than the queue holds.
    fn waiting(&self, mem: &GuestMemory) -> Result<u16, Broken> {
        let avail_idx = self.rings(mem).word(self.avail_at() + 2)?;
        let waiting = avail_idx.wrapping_sub(self.next_avail);
        if waiting > self.size {
            return Err(Broken::Overrun);
        }
        Ok(waiting)
    }

    /// Whether the chains made available, from the next one on and `most`
    /// of them at most, have writable buffers enough for `need` bytes, with
    /// nothing taken: for a frame spread over as many as it needs (mergeable
    /// receive buffers), which goes in whole, waits for more, or -- `most`
    /// of them falling short -- never goes. `most` bounds the walk too, which
    /// a guest's chains of no room would otherwise make the whole ring's at
    /// every look. `segs` is scratch.
    pub fn room_ahead(&self, mem: &GuestMemory, need: u64, most: u16, segs: &mut Vec<Seg>) -> Result<Room, Broken> {
        let waiting = self.waiting(mem)?;
        let mut room = 0u64;
        for k in 0..waiting.min(most) {
            self.chain(mem, self.next_avail.wrapping_add(k), segs)?;
            room += segs.iter().filter(|s| s.write).map(|s| u64::from(s.len)).sum::<u64>();
            if room >= need {
                return Ok(Room::Enough(k + 1));
            }
        }
        Ok(if waiting >= most { Room::Never } else { Room::Wait })
    }

    /// The chain whose head is in the available ring's entry `position`, its
    /// buffers onto `segs` (cleared first): its head.
    fn chain(&self, mem: &GuestMemory, position: u16, segs: &mut Vec<Seg>) -> Result<u16, Broken> {
        segs.clear();
        let rings = self.rings(mem);
        let slot = usize::from(position % self.size);
        let head = rings.half(self.avail_at() + 4 + 2 * slot)?;

        let mut index = head;
        loop {
            if index >= self.size {
                return Err(Broken::Index);
            }
            if segs.len() >= usize::from(self.size) {
                return Err(Broken::Loop);
            }
            let (addr, rest) = rings.desc(DESC_SIZE * usize::from(index))?;
            let len = rest as u32;
            let flags = (rest >> 32) as u16;
            let next = (rest >> 48) as u16;
            if flags & DESC_INDIRECT != 0 {
                return Err(Broken::Indirect);
            }
            /* The capacity is the queue's size, checked above. */
            segs.push(Seg { addr, len, write: flags & DESC_WRITE != 0 });
            if flags & DESC_NEXT == 0 {
                break;
            }
            index = next;
        }
        Ok(head)
    }

    /// Give the chain at `head` back, `len` bytes of it written by the
    /// device: the element first, then the index that publishes it -- a
    /// store of the whole word, with release order, after the element, which
    /// the driver reads on another CPU while it runs.
    pub fn push(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<(), Broken> {
        let used = self.used_at();
        let elem = used + 4 + USED_ELEM_SIZE * usize::from(self.next_used % self.size);
        let next = self.next_used.wrapping_add(1);
        let rings = self.rings(mem);
        rings.put_u32(elem, u32::from(head))?;
        rings.put_u32(elem + 4, len)?;
        let published = rings.put_word(used + 2, next);
        self.next_used = next;
        published
    }

    /// Whether the driver wants an interrupt for what was given back -- read
    /// after a full fence. The driver turns interrupts back on and then
    /// looks for what came meanwhile; without the fence, the device's load
    /// of its flag can pass its own store of the used index, still in the
    /// CPU's store buffer, and each sees the other's old value: the driver
    /// waits for an interrupt the device decided not to send. A copy through
    /// the kernel's window fenced it by accident -- the unmap's INVLPG is a
    /// serializing instruction -- which the mapped rings do not.
    pub fn wants_interrupt(&self, mem: &GuestMemory) -> bool {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.rings(mem).word(self.avail_at()).map_or(true, |flags| flags & AVAIL_NO_INTERRUPT == 0)
    }
}

/// A queue's rings, as the device reaches them: mapped when they are, and
/// copied through the kernel's window from `base` when not. Every value
/// read is the guest's to have written anything into, and is checked by
/// whoever reads it.
struct Rings<'a> {
    mapped: Option<&'a Mapping>,
    mem: &'a GuestMemory,
    base: u64,
}

impl Rings<'_> {
    fn gpa(&self, at: usize) -> u64 {
        self.base + at as u64
    }

    /// A word of a ring, read whole with acquire order: an index or the
    /// flags, which the driver stores on another CPU while the device reads
    /// -- a copy of its two bytes could take one from either side of that
    /// store, and an index read so, 0x01FF for 0x0100, is a ring that looks
    /// overrun -- and what it published before the word, there after it.
    fn word(&self, at: usize) -> Result<u16, Broken> {
        match self.mapped {
            Some(m) => m.load_u16(at).ok_or(Broken::Memory),
            None => self.mem.read_u16(self.gpa(at)).map_err(|_| Broken::Memory),
        }
    }

    /// A word of a ring that no other word published: an entry of the
    /// available ring.
    fn half(&self, at: usize) -> Result<u16, Broken> {
        match self.mapped {
            Some(m) => m.load::<u16>(at).ok_or(Broken::Memory),
            None => self.mem.read_u16(self.gpa(at)).map_err(|_| Broken::Memory),
        }
    }

    /// The descriptor at `at`: its address, and the eight bytes after --
    /// length, flags and next, low to high.
    fn desc(&self, at: usize) -> Result<(u64, u64), Broken> {
        match self.mapped {
            Some(m) => Ok((m.load::<u64>(at).ok_or(Broken::Memory)?, m.load::<u64>(at + 8).ok_or(Broken::Memory)?)),
            None => {
                let mut d = [0u8; DESC_SIZE];
                self.mem.read(self.gpa(at), &mut d).map_err(|_| Broken::Memory)?;
                let word = |i: usize| u64::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3], d[i + 4], d[i + 5], d[i + 6], d[i + 7]]);
                Ok((word(0), word(8)))
            }
        }
    }

    fn put_u32(&self, at: usize, value: u32) -> Result<(), Broken> {
        let done = match self.mapped {
            Some(m) => m.store::<u32>(at, value),
            None => self.mem.write(self.gpa(at), &value.to_le_bytes()).is_ok(),
        };
        if done { Ok(()) } else { Err(Broken::Memory) }
    }

    /// A word stored whole, with release order: what was written before it
    /// is there before it, for a driver on another CPU that reads the word
    /// and then what it publishes.
    fn put_word(&self, at: usize, value: u16) -> Result<(), Broken> {
        let done = match self.mapped {
            Some(m) => m.store_u16(at, value),
            None => self.mem.write_u16(self.gpa(at), value).is_ok(),
        };
        if done { Ok(()) } else { Err(Broken::Memory) }
    }
}

/// The header's bytes at a register's offset.
fn put(h: &mut [u8], at: u16, bytes: &[u8]) {
    let at = usize::from(at);
    h[at..at + bytes.len()].copy_from_slice(bytes);
}

fn get16(h: &[u8], at: u16) -> u16 {
    let at = usize::from(at);
    u16::from_le_bytes([h[at], h[at + 1]])
}

fn get32(h: &[u8], at: u16) -> u32 {
    let at = usize::from(at);
    u32::from_le_bytes([h[at], h[at + 1], h[at + 2], h[at + 3]])
}


/// The interrupts a device has for the driver once it has given buffers
/// back: the INTx line to raise -- an edge, the interrupt status having been
/// clear -- and MSI-X table entries to send, a bit each.
#[derive(Clone, Copy, Default)]
pub struct Raise {
    pub line: bool,
    pub vectors: u32,
}

impl Raise {
    pub fn or(self, other: Raise) -> Raise {
        Raise { line: self.line || other.line, vectors: self.vectors | other.vectors }
    }

    pub fn any(&self) -> bool {
        self.line || self.vectors != 0
    }
}

/// MSI-X as the transport keeps it: the table's size, whether the function
/// has it on, and the vector each source is to send.
struct Msix {
    entries: u16,
    enabled: bool,
    config: u16,
    queues: Vec<u16>,
}

/// What a write to the header asks of the device.
pub enum Asked {
    Nothing,
    /// The driver has made buffers available on this queue.
    Notify(u16),
    /// The driver wrote 0 to the status: everything back to how it began.
    Reset,
}

/// How many queues' rings a device may map (`Queue::mapped`): a burst of
/// them, then one a second. Each mapping undone is its address shot down on
/// every host CPU -- an interrupt each, other guests' CPUs among them -- and
/// a guest that set its queues up over and over could have them all do so
/// at the price of an exit each. Past the budget a queue's rings are copied,
/// as they were before rings were mapped, the guest's own device the slower
/// for it. A driver sets its queues up once a boot; a reset takes none back.
struct Maps {
    left: u32,
    at: u64,
}

const MAPS_BURST: u32 = 8;
const MAPS_EVERY_NS: u64 = 1_000_000_000;

impl Maps {
    fn new() -> Maps {
        Maps { left: MAPS_BURST, at: 0 }
    }

    /// Whether a queue set up at `now` may have its rings mapped.
    fn take(&mut self, now: u64) -> bool {
        let earned = now.saturating_sub(self.at) / MAPS_EVERY_NS;
        if earned != 0 {
            self.left = earned.saturating_add(u64::from(self.left)).min(u64::from(MAPS_BURST)) as u32;
            self.at = now;
        }
        if self.left == 0 {
            return false;
        }
        self.left -= 1;
        true
    }
}

/// The legacy header's state.
pub struct Transport {
    device_features: u32,
    driver_features: u32,
    status: u8,
    isr: u8,
    select: u16,
    queues: Vec<Queue>,
    msix: Option<Msix>,
    maps: Maps,
}

impl Transport {
    /// A device offering `features`, with `queues` (the capacity already
    /// taken) of the sizes it wants.
    pub fn new(device_features: u32, queues: Vec<Queue>) -> Transport {
        Transport { device_features, driver_features: 0, status: 0, isr: 0, select: 0, queues, msix: None, maps: Maps::new() }
    }

    /// The features offered, before a driver has read them: what the
    /// device is, for a guest's next boot.
    pub fn set_device_features(&mut self, features: u32) {
        self.device_features = features;
        self.driver_features &= features;
    }

    /// Offer MSI-X, with a table of `entries`: every source's vector starts
    /// at none, as a reset leaves it.
    pub fn offer_msix(&mut self, entries: u16) -> Option<()> {
        let mut queues = Vec::new();
        queues.try_reserve_exact(self.queues.len()).ok()?;
        queues.resize(self.queues.len(), NO_VECTOR);
        self.msix = Some(Msix { entries, enabled: false, config: NO_VECTOR, queues });
        Some(())
    }

    /// The function's MSI-X enable, as its capability has it now: what the
    /// header's layout and each interrupt follow.
    pub fn set_msix_enabled(&mut self, on: bool) {
        if let Some(m) = &mut self.msix {
            m.enabled = on;
        }
    }

    fn msix_on(&self) -> Option<&Msix> {
        self.msix.as_ref().filter(|m| m.enabled)
    }

    /// Whether the function asserts its INTx line: an interrupt status the
    /// driver has not read yet -- which is how long a level-triggered line
    /// stays up -- with MSI-X off, a function with it on having no line in
    /// use.
    pub fn line(&self) -> bool {
        self.isr != 0 && self.msix_on().is_none()
    }

    /// Where the device's own configuration begins in the BAR.
    pub fn config_offset(&self) -> u16 {
        if self.msix_on().is_some() { DEVICE_CONFIG_MSIX } else { DEVICE_CONFIG }
    }

    pub fn driver_features(&self) -> u32 {
        self.driver_features
    }

    /// Whether a queue that is up has its rings copied, no mapping having
    /// been had for them (`Maps`, `GuestMemory::map`): for a report, since
    /// nothing else says the device is the slower for it.
    pub fn rings_copied(&self) -> bool {
        self.queues.iter().any(|q| q.ready() && q.mapped.is_none())
    }

    pub fn queue(&mut self, index: u16) -> Option<&mut Queue> {
        self.queues.get_mut(usize::from(index))
    }

    fn selected(&self) -> Option<&Queue> {
        self.queues.get(usize::from(self.select))
    }

    fn reset(&mut self) {
        self.driver_features = 0;
        self.status = 0;
        self.isr = 0;
        self.select = 0;
        for q in &mut self.queues {
            q.reset();
        }
        /* The vectors go back to none; whether MSI-X is on is the PCI
         * function's, which a device reset does not touch. */
        if let Some(m) = &mut self.msix {
            m.config = NO_VECTOR;
            m.queues.iter_mut().for_each(|v| *v = NO_VECTOR);
        }
    }

    /// The header as bytes, for a read of any size at any offset in it --
    /// the vector registers' too, with MSI-X on.
    fn header(&self) -> [u8; DEVICE_CONFIG_MSIX as usize] {
        let mut h = [0u8; DEVICE_CONFIG_MSIX as usize];
        let (pfn, size) = self.selected().map_or((0, 0), |q| (q.pfn, q.size));
        put(&mut h, DEVICE_FEATURES, &self.device_features.to_le_bytes());
        put(&mut h, DRIVER_FEATURES, &self.driver_features.to_le_bytes());
        put(&mut h, QUEUE_PFN, &pfn.to_le_bytes());
        put(&mut h, QUEUE_SIZE, &size.to_le_bytes());
        put(&mut h, QUEUE_SELECT, &self.select.to_le_bytes());
        put(&mut h, DEVICE_STATUS, &[self.status]);
        put(&mut h, ISR_STATUS, &[self.isr]);
        if let Some(m) = self.msix_on() {
            let queue = m.queues.get(usize::from(self.select)).copied().unwrap_or(NO_VECTOR);
            put(&mut h, CONFIG_VECTOR, &m.config.to_le_bytes());
            put(&mut h, QUEUE_VECTOR, &queue.to_le_bytes());
        }
        h
    }

    /// A read of `size` bytes at `offset` in the header -- the interrupt
    /// status cleared by reading it, which is how the driver acknowledges an
    /// interrupt.
    pub fn read(&mut self, offset: u16, size: u8) -> u32 {
        let h = self.header();
        let end = usize::from(self.config_offset());
        let mut v = 0u32;
        for i in 0..u16::from(size) {
            let at = offset + i;
            if let Some(b) = h[..end].get(usize::from(at)) {
                v |= u32::from(*b) << (8 * i);
            }
            if at == ISR_STATUS {
                self.isr = 0;
            }
        }
        v
    }

    /// A write of `size` bytes at `offset` in the header, at `now`: a queue
    /// set up has its rings mapped in `mem` (`Queue::mapped`).
    pub fn write(&mut self, offset: u16, size: u8, value: u32, mem: &GuestMemory, now: u64) -> Asked {
        let mut h = self.header();
        let end = usize::from(self.config_offset());
        for i in 0..u16::from(size) {
            if let Some(b) = h[..end].get_mut(usize::from(offset + i)) {
                *b = (value >> (8 * i)) as u8;
            }
        }
        /* Whether the write reached the register at `reg`, `len` bytes. */
        let touches = |reg: u16, len: u16| offset < reg + len && offset + u16::from(size) > reg;
        let mut asked = Asked::Nothing;
        if touches(DRIVER_FEATURES, 4) {
            /* Only what was offered can be taken. */
            self.driver_features = get32(&h, DRIVER_FEATURES) & self.device_features;
        }
        if touches(QUEUE_SELECT, 2) {
            self.select = get16(&h, QUEUE_SELECT);
        }
        if touches(QUEUE_PFN, 4) {
            let pfn = get32(&h, QUEUE_PFN);
            if let Some(q) = self.queues.get_mut(usize::from(self.select)) {
                if pfn == 0 {
                    q.reset();
                } else {
                    let map = self.maps.take(now);
                    q.set_pfn(pfn, mem, map);
                }
            }
        }
        if touches(QUEUE_NOTIFY, 2) {
            asked = Asked::Notify(get16(&h, QUEUE_NOTIFY));
        }
        if self.msix_on().is_some() && touches(CONFIG_VECTOR, 4) {
            let select = usize::from(self.select);
            if let Some(m) = &mut self.msix {
                /* An entry the table has not got reads back as none, which
                 * is how a driver learns the vector was not taken. */
                let entries = m.entries;
                let valid = |v: u16| if v < entries { v } else { NO_VECTOR };
                if touches(CONFIG_VECTOR, 2) {
                    m.config = valid(get16(&h, CONFIG_VECTOR));
                }
                if touches(QUEUE_VECTOR, 2) {
                    let v = valid(get16(&h, QUEUE_VECTOR));
                    if let Some(q) = m.queues.get_mut(select) {
                        *q = v;
                    }
                }
            }
        }
        if touches(DEVICE_STATUS, 1) {
            self.status = h[usize::from(DEVICE_STATUS)];
            if self.status == 0 {
                self.reset();
                asked = Asked::Reset;
            }
        }
        asked
    }

    /// Queue `queue` gave buffers back and the driver wants to hear of it:
    /// with MSI-X on, its vector's entry to send, if it has one; without,
    /// the interrupt status noted, and the line to go up when it was clear
    /// -- an edge, which the PIC here takes; while it stays set the driver
    /// has yet to look, and will see these too.
    pub fn interrupt(&mut self, queue: u16) -> Raise {
        if let Some(m) = self.msix_on() {
            let v = m.queues.get(usize::from(queue)).copied().unwrap_or(NO_VECTOR);
            return match 1u32.checked_shl(u32::from(v)) {
                Some(bit) if v < m.entries => Raise { line: false, vectors: bit },
                _ => Raise::default(),
            };
        }
        let was = self.isr;
        self.isr |= ISR_QUEUE;
        Raise { line: was == 0, vectors: 0 }
    }
}
