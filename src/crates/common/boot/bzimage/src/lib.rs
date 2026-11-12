//! A Linux bzImage as a loader reads it, by the 64-bit boot protocol
//! (`Documentation/arch/x86/boot.rst`, and `arch/x86/include/uapi/asm/
//! bootparam.h` for the zero page): the setup header at offset 0x1f1, read
//! and held to what that protocol needs ([`Header`]), and the zero page --
//! `boot_params` -- a loader hands the kernel: the header copied in where it
//! sits, and the fields a loader owns filled -- the command line, the
//! initrd, the ACPI RSDP, the memory map ([`zero_page`]).
//!
//! No kernel in it and nothing unsafe: hv loads a guest's kernel by it
//! (`hypervisor/hv/src/linux.rs`), hvboot -- which boots the kernel a
//! guest's own GRUB would -- loads one by it too, and hv-fuzz and
//! hvboot-test link the same copy. It was two once, the same offsets and
//! checks in each, and a defect of one was the other's as well.
//!
//! An image is untrusted: every field is read out of the bytes the header
//! is parsed from, held long enough for all of them first, and what the
//! header says of its own end is bounded by those bytes and the page.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/* Offsets into the setup header and boot_params, which share a layout from
 * 0x1f1 on. */
const HDR_SETUP_SECTS: usize = 0x1f1;
const HDR_BOOT_FLAG: usize = 0x1fe;
/// The byte the header's end is measured by: it ends at 0x202 plus it.
const HDR_LEN_AT: usize = 0x201;
const HDR_HEADER_MAGIC: usize = 0x202;
const HDR_VERSION: usize = 0x206;
const HDR_TYPE_OF_LOADER: usize = 0x210;
const HDR_LOADFLAGS: usize = 0x211;
const HDR_RAMDISK_IMAGE: usize = 0x218;
const HDR_RAMDISK_SIZE: usize = 0x21c;
/// `heap_end_ptr`, and after it `ext_loader_ver` and `ext_loader_type`:
/// four bytes a loader owns, of which one of an undefined type says
/// nothing -- zeros, whatever the image has there.
const HDR_HEAP_END_PTR: usize = 0x224;
const HDR_HEAP_END_BYTES: usize = 4;
const HDR_CMD_LINE_PTR: usize = 0x228;
const HDR_INITRD_ADDR_MAX: usize = 0x22c;
const HDR_RELOCATABLE: usize = 0x234;
const HDR_XLOADFLAGS: usize = 0x236;
const HDR_CMDLINE_SIZE: usize = 0x238;
const HDR_PREF_ADDRESS: usize = 0x258;
const HDR_INIT_SIZE: usize = 0x260;
/// How much of the image's start the header is read from: the fields read
/// end before it, `init_size` last.
pub const HEADER_BYTES: usize = 0x270;

/// `boot_flag`, the 0xAA55 that says this is a kernel image at all.
const BOOT_FLAG: u16 = 0xAA55;
/// `header`, "HdrS": what says the boot protocol is 2.00 or newer.
const HEADER_MAGIC: u32 = 0x5372_6448;
/// The oldest protocol every field read here is in: `cmdline_size` is
/// 2.06's, `pref_address` and `init_size` 2.10's, and the 64-bit entry's
/// flag in `xloadflags` 2.12's. 2.05 was the floor once, and an older
/// header passed by its bytes at 0x236 reading zero.
pub const MIN_VERSION: u16 = 0x020C;
/// `xloadflags` bit 0: the kernel has a 64-bit entry point.
const XLF_KERNEL_64: u16 = 1 << 0;
/// `loadflags` bit 0: the protected-mode kernel is loaded high (at 1 MiB or
/// above), which the 64-bit protocol always is.
const LOADFLAGS_LOADED_HIGH: u8 = 1 << 0;
/// `type_of_loader`: an undefined boot loader, which is what this is as far
/// as the kernel needs to know.
const LOADER_UNDEFINED: u8 = 0xFF;
/// `setup_sects` of 0 means 4, for old images.
const SETUP_SECTS_OLD: u8 = 4;
const SECTOR_BYTES: u64 = 512;

/// The 64-bit entry point is the loaded kernel's start plus this.
pub const ENTRY64_OFFSET: u64 = 0x200;

