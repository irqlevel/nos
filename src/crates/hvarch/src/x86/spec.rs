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
//! Under AMD-V, decided for each CPU as the extension is turned on there,
//! from its family and what CPUID says of it, with Linux's list of which
//! families are open to what as the reference:
//!
//! - Automatic IBRS (Zen 4 and later) turned on while the extension is:
//!   the host's indirect branches, at CPL 0, are then not steered by what a
//!   guest trained.
//! - An IBPB right after every exit, before the host's first branch it
//!   could steer and its first `ret`, where a guest could otherwise steer
//!   them: BTI on a CPU without automatic IBRS -- this kernel has no
//!   retpolines -- Retbleed (branch type confusion: Zen 1 and 2, and the two
//!   families before), and SRSO (Zen 1 to 5) where the IBPB flushes return
//!   predictions too, which Zen 1 and 2's does and a later one's does where
//!   CPUID says IBPB_BRTYPE. What a guest trained is then gone before the
//!   host runs on it, which is Linux's `spec_rstack_overflow=ibpb-vmexit`
//!   and `retbleed=ibpb` for a host whose only way in from a guest is an
//!   exit. The IBPB between two guests is that IBPB's then.
//! - STIBP in IA32_SPEC_CTRL while the extension is on, where a core has
//!   two threads: what a guest on one trains does not steer the host's
//!   indirect branches on the other -- the IBPB at an exit clears this
//!   thread's training, not the sibling's, which goes on. A guest runs with
//!   it too, on a CPU without V_SPEC_CTRL (which would give it the VMCB's).
//! - HLT and MWAIT exits, whatever `hv` asks for (`svm::HOST_MISC1`): a
//!   thread that idles switches its core between its SMT modes, and on Zen 1
//!   and 2 a `ret` on the other thread may then be predicted from what this
//!   one left in the return stack (SMT_RSB) -- idled in the host, after the
//!   exit's refill, it left only the host's.
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
//! What is not here: the cheaper way to the AMD-V IBPB's end, every `ret`
//! the host has through an untrained return thunk and its indirect branches
//! through retpolines -- a compiler's job, for the whole kernel, C++, Rust
//! and the modules; a Zen 3 or 4 whose microcode has IBPB_BRTYPE and whose
//! CPUID does not say so, which Linux finds by writing PRED_CMD's SBPB and
//! this takes for open to SRSO; Zen 5's cheaper SRSO control (BpSpecReduce),
//! and TSA (Zen 3 and 4), which wants VERW before every entry; branch
//! history cleared by the software sequence on a CPU with enhanced IBRS
//! that is open to BHI and has no BHI_DIS_S (Ice Lake, Tiger Lake -- none
//! of the machines here); and every CPU open to something with nothing here
//! to close it is named by [`report`]. A guest is offered no speculation
//! control of its own (IBRS, STIBP, SSBD, IBPB), whose MSRs would have to be
//! switched with it; and L1TF with hyper-threading, which the flush does not
//! reach -- the sibling's guest reads the cache meanwhile -- and only whole
//! cores per guest and the host's own work kept off them would.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::percpu::{ConstInit, LocalCounter, PerCpu};

use super::{cpu, ENTRIES};

/// How many return addresses a run stub puts in the return stack after an
/// exit: as many as any CPU's holds (KVM's RSB_CLEAR_LOOPS).
pub(super) const RSB_ENTRIES: u32 = 32;

pub(super) const MSR_PRED_CMD: u32 = 0x49;
pub(super) const PRED_CMD_IBPB: u64 = 1 << 0;
pub(super) const MSR_SPEC_CTRL: u32 = 0x48;
const SPEC_CTRL_IBRS: u64 = 1 << 0;
const SPEC_CTRL_STIBP: u64 = 1 << 1;
/// BHI_DIS_S: branch history not used for predictions at CPL 0.
const SPEC_CTRL_BHI_DIS_S: u64 = 1 << 10;
const MSR_FLUSH_CMD: u32 = 0x10B;
const FLUSH_CMD_L1D: u64 = 1 << 0;
/// CPUID 0x8000_0008 EBX: IBPB, through PRED_CMD, and STIBP, through
/// IA32_SPEC_CTRL -- AMD's.
const LEAF_EXT_SIZES: u32 = 0x8000_0008;
const EBX_IBPB: u32 = 1 << 12;
const EBX_STIBP: u32 = 1 << 15;
/// CPUID 0x8000_0021 EAX: Automatic IBRS, EFER bit 21; an IBPB that flushes
/// the branch types and the return predictions as well; and not open to
/// SRSO.
const LEAF_EXT_FEATURES2: u32 = 0x8000_0021;
const EAX_AUTO_IBRS: u32 = 1 << 8;
const EAX_IBPB_BRTYPE: u32 = 1 << 28;
const EAX_SRSO_NO: u32 = 1 << 29;
/// CPUID 0x8000_001E EBX[15:8]: a core's threads, less one.
const LEAF_AMD_APIC_ID: u32 = 0x8000_001E;
const AMD_THREADS_SHIFT: u32 = 8;
const AMD_THREADS_MASK: u32 = 0xFF;
/// CPUID 1 EAX: the family, the base one in bits 11:8, and where that is
/// 0xF the extended one in bits 27:20 added to it.
const LEAF_FEATURES: u32 = 1;
const FAMILY_SHIFT: u32 = 8;
const FAMILY_MASK: u32 = 0xF;
const EXT_FAMILY_SHIFT: u32 = 20;
const EXT_FAMILY_MASK: u32 = 0xFF;
const FAMILY_EXTENDED: u32 = 0xF;
/// AMD's families 15h and 16h (Bulldozer to Jaguar), 17h (Zen to Zen 2),
/// and Hygon's 18h (Dhyana, a Zen).
const FAMILY_15H: u32 = 0x15;
const FAMILY_16H: u32 = 0x16;
const FAMILY_ZEN: u32 = 0x17;
const FAMILY_HYGON: u32 = 0x18;
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

