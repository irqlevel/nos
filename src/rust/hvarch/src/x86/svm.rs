//! AMD-V: what the CPU says about it, and turning it on for one CPU.
//!
//! SVM is the backend written first, for a reason that is about the dev loop
//! rather than the hardware: QEMU's TCG emulates SVM, nested paging
//! included, and emulates no VMX at all -- so on a Mac, where the x86 kernel
//! runs under TCG, AMD-V is the only extension a guest can be brought up on
//! at all. It is also the simpler of the two: the VMCB is a plain structure
//! in memory, with none of `vmread`/`vmwrite`'s ceremony, and the one AMD
//! machine this kernel runs on (the Hetzner AX41) is a real target.

use core::arch::asm;

use super::cpu;
use crate::{Error, Result};

/* CPUID.8000_0001:ECX -- is there an SVM at all. */
pub const CPUID_EXT_FEATURES: u32 = 0x8000_0001;
pub const ECX_SVM: u32 = 1 << 2;

/* CPUID.8000_000A: the revision (EAX[7:0]), how many ASIDs (EBX), and what
   of SVM this part has (EDX). */
pub const CPUID_SVM: u32 = 0x8000_000A;

pub const NP: u32 = 1 << 0;
pub const LBR_VIRT: u32 = 1 << 1;
pub const SVM_LOCK: u32 = 1 << 2;
pub const NRIP_SAVE: u32 = 1 << 3;
pub const TSC_RATE_MSR: u32 = 1 << 4;
pub const VMCB_CLEAN: u32 = 1 << 5;
pub const FLUSH_BY_ASID: u32 = 1 << 6;
pub const DECODE_ASSISTS: u32 = 1 << 7;
pub const PAUSE_FILTER: u32 = 1 << 10;
pub const PAUSE_THRESHOLD: u32 = 1 << 12;
pub const AVIC: u32 = 1 << 13;
pub const VMSAVE_VIRT: u32 = 1 << 15;
pub const VGIF: u32 = 1 << 16;
pub const VNMI: u32 = 1 << 24;

/// The features worth a line in a report, and why each matters here. The
/// ones with no note are read out and not yet used; they are in the list so
/// that a machine's report says what it could have been given.
pub const REPORTED: &[(u32, &str, &str)] = &[
    (NP, "nested paging", "guest physical addresses translated by the CPU"),
    (NRIP_SAVE, "next-RIP save", "where an intercepted instruction ends"),
    (DECODE_ASSISTS, "decode assists", ""),
    (FLUSH_BY_ASID, "flush by ASID", ""),
    (VMCB_CLEAN, "VMCB clean bits", ""),
    (VMSAVE_VIRT, "virtual VMSAVE", ""),
    (VGIF, "virtual GIF", ""),
    (AVIC, "AVIC", ""),
    (PAUSE_FILTER, "pause filter", ""),
    (SVM_LOCK, "SVM lock", ""),
];

pub const MSR_EFER: u32 = 0xC000_0080;
pub const EFER_SVME: u64 = 1 << 12;

pub const MSR_VM_CR: u32 = 0xC001_0114;
pub const VM_CR_LOCK: u64 = 1 << 3;
pub const VM_CR_SVMDIS: u64 = 1 << 4;

pub const MSR_VM_HSAVE_PA: u32 = 0xC001_0117;

/// What this CPU says about its SVM.
#[derive(Clone, Copy)]
pub struct Caps {
    /// CPUID.8000_000A:EAX[7:0] -- 1 on everything that exists.
    pub revision: u32,
    /// How many address space identifiers a guest may be given. ASID 0 is
    /// the host's, so one guest needs two.
    pub asids: u32,
    /// CPUID.8000_000A:EDX, the bits above.
    pub features: u32,
    /// `VM_CR` as it reads now: whether firmware left SVM on.
    pub vm_cr: u64,
}

impl Caps {
    /// None when the CPU has no SVM -- which is the only state in which the
    /// SVM MSRs must not be read, since reading one a CPU does not have is a
    /// #GP and this kernel's #GP handler panics.
    pub fn probe() -> Option<Self> {
        if cpu::cpuid(CPUID_EXT_FEATURES)?.ecx & ECX_SVM == 0 {
            return None;
        }
        let r = cpu::cpuid(CPUID_SVM)?;
        /* Both MSRs exist on any part that reports SVM in CPUID. */
        let vm_cr = unsafe { cpu::rdmsr(MSR_VM_CR) };
        Some(Self {
            revision: r.eax & 0xFF,
            asids: r.ebx,
            features: r.edx,
            vm_cr,
        })
    }

