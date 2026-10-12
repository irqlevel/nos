//! What keeps one guest's speculation from the host and from the next guest
//! on a CPU, under AMD-V -- the part of it that is the world switch's, with
//! KVM's as the reference.
//!
//! - After every `#vmexit` the return stack is refilled before the host's
//!   first `ret`, and the guest's values cleared out of every register the
//!   run stub returns with (`svm::vmrun_stub`): a guest that left return
//!   addresses of its choosing in the stack the CPU predicts returns from
//!   would have the host's returns predicted to them, and a register still
//!   holding a guest's value is a value the host may use past a branch it
//!   has not yet resolved.
//! - Between two guests on a CPU, an indirect branch prediction barrier
//!   (IBPB): what one trained the predictors with does not steer the next.
//!   A guest's own CPUs share its nested table, whose id tells guests
//!   apart ([`entering`]), so they do not pay for one between each other.
//! - Automatic IBRS (Zen 4 and later) turned on while the extension is:
//!   the host's indirect branches, at CPL 0, are then not steered by what a
//!   guest trained.
//!
//! What is not here: SRSO and Retbleed want every `ret` the host has
//! through an untrained return thunk -- a compiler's job, for the whole
//! kernel -- or an IBPB at every exit; a guest is offered no speculation
//! control of its own (IBRS, STIBP, SSBD, IBPB), whose MSRs would have to be
//! switched with it; and Intel's half -- VERW, an L1D flush, BHI -- is
//! VT-x's, not yet done.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::percpu::{ConstInit, CpuLocal};

use super::cpu;

const MSR_PRED_CMD: u32 = 0x49;
const PRED_CMD_IBPB: u64 = 1 << 0;
/// CPUID 0x8000_0008 EBX: IBPB, through PRED_CMD.
const LEAF_EXT_SIZES: u32 = 0x8000_0008;
const EBX_IBPB: u32 = 1 << 12;
/// CPUID 0x8000_0021 EAX: Automatic IBRS, EFER bit 21.
const LEAF_EXT_FEATURES2: u32 = 0x8000_0021;
const EAX_AUTO_IBRS: u32 = 1 << 8;
const MSR_EFER: u32 = 0xC000_0080;
const EFER_AIBRSE: u64 = 1 << 21;

/// Whether the CPU has IBPB: CPUID's answer, the same on every CPU of the
/// machine, taken as the extension is turned on and kept for each entry --
/// asking CPUID there would be an exit of its own under a nested
/// hypervisor. Never true unless CPUID said so, which is what makes the
/// write in [`entering`] sound.
static IBPB: AtomicBool = AtomicBool::new(false);

/// Guests entered on a CPU that had last entered another -- or none since
/// the module was loaded -- and the barriers issued for them.
static SWITCHES: AtomicU64 = AtomicU64::new(0);
static BARRIERS: AtomicU64 = AtomicU64::new(0);

/// Whether this CPU has IBPB.
pub fn has_ibpb() -> bool {
    cpu::cpuid(LEAF_EXT_SIZES).is_some_and(|r| r.ebx & EBX_IBPB != 0)
}

/// Whether this CPU has Automatic IBRS.
pub fn has_auto_ibrs() -> bool {
    cpu::cpuid(LEAF_EXT_FEATURES2).is_some_and(|r| r.eax & EAX_AUTO_IBRS != 0)
}

/// Per CPU: whether [`on`] set EFER.AIBRSE, to take it away again.
struct AutoIbrs(bool);

impl ConstInit for AutoIbrs {
    const INIT: Self = AutoIbrs(false);
}

static AUTO_IBRS: CpuLocal<AutoIbrs> = CpuLocal::new();

/// What the entries on the CPU this runs on are to have: IBPB asked of
/// CPUID once, and Automatic IBRS turned on where the CPU has it and it is
/// not on already.
///
/// # Safety
/// On the CPU it is for, as the extension is being turned on there; [`off`]
/// runs on the same CPU before the extension is off.
pub unsafe fn on() {
    IBPB.store(has_ibpb(), Ordering::Relaxed);
    if !has_auto_ibrs() {
        return;
    }
    AUTO_IBRS.with(|set, _| {
        /* EFER is on every x86-64 CPU, and the bit is one CPUID says this
         * one takes. */
        let efer = unsafe { cpu::rdmsr(MSR_EFER) };
        if efer & EFER_AIBRSE == 0 {
            unsafe { cpu::wrmsr(MSR_EFER, efer | EFER_AIBRSE) };
            set.0 = true;
        }
    });
}

/// Put back what [`on`] changed on the CPU this runs on.
///
/// # Safety
/// On the CPU `on` ran on, before its extension goes off.
pub unsafe fn off() {
    AUTO_IBRS.with(|set, _| {
        if core::mem::take(&mut set.0) {
            let efer = unsafe { cpu::rdmsr(MSR_EFER) };
            unsafe { cpu::wrmsr(MSR_EFER, efer & !EFER_AIBRSE) };
        }
    });
}

/// The last guest each CPU entered, by its nested table's id: None until
/// one has since the module was loaded.
struct Last(Option<u64>);

impl ConstInit for Last {
    const INIT: Self = Last(None);
}

static LAST: CpuLocal<Last> = CpuLocal::new();

/// The guest whose nested table has id `table` is about to be entered on
/// the CPU this runs on, with interrupts off: an IBPB first where the last
/// guest entered there was another -- or none this load knows of -- and
/// the CPU has one. False only where `with` gives nothing: a CPU past the
/// table, which no CPU the extension is on for is, and which is not to be
/// entered without the barrier.
pub(crate) fn entering(table: u64) -> bool {
    LAST.with(|last, _| {
        if last.0 != Some(table) {
            SWITCHES.fetch_add(1, Ordering::Relaxed);
            if IBPB.load(Ordering::Relaxed) {
                /* PRED_CMD exists where CPUID offers IBPB, and IBPB is all
                 * a write of this bit does. */
                unsafe { cpu::wrmsr(MSR_PRED_CMD, PRED_CMD_IBPB) };
                BARRIERS.fetch_add(1, Ordering::Relaxed);
            }
            last.0 = Some(table);
        }
    })
    .is_some()
}

/// Guests entered on a CPU after another, and the IBPBs issued for them;
/// whether the CPU has IBPB at all; and whether Automatic IBRS is on here.
pub fn report() -> (u64, u64, bool, bool) {
    let auto = has_auto_ibrs() && unsafe { cpu::rdmsr(MSR_EFER) } & EFER_AIBRSE != 0;
    (SWITCHES.load(Ordering::Relaxed), BARRIERS.load(Ordering::Relaxed), IBPB.load(Ordering::Relaxed), auto)
}
