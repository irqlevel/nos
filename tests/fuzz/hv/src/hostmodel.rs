//! A host's PCI function given to a guest (`hypervisor/hv/src/devices/host.rs`):
//! the guest reads and writes its configuration space -- through the bus
//! and through NVIDIA's mirror of it in BAR0 -- sizes and moves its BARs,
//! programs its MSI or its MSI-X table, reads its pending bits, reaches the
//! device's registers beside the table, and the device raises its
//! interrupts; and whatever the guest writes, what reaches the real function
//! holds to what the host keeps. The real function is a register file every
//! write lands in (`Model`), and it judges each write as it comes, by the
//! rules the host keeps whatever the guest does: its BARs, ROM, header, MSI
//! and MSI-X capabilities never written; its command register decoding
//! memory, no I/O, INTx off; its device control's phantom functions, no
//! snoop and FLR off and the host's fields as the host had them; its link
//! and Advanced Features' FLR never written; never out of D0; BAR0 written
//! only where the mirror acknowledges an MSI; MSI-X's real table and PBA
//! never reached at all -- the host's to program; a page of its BARs reached
//! only where it traps, by an access aligned to its size. And what the guest
//! sees is held to the PCI spec's registers, worked out here from the spec
//! and not from `host.rs`: a BAR sized reads back its size, holds no bit
//! below it and its type, a BAR moved is said to have moved from where it was
//! to where it is, the MSI or MSI-X entry the guest programmed is the message
//! the device's interrupt sends -- held pending while it is off or masked,
//! the PBA saying so, and sent once when it is not; the capability list it
//! walks is the real one without the one of MSI and MSI-X not given; MSI-X's
//! table is as long as the host gives, `MAX_VECTORS` at most; the device's
//! registers in a page MSI-X's table shares are the device's.
//!
//! The function is laid out the target's way: a capability a slot, in any
//! order, its MSI-X table and PBA in a BAR -- sharing a page with registers
//! of the device's, or each other's -- now and then one the host cannot
//! give -- MSI-X's table past its BAR and no MSI, a bridge, a second
//! capability of a kind it keeps, a list that loops or points into the
//! header, a page that cannot be mapped -- which it must refuse.

use std::sync::{Arc, Mutex};

use crate::devices::host::{decode_bar, Bar, HostFunction, Messages, Real, MAX_VECTORS, MIRROR_BYTES, NVIDIA_MIRROR,
                           NVIDIA_MSI_REARM, VENDOR_NVIDIA};
use crate::targets::Input;

const SPACE: usize = 256;
const PAGE: u64 = 4096;

/* The header (PCI Local Bus 3.0, 6.2) */
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const HEADER_TYPE: usize = 0x0E;
const BAR0: usize = 0x10;
const ROM: usize = 0x30;
const CAP_POINTER: usize = 0x34;
const INTERRUPT_LINE: usize = 0x3C;
const INTERRUPT_PIN: usize = 0x3D;
const COMMAND_IO: u16 = 1 << 0;
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_MASTER: u16 = 1 << 2;
const COMMAND_INTX_DISABLE: u16 = 1 << 10;
const STATUS_CAPABILITIES: u16 = 1 << 4;

/* Capabilities, and the slot the target puts each in: clear of each other */
const CAP_PM: u8 = 0x01;
const CAP_MSI: u8 = 0x05;
const CAP_VENDOR: u8 = 0x09;
const CAP_PCIE: u8 = 0x10;
const CAP_MSIX: u8 = 0x11;
const CAP_AF: u8 = 0x13;
const SLOT_PM: usize = 0x40;
const SLOT_MSI: usize = 0x48;
const SLOT_PCIE: usize = 0x60;
const SLOT_AF: usize = 0xA0;
const SLOT_MSIX: usize = 0xB0;
const SLOT_VENDOR: usize = 0xC0;
/// Where a second capability of a kept kind goes, when the target makes one.
const SLOT_SECOND: usize = 0xD0;
const PCIE_LEN: usize = 0x3C;

/* MSI (PCI 3.0, 6.8.1): its control word's bits */
const MSI_ENABLE: u16 = 1 << 0;
const MSI_64: u16 = 1 << 7;
const MSI_MASKABLE: u16 = 1 << 8;

/* MSI-X (PCI 3.0, 6.8.2): its control word's table size, function mask and
 * enable; the capability's bytes; a table entry's */
const MSIX_SIZE: u16 = 0x7FF;
const MSIX_FUNCTION_MASK: u16 = 1 << 14;
const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_LEN: usize = 12;
const ENTRY: u64 = 16;

/* PCIe (5.0, 7.5.3): device control's fields */
const DEVCTL: usize = 0x08;
const DEVCTL_ERRORS: u16 = 0xF;
const DEVCTL_MPS: u16 = 0x7 << 5;
const DEVCTL_PHANTOM: u16 = 1 << 9;
const DEVCTL_NO_SNOOP: u16 = 1 << 11;
const DEVCTL_MRRS: u16 = 0x7 << 12;
const DEVCTL_FLR: u16 = 1 << 15;
const LNKCTL: usize = 0x10;
const PMCSR: usize = 4;
const PM_STATE: u8 = 0x3;
const AF_CONTROL: usize = 4;

/// Where the target's function has each capability the host keeps registers
/// of: 0 for none.
#[derive(Clone, Copy, Debug, Default)]
pub struct Layout {
    pub pm: usize,
    pub msi: usize,
    pub msix: usize,
    pub pcie: usize,
    pub af: usize,
}

/// Where MSI-X's table and PBA are, as the capability says: a BAR's number
/// and an offset each, the device's entries -- and whether both are inside
/// a BAR the host was given, which is when it gives MSI-X.
#[derive(Clone, Copy, Debug)]
pub struct MsixAt {
    pub table_bar: usize,
    pub table: u64,
    pub pba_bar: usize,
    pub pba: u64,
    pub entries: usize,
    pub inside: bool,
}

impl MsixAt {
    fn table_len(&self) -> u64 {
        self.entries as u64 * ENTRY
    }

    fn pba_len(&self) -> u64 {
        (self.entries as u64).div_ceil(64) * 8
    }

    /// Whether `[offset, offset + len)` of BAR `bar` holds a byte of the
    /// table or the PBA.
    fn touches(&self, bar: usize, offset: u64, len: u64) -> bool {
        let within = |b: usize, start: u64, n: u64| b == bar && offset < start + n && start < offset + len;
        within(self.table_bar, self.table, self.table_len()) || within(self.pba_bar, self.pba, self.pba_len())
    }
}

/// The bytes of a BAR page of the device's own, before anything writes
/// them: told apart by where they are.
fn pattern(bar: usize, offset: u64) -> u8 {
    (offset.wrapping_mul(0x9E37_79B9).wrapping_add(bar as u64 * 0x51) >> 7) as u8
}

/// The real function: its configuration space, which every write lands in,
/// the pages of its BARs mapped for the host and what reached them, and the
/// first rule an access broke.
pub struct Model {
    pub space: [u8; SPACE],
    pub layout: Layout,
    /// The host's command register and device control, as the host had
    /// them before the function was given.
    host_command: u16,
    host_devctl: u16,
    pub broken: Option<String>,
    /// What reached its BARs through the pages that trap: BAR, offset, size,
    /// value.
    pub page_writes: Vec<(usize, u64, usize, u64)>,
    /// The pages mapped (`map_page`): BAR, offset, bytes.
    pages: Vec<(usize, u64, Box<[u8; PAGE as usize]>)>,
    /// Whether a page asked for is refused its mapping.
    map_fails: bool,
    /// MSI-X's real table and PBA, where the function has MSI-X.
    pub msix: Option<MsixAt>,
    /// Whether the host gives MSI-X: the mirror's MSI acknowledgement is
    /// then none of the guest's.
    msix_given: bool,
    /// Where each capability is: its id and next pointer read-only.
    caps: Vec<usize>,
    /// Writes of its configuration space, every one.
    pub writes: usize,
}

impl Model {
    fn broke(&mut self, why: String) {
        if self.broken.is_none() {
            self.broken = Some(why);
        }
    }

