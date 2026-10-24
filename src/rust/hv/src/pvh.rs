//! Loading a PVH image: an ELF the guest is started in at a 32-bit entry,
//! with paging off, handed a description of the machine -- the x86/HVM
//! direct boot ABI Xen made (its `docs/misc/pvh.pandoc`), which Linux's
//! `vmlinux`, QEMU's `-kernel`, Cloud Hypervisor and the firmware written
//! for them (rust-hypervisor-firmware, OVMF's `CloudHv`) all speak.
//!
//! What it asks of a loader, and this does: put the ELF's loadable segments
//! at their physical addresses; find the entry in the image's `Xen` note of
//! type `XEN_ELFNOTE_PHYS32_ENTRY`; lay out an `hvm_start_info` -- the
//! command line, the modules (the initrd, when there is one), the ACPI
//! RSDP, the memory map -- and start the CPU at the entry in 32-bit
//! protected mode, flat segments, paging and interrupts off, `EBX` at the
//! structure. Everything past that -- page tables, long mode, a GDT and an
//! IDT of its own -- the image does itself.
//!
//! Beside a `bzImage` (`crate::linux`) it is the way in for what is not
//! Linux's own image: `hvboot`, the loader that boots a guest's kernel out
//! of its own disk, is one.
//!
//! The image is a file of nos's, read through [`ReadAt`] and never trusted:
//! every header field is checked before it is used, a segment has to fit
//! the file and the guest's RAM above the tables written below 1 MiB, and
//! the entry has to be in one. As in `crate::linux`, what is written into
//! guest memory goes through `GuestMemory`'s copying accessors.

use hvarch::{Error, Result};

use crate::acpi::Route;
use crate::linux::{self, Firmware};
use crate::memory::GuestMemory;
use crate::svm::ProtectedMode;
use crate::vm::Backend;

/* The ELF header (the System V ABI's gABI, and its x86 supplements): the
 * identification bytes, then the fields this reads, at their offsets in the
 * 32- and the 64-bit layout. */
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const EI_VERSION: usize = 6;
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const EM_386: u16 = 3;
const EM_X86_64: u16 = 62;
const E_MACHINE: usize = 18;
/// The header's size: the most of it this reads.
const EHDR32_BYTES: usize = 52;
const EHDR64_BYTES: usize = 64;
/* e_phoff, e_phentsize and e_phnum, by class. */
const E_PHOFF32: usize = 28;
const E_PHENTSIZE32: usize = 42;
const E_PHNUM32: usize = 44;
const E_PHOFF64: usize = 32;
const E_PHENTSIZE64: usize = 54;
const E_PHNUM64: usize = 56;
/// A program header's size, by class.
const PHDR32_BYTES: usize = 32;
const PHDR64_BYTES: usize = 56;
const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;

/// The note that names the 32-bit entry: owner "Xen", type 18, its
/// descriptor the entry's physical address -- 4 bytes, as Linux writes it,
/// or 8.
const XEN_NOTE_NAME: &[u8] = b"Xen\0";
const XEN_ELFNOTE_PHYS32_ENTRY: u32 = 18;

/// The most program headers this reads: a kernel has a handful, and a
/// header that claims thousands is a file that is not one.
const MAX_PHDRS: usize = 64;
/// The most loadable segments an image may have.
pub const MAX_SEGMENTS: usize = 16;
/// The most of a note segment that is read looking for the entry.
const NOTES_MAX: usize = 4096;

/* hvm_start_info, version 1 (Xen's public/arch-x86/hvm/start_info.h):
 * magic, version, flags, nr_modules, modlist_paddr, cmdline_paddr,
 * rsdp_paddr, memmap_paddr, memmap_entries, reserved -- 56 bytes -- then
 * each module 32 bytes (paddr, size, cmdline_paddr, reserved) and each
 * memory map entry 24 (addr, size, type, reserved). */
const START_INFO_MAGIC: u32 = 0x336E_C578;
const START_INFO_VERSION: u32 = 1;
const START_INFO_BYTES: usize = 56;
const SI_MAGIC: usize = 0;
const SI_VERSION: usize = 4;
const SI_NR_MODULES: usize = 12;
const SI_MODLIST: usize = 16;
const SI_CMDLINE: usize = 24;
const SI_RSDP: usize = 32;
const SI_MEMMAP: usize = 40;
const SI_MEMMAP_ENTRIES: usize = 48;
const MODULE_BYTES: usize = 32;
const MEMMAP_ENTRY_BYTES: usize = 24;

/* Where the structures go: low RAM, as for a bzImage -- the start info
 * where a zero page would be, the module list and the memory map after it,
 * and the command line where a bzImage's is. All of it is RAM the memory
 * map gives the guest, which it may take back once it has read them. */
