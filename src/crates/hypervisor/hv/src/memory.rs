//! Guest physical memory: the host pages a guest's RAM is made of, the
//! nested table that is the guest's only way to them, and the host's only
//! way to read and write them.

use alloc::vec::Vec;
#[cfg(target_arch = "x86_64")]
use core::sync::atomic::{AtomicU64, Ordering};

use hvarch::{Error, Result};
use kcore::consts::PAGE_SIZE;
#[cfg(target_arch = "x86_64")]
use kcore::frame::{Frame, HUGE_PAGES};
use kcore::frame::{FramePage, Run, HUGE_SIZE};
use kcore::pod::{self, Pod};
use kcore::sync::Mutex;

#[cfg(target_arch = "x86_64")]
use crate::ept::Ept;
#[cfg(target_arch = "x86_64")]
use crate::npt::Npt;
#[cfg(target_arch = "x86_64")]
use hvarch::Vendor;

use crate::dirty;

/// The second-level translation, whichever the machine has: AMD's nested
/// page table or Intel's extended one. Both are built from the host and offer
/// the same operations, including clearing a mapping when its BAR moves, so
/// the memory above them is one set of code over an enum, not two.
#[cfg(target_arch = "x86_64")]
enum SecondLevel {
    Npt(Npt),
    Ept(Ept),
}

#[cfg(target_arch = "x86_64")]
impl SecondLevel {
    fn prepare(&mut self, gpa: u64, size: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.prepare(gpa, size),
            SecondLevel::Ept(t) => t.prepare(gpa, size),
        }
    }
    fn set(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set(gpa, hpa),
            SecondLevel::Ept(t) => t.set(gpa, hpa),
        }
    }
    fn set_ro(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set_ro(gpa, hpa),
            SecondLevel::Ept(t) => t.set_ro(gpa, hpa),
        }
    }
    fn set_rx(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set_ro(gpa, hpa),
            SecondLevel::Ept(t) => t.set_rx(gpa, hpa),
        }
    }
    fn prepare_huge(&mut self, gpa: u64, size: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.prepare_huge(gpa, size),
            SecondLevel::Ept(t) => t.prepare_huge(gpa, size),
        }
    }
    fn set_huge(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set_huge(gpa, hpa),
            SecondLevel::Ept(t) => t.set_huge(gpa, hpa),
        }
    }
    fn set_device(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set_device(gpa, hpa),
            SecondLevel::Ept(t) => t.set_device(gpa, hpa),
        }
    }
    fn set_huge_device(&mut self, gpa: u64, hpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.set_huge_device(gpa, hpa),
            SecondLevel::Ept(t) => t.set_huge_device(gpa, hpa),
        }
    }
    fn clear(&mut self, gpa: u64) -> Result<()> {
        match self {
            SecondLevel::Npt(t) => t.clear(gpa),
            SecondLevel::Ept(t) => t.clear(gpa),
        }
    }
    fn nested(&self) -> hvarch::x86::svm::Nested {
        match self {
            SecondLevel::Npt(t) => t.nested(),
            SecondLevel::Ept(t) => t.nested(),
        }
    }
    fn renew(&mut self) -> u64 {
        match self {
            SecondLevel::Npt(t) => {
                t.renew();
                t.id()
            }
            SecondLevel::Ept(t) => {
                t.renew();
                t.id()
            }
        }
    }
}

/// The dirty log's way to the table (`crate::dirty`).
#[cfg(target_arch = "x86_64")]
impl dirty::Table for SecondLevel {
    fn protect(&mut self, gpa: u64) -> bool {
        match self {
            SecondLevel::Npt(t) => t.protect(gpa).is_ok(),
            SecondLevel::Ept(t) => t.protect(gpa).is_ok(),
        }
    }
    fn unprotect(&mut self, gpa: u64) -> bool {
        match self {
            SecondLevel::Npt(t) => t.unprotect(gpa).is_ok(),
            SecondLevel::Ept(t) => t.unprotect(gpa).is_ok(),
        }
    }
}

/// The dirty log names an MSI-X page by the region that owns it; its nested
/// mapping may have moved. An unmapped BAR has nothing to protect, and is
/// mapped read-only if the guest moves it again before the log is over.
#[cfg(target_arch = "x86_64")]
struct DirtyTable<'a> {
    table: &'a mut SecondLevel,
    msix: &'a [MsixMapping],
}

#[cfg(target_arch = "x86_64")]
impl dirty::Table for DirtyTable<'_> {
    fn protect(&mut self, gpa: u64) -> bool {
        let page = gpa & !(PAGE - 1);
        match self.msix.iter().find(|m| m.backing == page) {
            Some(m) => !m.mapped || self.table.protect(m.at),
            None => self.table.protect(gpa),
        }
    }

    fn unprotect(&mut self, gpa: u64) -> bool {
        let page = gpa & !(PAGE - 1);
        match self.msix.iter().find(|m| m.backing == page) {
            Some(m) => !m.mapped || self.table.unprotect(m.at),
            None => self.table.unprotect(gpa),
        }
    }
}

const PAGE: u64 = PAGE_SIZE as u64;

/// Guest pages mapped into the host's space for as long as this lives
/// (`GuestMemory::map`): read and written by offset into them, one plain
/// value at a time.
pub use kcore::frame::Mapped as Mapping;

/// Where a guest's physical address space ends: four levels of nested table
/// translate 48 bits, and an address above that faults whatever is mapped.
pub const MAX_GPA: u64 = 1 << 48;

/// The platform's MMIO window below 4 GiB, where a PC keeps its chipset's
/// registers (the IOAPIC, the HPET, AMD's FCH, the local APIC, the firmware
/// flash): what [`GuestMemory::map_absent`] answers reads of.
#[cfg(target_arch = "x86_64")]
const MMIO_WINDOW: core::ops::Range<u64> = 0xC000_0000..0x1_0000_0000;
/// The most pages of it a guest may have answered that way: a guest that
/// walks the whole window is not probing for a device.
#[cfg(target_arch = "x86_64")]
const MAX_ABSENT_PAGES: usize = 64;
/// The local APIC's page in xAPIC mode: a guest's APIC reached here, each
/// access decoded and performed on its emulated registers (`mmio.rs`,
/// `lapic.rs`) -- never answered with all ones as an absent device's page
/// is (`map_absent`).
#[cfg(target_arch = "x86_64")]
pub const XAPIC_PAGE: u64 = 0xFEE0_0000;