    fn u16_at(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.space[at], self.space[at + 1]])
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.space[at..at + 4].try_into().unwrap())
    }

    fn nvidia(&self) -> bool {
        self.u16_at(0) == VENDOR_NVIDIA
    }

    /// The real command register and device control, as they are now.
    pub fn command(&self) -> u16 {
        self.u16_at(COMMAND)
    }

    pub fn devctl(&self) -> Option<u16> {
        (self.layout.pcie != 0).then(|| self.u16_at(self.layout.pcie + DEVCTL))
    }

    /// A write of `size` bytes of `value` at `at`, judged a byte at a time
    /// and then made.
    fn write(&mut self, at: u16, size: usize, value: u32) {
        let at = usize::from(at);
        if at % size != 0 || at + size > SPACE {
            self.broke(format!("a write of {} bytes at {:#x}: not aligned, or past the space", size, at));
            return;
        }
        let bytes = value.to_le_bytes();
        for (i, &byte) in bytes.iter().enumerate().take(size) {
            self.judge(at + i, byte);
        }
        self.space[at..at + size].copy_from_slice(&bytes[..size]);
        self.writes += 1;
    }

    /// One byte written at `b`, by the rules the host keeps.
    fn judge(&mut self, b: usize, v: u8) {
        let l = self.layout;
        let host = self.host_command.to_le_bytes();
        let in_cap = |cap: usize, from: usize, to: usize| cap != 0 && b >= cap + from && b < cap + to;
        match b {
            0x00..=0x03 => self.broke(format!("the ids written: {:#04x} at {:#x}", v, b)),
            0x04 => {
                let kept = !(COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER) as u8;
                if v & COMMAND_IO as u8 != 0 || v & COMMAND_MEMORY as u8 == 0 || v & kept != host[0] & kept {
                    self.broke(format!("the command register's low byte written {:#04x}, the host's {:#04x}", v,
                                       host[0]));
                }
            }
            0x05 => {
                let intx = (COMMAND_INTX_DISABLE >> 8) as u8;
                if v & intx == 0 || v & !intx != host[1] & !intx {
                    self.broke(format!("the command register's high byte written {:#04x}, the host's {:#04x}", v,
                                       host[1]));
                }
            }
            0x06 | 0x07 => {}
            0x08..=0x0F => self.broke(format!("the header written: {:#04x} at {:#x}", v, b)),
            0x10..=0x27 => self.broke(format!("a real BAR written: {:#04x} at {:#x}", v, b)),
            0x28..=0x3F => self.broke(format!("the header written: {:#04x} at {:#x}", v, b)),
            _ if l.msi != 0 && b >= l.msi && b < l.msi + msi_len(&self.space, l.msi) => {
                self.broke(format!("the real MSI capability written: {:#04x} at {:#x}", v, b));
            }
            _ if in_cap(l.msix, 0, MSIX_LEN) => {
                self.broke(format!("the real MSI-X capability written: {:#04x} at {:#x}", v, b));
            }
            _ if in_cap(l.pm, PMCSR, PMCSR + 1) => {
                if v & PM_STATE != self.space[b] & PM_STATE {
                    self.broke(format!("the power state written {}, from {}", v & PM_STATE, self.space[b] & PM_STATE));
                }
            }
            _ if in_cap(l.pcie, DEVCTL, DEVCTL + 2) => {
                let shift = 8 * (b - l.pcie - DEVCTL);
                let host = ((self.host_devctl >> shift) & 0xFF) as u8;
                let kept = (((DEVCTL_ERRORS | DEVCTL_MPS | DEVCTL_MRRS) >> shift) & 0xFF) as u8;
                let off = (((DEVCTL_PHANTOM | DEVCTL_NO_SNOOP | DEVCTL_FLR) >> shift) & 0xFF) as u8;
                if v & off != 0 || v & kept != host & kept {
                    self.broke(format!("device control's byte {} written {:#04x}, the host's {:#04x}", shift / 8, v,
                                       host));
                }
            }
            _ if in_cap(l.pcie, LNKCTL, LNKCTL + 4) => {
                self.broke(format!("the link's registers written: {:#04x} at {:#x}", v, b));
            }
            _ if in_cap(l.af, AF_CONTROL, AF_CONTROL + 2) => {
                self.broke(format!("Advanced Features' control written: {:#04x}", v));
            }
            _ if self.caps.iter().any(|&c| b == c || b == c + 1) => {
                self.broke(format!("a capability's id or next pointer written: {:#04x} at {:#x}", v, b));
            }
            _ => {}
        }
    }

    /// Whether the byte `offset` into BAR `bar` is NVIDIA's mirror's.
    fn in_mirror(&self, bar: usize, offset: u64) -> bool {
        let m = u64::from(NVIDIA_MIRROR);
        bar == 0 && self.nvidia() && offset >= m && offset - m < u64::from(MIRROR_BYTES)
    }

    /// The page an access of `size` bytes at `offset` into BAR `bar` is in,
    /// and where in it -- the rules for any: aligned, inside one page that
    /// was mapped. None, said, for one that breaks them.
    fn page_of(&mut self, bar: usize, offset: u64, size: usize) -> Option<(usize, usize)> {
        let base = offset & !(PAGE - 1);
        let at = (offset - base) as usize;
        if !matches!(size, 1 | 2 | 4 | 8) || at % size != 0 || at + size > PAGE as usize {
            self.broke(format!("a BAR access of {} bytes at {:#x}: not aligned to its size, or past its page", size,
                               offset));
            return None;
        }
        let Some(i) = self.pages.iter().position(|p| p.0 == bar && p.1 == base) else {
            self.broke(format!("BAR{}'s page at {:#x} reached, not one mapped", bar, base));
            return None;
        };
        Some((i, at))
    }
}

/// The MSI capability's bytes, by its control word.
fn msi_len(space: &[u8; SPACE], msi: usize) -> usize {
    let control = u16::from_le_bytes([space[msi + 2], space[msi + 3]]);
    msi_len_of(control)
}

fn msi_len_of(control: u16) -> usize {
    let base = if control & MSI_64 != 0 { 0x10 } else { 0x0C };
    if control & MSI_MASKABLE != 0 { base + 8 } else { base }
}

/// The model as `HostFunction` reaches it.
pub struct Handle(pub Arc<Mutex<Model>>);

impl Real for Handle {
    fn read32(&self, offset: u16) -> u32 {
        let mut m = self.0.lock().unwrap();
        let at = usize::from(offset);
        if at % 4 != 0 || at + 4 > SPACE {
            m.broke(format!("a read of a dword at {:#x}: not aligned, or past the space", at));
            return u32::MAX;
        }
        m.u32_at(at)
    }
    fn write32(&self, offset: u16, value: u32) {
        self.0.lock().unwrap().write(offset, 4, value)
    }
    fn write16(&self, offset: u16, value: u16) {
        self.0.lock().unwrap().write(offset, 2, u32::from(value))
    }
    fn write8(&self, offset: u16, value: u8) {
        self.0.lock().unwrap().write(offset, 1, u32::from(value))
    }
    fn map_page(&mut self, bar: usize, offset: u64) -> bool {
        let mut m = self.0.lock().unwrap();
        if offset % PAGE != 0 || bar >= 6 {
            m.broke(format!("BAR{}'s page at {:#x} asked for: no BAR's page", bar, offset));
            return false;
        }
        if m.pages.iter().any(|p| p.0 == bar && p.1 == offset) {
            m.broke(format!("BAR{}'s page at {:#x} mapped twice", bar, offset));
        }
        if m.map_fails {
            return false;
        }
        let mut bytes = Box::new([0u8; PAGE as usize]);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = pattern(bar, offset + i as u64);
        }
        m.pages.push((bar, offset, bytes));
        true
    }
    fn page_read(&self, bar: usize, offset: u64, size: usize) -> u64 {
        let mut m = self.0.lock().unwrap();
        if m.in_mirror(bar, offset) {
            m.broke(format!("the real mirror read at {:#x}: its configuration space is the host's view", offset));
        }
        if m.msix.is_some_and(|x| x.touches(bar, offset, size as u64)) {
            m.broke(format!("the real MSI-X table or PBA read: BAR{} at {:#x}", bar, offset));
        }
        let Some((i, at)) = m.page_of(bar, offset, size) else { return u64::MAX };
        let mut v = [0u8; 8];
        v[..size].copy_from_slice(&m.pages[i].2[at..at + size]);
        u64::from_le_bytes(v)
    }
    fn page_write(&self, bar: usize, offset: u64, size: usize, value: u64) {
        let mut m = self.0.lock().unwrap();
        if m.in_mirror(bar, offset) {
            let msi = m.layout.msi as u64;
            let ack = u64::from(NVIDIA_MIRROR) + msi;
            let rearm = size == 4 && offset == u64::from(NVIDIA_MIRROR + NVIDIA_MSI_REARM);
            let acknowledged = msi != 0 && !m.msix_given && matches!(size, 1 | 2 | 4) && offset >= ack
                && offset + size as u64 <= ack + 2;
            if !rearm && !acknowledged {
                m.broke(format!("BAR0 written {} bytes at {:#x}, not the mirror's MSI acknowledgement", size, offset));
            }
        } else if m.msix.is_some_and(|x| x.touches(bar, offset, size as u64)) {
            m.broke(format!("the real MSI-X table or PBA written: BAR{} at {:#x}, {} bytes", bar, offset, size));
        }
        let Some((i, at)) = m.page_of(bar, offset, size) else { return };
        m.pages[i].2[at..at + size].copy_from_slice(&value.to_le_bytes()[..size]);
        m.page_writes.push((bar, offset, size, value));
    }
}

