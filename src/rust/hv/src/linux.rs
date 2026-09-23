//! Loading a Linux `bzImage` by the 64-bit boot protocol
//! (`Documentation/arch/x86/boot.rst`): the path a modern hypervisor takes,
//! which skips real mode and the BIOS entirely.
//!
//! What the protocol asks of a loader, and this does: read the setup header
//! at offset `0x1f1` of the image; copy it into a `boot_params` (the "zero
//! page"); fill the fields a loader owns -- the command line, the initrd,
//! the memory map; load the 64-bit half of the kernel (everything past the
//! real-mode setup) at the address it wants; and hand the CPU to it in long
//! mode with `RSI` pointing at the zero page. The kernel's own decompressor
//! takes it from there.
//!
//! Everything the guest is given is laid out in *its* physical memory by
//! [`build`], through [`GuestMemory`]'s copying accessors, and the guest's
//! own page tables and GDT are built there too -- this loader never holds a
//! reference into guest memory, and what it writes the guest could have
//! written itself.

use hvarch::{Error, Result};

use crate::memory::GuestMemory;
use crate::svm::{LongMode, Vcpu};

/* Offsets into the setup header / boot_params (they share a layout from
 * 0x1f1 on). From boot.rst and arch/x86/include/uapi/asm/bootparam.h. */
const HDR_SETUP_SECTS: usize = 0x1f1;
const HDR_BOOT_FLAG: usize = 0x1fe;
const HDR_HEADER_MAGIC: usize = 0x202;
const HDR_VERSION: usize = 0x206;
const HDR_TYPE_OF_LOADER: usize = 0x210;
const HDR_LOADFLAGS: usize = 0x211;
const HDR_RAMDISK_IMAGE: usize = 0x218;
const HDR_RAMDISK_SIZE: usize = 0x21c;
const HDR_HEAP_END_PTR: usize = 0x224;
const HDR_CMD_LINE_PTR: usize = 0x228;
const HDR_INITRD_ADDR_MAX: usize = 0x22c;
const HDR_KERNEL_ALIGNMENT: usize = 0x230;
const HDR_RELOCATABLE: usize = 0x234;
const HDR_XLOADFLAGS: usize = 0x236;
const HDR_CMDLINE_SIZE: usize = 0x238;
const HDR_PREF_ADDRESS: usize = 0x258;
const HDR_INIT_SIZE: usize = 0x260;
/// The last byte the header end is measured from.
const HDR_LEN_AT: usize = 0x201;

/// `boot_flag`, the 0xAA55 that says this is a kernel image at all.
const BOOT_FLAG: u16 = 0xAA55;
/// `header`, "HdrS": what says the boot protocol is 2.00 or newer.
const HEADER_MAGIC: u32 = 0x5372_6448;
/// The oldest protocol with a 64-bit entry and a relocatable kernel.
const MIN_VERSION: u16 = 0x0205;
/// `xloadflags` bit 0: the kernel has a 64-bit entry point.
const XLF_KERNEL_64: u16 = 1 << 0;
/// `loadflags` bit 0: the protected-mode kernel is loaded high (at 1 MiB or
/// above), which the 64-bit protocol always is.
const LOADFLAGS_LOADED_HIGH: u8 = 1 << 0;
/// `type_of_loader`: an undefined boot loader, which is what this is as far
/// as the kernel needs to know.
const LOADER_UNDEFINED: u8 = 0xFF;

/// The 64-bit entry point is the loaded kernel's start plus this.
const ENTRY64_OFFSET: u64 = 0x200;

/* The guest's fixed furniture, all in the first 640 KiB -- identity-mapped,
 * and clear of the kernel at pref_address (16 MiB) and of the initrd, which
 * go higher. */
const GDT: u64 = 0x0500;
const TSS: u64 = 0x1000;
const BOOT_PARAMS: u64 = 0x7000;
const PML4: u64 = 0x9000;
const PDPT: u64 = 0xA000;
const PD_BASE: u64 = 0xB000;
const CMDLINE: u64 = 0x20000;
/// The guest's stack top before the kernel sets its own -- in low RAM,
/// below the zero page.
const STACK: u64 = 0x6000;

/// The command line's ceiling, whatever the header allows: it and the zero
/// page and the tables all live in the first 640 KiB.
const CMDLINE_MAX: usize = 2048;

