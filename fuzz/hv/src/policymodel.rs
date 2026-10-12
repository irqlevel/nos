//! The CPUID and MSR policy (`hv/src/policy.rs`) held to a model of its
//! own, for a host CPU made up an input (`cpu::set_host`):
//!
//! - every leaf a guest can ask -- of the basic range, the extended one and
//!   the hypervisor's -- answered with no more of the host's than the model
//!   lets through, and a leaf the model does not name, or one past the top
//!   the guest is told, with nothing at all;
//! - every feature an answer offers backed by what it promises: the MSR it
//!   names answered, the bit of EFER it names taken (`IMPLIED`) -- what
//!   would have caught IBPB offered with nothing behind PRED_CMD, and Zen
//!   4's AutoIBRS offered with the EFER bit that turns it on refused;
//! - every MSR the model does not name refused both ways, a #GP, and every
//!   one it names answered as it says (`MSRS`).
//!
//! The model is written from the manuals, not from the policy: a bit or an
//! MSR the policy comes to offer is a finding here until the model is told
//! of it too, on purpose.

use crate::policy::{self, Cpuid};
use crate::targets::Input;
use crate::x86::cpu;
use crate::x86::svm::vmcb::Save;

/* The tops of the two ranges a guest may be told: the highest leaf the
 * model answers in each. */
const BASIC_TOP: u32 = 0x16;
const EXT_TOP: u32 = 0x8000_0021;

/* Leaf 1 ECX: SSE3, PCLMULQDQ, SSSE3, CMPXCHG16B, SSE4.1, SSE4.2, MOVBE,
 * POPCNT, AES-NI, RDRAND -- instructions that run on the CPU as they are,
 * on registers the FXSAVE area holds. x2APIC for a guest with APICs, and
 * the hypervisor bit, are the policy's own. */
const LEAF1_ECX: u32 = (1 << 0) | (1 << 1) | (1 << 9) | (1 << 13) | (1 << 19) | (1 << 20) | (1 << 22) | (1 << 23)
    | (1 << 25) | (1 << 30);
const LEAF1_ECX_X2APIC: u32 = 1 << 21;
const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
/* Leaf 1 EDX: FPU, VME, DE, PSE, TSC, MSR, PAE, MCE, CX8, SEP, MTRR, PGE,
 * MCA, CMOV, PAT, PSE-36, CLFLUSH, MMX, FXSR, SSE, SSE2. The APIC is the
 * policy's own, for a guest with APICs. */
const LEAF1_EDX: u32 = 0x1F | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 11) | (1 << 12) | (1 << 13)
    | (1 << 14) | (1 << 15) | (1 << 16) | (1 << 17) | (1 << 19) | (1 << 23) | (1 << 24) | (1 << 25) | (1 << 26);
const LEAF1_EDX_APIC: u32 = 1 << 9;
/* Leaf 6 EAX: ARAT, for a guest with APICs. */
const LEAF6_EAX_ARAT: u32 = 1 << 2;
/* 0x8000_0001 ECX: LAHF in long mode, LZCNT, SSE4A, misaligned SSE,
 * PREFETCHW. */
const EXT1_ECX: u32 = (1 << 0) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8);
/* 0x8000_0001 EDX: leaf 1's mirrors (FPU to CX8, MTRR to PSE-36, MMX,
 * FXSR), SYSCALL, NX, the MMX extensions, 1 GiB pages, long mode. */
const EXT1_EDX: u32 = 0x1FF | (1 << 11) | (0x3F << 12) | (1 << 20) | (1 << 22) | (1 << 23) | (1 << 24) | (1 << 26)
    | (1 << 29);
/* 0x8000_0007 EDX: the invariant TSC. */
const EXT7_EDX: u32 = 1 << 8;
/* 0x8000_0008 EAX: the physical and linear address widths. EBX: CLZERO,
 * the FP error pointers always saved, and two the CPU is not affected by
 * -- speculative store bypass, branch type confusion. */