/// A function laid out the target's way: its model, its BARs, and whether
/// the host can give it.
pub struct Made {
    pub model: Arc<Mutex<Model>>,
    pub bars: [Option<Bar>; 6],
    pub givable: bool,
    /// The capabilities in the order the real list has them.
    caps: Vec<(u8, usize)>,
}

/// Where the target puts MSI-X's table or PBA, `len` bytes: mostly inside a
/// BAR given -- now and then where a page of the device's own registers
/// has room left, as QEMU's NVMe has its table after its doorbells -- and
/// now and then anywhere, which is no BAR's or past one's end.
fn place(r: &mut Input, bars: &[Option<Bar>; 6], len: u64) -> (usize, u64) {
    let given: Vec<usize> = (0..6).filter(|&i| bars[i].is_some()).collect();
    if given.is_empty() || r.u8() < 24 {
        return (r.below(8) as usize, u64::from(r.u32() & !7));
    }
    let i = r.pick(&given);
    let size = bars[i].unwrap().size;
    let room = size.saturating_sub(len).min(u64::from(u32::MAX & !7));
    let offset = match r.u8() % 4 {
        0 => 0,
        1 => (PAGE + 8 * r.below(64)).min(room),
        _ => r.below(room + 1),
    };
    (i, offset & !7)
}

/// What the target makes of a function: any kind it can, mostly one the
/// host can give.
pub fn make(r: &mut Input, nvidia: bool) -> Made {
    let mut space = [0u8; SPACE];
    let vendor = if nvidia { VENDOR_NVIDIA } else { r.u16() };
    space[0..2].copy_from_slice(&vendor.to_le_bytes());
    space[2..4].copy_from_slice(&r.u16().to_le_bytes());
    /* The host's command register as it left it: no decoding, no mastering
     * -- the module turned those off -- the rest whatever it had */
    let host_command = r.u16() & !(COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER);
    space[COMMAND..COMMAND + 2].copy_from_slice(&host_command.to_le_bytes());
    for b in space.iter_mut().take(0x10).skip(0x08) {
        *b = r.u8();
    }
    let bridge = r.u8() < 8;
    space[HEADER_TYPE] = (if bridge { 0x01 } else { 0 }) | (if r.bool() { 0x80 } else { 0 });
    for b in space.iter_mut().take(0x40).skip(0x28) {
        *b = r.u8();
    }

    /* The BARs, as the module read them: wide ones taking two registers */
    let mut bars = [None; 6];
    let mut i = 0;
    while i < 6 {
        if r.u8() < 160 {
            let wide = r.bool() && i + 1 < 6;
            let size = 1u64 << (12 + r.below(if wide { 26 } else { 19 }));
            let host = r.u64() & !(size - 1) & if wide { 0x0000_FFFF_FFFF_F000 } else { 0xFFFF_F000 };
            bars[i] = Some(Bar { host, size, wide, prefetchable: r.bool() });
            let low = (host as u32) | (if wide { 0x4 } else { 0 });
            space[BAR0 + 4 * i..BAR0 + 4 * i + 4].copy_from_slice(&low.to_le_bytes());
            if wide {
                space[BAR0 + 4 * i + 4..BAR0 + 4 * i + 8].copy_from_slice(&((host >> 32) as u32).to_le_bytes());
            }
            i += if wide { 2 } else { 1 };
        } else {
            i += 1;
        }
    }

    /* MSI-X's table and PBA, kept clear of NVIDIA's mirror page */
    let mirror_page = nvidia && bars[0].is_some_and(|b| u64::from(NVIDIA_MIRROR + MIRROR_BYTES) <= b.size);
    let mut msix_at = (r.u8() < 128).then(|| {
        let entries = if r.u8() < 224 { 1 + r.below(70) as usize } else { 1 + r.below(2048) as usize };
        let table_len = entries as u64 * ENTRY;
        let pba_len = (entries as u64).div_ceil(64) * 8;
        let (table_bar, table) = place(r, &bars, table_len);
        let (pba_bar, pba) = if r.u8() < 64 {
            (table_bar, (table + table_len).next_multiple_of(8).min(u64::from(u32::MAX & !7)))
        } else {
            place(r, &bars, pba_len)
        };
        let inside = |bar: usize, at: u64, len: u64| bars.get(bar).copied().flatten().is_some_and(|b| at + len <= b.size);
        let inside = inside(table_bar, table, table_len) && inside(pba_bar, pba, pba_len);
        MsixAt { table_bar, table, pba_bar, pba, entries, inside }
    });
    if mirror_page {
        let m = u64::from(NVIDIA_MIRROR);
        if msix_at.is_some_and(|x| x.touches(0, m, u64::from(MIRROR_BYTES))) {
            msix_at = None;
        }
    }

    /* The capabilities: which, in what order */
    let msi = r.u8() >= 16;
    let msix = msix_at.is_some();
    /* A second MSI-X only beside a first: one alone would be laid out by
     * no one */
    let second = if r.u8() < 12 { Some(r.pick(&[CAP_MSI, CAP_PCIE, CAP_PM, CAP_AF, CAP_MSIX])) } else { None }
        .filter(|&k| k != CAP_MSIX || msix);
    let mut caps: Vec<(u8, usize)> = Vec::new();
    if r.u8() >= 64 {
        caps.push((CAP_PM, SLOT_PM));
    }
    if msi {
        caps.push((CAP_MSI, SLOT_MSI));
    }
    if r.u8() >= 64 {
        caps.push((CAP_PCIE, SLOT_PCIE));
    }
    if r.u8() < 32 {
        caps.push((CAP_AF, SLOT_AF));
    }
    if msix {
        caps.push((CAP_MSIX, SLOT_MSIX));
    }
    if r.bool() {
        caps.push((CAP_VENDOR, SLOT_VENDOR));
    }
    if let Some(kind) = second {
        caps.push((kind, SLOT_SECOND));
    }
    for k in (1..caps.len()).rev() {
        let j = r.below(k as u64 + 1) as usize;
        caps.swap(k, j);
    }
    /* Now and then a list that loops, or points into the header */
    let broken_list = !caps.is_empty() && r.u8() < 8;
    for (k, &(id, at)) in caps.iter().enumerate() {
        let next = match caps.get(k + 1) {
            Some(&(_, n)) => n as u8,
            None if broken_list => if r.bool() { caps[0].1 as u8 } else { 0x20 },
            None => 0,
        };
        space[at] = id;
        space[at + 1] = next;
    }
    space[CAP_POINTER] = caps.first().map_or(0, |&(_, at)| at as u8);
    let status = r.u16() | (if caps.is_empty() { 0 } else { STATUS_CAPABILITIES });
    space[STATUS..STATUS + 2].copy_from_slice(&status.to_le_bytes());

    let mut layout = Layout::default();
    for &(id, at) in &caps {
        let slot = match id {
            CAP_PM => &mut layout.pm,
            CAP_MSI => &mut layout.msi,
            CAP_MSIX => &mut layout.msix,
            CAP_PCIE => &mut layout.pcie,
            CAP_AF => &mut layout.af,
            _ => continue,
        };
        if *slot == 0 {
            *slot = at;
        }
    }
    /* Each capability's registers: what a device of the kind has */
    for &(id, at) in &caps {
        match id {
            CAP_MSI => {
                let control = r.u16() & (MSI_64 | MSI_MASKABLE | 0x000E);
                space[at + 2..at + 4].copy_from_slice(&control.to_le_bytes());
                for b in space.iter_mut().skip(at + 4).take(msi_len_of(control) - 4) {
                    *b = r.u8();
                }
            }
            CAP_MSIX if at == SLOT_MSIX => {
                /* As the host left it: off, or on and masked whole */
                let x = msix_at.unwrap();
                let control = ((x.entries - 1) as u16 & MSIX_SIZE)
                    | if r.u8() < 32 { MSIX_ENABLE | MSIX_FUNCTION_MASK } else { 0 };
                space[at + 2..at + 4].copy_from_slice(&control.to_le_bytes());
                let table = (x.table as u32 & !7) | x.table_bar as u32;
                let pba = (x.pba as u32 & !7) | x.pba_bar as u32;
                space[at + 4..at + 8].copy_from_slice(&table.to_le_bytes());
                space[at + 8..at + 12].copy_from_slice(&pba.to_le_bytes());
            }
            CAP_PCIE => {
                /* Version 2, an endpoint; FLR or not; device control as the
                 * host left it -- no snoop on, out of reset, now and then
                 * phantom functions too */
                space[at + 2] = 0x02;
                let devcap = (r.u32() & !(1 << 28)) | (if r.bool() { 1 << 28 } else { 0 });
                space[at + 4..at + 8].copy_from_slice(&devcap.to_le_bytes());
                let devctl = r.u16() & !DEVCTL_FLR;
                space[at + DEVCTL..at + DEVCTL + 2].copy_from_slice(&devctl.to_le_bytes());
                for b in space.iter_mut().skip(at + 0x0A).take(PCIE_LEN - 0x0A) {
                    *b = r.u8();
                }
            }
            CAP_PM => {
                space[at + 2] = r.u8();
                space[at + 3] = r.u8();
                space[at + PMCSR] = (if r.u8() < 16 { 0x3 } else { 0 }) | (r.u8() & 0xFC);
                space[at + PMCSR + 1] = r.u8();
            }
            CAP_AF => {
                space[at + 2] = 6;
                space[at + 3] = 0x3;
            }
            _ => {
                space[at + 2] = r.u8();
                space[at + 3] = r.u8();
            }
        }
    }

    let host_devctl = if layout.pcie != 0 {
        u16::from_le_bytes([space[layout.pcie + DEVCTL], space[layout.pcie + DEVCTL + 1]])
    } else {
        0
    };
    /* Given where it is an endpoint with at most one of each kind the host
     * keeps registers of, a list that ends, and MSI-X with its table and PBA
     * in BARs given or MSI to give -- and the pages it traps mapped */
    let count = |id: u8| caps.iter().filter(|&&(c, _)| c == id).count();
    let msix_given = count(CAP_MSIX) == 1 && msix_at.is_some_and(|x| x.inside);
    let map_fails = r.u8() < 8;
    let traps = mirror_page || msix_given;
    let givable = !bridge && count(CAP_MSI) <= 1 && count(CAP_MSIX) <= 1 && count(CAP_PM) <= 1
        && count(CAP_PCIE) <= 1 && count(CAP_AF) <= 1 && !broken_list && (msix_given || count(CAP_MSI) == 1)
        && !(map_fails && traps);
    let model = Model { space, layout, host_command, host_devctl, broken: None, page_writes: Vec::new(),
                        pages: Vec::new(), map_fails, msix: msix_at, msix_given,
                        caps: caps.iter().map(|&(_, at)| at).collect(), writes: 0 };
    Made { model: Arc::new(Mutex::new(model)), bars, givable, caps }
}

