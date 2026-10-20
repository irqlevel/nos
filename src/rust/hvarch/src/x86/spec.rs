//! What keeps one guest's speculation from the host and from the next guest
//! on a CPU -- the part of it that is the world switch's, under both
//! extensions, with KVM's as the reference.
//!
//! Under both:
//!
//! - After every exit the return stack is refilled before the host's first
//!   `ret`, and the guest's values cleared out of every register the run
//!   stub returns with (`svm::vmrun_stub`, `vmx::vmx_launch_stub`): a guest
//!   that left return addresses of its choosing in the stack the CPU
//!   predicts returns from would have the host's returns predicted to them,
//!   and a register still holding a guest's value is a value the host may
//!   use past a branch it has not yet resolved.
//! - Between two guests on a CPU, an indirect branch prediction barrier
//!   (IBPB): what one trained the predictors with does not steer the next.
//!   A guest's own CPUs share its nested table, whose id tells guests
//!   apart ([`State::entering`]), so they do not pay for one between each
//!   other.
//!
//! Under AMD-V:
//!
//! - Automatic IBRS (Zen 4 and later) turned on while the extension is:
//!   the host's indirect branches, at CPL 0, are then not steered by what a
//!   guest trained.
//!
//! Under VT-x, decided for each CPU as the extension is turned on there,
//! from what CPUID and IA32_ARCH_CAPABILITIES say of it:
//!
//! - IBRS in IA32_SPEC_CTRL while the extension is on. With enhanced IBRS
//!   (Ice Lake and later; the EX44) set once, it keeps the predictions the
//!   host makes from what a guest trained; without it (Skylake; the Dell)
//!   IBRS is only a barrier at the moment it is written, and is written
//!   again after every exit, before the host's first indirect branch.
//! - BHI_DIS_S beside it, where the CPU has the control (Alder Lake and
//!   later): the branch history a guest leaves behind is not what the
//!   host's indirect branches are predicted by.
//! - VERW just before every entry, where MDS, TAA or RFDS leave what the
//!   host had in the CPU's buffers for a guest to sample (the EX44's
//!   E-cores: RFDS).
//! - The L1 data cache flushed before every entry, where L1TF lets a guest
//!   read it (Skylake).
//!
//! What is not here: SRSO and Retbleed want every `ret` the host has
//! through an untrained return thunk -- a compiler's job, for the whole
//! kernel -- or an IBPB at every exit; branch history cleared by the
//! software sequence on a CPU with enhanced IBRS that is open to BHI and has
//! no BHI_DIS_S (Ice Lake, Tiger Lake -- none of the machines here), which
//! [`report`] names; a guest is offered no speculation control of its own
//! (IBRS, STIBP, SSBD, IBPB), whose MSRs would have to be switched with it;
//! and L1TF with hyper-threading, which the flush does not reach -- the
//! sibling's guest reads the cache meanwhile -- and only whole cores per
//! guest and the host's own work kept off them would.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::percpu::ConstInit;

use super::{cpu, ENTRIES};

/// How many return addresses a run stub puts in the return stack after an
/// exit: as many as any CPU's holds (KVM's RSB_CLEAR_LOOPS).
pub(super) const RSB_ENTRIES: u32 = 32;

const MSR_PRED_CMD: u32 = 0x49;
const PRED_CMD_IBPB: u64 = 1 << 0;
pub(super) const MSR_SPEC_CTRL: u32 = 0x48;
const SPEC_CTRL_IBRS: u64 = 1 << 0;
/// BHI_DIS_S: branch history not used for predictions at CPL 0.
const SPEC_CTRL_BHI_DIS_S: u64 = 1 << 10;
const MSR_ARCH_CAPABILITIES: u32 = 0x10A;
const MSR_FLUSH_CMD: u32 = 0x10B;
const FLUSH_CMD_L1D: u64 = 1 << 0;
/// CPUID 0x8000_0008 EBX: IBPB, through PRED_CMD -- AMD's.
const LEAF_EXT_SIZES: u32 = 0x8000_0008;
const EBX_IBPB: u32 = 1 << 12;
/// CPUID 0x8000_0021 EAX: Automatic IBRS, EFER bit 21.
const LEAF_EXT_FEATURES2: u32 = 0x8000_0021;
const EAX_AUTO_IBRS: u32 = 1 << 8;
const MSR_EFER: u32 = 0xC000_0080;
const EFER_AIBRSE: u64 = 1 << 21;
/// CPUID 7: the structured features -- subleaf 0's EBX and EDX, subleaf 2's
/// EDX, and in subleaf 0's EAX the last subleaf there is.
const LEAF_STRUCTURED: u32 = 7;
const EBX_HLE: u32 = 1 << 4;
const EBX_RTM: u32 = 1 << 11;
/// MD_CLEAR: VERW clears the CPU's buffers.
const EDX_MD_CLEAR: u32 = 1 << 10;
/// IBRS and IBPB, IA32_SPEC_CTRL and IA32_PRED_CMD -- Intel's.
const EDX_IBRS_IBPB: u32 = 1 << 26;
/// IA32_FLUSH_CMD's L1D flush.
const EDX_L1D_FLUSH: u32 = 1 << 28;
/// IA32_ARCH_CAPABILITIES.
const EDX_ARCH_CAPABILITIES: u32 = 1 << 29;
const SUBLEAF_BHI: u32 = 2;
/// Subleaf 2 EDX: BHI_DIS_S in IA32_SPEC_CTRL.
const EDX_BHI_CTRL: u32 = 1 << 4;

