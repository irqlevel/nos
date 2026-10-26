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

use core::sync::atomic::{AtomicU64, Ordering};

use kcore::consts::MAX_CPUS;

use super::cpu;

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

/// A guest CPU's MSRs that the extension does not switch: its TSC_AUX, 0
/// out of reset -- INIT leaves it.
pub struct GuestMsrs {
    tsc_aux: u64,
}

impl GuestMsrs {
    pub const fn new() -> Self {
        Self { tsc_aux: 0 }
    }

    pub fn tsc_aux(&self) -> u64 {
        self.tsc_aux
    }

    /// Make `value` the guest's TSC_AUX, for a WRMSR of it the policy
    /// performs: false, and nothing changed, for one with a bit above the
    /// low 32, the #GP a CPU gives -- and a value an entry's write of the MSR
    /// would fault on in the host.
    pub fn set_tsc_aux(&mut self, value: u64) -> bool {
        if value & !TSC_AUX_BITS != 0 {
            return false;
        }
        self.tsc_aux = value;
        true
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
            /* The MSR is there (`on` read it), and `set_tsc_aux` took only
             * values of its bits. */
            unsafe { cpu::wrmsr(MSR_TSC_AUX, self.tsc_aux) };
            loaded.store(self.tsc_aux, Ordering::Relaxed);
        }
    }
}