/// MSI-X as the guest sees it, by the spec: the entries it is given, its
/// control word's enable and function mask, its table's bytes -- an entry
/// sixteen: address, data, vector control -- and what is held pending.
struct GuestMsix {
    at: MsixAt,
    given: usize,
    control: u16,
    table: Vec<u8>,
    pending: Vec<bool>,
}

impl GuestMsix {
    fn new(at: MsixAt) -> GuestMsix {
        let given = at.entries.min(MAX_VECTORS);
        let mut table = vec![0u8; given * ENTRY as usize];
        /* Masked, every entry, as a reset leaves them */
        for e in 0..given {
            table[e * ENTRY as usize + 12] = 1;
        }
        GuestMsix { at, given, control: 0, table, pending: vec![false; given] }
    }

    /// The writable bits of the table's byte `k`: a dword-aligned address
    /// (its two low bits none), the data, and the vector control's mask.
    fn writable(k: usize) -> u8 {
        match k % ENTRY as usize {
            0 => 0xFC,
            1..=11 => 0xFF,
            12 => 0x01,
            _ => 0,
        }
    }

    fn u32_at(&self, k: usize) -> u32 {
        u32::from_le_bytes(self.table[k..k + 4].try_into().unwrap())
    }

    /// The message entry `i` sends now: None while MSI-X is off, masked
    /// whole, or the entry is masked -- or there is no such entry.
    fn message(&self, i: usize) -> Option<(u64, u32)> {
        if i >= self.given || self.control & MSIX_ENABLE == 0 || self.control & MSIX_FUNCTION_MASK != 0 {
            return None;
        }
        let e = i * ENTRY as usize;
        if self.table[e + 12] & 1 != 0 {
            return None;
        }
        let address = u64::from(self.u32_at(e)) | (u64::from(self.u32_at(e + 4)) << 32);
        Some((address, self.u32_at(e + 8)))
    }
}

/// What the guest sees, by the spec: each register of the six BARs as the
/// guest has it, the command register's three bits, the line, the MSI
/// capability's registers -- a byte at a time, each with its writable bits
/// -- or MSI-X, and which of the two is not shown.
struct Judge {
    bars: [Option<Bar>; 6],
    regs: [u32; 6],
    command: u16,
    line: u8,
    msi: [u8; 0x18],
    msi_at: usize,
    msi_control: u16,
    /// Whether MSI is what the guest is given.
    msi_given: bool,
    pending: bool,
    msix: Option<GuestMsix>,
    msix_cap: usize,
    /// The capability not shown, and its bytes.
    hidden: Option<(usize, usize)>,
}

impl Judge {
    fn new(bars: [Option<Bar>; 6], m: &Model) -> Judge {
        let msi_at = m.layout.msi;
        let msi_control = if msi_at != 0 { m.u16_at(msi_at + 2) } else { 0 };
        let msix = if m.msix_given { m.msix.map(GuestMsix::new) } else { None };
        let hidden = match (msix.is_some(), msi_at, m.layout.msix) {
            (true, 0, _) => None,
            (true, at, _) => Some((at, msi_len_of(msi_control))),
            (false, _, 0) => None,
            (false, _, at) => Some((at, MSIX_LEN)),
        };
        let mut j = Judge { bars, regs: [0; 6], command: 0, line: 0, msi: [0; 0x18], msi_at, msi_control,
                            msi_given: msix.is_none() && msi_at != 0, pending: false, msix,
                            msix_cap: m.layout.msix, hidden };
        for k in 0..6 {
            j.regs[k] = j.fixed(k);
        }
        j
    }

    fn hides(&self, b: usize) -> bool {
        self.hidden.is_some_and(|(at, len)| b >= at && b < at + len)
    }

    /// The BAR whose register `k` is, and whether it is that BAR's high half.
    fn bar_of(&self, k: usize) -> Option<(usize, bool)> {
        if self.bars[k].is_some() {
            return Some((k, false));
        }
        let prev = k.checked_sub(1)?;
        self.bars[prev].filter(|b| b.wide).map(|_| (prev, true))
    }

    /// Register `k`'s writable bits: the address bits above the size.
    fn writable(&self, k: usize) -> u32 {
        match self.bar_of(k) {
            Some((i, false)) => {
                let size = self.bars[i].unwrap().size;
                if size > 1 << 32 { 0 } else { !((size - 1) as u32) & !0xF }
            }
            Some((i, true)) => !((self.bars[i].unwrap().size - 1) >> 32) as u32,
            None => 0,
        }
    }

    /// Register `k`'s bits that are not the guest's: its type.
    fn fixed(&self, k: usize) -> u32 {
        match self.bar_of(k) {
            Some((i, false)) => {
                let b = self.bars[i].unwrap();
                (if b.wide { 0x4 } else { 0 }) | (if b.prefetchable { 0x8 } else { 0 })
            }
            _ => 0,
        }
    }

