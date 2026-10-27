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
//! what is emulated here: XSAVE's state no further than AVX's (what the
//! switch around `vmrun` and `vmlaunch` moves, `hvarch::x86::fp`), so no
//! AVX-512, AMX or PKU; of paravirtualisation, KVM's interface as far as it
//! is backed here (`hypervisor_leaf`); no virtualization extension of its
//! own, no speculation control it would write into nothing -- and a
//! local APIC only in a guest of more than one CPU, where it is an x2APIC
//! (`lapic`) with its TSC-deadline timer. What is left is enough to
//! decompress and start a 64-bit kernel, and its other CPUs, and to run
//! what is built for x86-64-v3: long mode, NX, SYSCALL, the SSE and AVX
//! lines, the PAT, the APIC and the plain arithmetic features -- and the
//! hypervisor bit, since there is one.
//!
//! A guest's CPUs are one package, each CPU's APIC ID its number, of cores
//! of one thread or two -- two where its CPUs 2k and 2k+1 are one host
//! core's two threads -- sharing the last-level cache where they share the
//! host's ([`Shape`], from where they were placed): leaf 1, the x2APIC
//! topology leaf (0xB), the cache leaves' sharing counts (4, AMD's
//! 0x8000_001D) and AMD's core counts (0x8000_0008, 0x8000_001E) all say
//! so, and nothing of the host's topology reaches it. The CPUs are listed by
//! the MP table the loader writes (`linux`), with the same IDs. A guest of one
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

use hvarch::x86::{self, cpu, fp, spec};
use hvarch::x86::svm::vmcb::Save;
use hvarch::x86::svm::GuestRegs;

use crate::place::Shape;

/* The CPUID leaves a guest is answered -- each named, each looked at --
 * and the top of each range, which leaf 0 and leaf 0x8000_0000 report: the
 * highest of them, or the host's top if that is lower. Every other leaf is
 * all zeros, as a CPU answers one it has nothing in: no leaf of the host's
 * reaches a guest that this policy has not read. Among those left zero:
 * the PMU's (0xA), with no counters to back it; the hypervisor range (no
 * paravirtualisation is offered); and of AMD's, the SVM leaf, memory
 * encryption and the extended performance monitor. The topology leaves
 * are the guest's own (`Shape`), the host's x2APIC IDs none of them; 0x1F,
 * past the top, is not asked. */
const LEAF_BASIC: u32 = 0;
const LEAF_FEATURES: u32 = 1;
const LEAF_CACHE_DESCRIPTORS: u32 = 2;
const LEAF_CACHE: u32 = 4;
const LEAF_POWER: u32 = 6;
const LEAF_STRUCTURED: u32 = 7;
const LEAF_TOPOLOGY: u32 = 0xB;
const LEAF_XSAVE: u32 = 0xD;
const LEAF_TSC: u32 = 0x15;
const LEAF_FREQUENCY: u32 = 0x16;
const LEAF_BASIC_TOP: u32 = LEAF_FREQUENCY;
const LEAF_HYPERVISOR: u32 = 0x4000_0000;
const LEAF_KVM_FEATURES: u32 = 0x4000_0001;
const LEAF_EXT: u32 = 0x8000_0000;
const LEAF_EXT_FEATURES: u32 = 0x8000_0001;
const LEAF_BRAND_FIRST: u32 = 0x8000_0002;
const LEAF_BRAND_LAST: u32 = 0x8000_0004;
const LEAF_L1: u32 = 0x8000_0005;
const LEAF_L2: u32 = 0x8000_0006;
const LEAF_EXT_POWER: u32 = 0x8000_0007;
const LEAF_EXT_ADDRESS: u32 = 0x8000_0008;
const LEAF_EXT_CACHE: u32 = 0x8000_001D;
const LEAF_EXT_APIC_ID: u32 = 0x8000_001E;
const LEAF_EXT_FEATURES2: u32 = 0x8000_0021;
const LEAF_EXT_TOP: u32 = LEAF_EXT_FEATURES2;

/* Where the guest's CPUs sit is the guest's own (`Shape`), whatever host
 * CPU each runs on: leaf 1 EBX says so in its top two bytes -- the initial
 * APIC ID, and the APIC IDs a package has room for -- with EDX's HTT where
 * that is more than one; leaf 4 in its core and sharing counts, leaf 0xB
 * level by level, and AMD's 0x8000_0008 ECX in the package's CPUs and the
 * APIC ID's width, 0x8000_001D in its sharing counts and 0x8000_001E in
 * the core and its threads. The host's numbers there are another
 * machine's: a Linux guest took CPU 3's APIC ID for its own ("APIC ID
 * mismatch"). */