/// A count of its own for each CPU, a cache line apiece: one added to at
/// every exit, which every CPU running guests makes.
#[repr(align(64))]
struct Line(LocalCounter);

impl ConstInit for Line {
    const INIT: Self = Line(LocalCounter::new());
}

/// The IBPBs issued right after an exit, on each CPU.
static EXIT_BARRIERS: PerCpu<Line> = PerCpu::new();

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

/// What an AMD-V CPU is open to, and what its entries do about it, decided
/// once by [`on`] from its family and what CPUID says of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Amd {
    /// Open to BTI: no automatic IBRS, and no retpolines in this kernel.
    pub bti: bool,
    /// Open to Retbleed, branch type confusion: AMD's families 15h to 17h,
    /// Hygon's 18h.
    pub retbleed: bool,
    /// Open to SRSO: from family 17h on, where CPUID does not say SRSO_NO.
    pub srso: bool,
    /// An IBPB right after every exit, which closes what of the three above
    /// it can.
    pub ibpb_exit: bool,
    /// STIBP on, a core having two threads.
    pub stibp: bool,
    /// Open to BTI or Retbleed with no IBPB to close it.
    pub branches_open: bool,
    /// Open to SRSO with no IBPB that flushes return predictions: no
    /// IBPB_BRTYPE.
    pub srso_open: bool,
    /// A core of two threads, and no STIBP to keep them apart.
    pub smt_open: bool,
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
    amd: Amd,
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
        amd: Amd {
            bti: false,
            retbleed: false,
            srso: false,
            ibpb_exit: false,
            stibp: false,
            branches_open: false,
            srso_open: false,
            smt_open: false,
        },
        last: None,
    };
}

/// The family of the CPU this runs on: CPUID 1's, the extended one added
/// where the base one is 0xF.
fn family() -> u32 {
    let eax = cpu::cpuid(LEAF_FEATURES).map_or(0, |r| r.eax);
    let base = (eax >> FAMILY_SHIFT) & FAMILY_MASK;
    if base == FAMILY_EXTENDED {
        base + ((eax >> EXT_FAMILY_SHIFT) & EXT_FAMILY_MASK)
    } else {
        base
    }
}

/// What the CPU this runs on is open to under AMD-V, and what its entries
/// do about it -- Linux's list (`cpu_vuln_blacklist`) for which families
/// are open to what -- and where it is to have STIBP, the value
/// IA32_SPEC_CTRL holds now and the one that gives it that. Reads
/// IA32_SPEC_CTRL where CPUID says it takes STIBP, and writes nothing.
fn amd_decide() -> (Amd, Option<(u64, u64)>) {
    let vendor = cpu::vendor_id();
    let amd = vendor == *b"AuthenticAMD";
    let hygon = vendor == *b"HygonGenuine";
    let family = family();
    let sizes = cpu::cpuid(LEAF_EXT_SIZES).map_or(0, |r| r.ebx);
    let features2 = cpu::cpuid(LEAF_EXT_FEATURES2).map_or(0, |r| r.eax);
    let threads = cpu::cpuid(LEAF_AMD_APIC_ID).map_or(1, |r| ((r.ebx >> AMD_THREADS_SHIFT) & AMD_THREADS_MASK) + 1);

    let ibpb = sizes & EBX_IBPB != 0;
    /* Zen 1 and 2: their IBPB flushes the branch types already, which a
     * later family's does where CPUID says IBPB_BRTYPE (Linux's
     * `cpu_has_ibpb_brtype_microcode`). */
    let zen12 = (amd && family == FAMILY_ZEN) || (hygon && family == FAMILY_HYGON);
    let brtype = zen12 || features2 & EAX_IBPB_BRTYPE != 0;
    let bti = features2 & EAX_AUTO_IBRS == 0;
    let retbleed = zen12 || (amd && (family == FAMILY_15H || family == FAMILY_16H));
    let srso = (zen12 || (amd && family > FAMILY_ZEN)) && features2 & EAX_SRSO_NO == 0;
    let smt = threads > 1;
    let stibp = smt && sizes & EBX_STIBP != 0;
    let decided = Amd {
        bti,
        retbleed,
        srso,
        ibpb_exit: ibpb && (bti || retbleed || (srso && brtype)),
        stibp,
        branches_open: (bti || retbleed) && !ibpb,
        srso_open: srso && !(ibpb && brtype),
        smt_open: smt && !stibp,
    };
    if !stibp {
        return (decided, None);
    }
    /* IA32_SPEC_CTRL is there where CPUID offers STIBP. */
    let was = unsafe { cpu::rdmsr(MSR_SPEC_CTRL) };
    (decided, Some((was, was | SPEC_CTRL_STIBP)))
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
    let caps = cpu::arch_capabilities().unwrap_or(0);
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
/// CPUID once; under AMD-V, what the CPU is open to and what its entries do
/// about it decided, STIBP set where a core has two threads, and Automatic
/// IBRS turned on where the CPU has it and it is not on already; under VT-x
/// (`intel`), IBRS and BHI_DIS_S set where the CPU has them, and what each
/// entry does decided.
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
        let (decided, spec_ctrl) = amd_decide();
        s.amd = decided;
        if let Some((was, value)) = spec_ctrl {
            /* The MSR is there (`amd_decide` read it), and STIBP is a bit
             * CPUID says it takes. */
            unsafe { cpu::wrmsr(MSR_SPEC_CTRL, value) };
            s.spec_ctrl_was = Some(was);
            s.spec_ctrl = value;
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
        s.amd = Amd::default();
    });
}

