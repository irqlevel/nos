//! Guest physical memory, which the loader runs identity-mapped over the
//! first 4 GiB of: the start info read out of it, and the ranges `Ram`
//! gave a kernel and an initrd made slices to be read into.

use alloc::string::String;

use crate::ram::{Loan, MAPPED};

/// Copy `buf.len()` bytes at physical address `addr` out: the start info,
/// its memory map and command line, where hv put them.
pub fn read(addr: u64, buf: &mut [u8]) -> Result<(), String> {
    let end = addr.checked_add(buf.len() as u64).filter(|&e| e <= MAPPED);
    if addr == 0 || end.is_none() {
        return Err(alloc::format!("the start info points at {:#x}, outside the memory mapped", addr));
    }
    // SAFETY: the range is inside the identity map, which is all readable
    // memory -- RAM, or a device page reading as ones -- and nothing of
    // Rust's aliases it as `&mut`: only `lend` below hands out RAM, and it
    // runs after the start info is read.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len()) };
    Ok(())
}

/// The bytes of a range `Ram::take` gave, to be written.
pub fn lend(loan: Loan) -> Result<&'static mut [u8], String> {
    let n = usize::try_from(loan.len()).map_err(|_| String::from("a range too long"))?;
    // SAFETY: a `Loan` is made by `Ram::take` alone, for RAM the memory map
    // has inside the identity map, outside this program's image -- its
    // code, data, BSS, stack and heap -- and for a range no other loan
    // overlaps; it is consumed here, so this slice is the only reference to
    // those bytes.
    Ok(unsafe { core::slice::from_raw_parts_mut(loan.addr() as *mut u8, n) })
}