const LEAF1_EBX_KEEP: u32 = 0x0000_FFFF;
const LEAF1_EBX_IDS_SHIFT: u32 = 16;
const LEAF1_EBX_APIC_ID_SHIFT: u32 = 24;
const LEAF1_EDX_HTT: u32 = 1 << 28;
/// A deterministic cache leaf's EAX -- leaf 4's, AMD's 0x8000_001D -- of
/// which the cache's own fields are kept: its type, its level, whether it
/// initialises itself and is fully associative. The CPUs sharing it (bits
/// 25:14, less one) and, in leaf 4, the package's cores (31:26, less one)
/// are the guest's.
const LEAF4_EAX_KEEP: u32 = 0x0000_3FFF;
const CACHE_TYPE_MASK: u32 = 0x1F;
const CACHE_LEVEL_SHIFT: u32 = 5;
const CACHE_LEVEL_MASK: u32 = 0x7;
const CACHE_SHARING_SHIFT: u32 = 14;
const CACHE_CORES_SHIFT: u32 = 26;
/// More subleaves than any CPU has caches.
const CACHE_SUBLEAVES: u32 = 16;
/// Leaf 0xB's levels, in ECX[15:8]: a core's threads, and the package's
/// cores; 0 ends the list.
const TOPOLOGY_SMT: u32 = 1;
const TOPOLOGY_CORE: u32 = 2;
const TOPOLOGY_LEVEL_SHIFT: u32 = 8;
const TOPOLOGY_SUBLEAF_MASK: u32 = 0xFF;
/// 0x8000_0001 ECX bit 22, TOPOEXT: AMD's topology leaves, 0x8000_001D and
/// 0x8000_001E, are there -- answered with the guest's own, where the host
/// has them.
const EXT1_ECX_TOPOEXT: u32 = 1 << 22;
/// 0x8000_0008 ECX: the package's CPUs less one (7:0), and the APIC ID's
/// width below the package's (15:12).
const EXT8_ECX_APIC_ID_SIZE_SHIFT: u32 = 12;
/// 0x8000_001E EBX: the core's ID (7:0) and its threads less one (15:8).
const EXT1E_EBX_THREADS_SHIFT: u32 = 8;
/// Leaf 1 ECX bit 24: the APIC timer's TSC-deadline mode, emulated
/// (`lapic`) for a guest with APICs where the TSC's rate is known -- what
/// a deadline is turned into the host's time by.
const LEAF1_ECX_TSC_DEADLINE: u32 = 1 << 24;
/// Leaf 1: the APIC (EDX bit 9) and x2APIC mode (ECX bit 21), both
/// emulated (`lapic`), set whatever the host has.
const LEAF1_EDX_APIC: u32 = 1 << 9;
const LEAF1_ECX_X2APIC: u32 = 1 << 21;
/// Leaf 1 ECX bit 31: a hypervisor is there. It is, and a kernel that
/// knows it leaves alone what is the host's business -- Linux loads no
/// microcode (PATCH_LOADER, UCODE_WRITE), sets no chicken bits in AMD's
/// configuration MSRs, and does not take the revision of 0 it reads for
/// old microcode and taint itself. The hypervisor range names KVM
/// (`hypervisor_leaf`), whose paravirtual interface a Linux built with
/// KVM_GUEST then takes as far as it is offered, and runs as on bare metal
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
 * among them MONITOR, VMX and PDCM -- but for the XSAVE line
 * (`leaf1_xsave`), and x2APIC and the TSC-deadline timer, set on top
 * (`LEAF1_ECX_X2APIC`, `LEAF1_ECX_TSC_DEADLINE`) for a guest of more than
 * one CPU, and the hypervisor bit for every guest. PCID is the guest's own:
 * its TLB entries tagged under its VPID or ASID as the CPU tags them, CR4's
 * bit turned on under `crate::cr`'s rules; and INVPCID with it where it
 * runs in a guest (`LEAF7_EBX_INVPCID`). */
const LEAF1_ECX_KEEP: u32 = (1 << 0)   // SSE3
    | (1 << 1)   // PCLMULQDQ
    | (1 << 9)   // SSSE3
    | (1 << 13)  // CMPXCHG16B
    | (1 << 17)  // PCID
    | (1 << 19)  // SSE4.1
    | (1 << 20)  // SSE4.2
    | (1 << 22)  // MOVBE
    | (1 << 23)  // POPCNT
    | (1 << 25)  // AES-NI
    | (1 << 30); // RDRAND

/* Leaf 1, ECX: the XSAVE line. XSAVE where the host switches any of its
 * state for a guest (`fp::offered`), OSXSAVE with it as the guest's own
 * CR4.OSXSAVE has it, as a CPU's mirrors its CR4; and where AVX's state is
 * switched, AVX, and FMA and F16C where the host has them -- VEX-coded on
 * the YMM registers, as AVX is. */
const LEAF1_ECX_FMA: u32 = 1 << 12;
const LEAF1_ECX_XSAVE: u32 = 1 << 26;
const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
const LEAF1_ECX_AVX: u32 = 1 << 28;
const LEAF1_ECX_F16C: u32 = 1 << 29;
/// CR4.OSXSAVE, which leaf 1's OSXSAVE mirrors.
const CR4_OSXSAVE: u64 = 1 << 18;

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

/* Leaf 7, subleaf 0, the structured features: what runs on the CPU as it
 * is, with nothing of the host's behind it to switch, and what CR4 bits the
 * guest owns turn on -- SMEP, SMAP and UMIP for its kernel, FSGSBASE over
 * bases the VMCS and the VMCB keep -- and facts of the silicon; GFNI, whose
 * SSE forms are on the XMM registers and VEX forms on the YMM; where AVX's
 * state is switched, the AVX line (`LEAF7_EBX_AVX`); where they run in a
 * guest, INVPCID and RDPID (`hvarch::x86::offers`); where PKRU is switched,
 * PKU and OSPKE (`LEAF7_ECX_PKU`); and the speculation controls, whose MSRs
 * are the guest's own (`LEAF7_EDX_SPEC`). Its EAX is the last subleaf
 * answered, the host's or 2 (`LEAF7_LAST_SUBLEAF`). Among what is cleared:
 * AVX-512 and AMX (state past AVX's, which nothing switches), HLE and RTM,
 * MPX, PT, SGX, CET, WAITPKG, TSC_ADJUST, LA57, the hybrid flag, and
 * L1D_FLUSH, whose flush would have to run on the CPU the guest's data was
 * in, before its task could move. */
const LEAF7_EBX_KEEP: u32 = (1 << 0)   // FSGSBASE
    | (1 << 3)   // BMI1
    | (1 << 6)   // the x87 data pointer kept at exceptions only
    | (1 << 7)   // SMEP
    | (1 << 8)   // BMI2
    | (1 << 9)   // ERMS: fast REP MOVSB and STOSB
    | (1 << 13)  // the x87's CS and DS deprecated
    | (1 << 18)  // RDSEED
    | (1 << 19)  // ADX
    | (1 << 20)  // SMAP
    | (1 << 23)  // CLFLUSHOPT
    | (1 << 24)  // CLWB
    | (1 << 29); // SHA
const LEAF7_ECX_KEEP: u32 = (1 << 2)   // UMIP
    | (1 << 8);  // GFNI
/* Leaf 7, subleaf 0: the AVX line, where AVX's state is switched and the
 * host has each -- AVX2 in EBX, and in ECX VAES and VPCLMULQDQ, whose VEX
 * forms are on the YMM registers (their EVEX forms are AVX-512's). */
const LEAF7_EBX_AVX: u32 = 1 << 5; // AVX2
const LEAF7_ECX_AVX: u32 = (1 << 9)   // VAES
    | (1 << 10); // VPCLMULQDQ
/* Leaf 7, subleaf 0: INVPCID, which drops the translations of the guest's
 * own VPID or ASID; and RDPID, which reads its own TSC_AUX (`Own`) -- each
 * where it runs in a guest. */
const LEAF7_EBX_INVPCID: u32 = 1 << 10;
const LEAF7_ECX_RDPID: u32 = 1 << 22;
/* Leaf 7, subleaf 0, ECX: PKU where PKRU is switched for a guest
 * (`fp::offered`), and OSPKE with it as the guest's own CR4.PKE has it, as
 * a CPU's mirrors its CR4 -- not as the host's, which has PKE set while the
 * extension is on. */