/* boot_params: where the ACPI RSDP is (protocol 2.14; padding to an older
 * kernel, which scans the BIOS area for it), and the e820 map. */
const BP_ACPI_RSDP_ADDR: usize = 0x070;
const BP_E820_ENTRIES: usize = 0x1e8;
const BP_E820_TABLE: usize = 0x2d0;
const E820_ENTRY_BYTES: usize = 20;
/// The most entries the zero page's memory map holds.
pub const E820_MAX_ENTRIES: usize = 128;
/// The zero page, `boot_params`: a page.
pub const ZERO_PAGE_BYTES: usize = 4096;
const _: () = assert!(BP_E820_TABLE + E820_MAX_ENTRIES * E820_ENTRY_BYTES <= ZERO_PAGE_BYTES);
const _: () = assert!(E820_MAX_ENTRIES <= u8::MAX as usize);

/// The kinds of memory map entry a loader here gives: RAM, and reserved.
pub const E820_RAM: u32 = 1;
pub const E820_RESERVED: u32 = 2;

/// An entry of the memory map, as e820 has them -- and PVH's start info.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

/// Why an image, or what a loader would hand it, was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Shorter than the setup header.
    Short,
    /// No boot flag, or no "HdrS": not a bzImage.
    NotBzImage,
    /// A boot protocol older than [`MIN_VERSION`].
    Protocol(u16),
    /// No 64-bit entry point: the 64-bit protocol is the one taken, and
    /// every x86-64 kernel of the last decade has it.
    No64BitEntry,
    /// A command line of `len` bytes, where the kernel takes `most`.
    CommandLine { len: usize, most: u32 },
    /// A memory map of this many entries.
    MemoryMap(usize),
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refused::Short => f.write_str("shorter than a bzImage's setup header"),
            Refused::NotBzImage => f.write_str("not a bzImage: no boot flag or no HdrS"),
            Refused::Protocol(v) => write!(f, "boot protocol {}.{:02}, older than {}.{:02}", v >> 8, v & 0xFF,
                                           MIN_VERSION >> 8, MIN_VERSION & 0xFF),
            Refused::No64BitEntry => f.write_str("a kernel with no 64-bit entry"),
            Refused::CommandLine { len, most } => {
                write!(f, "a command line of {} bytes, and the kernel takes {}", len, most)
            }
            Refused::MemoryMap(n) => {
                write!(f, "a memory map of {} entries, and the zero page holds {}", n, E820_MAX_ENTRIES)
            }
        }
    }
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from(u32_at(b, o)) | (u64::from(u32_at(b, o + 4)) << 32)
}

/// The setup header, as much of it as a loader reads.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    /// Sectors of real-mode setup; the 64-bit kernel begins after them.
    pub setup_sects: u8,
    /// The boot protocol: its major number in the high byte.
    pub version: u16,
    /// Where the kernel wants to be loaded -- where one that is not
    /// relocatable must run.
    pub pref_address: u64,
    /// How much memory the kernel needs from its load address on, to
    /// decompress itself and start.
    pub init_size: u64,
    pub relocatable: bool,
    /// The highest address an initrd's last byte may be at.
    pub initrd_addr_max: u64,
    /// End of the header within the image, `0x202 + [0x201]`: how much is
    /// copied into the zero page.
    pub hdr_end: usize,
    /// The most characters of command line the kernel takes, its NUL not
    /// counted.
    pub cmdline_size: u32,
}

impl Header {
    /// The header, out of the image's first [`HEADER_BYTES`] or more.
    pub fn parse(first: &[u8]) -> Result<Header, Refused> {
        if first.len() < HEADER_BYTES {
            return Err(Refused::Short);
        }
        if u16_at(first, HDR_BOOT_FLAG) != BOOT_FLAG || u32_at(first, HDR_HEADER_MAGIC) != HEADER_MAGIC {
            return Err(Refused::NotBzImage);
        }
        let version = u16_at(first, HDR_VERSION);
        if version < MIN_VERSION {
            return Err(Refused::Protocol(version));
        }
        if u16_at(first, HDR_XLOADFLAGS) & XLF_KERNEL_64 == 0 {
            return Err(Refused::No64BitEntry);
        }
        let setup_sects = match first[HDR_SETUP_SECTS] {
            0 => SETUP_SECTS_OLD,
            n => n,
        };
        Ok(Header {
            setup_sects,
            version,
            pref_address: u64_at(first, HDR_PREF_ADDRESS),
            init_size: u64::from(u32_at(first, HDR_INIT_SIZE)),
            relocatable: first[HDR_RELOCATABLE] != 0,
            initrd_addr_max: u64::from(u32_at(first, HDR_INITRD_ADDR_MAX)),
            hdr_end: HDR_HEADER_MAGIC + usize::from(first[HDR_LEN_AT]),
            cmdline_size: u32_at(first, HDR_CMDLINE_SIZE),
        })
    }