    #[inline]
    pub fn has(&self, feature: u32) -> bool {
        self.features & feature != 0
    }

    /// Firmware turned SVM off. `SVM_LOCK` says whether it can be turned
    /// back on with the key MSR at all -- without it, nothing but a reset
    /// and a different BIOS setting will do.
    pub fn firmware_disabled(&self) -> bool {
        self.vm_cr & VM_CR_SVMDIS != 0
    }

    /// Whether a guest can be run on this CPU, and what is missing when it
    /// cannot.
    pub fn usable(&self) -> Result<()> {
        if self.firmware_disabled() {
            return Err(Error::FirmwareDisabled);
        }
        if !self.has(NP) {
            return Err(Error::NoNestedPaging);
        }
        if self.asids < 2 {
            /* ASID 0 is the host's; a guest needs one of its own. No part
             * that exists says this, but a CPU that did would fail at the
             * first vmrun rather than here. */
            return Err(Error::EnableFailed);
        }
        Ok(())
    }
}

/// Turn SVM on for the CPU this runs on.
///
/// # Safety
/// Runs on the CPU it is turning SVM on for -- so with preemption off, or
/// from that CPU's own IPI handler -- and `host_area_phys` is a page that
/// stays allocated, and is touched by nothing else, until [`disable`] runs
/// on that same CPU. It is where `vmrun` puts the host's own state before it
/// takes the CPU away, and where `#vmexit` gets it back from: freeing it
/// under a running guest loses the host.
pub unsafe fn enable(host_area_phys: u64) -> Result<()> {
    if unsafe { cpu::rdmsr(MSR_VM_CR) } & VM_CR_SVMDIS != 0 {
        return Err(Error::FirmwareDisabled);
    }

    let efer = unsafe { cpu::rdmsr(MSR_EFER) };
    unsafe { cpu::wrmsr(MSR_EFER, efer | EFER_SVME) };
    unsafe { cpu::wrmsr(MSR_VM_HSAVE_PA, host_area_phys) };

    /* EFER.SVME reads back as zero on a CPU that would not take it, and
     * says so here rather than at the first vmrun -- which is an
     * undefined-opcode fault, in a kernel whose handler panics. The error
     * path gives back what it took: the caller is about to free that page,
     * and the CPU must not be left naming it. */
    if !enabled() {
        unsafe { cpu::wrmsr(MSR_VM_HSAVE_PA, 0) };
        return Err(Error::EnableFailed);
    }
    Ok(())
}

/// Turn it off again, on the CPU this runs on.
///
/// # Safety
/// Runs on the CPU it is turning SVM off for, and no guest is running there:
/// a `vmrun` after this faults. The host save area may be freed once this
/// has returned.
pub unsafe fn disable() {
    /* KVM's order: the address, then the enable bit. Either order has a
     * window -- SVM on with no save area, or SVM off still naming the page
     * -- and neither matters, because this runs with interrupts off and
     * nothing executes `vmrun` in it. What matters is the state on return:
     * SVM off and the page named nowhere, so the caller may free it. */
    unsafe { cpu::wrmsr(MSR_VM_HSAVE_PA, 0) };
    let efer = unsafe { cpu::rdmsr(MSR_EFER) };
    if efer & EFER_SVME == 0 {
        return;
    }

    /* GIF to 1 before SVM goes: with it clear, INIT and NMI stay blocked,
     * and `stgi` is an undefined opcode once SVME is off, so after this
     * there would be no way to set it. Nothing clears GIF yet -- there is
     * no `clgi`/`vmrun` loop -- so today this changes nothing; it is here so
     * that the off switch stays right whatever the run loop does, including
     * a teardown that interrupts it. */
    unsafe { asm!("stgi", options(nomem, nostack)) };
    unsafe { cpu::wrmsr(MSR_EFER, efer & !EFER_SVME) };
}

/// Whether SVM is on for the CPU this runs on. EFER exists on every x86-64
/// CPU -- it is how long mode was turned on -- so this is sound anywhere.
pub fn enabled() -> bool {
    let efer = unsafe { cpu::rdmsr(MSR_EFER) };
    efer & EFER_SVME != 0
}
