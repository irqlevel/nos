//! A guest's `mov` to or from DR0-DR3, which AMD-V stops it at -- every
//! one, by the host's own intercept (`hvarch::x86::svm`). The four
//! breakpoint addresses are the CPU's, not the VMCB's: left to the guest,
//! one would read what the last guest on its CPU left there, and a
//! breakpoint it set would be moved by the next guest's write, or left
//! behind when its vCPU moved to another CPU. So each guest's four are its
//! own (`hvarch::x86::svm::Guest::debug_addrs`), made the CPU's before each
//! of its entries -- where its own DR7, which the VMCB does switch, arms
//! them -- and this answers its reads from them and writes into them.
//!
//! As `crate::cr` does, the instruction is fetched by the guest's own
//! paging and decoded (`crate::insn::decode_mov_dr`) rather than taken from
//! decode assists, which QEMU's CPU and a nested hypervisor's lack, and
//! checked against the exit: another register, or the other way, is an
//! instruction changed under the exit, and stops the guest rather than be
//! guessed at. What a CPU checks before the move it is made to check here
//! too, where a CPU may check it after the intercept: CPL above 0 is a #GP,
//! and DR7's general-detect bit a #DB -- DR6.BD set, GD cleared as the CPU
//! clears it to let the handler in -- and either leaves the move unmade.

use crate::insn::{self, Mode};
use crate::memory::GuestMemory;
use crate::mmio::{self, Bytes};
use crate::vm::Backend;

/// DR7's general-detect bit: any `mov` of a debug register is a #DB.
const DR7_GD: u64 = 1 << 13;
/// DR6's bit for a #DB that general detect raised.
const DR6_BD: u64 = 1 << 13;
/// The vector of #DB.
const VECTOR_DB: u8 = 1;
/// How many of the debug registers are the breakpoint addresses.
const ADDRS: usize = 4;
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
    /// Made above CPL 0: a #GP, the move not made.
    Refused,
    /// Made with DR7.GD set: a #DB, the move not made.
    Detected,
}

/// Perform the `mov` to (`write`) or from debug register `dr` the guest
/// stopped at. A read gets the guest's own; a write is the guest's from now
/// on -- any value, as a CPU takes any address. Either done, the
/// instruction is stepped past; a #GP or a #DB in its place, not. `bytes`
/// gets what was fetched, whatever the outcome; an error leaves the guest
/// as it stopped, for the caller to stop it over.
pub fn dr_access(v: &mut Backend, memory: &GuestMemory, write: bool, dr: u8, bytes: &mut Bytes)
    -> Result<Done, Error>
{
    let mode = mmio::fetch(v.save(), memory, bytes).map_err(Error::Fetch)?;
    let mov = insn::decode_mov_dr(&bytes.b[..usize::from(bytes.len)], mode).ok_or(Error::Decode)?;
    let n = usize::from(mov.dr);
    if mov.dr != dr || mov.write != write || n >= ADDRS {
        return Err(Error::Mismatch);
    }
    if v.save().cpl != 0 {
        v.inject_gp();
        return Ok(Done::Refused);
    }
    if v.save().dr7 & DR7_GD != 0 {
        let s = v.save_mut();
        s.dr6 |= DR6_BD;
        s.dr7 &= !DR7_GD;
        v.reflect_exception(VECTOR_DB, None);
        return Ok(Done::Detected);
    }
    /* The whole register in 64-bit mode, the low half in 32-bit code --
     * which a read zero-extends, as a 32-bit result does, and a write
     * leaves the register's high half clear by. */
    let width = if mode == Mode::Long { u64::MAX } else { LOW32 };
    let mut addrs = v.debug_addrs();
    let done = if write {
        addrs[n] = mmio::gpr(v.save(), v.regs(), mov.gpr) & width;
        v.set_debug_addrs(addrs);
        Done::Written
    } else {
        /* RSP too: under AMD-V, the one backend that stops a guest here,
         * it is the VMCB's, as RAX is. */
        let (save, regs) = v.save_and_regs_mut();
        *mmio::gpr_mut(save, regs, mov.gpr) = addrs[n] & width;
        Done::Read
    };
    v.skip_emulated(u64::from(mov.len));
    Ok(done)
}