const EXT8_EAX: u32 = 0xFFFF;
const EXT8_EBX: u32 = (1 << 0) | (1 << 2) | (1 << 26) | (1 << 29);
/* 0x8000_0021 EAX: no nested data breakpoints, LFENCE serializing, VERW
 * clearing what TSA leaves, a null selector clearing the base, and not
 * affected by SRSO, at all or across user and kernel. ECX: not affected
 * by TSA-SQ or TSA-L1. */
const EXT21_EAX: u32 = (1 << 0) | (1 << 2) | (1 << 5) | (1 << 6) | (1 << 29) | (1 << 30);
const EXT21_ECX: u32 = (1 << 1) | (1 << 2);

/// Leaves a guest is given as the host has them: descriptions of caches,
/// TLBs, frequencies and the brand -- nothing a guest can turn on.
const WHOLE: [u32; 7] = [2, 0x16, 0x8000_0002, 0x8000_0003, 0x8000_0004, 0x8000_0005, 0x8000_0006];

fn words(c: &Cpuid) -> [u32; 4] {
    [c.eax, c.ebx, c.ecx, c.edx]
}

/// The host's leaf, or zeros where it has none.
fn host(leaf: u32, sub: u32) -> Option<[u32; 4]> {
    cpu::cpuid_count(leaf, sub).map(|h| [h.eax, h.ebx, h.ecx, h.edx])
}

/// What the guest is told of the tops of the two ranges: no higher than
/// the model's or the host's, and the vendor as the host says it.
fn tops(apic_id: u32, apic: bool) -> (u32, u32) {
    let basic = words(&policy::cpuid(0, 0, apic_id, apic));
    let h = host(0, 0).unwrap_or([0; 4]);
    invariant!(basic[0] == h[0].min(BASIC_TOP) && basic[1..] == h[1..],
               "leaf 0 answered {:x?}, the host's {:x?}: wanted the top no higher than {:#x}", basic, h, BASIC_TOP);
    let ext = words(&policy::cpuid(0x8000_0000, 0, apic_id, apic));
    match host(0x8000_0000, 0) {
        Some(h) => invariant!(ext[0] == h[0].min(EXT_TOP) && ext[1..] == h[1..],
                              "leaf 0x8000_0000 answered {:x?}, the host's {:x?}: wanted the top no higher than {:#x}",
                              ext, h, EXT_TOP),
        None => invariant!(ext == [0; 4], "leaf 0x8000_0000 the host has not answered {:x?}", ext),
    }
    (basic[0], ext[0])
}