pub const START_INFO: u64 = 0x7000;
const CMDLINE: u64 = 0x2_0000;
/// The command line's ceiling, as a bzImage's.
const CMDLINE_MAX: usize = 2048;

/// The image's program headers are read through this: the file, at an
/// offset. A read past the end returns fewer bytes, as a file does.
pub trait ReadAt {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;
}

/// Read exactly `buf.len()` bytes at `offset`, or fail: a short read is a
/// file that is not what its header says.
fn read_exact(file: &mut dyn ReadAt, offset: u64, buf: &mut [u8]) -> Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let at = offset.checked_add(done as u64).ok_or(Error::BadAddress)?;
        let got = file.read_at(at, &mut buf[done..])?;
        if got == 0 || got > buf.len() - done {
            return Err(Error::BadAddress);
        }
        done += got;
    }
    Ok(())
}

/// A loadable segment: `filesz` bytes of the file from `offset`, at the
/// physical address `paddr`, then zeroes to `memsz`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Segment {
    pub offset: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

impl Segment {
    fn end(&self) -> Option<u64> {
        self.paddr.checked_add(self.memsz)
    }
}

/// A PVH image, as its headers describe it.
#[derive(Clone, Copy, Debug)]
pub struct Image {
    /// The 32-bit entry, a physical address.
    pub entry: u32,
    segments: [Segment; MAX_SEGMENTS],
    count: usize,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}

impl Image {
    /// Whether the file's first bytes are an ELF's: the module tells a PVH
    /// image from a bzImage by them.
    pub fn is_elf(first: &[u8]) -> bool {
        first.len() >= ELF_MAGIC.len() && first[..ELF_MAGIC.len()] == ELF_MAGIC
    }

    /// Read the image's headers out of `file`, `file_len` bytes long. An
    /// ELF with no PVH entry note is `NotImplemented`: an ELF, but not one
    /// this boots; anything malformed is `BadAddress`.
    pub fn parse(file: &mut dyn ReadAt, file_len: u64) -> Result<Image> {
        let mut ehdr = [0u8; EHDR64_BYTES];
        let short = file_len < EHDR32_BYTES as u64;
        if short {
            return Err(Error::BadAddress);
        }
        let want = (file_len.min(EHDR64_BYTES as u64)) as usize;
        read_exact(file, 0, &mut ehdr[..want])?;
        if ehdr[..4] != ELF_MAGIC || ehdr[EI_DATA] != ELFDATA2LSB || ehdr[EI_VERSION] != EV_CURRENT {
            return Err(Error::BadAddress);
        }
        let wide = match ehdr[EI_CLASS] {
            ELFCLASS32 => false,
            ELFCLASS64 => true,
            _ => return Err(Error::BadAddress),
        };
        if wide && want < EHDR64_BYTES {
            return Err(Error::BadAddress);
        }
        if !matches!(u16_at(&ehdr, E_MACHINE), EM_386 | EM_X86_64) {
            return Err(Error::BadAddress);
        }
        let (phoff, phentsize, phnum) = if wide {
            (u64_at(&ehdr, E_PHOFF64), u16_at(&ehdr, E_PHENTSIZE64) as usize, u16_at(&ehdr, E_PHNUM64) as usize)
        } else {
            (u32_at(&ehdr, E_PHOFF32) as u64, u16_at(&ehdr, E_PHENTSIZE32) as usize, u16_at(&ehdr, E_PHNUM32) as usize)
        };
        let phdr_bytes = if wide { PHDR64_BYTES } else { PHDR32_BYTES };
        if phnum == 0 || phnum > MAX_PHDRS || phentsize < phdr_bytes {
            return Err(Error::BadAddress);
        }

        let mut image = Image { entry: 0, segments: [Segment::default(); MAX_SEGMENTS], count: 0 };
        let mut entry: Option<u32> = None;
        let mut ph = [0u8; PHDR64_BYTES];
        for i in 0..phnum {
            let at = (i as u64)
                .checked_mul(phentsize as u64)
                .and_then(|o| o.checked_add(phoff))
                .ok_or(Error::BadAddress)?;
            if at.checked_add(phdr_bytes as u64).is_none_or(|end| end > file_len) {
                return Err(Error::BadAddress);
            }
            read_exact(file, at, &mut ph[..phdr_bytes])?;
            let (kind, offset, paddr, filesz, memsz) = if wide {
                (u32_at(&ph, 0), u64_at(&ph, 8), u64_at(&ph, 24), u64_at(&ph, 32), u64_at(&ph, 40))
            } else {
                (u32_at(&ph, 0), u32_at(&ph, 4) as u64, u32_at(&ph, 12) as u64, u32_at(&ph, 16) as u64,
                 u32_at(&ph, 20) as u64)
            };
            if filesz > memsz || offset.checked_add(filesz).is_none_or(|end| end > file_len) {
                return Err(Error::BadAddress);
            }
            match kind {
                PT_LOAD if memsz != 0 => {
                    let slot = image.segments.get_mut(image.count).ok_or(Error::BadAddress)?;
                    *slot = Segment { offset, paddr, filesz, memsz };
                    if slot.end().is_none() {
                        return Err(Error::BadAddress);
                    }
                    image.count += 1;
                }
                PT_NOTE => {
                    if let Some(e) = find_entry(file, offset, filesz)? {
                        if entry.is_some_and(|had| had != e) {
                            /* Two entries: no telling which is meant. */
                            return Err(Error::BadAddress);
                        }
                        entry = Some(e);
                    }
                }
                _ => {}
            }
        }

        let Some(entry) = entry else {
            return Err(Error::NotImplemented);
        };
        let segments = &image.segments[..image.count];
        if segments.is_empty() {
            return Err(Error::BadAddress);
        }
        /* Apart: two segments over the same bytes would each overwrite the
         * other, and which won would depend on the order they are read in. */
        for (i, a) in segments.iter().enumerate() {
            for b in &segments[i + 1..] {
                let (Some(a_end), Some(b_end)) = (a.end(), b.end()) else {
                    return Err(Error::BadAddress);
                };
                if a.paddr < b_end && b.paddr < a_end {
                    return Err(Error::BadAddress);
                }
            }
        }
        /* The entry is an instruction the file holds: in a segment's bytes,
         * not in the zeroes after them, nor nowhere at all. */
        let e = u64::from(entry);
        if !segments.iter().any(|s| e >= s.paddr && e - s.paddr < s.filesz) {
            return Err(Error::BadAddress);
        }
        image.entry = entry;
        Ok(image)
    }