impl State {
    /// The guest whose nested table has id `table` is about to be entered
    /// on this CPU, with interrupts off: an IBPB first where the last guest
    /// entered here was another -- or none this load knows of -- and the
    /// CPU has one, unless the IBPB after every exit here is already
    /// between the two: the host trains nothing a guest could use.
    pub(super) fn entering(&mut self, table: u64) {
        if self.last != Some(table) {
            SWITCHES.fetch_add(1, Ordering::Relaxed);
            if IBPB.load(Ordering::Relaxed) && !self.amd.ibpb_exit {
                /* PRED_CMD exists where CPUID offers IBPB, and IBPB is all
                 * a write of this bit does. */
                unsafe { cpu::wrmsr(MSR_PRED_CMD, PRED_CMD_IBPB) };
                BARRIERS.fetch_add(1, Ordering::Relaxed);
            }
            self.last = Some(table);
        }
    }

    /// What an AMD-V entry on this CPU is to do around the world switch.
    pub(super) fn svm_entry(&self) -> SvmEntry {
        SvmEntry { ibpb_exit: self.amd.ibpb_exit }
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

/// What an AMD-V entry on a CPU does for its speculation, as [`on`] decided
/// it there: made only by [`State::svm_entry`], so that what it says of the
/// CPU is what CPUID said.
#[derive(Clone, Copy)]
pub(crate) struct SvmEntry {
    ibpb_exit: bool,
}

impl SvmEntry {
    /// An IBPB right after the exit: given only where CPUID offers IBPB
    /// (`amd_decide`), so PRED_CMD is there to write.
    pub(crate) fn ibpb_exit(&self) -> bool {
        self.ibpb_exit
    }

    /// One IBPB issued after an exit on `cpu`, counted.
    pub(crate) fn count_exit_barrier(&self, cpu: u32) {
        if let Some(line) = EXIT_BARRIERS.get(cpu as usize) {
            line.0.add(1);
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
/// there: None where it set nothing -- an AMD-V CPU whose cores have one
/// thread, a CPU without the MSR, the extension off -- and otherwise whether
/// every bit it set is still set.
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
/// the IBPBs issued after exits, on every CPU; whether the CPU has IBPB at
/// all; whether Automatic IBRS is on here; and what the entries of the CPU
/// this runs on do, under VT-x or AMD-V.
pub struct Report {
    pub switches: u64,
    pub barriers: u64,
    pub exit_barriers: u64,
    pub ibpb: bool,
    pub auto_ibrs: bool,
    pub intel: Intel,
    pub amd: Amd,
}

pub fn report() -> Report {
    let auto = has_auto_ibrs() && unsafe { cpu::rdmsr(MSR_EFER) } & EFER_AIBRSE != 0;
    Report {
        switches: SWITCHES.load(Ordering::Relaxed),
        barriers: BARRIERS.load(Ordering::Relaxed),
        /* CPUID's answer, not the flag `on` keeps for the entries: before
         * any CPU has the extension on, that says no for a CPU that has it */
        ibpb: has_ibpb(),
        auto_ibrs: auto,
        exit_barriers: EXIT_BARRIERS.iter().map(|line| line.0.get() as u64).sum(),
        intel: ENTRIES.with(|e, _| e.spec.intel).unwrap_or_default(),
        amd: ENTRIES.with(|e, _| e.spec.amd).unwrap_or_default(),
    }
}