/// A guest's memory.
///
/// Its pages are [`Frame`]s -- or, given from the huge pool (`add_huge`),
/// 2 MiB blocks of it, each one entry of the nested table where the CPU
/// walks those: RAM the kernel handed out by address and mapped
/// nowhere, so that the guest's nested table is the only mapping of it there
/// is, and a guest of any size costs pages and no kernel address space. The
/// guest writes it whenever it runs, on whatever CPU it runs on, so nothing
/// here hands out a reference into it -- a reference that changes under the
/// compiler is undefined behaviour however it is used. Every access is a
/// copy, bounds-checked against the region it falls in, made by the kernel
/// through its temporary window onto the page: an address the guest gave,
/// whatever it is, is at worst an `Unmapped`. The exception is a few pages
/// a device reads and writes at every request, its queues' rings, which it
/// has mapped while the queue is up (`map`) -- three pages a queue, and
/// still no reference: a load or a store of one value at a time.
///
/// The nested table is inside, and that is the point of it. A guest reaches
/// exactly what its nested table maps, and the table here maps nothing but
/// pages this value owns -- each one owned before it is mapped, and freed
/// only with the table -- so "the guest can reach host memory it was not
/// given" is not a thing a caller can get wrong.
///
/// A guest of several CPUs shares one: every vCPU's task copies in and out
/// of it (`read` and `write` take `&self`, each copy a CPU's own), and any
/// of them may find an absent device (`map_absent`), which grows the table
/// under a lock. The regions are fixed once the guest is built.
pub struct GuestMemory {
    /* Before the regions and the absent page, so that it is dropped first:
     * the table goes before the pages it maps are back on the free list. */
    #[cfg(target_arch = "x86_64")]
    table: Mutex<SecondLevel>,
    /// The table's top level, as an entry names it: it does not change for
    /// as long as the table lives -- what a CPU knows its guest's memory by
    /// (`vm::Cpu`) -- so an entry reads it here without the table's lock.
    #[cfg(target_arch = "x86_64")]
    root: u64,
    /// The table's identity for the TLBs: new each time a migration's
    /// dirty log narrows the table (`log_start`, `log_take`), and taken by
    /// each entry, without the lock, as the one it enters over.
    #[cfg(target_arch = "x86_64")]
    id: AtomicU64,
    regions: Vec<Region>,
    /// The NUMA node its RAM is taken from first (`place_on`): the node of
    /// the CPU taking it where None
    node: Option<usize>,
    /// Which of its pages were written since the dirty log last looked
    /// (`crate::dirty`): the host's writes marked here as they are made, the
    /// guest's at its nested faults while the log looks after them.
    log: dirty::Log,
    /// The page of all ones a guest's reads of an absent device find, and
    /// where it is mapped.
    absent: Mutex<Absent>,
    /// The BARs of the host's functions the guest holds, mapped straight:
    /// where each is in the guest's space, and how long. Not RAM -- in no
    /// region, the dirty log's or anyone's to copy -- and not this value's
    /// pages: a function's, whose claim the guest holds for as long as it
    /// lives, which outlives this table (`run::LinuxGuest`'s fields' order).
    #[cfg(target_arch = "x86_64")]
    devices: Mutex<Vec<(u64, u64)>>,
    /// Each virtio MSI-X page: the region that owns its frame, and where its
    /// nested mapping is now. The backing address remains the host's way to
    /// the table and the dirty log's name for it.
    #[cfg(target_arch = "x86_64")]
    msix: Mutex<Vec<MsixMapping>>,
    /// Hyper-V's two pages: each one's GPA now, and the RAM frame it covers.
    #[cfg(target_arch = "x86_64")]
    overlays: Mutex<[Overlay; 2]>,
    /// Copies of the host's functions' ROMs, a function's frames each by its
    /// slot (`map_rom`): after the table, so that it goes before they are
    /// back on the free list.
    #[cfg(target_arch = "x86_64")]
    roms: Mutex<Vec<(usize, Vec<Frame>)>>,
}