/// Leaf `leaf`, subleaf `sub`, as the guest is told it, held to the model.
fn check_leaf(leaf: u32, sub: u32, apic_id: u32, apic: bool, (basic_top, ext_top): (u32, u32)) {
    let g = words(&policy::cpuid(leaf, sub, apic_id, apic));
    let top = if leaf >= 0x8000_0000 { ext_top } else { basic_top };
    let Some(h) = host(leaf, sub).filter(|_| leaf <= top && (leaf < 0x4000_0000 || leaf >= 0x8000_0000)) else {
        invariant!(g == [0; 4], "leaf {:#x}.{}, past the top the guest is told ({:#x}) or the host's, answered {:x?}",
                   leaf, sub, top, g);
        return;
    };
    /* Bits of the guest's answer that the model does not let through. */
    let leaked = |got: u32, host: u32, mask: u32, own: u32| got & !((host & mask) | own);
    let mut bad: Option<String> = None;
    let mut want = |ok: bool, what: &str| {
        if !ok && bad.is_none() {
            bad = Some(what.to_string());
        }
    };
    let (x2apic, apic_bit, arat) = if apic { (LEAF1_ECX_X2APIC, LEAF1_EDX_APIC, LEAF6_EAX_ARAT) } else { (0, 0, 0) };
    match leaf {
        0 | 0x8000_0000 => {}
        1 => {
            want(g[0] == h[0], "the signature, the host's");
            want(g[1] & 0xFFFF == h[1] & 0xFFFF && (g[1] >> 16) & 0xFF == 1 && g[1] >> 24 == apic_id & 0xFF,
                 "EBX: the host's low half, one logical CPU, its own APIC ID");
            want(leaked(g[2], h[2], LEAF1_ECX, x2apic | LEAF1_ECX_HYPERVISOR) == 0, "ECX within the model's");
            want(g[2] & LEAF1_ECX_X2APIC == x2apic, "x2APIC exactly when the guest has APICs");
            want(g[2] & LEAF1_ECX_HYPERVISOR != 0, "the hypervisor bit");
            want(leaked(g[3], h[3], LEAF1_EDX, apic_bit) == 0, "EDX within the model's");
            want(g[3] & LEAF1_EDX_APIC == apic_bit, "the APIC exactly when the guest has APICs");
        }
        4 => {
            want(leaked(g[0], h[0], 0x3FFF, 0) == 0, "EAX: the cache's own fields, no core or sharing counts");
            want(g[1..] == h[1..], "EBX to EDX: the host's");
        }
        6 => want(g == [arat, 0, 0, 0], "ARAT for a guest with APICs, and nothing else"),
        0x15 if apic => want(g == [0; 4] || (g[0] == 1_000_000 && g[1] != 0 && g[2] == 1_000_000_000 && g[3] == 0),
                             "the TSC over the emulated APIC's 1 GHz, or nothing"),
        0x15 => want(g == h, "the host's"),
        l if WHOLE.contains(&l) => want(g == h, "the host's, whole"),
        0x8000_0001 => {
            want(g[0] == h[0] && g[1] == h[1], "EAX and EBX: the host's");
            want(leaked(g[2], h[2], EXT1_ECX, 0) == 0, "ECX within the model's");
            want(leaked(g[3], h[3], EXT1_EDX, apic_bit) == 0, "EDX within the model's");
        }
        0x8000_0007 => want(g == [0, 0, 0, h[3] & EXT7_EDX], "the invariant TSC alone"),
        0x8000_0008 => {
            want(g[0] == h[0] & EXT8_EAX, "EAX: the address widths, the host's");
            want(leaked(g[1], h[1], EXT8_EBX, 0) == 0, "EBX within the model's");
            want(g[2] == 0 && g[3] == 0, "ECX and EDX: nothing -- one core");
        }
        0x8000_0021 => {
            want(leaked(g[0], h[0], EXT21_EAX, 0) == 0, "EAX within the model's");
            want(leaked(g[2], h[2], EXT21_ECX, 0) == 0, "ECX within the model's");
            want(g[1] == 0 && g[3] == 0, "EBX and EDX: nothing");
        }
        _ => want(g == [0; 4], "nothing: a leaf the model does not name"),
    }
    if let Some(what) = bad {
        panic!("invariant: leaf {:#x}.{} answered {:x?} from the host's {:x?} (apic {}): wanted {}",
               leaf, sub, g, h, apic, what);
    }
}

/// What a feature the guest is told of promises it.
#[derive(Clone, Copy, Debug)]
enum Needs {
    /// An MSR it may read.
    Read(u32),
    /// An MSR it may write this value to.
    Write(u32, u64),
    /// A bit of EFER it may set, paging off.
    Efer(u64),
}

const EAX: usize = 0;
const EBX: usize = 1;
const ECX: usize = 2;
const EDX: usize = 3;

const MSR_EFER: u32 = 0xC000_0080;

