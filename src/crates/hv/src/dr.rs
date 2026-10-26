//! A guest's `mov` to or from a debug register, which AMD-V stops it at for
//! DR0-DR3 -- every one, by the host's own intercept (`hvarch::x86::svm`)
//! -- and VT-x for every debug register (`hvarch::x86::vmx`). The four
//! breakpoint addresses are the CPU's, not the VMCB's or the VMCS's: left to
//! the guest, one would read what the last guest on its CPU left there, and
//! a breakpoint it set would be moved by the next guest's write, or left
//! behind when its vCPU moved to another CPU. So each guest's four are its
//! own (`Guest::debug_addrs` on either side), made the CPU's before each of
//! its entries -- where its own DR7, which the VMCB and the VMCS do switch,
//! arms them -- and this answers its reads from them and writes into them.
//! Under VT-x, whose control stops every `mov` of a debug register or none,
//! DR6 and DR7 are answered here too: DR6 is no VMCS field, and is the
//! guest's own here alone; DR7 is the VMCS's, written back to it.
//!
//! As `crate::cr` does, under AMD-V the instruction is fetched by the
//! guest's own paging and decoded (`crate::insn::decode_mov_dr`) rather than
//! taken from decode assists, which QEMU's CPU and a nested hypervisor's
//! lack, and checked against the exit: another register, or the other way,
//! is an instruction changed under the exit, and stops the guest rather than
//! be guessed at. VT-x's exit names the general-purpose register and the
//! instruction's length always. What a CPU checks before the move it is
//! made to check here too, where the exit comes before the CPU checks it:
//! DR4 and DR5 under CR4.DE are a #UD -- and DR6 and DR7's other names
//! without it; CPL above 0 is a #GP; DR7's general-detect bit a #DB -- DR6.BD
//! set, GD cleared as the CPU clears it to let the handler in; and a DR6 or
//! DR7 written with a bit above 31 a #GP -- and any of them leaves the move
//! unmade.

use crate::insn::{self, Mode};
use crate::memory::GuestMemory;
use crate::mmio::{self, Bytes};
use crate::vm::Backend;

/// DR7's general-detect bit: any `mov` of a debug register is a #DB.
const DR7_GD: u64 = 1 << 13;
/// DR6's bit for a #DB that general detect raised.
const DR6_BD: u64 = 1 << 13;
/// The bits of DR6 a write sets as it is told -- B0-B3, BD, BS and BT --
/// and the ones it reads as 1 whatever is written: 4-11 and 16-31, BLD's
/// and RTM's among them, neither feature being offered. Bit 12 is 0.
const DR6_VOLATILE: u64 = 0xE00F;
const DR6_FIXED_1: u64 = 0xFFFF_0FF0;
/// The bits of DR7 a write sets as it is told -- the enables, the
/// conditions and lengths, LE and GE, GD and bit 11 -- and bit 10, which
/// reads as 1. 12, 14 and 15 are 0.
const DR7_VOLATILE: u64 = 0xFFFF_2BFF;
const DR7_FIXED_1: u64 = 0x400;
/// CR4.DE: DR4 and DR5 a #UD, rather than DR6 and DR7 again.
const CR4_DE: u64 = 1 << 3;
/// The vector of #DB.
const VECTOR_DB: u8 = 1;
/// How many of the debug registers are the breakpoint addresses, and the
/// numbers of the others this answers.
const ADDRS: u8 = 4;
const DR6: u8 = 6;
const DR7: u8 = 7;
/// What 32-bit code reads and writes of a register.
const LOW32: u64 = 0xFFFF_FFFF;

/// Why an access was not performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The code could not be read, or the CPU is in a mode not decoded here.
    Fetch(mmio::Error),
    /// The bytes are no `mov` to or from a debug register.
    Decode,
    /// The bytes are a `mov` of another debug register, or the other way
    /// than the exit: an instruction changed under it.
    Mismatch,
}

impl Error {
    /// For a report.
    pub fn describe(&self) -> &'static str {
        match self {
            Error::Fetch(e) => e.describe(),
            Error::Decode => "the instruction is no mov to or from a debug register",
            Error::Mismatch => "the instruction is not the access of a debug register the exit was for",
        }
    }
}