    fn address(&self, i: usize) -> u64 {
        let Some(b) = self.bars[i] else { return 0 };
        let low = u64::from(self.regs[i] & !0xF);
        if b.wide { low | (u64::from(self.regs[i + 1]) << 32) } else { low }
    }

    /// The writable bits of the MSI capability's byte `o`.
    fn msi_writable(&self, o: usize) -> u8 {
        let wide = self.msi_control & MSI_64 != 0;
        let data = if wide { 0x0C } else { 0x08 };
        match o {
            2 => MSI_ENABLE as u8,
            4 => 0xFC,
            5..=7 => 0xFF,
            8..=0x0B if wide => 0xFF,
            _ if o == data || o == data + 1 => 0xFF,
            _ if self.msi_control & MSI_MASKABLE != 0 && o == data + 4 => 0x01,
            _ => 0,
        }
    }

    fn msi_u32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.msi[o..o + 4].try_into().unwrap())
    }

    /// The message the device's interrupt `i` sends now, by the registers.
    fn message(&self, i: usize) -> Option<(u64, u32)> {
        if let Some(x) = &self.msix {
            return x.message(i);
        }
        if i != 0 || !self.msi_given {
            return None;
        }
        let wide = self.msi_control & MSI_64 != 0;
        let data_at = if wide { 0x0C } else { 0x08 };
        let enabled = self.msi[2] & MSI_ENABLE as u8 != 0;
        let masked = self.msi_control & MSI_MASKABLE != 0 && self.msi[data_at + 4] & 1 != 0;
        if !enabled || masked {
            return None;
        }
        let address = u64::from(self.msi_u32(4)) | if wide { u64::from(self.msi_u32(8)) << 32 } else { 0 };
        let data = u32::from(u16::from_le_bytes([self.msi[data_at], self.msi[data_at + 1]]));
        Some((address, data))
    }

    /// How many messages the device has, as the guest is given them.
    fn messages(&self) -> usize {
        self.msix.as_ref().map_or(1, |x| x.given)
    }

    fn is_pending(&self, i: usize) -> bool {
        match &self.msix {
            Some(x) => x.pending.get(i).copied().unwrap_or(false),
            None => i == 0 && self.pending,
        }
    }

    fn set_pending(&mut self, i: usize, on: bool) {
        match &mut self.msix {
            Some(x) => {
                if let Some(p) = x.pending.get_mut(i) {
                    *p = on;
                }
            }
            None if i == 0 => self.pending = on,
            None => {}
        }
    }

    /// A guest's write of `size` bytes of `value` at `at`, as the registers
    /// take it; the BAR it moved, if it did: (bar, from, to).
    fn write(&mut self, at: usize, size: usize, value: u32) -> Option<(usize, u64, u64)> {
        if at >= SPACE || size == 0 || !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > SPACE {
            return None;
        }
        let bytes = value.to_le_bytes();
        let mut moved = None;
        for (i, &v) in bytes.iter().enumerate().take(size) {
            let b = at + i;
            match b {
                0x04 | 0x05 => {
                    let shift = 8 * (b - COMMAND);
                    let bits = ((COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER) >> shift) & 0xFF;
                    self.command = (self.command & !(bits << shift)) | ((u16::from(v) & bits) << shift);
                }
                0x10..=0x27 => {
                    let k = (b - BAR0) / 4;
                    let shift = 8 * ((b - BAR0) % 4);
                    let before = self.bar_of(k).map(|(i, _)| (i, self.address(i)));
                    let w = self.writable(k);
                    let reg = (self.regs[k] & !(0xFF << shift)) | (u32::from(v) << shift);
                    self.regs[k] = (reg & w) | self.fixed(k);
                    if let Some((i, from)) = before {
                        let to = self.address(i);
                        if to != from {
                            moved = Some((i, moved.map_or(from, |(_, f, _)| f), to));
                        }
                    }
                }
                INTERRUPT_LINE => self.line = v,
                _ if self.hides(b) => {}
                _ if self.msix.is_some() && (b == self.msix_cap + 2 || b == self.msix_cap + 3) => {
                    let shift = 8 * (b - self.msix_cap - 2);
                    let bits = ((MSIX_ENABLE | MSIX_FUNCTION_MASK) >> shift) & 0xFF;
                    let x = self.msix.as_mut().unwrap();
                    x.control = (x.control & !(bits << shift)) | ((u16::from(v) & bits) << shift);
                }
                _ if self.msi_given && b >= self.msi_at && b < self.msi_at + msi_len_of(self.msi_control) => {
                    let o = b - self.msi_at;
                    let w = self.msi_writable(o);
                    self.msi[o] = (self.msi[o] & !w) | (v & w);
                }
                _ => {}
            }
        }
        /* A BAR moved and moved back within one write is no move */
        moved.filter(|&(_, from, to)| from != to)
    }

    /// What the guest reads of the byte `offset` into BAR `bar` of a page
    /// MSI-X traps, as the spec has it -- None for one of the device's own
    /// registers, whose byte the model holds -- and the write of `v` there.
    fn msix_byte(&mut self, bar: usize, offset: u64, write: Option<u8>) -> Option<u8> {
        let x = self.msix.as_mut()?;
        let a = x.at;
        if bar == a.table_bar && offset >= a.table && offset - a.table < a.table_len() {
            let k = (offset - a.table) as usize;
            if k >= x.table.len() {
                return Some(0);
            }
            if let Some(v) = write {
                let w = GuestMsix::writable(k);
                x.table[k] = (x.table[k] & !w) | (v & w);
            }
            return Some(x.table[k]);
        }
        if bar == a.pba_bar && offset >= a.pba && offset - a.pba < a.pba_len() {
            let first = (offset - a.pba) as usize * 8;
            let byte = (0..8).filter(|&b| x.pending.get(first + b).copied().unwrap_or(false))
                .fold(0u8, |v, b| v | (1 << b));
            return Some(byte);
        }
        None
    }

    /// The pages of the BARs the host must trap: NVIDIA's mirror, and every
    /// page MSI-X's table or PBA has a byte in.
    fn traps(&self, mirror: bool) -> Vec<(usize, u64)> {
        let mut pages = Vec::new();
        if mirror {
            pages.push((0, u64::from(NVIDIA_MIRROR)));
        }
        if let Some(x) = &self.msix {
            let a = x.at;
            for (bar, start, len) in [(a.table_bar, a.table, a.table_len()), (a.pba_bar, a.pba, a.pba_len())] {
                let mut page = start & !(PAGE - 1);
                while page < start + len {
                    if !pages.contains(&(bar, page)) {
                        pages.push((bar, page));
                    }
                    page += PAGE;
                }
            }
        }
        pages.sort();
        pages
    }
}

/// Registers worth the guest's while, and any.
fn register(r: &mut Input, layout: &Layout) -> usize {
    let at = match r.u8() % 9 {
        0 => COMMAND,
        1 | 2 => BAR0 + 4 * r.below(6) as usize + r.pick(&[0, 0, 0, 1, 2, 3]),
        3 if layout.msi != 0 => layout.msi + r.below(0x18) as usize,
        4 if layout.pcie != 0 => layout.pcie + r.pick(&[DEVCTL, DEVCTL + 1, DEVCTL + 2, LNKCTL, 0x28]),
        5 if layout.pm != 0 => layout.pm + PMCSR + r.below(4) as usize,
        6 => r.pick(&[ROM, INTERRUPT_LINE, INTERRUPT_PIN, HEADER_TYPE, CAP_POINTER, STATUS, 0x00]),
        7 if layout.msix != 0 => layout.msix + r.below(MSIX_LEN as u64) as usize,
        _ => r.below(SPACE as u64) as usize,
    };
    at.min(SPACE - 1)
}

fn sized(r: &mut Input, at: usize) -> usize {
    let size = r.pick(&[4usize, 4, 2, 1]);
    if at % size == 0 { size } else { 1 }
}