const LEAF7_ECX_PKU: u32 = 1 << 3;
const LEAF7_ECX_OSPKE: u32 = 1 << 4;
const CR4_PKE: u64 = 1 << 22;
/// The last subleaf of leaf 7 answered: 2, or the host's where that is less.
const LEAF7_LAST_SUBLEAF: u32 = SUBLEAF_7_2;
const SUBLEAF_7_1: u32 = 1;
const SUBLEAF_7_2: u32 = 2;
/* Leaf 7, subleaf 1: in EAX, facts of the silicon -- REP MOVSB fast at no
 * bytes, REP STOSB, CMPSB and SCASB fast at a few -- and where AVX's state is
 * switched, the instructions VEX codes on the YMM registers: SHA512, SM3 and
 * SM4, AVX-VNNI and AVX-IFMA; in EDX the same of AVX-VNNI-INT8,
 * AVX-NE-CONVERT and AVX-VNNI-INT16 -- each where the host has it. Among
 * what is cleared: AVX512_BF16 and AMX-FP16 (state past AVX's), LASS and
 * LAM (CR4's and CR3's), FRED and LKGS, WRMSRNS and MSRLIST, HRESET,
 * CMPCCXADD and RAO-INT, PPIN, the performance monitor's extensions, and
 * every bit of EBX and ECX. */
const LEAF7_1_EAX_KEEP: u32 = (1 << 10)  // FZRM
    | (1 << 11)  // FSRS
    | (1 << 12); // FSRC
const LEAF7_1_EAX_AVX: u32 = (1 << 0)   // SHA512
    | (1 << 1)   // SM3
    | (1 << 2)   // SM4
    | (1 << 4)   // AVX-VNNI
    | (1 << 23); // AVX-IFMA
const LEAF7_1_EDX_AVX: u32 = (1 << 4)   // AVX-VNNI-INT8
    | (1 << 5)   // AVX-NE-CONVERT
    | (1 << 10); // AVX-VNNI-INT16

/* Leaf 0xD, where anything of XSAVE's state is offered: subleaf 1's EAX,
 * the XSAVE line as the host has it -- XSAVEOPT, XSAVEC, XGETBV with ECX 1,
 * each on the components the guest's XCR0 names -- but not XSAVES, whose
 * supervisor state is not switched, nor XFD. The sizes the leaf gives are
 * those of the components offered: the legacy region and the header, 576
 * bytes, and AVX's 256 after them -- in either of XSAVE's formats, AVX's
 * component wanting no alignment of its own in the compacted one. */
const LEAF_D1_EAX_KEEP: u32 = (1 << 0)   // XSAVEOPT
    | (1 << 1)   // XSAVEC
    | (1 << 2);  // XGETBV with ECX 1
const LEAF_D1_EAX_XSAVEC: u32 = 1 << 1;
const XSAVE_LEGACY_BYTES: u32 = 576;
const XSAVE_AVX_BYTES: u32 = 256;
const SUBLEAF_AVX: u32 = 2;
/// PKRU's subleaf; a component's ECX bit 1, its compacted offset a multiple
/// of 64.
const SUBLEAF_PKRU: u32 = 9;
const LEAF_D_ECX_ALIGNED: u32 = 1 << 1;
const XSAVE_ALIGNMENT: u32 = 64;
/* In EDX: fast short REP MOVSB; MD_CLEAR -- VERW clears the CPU's buffers,
 * which it does in a guest as anywhere, and which a guest's kernel needs to
 * know to use it; SERIALIZE; and ARCH_CAPABILITIES, which the guest reads
 * from the host's (`MSR_ARCH_CAPABILITIES`). */
const LEAF7_EDX_KEEP: u32 = (1 << 4)   // FSRM
    | (1 << 10)  // MD_CLEAR
    | (1 << 14)  // SERIALIZE
    | (1 << 29); // ARCH_CAPABILITIES

/* The speculation controls, Intel's in leaf 7 and AMD's in 0x8000_0008 and
 * 0x8000_0021, each where the host has it and the bits of IA32_SPEC_CTRL or
 * IA32_PRED_CMD it names are the guest's to write (`spec::guest_controls`,
 * `Own`): the guest's SPEC_CTRL is its own, made the CPU's over the host's
 * for its run, and an IBPB it asks for is performed before its next entry on
 * any CPU it may have trained. Leaf 7, subleaf 0, EDX: IBRS and IBPB, STIBP,
 * SSBD. Subleaf 2, EDX: PSFD, IPRED_DIS, RRSBA_DIS, DDPD_U and BHI_DIS_S,
 * and MCDT_NO, a fact of the silicon. 0x8000_0008 EBX: IBPB -- with it
 * IBPB_RET, a fact of that IBPB -- IBRS, STIBP, SSBD and PSFD. 0x8000_0021
 * EAX: SBPB, and with IBPB IBPB_BRTYPE, a fact of it. Each bit with the
 * controls it names: (leaf, register's bit, SPEC_CTRL's, PRED_CMD's). */
const LEAF7_EDX_SPEC: &[(u32, u64, u64)] = &[
    (1 << 26, SPEC_CTRL_IBRS, PRED_CMD_IBPB), // IBRS and IBPB
    (1 << 27, SPEC_CTRL_STIBP, 0),            // STIBP
    (1 << 31, SPEC_CTRL_SSBD, 0),             // SSBD
];
const LEAF7_2_EDX_KEEP: u32 = 1 << 5; // MCDT_NO
const LEAF7_2_EDX_SPEC: &[(u32, u64, u64)] = &[
    (1 << 0, SPEC_CTRL_PSFD, 0),      // PSFD
    (1 << 1, SPEC_CTRL_IPRED_DIS, 0), // IPRED_CTRL
    (1 << 2, SPEC_CTRL_RRSBA_DIS, 0), // RRSBA_CTRL
    (1 << 3, SPEC_CTRL_DDPD_U, 0),    // DDPD_U
    (1 << 4, SPEC_CTRL_BHI_DIS_S, 0), // BHI_CTRL
];
const EXT8_EBX_SPEC: &[(u32, u64, u64)] = &[
    (1 << 12, 0, PRED_CMD_IBPB),  // IBPB
    (1 << 30, 0, PRED_CMD_IBPB),  // IBPB_RET: that IBPB clears the return predictions too
    (1 << 14, SPEC_CTRL_IBRS, 0), // IBRS
    (1 << 15, SPEC_CTRL_STIBP, 0), // STIBP
    (1 << 24, SPEC_CTRL_SSBD, 0), // SSBD
    (1 << 28, SPEC_CTRL_PSFD, 0), // PSFD
];
const EXT21_EAX_SPEC: &[(u32, u64, u64)] = &[
    (1 << 27, 0, PRED_CMD_SBPB), // SBPB
    (1 << 28, 0, PRED_CMD_IBPB), // IBPB_BRTYPE: that IBPB flushes the branch types too
];
/* IA32_SPEC_CTRL's bits and IA32_PRED_CMD's, as those CPUID bits name them. */
const SPEC_CTRL_IBRS: u64 = 1 << 0;
const SPEC_CTRL_STIBP: u64 = 1 << 1;
const SPEC_CTRL_SSBD: u64 = 1 << 2;
const SPEC_CTRL_IPRED_DIS: u64 = (1 << 3) | (1 << 4);
const SPEC_CTRL_RRSBA_DIS: u64 = (1 << 5) | (1 << 6);
const SPEC_CTRL_PSFD: u64 = 1 << 7;
const SPEC_CTRL_DDPD_U: u64 = 1 << 8;
const SPEC_CTRL_BHI_DIS_S: u64 = 1 << 10;
const PRED_CMD_IBPB: u64 = 1 << 0;
const PRED_CMD_SBPB: u64 = 1 << 7;