/// Each CPUID bit and what it promises, from the manuals: the leaf, the
/// subleaf, the register and the bit, and the MSR or the EFER bit a guest
/// that sees it will use. The local APIC's MSRs are not here: the run loop
/// gives them to the APIC before the policy is asked.
const IMPLIED: &[(u32, u32, usize, u32, Needs, &str)] = &[
    (1, 0, EDX, 4, Needs::Read(0x10), "TSC"),
    (1, 0, EDX, 11, Needs::Read(0x174), "SEP"),
    (1, 0, EDX, 12, Needs::Read(0xFE), "MTRR"),
    (1, 0, EDX, 12, Needs::Read(0x2FF), "MTRR"),
    (1, 0, EDX, 14, Needs::Read(0x179), "MCA"),
    (1, 0, EDX, 14, Needs::Read(0x17A), "MCA"),
    (1, 0, EDX, 16, Needs::Read(0x277), "PAT"),
    (1, 0, EDX, 21, Needs::Read(0x600), "DS"),
    (1, 0, ECX, 5, Needs::Read(0x480), "VMX"),
    (1, 0, ECX, 15, Needs::Read(0x345), "PDCM"),
    (1, 0, ECX, 24, Needs::Read(0x6E0), "TSC deadline"),
    (6, 0, EAX, 0, Needs::Read(0x19C), "digital thermal sensor"),
    (6, 0, EAX, 7, Needs::Read(0x770), "HWP"),
    (6, 0, ECX, 0, Needs::Read(0xE7), "APERF/MPERF"),
    (6, 0, ECX, 0, Needs::Read(0xE8), "APERF/MPERF"),
    (6, 0, ECX, 3, Needs::Read(0x1B0), "energy/performance bias"),
    (7, 0, EBX, 1, Needs::Read(0x3B), "TSC_ADJUST"),
    (7, 0, ECX, 5, Needs::Read(0xE1), "WAITPKG"),
    (7, 0, ECX, 7, Needs::Read(0x6A2), "CET shadow stacks"),
    (7, 0, EDX, 20, Needs::Read(0x6A2), "CET indirect branch tracking"),
    (7, 0, EDX, 26, Needs::Read(0x48), "IBRS and IBPB"),
    (7, 0, EDX, 26, Needs::Write(0x49, 1), "IBRS and IBPB"),
    (7, 0, EDX, 27, Needs::Read(0x48), "STIBP"),
    (7, 0, EDX, 28, Needs::Write(0x10B, 1), "L1D_FLUSH"),
    (7, 0, EDX, 29, Needs::Read(0x10A), "ARCH_CAPABILITIES"),
    (7, 0, EDX, 30, Needs::Read(0xCF), "CORE_CAPABILITIES"),
    (7, 0, EDX, 31, Needs::Read(0x48), "SSBD"),
    (0xD, 1, EAX, 3, Needs::Read(0xDA0), "XSAVES"),
    (0x8000_0001, 0, ECX, 2, Needs::Read(0xC001_0114), "SVM"),
    (0x8000_0001, 0, ECX, 9, Needs::Read(0xC001_0140), "OSVW"),
    (0x8000_0001, 0, ECX, 10, Needs::Read(0xC001_1030), "IBS"),
    (0x8000_0001, 0, ECX, 19, Needs::Read(0xC001_100C), "NodeId"),
    (0x8000_0001, 0, ECX, 23, Needs::Read(0xC001_0200), "core performance counters"),
    (0x8000_0001, 0, EDX, 11, Needs::Read(0xC000_0081), "SYSCALL"),
    (0x8000_0001, 0, EDX, 20, Needs::Efer(1 << 11), "NX"),
    (0x8000_0001, 0, EDX, 27, Needs::Read(0xC000_0103), "RDTSCP"),
    (0x8000_0001, 0, EDX, 29, Needs::Efer(1 << 8), "long mode"),
    (0x8000_0007, 0, EDX, 7, Needs::Read(0xC001_0061), "hardware P-states"),
    (0x8000_0008, 0, EBX, 1, Needs::Read(0xC000_00E9), "IRPERF"),
    (0x8000_0008, 0, EBX, 12, Needs::Write(0x49, 1), "IBPB"),
    (0x8000_0008, 0, EBX, 14, Needs::Read(0x48), "IBRS"),
    (0x8000_0008, 0, EBX, 15, Needs::Read(0x48), "STIBP"),
    (0x8000_0008, 0, EBX, 23, Needs::Read(0xC001_02F0), "PPIN"),
    (0x8000_0008, 0, EBX, 24, Needs::Read(0x48), "SSBD"),
    (0x8000_0008, 0, EBX, 25, Needs::Read(0xC001_011F), "VIRT_SSBD"),
    (0x8000_0008, 0, EBX, 27, Needs::Read(0xC001_02B0), "CPPC"),
    (0x8000_0008, 0, EBX, 28, Needs::Read(0x48), "PSFD"),
    (0x8000_0021, 0, EAX, 8, Needs::Efer(1 << 21), "AutoIBRS"),
    (0x8000_0021, 0, EAX, 27, Needs::Write(0x49, 1 << 7), "SBPB"),
    (0x8000_0022, 0, EAX, 0, Needs::Read(0xC000_0300), "PerfMonV2"),
];

