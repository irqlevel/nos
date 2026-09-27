//! What a guest is told when it asks the CPU about itself (CPUID) and when
//! it reads or writes a model-specific register (MSR).
//!
//! Both are intercepted for every guest -- a CPUID the host did not shape
//! could promise a feature this hypervisor cannot back, and an MSR reaching
//! the CPU directly is the host's own hardware. So a guest sees a CPU cut
//! down to what is emulated here: no XSAVE and so no AVX (the state switch
//! around `vmrun` moves only the FXSAVE registers), no paravirtualisation,
//! no virtualization extension of its own -- and a local APIC only in a
//! guest of more than one CPU, where it is an x2APIC (`lapic`) with no
//! TSC-deadline timer. What is left is enough to decompress and start a
//! 64-bit kernel, and its other CPUs: long mode, NX, SYSCALL, the SSE line,
//! the PAT, the APIC and the plain arithmetic features.
//!
//! Each of a guest's CPUs is a package of its own, one core and one thread,
//! its APIC ID its number: leaf 1 says so, and the leaves that would say
//! otherwise -- the topology ones -- are blank. The CPUs are listed by the
//! MP table the loader writes (`linux`), with the same IDs. A guest of one
//! CPU is the PC it always was: no APIC, and no MP table -- which any kernel
//! boots, where a firmware-enabled x2APIC listed by an MP table takes Linux
//! 6.6 or later (an older one reads its APIC ID through the xAPIC's page
//! before it has switched to x2APIC's MSRs).
//!
//! The system MSRs -- EFER, the PAT, the segment bases, the SYSCALL and
//! SYSENTER registers -- are the guest's own state, and the VMCB's save area
//! is where they live; a read or a write of one is served from there. Every
//! other MSR reads as zero and swallows a write, which is what a guest's
//! `rdmsr_safe`/`wrmsr_safe` probes expect and what keeps a feature the
//! guest cannot really use from doing anything when it tries.

use hvarch::x86::cpu;
use hvarch::x86::svm::vmcb::Save;
use hvarch::x86::svm::GuestRegs;

/* CPUID leaves. */
const LEAF_FEATURES: u32 = 1;
const LEAF_CACHE: u32 = 4;
const LEAF_POWER: u32 = 6;
const LEAF_STRUCTURED: u32 = 7;
const LEAF_TOPOLOGY: u32 = 0xB;
const LEAF_XSTATE: u32 = 0xD;
const LEAF_TSC: u32 = 0x15;
const LEAF_TOPOLOGY_V2: u32 = 0x1F;
const LEAF_HYPERVISOR_BASE: u32 = 0x4000_0000;
const LEAF_HYPERVISOR_END: u32 = 0x4000_00FF;
const LEAF_EXT_FEATURES: u32 = 0x8000_0001;
const LEAF_EXT_POWER: u32 = 0x8000_0007;
const LEAF_EXT_ADDRESS: u32 = 0x8000_0008;
const LEAF_SVM: u32 = 0x8000_000A;
const LEAF_EXT_APIC_ID: u32 = 0x8000_001E;
const LEAF_ENCRYPTION: u32 = 0x8000_001F;

/* Each of the guest's CPUs is a package of its own, with its own APIC ID,
 * whatever host CPU its vCPU runs on: leaf 1 EBX says so in its top two
 * bytes -- the initial APIC ID, and the logical processors in the package --
 * leaf 4 in its core and sharing counts, and leaf 0x80000008 ECX in the core
 * count and the APIC ID's width. The host's numbers there are another
 * machine's: a Linux guest took CPU 3's APIC ID for its own ("APIC ID
 * mismatch"). */
