//! What a guest is told when it asks the CPU about itself (CPUID) and when
//! it reads or writes a model-specific register (MSR).
//!
//! Both are intercepted for every guest -- a CPUID the host did not shape
//! could promise a feature this hypervisor cannot back, and an MSR reaching
//! the CPU directly is the host's own hardware. So a guest sees a CPU cut
//! down to what is emulated here: no local APIC, no x2APIC, no XSAVE and so
//! no AVX (the state switch around `vmrun` moves only the FXSAVE registers),
//! no paravirtualisation, no virtualization extension of its own. What is
//! left is enough to decompress and start a 64-bit kernel: long mode, NX,
//! SYSCALL, the SSE line, the PAT and the plain arithmetic features.
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
const LEAF_STRUCTURED: u32 = 7;
const LEAF_TOPOLOGY: u32 = 0xB;
const LEAF_XSTATE: u32 = 0xD;
const LEAF_TOPOLOGY_V2: u32 = 0x1F;
const LEAF_HYPERVISOR_BASE: u32 = 0x4000_0000;
const LEAF_HYPERVISOR_END: u32 = 0x4000_00FF;
const LEAF_EXT_FEATURES: u32 = 0x8000_0001;
const LEAF_EXT_ADDRESS: u32 = 0x8000_0008;
const LEAF_SVM: u32 = 0x8000_000A;
const LEAF_EXT_APIC_ID: u32 = 0x8000_001E;
const LEAF_ENCRYPTION: u32 = 0x8000_001F;

/* The guest is one CPU with APIC ID 0 in a package of its own, whatever the
 * host CPU its vCPU runs on: leaf 1 EBX says so in its top two bytes -- the
 * initial APIC ID, and the logical processors in the package -- leaf 4 in
 * its core and sharing counts, and leaf 0x80000008 ECX in the core count and
 * the APIC ID's width. The host's numbers there are another machine's: a
 * Linux guest took CPU 3's APIC ID for its own ("APIC ID mismatch"). */
const LEAF1_EBX_KEEP: u32 = 0x0000_FFFF;
const LEAF1_EBX_ONE_CPU: u32 = 1 << 16;
const LEAF4_EAX_KEEP: u32 = 0x0000_3FFF;

/* Leaf 1, ECX: the features kept. Everything not named here is cleared --
 * among them MONITOR, VMX, x2APIC, the TSC deadline timer, XSAVE, OSXSAVE,
 * AVX and the hypervisor bit. */
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

/* Leaf 1, EDX: the features kept. APIC (bit 9) is cleared, so the guest
 * expects no local APIC -- which this hypervisor does not emulate yet. */
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
 * there (the APIC again not), SYSCALL, NX, the MMX extensions, 1 GiB pages and
 * long mode. RDTSCP (bit 27) is cleared because its intercept has no answer
 * here but #UD; FFXSR and 3DNow! go too. */
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

/// The answer to a CPUID: the four registers it fills.
pub struct Cpuid {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// What the guest is told for CPUID leaf `leaf`, subleaf `sub` (ECX on the
/// way in, for the leaves that take one).
pub fn cpuid(leaf: u32, sub: u32) -> Cpuid {
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

    if leaf == LEAF_FEATURES {
        ebx = (ebx & LEAF1_EBX_KEEP) | LEAF1_EBX_ONE_CPU;
        ecx &= LEAF1_ECX_KEEP;
        edx &= LEAF1_EDX_KEEP;
    } else if leaf == LEAF_CACHE {
        eax &= LEAF4_EAX_KEEP;
    } else if leaf == LEAF_EXT_FEATURES {
        ecx &= EXT1_ECX_KEEP;
        edx &= EXT1_EDX_KEEP;
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