/// What a device range of the guest's maps: a BAR of the host's -- where it
/// is, and the pages of it that trap -- a copy in frames, read-only -- or
/// nothing, every access of it trapping.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
enum DevicePages<'a> {
    Bar(u64, &'a [u64]),
    Copy(&'a [Frame]),
    None,
}

/// Hyper-V's hypercall and reference TSC pages, independently mapped.
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
pub enum OverlaySlot {
    Hypercall,
    ReferenceTsc,
}

/// A page mapped over RAM or a hole. `under` is the RAM frame to restore,
/// None where the GPA was a hole.
#[cfg(target_arch = "x86_64")]
struct Overlay {
    at: u64,
    under: Option<u64>,
    mapped: bool,
    /// Owned before it can be mapped, and kept with the nested table.
    frame: Option<Frame>,
}

#[cfg(target_arch = "x86_64")]
impl Overlay {
    const fn new() -> Self {
        Self { at: 0, under: None, mapped: false, frame: None }
    }
}

/// What [`GuestMemory::map_absent`] has made so far.
struct Absent {
    /// A page of all ones: made the first time one is needed, mapped
    /// read-only wherever it is.
    #[cfg(target_arch = "x86_64")]
    frame: Option<Frame>,
    /// Where it is mapped, page-aligned, in the order the guest found them.
    at: Vec<u64>,
}

/// One run of guest physical addresses with memory behind it.
struct Region {
    base: u64,
    size: u64,
    /// Its pages, in order -- frames of their own, or blocks of the huge
    /// pool: shared with every mapping of some of them (`GuestMemory::map`),
    /// which keeps them until it is undone.
    frames: Run,
    /// They are the huge pool's.
    huge: bool,
}

#[cfg(target_arch = "x86_64")]
struct MsixMapping {
    backing: u64,
    at: u64,
    mapped: bool,
}

impl Region {
    fn contains(&self, gpa: u64, len: u64) -> bool {
        gpa >= self.base && len <= self.size && gpa - self.base <= self.size - len
    }

    /// The page `gpa` is in, and where in it: None for an address outside
    /// the region.
    fn page(&self, gpa: u64) -> Option<(FramePage<'_>, usize)> {
        let page = usize::try_from(gpa.checked_sub(self.base)? / PAGE).ok()?;
        Some((self.frames.page(page)?, (gpa % PAGE) as usize))
    }
}

impl GuestMemory {
    /// No memory at all, and a second-level table -- of the kind the
    /// machine's `vendor` calls for -- that maps nothing.
    #[cfg(target_arch = "x86_64")]
    pub fn new(vendor: Vendor) -> Result<Self> {
        let table = match vendor {
            Vendor::Svm => SecondLevel::Npt(Npt::new()?),
            Vendor::Vmx => SecondLevel::Ept(Ept::new()?),
            _ => return Err(Error::NotImplemented),
        };
        let nested = table.nested();
        Ok(Self {
            table: Mutex::new(table).ok_or(Error::NoMemory)?,
            root: nested.root,
            id: AtomicU64::new(nested.id),
            regions: Vec::new(),
            node: None,
            log: dirty::Log::new(),
            absent: Mutex::new(Absent { frame: None, at: Vec::new() }).ok_or(Error::NoMemory)?,
            devices: Mutex::new(Vec::new()).ok_or(Error::NoMemory)?,
            msix: Mutex::new(Vec::new()).ok_or(Error::NoMemory)?,
            overlays: Mutex::new([Overlay::new(), Overlay::new()]).ok_or(Error::NoMemory)?,
            roms: Mutex::new(Vec::new()).ok_or(Error::NoMemory)?,
        })
    }

    /// `size` bytes of a host function's BAR at the guest's `gpa`, the host's
    /// `hpa`, mapped straight and uncacheable -- 2 MiB at a time where both
    /// are aligned to it -- but for `holes`, offsets into it of pages left
    /// out: those trap, and the run loop performs them (`run::host_mmio`).
    /// Refused where it would cover RAM, another BAR, a page of all ones
    /// made for the guest (`map_absent`) or the platform's own pages, or is
    /// not whole pages; mapped whole or not at all.
    #[cfg(target_arch = "x86_64")]
    pub fn map_device(&self, gpa: u64, hpa: u64, size: u64, holes: &[u64]) -> Result<()> {
        self.add_device(gpa, size, DevicePages::Bar(hpa, holes))
    }

    /// `size` bytes at the guest's `gpa` that are a device's of the run
    /// loop's and map nothing -- the PCI Express configuration space's
    /// window (`linux::ECAM_BASE`) -- every access of them a nested fault the
    /// run loop answers (`run::host_mmio`); an absent device's page never
    /// made there. Refused where `map_device` would be.
    #[cfg(target_arch = "x86_64")]
    pub fn trap_range(&self, gpa: u64, size: u64) -> Result<()> {
        self.add_device(gpa, size, DevicePages::None)
    }

    /// The ROM of the host's function `id` (its slot on the guest's bus) at
    /// the guest's `gpa`: `image`, whole pages of it, a copy in frames of the
    /// host's mapped read-only -- a read is the guest's own, a write a nested
    /// fault the run loop takes for nothing (`run::host_mmio`). The frames
    /// are made the first time and kept for as long as the table lives, the
    /// ROM moved or turned off again included: a frame let go of while a
    /// CPU's TLB may still map it would be the host's memory to read.
    /// Refused where `map_device` would be; taken out by `unmap_device`.
    #[cfg(target_arch = "x86_64")]
    pub fn map_rom(&self, id: usize, gpa: u64, image: &[u8]) -> Result<()> {
        let size = image.len() as u64;
        if size == 0 || size % PAGE != 0 {
            return Err(Error::BadAddress);
        }
        let mut roms = self.roms.lock();
        let at = match roms.iter().position(|(i, _)| *i == id) {
            Some(at) => at,
            None => {
                let mut frames = Vec::new();
                frames.try_reserve_exact(image.len() / PAGE_SIZE).map_err(|_| Error::NoMemory)?;
                for piece in image.chunks(PAGE_SIZE) {
                    let frame = Frame::new().ok_or(Error::NoMemory)?;
                    if !frame.write(0, piece) {
                        return Err(Error::NoMemory);
                    }
                    frames.push(frame);
                }
                roms.try_reserve(1).map_err(|_| Error::NoMemory)?;
                roms.push((id, frames));
                roms.len() - 1
            }
        };
        let frames = &roms[at].1;
        if frames.len() as u64 * PAGE != size {
            return Err(Error::BadAddress);
        }
        self.add_device(gpa, size, DevicePages::Copy(frames))
    }

    /// `map_device`'s, `map_rom`'s and `trap_range`'s: the range held to the rules, then its
    /// pages set, and the range noted.
    #[cfg(target_arch = "x86_64")]
    fn add_device(&self, gpa: u64, size: u64, pages: DevicePages<'_>) -> Result<()> {
        let hpa = match pages {
            DevicePages::Bar(h, _) => h,
            DevicePages::Copy(_) | DevicePages::None => 0,
        };
        let end = gpa.checked_add(size).ok_or(Error::BadAddress)?;
        if size == 0 || gpa % PAGE != 0 || hpa % PAGE != 0 || size % PAGE != 0 || end > MAX_GPA
            || self.regions.iter().any(|r| gpa < r.base + r.size && r.base < end)
            || (gpa..end).contains(&XAPIC_PAGE)
        {
            return Err(Error::BadAddress);
        }
        /* `map_absent`'s order: nothing of the range is anyone else's below */
        let absent = self.absent.lock();
        if absent.at.iter().any(|&p| p >= gpa && p < end) {
            return Err(Error::BadAddress);
        }
        let mut devices = self.devices.lock();
        if devices.iter().any(|&(b, s)| gpa < b + s && b < end) {
            return Err(Error::BadAddress);
        }
        let msix = self.msix.lock();
        if msix.iter().any(|m| m.mapped && gpa < m.at + PAGE && m.at < end) {
            return Err(Error::BadAddress);
        }
        let overlays = self.overlays.lock();
        if overlays.iter().any(|o| o.mapped && gpa <= o.at && o.at < end) {
            return Err(Error::BadAddress);
        }
        devices.try_reserve(1).map_err(|_| Error::NoMemory)?;

        let mut table = self.table.lock();
        let mut at = 0u64;
        let set = match pages {
            DevicePages::Bar(_, holes) => Self::set_device_pages(&mut table, gpa, hpa, size, holes, &mut at),
            DevicePages::Copy(frames) => Self::set_copy_pages(&mut table, gpa, frames, &mut at),
            DevicePages::None => Ok(()),
        };
        if let Err(e) = set {
            /* What was set of it taken out again -- nothing in the range was
             * anyone else's -- and dropped by every CPU that may have cached
             * it in the moment it was there */
            let mut back = 0u64;
            while back < at {
                let _ = table.clear(gpa + back);
                back += PAGE;
            }
            self.renew(&mut table);
            return Err(e);
        }
        devices.push((gpa, size));
        drop(absent);
        Ok(())
    }

    /// `map_device`'s pages set, `at` how far it got.
    #[cfg(target_arch = "x86_64")]
    fn set_device_pages(table: &mut SecondLevel, gpa: u64, hpa: u64, size: u64, holes: &[u64], at: &mut u64)
        -> Result<()>
    {
        let huge = HUGE_SIZE;
        let in_hole = |from: u64, len: u64| holes.iter().any(|&h| h < from + len && from < h + PAGE);
        while *at < size {
            let from = *at;
            let whole = (gpa + from) % huge == 0 && (hpa + from) % huge == 0 && size - from >= huge
                && !in_hole(from, huge);
            /* A table of 4 KiB pages made there before -- a BAR that was
             * there once -- keeps these 2 MiB in pages too */
            if whole && table.prepare_huge(gpa + from, huge).is_ok() {
                /* Counted before it is set: a failure clears what may be set */
                *at = from + huge;
                table.set_huge_device(gpa + from, hpa + from)?;
                continue;
            }
            *at = from + PAGE;
            if !in_hole(from, PAGE) {
                table.prepare(gpa + from, PAGE)?;
                table.set_device(gpa + from, hpa + from)?;
            }
        }
        Ok(())
    }

    /// `map_rom`'s pages set, read-only, `at` how far it got.
    #[cfg(target_arch = "x86_64")]
    fn set_copy_pages(table: &mut SecondLevel, gpa: u64, frames: &[Frame], at: &mut u64) -> Result<()> {
        for frame in frames {
            let from = *at;
            *at = from + PAGE;
            table.prepare(gpa + from, PAGE)?;
            table.set_ro(gpa + from, frame.phys())?;
        }
        Ok(())
    }

    /// The BAR mapped at `gpa`, `size` long, taken out of the table -- the
    /// guest moved it -- and every CPU to drop what it cached of it as it
    /// next enters (`renew`).
    #[cfg(target_arch = "x86_64")]
    pub fn unmap_device(&self, gpa: u64, size: u64) -> Result<u64> {
        let mut devices = self.devices.lock();
        let Some(i) = devices.iter().position(|&d| d == (gpa, size)) else {
            return Err(Error::Unmapped);
        };
        let mut table = self.table.lock();
        let mut at = 0u64;
        while at < size {
            table.clear(gpa + at)?;
            at += PAGE;
        }
        devices.swap_remove(i);
        Ok(self.renew(&mut table))
    }

    /// An overlay's contents and frame, made or refreshed before a vCPU can run.
    #[cfg(target_arch = "x86_64")]
    pub fn prepare_overlay(&self, slot: OverlaySlot, contents: &[u8]) -> Result<()> {
        let mut overlays = self.overlays.lock();
        let overlay = &mut overlays[slot as usize];
        if let Some(frame) = &overlay.frame {
            if !frame.write(0, contents) {
                return Err(Error::BadAddress);
            }
            return Ok(());
        }
        let frame = Frame::new().ok_or(Error::NoMemory)?;
        if !frame.write(0, contents) {
            return Err(Error::BadAddress);
        }
        overlay.frame = Some(frame);
        Ok(())
    }

    /// Map an overlay at `gpa`, over RAM or a hole, read-only and executable
    /// only for the hypercall page. An existing one moves only after the new
    /// mapping is in place; the old GPA then gets its original RAM frame back.
    /// No host-side access is redirected: `read` and `write` still name RAM.
    #[cfg(target_arch = "x86_64")]
    pub fn map_overlay(&self, slot: OverlaySlot, gpa: u64) -> Result<()> {
        if gpa % PAGE != 0 || gpa.checked_add(PAGE).is_none_or(|end| end > MAX_GPA) || gpa == XAPIC_PAGE {
            return Err(Error::BadAddress);
        }
        /* The same order as map_absent/add_device/move_msix, the overlay's
         * lock immediately before the table's. */
        let absent = self.absent.lock();
        if absent.at.contains(&gpa) {
            return Err(Error::BadAddress);
        }
        let devices = self.devices.lock();
        if devices.iter().any(|&(b, s)| gpa >= b && gpa - b < s) {
            return Err(Error::BadAddress);
        }
        let msix = self.msix.lock();
        if msix.iter().any(|m| m.mapped && m.at == gpa) {
            return Err(Error::BadAddress);
        }
        let mut overlays = self.overlays.lock();
        let index = slot as usize;
        if overlays.iter().enumerate().any(|(i, o)| i != index && o.mapped && o.at == gpa) {
            return Err(Error::BadAddress);
        }
        if overlays[index].mapped && overlays[index].at == gpa {
            return Ok(());
        }
        let hpa = overlays[index].frame.as_ref().map(|f| f.phys()).ok_or(Error::NoMemory)?;
        let under = match self.region(gpa, PAGE_SIZE) {
            Ok(i) => Some(self.regions[i].page(gpa).ok_or(Error::Unmapped)?.0.phys()),
            Err(Error::Unmapped) => None,
            Err(e) => return Err(e),
        };
        let (old_at, old_under, old_mapped) = (overlays[index].at, overlays[index].under, overlays[index].mapped);
        let mut table = self.table.lock();
        let result = (|| {
            if under.is_some() {
                /* Also splits a 2 MiB RAM mapping, so the one page can be
                 * replaced without changing any of its neighbours. */
                if !dirty::Table::protect(&mut *table, gpa) {
                    return Err(Error::Unmapped);
                }
                table.clear(gpa)?;
            }
            table.prepare(gpa, PAGE)?;
            match slot {
                OverlaySlot::Hypercall => table.set_rx(gpa, hpa)?,
                OverlaySlot::ReferenceTsc => table.set_ro(gpa, hpa)?,
            }
            if old_mapped {
                self.restore_overlay(&mut table, old_at, old_under)?;
            }
            Ok(())
        })();
        self.renew(&mut table);
        if let Err(e) = result {
            if under.is_some() {
                let _ = table.clear(gpa);
                let _ = self.restore_overlay(&mut table, gpa, under);
                self.renew(&mut table);
            }
            return Err(e);
        }
        overlays[index].at = gpa;
        overlays[index].under = under;
        overlays[index].mapped = true;
        Ok(())
    }

    /// Remove one overlay, restoring the RAM frame it covered.
    #[cfg(target_arch = "x86_64")]
    pub fn unmap_overlay(&self, slot: OverlaySlot) -> Result<()> {
        let mut overlays = self.overlays.lock();
        let overlay = &mut overlays[slot as usize];
        if !overlay.mapped {
            return Ok(());
        }
        let (at, under) = (overlay.at, overlay.under);
        let mut table = self.table.lock();
        self.restore_overlay(&mut table, at, under)?;
        self.renew(&mut table);
        overlay.at = 0;
        overlay.under = None;
        overlay.mapped = false;
        Ok(())
    }

    /// Whether `gpa` writes a read-only Hyper-V overlay: a #GP, not a
    /// dirty-log fault and not an unexpected nested fault that stops the VM.
    #[cfg(target_arch = "x86_64")]
    pub fn overlay_contains(&self, gpa: u64) -> bool {
        let overlays = self.overlays.lock();
        overlays.iter().any(|o| o.mapped && gpa >= o.at && gpa - o.at < PAGE)
    }

    #[cfg(target_arch = "x86_64")]
    fn restore_overlay(&self, table: &mut SecondLevel, at: u64, under: Option<u64>) -> Result<()> {
        /* The overlay may own the entry now; a failed move may have left the
         * RAM page's entry clear. Either way, restore starts with no mapping. */
        let _ = table.clear(at);
        if let Some(hpa) = under {
            table.prepare(at, PAGE)?;
            table.set(at, hpa)?;
            if self.log.active() && !dirty::Table::protect(table, at) {
                return Err(Error::Unmapped);
            }
        }
        Ok(())
    }

    /// Move the nested mapping of the MSI-X page owned by the one-page region
    /// at `backing`, from its current BAR address to `to`. The frame and its
    /// host-facing address do not move. As with a passthrough BAR, an invalid
    /// or unallocatable destination leaves the BAR unmapped; a later write
    /// can move it again. Only this page's own old mapping is cleared.
    #[cfg(target_arch = "x86_64")]
    pub fn move_msix(&self, backing: u64, from: u64, to: u64) -> Result<u64> {
        let region = self.regions.iter().find(|r| r.base == backing && r.size == PAGE).ok_or(Error::Unmapped)?;
        let mut extents = region.frames.extents();
        let extent = extents.next().ok_or(Error::Unmapped)?;
        if extent.pages != 1 || extents.next().is_some() {
            return Err(Error::BadAddress);
        }
        let hpa = extent.phys;

        let absent = self.absent.lock();
        let devices = self.devices.lock();
        let mut msix = self.msix.lock();
        let overlays = self.overlays.lock();
        let i = msix.iter().position(|m| m.backing == backing).ok_or(Error::Unmapped)?;
        if msix[i].at != from {
            return Err(Error::BadAddress);
        }
        let old_mapped = msix[i].mapped;
        let valid = to.checked_add(PAGE).is_some_and(|end| {
            to % PAGE == 0 && end <= MAX_GPA && to != XAPIC_PAGE
                && (to == backing || !self.regions.iter().any(|r| to < r.base + r.size && r.base < end))
                && !absent.at.contains(&to)
                && !devices.iter().any(|&(b, s)| to < b + s && b < end)
                && !msix.iter().enumerate().any(|(j, m)| j != i && m.mapped && to < m.at + PAGE && m.at < end)
                && overlays.iter().all(|o| !o.mapped || o.at != to)
        });

        let mut table = self.table.lock();
        let prepare = valid && table.prepare(to, PAGE).is_ok();
        if old_mapped {
            table.clear(from)?;
        }
        let mapped = prepare
            && if self.log.active() { table.set_ro(to, hpa).is_ok() } else { table.set(to, hpa).is_ok() };
        let mapping = &mut msix[i];
        mapping.at = to;
        mapping.mapped = mapped;
        Ok(self.renew(&mut table))
    }

    /// Whether `gpa` is in a host function's BAR the guest holds.
    #[cfg(target_arch = "x86_64")]
    pub fn is_device(&self, gpa: u64) -> bool {
        self.devices.lock().iter().any(|&(b, s)| gpa >= b && gpa - b < s)
    }

    /// No memory, and no second-level table: this architecture runs no
    /// guests yet.
    #[cfg(not(target_arch = "x86_64"))]
    pub fn new() -> Result<Self> {
        Ok(Self {
            regions: Vec::new(),
            node: None,
            log: dirty::Log::new(),
            absent: Mutex::new(Absent { at: Vec::new() }).ok_or(Error::NoMemory)?,
        })
    }

    /// Its RAM taken from NUMA node `node` first (`kcore::numa`), from
    /// here on: the node its CPUs are placed on, so that what they read is
    /// near -- a node with none to give, the nearest that has.
    pub fn place_on(&mut self, node: usize) {
        self.node = Some(node);
    }

    /// Of its RAM, the pages on NUMA node `node`, and all of them: what its
    /// placement asked of the allocator, as their physical addresses say --
    /// a block of the pool the node's its first page is.
    pub fn pages_on(&self, node: usize) -> (u64, u64) {
        let mut on = 0u64;
        let mut all = 0u64;
        for region in &self.regions {
            for e in region.frames.extents() {
                all += e.pages as u64;
                if kcore::numa::phys_node(e.phys) == node {
                    on += e.pages as u64;
                }
            }
        }
        (on, all)
    }

    /// Give the guest `size` bytes of zeroed memory at `base`, a frame a
    /// page. Both are whole pages, and the range is clear of every region
    /// already given.
    pub fn add(&mut self, base: u64, size: u64) -> Result<()> {
        let pages = self.check_new(base, size, PAGE)?;
        /* The kernel zeroes every frame it hands out: what the guest finds
         * in its memory is what it was given, never what the host left
         * there. A failure part way drops what was taken. */
        let frames = match self.node {
            Some(node) => Run::new_on(pages, node),
            None => Run::new(pages),
        }
        .ok_or(Error::NoMemory)?;
        self.place(base, size, frames, false, false)
    }

    /// A virtio function's MSI-X table page: owned by a one-page region at
    /// `base`, with its first nested mapping there. Its BAR can move later.
    #[cfg(target_arch = "x86_64")]
    pub fn add_msix(&mut self, base: u64) -> Result<()> {
        self.msix.lock().try_reserve(1).map_err(|_| Error::NoMemory)?;
        self.add(base, PAGE)?;
        self.msix.lock().push(MsixMapping { backing: base, at: base, mapped: true });
        Ok(())
    }

    /// Give the guest `size` bytes of zeroed memory at `base` from the huge
    /// pool (`hugepages=`), a block of it every 2 MiB: both whole blocks,
    /// and the range clear of every region already given. Each block is one
    /// entry of the nested table where `leaves_2m` says the CPU walks those
    /// (`hvarch::x86::Caps::nested_2m`), and 512 where it does not -- the
    /// pool's memory either way. `NoHugePages` when the pool has not that
    /// many free, the blocks taken by then going back.
    pub fn add_huge(&mut self, base: u64, size: u64, leaves_2m: bool) -> Result<()> {
        let pages = self.check_new(base, size, HUGE_SIZE)?;
        let frames = match self.node {
            Some(node) => Run::new_huge_on(pages, node),
            None => Run::new_huge(pages),
        }
        .map_err(|short| match short {
            kcore::frame::Shortage::Pool => Error::NoHugePages,
            kcore::frame::Shortage::Memory => Error::NoMemory,
        })?;
        self.place(base, size, frames, true, leaves_2m)
    }

    /// How many pages `[base, base + size)` is, when it is whole multiples
    /// of `unit`, below `MAX_GPA` and clear of every region already given.
    fn check_new(&self, base: u64, size: u64, unit: u64) -> Result<usize> {
        if size == 0 || base % unit != 0 || size % unit != 0 {
            return Err(Error::BadAddress);
        }
        match base.checked_add(size) {
            Some(end) if end <= MAX_GPA => {}
            _ => return Err(Error::BadAddress),
        }
        if self.regions.iter().any(|r| base < r.base + r.size && r.base < base + size) {
            return Err(Error::BadAddress);
        }
        usize::try_from(size / PAGE).map_err(|_| Error::NoMemory)
    }

    /// `frames` made the guest's at `base`: a region, and the nested
    /// table's entries for it -- a block of the pool one entry where
    /// `leaves_2m`, every other page one of its own.
    fn place(&mut self, base: u64, size: u64, frames: Run, huge: bool, leaves_2m: bool) -> Result<()> {
        /* The tables next, which is all that can fail for want of memory:
         * a failure there leaves empty tables and no page mapped. */
        #[cfg(target_arch = "x86_64")]
        let mut table = self.table.lock();
        #[cfg(target_arch = "x86_64")]
        if leaves_2m {
            table.prepare_huge(base, size)?;
        } else {
            table.prepare(base, size)?;
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = leaves_2m;

        self.regions.try_reserve(1).map_err(|_| Error::NoMemory)?;
        if !self.log.add(base, size) {
            return Err(Error::NoMemory);
        }
        self.regions.push(Region { base, size, frames, huge });

        /* Owned first, mapped after, so every page the table points at is
         * this value's from before the moment it is reachable. */
        #[cfg(target_arch = "x86_64")]
        {
            let region = self.regions.last().expect("just pushed");
            let mut gpa = base;
            for e in region.frames.extents() {
                if leaves_2m && e.pages == HUGE_PAGES {
                    table.set_huge(gpa, e.phys)?;
                } else {
                    for k in 0..e.pages as u64 {
                        table.set(gpa + k * PAGE, e.phys + k * PAGE)?;
                    }
                }
                gpa += e.pages as u64 * PAGE;
            }
        }
        Ok(())
    }

    /// Answer the guest's read of `gpa` -- in the platform's MMIO window and
    /// backed by no memory -- as an empty bus answers one: with all ones. A
    /// page of them is mapped there, read-only, and the guest's instruction
    /// runs again and reads it. What makes this necessary is Linux on a Zen
    /// CPU reading the reset-status register of AMD's FCH at a fixed address,
    /// on hardware that has one and in a guest that does not; all ones is
    /// what it takes for "no such device" and moves on from.
    ///
    /// A write is not answered this way, nor a read anywhere else: a write to
    /// nothing is no probe a guest makes, and a read of an address outside
    /// the window is a guest's mistake -- the decoder (`insn.rs`) could
    /// step past either, and stopping says more. So a write to the page is
    /// still a nested fault, and stops the guest; so is one read past its
    /// RAM. And not the local APIC's own page (`XAPIC_PAGE`), whose accesses
    /// are the APIC's in xAPIC mode (`run.rs`'s `xapic_access`).
    #[cfg(target_arch = "x86_64")]
    pub fn map_absent(&self, gpa: u64) -> Result<()> {
        let page = gpa & !(PAGE - 1);
        if !MMIO_WINDOW.contains(&page) || page == XAPIC_PAGE || self.region(page, PAGE_SIZE).is_ok() {
            return Err(Error::BadAddress);
        }
        /* The absent pages' lock, then the devices', then the table's: the
         * one order anything here takes them in -- so a BAR the guest moves
         * here as it reads the page is either here first, and the page is
         * not made, or not yet, and refused the page (`map_device`). */
        let mut absent = self.absent.lock();
        #[cfg(target_arch = "x86_64")]
        if self.is_device(page) {
            return Err(Error::BadAddress);
        }
        let msix = self.msix.lock();
        if msix.iter().any(|m| m.mapped && m.at == page) {
            return Err(Error::BadAddress);
        }
        let overlays = self.overlays.lock();
        if overlays.iter().any(|o| o.mapped && o.at == page) {
            return Err(Error::BadAddress);
        }
        if absent.at.contains(&page) {
            /* Another of the guest's CPUs read it first, and it is mapped
             * already: this one's read runs again and finds it. */
            return Ok(());
        }
        if absent.at.len() >= MAX_ABSENT_PAGES {
            return Err(Error::NoMemory);
        }
        if absent.frame.is_none() {
            /* Filled a piece at a time: a page of it on the task's stack is
             * a page the stack may not have. */
            const PIECE: usize = 512;
            let frame = Frame::new().ok_or(Error::NoMemory)?;
            for at in (0..PAGE_SIZE).step_by(PIECE) {
                if !frame.write(at, &[0xFF; PIECE]) {
                    return Err(Error::NoMemory);
                }
            }
            absent.frame = Some(frame);
        }
        let hpa = absent.frame.as_ref().map(|f| f.phys()).ok_or(Error::NoMemory)?;
        absent.at.try_reserve(1).map_err(|_| Error::NoMemory)?;
        {
            let mut table = self.table.lock();
            table.prepare(page, PAGE)?;
            table.set_ro(page, hpa)?;
        }
        absent.at.push(page);
        Ok(())
    }

    /// Where the guest has read an absent device, for a report: empty when
    /// there is no memory to say it in.
    pub fn absent_pages(&self) -> Vec<u64> {
        let absent = self.absent.lock();
        let mut pages = Vec::new();
        if pages.try_reserve_exact(absent.at.len()).is_ok() {
            pages.extend_from_slice(&absent.at);
        }
        pages
    }

    /// Whether `[gpa, gpa + len)` is RAM of one region, whole.
    pub fn is_ram(&self, gpa: u64, len: u64) -> bool {
        len != 0 && self.regions.iter().any(|r| r.contains(gpa, len))
    }

    /// The RAM's regions, as base and size, in the order they were given.
    pub fn ranges(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.regions.iter().map(|r| (r.base, r.size))
    }

    /// Its RAM: each region's base in the guest's space, and its pages -- what
    /// an IOMMU domain of the guest's maps a host's function's DMA to
    /// (`kcore::iommu::Domain::map`), holding a share of each.
    pub fn ram(&self) -> impl Iterator<Item = (u64, &Run)> + '_ {
        self.regions.iter().map(|r| (r.base, &r.frames))
    }

    /// How much memory the guest has, over every region.
    pub fn size(&self) -> u64 {
        self.regions.iter().map(|r| r.size).sum()
    }

    /// How much of it is the huge pool's.
    pub fn huge_size(&self) -> u64 {
        self.regions.iter().filter(|r| r.huge).map(|r| r.size).sum()
    }

    /// The region `[gpa, gpa + len)` lies whole in.
    fn region(&self, gpa: u64, len: usize) -> Result<usize> {
        let len = len as u64;
        self.regions.iter().position(|r| r.contains(gpa, len)).ok_or(Error::Unmapped)
    }

    /// Copy out `buf.len()` bytes from `gpa`.
    pub fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let region = &self.regions[self.region(gpa, buf.len())?];
        let mut done = 0usize;
        while done < buf.len() {
            let (page, off) = region.page(gpa + done as u64).ok_or(Error::Unmapped)?;
            let n = (buf.len() - done).min(PAGE_SIZE - off);
            if !page.read(off, &mut buf[done..done + n]) {
                return Err(Error::Unmapped);
            }
            done += n;
        }
        Ok(())
    }

    /// Copy `data` in at `gpa`. Through `&self`, as `read` is: a copy into
    /// the guest's memory is one more writer of it beside the guest's own
    /// CPUs, which write it with no reference of the host's in sight.
    pub fn write(&self, gpa: u64, data: &[u8]) -> Result<()> {
        let region = &self.regions[self.region(gpa, data.len())?];
        let mut done = 0usize;
        while done < data.len() {
            let (page, off) = region.page(gpa + done as u64).ok_or(Error::Unmapped)?;
            let n = (data.len() - done).min(PAGE_SIZE - off);
            if !page.write(off, &data[done..done + n]) {
                return Err(Error::Unmapped);
            }
            done += n;
        }
        /* After the bytes: a round that finds the mark finds them */
        self.log.wrote(gpa, data.len() as u64);
        Ok(())
    }

    /// The 16-bit word at `gpa`, read as one access with acquire order --
    /// whole, where `read` copies a byte at a time and another of the
    /// guest's CPUs may be storing the word meanwhile: a virtqueue's indices
    /// (`kcore::frame::FramePage::load_u16`). An address that is not even is
    /// refused, as one outside the guest's memory is.
    pub fn read_u16(&self, gpa: u64) -> Result<u16> {
        if gpa % 2 != 0 {
            return Err(Error::BadAddress);
        }
        let region = &self.regions[self.region(gpa, 2)?];
        let (page, off) = region.page(gpa).ok_or(Error::Unmapped)?;
        page.load_u16(off).ok_or(Error::Unmapped)
    }

    /// `value` stored at `gpa` as one access with release order: what was
    /// written before it is there before it, for a driver on another CPU
    /// that reads the word and then what it publishes.
    pub fn write_u16(&self, gpa: u64, value: u16) -> Result<()> {
        if gpa % 2 != 0 {
            return Err(Error::BadAddress);
        }
        let region = &self.regions[self.region(gpa, 2)?];
        let (page, off) = region.page(gpa).ok_or(Error::Unmapped)?;
        if !page.store_u16(off, value) {
            return Err(Error::Unmapped);
        }
        self.log.wrote(gpa, 2);
        Ok(())
    }

    /// The guest's pages from `gpa`, as many as `len` bytes reach,
    /// `kcore::frame::MAPPED_MAX` at most, mapped into the host's space
    /// until the mapping is dropped: what a device reads and writes at every
    /// request -- a virtqueue's rings -- without a page mapped and unmapped
    /// at every access as `read` and `write` have. `gpa` is a page's, and
    /// every page is RAM of one region; the mapping keeps its frames
    /// allocated however long it outlives this.
    pub fn map(&self, gpa: u64, len: usize) -> Result<Mapping> {
        if gpa % PAGE != 0 || len == 0 {
            return Err(Error::BadAddress);
        }
        let pages = len.div_ceil(PAGE_SIZE);
        if pages > kcore::frame::MAPPED_MAX {
            return Err(Error::BadAddress);
        }
        let region = &self.regions[self.region(gpa, pages.checked_mul(PAGE_SIZE).ok_or(Error::BadAddress)?)?];
        let first = usize::try_from((gpa - region.base) / PAGE).map_err(|_| Error::BadAddress)?;
        region.frames.map(first, pages).ok_or(Error::NoMemory)
    }

    /// The `T` at `gpa`, as it is this instant.
    pub fn read_obj<T: Pod>(&self, gpa: u64) -> Result<T> {
        let mut value = pod::zeroed::<T>();
        self.read(gpa, pod::bytes_of_mut(&mut value))?;
        Ok(value)
    }

    pub fn write_obj<T: Pod>(&self, gpa: u64, value: &T) -> Result<()> {
        self.write(gpa, pod::bytes_of(value))
    }

    /// The nested table, for the VMCB: what `vmrun` translates every guest
    /// physical address through, and what the TLB may keep of it -- its
    /// identity as of now (`log_start`).
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn nested(&self) -> hvarch::x86::svm::Nested {
        hvarch::x86::svm::Nested { root: self.root, id: self.id.load(Ordering::Acquire) }
    }

    /// The nested table's top level: the same for as long as the memory
    /// lives, whatever its identity for the TLBs.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn table_root(&self) -> u64 {
        self.root
    }

    /// The table's new identity, after the log narrowed it: each CPU takes
    /// it at its next entry, its TLB clear of what it cached through the
    /// old one (`run.rs`'s `sync_tlbs` waits for that). Under the table's
    /// lock, `table`.
    #[cfg(target_arch = "x86_64")]
    fn renew(&self, table: &mut SecondLevel) -> u64 {
        let id = table.renew();
        self.id.store(id, Ordering::Release);
        id
    }

    /// The guest's write at `gpa` faulted on its nested table: a page of
    /// its RAM the dirty log protected -- marked, and writable again, and
    /// true: the write runs again. False for anything else.
    #[cfg(target_arch = "x86_64")]
    pub fn write_fault(&self, gpa: u64) -> bool {
        let page = gpa & !(PAGE - 1);
        let msix = self.msix.lock();
        let moved = msix.iter().find(|m| m.mapped && m.at == page);
        if moved.is_none() && msix.iter().any(|m| m.backing == page && (!m.mapped || m.at != page)) {
            return false;
        }
        let gpa = moved.map_or(gpa, |m| m.backing + (gpa - page));
        if self.region(gpa & !(PAGE - 1), PAGE_SIZE).is_err() {
            return false;
        }
        let mut table = self.table.lock();
        self.log.fault(&mut DirtyTable { table: &mut table, msix: &msix }, gpa)
    }

    /// The dirty log begun: every page of RAM read-only to the guest, the
    /// table given a new identity -- its id, which every CPU is to take
    /// before the pages are read (`run.rs`'s `sync_tlbs`). A page that could
    /// not be protected -- no memory to split a 2 MiB leaf -- refuses it,
    /// and what was protected is let go of.
    #[cfg(target_arch = "x86_64")]
    pub fn log_start(&self) -> Result<u64> {
        let msix = self.msix.lock();
        let mut table = self.table.lock();
        let mut dirty = DirtyTable { table: &mut table, msix: &msix };
        if !self.log.start(&mut dirty) {
            self.log.release(&mut dirty);
            drop(dirty);
            self.renew(&mut table);
            return Err(Error::NoMemory);
        }
        drop(dirty);
        Ok(self.renew(&mut table))
    }

    /// A round of the dirty log: the pages written since the last, each to
    /// `out`, read-only to the guest again -- the table given a new identity,
    /// its id for `sync_tlbs` -- and how many. A page that could not be
    /// protected again refuses the round.
    #[cfg(target_arch = "x86_64")]
    pub fn log_take(&self, out: &mut dyn FnMut(u64)) -> Result<(u64, u64)> {
        let msix = self.msix.lock();
        let mut table = self.table.lock();
        let n = self.log.take(Some(&mut DirtyTable { table: &mut table, msix: &msix }), out);
        let id = self.renew(&mut table);
        n.map(|n| (n, id)).ok_or(Error::NoMemory)
    }

    /// The last round, the guest paused: the pages written since the round
    /// before, each to `out`, nothing protected. How many.
    pub fn log_last(&self, out: &mut dyn FnMut(u64)) -> u64 {
        self.log.take(None, out).unwrap_or(0)
    }

    /// How many pages are marked now: what a round would send.
    pub fn log_marked(&self) -> u64 {
        self.log.marked()
    }

    /// The dirty log over: every page writable to the guest again. No new
    /// identity: nothing a TLB keeps is wider than the table now.
    #[cfg(target_arch = "x86_64")]
    pub fn log_stop(&self) {
        let msix = self.msix.lock();
        let mut table = self.table.lock();
        self.log.release(&mut DirtyTable { table: &mut table, msix: &msix });
    }
}