    /// Its loadable segments, as the module streams them in.
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }

    /// Where the last of its segments ends.
    fn end(&self) -> u64 {
        self.segments().iter().filter_map(Segment::end).max().unwrap_or(0)
    }
}

/// The PVH entry in the notes of the segment at `offset`, `len` bytes: the
/// descriptor of the `Xen` note of type 18, when there is one. A note is a
/// 12-byte header -- name size, descriptor size, type -- then the name and
/// the descriptor, each padded to 4 bytes.
fn find_entry(file: &mut dyn ReadAt, offset: u64, len: u64) -> Result<Option<u32>> {
    let len = len.min(NOTES_MAX as u64) as usize;
    let mut notes = [0u8; NOTES_MAX];
    read_exact(file, offset, &mut notes[..len])?;
    let notes = &notes[..len];
    let pad = |n: usize| n.checked_add(3).map(|v| v & !3);
    let mut at = 0usize;
    while at + 12 <= notes.len() {
        let namesz = u32_at(notes, at) as usize;
        let descsz = u32_at(notes, at + 4) as usize;
        let kind = u32_at(notes, at + 8);
        let name_at = at + 12;
        let (Some(name_len), Some(desc_len)) = (pad(namesz), pad(descsz)) else {
            return Err(Error::BadAddress);
        };
        let desc_at = name_at.checked_add(name_len).ok_or(Error::BadAddress)?;
        let next = desc_at.checked_add(desc_len).ok_or(Error::BadAddress)?;
        if next > notes.len() || desc_at + descsz > notes.len() {
            /* A note cut off by the segment, or by how much of it is read:
             * none past it can be read either. */
            return Ok(None);
        }
        if kind == XEN_ELFNOTE_PHYS32_ENTRY && notes[name_at..name_at + namesz] == *XEN_NOTE_NAME {
            let desc = &notes[desc_at..desc_at + descsz];
            return match descsz {
                4 => Ok(Some(u32_at(desc, 0))),
                8 => u32::try_from(u64_at(desc, 0)).map(Some).map_err(|_| Error::BadAddress),
                _ => Err(Error::BadAddress),
            };
        }
        at = next;
    }
    Ok(None)
}

/// Where everything ends up in the guest's memory: the image's segments
/// where its headers put them, and the initrd -- module 0 -- above them.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub mem_bytes: u64,
    /// Where the initrd goes, and how long it is; 0 when there is none.
    pub initrd_addr: u64,
    pub initrd_len: u64,
}