const LEAF1_EBX_KEEP: u32 = 0x0000_FFFF;
const LEAF1_EBX_ONE_CPU: u32 = 1 << 16;
const LEAF1_EBX_APIC_ID_SHIFT: u32 = 24;
const LEAF4_EAX_KEEP: u32 = 0x0000_3FFF;
/// Leaf 1: the APIC (EDX bit 9) and x2APIC mode (ECX bit 21), both
/// emulated (`lapic`), set whatever the host has.
const LEAF1_EDX_APIC: u32 = 1 << 9;
const LEAF1_ECX_X2APIC: u32 = 1 << 21;
/// Leaf 6 EAX bit 2, ARAT: the APIC timer runs in every C-state. It does --
/// it is the host's clock -- and without the bit a guest looks for another
/// timer to wake it from a deep sleep, which it has none of.
const LEAF6_EAX_ARAT: u32 = 1 << 2;
/// Leaf 0x15 as a guest with an APIC is told it: the "crystal" the APIC
/// timer counts is the emulated APIC's bus, 1 GHz (`lapic`), and the TSC's
/// ratio to it is the host's TSC -- in kHz, over a denominator of a million
/// kHz. Linux on Intel takes the APIC timer's rate from this leaf and skips
/// measuring it; passed through, the leaf names the host's crystal, tens of
/// MHz, and the guest's timer would fire tens of times too soon.
const LEAF15_CRYSTAL_HZ: u32 = 1_000_000_000;
const LEAF15_DENOMINATOR_KHZ: u32 = 1_000_000;

/* Leaf 1, ECX: the features kept. Everything not named here is cleared --
 * among them MONITOR, VMX, the TSC deadline timer, XSAVE, OSXSAVE, AVX and
 * the hypervisor bit. x2APIC is set on top (`LEAF1_ECX_X2APIC`) for a guest
 * of more than one CPU. */
const LEAF1_ECX_KEEP: u32 = (1 << 0)   // SSE3
    | (1 << 1)   // PCLMULQDQ
    | (1 << 9)   // SSSE3
    | (1 << 13)  // CMPXCHG16B
    | (1 << 19)  // SSE4.1
    | (1 << 20)  // SSE4.2
    | (1 << 22)  // MOVBE
    | (1 << 23)  // POPCNT
    | (1 << 25)  // AES-NI
    | (1 << 30); // RDRAND

/* Leaf 1, EDX: the features kept. APIC (bit 9) is set on top of them
 * (`LEAF1_EDX_APIC`) for a guest of more than one CPU, whatever the host
 * has, and cleared for one of one. */
const LEAF1_EDX_KEEP: u32 = (1 << 0)   // FPU
    | (1 << 1)   // VME
    | (1 << 2)   // DE
    | (1 << 3)   // PSE
    | (1 << 4)   // TSC
    | (1 << 5)   // MSR
    | (1 << 6)   // PAE
    | (1 << 7)   // MCE
    | (1 << 8)   // CX8
    | (1 << 11)  // SEP (SYSENTER)
    | (1 << 12)  // MTRR
    | (1 << 13)  // PGE
    | (1 << 14)  // MCA
    | (1 << 15)  // CMOV
    | (1 << 16)  // PAT
    | (1 << 17)  // PSE-36
    | (1 << 19)  // CLFLUSH
    | (1 << 23)  // MMX
    | (1 << 24)  // FXSR
    | (1 << 25)  // SSE
    | (1 << 26); // SSE2

/* Extended leaf 0x80000001, ECX: the features kept, on the same terms as
 * leaf 1's -- anything not named is cleared. That list was once "all of it
 * but SVM", and on a real Zen 2 the guest found MWAITX there (bit 29), used
 * MONITORX for its udelay, and stopped at an intercept nothing answers: TCG's
 * `-cpu max` never offered it. Among what is cleared: SVM, the extended APIC
 * space, OSVW, IBS, XOP and FMA4 and TBM (VEX-coded, and so on state XSAVE
 * would switch), SKINIT, the watchdog, LWP, the topology and performance
 * counter extensions, MWAITX. */
