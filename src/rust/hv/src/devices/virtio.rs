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

use crate::memory::GuestMemory;

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
const QUEUE_ALIGN: u64 = 4096;
const PFN_SHIFT: u32 = 12;

/// Descriptor flags.
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const DESC_INDIRECT: u16 = 4;
/// A descriptor's size in the table, and a used element's.
const DESC_SIZE: u64 = 16;
const USED_ELEM_SIZE: u64 = 8;
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
}

impl Queue {
    pub fn new(size: u16) -> Queue {
        Queue { size, pfn: 0, next_avail: 0, next_used: 0 }
    }

    fn reset(&mut self) {
        self.pfn = 0;
        self.next_avail = 0;
        self.next_used = 0;
    }

    pub fn ready(&self) -> bool {
        self.pfn != 0
    }

    fn desc_base(&self) -> u64 {
        u64::from(self.pfn) << PFN_SHIFT
    }

    fn avail_base(&self) -> u64 {
        self.desc_base() + DESC_SIZE * u64::from(self.size)
    }

    fn used_base(&self) -> u64 {
        /* flags, idx, the ring, used_event: 6 + 2 * size bytes */
        let avail_end = self.avail_base() + 6 + 2 * u64::from(self.size);
        (avail_end + QUEUE_ALIGN - 1) & !(QUEUE_ALIGN - 1)
    }

    /// The next chain the driver has made available: its head, its buffers
    /// onto `segs` (cleared first; its capacity is the queue's size, so
    /// nothing here allocates) -- or `Ok(None)` when there is none.
    pub fn pop(&mut self, mem: &GuestMemory, segs: &mut Vec<Seg>) -> Result<Option<u16>, Broken> {
        segs.clear();
        let avail = self.avail_base();
        let avail_idx = read_u16(mem, avail + 2)?;
        let waiting = avail_idx.wrapping_sub(self.next_avail);
        if waiting == 0 {
            return Ok(None);
        }
        if waiting > self.size {
            return Err(Broken::Overrun);
        }
        let slot = u64::from(self.next_avail % self.size);
        let head = read_u16(mem, avail + 4 + 2 * slot)?;

        let mut index = head;
        loop {
            if index >= self.size {
                return Err(Broken::Index);
            }
            if segs.len() >= usize::from(self.size) {
                return Err(Broken::Loop);
            }
            let mut d = [0u8; DESC_SIZE as usize];
            mem.read(self.desc_base() + DESC_SIZE * u64::from(index), &mut d).map_err(|_| Broken::Memory)?;
            let addr = u64::from_le_bytes([d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]);
            let len = u32::from_le_bytes([d[8], d[9], d[10], d[11]]);
            let flags = u16::from_le_bytes([d[12], d[13]]);
            let next = u16::from_le_bytes([d[14], d[15]]);
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
        self.next_avail = self.next_avail.wrapping_add(1);
        Ok(Some(head))
    }

    /// Give the chain at `head` back, `len` bytes of it written by the
    /// device: the element first, then the index that publishes it.
    pub fn push(&mut self, mem: &GuestMemory, head: u16, len: u32) -> Result<(), Broken> {
        let used = self.used_base();
        let slot = u64::from(self.next_used % self.size);
        let mut elem = [0u8; USED_ELEM_SIZE as usize];
        elem[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        elem[4..].copy_from_slice(&len.to_le_bytes());
        mem.write(used + 4 + USED_ELEM_SIZE * slot, &elem).map_err(|_| Broken::Memory)?;
        self.next_used = self.next_used.wrapping_add(1);
        mem.write(used + 2, &self.next_used.to_le_bytes()).map_err(|_| Broken::Memory)
    }

    /// Whether the driver wants an interrupt for what was given back.
    pub fn wants_interrupt(&self, mem: &GuestMemory) -> bool {
        read_u16(mem, self.avail_base()).map_or(true, |flags| flags & AVAIL_NO_INTERRUPT == 0)
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

fn read_u16(mem: &GuestMemory, gpa: u64) -> Result<u16, Broken> {
    let mut b = [0u8; 2];
    mem.read(gpa, &mut b).map_err(|_| Broken::Memory)?;
    Ok(u16::from_le_bytes(b))
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

/// The legacy header's state.
pub struct Transport {
    device_features: u32,
    driver_features: u32,
    status: u8,
    isr: u8,
    select: u16,
    queues: Vec<Queue>,
    msix: Option<Msix>,
}

impl Transport {
    /// A device offering `features`, with `queues` (the capacity already
    /// taken) of the sizes it wants.
    pub fn new(device_features: u32, queues: Vec<Queue>) -> Transport {
        Transport { device_features, driver_features: 0, status: 0, isr: 0, select: 0, queues, msix: None }
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

    /// A write of `size` bytes at `offset` in the header.
    pub fn write(&mut self, offset: u16, size: u8, value: u32) -> Asked {
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
                    q.pfn = pfn;
                    q.next_avail = 0;
                    q.next_used = 0;
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
