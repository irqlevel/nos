//! What a guest is told when it asks the CPU about itself (CPUID) and when
//! it reads or writes a model-specific register (MSR).
//!
//! Both are intercepted for every guest -- a CPUID the host did not shape
//! could promise a feature this hypervisor cannot back, and an MSR reaching
//! the CPU directly is the host's own hardware. And both are allow-lists:
//! a leaf this policy does not name is all zeros, and an MSR it does not
//! name is a #GP, read or written -- nothing of the host's reaches a guest
//! that has not been looked at here, and nothing a guest writes goes
//! nowhere while it believes it went in. So a guest sees a CPU cut down to
//! what is emulated here: no XSAVE and so no AVX (the state switch around
//! `vmrun` moves only the FXSAVE registers), no paravirtualisation, no
//! virtualization extension of its own, no speculation control it would
//! write into nothing -- and a local APIC only in a guest of more than one
//! CPU, where it is an x2APIC (`lapic`) with no TSC-deadline timer. What is
//! left is enough to decompress and start a 64-bit kernel, and its other
//! CPUs: long mode, NX, SYSCALL, the SSE line, the PAT, the APIC and the
//! plain arithmetic features -- and the hypervisor bit, since there is one.
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
//! is where they live; a read or a write of one is served from there. The
//! TSC reads as the host's. A few more answer as a CPU with nothing behind
//! them would (`constant`): the ones a feature the guest's CPUID offers says
//! the CPU has -- MTRRs off, machine checks with no banks -- and the ones a
//! kernel reads of every CPU of the vendor without asking first. Every other
//! MSR is a #GP both ways, which a guest's `rdmsr_safe`/`wrmsr_safe` probes
//! are ready for; a refusal the guest was not ready for is a line in its
//! log (Linux's "unchecked MSR access error"), and in the VM's report.

use hvarch::x86::cpu;
use hvarch::x86::svm::vmcb::Save;
use hvarch::x86::svm::GuestRegs;

/* The CPUID leaves a guest is answered -- each named, each looked at --
 * and the top of each range, which leaf 0 and leaf 0x8000_0000 report: the
 * highest of them, or the host's top if that is lower. Every other leaf is
 * all zeros, as a CPU answers one it has nothing in: no leaf of the host's
 * reaches a guest that this policy has not read. Among those left zero:
 * the structured features (7) -- SMEP, SMAP, FSGSBASE, AVX2 and the rest,
 * either the guest's own CR4 business or state XSAVE would switch, which
 * is off -- and with them every speculation control Intel enumerates
 * there; the topology leaves (0xB, 0x1F), where the host's x2APIC IDs are;
 * XSAVE's (0xD); the PMU's (0xA), with no counters to back it; the
 * hypervisor range (no paravirtualisation is offered); and of AMD's, the
 * SVM leaf, the extended APIC ID, memory encryption and the extended
 * performance monitor. */
const LEAF_BASIC: u32 = 0;
const LEAF_FEATURES: u32 = 1;
const LEAF_CACHE_DESCRIPTORS: u32 = 2;
const LEAF_CACHE: u32 = 4;
const LEAF_POWER: u32 = 6;
const LEAF_STRUCTURED: u32 = 7;
const LEAF_TSC: u32 = 0x15;
const LEAF_FREQUENCY: u32 = 0x16;
const LEAF_BASIC_TOP: u32 = LEAF_FREQUENCY;
const LEAF_EXT: u32 = 0x8000_0000;
const LEAF_EXT_FEATURES: u32 = 0x8000_0001;
const LEAF_BRAND_FIRST: u32 = 0x8000_0002;
const LEAF_BRAND_LAST: u32 = 0x8000_0004;
const LEAF_L1: u32 = 0x8000_0005;
const LEAF_L2: u32 = 0x8000_0006;
const LEAF_EXT_POWER: u32 = 0x8000_0007;
const LEAF_EXT_ADDRESS: u32 = 0x8000_0008;
const LEAF_EXT_FEATURES2: u32 = 0x8000_0021;
const LEAF_EXT_TOP: u32 = LEAF_EXT_FEATURES2;

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
/// Leaf 1 ECX bit 31: a hypervisor is there. It is, and a kernel that
/// knows it leaves alone what is the host's business -- Linux loads no
/// microcode (PATCH_LOADER, UCODE_WRITE), sets no chicken bits in AMD's
/// configuration MSRs, and does not take the revision of 0 it reads for
/// old microcode and taint itself. The hypervisor range is blank, so it
/// finds no hypervisor it knows by name, and runs as on bare metal
/// otherwise.
const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
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
 * among them MONITOR, VMX, PDCM, PCID, the TSC deadline timer, XSAVE,
 * OSXSAVE and AVX. x2APIC is set on top (`LEAF1_ECX_X2APIC`) for a guest of
 * more than one CPU, and the hypervisor bit for every guest. */
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