const EXT1_ECX_KEEP: u32 = (1 << 0)   // LAHF/SAHF in 64-bit mode
    | (1 << 5)   // ABM: LZCNT
    | (1 << 6)   // SSE4A
    | (1 << 7)   // misaligned SSE
    | (1 << 8);  // PREFETCHW

/* Extended leaf 0x80000001, EDX: what leaf 1 keeps of the bits AMD mirrors
 * there, SYSCALL, NX, the MMX extensions, 1 GiB pages and long mode. The
 * APIC's mirror (bit 9) is set as leaf 1's is. RDTSCP (bit 27) is cleared
 * because its intercept has no answer here but #UD; FFXSR and 3DNow! go too. */
const EXT1_EDX_KEEP: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4)
    | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8)   // FPU..CX8, as leaf 1
    | (1 << 11)  // SYSCALL
    | (1 << 12) | (1 << 13) | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 17) // MTRR..PSE-36
    | (1 << 20)  // NX
    | (1 << 22)  // MMX extensions
    | (1 << 23) | (1 << 24) // MMX, FXSR
    | (1 << 26)  // 1 GiB pages
    | (1 << 29); // long mode

/* Extended leaf 0x80000008, EBX: RDPRU (bit 4) and WBNOINVD (bit 9) are
 * cleared -- RDPRU's intercept has no answer here, and WBNOINVD is WBINVD with
 * a prefix whose length only next-RIP save could tell. The rest -- the
 * speculation controls a kernel wants to know of -- is the host's. */
const EXT8_EBX_CLEAR: u32 = (1 << 4) | (1 << 9);

/* Extended leaf 0x80000007: of AMD's power and RAS leaf, the invariant TSC
 * alone (EDX bit 8), which is the host's and so the guest's -- as KVM gives
 * it. EBX is the machine-check RAS set: with SUCCOR there, Linux sets up the
 * deferred-error interrupt through the APIC's extended LVT, a register this
 * APIC has not got, and logs the #GP and a firmware bug. The machine-check
 * banks read as zero here; there is nothing to recover from. */
const EXT7_EDX_INVARIANT_TSC: u32 = 1 << 8;

/// The answer to a CPUID: the four registers it fills.
pub struct Cpuid {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// What the guest's CPU whose APIC ID is `apic_id` is told for CPUID leaf
/// `leaf`, subleaf `sub` (ECX on the way in, for the leaves that take one);
/// `apic` says whether the guest has local APICs -- one of more than one
/// CPU -- which leaf 1 then says, the x2APIC with it.
pub fn cpuid(leaf: u32, sub: u32, apic_id: u32, apic: bool) -> Cpuid {
    /* Leaves this hypervisor blanks outright: the structured-features leaf
     * (its SMEP/SMAP/FSGSBASE/AVX2 are either the guest's own CR4 business
     * or things XSAVE gates, which is off), the XSAVE state leaf, and the
     * hypervisor range (no paravirtualisation is offered). */
    /* And the topology leaves, where the host's x2APIC IDs are (a guest
     * with none of them takes its one CPU from leaf 1); the SVM leaf, for an
     * extension it is not given; the extended APIC ID; and memory
     * encryption, which is not the guest's to turn on. */
    if leaf == LEAF_STRUCTURED
        || leaf == LEAF_XSTATE
        || leaf == LEAF_TOPOLOGY
        || leaf == LEAF_TOPOLOGY_V2
        || leaf == LEAF_SVM
        || leaf == LEAF_EXT_APIC_ID
        || leaf == LEAF_ENCRYPTION
        || (LEAF_HYPERVISOR_BASE..=LEAF_HYPERVISOR_END).contains(&leaf)
    {
        return Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 };
    }