/// The real function's state against the host's rules, after a step.
fn held(model: &Arc<Mutex<Model>>, judge: &Judge) {
    let m = model.lock().unwrap();
    invariant!(m.broken.is_none(), "{}", m.broken.as_deref().unwrap_or(""));
    let command = m.command();
    invariant!(command & COMMAND_MEMORY != 0 && command & COMMAND_IO == 0 && command & COMMAND_INTX_DISABLE != 0
               && (command & COMMAND_MASTER != 0) == (judge.command & COMMAND_MASTER != 0),
               "the real command register {:#06x} for the guest's {:#x}", command, judge.command);
    if let Some(devctl) = m.devctl() {
        invariant!(devctl & (DEVCTL_PHANTOM | DEVCTL_NO_SNOOP | DEVCTL_FLR) == 0,
                   "device control {:#06x}: phantom functions, no snoop or FLR on", devctl);
    }
}

/// What a guest's write let out of what was held pending, against what the
/// registers say: each message held that can go now, sent once, in order.
fn released(messages: &Messages, judge: &mut Judge) {
    let mut sent = Vec::new();
    messages.send_pending(|address, data| sent.push((address, data)));
    let mut want = Vec::new();
    for i in 0..judge.messages() {
        if judge.is_pending(i) {
            if let Some(m) = judge.message(i) {
                want.push(m);
                judge.set_pending(i, false);
            }
        }
    }
    invariant!(sent == want, "messages let out {:?}, the registers' {:?}", sent, want);
}

/// The capability list as the guest walks it, against the real one: the
/// same capabilities in the same order but the one not shown, and an end.
fn walked(f: &HostFunction, made: &Made, judge: &Judge) {
    let mut seen = Vec::new();
    let mut at = (f.config_read(CAP_POINTER, 1) & 0xFC) as usize;
    while at != 0 && seen.len() <= made.caps.len() {
        seen.push(at);
        at = (f.config_read(at + 1, 1) & 0xFC) as usize;
    }
    let want: Vec<usize> = made.caps.iter().map(|&(_, at)| at).filter(|&at| judge.hidden.is_none_or(|(h, _)| h != at))
        .collect();
    invariant!(at == 0 && seen == want, "the capability list walked {:x?}, not the real one's {:x?} without {:?}",
               seen, want, judge.hidden);
    if let Some(x) = &judge.msix {
        let control = (f.config_read(judge.msix_cap + 2, 2)) as u16;
        invariant!(control == ((x.given - 1) as u16 | x.control),
                   "MSI-X's control read {:#x}: {} entries given, the guest's bits {:#x}", control, x.given, x.control);
    }
}