/// Check the image fits a guest of `mem_bytes`, and place an initrd of
/// `initrd_len` bytes: every segment in RAM above the tables below 1 MiB,
/// the initrd as high as it goes, page-aligned and clear of the image.
pub fn plan(image: &Image, mem_bytes: u64, initrd_len: u64) -> Result<Layout> {
    for s in image.segments() {
        /* Below 1 MiB are the start info, the command line, the MP table
         * and the ACPI tables this writes; past the end, nothing at all. */
        if s.paddr < linux::ONE_MIB || s.end().is_none_or(|end| end > mem_bytes) {
            return Err(Error::BadAddress);
        }
    }
    let (initrd_addr, initrd_len) = if initrd_len == 0 {
        (0, 0)
    } else {
        let addr = mem_bytes.checked_sub(initrd_len).ok_or(Error::NoMemory)? & !0xFFF;
        if addr < round_up(image.end(), 0x1000).ok_or(Error::BadAddress)? {
            return Err(Error::NoMemory);
        }
        (addr, initrd_len)
    };
    Ok(Layout { mem_bytes, initrd_addr, initrd_len })
}

fn round_up(v: u64, to: u64) -> Option<u64> {
    v.checked_add(to - 1).map(|r| r & !(to - 1))
}

/// Write what the image is handed into the guest's memory: the command
/// line, the start info, its module list and memory map -- and the MP
/// table, for a guest of more than one CPU, as `crate::linux::build` writes
/// it for a bzImage. The segments and the initrd are the module's to
/// stream in, the ACPI tables the caller's, where `firmware` says.
#[allow(clippy::too_many_arguments)]
pub fn build(
    memory: &GuestMemory,
    layout: &Layout,
    cmdline: &[u8],
    cpus: u32,
    firmware: Option<Firmware>,
    pci: &[Route],
    ioapic: bool,
) -> Result<()> {
    if cmdline.len() >= CMDLINE_MAX {
        return Err(Error::BadAddress);
    }
    memory.write(CMDLINE, cmdline)?;
    memory.write(CMDLINE + cmdline.len() as u64, &[0])?;

    let modules = usize::from(layout.initrd_len != 0);
    let modlist = START_INFO + START_INFO_BYTES as u64;
    let memmap = modlist + (modules * MODULE_BYTES) as u64;

    if modules != 0 {
        let mut m = [0u8; MODULE_BYTES];
        m[0..8].copy_from_slice(&layout.initrd_addr.to_le_bytes());
        m[8..16].copy_from_slice(&layout.initrd_len.to_le_bytes());
        memory.write(modlist, &m)?;
    }

    let mut map = [linux::E820Entry::default(); linux::E820_LOW_ENTRIES];
    let n = linux::e820_entries(layout.mem_bytes, firmware, &mut map)?;
    for (i, e) in map[..n].iter().enumerate() {
        let mut entry = [0u8; MEMMAP_ENTRY_BYTES];
        entry[0..8].copy_from_slice(&e.addr.to_le_bytes());
        entry[8..16].copy_from_slice(&e.size.to_le_bytes());
        entry[16..20].copy_from_slice(&e.kind.to_le_bytes());
        memory.write(memmap + (i * MEMMAP_ENTRY_BYTES) as u64, &entry)?;
    }
    const _: () = assert!(START_INFO as usize + START_INFO_BYTES + MODULE_BYTES
        + linux::E820_LOW_ENTRIES * MEMMAP_ENTRY_BYTES <= CMDLINE as usize);

    let mut si = [0u8; START_INFO_BYTES];
    si[SI_MAGIC..SI_MAGIC + 4].copy_from_slice(&START_INFO_MAGIC.to_le_bytes());
    si[SI_VERSION..SI_VERSION + 4].copy_from_slice(&START_INFO_VERSION.to_le_bytes());
    si[SI_NR_MODULES..SI_NR_MODULES + 4].copy_from_slice(&(modules as u32).to_le_bytes());
    if modules != 0 {
        si[SI_MODLIST..SI_MODLIST + 8].copy_from_slice(&modlist.to_le_bytes());
    }
    si[SI_CMDLINE..SI_CMDLINE + 8].copy_from_slice(&CMDLINE.to_le_bytes());
    if let Some(f) = firmware {
        si[SI_RSDP..SI_RSDP + 8].copy_from_slice(&f.rsdp.to_le_bytes());
    }
    si[SI_MEMMAP..SI_MEMMAP + 8].copy_from_slice(&memmap.to_le_bytes());
    si[SI_MEMMAP_ENTRIES..SI_MEMMAP_ENTRIES + 4].copy_from_slice(&(n as u32).to_le_bytes());
    memory.write(START_INFO, &si)?;

    /* A guest of one CPU has no APIC, and so no table of its CPUs. */
    if cpus > 1 {
        linux::write_mp_table(memory, cpus, pci, ioapic)?;
    }
    Ok(())
}

/// Put the vCPU where the PVH entry wants it: 32-bit protected mode with
/// flat segments, paging and interrupts off, at the image's entry, `EBX` at
/// the start info.
pub fn set_entry(vcpu: &mut Backend, image: &Image) {
    vcpu.protected_mode(&ProtectedMode { entry: image.entry });
    vcpu.regs_mut().rbx = START_INFO;
}
