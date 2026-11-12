//! What hv hands the loader: the PVH start info (src/crates/hypervisor/hv/src/pvh.rs,
//! Xen's start_info.h) -- its command line, the modules, the memory map,
//! the ACPI RSDP.

use alloc::string::String;
use alloc::vec::Vec;

use bzimage::E820Entry;

use crate::phys;

const START_INFO_MAGIC: u32 = 0x336E_C578;
/// Version 1: the first with the memory map.
const MIN_VERSION: u32 = 1;
const START_INFO_BYTES: usize = 56;
const SI_NR_MODULES: usize = 12;
const SI_MODLIST: usize = 16;
const MODULE_BYTES: usize = 32;
const MEMMAP_ENTRY_BYTES: usize = 24;
/// The most memory map entries read: e820's own ceiling.
const MAX_MEMMAP: usize = 128;
/// The most modules read: hv gives one at most, an initrd.
const MAX_MODULES: usize = 8;
/// The longest command line read.
const CMDLINE_MAX: usize = 4096;

/// The start info, read.
pub struct StartInfo {
    pub cmdline: String,
    pub memmap: Vec<E820Entry>,
    pub rsdp: u64,
    /// The modules hv loaded beside the loader -- an `initrd=` given to
    /// `hv start`: where each is, and how long.
    pub modules: Vec<(u64, u64)>,
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

        let count = u32_at(&si, SI_NR_MODULES) as usize;
        let list = u64_at(&si, SI_MODLIST);
        if count > MAX_MODULES || (count != 0 && list == 0) {
            return Err(alloc::format!("{} modules at {:#x}", count, list));
        }
        let mut modules = Vec::with_capacity(count);
        for i in 0..count {
            let mut m = [0u8; MODULE_BYTES];
            let at = list.checked_add((i * MODULE_BYTES) as u64)
                .ok_or_else(|| alloc::format!("a module list at {:#x}", list))?;
            phys::read(at, &mut m)?;
            modules.push((u64_at(&m, 0), u64_at(&m, 8)));
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
            memmap.push(E820Entry { addr: u64_at(&e, 0), size: u64_at(&e, 8), kind: u32_at(&e, 16) });
        }
        Ok(StartInfo { cmdline, memmap, rsdp, modules })
    }
}