/// A save area with nothing in it -- paging off, so that EFER's long mode
/// bit may change.
fn blank_save() -> Save {
    // SAFETY: the save area is plain integers and padding, for which zero
    // is a value like any other.
    unsafe { core::mem::zeroed() }
}

/// Every feature the guest is told of backed by what it promises.
fn check_implied(apic_id: u32, apic: bool) {
    for &(leaf, sub, reg, bit, needs, name) in IMPLIED {
        let g = words(&policy::cpuid(leaf, sub, apic_id, apic));
        if g[reg] & (1 << bit) == 0 {
            continue;
        }
        let mut save = blank_save();
        let backed = match needs {
            Needs::Read(msr) => policy::rdmsr(&save, msr).is_some(),
            Needs::Write(msr, value) => policy::wrmsr(&mut save, msr, value),
            Needs::Efer(b) => policy::wrmsr(&mut save, MSR_EFER, b),
        };
        invariant!(backed, "the guest is told of {} (leaf {:#x}.{} register {} bit {}), and {:x?} is refused",
                   name, leaf, sub, reg, bit, needs);
    }
}

/* ---- the MSRs ---- */

/// What the model says an MSR is.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// The guest's own state, which reads back what was written: any value,
    /// a PAT whose every entry is a memory type, or a canonical address.
    Own(Takes),
    /// EFER: the guest's own, its bits held to EFER's rules.
    Efer,
    /// The TSC: read, never set.
    Tsc,
    /// Reads this, and takes no write.
    ReadOnly(u64),
    /// Reads this, and takes a write of it and of nothing else.
    Fixed(u64),
    /// Intel's MISC_ENABLE: fixed, at what the host's fast strings make it.
    MiscEnable,
}

#[derive(Clone, Copy, Debug)]
enum Takes {
    Any,
    Pat,
    Address,
}

/// Whose CPU has an MSR.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Of {
    Every,
    Amd,
    Intel,
}

/// The MSRs a guest is answered, from the manuals: its own state, the TSC,
/// and the constants a feature its CPUID offers says it has -- the MTRRs',
/// the machine checks', the microcode revision -- or that a kernel reads of
/// every CPU of the vendor.
const MSRS: &[(u32, Of, Kind)] = &[
    (0xC000_0080, Of::Every, Kind::Efer),
    (0xC000_0081, Of::Every, Kind::Own(Takes::Any)),     // STAR
    (0xC000_0082, Of::Every, Kind::Own(Takes::Address)), // LSTAR
    (0xC000_0083, Of::Every, Kind::Own(Takes::Address)), // CSTAR
    (0xC000_0084, Of::Every, Kind::Own(Takes::Any)),     // SFMASK
    (0xC000_0100, Of::Every, Kind::Own(Takes::Address)), // FS base
    (0xC000_0101, Of::Every, Kind::Own(Takes::Address)), // GS base
    (0xC000_0102, Of::Every, Kind::Own(Takes::Address)), // kernel GS base
    (0x174, Of::Every, Kind::Own(Takes::Any)),           // SYSENTER CS
    (0x175, Of::Every, Kind::Own(Takes::Address)),       // SYSENTER ESP
    (0x176, Of::Every, Kind::Own(Takes::Address)),       // SYSENTER EIP
    (0x277, Of::Every, Kind::Own(Takes::Pat)),           // the PAT
    (0x10, Of::Every, Kind::Tsc),
    (0xFE, Of::Every, Kind::ReadOnly(0)),                // MTRRcap: none
    (0x179, Of::Every, Kind::ReadOnly(0)),               // MCG_CAP: no banks
    (0x2FF, Of::Every, Kind::Fixed(0)),                  // the MTRRs' default type: off
    (0x17A, Of::Every, Kind::Fixed(0)),                  // MCG_STATUS: nothing
    (0x8B, Of::Every, Kind::Fixed(0)),                   // the microcode revision: none
    (0x17, Of::Intel, Kind::ReadOnly(0)),                // the platform ID
    (0x1A0, Of::Intel, Kind::MiscEnable),
    (0xC001_0015, Of::Amd, Kind::Fixed(1 << 24)),        // HWCR: TscFreqSel
    (0xC001_001F, Of::Amd, Kind::Fixed(1 << 46)),        // NB_CFG: CF8's extended configuration
    (0xC001_1029, Of::Amd, Kind::Fixed(1 << 1)),         // DE_CFG: LFENCE serializing
];

