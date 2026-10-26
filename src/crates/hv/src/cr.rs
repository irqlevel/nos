//! A guest's `mov` to or from CR4, which AMD-V stops it at -- every write,
//! by the host's own intercept (`hvarch::x86::svm`), and every read, by
//! this hypervisor's -- and VT-x at a write that changes a bit the host
//! keeps (`hvarch::x86::vmx`'s guest/host mask), its reads answered by the
//! read shadow. The CR4 the guest runs with keeps MCE: with it clear, a
//! machine check while the guest ran would be a shutdown that looks like
//! the guest's triple fault, and the host would never hear of it. The guest
//! reads back the CR4 it wrote, MCE as it left it. And a bit its CPUID does
//! not offer -- PKE, whose PKRU nothing switches between guests, SMEP,
//! FSGSBASE, LA57, OSXSAVE on a CPU without XSAVE, PCIDE -- is the #GP a CPU
//! without the feature gives, not a feature the guest has behind its CPUID's
//! back.
//!
//! And its XSETBV, which both stop it at -- XCR0 is the CPU's, and neither
//! `vmrun` nor `vmlaunch` switches it -- performed as a CPU offering the
//! XSAVE components `hvarch::x86::fp::offered` names does ([`xsetbv`]).
//!
//! Under AMD-V the exit names the register and which way, and on a CPU with
//! decode assists the general-purpose register too. That last is not used:
//! the instruction is fetched by the guest's own paging and decoded
//! (`crate::insn::decode_mov_cr`), as an MMIO access's is, so that the one
//! path is the one every CPU takes -- QEMU's and a nested hypervisor's have
//! no decode assists. It is checked against the exit: another register, or
//! the other way, is an instruction changed under the exit, and stops the
//! guest rather than be guessed at. VT-x's exit names the general-purpose
//! register and the instruction's length always, as part of the
//! architecture, and there nothing is fetched. A write is held to what a
//! CPU checks, and the guest's TLB flushed where a CPU flushes it.

use hvarch::x86::svm::vmcb::Save;

use crate::insn::{self, Mode};
use crate::memory::GuestMemory;
use crate::mmio::{self, Bytes};
use crate::vm::Backend;

/* The CR4 bits a write is checked against, or flushes for. */
const CR4_PSE: u64 = 1 << 4;
const CR4_PAE: u64 = 1 << 5;
const CR4_PGE: u64 = 1 << 7;
const CR4_LA57: u64 = 1 << 12;
const CR4_PCIDE: u64 = 1 << 17;
const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;
const CR4_PKE: u64 = 1 << 22;
const CR4_CET: u64 = 1 << 23;
/// The bits a CPU flushes its TLB over when they change -- PGE and PCIDE --
/// or translates by: a write that changes one has the guest's TLB flushed,
/// so that nothing it translated before is used after.
const CR4_FLUSH: u64 = CR4_PSE | CR4_PAE | CR4_PGE | CR4_LA57 | CR4_PCIDE | CR4_SMEP | CR4_SMAP | CR4_PKE;
const CR0_WP: u64 = 1 << 16;
const EFER_LMA: u64 = 1 << 10;
/// CR3's PCID, its low twelve bits: 0 for PCIDE to be turned on.
const CR3_PCID: u64 = 0xFFF;
/// The control register whose accesses this performs.
const CR4: u8 = 4;
/// CR4.OSXSAVE: XGETBV and XSETBV may be run.
const CR4_OSXSAVE: u64 = 1 << 18;
/// The extended control register XSETBV writes, of those ECX names: XCR0,
/// the only one there is to write.
const XCR0: u32 = 0;
/// What 32-bit code reads and writes of a register.
const LOW32: u64 = 0xFFFF_FFFF;

/// Why an access was not performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The code could not be read, or the CPU is in a mode not decoded here.
    Fetch(mmio::Error),
    /// The bytes are no `mov` to or from a control register.
    Decode,
    /// The bytes are a `mov` of another control register, or the other way
    /// than the exit: an instruction changed under it.
    Mismatch,
}

impl Error {
    /// For a report.
    pub fn describe(&self) -> &'static str {
        match self {
            Error::Fetch(e) => e.describe(),
            Error::Decode => "the instruction is no mov to or from a control register",
            Error::Mismatch => "the instruction is not the access of CR4 the exit was for",
        }
    }
}

/// What became of an access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Done {
    /// CR4, as the guest set it, into its register.
    Read,
    /// The value is the guest's CR4 from now on.
    Written,
    /// The value is none a CPU takes: a #GP, the instruction not run.
    Refused,
}