/* boot_params: e820 map. */
const BP_E820_ENTRIES: usize = 0x1e8;
const BP_E820_TABLE: usize = 0x2d0;
const E820_ENTRY_BYTES: usize = 20;
const E820_MAX_ENTRIES: usize = 128;
const E820_RAM: u32 = 1;

/* Guest page-table entry bits. */
const PTE_PRESENT: u64 = 1 << 0;
const PTE_WRITE: u64 = 1 << 1;
const PTE_PAGE_SIZE: u64 = 1 << 7;
const PAGE_2MIB: u64 = 2 * 1024 * 1024;
/// How many GiB of guest physical addresses the identity map covers: four
/// page-directory pages of 2 MiB entries, which is every address a guest of
/// up to this much RAM has. A page-table page is 512 entries, so one covers
/// 1 GiB.
const IDENTITY_GIB: u64 = 4;

/* GDT selectors the 64-bit protocol names: __BOOT_CS and __BOOT_DS. */
const BOOT_CS: u16 = 0x10;
const BOOT_DS: u16 = 0x18;
const BOOT_TR: u16 = 0x20;

/// A 64-bit flat code descriptor, and a flat data one, as raw GDT words.
const GDT_CODE64: u64 = 0x00AF_9B00_0000_FFFF;
const GDT_DATA: u64 = 0x00CF_9300_0000_FFFF;
const TSS_LIMIT: u64 = 0x67;
const TSS_BUSY_PRESENT: u64 = 0x8B;

/// The setup header, as much of it as a loader reads.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    /// Sectors of real-mode setup; the 64-bit kernel begins after them.
    pub setup_sects: u8,
    pub version: u16,
    pub pref_address: u64,
    pub init_size: u64,
    pub relocatable: bool,
    pub kernel_alignment: u64,
    pub initrd_addr_max: u64,
    /// End of the header within the image, `0x202 + [0x201]`: how much is
    /// copied into the zero page.
    pub hdr_end: usize,
    pub cmdline_size: u32,
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

impl Header {
    /// Parse the header out of the first bytes of the image. `first` has to
    /// reach at least the end of the header (`0x268 + 8`); the caller reads
    /// a page or two, which always does.
    pub fn parse(first: &[u8]) -> Result<Header> {
        if first.len() < 0x270 {
            return Err(Error::BadAddress);
        }
        if u16_at(first, HDR_BOOT_FLAG) != BOOT_FLAG {
            return Err(Error::BadAddress);
        }
        if u32_at(first, HDR_HEADER_MAGIC) != HEADER_MAGIC {
            return Err(Error::BadAddress);
        }
        let version = u16_at(first, HDR_VERSION);
        if version < MIN_VERSION {
            return Err(Error::NotImplemented);
        }
        if u16_at(first, HDR_XLOADFLAGS) & XLF_KERNEL_64 == 0 {
            /* No 64-bit entry point: this loader only takes the 64-bit
             * protocol, which every x86-64 kernel of the last decade has. */
            return Err(Error::NotImplemented);
        }

        /* setup_sects of 0 means 4, for old images. */
        let setup_sects = match first[HDR_SETUP_SECTS] {
            0 => 4,
            n => n,
        };
        let hdr_end = HDR_HEADER_MAGIC + first[HDR_LEN_AT] as usize;

        Ok(Header {
            setup_sects,
            version,
            pref_address: u64_at(first, HDR_PREF_ADDRESS),
            init_size: u32_at(first, HDR_INIT_SIZE) as u64,
            relocatable: first[HDR_RELOCATABLE] != 0,
            kernel_alignment: u32_at(first, HDR_KERNEL_ALIGNMENT) as u64,
            initrd_addr_max: u32_at(first, HDR_INITRD_ADDR_MAX) as u64,
            hdr_end,
            cmdline_size: u32_at(first, HDR_CMDLINE_SIZE),
        })
    }

    /// The offset in the file where the 64-bit kernel begins.
    pub fn pm_offset(&self) -> u64 {
        (self.setup_sects as u64 + 1) * 512
    }
}