/* IA32_ARCH_CAPABILITIES: what a CPU is not open to, and what it has. */
const CAP_RDCL_NO: u64 = 1 << 0;
const CAP_IBRS_ALL: u64 = 1 << 1;
const CAP_SKIP_L1DFL_VMENTRY: u64 = 1 << 3;
const CAP_MDS_NO: u64 = 1 << 5;
const CAP_TAA_NO: u64 = 1 << 8;
const CAP_SBDR_SSDP_NO: u64 = 1 << 13;
const CAP_FBSDP_NO: u64 = 1 << 14;
const CAP_PSDP_NO: u64 = 1 << 15;
const CAP_FB_CLEAR: u64 = 1 << 17;
const CAP_BHI_NO: u64 = 1 << 20;
const CAP_RFDS_NO: u64 = 1 << 27;
const CAP_RFDS_CLEAR: u64 = 1 << 28;
/// The three MMIO stale data kinds a CPU may say it is not open to.
const CAP_MMIO_NO: u64 = CAP_SBDR_SSDP_NO | CAP_FBSDP_NO | CAP_PSDP_NO;

/// Whether the CPU has IBPB: CPUID's answer, the same on every CPU of the
/// machine, taken as the extension is turned on and kept for each entry --
/// asking CPUID there would be an exit of its own under a nested
/// hypervisor. Never true unless CPUID said so, which is what makes the
/// write in [`State::entering`] sound.
static IBPB: AtomicBool = AtomicBool::new(false);

/// Guests entered on a CPU that had last entered another -- or none since
/// the module was loaded -- and the barriers issued for them.
static SWITCHES: AtomicU64 = AtomicU64::new(0);
static BARRIERS: AtomicU64 = AtomicU64::new(0);

/// Whether this CPU has IBPB: AMD's CPUID bit, or Intel's.
pub fn has_ibpb() -> bool {
    cpu::cpuid(LEAF_EXT_SIZES).is_some_and(|r| r.ebx & EBX_IBPB != 0)
        || cpu::cpuid_count(LEAF_STRUCTURED, 0).is_some_and(|r| r.edx & EDX_IBRS_IBPB != 0)
}

/// Whether this CPU has Automatic IBRS.
pub fn has_auto_ibrs() -> bool {
    cpu::cpuid(LEAF_EXT_FEATURES2).is_some_and(|r| r.eax & EAX_AUTO_IBRS != 0)
}

/// IBRS in IA32_SPEC_CTRL, as the host runs with it while the extension is
/// on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ibrs {
    /// Enhanced: set once, it holds across every exit.
    Enhanced,
    /// Legacy: a barrier when written, and so written after every exit.
    EveryExit,
}

/// What a VT-x CPU's entries do for it, decided once by [`on`] from what
/// the CPU says it is open to and what it has.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Intel {
    /// IBRS on, and how; None where the CPU has no IA32_SPEC_CTRL.
    pub ibrs: Option<Ibrs>,
    /// BHI_DIS_S on beside it.
    pub bhi_dis_s: bool,
    /// Open to branch history injection, with enhanced IBRS, and no
    /// BHI_DIS_S to turn it away: the software sequence that clears the
    /// history at every exit would, and is not here.
    pub bhi_open: bool,
    /// VERW before every entry.
    pub verw: bool,
    /// The L1 data cache flushed before every entry.
    pub l1d_flush: bool,
}

/// What [`on`] did on a CPU, to undo, what that CPU's entries do, and the
/// last guest it entered: the CPU's own, in its [`super::Entries`].
pub(super) struct State {
    /// EFER.AIBRSE was set here.
    auto_ibrs: bool,
    /// IA32_SPEC_CTRL as it was, to put back.
    spec_ctrl_was: Option<u64>,
    /// The value it holds while the extension is on, written again after
    /// every exit for legacy IBRS.
    spec_ctrl: u64,
    intel: Intel,
    /// The last guest entered here, by its nested table's id: None until
    /// one has since the module was loaded.
    last: Option<u64>,
}