/// What became of an access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Done {
    /// The guest's register, into the general-purpose one.
    Read,
    /// The value is the guest's register from now on.
    Written,
    /// Made above CPL 0, or a DR6 or DR7 written with a bit above 31: a
    /// #GP, the move not made.
    Refused,
    /// DR4 or DR5 under CR4.DE: a #UD, the move not made.
    Undefined,
    /// Made with DR7.GD set: a #DB, the move not made.
    Detected,
}

/// Perform the `mov` to (`write`) or from debug register `dr` the guest
/// stopped at, through general-purpose register `gpr` where the exit names
/// it and the instruction decoded where it does not. A read gets the
/// guest's own; a write is the guest's from now on -- any value of DR0-DR3,
/// as a CPU takes any address, and of DR6 and DR7 the bits they have. Either
/// done, the instruction is stepped past; a #UD, a #GP or a #DB in its
/// place, not. `bytes` gets what was fetched, whatever the outcome; an error
/// leaves the guest as it stopped, for the caller to stop it over.
pub fn dr_access(v: &mut Backend, memory: &GuestMemory, write: bool, dr: u8, gpr: Option<u8>,
                 bytes: &mut Bytes) -> Result<Done, Error>
{
    let (gpr, len, long) = match gpr {
        /* Named by the exit, with the instruction's length: nothing to
         * fetch. An exit that names a register says how long it is, and
         * one that does not is a backend's bug -- a stop, not a guess. */
        Some(gpr) => (gpr, v.exit_insn_len().ok_or(Error::Decode)?, v.in_64bit_code()),
        None => {
            let mode = mmio::fetch(v.save(), memory, bytes).map_err(Error::Fetch)?;
            let mov = insn::decode_mov_dr(&bytes.b[..usize::from(bytes.len)], mode).ok_or(Error::Decode)?;
            if mov.dr != dr || mov.write != write {
                return Err(Error::Mismatch);
            }
            (mov.gpr, u64::from(mov.len), mode == Mode::Long)
        }
    };
    /* DR4 and DR5: DR6 and DR7 by other names, unless debug extensions
     * make them undefined. Nothing past DR7 is: a CPU faults on it before
     * any exit. */
    let n = match dr {
        4 | 5 if v.cr4() & CR4_DE != 0 => {
            v.inject_ud();
            return Ok(Done::Undefined);
        }
        4 | 5 => dr + 2,
        0..=3 | DR6 | DR7 => dr,
        _ => return Err(Error::Mismatch),
    };
    if v.save().cpl != 0 {
        v.inject_gp();
        return Ok(Done::Refused);
    }
    if v.save().dr7 & DR7_GD != 0 {
        let dr7 = v.save().dr7 & !DR7_GD;
        v.save_mut().dr6 |= DR6_BD;
        v.set_dr7(dr7);
        v.reflect_exception(VECTOR_DB, None);
        return Ok(Done::Detected);
    }
    /* The whole register in 64-bit mode, the low half in 32-bit code --
     * which a read zero-extends, as a 32-bit result does, and a write
     * leaves the register's high half clear by. */
    let width = if long { u64::MAX } else { LOW32 };
    let done = if write {
        let value = mmio::gpr(v.save(), v.regs(), gpr) & width;
        match n {
            DR6 | DR7 if value & !LOW32 != 0 => {
                v.inject_gp();
                return Ok(Done::Refused);
            }
            DR6 => v.save_mut().dr6 = (value & DR6_VOLATILE) | DR6_FIXED_1,
            DR7 => v.set_dr7((value & DR7_VOLATILE) | DR7_FIXED_1),
            _ => {
                let mut addrs = v.debug_addrs();
                addrs[usize::from(n)] = value;
                v.set_debug_addrs(addrs);
            }
        }
        Done::Written
    } else {
        let value = match n {
            DR6 => v.save().dr6,
            DR7 => v.save().dr7,
            _ if n < ADDRS => v.debug_addrs()[usize::from(n)],
            _ => return Err(Error::Mismatch),
        };
        v.set_gpr(gpr, value & width);
        Done::Read
    };
    v.skip_emulated(len);
    Ok(done)
}