/// Where everything a Linux guest needs ends up in its physical memory, once
/// the header and the sizes are known. The module streams the kernel and the
/// initrd into `kernel_addr` and `initrd_addr`; [`build`] writes the rest.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub mem_bytes: u64,
    /// Where the 64-bit kernel is loaded: `pref_address`, since a kernel
    /// that is not relocatable must run there, and one that is may as well.
    pub kernel_addr: u64,
    /// How many bytes of the file are the 64-bit kernel.
    pub kernel_len: u64,
    /// Where the initrd goes, and how long it is; 0 when there is none.
    pub initrd_addr: u64,
    pub initrd_len: u64,
}

/// Decide the layout for a guest with `mem_bytes` of RAM, a kernel whose
/// 64-bit half is `kernel_len` bytes, and an initrd of `initrd_len`.
pub fn plan(header: &Header, mem_bytes: u64, kernel_len: u64, initrd_len: u64) -> Result<Layout> {
    if mem_bytes < 64 * 1024 * 1024 {
        /* Below this a kernel and an initrd do not fit with room to
         * decompress -- init_size alone is megabytes. */
        return Err(Error::BadAddress);
    }
    let kernel_addr = header.pref_address;
    /* Everything the kernel needs while it starts, from where it is loaded:
     * past this is free for the initrd. */
    let after_kernel = kernel_addr
        .checked_add(header.init_size.max(kernel_len))
        .ok_or(Error::BadAddress)?;
    if after_kernel >= mem_bytes {
        return Err(Error::NoMemory);
    }

    let (initrd_addr, initrd_len) = if initrd_len == 0 {
        (0, 0)
    } else {
        /* As high as it will go: below the top of RAM and below the
         * header's initrd ceiling, page-aligned, and clear of the kernel. */
        let ceiling = mem_bytes.min(header.initrd_addr_max.saturating_add(1));
        let addr = ceiling
            .checked_sub(initrd_len)
            .ok_or(Error::NoMemory)?
            & !0xFFF;
        if addr < round_up(after_kernel, 0x1000) {
            return Err(Error::NoMemory);
        }
        (addr, initrd_len)
    };

    Ok(Layout { mem_bytes, kernel_addr, kernel_len, initrd_addr, initrd_len })
}

fn round_up(v: u64, to: u64) -> u64 {
    (v + to - 1) & !(to - 1)
}

/// Write the guest's furniture into its memory: the zero page from the
/// header, the command line, the memory map, the identity page tables and
/// the GDT. The kernel and the initrd are the module's to stream in, at the
/// addresses `layout` names; everything here is small and goes in one call.
pub fn build(
    memory: &mut GuestMemory,
    header: &Header,
    first: &[u8],
    layout: &Layout,
    cmdline: &[u8],
) -> Result<()> {
    if cmdline.len() >= CMDLINE_MAX || cmdline.len() as u32 >= header.cmdline_size {
        return Err(Error::BadAddress);
    }

    /* The zero page: a clean page, then the header copied in where it sits,
     * then the fields a loader owns. */
    memory.write(BOOT_PARAMS, &[0u8; 8])?; // touch it, and fail early if unmapped
    let hdr = &first[HDR_SETUP_SECTS..header.hdr_end.min(first.len())];
    memory.write(BOOT_PARAMS + HDR_SETUP_SECTS as u64, hdr)?;

    put8(memory, BOOT_PARAMS + HDR_TYPE_OF_LOADER as u64, LOADER_UNDEFINED)?;
    let loadflags = first[HDR_LOADFLAGS] | LOADFLAGS_LOADED_HIGH;
    put8(memory, BOOT_PARAMS + HDR_LOADFLAGS as u64, loadflags)?;
    put32(memory, BOOT_PARAMS + HDR_CMD_LINE_PTR as u64, CMDLINE as u32)?;
    put32(memory, BOOT_PARAMS + HDR_HEAP_END_PTR as u64, 0)?;
    put32(memory, BOOT_PARAMS + HDR_RAMDISK_IMAGE as u64, layout.initrd_addr as u32)?;
    put32(memory, BOOT_PARAMS + HDR_RAMDISK_SIZE as u64, layout.initrd_len as u32)?;
    /* The command line, NUL-terminated. */
    memory.write(CMDLINE, cmdline)?;
    put8(memory, CMDLINE + cmdline.len() as u64, 0)?;

    write_e820(memory, layout.mem_bytes)?;
    write_page_tables(memory)?;
    write_gdt(memory)?;
    Ok(())
}

