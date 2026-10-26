//! A guest's MSRs that neither `vmrun` nor `vmlaunch` switches and that its
//! own instructions read without an exit: IA32_TSC_AUX, which RDTSCP and
//! RDPID return. Kept per guest CPU ([`GuestMsrs`]) and put into the CPU
//! before an entry where the CPU holds another value -- which a record per
//! CPU says ([`LOADED`]): nothing in this kernel writes TSC_AUX or reads it,
//! so between a guest's exit and the next entry it holds what the last entry
//! put there.
//!
//! Loaded on every CPU that has the MSR, whether the guest is offered RDTSCP
//! and RDPID or not: AMD-V cannot stop a guest at RDPID, and one running it
//! behind its CPUID's back reads its own TSC_AUX -- 0, until it is offered
//! the instructions and writes one -- not whatever the CPU last held, the
//! host's or another guest's.
//!
//! What a CPU held when the extension went on for it is put back when it
//! goes off: a CPU the extension goes off for is as it booted.
//!
//! And what a guest CPU set of its speculation controls -- its
//! IA32_SPEC_CTRL, which `super::spec` makes the CPU's over the host's for
//! its run, and the IBPBs it asked for by IA32_PRED_CMD, which `super::spec`
//! performs before its next entry on any CPU it may have trained -- by a
//! number of its own, no other guest CPU's ever ([`GuestMsrs::id`]).

use core::sync::atomic::{AtomicU64, Ordering};

use kcore::consts::MAX_CPUS;

use super::cpu;
use super::spec;

pub const MSR_TSC_AUX: u32 = 0xC000_0103;
/// TSC_AUX's bits, the low 32: a write of any other is a #GP.
pub const TSC_AUX_BITS: u64 = 0xFFFF_FFFF;
/// CPUID 0x8000_0001 EDX: RDTSCP; leaf 7 ECX: RDPID. Either says the CPU has
/// TSC_AUX.
const EDX_RDTSCP: u32 = 1 << 27;
const ECX_RDPID: u32 = 1 << 22;

/// Per CPU: what its TSC_AUX holds -- [`ABSENT`] where it has no TSC_AUX or
/// the extension is off there. Written by `on`, `off` and the entries on
/// that CPU, each with interrupts off, and read by nothing else.
static LOADED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(ABSENT) }; MAX_CPUS];
/// Per CPU: what `on` found in TSC_AUX, for `off` to put back.
static FOUND: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(ABSENT) }; MAX_CPUS];
/// No value a TSC_AUX holds: its bits are the low 32.
const ABSENT: u64 = u64::MAX;

/// Whether the CPU this runs on has IA32_TSC_AUX.
fn has_tsc_aux() -> bool {
    cpu::cpuid(0x8000_0001).is_some_and(|r| r.edx & EDX_RDTSCP != 0)
        || cpu::cpuid_count(7, 0).is_some_and(|r| r.ecx & ECX_RDPID != 0)
}

/// Note what the CPU this runs on holds in TSC_AUX, where it has one, for
/// the entries to load over and [`off`] to put back.
///
/// # Safety
/// On the CPU it is for, with interrupts off, as the extension is being
/// turned on there; `off` runs on the same CPU before the extension is off.
pub unsafe fn on() {
    let cpu = kcore::cpu::id() as usize;
    let (Some(loaded), Some(found)) = (LOADED.get(cpu), FOUND.get(cpu)) else { return };
    let now = if has_tsc_aux() {
        /* The MSR is there: CPUID said so. */
        let value = unsafe { cpu::rdmsr(MSR_TSC_AUX) };
        value & TSC_AUX_BITS
    } else {
        ABSENT
    };
    found.store(now, Ordering::Relaxed);
    loaded.store(now, Ordering::Relaxed);
}

/// Put back what [`on`] found in TSC_AUX, on the CPU this runs on.
///
/// # Safety
/// On the CPU `on` ran on, with interrupts off, before its extension goes
/// off.
pub unsafe fn off() {
    let cpu = kcore::cpu::id() as usize;
    let (Some(loaded), Some(found)) = (LOADED.get(cpu), FOUND.get(cpu)) else { return };
    let was = found.swap(ABSENT, Ordering::Relaxed);
    let now = loaded.swap(ABSENT, Ordering::Relaxed);
    if was != ABSENT && now != was {
        /* The MSR is there (`on` read it), and the value is one it held. */
        unsafe { cpu::wrmsr(MSR_TSC_AUX, was) };
    }
}