    let host = match cpu::cpuid_count(leaf, sub) {
        Some(r) => r,
        None => return Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 },
    };
    let (mut eax, mut ebx, mut ecx, mut edx) = (host.eax, host.ebx, host.ecx, host.edx);

    let (apic_ecx, apic_edx) = if apic { (LEAF1_ECX_X2APIC, LEAF1_EDX_APIC) } else { (0, 0) };
    if leaf == LEAF_FEATURES {
        ebx = (ebx & LEAF1_EBX_KEEP) | LEAF1_EBX_ONE_CPU | ((apic_id & 0xFF) << LEAF1_EBX_APIC_ID_SHIFT);
        ecx = (ecx & LEAF1_ECX_KEEP) | apic_ecx;
        edx = (edx & LEAF1_EDX_KEEP) | apic_edx;
    } else if leaf == LEAF_CACHE {
        eax &= LEAF4_EAX_KEEP;
    } else if leaf == LEAF_POWER && apic {
        eax |= LEAF6_EAX_ARAT;
    } else if leaf == LEAF_TSC && apic {
        /* The host's TSC, from the host's own leaf: crystal times ratio.
         * Blank when the host does not say -- the guest then measures. */
        let tsc_khz = (u64::from(ecx) * u64::from(ebx)).checked_div(u64::from(eax)).unwrap_or(0) / 1000;
        return match u32::try_from(tsc_khz) {
            Ok(khz) if khz != 0 => Cpuid { eax: LEAF15_DENOMINATOR_KHZ, ebx: khz, ecx: LEAF15_CRYSTAL_HZ, edx: 0 },
            _ => Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 },
        };
    } else if leaf == LEAF_EXT_FEATURES {
        ecx &= EXT1_ECX_KEEP;
        edx = (edx & EXT1_EDX_KEEP) | apic_edx;
    } else if leaf == LEAF_EXT_POWER {
        eax = 0;
        ebx = 0;
        ecx = 0;
        edx &= EXT7_EDX_INVARIANT_TSC;
    } else if leaf == LEAF_EXT_ADDRESS {
        ebx &= !EXT8_EBX_CLEAR;
        ecx = 0;
    }

    Cpuid { eax, ebx, ecx, edx }
}

/// Apply a CPUID answer to the guest's registers: EAX and the GPRs the run
/// stub keeps for EBX, ECX, EDX.
pub fn apply_cpuid(save: &mut Save, regs: &mut GuestRegs, r: &Cpuid) {
    save.rax = r.eax as u64;
    regs.rbx = r.ebx as u64;
    regs.rcx = r.ecx as u64;
    regs.rdx = r.edx as u64;
}

/* System MSRs: the guest's own state, kept in the VMCB save area. */
const MSR_EFER: u32 = 0xC000_0080;
const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_CSTAR: u32 = 0xC000_0083;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
const MSR_PAT: u32 = 0x277;
const MSR_SYSENTER_CS: u32 = 0x174;
const MSR_SYSENTER_ESP: u32 = 0x175;
const MSR_SYSENTER_EIP: u32 = 0x176;

/* Two of AMD's that a Linux guest reads to learn what the CPU does, and
 * finds true: they answer so, and a write to either is swallowed as any
 * other is. DE_CFG's LFENCE_SERIALIZE -- set on every AMD host CPU by the
 * kernel (`Hal::SetupSerializingLfence`), as KVM reports it -- and HWCR's
 * TscFreqSel, read-only 1 on every AMD CPU since family 10h: read as zero,
 * the guest logs "TSC doesn't count with P0 frequency" as a firmware bug. */
const MSR_AMD_DE_CFG: u32 = 0xC001_1029;
const DE_CFG_LFENCE_SERIALIZE: u64 = 1 << 1;
const MSR_AMD_HWCR: u32 = 0xC001_0015;
const HWCR_TSC_FREQ_SEL: u64 = 1 << 24;