fn put8(m: &mut GuestMemory, gpa: u64, v: u8) -> Result<()> {
    m.write(gpa, &[v])
}
fn put32(m: &mut GuestMemory, gpa: u64, v: u32) -> Result<()> {
    m.write(gpa, &v.to_le_bytes())
}
fn put64(m: &mut GuestMemory, gpa: u64, v: u64) -> Result<()> {
    m.write(gpa, &v.to_le_bytes())
}

/// The memory map the kernel reads instead of asking a BIOS: low RAM, the
/// hole at 640 KiB, and the rest of RAM from 1 MiB up.
fn write_e820(m: &mut GuestMemory, mem_bytes: u64) -> Result<()> {
    const LOW_TOP: u64 = 0x9_FC00; // 639 KiB, the usual top of low RAM
    const ONE_MIB: u64 = 0x10_0000;

    let mut entries = [(0u64, 0u64, 0u32); 2];
    let mut n = 0;
    entries[n] = (0, LOW_TOP, E820_RAM);
    n += 1;
    if mem_bytes > ONE_MIB {
        entries[n] = (ONE_MIB, mem_bytes - ONE_MIB, E820_RAM);
        n += 1;
    }
    if n > E820_MAX_ENTRIES {
        return Err(Error::BadAddress);
    }

    put8(m, BOOT_PARAMS + BP_E820_ENTRIES as u64, n as u8)?;
    for (i, (addr, size, kind)) in entries[..n].iter().enumerate() {
        let at = BOOT_PARAMS + BP_E820_TABLE as u64 + (i * E820_ENTRY_BYTES) as u64;
        put64(m, at, *addr)?;
        put64(m, at + 8, *size)?;
        put32(m, at + 16, *kind)?;
    }
    Ok(())
}

/// Identity page tables covering the low `IDENTITY_GIB` GiB with 2 MiB
/// pages: every guest physical address a guest of a few GiB has, mapped to
/// itself, which is what the 64-bit entry needs of the zero page, the
/// command line and the kernel's init range.
fn write_page_tables(m: &mut GuestMemory) -> Result<()> {
    put64(m, PML4, PDPT | PTE_PRESENT | PTE_WRITE)?;
    for gib in 0..IDENTITY_GIB {
        let pd = PD_BASE + gib * 0x1000;
        put64(m, PDPT + gib * 8, pd | PTE_PRESENT | PTE_WRITE)?;
        for i in 0..512u64 {
            let phys = gib * (1u64 << 30) + i * PAGE_2MIB;
            put64(m, pd + i * 8, phys | PTE_PRESENT | PTE_WRITE | PTE_PAGE_SIZE)?;
        }
    }
    Ok(())
}

/// The GDT the entry names: null, then __BOOT_CS, __BOOT_DS and a 64-bit
/// TSS, at the selectors the protocol fixes.
fn write_gdt(m: &mut GuestMemory) -> Result<()> {
    put64(m, GDT + (BOOT_CS as u64 / 8) * 8, GDT_CODE64)?;
    put64(m, GDT + (BOOT_DS as u64 / 8) * 8, GDT_DATA)?;
    let ti = BOOT_TR as u64 / 8;
    let tss_low = TSS_LIMIT | (TSS & 0xFF_FFFF) << 16 | TSS_BUSY_PRESENT << 40 | (TSS >> 24 & 0xFF) << 56;
    put64(m, GDT + ti * 8, tss_low)?;
    put64(m, GDT + (ti + 1) * 8, TSS >> 32)?;
    Ok(())
}

/// Put the vCPU in the state the 64-bit entry expects: long mode at CPL 0,
/// the flat __BOOT_CS/__BOOT_DS segments, the guest's own page table and
/// GDT, `RIP` at the kernel's entry and `RSI` at the zero page.
pub fn set_entry(vcpu: &mut Vcpu, layout: &Layout) {
    vcpu.long_mode(&LongMode {
        entry: layout.kernel_addr + ENTRY64_OFFSET,
        stack: STACK,
        cr3: PML4,
        gdt: GDT,
        gdt_limit: (8 * 6 - 1) as u16,
        idt_limit: 0,
        code_selector: BOOT_CS,
        data_selector: BOOT_DS,
        tss_selector: BOOT_TR,
        tss: TSS,
    });
    /* The 64-bit protocol: %rsi holds the zero page's address. */
    vcpu.regs_mut().rsi = BOOT_PARAMS;
}