/* Extended leaf 0x80000008: in EAX the widths of a physical and a linear
 * address, the host's -- what a guest's page tables are held to -- and not
 * the width of a nested guest's. In EBX, what the CPU does as it is: CLZERO,
 * an instruction with nothing to intercept; the FP error pointers FXSAVE
 * always keeps; and two attacks it is not open to, speculative store bypass
 * and branch type confusion -- facts of the silicon, true of a guest on it.
 * A speculation control is offered only beside the MSR that makes it true,
 * and there is none here yet: IBPB was offered while a write to PRED_CMD
 * went nowhere, and IBRS, STIBP and SSBD while SPEC_CTRL kept nothing, so a
 * guest on AMD reported itself mitigated and was not. Among the rest:
 * INVLPGB, which the VMCB does not enable (Linux 6.15 on Zen 3 would flush
 * its TLBs with it and take a #UD, by its source), RDPRU and WBNOINVD,
 * whose exits have no answer here, PPIN and CPPC, MSRs nobody answers. */
const EXT8_EAX_KEEP: u32 = 0xFFFF;
const EXT8_EBX_KEEP: u32 = (1 << 0)   // CLZERO
    | (1 << 2)   // FP error pointers always saved
    | (1 << 26)  // not affected by speculative store bypass
    | (1 << 29); // not affected by branch type confusion

/* Extended leaf 0x80000021, AMD's second feature leaf, of Zen 3 and later:
 * what is true of the CPU a guest runs on and asks nothing of the host -- no
 * nested data breakpoints, LFENCE serializing, VERW clearing what TSA would
 * leave, a null selector clearing the base, and not affected by SRSO, at
 * all or across user and kernel; in ECX, not affected by TSA-SQ or TSA-L1.
 * Not AutoIBRS, whose EFER bit a guest may not set (offered with its
 * write refused, Linux on Zen 4 runs with neither it nor retpolines, by its
 * source), SBPB and IBPB_BRTYPE, PRED_CMD's, nor CPUID faulting, HWCR's. */
const EXT21_EAX_KEEP: u32 = (1 << 0)   // no nested data breakpoints
    | (1 << 2)   // LFENCE always serializing
    | (1 << 5)   // VERW clears what TSA would leave
    | (1 << 6)   // a null selector clears the base
    | (1 << 29)  // not affected by SRSO
    | (1 << 30); // nor across user and kernel