pub fn hostfn_target(r: &mut Input) {
    let nvidia = r.bool();
    let made = make(r, nvidia);
    let function = HostFunction::new(Box::new(Handle(made.model.clone())), made.bars);
    invariant!(function.is_some() == made.givable, "a function the host {} given",
               if made.givable { "can be, not" } else { "cannot be, " });
    let Some(mut f) = function else {
        let m = made.model.lock().unwrap();
        invariant!(m.writes == 0 && m.page_writes.is_empty() && m.broken.is_none(),
                   "a function not given written: {} writes, {} of its BARs ({:?})", m.writes, m.page_writes.len(),
                   m.broken);
        return;
    };
    let layout = made.model.lock().unwrap().layout;
    let mut judge = Judge::new(made.bars, &made.model.lock().unwrap());
    let mirror = nvidia && made.bars[0].is_some_and(|b| u64::from(NVIDIA_MIRROR + MIRROR_BYTES) <= b.size);
    let traps = judge.traps(mirror);
    let msix_pages: Vec<(usize, u64)> = judge.traps(false);
    {
        let mut got: Vec<(usize, u64)> = f.traps().iter().map(|t| (t.bar, t.offset)).collect();
        got.sort();
        invariant!(got == traps, "pages trapped {:x?}, not {:x?}", got, traps);
        let m = made.model.lock().unwrap();
        let mut mapped: Vec<(usize, u64)> = m.pages.iter().map(|p| (p.0, p.1)).collect();
        mapped.sort();
        invariant!(mapped == traps, "pages mapped {:x?}, not the ones trapped {:x?}", mapped, traps);
    }
    invariant!(f.msix_layout().is_some() == judge.msix.is_some(), "MSI-X {} given",
               if judge.msix.is_some() { "not" } else { "" });
    held(&made.model, &judge);
    walked(&f, &made, &judge);
    let messages = f.messages();
    invariant!(messages.count() == judge.messages(), "{} messages, not {}", messages.count(), judge.messages());

    while let Some(op) = r.op(14) {
        match op {
            0 | 1 => {
                /* A register written, as the bus has the guest write it */
                let at = register(r, &layout);
                let size = sized(r, at);
                let value = if r.bool() { u32::MAX } else { r.value32() };
                let moved = f.config_write(at, size, value);
                let want = judge.write(at, size, value);
                let got = moved.map(|m| (m.bar, m.from, m.to));
                invariant!(got == want, "a write of {:#x} ({} bytes) at {:#x} moved {:?}, not {:?}", value, size, at,
                           got, want);
            }
            2 => {
                /* A register read: what the spec says the guest sees */
                let at = register(r, &layout);
                let size = sized(r, at);
                let v = f.config_read(at, size);
                let dword = at & !3;
                let shift = 8 * (at - dword);
                let mask = if size == 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
                let expect = match dword {
                    d if (BAR0..BAR0 + 24).contains(&d) => Some(judge.regs[(d - BAR0) / 4]),
                    ROM => Some(0),
                    /* The device's pin, in the dword's second byte */
                    _ if at == INTERRUPT_PIN && size == 1 => {
                        Some(u32::from(made.model.lock().unwrap().space[INTERRUPT_PIN]) << 8)
                    }
                    _ if at == INTERRUPT_LINE && size == 1 => Some(u32::from(judge.line)),
                    _ if at == HEADER_TYPE && size == 1 => None,
                    d if judge.hides(d) => Some(0),
                    d if judge.msi_given && d >= judge.msi_at + 4
                        && d < judge.msi_at + msi_len_of(judge.msi_control) =>
                    {
                        Some(judge.msi_u32(d - judge.msi_at))
                    }
                    d if judge.msix.is_some() && (d == judge.msix_cap + 4 || d == judge.msix_cap + 8) => {
                        Some(made.model.lock().unwrap().u32_at(d))
                    }
                    _ => None,
                };
                if let Some(e) = expect {
                    invariant!(v == (e >> shift) & mask, "a read of {} bytes at {:#x}: {:#x}, the spec's {:#x}", size,
                               at, v, (e >> shift) & mask);
                }
                if at == HEADER_TYPE && size == 1 {
                    invariant!(v & 0x80 == 0, "the header type read {:#x}: another function said to be there", v);
                }
                if dword == COMMAND && shift == 0 && size >= 2 {
                    invariant!(v as u16 & (COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER) == judge.command,
                               "the command register read {:#x}, the guest's {:#x}", v, judge.command);
                }
            }
            3 => {
                /* A BAR sized, as a driver does: all ones, read back */
                let i = r.below(6) as usize;
                let at = BAR0 + 4 * i;
                let _ = f.config_write(at, 4, u32::MAX);
                let _ = judge.write(at, 4, u32::MAX);
                let v = f.config_read(at, 4);
                if let Some((bar, high)) = judge.bar_of(i) {
                    let b = made.bars[bar].unwrap();
                    let want = if high {
                        !((b.size - 1) >> 32) as u32
                    } else if b.size > 1 << 32 {
                        judge.fixed(i)
                    } else {
                        (!((b.size - 1) as u32) & !0xF) | judge.fixed(i)
                    };
                    invariant!(v == want, "BAR register {} sized reads {:#x}, not {:#x} for {:#x} bytes", i, v, want,
                               b.size);
                } else {
                    invariant!(v == 0, "register {}, no BAR's, sized reads {:#x}", i, v);
                }
            }
            4 => {
                /* Placed, as firmware would place it */
                let i = r.below(6) as usize;
                let gpa = match made.bars[i] {
                    Some(b) if r.u8() < 200 => r.u64() & !(b.size - 1) & if b.wide { (1 << 48) - 1 } else { 0xFFFF_FFFF },
                    _ => r.u64(),
                };
                let ok = f.place(i, gpa);
                let want = made.bars[i].is_some_and(|b| gpa % b.size == 0 && (b.wide || gpa <= u64::from(u32::MAX)));
                invariant!(ok == want, "BAR {} placed at {:#x}: {}", i, gpa, ok);
                if ok {
                    judge.regs[i] = (gpa as u32 & judge.writable(i)) | judge.fixed(i);
                    if made.bars[i].unwrap().wide {
                        judge.regs[i + 1] = (gpa >> 32) as u32;
                    }
                    invariant!(f.guest_address(i) == gpa && judge.address(i) == gpa,
                               "BAR {} placed at {:#x}, at {:#x}", i, gpa, f.guest_address(i));
                }
            }
            5 if judge.msi_given => {
                /* MSI programmed: an address, data, maybe its mask, on */
                let base = layout.msi;
                let wide = judge.msi_control & MSI_64 != 0;
                let data_at = base + if wide { 0x0C } else { 0x08 };
                let mut writes = vec![(base + 4, 4, 0xFEE0_0000 | (r.u32() & 0xF_FFFC)), (data_at, 2, r.value32() & 0xFFFF)];
                if wide {
                    writes.push((base + 8, 4, if r.u8() < 224 { 0 } else { r.u32() }));
                }
                if judge.msi_control & MSI_MASKABLE != 0 {
                    writes.push((data_at + 4, 4, u32::from(r.u8() < 64)));
                }
                writes.push((base + 2, 2, u32::from(r.u8() >= 32)));
                for (at, size, value) in writes {
                    let _ = f.config_write(at, size, value);
                    let _ = judge.write(at, size, value);
                }
                invariant!(messages.message(0) == judge.message(0), "MSI's message {:?}, the registers' {:?}",
                           messages.message(0), judge.message(0));
            }
            5 | 6 => {
                /* One of the device's interrupts -- now and then one past
                 * those given, which no handler of the host's is for */
                let past = u64::from(r.u8() < 16);
                let i = r.below(judge.messages() as u64 + past) as usize;
                let sent = messages.raise(i);
                let now = judge.message(i);
                invariant!(sent == now, "interrupt {} raised sends {:?}, the registers say {:?}", i, sent, now);
                if now.is_none() && i < judge.messages() {
                    judge.set_pending(i, true);
                }
            }
            7 if mirror => {
                /* NVIDIA's mirror, read or written: the configuration space's
                 * access, past its end nothing, the MSI's first two bytes
                 * acknowledged through the real one where MSI is given */
                let offset = match r.u8() % 8 {
                    0..=4 => register(r, &layout) as u32,
                    5 => NVIDIA_MSI_REARM,
                    _ => r.below(u64::from(MIRROR_BYTES)) as u32,
                };
                let size = r.pick(&[4usize, 4, 2, 1, 8]);
                let write = r.bool();
                let value = r.value32();
                let acks = made.model.lock().unwrap().page_writes.len();
                let at = offset as usize;
                let sized = matches!(size, 1 | 2 | 4);
                let inside = sized && at + size <= SPACE;
                let before = (!write && inside).then(|| f.config_read(at, size));
                let (v, moved) = f.mirror(NVIDIA_MIRROR, offset, size, write, value);
                if write {
                    let want = if inside { judge.write(at, size, value) } else { None };
                    let got = moved.map(|m| (m.bar, m.from, m.to));
                    invariant!(got == want, "a mirror write at {:#x} moved {:?}, not {:?}", offset, got, want);
                    /* QEMU's rule: an acknowledgement is a write within the
                     * capability's id and next pointer, and nothing past;
                     * and Pascal's on: a dword at 0x704 */
                    let ack = (inside && judge.msi_given && at >= layout.msi && at + size <= layout.msi + 2)
                        || (offset == NVIDIA_MSI_REARM && size == 4);
                    let now = made.model.lock().unwrap().page_writes.len();
                    invariant!(now == acks + usize::from(ack), "a mirror write of {} bytes at {:#x}: {} acknowledgements, \
                                                               not {}", size, offset, now - acks, usize::from(ack));
                } else if let Some(b) = before {
                    invariant!(v == b, "the mirror read {:#x} at {:#x}, the space {:#x}", v, offset, b);
                } else if !sized {
                    invariant!(v == u32::MAX, "a mirror read of {} bytes at {:#x}: {:#x}, not all ones", size, offset, v);
                } else {
                    invariant!(v == 0, "the mirror past the space read {:#x} at {:#x}", v, offset);
                }
            }
            8 => bar_decoded(r),
            9 | 10 if judge.msix.is_some() => {
                /* A page MSI-X traps: its table programmed -- an address, the
                 * data, the mask -- read back, its PBA read, or a register of
                 * the device's beside them reached; mostly as a driver does
                 * it, dword or qword aligned, now and then any way at all */
                let x = judge.msix.as_ref().unwrap();
                let a = x.at;
                let given = x.given;
                let (bar, offset) = match r.u8() % 8 {
                    0..=3 => {
                        let past = u64::from(r.u8() < 16);
                        let e = r.below(given as u64 + past);
                        (a.table_bar, a.table + e * ENTRY + 4 * r.below(4))
                    }
                    4 => (a.pba_bar, a.pba + 8 * r.below(a.pba_len() / 8)),
                    _ => {
                        let (bar, page) = r.pick(&msix_pages);
                        (bar, page + r.below(PAGE))
                    }
                };
                let size = if r.u8() < 220 { r.pick(&[4usize, 4, 4, 8]) } else { r.pick(&[1usize, 2, 4, 8]) };
                let offset = if r.u8() < 224 { offset & !(size as u64 - 1) } else { offset };
                let offset = offset.min((offset & !(PAGE - 1)) + PAGE - size as u64);
                let write = op == 10 || r.u8() < 32;
                let value = match offset.wrapping_sub(a.table) % ENTRY {
                    12 if r.u8() < 200 => u64::from(r.u8() < 64),
                    _ => r.value64(),
                };
                let before = made.model.lock().unwrap().page_writes.len();
                let (got, moves) = f.trapped(bar, offset, size, write, value);
                if !traps.contains(&(bar, offset & !(PAGE - 1))) {
                    /* Past the table's last page: one that maps, reaching
                     * nothing here */
                    let ones = match size { 1 => 0xFF, 2 => 0xFFFF, 4 => 0xFFFF_FFFF, _ => u64::MAX };
                    let reached = made.model.lock().unwrap().page_writes.len() - before;
                    invariant!(got == ones && reached == 0, "BAR{} at {:#x}, a page that does not trap, read {:#x} and \
                                                            reached {} times", bar, offset, got, reached);
                    released(&messages, &mut judge);
                    held(&made.model, &judge);
                    continue;
                }
                invariant!(moves.iter().next().is_none(), "a write of MSI-X's page moved a BAR");
                /* Byte by byte, by the spec: the table's and the PBA's the
                 * guest's own, the rest the device's */
                let mut want = 0u64;
                let mut device = Vec::new();
                for k in 0..size as u64 {
                    let v = (value >> (8 * k)) as u8;
                    match judge.msix_byte(bar, offset + k, write.then_some(v)) {
                        Some(b) => want |= u64::from(b) << (8 * k),
                        None => device.push(k),
                    }
                }
                let m = made.model.lock().unwrap();
                if let Some((i, _)) = m.pages.iter().enumerate().find(|(_, p)| p.0 == bar && p.1 == offset & !(PAGE - 1)) {
                    for &k in &device {
                        let at = ((offset + k) & (PAGE - 1)) as usize;
                        want |= u64::from(m.pages[i].2[at]) << (8 * k);
                    }
                }
                let reached = m.page_writes.len() - before;
                drop(m);
                if !write {
                    invariant!(got == want, "BAR{} read {} bytes at {:#x}: {:#x}, not {:#x}", bar, size, offset, got,
                               want);
                }
                invariant!(write || reached == 0, "a read of BAR{} at {:#x} wrote the device", bar, offset);
                invariant!(!write || device.is_empty() || reached > 0,
                           "a write of the device's registers at BAR{} {:#x} never reached it", bar, offset);
            }
            11 if judge.msix.is_some() => {
                /* MSI-X turned on, off, masked whole or not */
                let at = judge.msix_cap + 2 + usize::from(r.u8() < 32);
                let size = if at % 2 == 0 { r.pick(&[2usize, 1]) } else { 1 };
                let value = match r.u8() % 4 {
                    0 => u32::from(MSIX_ENABLE),
                    1 => u32::from(MSIX_ENABLE | MSIX_FUNCTION_MASK),
                    2 => 0,
                    _ => r.value32(),
                } >> (8 * (at - judge.msix_cap - 2));
                let _ = f.config_write(at, size, value);
                let _ = judge.write(at, size, value);
            }
            12 => walked(&f, &made, &judge),
            _ => {
                /* Anything at all: no panic, and no rule broken -- a page that
                 * does not trap among it */
                let at = r.below(300) as usize;
                let size = r.below(9) as usize;
                let value = r.value32();
                match r.u8() % 3 {
                    0 => {
                        let _ = f.config_read(at, size);
                    }
                    1 => {
                        let moved = f.config_write(at, size, value);
                        let want = judge.write(at, size, value);
                        let got = moved.map(|m| (m.bar, m.from, m.to));
                        invariant!(got == want, "a write of {} bytes at {:#x} moved {:?}, not {:?}", size, at, got, want);
                    }
                    _ => {
                        let bar = r.below(7) as usize;
                        let offset = r.u64() & 0xFFFF_FFFF;
                        let trapped = traps.contains(&(bar, offset & !(PAGE - 1)));
                        if !trapped {
                            let before = made.model.lock().unwrap().page_writes.len();
                            let (v, _) = f.trapped(bar, offset, size, r.bool(), u64::from(value));
                            let after = made.model.lock().unwrap().page_writes.len();
                            let ones = match size { 1 => 0xFF, 2 => 0xFFFF, 4 => 0xFFFF_FFFF, _ => u64::MAX };
                            invariant!(after == before && v == ones,
                                       "a page that does not trap reached: BAR{} at {:#x}, read {:#x}", bar, offset, v);
                        }
                    }
                }
            }
        }
        released(&messages, &mut judge);
        held(&made.model, &judge);
    }
}

