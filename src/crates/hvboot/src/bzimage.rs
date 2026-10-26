//! A Linux bzImage, booted by the 64-bit boot protocol
//! (Documentation/arch/x86/boot.rst) -- as hv itself boots one
//! (src/crates/hv/src/linux.rs), which this follows: the setup header read and
//! checked, the 64-bit kernel loaded at `pref_address`, and a zero page with
//! the header copied in and the fields a loader owns filled -- the command
//! line, the initrd, the memory map, the ACPI RSDP.

use alloc::string::String;

use crate::start::MemEntry;

/* Offsets into the setup header / boot_params, which share a layout from
 * 0x1f1 on. */
const HDR_SETUP_SECTS: usize = 0x1f1;
const HDR_LEN_AT: usize = 0x201;
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
const HDR_RELOCATABLE: usize = 0x234;
const HDR_XLOADFLAGS: usize = 0x236;
const HDR_CMDLINE_SIZE: usize = 0x238;
const HDR_PREF_ADDRESS: usize = 0x258;
const HDR_INIT_SIZE: usize = 0x260;
/// How much of the image's start the header lies in.
pub const HEADER_BYTES: usize = 0x270;

const BOOT_FLAG: u16 = 0xAA55;
const HEADER_MAGIC: u32 = 0x5372_6448;
/// The oldest protocol with a 64-bit entry and a relocatable kernel.
const MIN_VERSION: u16 = 0x0205;
const XLF_KERNEL_64: u16 = 1 << 0;
const LOADFLAGS_LOADED_HIGH: u8 = 1 << 0;
const LOADER_UNDEFINED: u8 = 0xFF;
/// The 64-bit entry is the loaded kernel's start plus this.
pub const ENTRY64_OFFSET: u64 = 0x200;

/* boot_params: the ACPI RSDP and the e820 map. */
const BP_ACPI_RSDP_ADDR: usize = 0x070;
const BP_E820_ENTRIES: usize = 0x1e8;
const BP_E820_TABLE: usize = 0x2d0;
const E820_ENTRY_BYTES: usize = 20;
const E820_MAX_ENTRIES: usize = 128;
pub const ZERO_PAGE_BYTES: usize = 4096;

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
    pub setup_sects: u8,
    pub version: u16,
    pub pref_address: u64,
    pub init_size: u64,
    pub relocatable: bool,
    pub initrd_addr_max: u64,
    pub hdr_end: usize,
    pub cmdline_size: u32,
}

impl Header {
    /// The header, out of the image's first `HEADER_BYTES` or more.
    pub fn parse(first: &[u8]) -> Result<Header, String> {
        if first.len() < HEADER_BYTES {
            return Err(String::from("shorter than a bzImage's setup header"));
        }
        if u16_at(first, HDR_BOOT_FLAG) != BOOT_FLAG || u32_at(first, HDR_HEADER_MAGIC) != HEADER_MAGIC {
            return Err(String::from("not a bzImage: no boot flag or no HdrS"));
        }
        let version = u16_at(first, HDR_VERSION);
        if version < MIN_VERSION {
            return Err(alloc::format!("boot protocol {:#x}, older than 2.05", version));
        }
        if u16_at(first, HDR_XLOADFLAGS) & XLF_KERNEL_64 == 0 {
            return Err(String::from("a kernel with no 64-bit entry"));
        }
        let setup_sects = match first[HDR_SETUP_SECTS] {
            0 => 4,
            n => n,
        };
        let hdr_end = HDR_HEADER_MAGIC + usize::from(first[HDR_LEN_AT]);
        Ok(Header {
            setup_sects,
            version,
            pref_address: u64_at(first, HDR_PREF_ADDRESS),
            init_size: u64::from(u32_at(first, HDR_INIT_SIZE)),
            relocatable: first[HDR_RELOCATABLE] != 0,
            initrd_addr_max: u64::from(u32_at(first, HDR_INITRD_ADDR_MAX)),
            hdr_end,
            cmdline_size: u32_at(first, HDR_CMDLINE_SIZE),
        })
    }

    /// Where in the file the 64-bit kernel begins.
    pub fn pm_offset(&self) -> u64 {
        (u64::from(self.setup_sects) + 1) * 512
    }
}

/// The zero page for a kernel whose header is in `first`, its command line
/// at `cmdline` (`cmdline_len` bytes, NUL after), its initrd `initrd`, the
/// memory map `e820`, the RSDP `rsdp`.
pub fn zero_page(first: &[u8], header: &Header, cmdline: u64, cmdline_len: usize, initrd: (u64, u64),
                 e820: &[MemEntry], rsdp: u64, page: &mut [u8; ZERO_PAGE_BYTES]) -> Result<(), String>
{
    if cmdline_len as u64 >= u64::from(header.cmdline_size) {
        return Err(alloc::format!("a command line of {} bytes, and the kernel takes {}", cmdline_len,
                                  header.cmdline_size));
    }
    if e820.len() > E820_MAX_ENTRIES {
        return Err(alloc::format!("a memory map of {} entries", e820.len()));
    }
    let (Ok(cmdline32), Ok(initrd_addr), Ok(initrd_len)) =
        (u32::try_from(cmdline), u32::try_from(initrd.0), u32::try_from(initrd.1))
    else {
        return Err(String::from("a command line or an initrd above 4 GiB"));
    };
    page.fill(0);
    let end = header.hdr_end.min(first.len()).max(HDR_SETUP_SECTS);
    page[HDR_SETUP_SECTS..end].copy_from_slice(&first[HDR_SETUP_SECTS..end]);
    page[HDR_TYPE_OF_LOADER] = LOADER_UNDEFINED;
    page[HDR_LOADFLAGS] = first[HDR_LOADFLAGS] | LOADFLAGS_LOADED_HIGH;
    page[HDR_CMD_LINE_PTR..HDR_CMD_LINE_PTR + 4].copy_from_slice(&cmdline32.to_le_bytes());
    page[HDR_HEAP_END_PTR..HDR_HEAP_END_PTR + 2].fill(0);
    page[HDR_RAMDISK_IMAGE..HDR_RAMDISK_IMAGE + 4].copy_from_slice(&initrd_addr.to_le_bytes());
    page[HDR_RAMDISK_SIZE..HDR_RAMDISK_SIZE + 4].copy_from_slice(&initrd_len.to_le_bytes());
    page[BP_ACPI_RSDP_ADDR..BP_ACPI_RSDP_ADDR + 8].copy_from_slice(&rsdp.to_le_bytes());
    page[BP_E820_ENTRIES] = e820.len() as u8;
    for (i, e) in e820.iter().enumerate() {
        let at = BP_E820_TABLE + i * E820_ENTRY_BYTES;
        page[at..at + 8].copy_from_slice(&e.addr.to_le_bytes());
        page[at + 8..at + 16].copy_from_slice(&e.size.to_le_bytes());
        page[at + 16..at + 20].copy_from_slice(&e.kind.to_le_bytes());
    }
    Ok(())
}