/// The bits of `table` -- (register's bit, SPEC_CTRL's, PRED_CMD's) -- whose
/// controls are all the guest's.
fn speculation(table: &[(u32, u64, u64)]) -> u32 {
    let c = spec::guest_controls();
    table.iter()
        .filter(|&&(_, ctrl, pred)| c.spec_ctrl & ctrl == ctrl && c.pred_cmd & pred == pred)
        .fold(0, |bits, &(bit, _, _)| bits | bit)
}

/* Extended leaf 0x80000001, ECX: the features kept, on the same terms as
 * leaf 1's -- anything not named is cleared. That list was once "all of it
 * but SVM", and on a real Zen 2 the guest found MWAITX there (bit 29), used
 * MONITORX for its udelay, and stopped at an intercept nothing answers: TCG's
 * `-cpu max` never offered it. Among what is cleared: SVM, the extended APIC
 * space, OSVW, IBS, XOP and FMA4 and TBM (Bulldozer's own, which Zen
 * dropped), SKINIT, the watchdog, LWP, the topology and performance counter
 * extensions, MWAITX. */
const EXT1_ECX_KEEP: u32 = (1 << 0)   // LAHF/SAHF in 64-bit mode
    | (1 << 5)   // ABM: LZCNT
    | (1 << 6)   // SSE4A
    | (1 << 7)   // misaligned SSE
    | (1 << 8);  // PREFETCHW

/* Extended leaf 0x80000001, EDX: what leaf 1 keeps of the bits AMD mirrors
 * there, SYSCALL, NX, the MMX extensions, 1 GiB pages and long mode. The
 * APIC's mirror (bit 9) is set as leaf 1's is, and RDTSCP where it runs in a
 * guest (`EXT1_EDX_RDTSCP`); FFXSR and 3DNow! are cleared. */
const EXT1_EDX_KEEP: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4)
    | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8)   // FPU..CX8, as leaf 1
    | (1 << 11)  // SYSCALL
    | (1 << 12) | (1 << 13) | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 17) // MTRR..PSE-36
    | (1 << 20)  // NX
    | (1 << 22)  // MMX extensions
    | (1 << 23) | (1 << 24) // MMX, FXSR
    | (1 << 26)  // 1 GiB pages
    | (1 << 29); // long mode
/// RDTSCP: the TSC and the guest's own TSC_AUX (`Own`), where it runs in a
/// guest -- intercepted, under AMD-V, only where it does not.
const EXT1_EDX_RDTSCP: u32 = 1 << 27;

/* Extended leaf 0x80000008: in EAX the widths of a physical and a linear
 * address, the host's -- what a guest's page tables are held to -- and not
 * the width of a nested guest's. In EBX, what the CPU does as it is: CLZERO,
 * an instruction with nothing to intercept; the FP error pointers FXSAVE
 * always keeps; and two attacks it is not open to, speculative store bypass
 * and branch type confusion -- facts of the silicon, true of a guest on it;
 * and the speculation controls (`EXT8_EBX_SPEC`), each only beside the MSR
 * that makes it true: IBPB was offered once while a write to PRED_CMD went
 * nowhere, and IBRS, STIBP and SSBD while SPEC_CTRL kept nothing, so a
 * guest on AMD reported itself mitigated and was not. Not VIRT_SSBD, whose
 * MSR nothing answers. Among the rest:
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
 * all or across user and kernel; in ECX, not affected by TSA-SQ or TSA-L1;
 * and SBPB and IBPB_BRTYPE where PRED_CMD is the guest's (`EXT21_EAX_SPEC`).
 * Not AutoIBRS, whose EFER bit a guest may not set (offered with its
 * write refused, Linux on Zen 4 runs with neither it nor retpolines, by its
 * source), nor CPUID faulting, HWCR's. */
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

const NONE: Cpuid = Cpuid { eax: 0, ebx: 0, ecx: 0, edx: 0 };

/// Who asks a CPUID, as far as the answer is theirs: the guest's CPU's APIC
/// ID; whether the guest has local APICs -- one of more than one CPU --
/// which leaf 1 then says, the x2APIC and its TSC-deadline timer with it;
/// the CPU's CR4 and XCR0, whose OSXSAVE leaf 1 mirrors and whose components
/// leaf 0xD gives the XSAVE area's size for; and where the guest's CPUs sit.
#[derive(Clone, Copy, Debug)]
pub struct Asker {
    pub apic_id: u32,
    pub apic: bool,
    pub cr4: u64,
    pub xcr0: u64,
    pub shape: Shape,
}

impl Asker {
    /// A guest's first CPU out of reset, where nothing asked for depends on
    /// more: its signature, its features, its address widths.
    pub const RESET: Asker = Asker { apic_id: 0, apic: true, cr4: 0, xcr0: fp::XCR0_X87, shape: Shape::cores(1) };
}

