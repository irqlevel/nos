//! A guest's linear addresses, translated by its own page tables: how the
//! MMIO path finds the instruction that faulted, which the exit names only
//! by its linear address (RIP, and the code segment's base).
//!
//! The walk is the CPU's for a fetch, and only as much of it as finding the
//! bytes takes: long mode's four levels -- five, with CR4.LA57 -- with their
//! 1 GiB and 2 MiB pages, or no paging at all. The permissions are not
//! checked: the CPU fetched the instruction already, and faulted on its data
//! access, so the page was one it could execute from. What is read is the
//! guest's memory, through the reader the caller gives -- guest RAM, and
//! nothing that is not -- and a table the guest changed since is read as it
//! is now: another of its CPUs racing the walk can only have the guest's own
//! instruction come out wrong, which the caller checks against the fault
//! and stops the guest over. 32-bit paging is none of a 64-bit kernel's,
//! and is refused.

/* Control register and EFER bits. */
const CR0_PG: u64 = 1 << 31;
const CR4_PAE: u64 = 1 << 5;
const CR4_LA57: u64 = 1 << 12;
const EFER_LMA: u64 = 1 << 10;

/* Page-table entry bits, and the address in one: bits 51:12, the most a
 * CPU has. */
const PTE_PRESENT: u64 = 1 << 0;
const PTE_LARGE: u64 = 1 << 7;
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// CR3's table address: the same bits, the low twelve being the PCID or
/// cache controls.
const CR3_MASK: u64 = ADDRESS_MASK;

pub const PAGE_SIZE: u64 = 4096;
const INDEX_BITS: u32 = 9;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
const PAGE_SHIFT: u32 = 12;
/// The levels whose entries may map a page themselves: a PDPT entry 1 GiB,
/// a page directory's 2 MiB. By the level's number, 1 the page table.
const LARGE_LEVELS: [u32; 2] = [2, 3];

/// The paging state an exit leaves: what the translation depends on.
#[derive(Clone, Copy, Debug)]
pub struct Paging {
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
}

/// Why a linear address has no guest physical one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Miss {
    /// Not canonical for the paging mode: no fetch could have come from it.
    NonCanonical,
    /// An entry on the way was not present, or its table was no RAM the
    /// reader could read.
    NotMapped,
    /// A paging mode this does not walk: 32-bit paging, with or without
    /// PAE.
    Unsupported,
}

/// The guest physical address `la` is at, by `paging`, the tables read by
/// `read` -- which gives the quadword at a guest physical address, or None
/// where there is no RAM.
pub fn translate(paging: &Paging, la: u64, read: impl Fn(u64) -> Option<u64>) -> Result<u64, Miss> {
    if paging.cr0 & CR0_PG == 0 {
        /* No paging: the linear address is the physical one. */
        return Ok(la);
    }
    if paging.efer & EFER_LMA == 0 || paging.cr4 & CR4_PAE == 0 {
        return Err(Miss::Unsupported);
    }
    let levels: u32 = if paging.cr4 & CR4_LA57 != 0 { 5 } else { 4 };
    /* The top bit an address has, and every bit above it its copy. */
    let bits = PAGE_SHIFT + INDEX_BITS * levels;
    let top = (la as i64) >> (bits - 1);
    if top != 0 && top != -1 {
        return Err(Miss::NonCanonical);
    }

    let mut table = paging.cr3 & CR3_MASK;
    for level in (1..=levels).rev() {
        let shift = PAGE_SHIFT + INDEX_BITS * (level - 1);
        let index = (la >> shift) & INDEX_MASK;
        let entry = read(table + index * 8).ok_or(Miss::NotMapped)?;
        if entry & PTE_PRESENT == 0 {
            return Err(Miss::NotMapped);
        }
        if level == 1 || (LARGE_LEVELS.contains(&level) && entry & PTE_LARGE != 0) {
            /* A page: its frame, and the offset in it -- the address bits
             * below this level's shift. */
            let offset_mask = (1u64 << shift) - 1;
            return Ok((entry & ADDRESS_MASK & !offset_mask) | (la & offset_mask));
        }
        table = entry & ADDRESS_MASK;
    }
    Err(Miss::NotMapped)
}

/// Read up to `buf.len()` bytes of the guest's code at linear address `la`:
/// page by page, each translated on its own -- an instruction may cross into
/// the next page, and that page be mapped elsewhere, or not at all. How many
/// bytes were read: fewer than asked where a page on the way is not mapped,
/// or not RAM; `read_bytes` reads guest physical memory into a slice, false
/// where there is none.
pub fn fetch(
    paging: &Paging,
    la: u64,
    buf: &mut [u8],
    read: impl Fn(u64) -> Option<u64>,
    read_bytes: impl Fn(u64, &mut [u8]) -> bool,
) -> Result<usize, Miss> {
    let mut done = 0;
    while done < buf.len() {
        let at = la.wrapping_add(done as u64);
        let gpa = match translate(paging, at, &read) {
            Ok(gpa) => gpa,
            /* The first byte must be there; past it, the instruction may
             * end before the page that is not. */
            Err(miss) if done == 0 => return Err(miss),
            Err(_) => break,
        };
        let in_page = (PAGE_SIZE - (at & (PAGE_SIZE - 1))) as usize;
        let n = in_page.min(buf.len() - done);
        if !read_bytes(gpa, &mut buf[done..done + n]) {
            if done == 0 {
                return Err(Miss::NotMapped);
            }
            break;
        }
        done += n;
    }
    Ok(done)
}
