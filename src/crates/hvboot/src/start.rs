//! What hv hands the loader: the PVH start info (src/crates/hv/src/pvh.rs,
//! Xen's start_info.h) -- its command line, the memory map, the ACPI RSDP
//! -- and the command line read for the loader's own words.

use alloc::string::String;
use alloc::vec::Vec;

use crate::phys;

const START_INFO_MAGIC: u32 = 0x336E_C578;
/// Version 1: the first with the memory map.
const MIN_VERSION: u32 = 1;
const START_INFO_BYTES: usize = 56;
const MEMMAP_ENTRY_BYTES: usize = 24;
/// The most memory map entries read: e820's own ceiling.
const MAX_MEMMAP: usize = 128;
/// The longest command line read.
const CMDLINE_MAX: usize = 4096;

pub const E820_RAM: u32 = 1;

/// An entry of the memory map, as e820 has them.
#[derive(Clone, Copy, Debug)]
pub struct MemEntry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

/// The start info, read.
pub struct StartInfo {
    pub cmdline: String,
    pub memmap: Vec<MemEntry>,
    pub rsdp: u64,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from(u32_at(b, o)) | (u64::from(u32_at(b, o + 4)) << 32)
}

impl StartInfo {
    /// Read the start info at `addr`, which `EBX` held at the entry.
    pub fn read(addr: u64) -> Result<StartInfo, String> {
        let mut si = [0u8; START_INFO_BYTES];
        phys::read(addr, &mut si)?;
        if u32_at(&si, 0) != START_INFO_MAGIC {
            return Err(alloc::format!("no PVH start info at {:#x} (magic {:#x})", addr, u32_at(&si, 0)));
        }
        let version = u32_at(&si, 4);
        if version < MIN_VERSION {
            return Err(alloc::format!("a PVH start info of version {}, without a memory map", version));
        }

        let cmdline_at = u64_at(&si, 24);
        let mut cmdline = String::new();
        if cmdline_at != 0 {
            let mut buf = [0u8; CMDLINE_MAX];
            /* Read a byte at a time up to the NUL: the line may end anywhere,
             * and what follows it need not be readable. */
            let mut n = 0;
            while n < CMDLINE_MAX {
                phys::read(cmdline_at + n as u64, &mut buf[n..n + 1])?;
                if buf[n] == 0 {
                    break;
                }
                n += 1;
            }
            cmdline = String::from_utf8_lossy(&buf[..n]).into_owned();
        }

        let rsdp = u64_at(&si, 32);
        let memmap_at = u64_at(&si, 40);
        let entries = u32_at(&si, 48) as usize;
        if entries == 0 || entries > MAX_MEMMAP || memmap_at == 0 {
            return Err(alloc::format!("a memory map of {} entries at {:#x}", entries, memmap_at));
        }
        let mut memmap = Vec::with_capacity(entries);
        for i in 0..entries {
            let mut e = [0u8; MEMMAP_ENTRY_BYTES];
            phys::read(memmap_at + (i * MEMMAP_ENTRY_BYTES) as u64, &mut e)?;
            memmap.push(MemEntry { addr: u64_at(&e, 0), size: u64_at(&e, 8), kind: u32_at(&e, 16) });
        }
        Ok(StartInfo { cmdline, memmap, rsdp })
    }
}

/// The loader's command line: its own words -- `hvboot.<name>[=<value>]` --
/// and the rest, which go on to the kernel after its entry's own: what hv
/// adds a guest's line (the TSC's rate, `ip=`, `noapic`) and what the VM's
/// `cmdline=` says.
pub struct Options {
    /// `hvboot.entry=`: the GRUB entry to boot in place of its default --
    /// a number, `N>M` into a submenu, a title or an id, as GRUB's
    /// `default` takes.
    pub entry: Option<String>,
    /// `hvboot.disk=N`: look for GRUB's configuration on disk N alone
    /// (0 for `vda`).
    pub disk: Option<usize>,
    /// `hvboot.verbose`: say each filesystem looked at.
    pub verbose: bool,
    /// `tsc_early_khz=`, for timeouts: the TSC's rate, as hv knows it.
    pub tsc_khz: Option<u64>,
    /// Every other word, in order.
    pub kernel_args: Vec<String>,
}

impl Options {
    pub fn parse(line: &str) -> Result<Options, String> {
        let mut o = Options { entry: None, disk: None, verbose: false, tsc_khz: None, kernel_args: Vec::new() };
        for word in line.split_ascii_whitespace() {
            if let Some(own) = word.strip_prefix("hvboot.") {
                let (name, value) = match own.split_once('=') {
                    Some((n, v)) => (n, Some(v)),
                    None => (own, None),
                };
                match (name, value) {
                    ("entry", Some(v)) if !v.is_empty() => o.entry = Some(String::from(v)),
                    ("disk", Some(v)) => {
                        o.disk = Some(v.parse().map_err(|_| alloc::format!("hvboot.disk={}: not a number", v))?);
                    }
                    ("verbose", None) => o.verbose = true,
                    _ => return Err(alloc::format!("{}: not an option of hvboot's (hvboot.entry=, hvboot.disk=, \
                                                    hvboot.verbose)", word)),
                }
                continue;
            }
            if let Some(khz) = word.strip_prefix("tsc_early_khz=") {
                o.tsc_khz = khz.parse().ok().filter(|&k| k != 0);
            }
            o.kernel_args.push(String::from(word));
        }
        Ok(o)
    }
}