/* The hypervisor's own range: KVM's signature, "KVMKVMKVM" in EBX, ECX and
 * EDX of leaf 0x4000_0000 with the top of the range in EAX, and the
 * features of its paravirtual interface in 0x4000_0001 -- what a Linux
 * built with KVM_GUEST looks for, and takes no more of than the bits it is
 * given. Of KVM's features, the two backed here: NOP_IO_DELAY, a fact --
 * an `out` to port 0x80 delays nothing, and is an exit -- and for a guest
 * with APICs, PV_EOI, an EOI made through a word in the guest's memory
 * where it need not exit (`MSR_KVM_PV_EOI_EN`, `crate::run`). Not offered:
 * kvmclock, the TSC being invariant and the host's; steal time, and what
 * stands on it -- PV TLB flushes, PV sched yield -- a guest's CPUs having
 * cores of their own; PV unhalt, for the same; and async page faults, a
 * guest's memory never being paged out. No hypercall either: each is
 * answered -KVM_ENOSYS, as KVM answers one it does not know. */
const KVM_SIGNATURE: [u32; 3] = [0x4B4D_564B, 0x564B_4D56, 0x0000_004D];
const KVM_FEATURE_NOP_IO_DELAY: u32 = 1 << 1;
const KVM_FEATURE_PV_EOI: u32 = 1 << 6;
/// MSR_KVM_PV_EOI_EN: where the guest's PV EOI word is, and whether it is
/// on (bit 0) -- the run loop's, as the local APIC's MSRs are.
pub const MSR_KVM_PV_EOI_EN: u32 = 0x4B56_4D04;

/// Whether a guest with APICs is offered the TSC-deadline timer: where the
/// TSC's rate is known, which a deadline is turned into host time by.
pub fn tsc_deadline() -> bool {
    kcore::time::cycle_counter_hz().is_some()
}

/// What the guest's CPU `asker` is told for CPUID leaf `leaf`, subleaf
/// `sub` (ECX on the way in, for the leaves that take one).
pub fn cpuid(leaf: u32, sub: u32, asker: &Asker) -> Cpuid {
    if (LEAF_HYPERVISOR..LEAF_EXT).contains(&leaf) {
        return hypervisor_leaf(leaf, asker);
    }
    /* Above a range's top the guest is told there is nothing, and is
     * answered so: a leaf of the host's past it would be the host's no
     * matter what this policy says of it. */
    let top = if leaf >= LEAF_EXT { LEAF_EXT_TOP } else { LEAF_BASIC_TOP };
    if leaf > top {
        return NONE;
    }
    let Some(host) = cpu::cpuid_count(leaf, sub) else { return NONE };
    let (eax, ebx, ecx, edx) = (host.eax, host.ebx, host.ecx, host.edx);
    let (apic_ecx, apic_edx) = if asker.apic { (LEAF1_ECX_X2APIC, LEAF1_EDX_APIC) } else { (0, 0) };
    let apic = asker.apic;
    let avx = fp::offered() & fp::XCR0_AVX != 0;
    let runs = x86::offers();
    let shape = &asker.shape;
    let deadline = if apic && tsc_deadline() { LEAF1_ECX_TSC_DEADLINE } else { 0 };
    /* The APIC IDs a package has room for, and HTT where that is more than
     * one: the field is the guest's then, as a CPU's is. */
    let ids = 1u32 << shape.package_shift();
    let htt = if ids > 1 { LEAF1_EDX_HTT } else { 0 };

    match leaf {
        LEAF_BASIC => Cpuid { eax: eax.min(LEAF_BASIC_TOP), ebx, ecx, edx },
        LEAF_FEATURES => Cpuid {
            eax,
            ebx: (ebx & LEAF1_EBX_KEEP) | ((ids & 0xFF) << LEAF1_EBX_IDS_SHIFT)
                | ((asker.apic_id & 0xFF) << LEAF1_EBX_APIC_ID_SHIFT),
            ecx: (ecx & LEAF1_ECX_KEEP) | leaf1_xsave(ecx, asker.cr4) | apic_ecx | deadline | LEAF1_ECX_HYPERVISOR,
            edx: (edx & LEAF1_EDX_KEEP) | apic_edx | htt,
        },
        /* What the caches, TLBs and the brand are, and how fast the CPU is
         * clocked: descriptions, nothing a guest can turn on. */
        LEAF_CACHE_DESCRIPTORS | LEAF_FREQUENCY | LEAF_BRAND_FIRST..=LEAF_BRAND_LAST | LEAF_L1 | LEAF_L2 => {
            Cpuid { eax, ebx, ecx, edx }
        }
        LEAF_CACHE => Cpuid { eax: cache_eax(eax, shape, LEAF_CACHE, true), ebx, ecx, edx },
        LEAF_TOPOLOGY => topology(sub, asker),
        /* Of the power leaf, ARAT alone, for a guest with an APIC timer:
         * the rest -- APERF/MPERF, HWP, the energy bias, the thermal sensor
         * -- each names MSRs this hypervisor does not answer. */
        LEAF_POWER => Cpuid { eax: if apic { LEAF6_EAX_ARAT } else { 0 }, ebx: 0, ecx: 0, edx: 0 },
        LEAF_STRUCTURED if sub == 0 => {
            let (avx_ebx, avx_ecx) = if avx { (LEAF7_EBX_AVX, LEAF7_ECX_AVX) } else { (0, 0) };
            let invpcid = if runs.invpcid { LEAF7_EBX_INVPCID } else { 0 };
            let rdpid = if runs.rdpid { LEAF7_ECX_RDPID } else { 0 };
            let pku = if fp::offered() & fp::XCR0_PKRU != 0 { LEAF7_ECX_PKU } else { 0 };
            let ospke = if pku != 0 && asker.cr4 & CR4_PKE != 0 { LEAF7_ECX_OSPKE } else { 0 };
            Cpuid {
                eax: eax.min(LEAF7_LAST_SUBLEAF),
                ebx: ebx & (LEAF7_EBX_KEEP | avx_ebx | invpcid),
                ecx: (ecx & (LEAF7_ECX_KEEP | avx_ecx | rdpid | pku)) | ospke,
                edx: edx & (LEAF7_EDX_KEEP | speculation(LEAF7_EDX_SPEC)),
            }
        }
        /* Where the host has the subleaf: past its last, its answer is no
         * CPU's. */
        LEAF_STRUCTURED if sub == SUBLEAF_7_1 && structured_last() >= SUBLEAF_7_1 => {
            let (avx_eax, avx_edx) = if avx { (LEAF7_1_EAX_AVX, LEAF7_1_EDX_AVX) } else { (0, 0) };
            Cpuid { eax: eax & (LEAF7_1_EAX_KEEP | avx_eax), ebx: 0, ecx: 0, edx: edx & avx_edx }
        }
        LEAF_STRUCTURED if sub == SUBLEAF_7_2 && structured_last() >= SUBLEAF_7_2 => {
            Cpuid { eax: 0, ebx: 0, ecx: 0, edx: edx & (LEAF7_2_EDX_KEEP | speculation(LEAF7_2_EDX_SPEC)) }
        }
        LEAF_XSAVE => xsave_leaf(sub, eax, asker.xcr0),
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
        LEAF_EXT_FEATURES => {
            let rdtscp = if runs.rdtscp { EXT1_EDX_RDTSCP } else { 0 };
            Cpuid {
                eax,
                ebx,
                ecx: ecx & (EXT1_ECX_KEEP | EXT1_ECX_TOPOEXT),
                edx: (edx & (EXT1_EDX_KEEP | rdtscp)) | apic_edx,
            }
        }
        LEAF_EXT_POWER => Cpuid { eax: 0, ebx: 0, ecx: 0, edx: edx & EXT7_EDX_INVARIANT_TSC },
        LEAF_EXT_ADDRESS => Cpuid {
            eax: eax & EXT8_EAX_KEEP,
            ebx: ebx & (EXT8_EBX_KEEP | speculation(EXT8_EBX_SPEC)),
            /* AMD's: the package's CPUs, and the APIC ID's width below the
             * package's. Intel's is reserved. */
            ecx: if vendor() == Vendor::Amd {
                (shape.cpus().saturating_sub(1) & 0xFF) | (shape.package_shift() << EXT8_ECX_APIC_ID_SIZE_SHIFT)
            } else {
                0
            },
            edx: 0,
        },
        /* AMD's topology leaves, where the host has them (TOPOEXT): its
         * caches in leaf 4's format, with the guest's sharing counts, and
         * the asking CPU's APIC ID, core and threads -- one node. */
        LEAF_EXT_CACHE if topoext() => Cpuid { eax: cache_eax(eax, shape, LEAF_EXT_CACHE, false), ebx, ecx, edx },
        LEAF_EXT_APIC_ID if topoext() => Cpuid {
            eax: asker.apic_id,
            ebx: ((asker.apic_id >> shape.smt_shift()) & 0xFF) | ((shape.threads() - 1) << EXT1E_EBX_THREADS_SHIFT),
            ecx: 0,
            edx: 0,
        },
        LEAF_EXT_FEATURES2 => Cpuid {
            eax: eax & (EXT21_EAX_KEEP | speculation(EXT21_EAX_SPEC)),
            ebx: 0,
            ecx: ecx & EXT21_ECX_KEEP,
            edx: 0,
        },
        _ => NONE,
    }
}