/* EFER's bits as the model has them. */
const EFER_SCE: u64 = 1 << 0;
const EFER_LME: u64 = 1 << 8;
const EFER_LMA: u64 = 1 << 10;
const EFER_NXE: u64 = 1 << 11;
const EFER_SVME: u64 = 1 << 12;
const CR0_PG: u64 = 1 << 31;

/// What the model says MSR `msr` is, on this input's host -- or None, for
/// one it does not name.
fn kind(msr: u32) -> Option<Kind> {
    let intel = cpu::host_is_intel();
    MSRS.iter()
        .find(|&&(m, of, _)| m == msr && (of == Of::Every || (of == Of::Intel) == intel))
        .map(|&(_, _, k)| k)
}

/// Intel's MISC_ENABLE: fast strings as the host has them, branch tracing
/// and PEBS unavailable.
fn misc_enable() -> u64 {
    u64::from(cpu::fast_strings()) | (1 << 11) | (1 << 12)
}

fn canonical(a: u64) -> bool {
    a < 0x0000_8000_0000_0000 || a >= 0xFFFF_8000_0000_0000
}

/// Whether the model has MSR `msr`, of kind `k`, take `value` from a guest
/// whose state is `save`.
fn takes(k: Kind, value: u64, save: &Save) -> bool {
    match k {
        Kind::Own(Takes::Any) => true,
        Kind::Own(Takes::Pat) => value.to_le_bytes().iter().all(|t| [0, 1, 4, 5, 6, 7].contains(t)),
        Kind::Own(Takes::Address) => canonical(value),
        Kind::Efer => value & !(EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE) == 0
            && !(save.cr0 & CR0_PG != 0 && (value ^ save.efer) & EFER_LME != 0),
        Kind::Tsc | Kind::ReadOnly(_) => false,
        Kind::Fixed(v) => value == v,
        Kind::MiscEnable => value == misc_enable(),
    }
}

fn bytes(save: &Save) -> Vec<u8> {
    // SAFETY: the save area is plain integers and padding, all of it
    // initialised -- it is made zeroed, and only ever written whole fields.
    unsafe { core::slice::from_raw_parts(save as *const Save as *const u8, core::mem::size_of::<Save>()) }.to_vec()
}

/// A read of MSR `msr`, held to the model.
fn check_rdmsr(save: &Save, msr: u32) {
    let got = policy::rdmsr(save, msr);
    let ok = match kind(msr) {
        None => got.is_none(),
        Some(Kind::Own(_)) | Some(Kind::Efer) | Some(Kind::Tsc) => got.is_some(),
        Some(Kind::ReadOnly(v)) | Some(Kind::Fixed(v)) => got == Some(v),
        Some(Kind::MiscEnable) => got == Some(misc_enable()),
    };
    invariant!(ok, "rdmsr {:#x} answered {:#x?}, where the model has it {:?} (intel {})", msr, got, kind(msr),
               cpu::host_is_intel());
}