impl ConstInit for State {
    const INIT: Self = State {
        auto_ibrs: false,
        spec_ctrl_was: None,
        spec_ctrl: 0,
        intel: Intel { ibrs: None, bhi_dis_s: false, bhi_open: false, verw: false, l1d_flush: false },
        last: None,
    };
}

/// What the CPU this runs on is to have under VT-x, from what it says of
/// itself, and where it has IA32_SPEC_CTRL, the value the MSR holds now and
/// the one that gives it that: IBRS and BHI_DIS_S over what is there
/// already. Reads IA32_ARCH_CAPABILITIES and IA32_SPEC_CTRL where CPUID
/// says they exist, and writes nothing.
fn intel_decide() -> (Intel, Option<(u64, u64)>) {
    let Some(features) = cpu::cpuid_count(LEAF_STRUCTURED, 0) else {
        return (Intel::default(), None);
    };
    let caps = if features.edx & EDX_ARCH_CAPABILITIES != 0 {
        /* The MSR is there where CPUID says it is. */
        unsafe { cpu::rdmsr(MSR_ARCH_CAPABILITIES) }
    } else {
        0
    };
    let bhi_ctrl = features.eax >= SUBLEAF_BHI
        && cpu::cpuid_count(LEAF_STRUCTURED, SUBLEAF_BHI).is_some_and(|r| r.edx & EDX_BHI_CTRL != 0);
    let enhanced = caps & CAP_IBRS_ALL != 0;
    let tsx = features.ebx & (EBX_HLE | EBX_RTM) != 0;
    let mds = caps & CAP_MDS_NO == 0;
    let taa = tsx && caps & CAP_TAA_NO == 0;
    let rfds = caps & CAP_RFDS_NO == 0 && caps & CAP_RFDS_CLEAR != 0;
    let mmio = caps & CAP_MMIO_NO != CAP_MMIO_NO && caps & CAP_FB_CLEAR != 0;
    let mut intel = Intel {
        verw: features.edx & EDX_MD_CLEAR != 0 && (mds || taa || rfds || mmio),
        l1d_flush: caps & (CAP_RDCL_NO | CAP_SKIP_L1DFL_VMENTRY) == 0 && features.edx & EDX_L1D_FLUSH != 0,
        bhi_open: enhanced && caps & CAP_BHI_NO == 0 && !bhi_ctrl,
        ..Intel::default()
    };
    if features.edx & EDX_IBRS_IBPB == 0 {
        return (intel, None);
    }
    intel.ibrs = Some(if enhanced { Ibrs::Enhanced } else { Ibrs::EveryExit });
    intel.bhi_dis_s = bhi_ctrl;
    /* IA32_SPEC_CTRL is there where CPUID offers IBRS. */
    let was = unsafe { cpu::rdmsr(MSR_SPEC_CTRL) };
    (intel, Some((was, was | SPEC_CTRL_IBRS | if bhi_ctrl { SPEC_CTRL_BHI_DIS_S } else { 0 })))
}

/// What the entries on the CPU this runs on are to have: IBPB asked of
/// CPUID once; under AMD-V, Automatic IBRS turned on where the CPU has it
/// and it is not on already; under VT-x (`intel`), IBRS and BHI_DIS_S set
/// where the CPU has them, and what each entry does decided.
///
/// # Safety
/// On the CPU it is for, as the extension is being turned on there; [`off`]
/// runs on the same CPU before the extension is off.
pub unsafe fn on(intel: bool) {
    IBPB.store(has_ibpb(), Ordering::Relaxed);
    ENTRIES.with(|e, _| {
        let s = &mut e.spec;
        if intel {
            let (decided, spec_ctrl) = intel_decide();
            s.intel = decided;
            if let Some((was, value)) = spec_ctrl {
                /* The MSR is there (`intel_decide` read it), and IBRS and
                 * BHI_DIS_S are bits CPUID says it takes. */
                unsafe { cpu::wrmsr(MSR_SPEC_CTRL, value) };
                s.spec_ctrl_was = Some(was);
                s.spec_ctrl = value;
            }
            return;
        }
        if !has_auto_ibrs() {
            return;
        }
        /* EFER is on every x86-64 CPU, and the bit is one CPUID says this
         * one takes. */
        let efer = unsafe { cpu::rdmsr(MSR_EFER) };
        if efer & EFER_AIBRSE == 0 {
            unsafe { cpu::wrmsr(MSR_EFER, efer | EFER_AIBRSE) };
            s.auto_ibrs = true;
        }
    });
}