/// The hypervisor's range, as KVM answers it: its signature and the top of
/// the range, and the features of its paravirtual interface backed here --
/// PV EOI for a guest with APICs. Nothing else in the range.
fn hypervisor_leaf(leaf: u32, asker: &Asker) -> Cpuid {
    match leaf {
        LEAF_HYPERVISOR => Cpuid {
            eax: LEAF_KVM_FEATURES,
            ebx: KVM_SIGNATURE[0],
            ecx: KVM_SIGNATURE[1],
            edx: KVM_SIGNATURE[2],
        },
        LEAF_KVM_FEATURES => Cpuid {
            eax: KVM_FEATURE_NOP_IO_DELAY | if asker.apic { KVM_FEATURE_PV_EOI } else { 0 },
            ebx: 0,
            ecx: 0,
            edx: 0,
        },
        _ => NONE,
    }
}

/// Leaf 0xB, the x2APIC topology, for the asking CPU: subleaf 0 its core's
/// threads -- the APIC ID's bits below the core's, and how many -- subleaf
/// 1 the package's cores -- the bits below the package's, and the
/// package's CPUs -- and every other the end of the list; each with the
/// CPU's APIC ID, which is its number.
fn topology(sub: u32, asker: &Asker) -> Cpuid {
    let shape = &asker.shape;
    let (shift, count, level) = match sub {
        0 => (shape.smt_shift(), shape.threads(), TOPOLOGY_SMT),
        1 => (shape.package_shift(), shape.cpus(), TOPOLOGY_CORE),
        _ => (0, 0, 0),
    };
    Cpuid {
        eax: shift,
        ebx: count,
        ecx: (level << TOPOLOGY_LEVEL_SHIFT) | (sub & TOPOLOGY_SUBLEAF_MASK),
        edx: asker.apic_id,
    }
}

/// A deterministic cache leaf's EAX -- `leaf` 4's, or AMD's 0x8000_001D --
/// with the guest's counts in place of the host's: the CPUs that share the
/// cache -- a core's threads, and for the last level the whole package's
/// where they share the host's -- and with `cores`, leaf 4's, the
/// package's cores. A subleaf with no cache in it ends the list, as it is.
fn cache_eax(eax: u32, shape: &Shape, leaf: u32, cores: bool) -> u32 {
    let kept = eax & LEAF4_EAX_KEEP;
    if eax & CACHE_TYPE_MASK == 0 {
        return kept;
    }
    let level = (eax >> CACHE_LEVEL_SHIFT) & CACHE_LEVEL_MASK;
    let sharing = if level >= last_level(leaf) && shape.llc_shared() {
        1 << shape.package_shift()
    } else {
        shape.threads()
    };
    let core_ids = 1u32 << (shape.package_shift() - shape.smt_shift());
    let cores = if cores { (core_ids - 1) << CACHE_CORES_SHIFT } else { 0 };
    kept | ((sharing - 1) << CACHE_SHARING_SHIFT) | cores
}

/// The host's last level of cache, by its `leaf` -- 4, or 0x8000_001D: the
/// highest a subleaf names before the one that ends the list.
fn last_level(leaf: u32) -> u32 {
    let mut last = 0;
    for sub in 0..CACHE_SUBLEAVES {
        let Some(c) = cpu::cpuid_count(leaf, sub) else { break };
        if c.eax & CACHE_TYPE_MASK == 0 {
            break;
        }
        last = last.max((c.eax >> CACHE_LEVEL_SHIFT) & CACHE_LEVEL_MASK);
    }
    last
}

/// Whether the host has AMD's topology leaves (TOPOEXT), which the guest's
/// are then answered in.
fn topoext() -> bool {
    cpu::cpuid_count(LEAF_EXT_FEATURES, 0).is_some_and(|c| c.ecx & EXT1_ECX_TOPOEXT != 0)
}

/// The host's last subleaf of leaf 7, which subleaf 0's EAX gives.
fn structured_last() -> u32 {
    cpu::cpuid_count(LEAF_STRUCTURED, 0).map_or(0, |c| c.eax)
}