const EXT21_ECX_KEEP: u32 = (1 << 1)   // not affected by TSA-SQ
    | (1 << 2);  // nor TSA-L1

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
    const NONE: Cpuid = Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 };
    /* Above a range's top the guest is told there is nothing, and is
     * answered so: a leaf of the host's past it would be the host's no
     * matter what this policy says of it. */
    let top = if leaf >= LEAF_EXT { LEAF_EXT_TOP } else { LEAF_BASIC_TOP };
    if leaf > top {
        return NONE;
    }
    let Some(host) = cpu::cpuid_count(leaf, sub) else { return NONE };
    let (eax, ebx, ecx, edx) = (host.eax, host.ebx, host.ecx, host.edx);
    let (apic_ecx, apic_edx) = if apic { (LEAF1_ECX_X2APIC, LEAF1_EDX_APIC) } else { (0, 0) };

    match leaf {
        LEAF_BASIC => Cpuid { eax: eax.min(LEAF_BASIC_TOP), ebx, ecx, edx },
        LEAF_FEATURES => Cpuid {
            eax,
            ebx: (ebx & LEAF1_EBX_KEEP) | LEAF1_EBX_ONE_CPU | ((apic_id & 0xFF) << LEAF1_EBX_APIC_ID_SHIFT),
            ecx: (ecx & LEAF1_ECX_KEEP) | apic_ecx | LEAF1_ECX_HYPERVISOR,
            edx: (edx & LEAF1_EDX_KEEP) | apic_edx,
        },
        /* What the caches, TLBs and the brand are, and how fast the CPU is
         * clocked: descriptions, nothing a guest can turn on. */
        LEAF_CACHE_DESCRIPTORS | LEAF_FREQUENCY | LEAF_BRAND_FIRST..=LEAF_BRAND_LAST | LEAF_L1 | LEAF_L2 => {
            Cpuid { eax, ebx, ecx, edx }
        }
        LEAF_CACHE => Cpuid { eax: eax & LEAF4_EAX_KEEP, ebx, ecx, edx },
        /* Of the power leaf, ARAT alone, for a guest with an APIC timer:
         * the rest -- APERF/MPERF, HWP, the energy bias, the thermal sensor
         * -- each names MSRs this hypervisor does not answer. */
        LEAF_POWER => Cpuid { eax: if apic { LEAF6_EAX_ARAT } else { 0 }, ebx: 0, ecx: 0, edx: 0 },
        LEAF_TSC if apic => {
            /* The host's TSC, from the host's own leaf: crystal times
             * ratio. Blank when the host does not say -- the guest then
             * measures. */
            let tsc_khz = (u64::from(ecx) * u64::from(ebx)).checked_div(u64::from(eax)).unwrap_or(0) / 1000;
            match u32::try_from(tsc_khz) {
                Ok(khz) if khz != 0 => Cpuid { eax: LEAF15_DENOMINATOR_KHZ, ebx: khz, ecx: LEAF15_CRYSTAL_HZ, edx: 0 },
                _ => NONE,
            }
        }
        /* A guest with no APIC has no timer it counts: the leaf is the
         * host's TSC and crystal, which are the guest's too. */
        LEAF_TSC => Cpuid { eax, ebx, ecx, edx },
        LEAF_EXT => Cpuid { eax: eax.min(LEAF_EXT_TOP), ebx, ecx, edx },
        LEAF_EXT_FEATURES => Cpuid { eax, ebx, ecx: ecx & EXT1_ECX_KEEP, edx: (edx & EXT1_EDX_KEEP) | apic_edx },
        LEAF_EXT_POWER => Cpuid { eax: 0, ebx: 0, ecx: 0, edx: edx & EXT7_EDX_INVARIANT_TSC },
        LEAF_EXT_ADDRESS => Cpuid { eax: eax & EXT8_EAX_KEEP, ebx: ebx & EXT8_EBX_KEEP, ecx: 0, edx: 0 },
        LEAF_EXT_FEATURES2 => Cpuid { eax: eax & EXT21_EAX_KEEP, ebx: 0, ecx: ecx & EXT21_ECX_KEEP, edx: 0 },
        _ => NONE,
    }
}

/// A register of a CPUID answer.
#[derive(Clone, Copy)]
enum Word {
    Ebx,
    Ecx,
    Edx,
}

/* CR4's bits, each with the feature a CPU has it with: the leaf, the
 * register and the bit that offer it. A CPU without the feature raises #GP
 * for its bit. A bit not here is never the guest's: one this policy comes to
 * offer is to be added, or the guest's kernel finds its CR4 write refused. */