/// Perform the `mov` to (`write`) or from CR4 the guest stopped at, through
/// general-purpose register `gpr` where the exit names it and the
/// instruction decoded where it does not. A read gets the CR4 the guest
/// set. A write a CPU would take (`valid`, every bit in `allowed` -- the
/// guest's CPUID's, `crate::policy::cr4_allowed`) is the guest's CR4 from
/// now on; one it would not is a #GP. Either done, the instruction is
/// stepped past -- for a #GP, not. `bytes` gets what was fetched, whatever
/// the outcome; an error leaves the guest as it stopped, for the caller to
/// stop it over.
pub fn cr4_access(v: &mut Backend, memory: &GuestMemory, write: bool, allowed: u64, gpr: Option<u8>,
                  bytes: &mut Bytes) -> Result<Done, Error>
{
    let (gpr, len, long) = match gpr {
        /* Named by the exit, with the instruction's length: nothing to
         * fetch. An exit that names a register says how long it is, and
         * one that does not is a backend's bug -- a stop, not a guess. */
        Some(gpr) => (gpr, v.exit_insn_len().ok_or(Error::Decode)?, v.in_64bit_code()),
        None => {
            let mode = mmio::fetch(v.save(), memory, bytes).map_err(Error::Fetch)?;
            let mov = insn::decode_mov_cr(&bytes.b[..usize::from(bytes.len)], mode).ok_or(Error::Decode)?;
            if mov.cr != CR4 || mov.write != write {
                return Err(Error::Mismatch);
            }
            (mov.gpr, u64::from(mov.len), mode == Mode::Long)
        }
    };
    /* The whole register in 64-bit mode, the low half in 32-bit code --
     * which a read zero-extends, as a 32-bit result does. */
    let width = if long { u64::MAX } else { LOW32 };
    let done = if write {
        let value = mmio::gpr(v.save(), v.regs(), gpr) & width;
        let old = v.cr4();
        if !valid(old, value, v.save(), allowed) {
            v.inject_gp();
            return Ok(Done::Refused);
        }
        v.set_cr4(value, (old ^ value) & CR4_FLUSH != 0);
        Done::Written
    } else {
        let value = v.cr4() & width;
        v.set_gpr(gpr, value);
        Done::Read
    };
    v.skip_emulated(len);
    Ok(done)
}

/// Whether a CPU takes `new` into CR4 over `old`, in the state `save` holds:
/// no bit outside `allowed`; PAE kept, and LA57 as it is, while long mode is
/// active; PCIDE turned on only in long mode with CR3's PCID 0; and CET
/// only with CR0.WP.
pub fn valid(old: u64, new: u64, save: &Save, allowed: u64) -> bool {
    let long = save.efer & EFER_LMA != 0;
    if new & !allowed != 0 {
        return false;
    }
    if long && (new & CR4_PAE == 0 || (old ^ new) & CR4_LA57 != 0) {
        return false;
    }
    if new & !old & CR4_PCIDE != 0 && (!long || save.cr3 & CR3_PCID != 0) {
        return false;
    }
    if new & CR4_CET != 0 && save.cr0 & CR0_WP == 0 {
        return false;
    }
    true
}

/// What became of an XSETBV.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Xcr {
    /// The value is the guest's XCR0 from now on.
    Written,
    /// A CPU refuses it: above CPL 0, of an XCR other than XCR0, or a value
    /// XCR0 does not take -- a #GP, the instruction not run.
    Refused,
    /// CR4.OSXSAVE is clear: a #UD, the instruction not run.
    Undefined,
}

/// Perform the XSETBV the guest stopped at -- ECX naming the register,
/// EDX:EAX the value -- in the order a CPU checks it: a #UD without
/// CR4.OSXSAVE, a #GP above CPL 0, for any register but XCR0, and for a
/// value XCR0 does not take (`GuestFp::set_xcr0`: the x87 in it, AVX only
/// with SSE, and nothing not offered). VT-x faults the first two before its
/// exit, and AMD-V's intercept may come before either -- QEMU's TCG takes it
/// first -- so they are checked here whichever it is. Taken, the value is
/// the guest's XCR0 from its next entry, and the instruction stepped past.
pub fn xsetbv(v: &mut Backend) -> Xcr {
    if v.cr4() & CR4_OSXSAVE == 0 {
        v.inject_ud();
        return Xcr::Undefined;
    }
    let xcr = v.regs().rcx as u32;
    let value = (v.regs().rdx & LOW32) << 32 | (v.save().rax & LOW32);
    if v.save().cpl != 0 || xcr != XCR0 || !v.set_xcr0(value) {
        v.inject_gp();
        return Xcr::Refused;
    }
    v.skip_xsetbv();
    Xcr::Written
}