/// Leaf 1 ECX's XSAVE line for a guest whose CR4 is `cr4`, of the host's
/// `host_ecx`: XSAVE where anything of its state is switched for a guest
/// (`fp::offered`), with OSXSAVE as the guest's CR4 has it; and where AVX's
/// is, AVX, and FMA and F16C as the host has them.
fn leaf1_xsave(host_ecx: u32, cr4: u64) -> u32 {
    let offered = fp::offered();
    if offered == 0 {
        return 0;
    }
    let osxsave = if cr4 & CR4_OSXSAVE != 0 { LEAF1_ECX_OSXSAVE } else { 0 };
    let avx = if offered & fp::XCR0_AVX != 0 {
        LEAF1_ECX_AVX | (host_ecx & (LEAF1_ECX_FMA | LEAF1_ECX_F16C))
    } else {
        0
    };
    LEAF1_ECX_XSAVE | osxsave | avx
}

/// PKRU's component as the host's CPUID gives it, where it is offered: its
/// size, its offset in the standard format -- past AVX's, as `fp` holds it
/// to -- and whether the compacted format puts it at a multiple of 64.
fn pkru_component() -> Option<(u32, u32, bool)> {
    if fp::offered() & fp::XCR0_PKRU == 0 {
        return None;
    }
    let c = cpu::cpuid_count(LEAF_XSAVE, SUBLEAF_PKRU)?;
    Some((c.eax, c.ebx, c.ecx & LEAF_D_ECX_ALIGNED != 0))
}

/// The size of XSAVE's standard area for `components`, of those offered:
/// the end of the last of them -- the legacy region and the header, AVX's
/// after them, PKRU's at its own offset.
fn xsave_bytes(components: u64) -> u32 {
    let mut end = XSAVE_LEGACY_BYTES + if components & fp::XCR0_AVX != 0 { XSAVE_AVX_BYTES } else { 0 };
    if components & fp::XCR0_PKRU != 0 {
        if let Some((size, offset, _)) = pkru_component() {
            end = end.max(offset.saturating_add(size));
        }
    }
    end
}

/// The size of XSAVE's compacted area for `components`: each after the
/// last, in order -- AVX's wanting no alignment, PKRU's where CPUID says.
fn xsave_compacted_bytes(components: u64) -> u32 {
    let mut end = XSAVE_LEGACY_BYTES + if components & fp::XCR0_AVX != 0 { XSAVE_AVX_BYTES } else { 0 };
    if components & fp::XCR0_PKRU != 0 {
        if let Some((size, _, aligned)) = pkru_component() {
            if aligned {
                end = end.next_multiple_of(XSAVE_ALIGNMENT);
            }
            end = end.saturating_add(size);
        }
    }
    end
}

/// Leaf 0xD, subleaf `sub`, for a guest whose XCR0 is `xcr0`, the host's
/// subleaf EAX being `host_eax`: nothing where nothing is offered. Subleaf
/// 0: the components it may turn on, and the area's size for those its
/// XCR0 has on and for all of them. Subleaf 1: the XSAVE line kept of the
/// host's, and -- with XSAVEC -- the compacted area's size for its XCR0, as
/// a CPU gives it (with no supervisor components, IA32_XSS being none of
/// the guest's). Subleaf 2: AVX's component, its size and its offset;
/// subleaf 9 PKRU's, as the host's CPUID has it. Every other subleaf is
/// blank: no other component is offered.
fn xsave_leaf(sub: u32, host_eax: u32, xcr0: u64) -> Cpuid {
    let offered = fp::offered();
    if offered == 0 {
        return NONE;
    }
    let on = xcr0 & offered;
    match sub {
        0 => Cpuid {
            eax: offered as u32,
            ebx: xsave_bytes(on),
            ecx: xsave_bytes(offered),
            edx: (offered >> 32) as u32,
        },
        1 => {
            let eax = host_eax & LEAF_D1_EAX_KEEP;
            let ebx = if eax & LEAF_D1_EAX_XSAVEC != 0 { xsave_compacted_bytes(on) } else { 0 };
            Cpuid { eax, ebx, ecx: 0, edx: 0 }
        }
        SUBLEAF_AVX if offered & fp::XCR0_AVX != 0 => {
            Cpuid { eax: XSAVE_AVX_BYTES, ebx: XSAVE_LEGACY_BYTES, ecx: 0, edx: 0 }
        }
        SUBLEAF_PKRU => match pkru_component() {
            Some((size, offset, aligned)) => {
                Cpuid { eax: size, ebx: offset, ecx: if aligned { LEAF_D_ECX_ALIGNED } else { 0 }, edx: 0 }
            }
            None => NONE,
        },
        _ => NONE,
    }
}

/// The width of a physical address the guest is told -- leaf 0x80000008's
/// EAX bits 7:0, the host's -- below `1 << which` its RAM must end, or it
/// cannot reach it: an entry of its own page table with an address bit past
/// the width is a reserved bit, and a fault. 36 where the leaf is not there,
/// as the architecture has a CPU without it; and never past the 52 bits the
/// architecture has at most, whatever a CPU -- or a hypervisor under this
/// one -- says (hv-fuzz found the shift by a width of 64 or more).
pub fn phys_bits() -> u32 {
    match cpuid(LEAF_EXT_ADDRESS, 0, &Asker::RESET).eax & EXT8_EAX_PHYS_BITS {
        0 => DEFAULT_PHYS_BITS,
        bits => bits.min(MAX_PHYS_BITS),
    }
}
const EXT8_EAX_PHYS_BITS: u32 = 0xFF;
const DEFAULT_PHYS_BITS: u32 = 36;
const MAX_PHYS_BITS: u32 = 52;

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
/// CPUID's back -- PKE above all, where a guest's PKRU is not switched
/// between guests. Today that is VME and PVI, TSD, DE, PSE, PAE, MCE, PGE, PCE,
/// OSFXSR and OSXMMEXCPT, UMIP, FSGSBASE, PCIDE, OSXSAVE, SMEP and SMAP,
/// and PKE, each where the host has its feature -- OSXSAVE where XSAVE's
/// state is switched for a guest, PKE where its PKRU is; never LA57 or CET,
/// whose features are not offered.
pub fn cr4_allowed() -> u64 {
    let features = cpuid(LEAF_FEATURES, 0, &Asker::RESET);
    let structured = cpuid(LEAF_STRUCTURED, 0, &Asker::RESET);
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

/* IA32_TSC_AUX: what RDTSCP and RDPID read beside the TSC -- a kernel's CPU
 * number, as Linux keeps it -- the guest's own, its low 32 bits, where its
 * CPUID offers either instruction (`Own`). */
const MSR_TSC_AUX: u32 = 0xC000_0103;
const TSC_AUX_BITS: u64 = 0xFFFF_FFFF;

/// A guest CPU's MSRs that its save area does not hold, which the backend
/// keeps and its entries make the CPU's, and the IBPBs it asked for
/// (`hvarch::x86::msrs`): read here, and written back through what checks
/// them again (`Backend::set_own_msrs`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Own {
    /// IA32_TSC_AUX.
    pub tsc_aux: u64,
    /// IA32_SPEC_CTRL: the bits the guest set.
    pub spec_ctrl: u64,
    /// How many IBPBs it asked for by IA32_PRED_CMD: one more for each write
    /// of it that asks for one.
    pub ibpbs: u64,
}