const CR4_FEATURES: &[(u64, u32, Word, u32)] = &[
    (1 << 0, LEAF_FEATURES, Word::Edx, 1),     // VME
    (1 << 1, LEAF_FEATURES, Word::Edx, 1),     // PVI, VME's
    (1 << 2, LEAF_FEATURES, Word::Edx, 4),     // TSD: the TSC
    (1 << 3, LEAF_FEATURES, Word::Edx, 2),     // DE
    (1 << 4, LEAF_FEATURES, Word::Edx, 3),     // PSE
    (1 << 5, LEAF_FEATURES, Word::Edx, 6),     // PAE
    (1 << 6, LEAF_FEATURES, Word::Edx, 7),     // MCE
    (1 << 7, LEAF_FEATURES, Word::Edx, 13),    // PGE
    (1 << 9, LEAF_FEATURES, Word::Edx, 24),    // OSFXSR: FXSR
    (1 << 10, LEAF_FEATURES, Word::Edx, 25),   // OSXMMEXCPT: SSE
    (1 << 11, LEAF_STRUCTURED, Word::Ecx, 2),  // UMIP
    (1 << 12, LEAF_STRUCTURED, Word::Ecx, 16), // LA57
    (1 << 13, LEAF_FEATURES, Word::Ecx, 5),    // VMXE: VMX
    (1 << 14, LEAF_FEATURES, Word::Ecx, 6),    // SMXE: SMX
    (1 << 16, LEAF_STRUCTURED, Word::Ebx, 0),  // FSGSBASE
    (1 << 17, LEAF_FEATURES, Word::Ecx, 17),   // PCIDE: PCID
    (1 << 18, LEAF_FEATURES, Word::Ecx, 26),   // OSXSAVE: XSAVE
    (1 << 20, LEAF_STRUCTURED, Word::Ebx, 7),  // SMEP
    (1 << 21, LEAF_STRUCTURED, Word::Ebx, 20), // SMAP
    (1 << 22, LEAF_STRUCTURED, Word::Ecx, 3),  // PKE: PKU
    (1 << 23, LEAF_STRUCTURED, Word::Ecx, 7),  // CET: shadow stacks,
    (1 << 23, LEAF_STRUCTURED, Word::Edx, 20), // or indirect-branch tracking
];
/// CR4.PCE: RDPMC at any privilege level -- an instruction every x86-64 CPU
/// has, and so a bit no feature offers.
const CR4_PCE: u64 = 1 << 8;