/// Put back what [`on`] changed on the CPU this runs on.
///
/// # Safety
/// On the CPU `on` ran on, before its extension goes off.
pub unsafe fn off() {
    ENTRIES.with(|e, _| {
        let s = &mut e.spec;
        if core::mem::take(&mut s.auto_ibrs) {
            let efer = unsafe { cpu::rdmsr(MSR_EFER) };
            unsafe { cpu::wrmsr(MSR_EFER, efer & !EFER_AIBRSE) };
        }
        if let Some(was) = s.spec_ctrl_was.take() {
            /* A value the MSR held before `on`. */
            unsafe { cpu::wrmsr(MSR_SPEC_CTRL, was) };
        }
        s.spec_ctrl = 0;
        s.intel = Intel::default();
    });
}

impl State {
    /// The guest whose nested table has id `table` is about to be entered
    /// on this CPU, with interrupts off: an IBPB first where the last guest
    /// entered here was another -- or none this load knows of -- and the
    /// CPU has one.
    pub(super) fn entering(&mut self, table: u64) {
        if self.last != Some(table) {
            SWITCHES.fetch_add(1, Ordering::Relaxed);
            if IBPB.load(Ordering::Relaxed) {
                /* PRED_CMD exists where CPUID offers IBPB, and IBPB is all
                 * a write of this bit does. */
                unsafe { cpu::wrmsr(MSR_PRED_CMD, PRED_CMD_IBPB) };
                BARRIERS.fetch_add(1, Ordering::Relaxed);
            }
            self.last = Some(table);
        }
    }

    /// What a VT-x entry on this CPU is to do around the world switch.
    pub(super) fn vmx_entry(&self) -> VmxEntry {
        VmxEntry {
            verw: self.intel.verw,
            spec_ctrl: (self.intel.ibrs == Some(Ibrs::EveryExit)).then_some(self.spec_ctrl),
            l1d_flush: self.intel.l1d_flush,
        }
    }
}

/// What a VT-x entry on a CPU does for its speculation, as [`on`] decided
/// it there: made only by [`State::vmx_entry`], so that what it says of the
/// CPU is what CPUID said.
#[derive(Clone, Copy)]
pub(crate) struct VmxEntry {
    verw: bool,
    spec_ctrl: Option<u64>,
    l1d_flush: bool,
}

impl VmxEntry {
    /// VERW just before the entry.
    pub(crate) fn verw(&self) -> bool {
        self.verw
    }

    /// The IA32_SPEC_CTRL value to write right after the exit, before the
    /// host's first indirect branch: legacy IBRS's barrier.
    pub(crate) fn spec_ctrl(&self) -> Option<u64> {
        self.spec_ctrl
    }

    /// The L1 data cache flushed, where L1TF lets a guest read it: the last
    /// thing before the launch stub, with interrupts off.
    pub(crate) fn flush_l1d(&self) {
        if self.l1d_flush {
            /* IA32_FLUSH_CMD is there where CPUID offers the flush (`on`
             * asked), and a flush of L1D is all its bit does. */
            unsafe { cpu::wrmsr(MSR_FLUSH_CMD, FLUSH_CMD_L1D) };
        }
    }
}

/// Whether IA32_SPEC_CTRL holds, on the CPU this runs on, what [`on`] set
/// there: None where it set nothing -- AMD-V, a CPU without the MSR, the
/// extension off -- and otherwise whether every bit it set is still set.
/// Asked of the register, so that a CPU it was undone on is said rather
/// than assumed: nothing else in this kernel writes it.
pub fn held() -> Option<bool> {
    ENTRIES
        .with(|e, _| {
            let s = &e.spec;
            s.spec_ctrl_was.map(|_| {
                /* The MSR is there: `on` wrote it. */
                let now = unsafe { cpu::rdmsr(MSR_SPEC_CTRL) };
                now & s.spec_ctrl == s.spec_ctrl
            })
        })
        .flatten()
}

/// Guests entered on a CPU after another, and the IBPBs issued for them;
/// whether the CPU has IBPB at all; whether Automatic IBRS is on here; and
/// what the entries of a VT-x CPU this runs on do.
pub struct Report {
    pub switches: u64,
    pub barriers: u64,
    pub ibpb: bool,
    pub auto_ibrs: bool,
    pub intel: Intel,
}

pub fn report() -> Report {
    let auto = has_auto_ibrs() && unsafe { cpu::rdmsr(MSR_EFER) } & EFER_AIBRSE != 0;
    Report {
        switches: SWITCHES.load(Ordering::Relaxed),
        barriers: BARRIERS.load(Ordering::Relaxed),
        ibpb: IBPB.load(Ordering::Relaxed),
        auto_ibrs: auto,
        intel: ENTRIES.with(|e, _| e.spec.intel).unwrap_or_default(),
    }
}