/// The number the next guest CPU made is given.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A guest CPU's MSRs that the extension does not switch: its TSC_AUX and
/// its IA32_SPEC_CTRL, 0 out of reset -- INIT leaves both -- and the IBPBs
/// it asked for.
pub struct GuestMsrs {
    tsc_aux: u64,
    spec_ctrl: u64,
    /// How many IBPBs it asked for, and how many of those an IBPB was
    /// between before an entry of its -- the last entry's count, there.
    ibpbs: u64,
    answered: u64,
    /// The IA32_SPEC_CTRL its last entry ran under -- its bits over the
    /// host's -- or None where the CPU has none, or before its first.
    ran_with: Option<u64>,
    id: u64,
}

impl GuestMsrs {
    pub fn new() -> Self {
        Self {
            tsc_aux: 0,
            spec_ctrl: 0,
            ibpbs: 0,
            answered: 0,
            ran_with: None,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }
    }

    /// A number no other guest CPU has had: which one a CPU last entered,
    /// for the IBPB between two (`super::spec::State::entering`). One never
    /// given twice, so a guest CPU dropped and another made in its memory
    /// cannot be taken for it.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn tsc_aux(&self) -> u64 {
        self.tsc_aux
    }

    /// The guest's IA32_SPEC_CTRL: the bits it set.
    pub fn spec_ctrl(&self) -> u64 {
        self.spec_ctrl
    }

    /// How many IBPBs it asked for.
    pub fn ibpbs(&self) -> u64 {
        self.ibpbs
    }

    /// How many of those an IBPB was between before an entry of its, on the
    /// CPU it entered: all of them, once it has run again.
    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// Make `tsc_aux` and `spec_ctrl` the guest's TSC_AUX and IA32_SPEC_CTRL,
    /// and where `ibpb`, ask for an IBPB -- done before its next entry on any
    /// CPU it may have trained (`spec::State::entering`) -- for a WRMSR the
    /// policy performed. False, and nothing changed, where any of it is what
    /// a CPU refuses: TSC_AUX with a bit above its 32, SPEC_CTRL with a bit
    /// the CPU has no control of (`spec::guest_controls`), an IBPB where it
    /// has none -- the #GP a CPU gives, and each a value an entry's write
    /// would fault on in the host.
    pub fn set(&mut self, tsc_aux: u64, spec_ctrl: u64, ibpb: bool) -> bool {
        let controls = spec::guest_controls();
        if tsc_aux & !TSC_AUX_BITS != 0 || spec_ctrl & !controls.spec_ctrl != 0 || (ibpb && controls.pred_cmd == 0) {
            return false;
        }
        self.tsc_aux = tsc_aux;
        self.spec_ctrl = spec_ctrl;
        if ibpb {
            self.ibpbs = self.ibpbs.wrapping_add(1);
        }
        true
    }

    /// An entry of its had a barrier before it (`spec::State::entering`):
    /// every IBPB it asked for so far is answered.
    pub(super) fn barrier(&mut self) {
        self.answered = self.ibpbs;
    }

    /// The IA32_SPEC_CTRL its last entry ran under: for a check, which
    /// cannot ask the CPU while the guest runs.
    pub fn ran_with(&self) -> Option<u64> {
        self.ran_with
    }

    /// Its entry ran under `value` (`spec::State::load_guest`, or its VMCB's).
    pub(super) fn ran(&mut self, value: Option<u64>) {
        self.ran_with = value;
    }

    /// Put the guest's TSC_AUX into CPU `cpu` before its entry, unless the
    /// CPU holds it already or has none.
    ///
    /// # Safety
    /// On `cpu`, the extension on for it, interrupts off from here to the
    /// entry.
    pub unsafe fn load(&self, cpu: u32) {
        let Some(loaded) = LOADED.get(cpu as usize) else { return };
        let now = loaded.load(Ordering::Relaxed);
        if now != ABSENT && now != self.tsc_aux {
            /* The MSR is there (`on` read it), and `set` took only values of
             * its bits. */
            unsafe { cpu::wrmsr(MSR_TSC_AUX, self.tsc_aux) };
            loaded.store(self.tsc_aux, Ordering::Relaxed);
        }
    }
}