/* IA32_SPEC_CTRL and IA32_PRED_CMD, where the guest's CPUID offers a control
 * of them (`spec::guest_controls`): SPEC_CTRL holds any of the bits offered
 * and reads back what it was given; PRED_CMD is written alone -- a read is a
 * #GP, as a CPU's -- and takes IBPB, SBPB where it is offered, which an IBPB
 * more than does, and 0; each barrier one asked of the backend. */
const MSR_SPEC_CTRL: u32 = 0x48;
const MSR_PRED_CMD: u32 = 0x49;

/// Whether the guest's CPU has IA32_TSC_AUX: where its CPUID offers RDTSCP
/// or RDPID, which read it.
fn has_tsc_aux() -> bool {
    let runs = x86::offers();
    runs.rdtscp || runs.rdpid
}

/// Whether IA32_SPEC_CTRL takes `value`: the MSR is there -- a control of it
/// offered -- and no bit of it is one not offered.
fn spec_ctrl_takes(value: u64) -> bool {
    let offered = spec::guest_controls().spec_ctrl;
    offered != 0 && value & !offered == 0
}

/// Whether IA32_PRED_CMD takes `value`: the same.
fn pred_cmd_takes(value: u64) -> bool {
    let offered = spec::guest_controls().pred_cmd;
    offered != 0 && value & !offered == 0
}

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
/// IA32_ARCH_CAPABILITIES, where the host's CPUID offers it, as the
/// guest reads it: the host's, but only the bits that say what the CPU is
/// not open to, or what an instruction the guest has does -- facts of the
/// silicon and its microcode, true of a guest on it. Without them Linux
/// takes the CPU for one open to everything its model was ever open to: on
/// the EX44 it isolated its page tables for Meltdown (with no PCID, a whole
/// TLB flush at every system call) and reported MDS and RFDS unmitigated.
/// IBRS_ALL with them, the guest's SPEC_CTRL its own: an IBRS it sets once
/// is set for every one of its runs, as on the host's CPU -- which keeps its
/// own set under it anyway. Not SKIP_L1DFL_VMENTRY, a hypervisor's, and the
/// guest runs none; nor any bit that names an MSR
/// this policy does not answer -- TSX_CTRL, FB_CLEAR_CTRL, GDS_CTRL,
/// DOITM, the energy filter's -- which a kernel told of one writes.
const MSR_ARCH_CAPABILITIES: u32 = 0x10A;
const ARCH_CAP_GUEST: u64 = (1 << 0)   // RDCL_NO: not open to Meltdown
    | (1 << 1)   // IBRS_ALL: enhanced IBRS, SPEC_CTRL's IBRS the guest's
    | (1 << 2)   // RSBA: a RET may be predicted from elsewhere than the RSB
    | (1 << 4)   // SSB_NO
    | (1 << 5)   // MDS_NO
    | (1 << 6)   // PSCHANGE_MC_NO
    | (1 << 8)   // TAA_NO
    | (1 << 13)  // SBDR_SSDP_NO
    | (1 << 14)  // FBSDP_NO
    | (1 << 15)  // PSDP_NO
    | (1 << 17)  // FB_CLEAR: VERW clears the fill buffers
    | (1 << 19)  // RRSBA
    | (1 << 20)  // BHI_NO
    | (1 << 24)  // PBRSB_NO
    | (1 << 26)  // GDS_NO
    | (1 << 27)  // RFDS_NO
    | (1 << 28)  // RFDS_CLEAR: VERW clears the register file
    | (1 << 62); // ITS_NO
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
/// kernel (`hal::setup_serializing_lfence`), as KVM reports it -- HWCR's
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
        /* Where the host's CPUID offers it, which is what offers it to the
         * guest too (`LEAF7_EDX_KEEP`), whoever made the CPU. */
        MSR_ARCH_CAPABILITIES => Constant::ReadOnly(cpu::arch_capabilities()? & ARCH_CAP_GUEST),
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

/// What a guest whose state is `save` and `own` read from MSR `msr`, and
/// whether the read is allowed: `None` -- for every MSR not named here --
/// means inject a #GP, which is what a real CPU does for one it does not
/// have and what a guest's `rdmsr_safe` is ready for.
pub fn rdmsr(save: &Save, own: &Own, msr: u32) -> Option<u64> {
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
        MSR_TSC_AUX if has_tsc_aux() => own.tsc_aux,
        MSR_SPEC_CTRL if spec::guest_controls().spec_ctrl != 0 => own.spec_ctrl,
        _ => match constant(msr)? {
            Constant::ReadOnly(v) | Constant::Fixed(v) => v,
        },
    };
    Some(value)
}

/// A guest wrote `value` to MSR `msr`. Returns whether it is allowed; `false`
/// injects a #GP, with nothing changed. The system MSRs land in the save
/// area, TSC_AUX in `own`; a constant one takes a write that leaves it as it
/// is (`Constant`); every other write -- to the TSC, to an MSR not named
/// here -- is refused, and nothing a guest writes goes nowhere.
pub fn wrmsr(save: &mut Save, own: &mut Own, msr: u32, value: u64) -> bool {
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
        /* Its low 32 bits: a write of another is the CPU's #GP. */
        MSR_TSC_AUX if has_tsc_aux() && value & !TSC_AUX_BITS == 0 => own.tsc_aux = value,
        MSR_SPEC_CTRL if spec_ctrl_takes(value) => own.spec_ctrl = value,
        MSR_PRED_CMD if pred_cmd_takes(value) => {
            if value != 0 {
                own.ibpbs = own.ibpbs.wrapping_add(1);
            }
        }
        _ => return matches!(constant(msr), Some(Constant::Fixed(v)) if v == value),
    }
    true
}