/// A BAR's registers as the module reads them -- as they were, and what
/// all ones written read back as -- decoded (`decode_bar`), against the
/// spec's: a BAR of `size` keeps the address bits above its size writable in
/// both its registers, none in its low one at 4 GiB and past; given where it
/// is a page or more, assigned and aligned, a 32-bit one 2 GiB at most. And
/// any registers at all decode to a BAR of such a size, or to none.
fn bar_decoded(r: &mut Input) {
    let wide = r.bool();
    let size = 1u64 << (4 + r.below(if wide { 44 } else { 28 }));
    let prefetchable = r.bool();
    let kind = (if wide { 0x4 } else { 0 }) | (if prefetchable { 0x8 } else { 0 });
    let host = match r.u8() % 8 {
        0 => 0,
        1 => r.u64(),
        _ => r.u64() & !(size - 1) & if wide { (1 << 48) - 1 } else { 0xFFFF_FFFF },
    };
    let mask_low = (!((size - 1) as u32) & !0xF) | kind;
    let mask_high = wide.then(|| !((size - 1) >> 32) as u32);
    let low = (host as u32 & !0xF) | kind;
    if r.u8() < 24 {
        /* Registers no device reads back as: decoded to a BAR there could
         * be, or to none */
        let (noise_low, noise_high) = (r.u32(), mask_high.map(|_| r.u32()));
        if let Some(b) = decode_bar(low, noise_low, noise_high, host) {
            invariant!(b.size.is_power_of_two() && b.size >= PAGE && b.host != 0 && b.host % b.size == 0
                       && (b.wide || b.size <= 1 << 31),
                       "masks {:#x} {:?} decoded to a BAR of {:#x} at {:#x}", noise_low, noise_high, b.size, b.host);
        }
        return;
    }
    let got = decode_bar(low, mask_low, mask_high, host);
    let want = (size >= PAGE && host != 0 && host % size == 0)
        .then_some(Bar { host, size, wide, prefetchable });
    invariant!(got == want, "a {} BAR of {:#x} at {:#x}, read back {:#x} {:?}: decoded {:?}, not {:?}",
               if wide { "64-bit" } else { "32-bit" }, size, host, mask_low, mask_high, got, want);
}

/* ---- the platform target's GPU ---- */

/// Where the GPU the platform target gives its guest has what: NVIDIA's,
/// its capabilities PM, MSI (64-bit, maskable or not) and PCIe in their
/// slots, BAR0 of 16 MiB, BAR1 and BAR3 64-bit and prefetchable.
pub const GPU_BAR0_SIZE: u64 = 16 << 20;
pub const GPU_PM: usize = SLOT_PM;
pub const GPU_MSI: usize = SLOT_MSI;
pub const GPU_PCIE: usize = SLOT_PCIE;

pub fn gpu(r: &mut Input) -> Made {
    let mut space = [0u8; SPACE];
    space[0..2].copy_from_slice(&VENDOR_NVIDIA.to_le_bytes());
    space[2..4].copy_from_slice(&r.u16().to_le_bytes());
    space[0x0B] = 0x03;
    let bars = [
        Some(Bar { host: 0xF000_0000, size: GPU_BAR0_SIZE, wide: false, prefetchable: false }),
        Some(Bar { host: 0x40_0000_0000, size: if r.bool() { 8 << 30 } else { 256 << 20 }, wide: true,
                   prefetchable: true }),
        None,
        Some(Bar { host: 0x41_0000_0000, size: 32 << 20, wide: true, prefetchable: true }),
        None,
        None,
    ];
    for (i, b) in bars.iter().enumerate() {
        if let Some(b) = b {
            let low = (b.host as u32) | (if b.wide { 0x4 } else { 0 }) | (if b.prefetchable { 0x8 } else { 0 });
            space[BAR0 + 4 * i..BAR0 + 4 * i + 4].copy_from_slice(&low.to_le_bytes());
            if b.wide {
                space[BAR0 + 4 * i + 4..BAR0 + 4 * i + 8].copy_from_slice(&((b.host >> 32) as u32).to_le_bytes());
            }
        }
    }
    space[STATUS..STATUS + 2].copy_from_slice(&STATUS_CAPABILITIES.to_le_bytes());
    space[INTERRUPT_PIN] = 1;
    space[CAP_POINTER] = SLOT_PM as u8;
    space[SLOT_PM] = CAP_PM;
    space[SLOT_PM + 1] = SLOT_MSI as u8;
    space[SLOT_MSI] = CAP_MSI;
    space[SLOT_MSI + 1] = SLOT_PCIE as u8;
    let control = MSI_64 | if r.bool() { MSI_MASKABLE } else { 0 };
    space[SLOT_MSI + 2..SLOT_MSI + 4].copy_from_slice(&control.to_le_bytes());
    space[SLOT_PCIE] = CAP_PCIE;
    space[SLOT_PCIE + 2] = 0x02;
    space[SLOT_PCIE + 4..SLOT_PCIE + 8].copy_from_slice(&(1u32 << 28).to_le_bytes());
    /* Device control as out of reset: no snoop on, 512-byte reads */
    let devctl = DEVCTL_NO_SNOOP | (0x2 << 12) | (0x1 << 5) | 0x7;
    space[SLOT_PCIE + DEVCTL..SLOT_PCIE + DEVCTL + 2].copy_from_slice(&devctl.to_le_bytes());
    let layout = Layout { pm: SLOT_PM, msi: SLOT_MSI, msix: 0, pcie: SLOT_PCIE, af: 0 };
    let model = Model { space, layout, host_command: 0, host_devctl: devctl, broken: None, page_writes: Vec::new(),
                        pages: Vec::new(), map_fails: false, msix: None, msix_given: false,
                        caps: vec![SLOT_PM, SLOT_MSI, SLOT_PCIE], writes: 0 };
    let caps = vec![(CAP_PM, SLOT_PM), (CAP_MSI, SLOT_MSI), (CAP_PCIE, SLOT_PCIE)];
    Made { model: Arc::new(Mutex::new(model)), bars, givable: true, caps }
}

/// The host's rules, after a run of a whole guest that held the function:
/// none broken, its command register decoding memory, no I/O, INTx off, and
/// phantom functions, no snoop and FLR off.
pub fn rules_held(model: &Arc<Mutex<Model>>) {
    let m = model.lock().unwrap();
    invariant!(m.broken.is_none(), "{}", m.broken.as_deref().unwrap_or(""));
    let command = m.command();
    invariant!(command & COMMAND_MEMORY != 0 && command & COMMAND_IO == 0 && command & COMMAND_INTX_DISABLE != 0,
               "the real command register {:#06x}", command);
    if let Some(devctl) = m.devctl() {
        invariant!(devctl & (DEVCTL_PHANTOM | DEVCTL_NO_SNOOP | DEVCTL_FLR) == 0,
                   "device control {:#06x}: phantom functions, no snoop or FLR on", devctl);
    }
}