/// The CR4 bits a guest may set: those whose feature the CPUID it is told
/// offers (`cpuid`), and PCE. Any other is the #GP a CPU without the
/// feature gives (`crate::cr`), so no feature reaches a guest behind its
/// CPUID's back -- PKE above all, whose PKRU nothing switches between
/// guests. Today that is VME and PVI, TSD, DE, PSE, PAE, MCE, PGE, PCE,
/// OSFXSR and OSXMMEXCPT, as the host has them: the structured-features
/// leaf is blank, and PCID and XSAVE are not offered.
pub fn cr4_allowed() -> u64 {
    let features = cpuid(LEAF_FEATURES, 0, 0, false);
    let structured = cpuid(LEAF_STRUCTURED, 0, 0, false);
    let mut allowed = CR4_PCE;
    for &(bit, leaf, word, feature) in CR4_FEATURES {
        let answer = if leaf == LEAF_FEATURES { &features } else { &structured };
        let value = match word {
            Word::Ebx => answer.ebx,
            Word::Ecx => answer.ecx,
            Word::Edx => answer.edx,
        };
        if value & (1 << feature) != 0 {
            allowed |= bit;
        }
    }
    allowed
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

/* The time-stamp counter: the guest's is the host's, which it reads with
 * `rdtsc` and may read here. Not set: a write would have to move the
 * guest's clock alone, and nothing offsets it. */
const MSR_TSC: u32 = 0x10;

/* MSRs that a feature the guest's CPUID offers says its CPU has, or that a
 * kernel reads of every CPU of its vendor without asking: each answers as a
 * CPU with nothing more behind it would, and keeps nothing of the guest's
 * (`Constant`). */
/// The MTRRs (CPUID's MTRR): MTRRcap -- no variable ranges, no fixed ones,
/// no write-combining -- and the default type, off. A guest's memory types
/// are its PAT's and the nested table's, which no MTRR of its own changes;
/// Linux, finding them "disabled by BIOS", takes its PAT on its own.
const MSR_MTRR_CAP: u32 = 0xFE;
const MSR_MTRR_DEF_TYPE: u32 = 0x2FF;
/// Machine checks (CPUID's MCA): no banks and no MCG_CTL, MCG_CAP says, and
/// nothing to report. Linux reads both with no fallback for a fault -- and
/// takes a fault on a machine-check register for an architectural violation
/// and panics.
const MSR_MCG_CAP: u32 = 0x179;
const MSR_MCG_STATUS: u32 = 0x17A;
/// The microcode revision, Intel's BIOS_SIGN_ID and AMD's patch level: none,
/// the host's being the host's to know. Intel's protocol writes 0 to it,
/// then runs CPUID and reads it back, which Linux does on every Intel CPU.
const MSR_UCODE_REV: u32 = 0x8B;
/// Intel's platform ID, which a microcode loader reads first and with no
/// fallback for a fault -- Linux 6.1 does even under a hypervisor, when its
/// initramfs carries Intel's microcode. Not the platform information
/// (0xCE): what reads it is ready for a fault, and intel_pstate, which is
/// not, only runs where leaf 6 offers APERF/MPERF.
const MSR_PLATFORM_ID: u32 = 0x17;
/// Intel's IA32_MISC_ENABLE: fast strings as the host has them -- a guest's
/// `rep movs` is the host CPU's -- and branch tracing and PEBS unavailable,
/// as they are. Read as zero, Linux turned its fast string paths off ("Disabled
/// fast string operations").
const MSR_MISC_ENABLE: u32 = 0x1A0;
const MISC_ENABLE_FAST_STRING: u64 = 1 << 0;
const MISC_ENABLE_BTS_UNAVAILABLE: u64 = 1 << 11;
const MISC_ENABLE_PEBS_UNAVAILABLE: u64 = 1 << 12;
/// AMD's, which Linux reads on every AMD CPU to learn what it does, and
/// finds true: DE_CFG's LFENCE_SERIALIZE -- set on every AMD host CPU by the
/// kernel (`Hal::SetupSerializingLfence`), as KVM reports it -- HWCR's
/// TscFreqSel, read-only 1 on every AMD CPU since family 10h (read as zero,
/// the guest logs "TSC doesn't count with P0 frequency" as a firmware bug),
/// and NB_CFG's extended configuration through CF8, which Linux turns on for
/// family 10h and later, with no fallback for a fault: on, and no function
/// here has a register for it to reach (`devices::pci`).
const MSR_AMD_HWCR: u32 = 0xC001_0015;
const HWCR_TSC_FREQ_SEL: u64 = 1 << 24;
const MSR_AMD_NB_CFG: u32 = 0xC001_001F;
const NB_CFG_CF8_EXT_CFG: u64 = 1 << 46;
const MSR_AMD_DE_CFG: u32 = 0xC001_1029;
const DE_CFG_LFENCE_SERIALIZE: u64 = 1 << 1;

/// What an MSR this policy names, but keeps nothing of, reads -- and the
/// writes it takes: one that leaves it as it reads (`Fixed`), or none
/// (`ReadOnly`), as the CPU's own read-only registers take none. A write
/// that would change one is refused, a #GP: there is nothing behind it to
/// change, and a guest told its write went in would believe what is not so.
#[derive(Clone, Copy)]
enum Constant {
    ReadOnly(u64),
    Fixed(u64),
}

/// Whose MSRs the guest's CPU has: the host's vendor's, whose name CPUID's
/// leaf 0 gives it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vendor {
    Amd,
    Intel,
    Other,
}

fn vendor() -> Vendor {
    match &cpu::vendor_id() {
        b"AuthenticAMD" | b"HygonGenuine" => Vendor::Amd,
        b"GenuineIntel" => Vendor::Intel,
        _ => Vendor::Other,
    }
}