/// A write of `value` to MSR `msr`, held to the model: taken or refused as
/// it says, a refusal leaving the guest's state as it was, and what was
/// taken of the guest's own read back.
fn check_wrmsr(save: &mut Save, msr: u32, value: u64) {
    let k = kind(msr);
    let before = *save;
    let efer_before = before.efer;
    let want = k.map_or(false, |k| takes(k, value, &before));
    let took = policy::wrmsr(save, msr, value);
    invariant!(took == want, "wrmsr {:#x} <- {:#x} {}, where the model has it {:?} (intel {}, CR0 {:#x}, EFER {:#x})",
               msr, value, if took { "taken" } else { "refused" }, k, cpu::host_is_intel(), before.cr0, before.efer);
    if !took {
        invariant!(bytes(save) == bytes(&before), "wrmsr {:#x} <- {:#x}, refused, changed the guest's state", msr, value);
        return;
    }
    let read = policy::rdmsr(save, msr);
    let back = match k {
        Some(Kind::Own(_)) => Some(value),
        Some(Kind::Efer) => Some((value & !EFER_LMA) | (efer_before & EFER_LMA)),
        _ => read,
    };
    invariant!(read == back, "wrmsr {:#x} <- {:#x} taken, and it reads back {:#x?}, not {:#x?}", msr, value, read, back);
}

/// A value to write to an MSR of kind `k`: what it holds, a PAT, an address
/// on either side of the canonical hole, a single bit, anything.
fn msr_value(r: &mut Input, k: Option<Kind>) -> u64 {
    match r.u8() % 6 {
        0 => match k {
            Some(Kind::ReadOnly(v)) | Some(Kind::Fixed(v)) => v,
            Some(Kind::MiscEnable) => misc_enable(),
            Some(Kind::Efer) => r.pick(&[EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE, EFER_LME, EFER_SCE | EFER_NXE]),
            _ => r.value64(),
        },
        1 => if r.bool() { 0x0007_0406_0007_0406 } else { r.u64() & 0x0707_0707_0707_0707 },
        2 => {
            let low = r.u64() & 0x0000_7FFF_FFFF_FFFF;
            match r.u8() % 3 {
                0 => low,
                1 => low | 0xFFFF_8000_0000_0000,
                _ => low | 1 << (47 + r.below(16)),
            }
        }
        3 => 1 << r.below(64),
        _ => r.value64(),
    }
}

/// The policy target: a host an input, the guest's every leaf, what each
/// feature promises, and MSRs named and not, read and written by a guest
/// in long mode with paging on, or before it.
pub fn policy_target(r: &mut Input) {
    cpu::set_host(r.u64());
    let apic = r.bool();
    let apic_id = r.below(16) as u32;
    let told = tops(apic_id, apic);
    for leaf in (0..=0x30u32).chain(0x4000_0000..=0x4000_0010).chain(0x8000_0000..=0x8000_0030) {
        check_leaf(leaf, r.below(4) as u32, apic_id, apic, told);
    }
    check_implied(apic_id, apic);

    let mut save = blank_save();
    if r.bool() {
        save.cr0 = CR0_PG | 1;
        save.efer = EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE | EFER_SVME;
    }
    for &(msr, _, _) in MSRS {
        check_rdmsr(&save, msr);
    }
    while let Some(op) = r.op(4) {
        let msr = match op {
            0 => {
                check_leaf(r.u32(), r.u32(), apic_id, apic, told);
                continue;
            }
            1 => r.pick(MSRS).0,
            /* Beside one: the MSRs a policy might take for a range. */
            2 => r.pick(MSRS).0.wrapping_add(r.below(9) as u32).wrapping_sub(4),
            _ => r.u32(),
        };
        if r.bool() {
            check_rdmsr(&save, msr);
        } else {
            let value = msr_value(r, kind(msr));
            check_wrmsr(&mut save, msr, value);
        }
    }
}