/// EFER bits the guest may set: SCE, LME, LMA, NXE, FFXSR. SVME and the rest
/// are the host's, and a guest that sets one is refused (a #GP) rather than
/// let into a state `vmrun` would bounce.
const EFER_GUEST_MASK: u64 = (1 << 0) | (1 << 8) | (1 << 10) | (1 << 11) | (1 << 14);
/// Long mode active: the CPU's to set and clear, with paging and LME, and
/// read-only to a `wrmsr` -- a write that says otherwise is ignored, as the
/// silicon ignores it, rather than put into the guest's EFER, where it would
/// be an entry the CPU refuses (VT-x checks it against its IA-32e control).
const EFER_LMA: u64 = 1 << 10;
/// The bit the CPU keeps set once long mode is active: the guest never
/// clears it while it runs 64-bit code, and it is part of its EFER.
const EFER_SVME: u64 = 1 << 12;
/// Long mode enable: fixed while paging is on -- the CPU faults a write that
/// changes it then, and a guest let change it would be one whose EFER says
/// long mode and whose CPU is not in it, which `vmrun` and VT-x both refuse.
const EFER_LME: u64 = 1 << 8;
const CR0_PG: u64 = 1 << 31;

/// What a guest read from MSR `msr`, and whether the read is allowed: `None`
/// means inject a #GP, which is what a real CPU does for a reserved MSR and
/// what a guest's `rdmsr_safe` is ready for.
pub fn rdmsr(save: &Save, msr: u32) -> Option<u64> {
    let value = match msr {
        MSR_EFER => save.efer & !EFER_SVME,
        MSR_STAR => save.star,
        MSR_LSTAR => save.lstar,
        MSR_CSTAR => save.cstar,
        MSR_SFMASK => save.sfmask,
        MSR_FS_BASE => save.fs.base,
        MSR_GS_BASE => save.gs.base,
        MSR_KERNEL_GS_BASE => save.kernel_gs_base,
        MSR_PAT => save.g_pat,
        MSR_SYSENTER_CS => save.sysenter_cs,
        MSR_SYSENTER_ESP => save.sysenter_esp,
        MSR_SYSENTER_EIP => save.sysenter_eip,
        MSR_AMD_DE_CFG => DE_CFG_LFENCE_SERIALIZE,
        MSR_AMD_HWCR => HWCR_TSC_FREQ_SEL,
        /* Every other MSR reads as zero: a feature MSR a guest probes and
         * finds empty, which is what a guest that cannot use the feature
         * anyway should see. */
        _ => 0,
    };
    Some(value)
}

/// A guest wrote `value` to MSR `msr`. Returns whether it is allowed; `false`
/// injects a #GP. The system MSRs land in the save area; everything else is
/// swallowed.
pub fn wrmsr(save: &mut Save, msr: u32, value: u64) -> bool {
    match msr {
        MSR_EFER => {
            if value & !EFER_GUEST_MASK != 0 {
                /* A bit the guest may not set -- SVME, or a reserved one. */
                return false;
            }
            if save.cr0 & CR0_PG != 0 && (value ^ save.efer) & EFER_LME != 0 {
                /* LME changed with paging on: the #GP the CPU gives. */
                return false;
            }
            /* SVME stays set, since the guest runs under SVM whether it
             * knows it or not; LMA stays what the CPU made it -- it follows
             * LME and paging, not a write. */
            save.efer = (value & !EFER_LMA) | (save.efer & EFER_LMA) | EFER_SVME;
        }
        MSR_STAR => save.star = value,
        MSR_LSTAR => save.lstar = value,
        MSR_CSTAR => save.cstar = value,
        MSR_SFMASK => save.sfmask = value,
        MSR_FS_BASE => save.fs.base = value,
        MSR_GS_BASE => save.gs.base = value,
        MSR_KERNEL_GS_BASE => save.kernel_gs_base = value,
        MSR_PAT => save.g_pat = value,
        MSR_SYSENTER_CS => save.sysenter_cs = value,
        MSR_SYSENTER_ESP => save.sysenter_esp = value,
        MSR_SYSENTER_EIP => save.sysenter_eip = value,
        /* Everything else: accepted and dropped, so a guest setting a
         * feature MSR it found empty does not fault on the write. */
        _ => {}
    }
    true
}