/// The MSR `msr`, if it is one of the constant ones a guest of this host's
/// vendor is answered.
fn constant(msr: u32) -> Option<Constant> {
    let vendor = vendor();
    Some(match msr {
        MSR_MTRR_CAP | MSR_MCG_CAP => Constant::ReadOnly(0),
        MSR_MTRR_DEF_TYPE | MSR_MCG_STATUS | MSR_UCODE_REV => Constant::Fixed(0),
        MSR_PLATFORM_ID if vendor == Vendor::Intel => Constant::ReadOnly(0),
        MSR_MISC_ENABLE if vendor == Vendor::Intel => Constant::Fixed(
            if cpu::fast_strings() { MISC_ENABLE_FAST_STRING } else { 0 }
                | MISC_ENABLE_BTS_UNAVAILABLE | MISC_ENABLE_PEBS_UNAVAILABLE),
        MSR_AMD_HWCR if vendor == Vendor::Amd => Constant::Fixed(HWCR_TSC_FREQ_SEL),
        MSR_AMD_NB_CFG if vendor == Vendor::Amd => Constant::Fixed(NB_CFG_CF8_EXT_CFG),
        MSR_AMD_DE_CFG if vendor == Vendor::Amd => Constant::Fixed(DE_CFG_LFENCE_SERIALIZE),
        _ => return None,
    })
}

/// The memory types a PAT entry may be: UC, WC, WT, WP, WB and UC- -- 2, 3
/// and every byte with a bit above the three of the type set are reserved.
fn pat_valid(value: u64) -> bool {
    value.to_le_bytes().iter().all(|&t| matches!(t, 0 | 1 | 4 | 5 | 6 | 7))
}

/// Whether an address is canonical: bits 63 to 47 all the same -- 48 bits
/// of address, the guest not being offered LA57's 57.
fn canonical(value: u64) -> bool {
    (((value as i64) << 16) >> 16) as u64 == value
}

/// EFER bits the guest may set: SCE, LME, LMA and NXE -- SYSCALL, long mode
/// and NX, which its CPUID offers. SVME and the rest are the host's, or a
/// feature its CPUID does not offer (FFXSR, LMSLE, TCE, AutoIBRS), and a
/// guest that sets one is refused (a #GP) rather than let into a state
/// `vmrun` would bounce or the CPU would run it in behind its CPUID's back.
const EFER_GUEST_MASK: u64 = (1 << 0) | (1 << 8) | (1 << 10) | (1 << 11);
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
/// -- for every MSR not named here -- means inject a #GP, which is what a
/// real CPU does for one it does not have and what a guest's `rdmsr_safe`
/// is ready for.
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
        MSR_TSC => cpu::rdtsc(),
        _ => match constant(msr)? {
            Constant::ReadOnly(v) | Constant::Fixed(v) => v,
        },
    };
    Some(value)
}

/// A guest wrote `value` to MSR `msr`. Returns whether it is allowed; `false`
/// injects a #GP. The system MSRs land in the save area; a constant one
/// takes a write that leaves it as it is (`Constant`); every other write --
/// to the TSC, to an MSR not named here -- is refused, and nothing a guest
/// writes goes nowhere.
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
        /* What the CPU refuses of the rest, a #GP: a PAT entry that is no
         * memory type, and an address that is not canonical. Taken into the
         * save area instead, either was a VMCB `vmrun` refuses, or a VM entry
         * VT-x refuses -- the guest stopped where it should have faulted. */
        MSR_PAT if !pat_valid(value) => return false,
        MSR_LSTAR | MSR_CSTAR | MSR_FS_BASE | MSR_GS_BASE | MSR_KERNEL_GS_BASE | MSR_SYSENTER_ESP
        | MSR_SYSENTER_EIP if !canonical(value) => return false,
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
        _ => return matches!(constant(msr), Some(Constant::Fixed(v)) if v == value),
    }
    true
}