    /// Where in the file the 64-bit kernel begins: past the boot sector and
    /// the setup's sectors.
    pub fn pm_offset(&self) -> u64 {
        (u64::from(self.setup_sects) + 1) * SECTOR_BYTES
    }
}

/// Where a loader put what the kernel is handed: the command line (its NUL
/// after it, not counted in `cmdline_len`), the initrd -- its address and
/// length, 0 and 0 for none -- and the ACPI RSDP, 0 for none, where a
/// kernel scans the BIOS area for it.
#[derive(Clone, Copy, Debug)]
pub struct Loaded {
    pub cmdline: u32,
    pub cmdline_len: usize,
    pub initrd: (u32, u32),
    pub rsdp: u64,
}

/// The zero page, into `page`, for the kernel `header` was parsed out of --
/// `first`, its first bytes: the header copied in where it sits, up to its
/// end; then the fields a loader owns -- its type, loaded high, where the
/// command line is, no heap, the initrd, the RSDP -- and the memory map
/// `e820`. A command line longer than the kernel takes, or a map longer
/// than the page holds, is refused, the page as it was.
pub fn zero_page(first: &[u8], header: &Header, loaded: &Loaded, e820: &[E820Entry],
                 page: &mut [u8; ZERO_PAGE_BYTES]) -> Result<(), Refused>
{
    if first.len() < HEADER_BYTES {
        return Err(Refused::Short);
    }
    /* `cmdline_size` is the most characters, the terminating zero not
     * counted: a line of exactly that many is the kernel's to take. */
    if loaded.cmdline_len as u64 > u64::from(header.cmdline_size) {
        return Err(Refused::CommandLine { len: loaded.cmdline_len, most: header.cmdline_size });
    }
    if e820.len() > E820_MAX_ENTRIES {
        return Err(Refused::MemoryMap(e820.len()));
    }
    page.fill(0);
    let end = header.hdr_end.min(first.len()).min(ZERO_PAGE_BYTES).max(HDR_SETUP_SECTS);
    page[HDR_SETUP_SECTS..end].copy_from_slice(&first[HDR_SETUP_SECTS..end]);
    page[HDR_TYPE_OF_LOADER] = LOADER_UNDEFINED;
    page[HDR_LOADFLAGS] = first[HDR_LOADFLAGS] | LOADFLAGS_LOADED_HIGH;
    put(page, HDR_CMD_LINE_PTR, &loaded.cmdline.to_le_bytes());
    page[HDR_HEAP_END_PTR..HDR_HEAP_END_PTR + HDR_HEAP_END_BYTES].fill(0);
    put(page, HDR_RAMDISK_IMAGE, &loaded.initrd.0.to_le_bytes());
    put(page, HDR_RAMDISK_SIZE, &loaded.initrd.1.to_le_bytes());
    put(page, BP_ACPI_RSDP_ADDR, &loaded.rsdp.to_le_bytes());
    /* At most E820_MAX_ENTRIES, which a byte holds (the asserts above). */
    page[BP_E820_ENTRIES] = e820.len() as u8;
    for (i, e) in e820.iter().enumerate() {
        let at = BP_E820_TABLE + i * E820_ENTRY_BYTES;
        put(page, at, &e.addr.to_le_bytes());
        put(page, at + 8, &e.size.to_le_bytes());
        put(page, at + 16, &e.kind.to_le_bytes());
    }
    Ok(())
}

fn put(page: &mut [u8], at: usize, bytes: &[u8]) {
    page[at..at + bytes.len()].copy_from_slice(bytes);
}
