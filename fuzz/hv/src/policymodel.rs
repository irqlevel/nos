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
//!
//! XSAVE's state is offered as far as the host switches it (`fp::offered`,
//! made up with the host): leaf 1's XSAVE line and leaf 0xD answered for
//! what is offered, and for the asking CPU's own CR4 and XCR0, which the
//! input makes up too. RDTSCP, RDPID and INVPCID as far as they run in a
//! guest (`x86::offers`, made up with the host too), and TSC_AUX with the
//! first two. The speculation controls as far as the host has them, and
//! IA32_SPEC_CTRL and IA32_PRED_CMD as what the guest's CPUID promises of
//! them: the bits its answers name, and nothing else.

use crate::policy::{self, Asker, Cpuid, Own};
use crate::targets::Input;
use crate::x86::{self, cpu, fp};
use crate::x86::svm::vmcb::Save;

/* The tops of the two ranges a guest may be told: the highest leaf the
 * model answers in each. */
const BASIC_TOP: u32 = 0x16;
const EXT_TOP: u32 = 0x8000_0021;

/* Leaf 1 ECX: SSE3, PCLMULQDQ, SSSE3, CMPXCHG16B, SSE4.1, SSE4.2, MOVBE,
 * POPCNT, AES-NI, RDRAND -- instructions that run on the CPU as they are,
 * on registers the FXSAVE area holds -- and PCID, tags in the guest's own
 * TLB entries. x2APIC for a guest with APICs, and the hypervisor bit, are
 * the policy's own. */
const LEAF1_ECX: u32 = (1 << 0) | (1 << 1) | (1 << 9) | (1 << 13) | (1 << 17) | (1 << 19) | (1 << 20) | (1 << 22)
    | (1 << 23) | (1 << 25) | (1 << 30);
const LEAF1_ECX_X2APIC: u32 = 1 << 21;
const LEAF1_ECX_HYPERVISOR: u32 = 1 << 31;
/* Leaf 1 ECX's XSAVE line: XSAVE where any of its state is offered, and
 * OSXSAVE as the asking CPU's CR4 has it; AVX where AVX's is, and FMA and
 * F16C with it as the host has them -- VEX-coded on the YMM registers. */
const LEAF1_ECX_XSAVE: u32 = 1 << 26;
const LEAF1_ECX_OSXSAVE: u32 = 1 << 27;
const LEAF1_ECX_AVX: u32 = 1 << 28;
const LEAF1_ECX_AVX_LINE: u32 = (1 << 12) | (1 << 29);
const CR4_OSXSAVE: u64 = 1 << 18;
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
 * FXSR), SYSCALL, NX, the MMX extensions, 1 GiB pages, long mode; and
 * RDTSCP where it runs in a guest. */
const EXT1_EDX: u32 = 0x1FF | (1 << 11) | (0x3F << 12) | (1 << 20) | (1 << 22) | (1 << 23) | (1 << 24) | (1 << 26)
    | (1 << 29);
const EXT1_EDX_RDTSCP: u32 = 1 << 27;
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
/* The speculation controls, as far as the host has them: leaf 7's EDX --
 * IBRS and IBPB, STIBP, SSBD; its subleaf 2's -- PSFD, IPRED_CTRL,
 * RRSBA_CTRL, DDPD_U, BHI_CTRL, and MCDT_NO, a fact; 0x8000_0008's EBX --
 * IBPB, IBRS, STIBP, SSBD, PSFD, and IBPB_RET, a fact of that IBPB; and
 * 0x8000_0021's EAX -- SBPB, and IBPB_BRTYPE, a fact of IBPB. What each
 * promises of IA32_SPEC_CTRL and IA32_PRED_CMD is `promised`'s. */
const LEAF7_EDX_SPEC: u32 = (1 << 26) | (1 << 27) | (1 << 31);
const LEAF7_2_EDX: u32 = 0x3F;
const EXT8_EBX_SPEC: u32 = (1 << 12) | (1 << 14) | (1 << 15) | (1 << 24) | (1 << 28) | (1 << 30);
const EXT21_EAX_SPEC: u32 = (1 << 27) | (1 << 28);
const EXT21_ECX: u32 = (1 << 1) | (1 << 2);

/* Leaf 7, subleaf 0: FSGSBASE, BMI1, the x87 FDP and CS/DS facts, SMEP,
 * BMI2, ERMS, RDSEED, ADX, SMAP, CLFLUSHOPT, CLWB and SHA in EBX; UMIP and
 * GFNI in ECX; FSRM, MD_CLEAR, SERIALIZE and ARCH_CAPABILITIES in EDX --
 * what runs as it is, on no state the host would switch, and facts of the
 * CPU. */
const LEAF7_EBX: u32 = (1 << 0) | (1 << 3) | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 13) | (1 << 18)
    | (1 << 19) | (1 << 20) | (1 << 23) | (1 << 24) | (1 << 29);
const LEAF7_ECX: u32 = (1 << 2) | (1 << 8);
const LEAF7_EDX: u32 = (1 << 4) | (1 << 10) | (1 << 14) | (1 << 29);
/* And where AVX's state is offered, its line: AVX2 in EBX, VAES and
 * VPCLMULQDQ in ECX. Where each runs in a guest, INVPCID in EBX and RDPID
 * in ECX. */
const LEAF7_EBX_AVX: u32 = 1 << 5;
const LEAF7_ECX_AVX: u32 = (1 << 9) | (1 << 10);
const LEAF7_EBX_INVPCID: u32 = 1 << 10;
const LEAF7_ECX_RDPID: u32 = 1 << 22;
/* And where PKRU is switched, PKU, and OSPKE as the asking CPU's CR4.PKE
 * has it. */
const LEAF7_ECX_PKU: u32 = 1 << 3;
const LEAF7_ECX_OSPKE: u32 = 1 << 4;
const CR4_PKE: u64 = 1 << 22;
/* Subleaf 1, the last answered (subleaf 0's EAX no more than 1): in EAX the
 * fast string facts -- zero-length MOVSB, short STOSB, short CMPSB and SCASB
 * -- and where AVX's state is offered the VEX-coded SHA512, SM3, SM4,
 * AVX-VNNI and AVX-IFMA; in EDX AVX-VNNI-INT8, AVX-NE-CONVERT and
 * AVX-VNNI-INT16; nothing in EBX or ECX. */
const LEAF7_LAST: u32 = 2;
const LEAF7_1_EAX: u32 = (1 << 10) | (1 << 11) | (1 << 12);
const LEAF7_1_EAX_AVX: u32 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 4) | (1 << 23);
const LEAF7_1_EDX_AVX: u32 = (1 << 4) | (1 << 5) | (1 << 10);

/* Leaf 0xD (SDM vol. 1, 13.2; APM vol. 2, 11.5.2): XSAVE's standard area
 * is the legacy region and the header, 576 bytes, then each component at
 * its offset -- AVX's at 576, 256 bytes long, unaligned in the compacted
 * one too. Subleaf 1 EAX: XSAVEOPT, XSAVEC and XGETBV's ECX 1 may be the
 * host's; XSAVES, whose supervisor state nothing switches, and XFD not. */
const XSAVE_LEGACY: u32 = 576;
const XSAVE_AVX_OFFSET: u32 = 576;
const XSAVE_AVX_SIZE: u32 = 256;
const LEAF_D1_EAX: u32 = 0b111;
const LEAF_D1_EAX_XSAVEC: u32 = 1 << 1;

/* IA32_ARCH_CAPABILITIES as a guest may read it: of the host's, the bits
 * that say what the CPU is not open to and what VERW clears -- RDCL_NO,
 * IBRS_ALL (SPEC_CTRL being the guest's), RSBA, SSB_NO, MDS_NO, PSCHANGE_MC_NO, TAA_NO, the three MMIO stale data
 * NOs, FB_CLEAR, RRSBA, BHI_NO, PBRSB_NO, GDS_NO, RFDS_NO, RFDS_CLEAR and
 * ITS_NO -- and none that names an MSR or a control the guest would use. */
const ARCH_CAP: u64 = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 13) | (1 << 14)
    | (1 << 15) | (1 << 17) | (1 << 19) | (1 << 20) | (1 << 24) | (1 << 26) | (1 << 27) | (1 << 28) | (1 << 62);

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

/// The XSAVE area's size for `components`: the legacy region and the
/// header, and AVX's past them.
fn xsave_size(components: u64) -> u32 {
    let avx = if components & fp::XCR0_AVX != 0 { XSAVE_AVX_OFFSET + XSAVE_AVX_SIZE } else { XSAVE_LEGACY };
    match pkru() {
        /* The standard format: PKRU's component at its own offset. */
        Some((size, offset, _)) if components & fp::XCR0_PKRU != 0 => avx.max(offset + size),
        _ => avx,
    }
}

/// The compacted area's size for `components`: each after the last, PKRU's
/// at a multiple of 64 where its subleaf's ECX bit 1 says so.
fn xsave_compacted_size(components: u64) -> u32 {
    let avx = if components & fp::XCR0_AVX != 0 { XSAVE_AVX_OFFSET + XSAVE_AVX_SIZE } else { XSAVE_LEGACY };
    match pkru() {
        Some((size, _, aligned)) if components & fp::XCR0_PKRU != 0 => {
            (if aligned { avx.next_multiple_of(64) } else { avx }) + size
        }
        _ => avx,
    }
}

/// PKRU's component as the host's leaf 0xD subleaf 9 gives it, where it is
/// offered: its size, its offset, and its alignment in the compacted format.
fn pkru() -> Option<(u32, u32, bool)> {
    if fp::offered() & fp::XCR0_PKRU == 0 {
        return None;
    }
    let h = host(0xD, 9)?;
    Some((h[EAX], h[EBX], h[ECX] & 0b10 != 0))
}

/// What the guest is told of the tops of the two ranges: no higher than
/// the model's or the host's, and the vendor as the host says it.
fn tops(asker: &Asker) -> (u32, u32) {
    let basic = words(&policy::cpuid(0, 0, asker));
    let h = host(0, 0).unwrap_or([0; 4]);
    invariant!(basic[0] == h[0].min(BASIC_TOP) && basic[1..] == h[1..],
               "leaf 0 answered {:x?}, the host's {:x?}: wanted the top no higher than {:#x}", basic, h, BASIC_TOP);
    let ext = words(&policy::cpuid(0x8000_0000, 0, asker));
    match host(0x8000_0000, 0) {
        Some(h) => invariant!(ext[0] == h[0].min(EXT_TOP) && ext[1..] == h[1..],
                              "leaf 0x8000_0000 answered {:x?}, the host's {:x?}: wanted the top no higher than {:#x}",
                              ext, h, EXT_TOP),
        None => invariant!(ext == [0; 4], "leaf 0x8000_0000 the host has not answered {:x?}", ext),
    }
    (basic[0], ext[0])
}

/// Leaf `leaf`, subleaf `sub`, as the guest's CPU `asker` is told it, held
/// to the model.
fn check_leaf(leaf: u32, sub: u32, asker: &Asker, (basic_top, ext_top): (u32, u32)) {
    let (apic_id, apic) = (asker.apic_id, asker.apic);
    let g = words(&policy::cpuid(leaf, sub, asker));
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
    let offered = fp::offered();
    let avx = offered & fp::XCR0_AVX != 0;
    /* Leaf 1's XSAVE line, exactly; and the AVX line's other bits, which
     * are the host's to have. */
    let xsave_bits = if offered == 0 {
        0
    } else {
        LEAF1_ECX_XSAVE
            | if asker.cr4 & CR4_OSXSAVE != 0 { LEAF1_ECX_OSXSAVE } else { 0 }
            | if avx { LEAF1_ECX_AVX } else { 0 }
    };
    let avx_line = if avx { LEAF1_ECX_AVX_LINE } else { 0 };
    let runs = x86::offers();
    let bit = |yes: bool, b: u32| if yes { b } else { 0 };
    match leaf {
        0 | 0x8000_0000 => {}
        1 => {
            want(g[0] == h[0], "the signature, the host's");
            want(g[1] & 0xFFFF == h[1] & 0xFFFF && (g[1] >> 16) & 0xFF == 1 && g[1] >> 24 == apic_id & 0xFF,
                 "EBX: the host's low half, one logical CPU, its own APIC ID");
            want(leaked(g[2], h[2], LEAF1_ECX | avx_line, x2apic | LEAF1_ECX_HYPERVISOR | xsave_bits) == 0,
                 "ECX within the model's");
            want(g[2] & (LEAF1_ECX_XSAVE | LEAF1_ECX_OSXSAVE | LEAF1_ECX_AVX) == xsave_bits,
                 "XSAVE where its state is offered, OSXSAVE as CR4 has it, AVX where its state is");
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
        7 if sub == 0 => {
            let (avx_ebx, avx_ecx) = if avx { (LEAF7_EBX_AVX, LEAF7_ECX_AVX) } else { (0, 0) };
            want(g[0] == h[0].min(LEAF7_LAST), "EAX: the host's last subleaf, no further than 2");
            want(leaked(g[1], h[1], LEAF7_EBX | avx_ebx | bit(runs.invpcid, LEAF7_EBX_INVPCID), 0) == 0,
                 "EBX within the model's");
            let pku = bit(offered & fp::XCR0_PKRU != 0, LEAF7_ECX_PKU);
            let ospke = bit(pku != 0 && asker.cr4 & CR4_PKE != 0, LEAF7_ECX_OSPKE);
            want(leaked(g[2], h[2], LEAF7_ECX | avx_ecx | bit(runs.rdpid, LEAF7_ECX_RDPID), pku | ospke) == 0,
                 "ECX within the model's");
            want(g[2] & (LEAF7_ECX_PKU | LEAF7_ECX_OSPKE) == pku | ospke,
                 "PKU where PKRU is switched, and OSPKE with it as the asking CPU's CR4.PKE has it");
            want(leaked(g[3], h[3], LEAF7_EDX | LEAF7_EDX_SPEC, 0) == 0, "EDX within the model's");
        }
        7 if sub == 2 && host(7, 0).map_or(0, |h0| h0[0]) >= 2 => {
            want(g[..3] == [0; 3], "EAX to ECX: nothing");
            want(leaked(g[3], h[3], LEAF7_2_EDX, 0) == 0, "EDX within the model's");
        }
        7 if sub == 1 && host(7, 0).map_or(0, |h0| h0[0]) >= 1 => {
            let (avx_eax, avx_edx) = if avx { (LEAF7_1_EAX_AVX, LEAF7_1_EDX_AVX) } else { (0, 0) };
            want(leaked(g[0], h[0], LEAF7_1_EAX | avx_eax, 0) == 0, "EAX within the model's");
            want(g[1] == 0 && g[2] == 0, "EBX and ECX: nothing");
            want(leaked(g[3], h[3], avx_edx, 0) == 0, "EDX within the model's");
        }
        0xD if offered == 0 => want(g == [0; 4], "nothing: no XSAVE state offered"),
        0xD => {
            let on = asker.xcr0 & offered;
            match sub {
                0 => want(g == [offered as u32, xsave_size(on), xsave_size(offered), (offered >> 32) as u32],
                          "the components offered, the area's size for those XCR0 has on and for all"),
                1 => {
                    let compacted = if g[0] & LEAF_D1_EAX_XSAVEC != 0 { xsave_compacted_size(on) } else { 0 };
                    want(g[0] == h[0] & LEAF_D1_EAX && g[1] == compacted && g[2] == 0 && g[3] == 0,
                         "XSAVEOPT, XSAVEC, XGETBV's ECX 1 as the host has them, with XSAVEC the compacted size, \
                          and no supervisor state");
                }
                2 if avx => want(g == [XSAVE_AVX_SIZE, XSAVE_AVX_OFFSET, 0, 0], "AVX's component, at 576"),
                9 if pkru().is_some() => {
                    let (size, offset, aligned) = pkru().unwrap_or((0, 0, false));
                    want(g == [size, offset, if aligned { 0b10 } else { 0 }, 0] && size == 8 && offset >= 832,
                         "PKRU's component, as the host has it: 8 bytes, past AVX's");
                }
                _ => want(g == [0; 4], "nothing: no component past AVX's but PKRU's"),
            }
        }
        0x15 if apic => want(g == [0; 4] || (g[0] == 1_000_000 && g[1] != 0 && g[2] == 1_000_000_000 && g[3] == 0),
                             "the TSC over the emulated APIC's 1 GHz, or nothing"),
        0x15 => want(g == h, "the host's"),
        l if WHOLE.contains(&l) => want(g == h, "the host's, whole"),
        0x8000_0001 => {
            want(g[0] == h[0] && g[1] == h[1], "EAX and EBX: the host's");
            want(leaked(g[2], h[2], EXT1_ECX, 0) == 0, "ECX within the model's");
            want(leaked(g[3], h[3], EXT1_EDX | bit(runs.rdtscp, EXT1_EDX_RDTSCP), apic_bit) == 0,
                 "EDX within the model's");
        }
        0x8000_0007 => want(g == [0, 0, 0, h[3] & EXT7_EDX], "the invariant TSC alone"),
        0x8000_0008 => {
            want(g[0] == h[0] & EXT8_EAX, "EAX: the address widths, the host's");
            want(leaked(g[1], h[1], EXT8_EBX | EXT8_EBX_SPEC, 0) == 0, "EBX within the model's");
            want(g[2] == 0 && g[3] == 0, "ECX and EDX: nothing -- one core");
        }
        0x8000_0021 => {
            want(leaked(g[0], h[0], EXT21_EAX | EXT21_EAX_SPEC, 0) == 0, "EAX within the model's");
            want(leaked(g[2], h[2], EXT21_ECX, 0) == 0, "ECX within the model's");
            want(g[1] == 0 && g[3] == 0, "EBX and EDX: nothing");
        }
        _ => want(g == [0; 4], "nothing: a leaf the model does not name"),
    }
    if let Some(what) = bad {
        panic!("invariant: leaf {:#x}.{} answered {:x?} from the host's {:x?} (apic {}, CR4 {:#x}, XCR0 {:#x}, \
                offered {:#x}): wanted {}", leaf, sub, g, h, apic, asker.cr4, asker.xcr0, offered, what);
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
    (7, 0, ECX, 22, Needs::Read(0xC000_0103), "RDPID"),
    (7, 0, ECX, 22, Needs::Write(0xC000_0103, 0xFFFF_FFFF), "RDPID"),
    (7, 0, ECX, 5, Needs::Read(0xE1), "WAITPKG"),
    (7, 0, ECX, 7, Needs::Read(0x6A2), "CET shadow stacks"),
    (7, 0, EDX, 20, Needs::Read(0x6A2), "CET indirect branch tracking"),
    (7, 0, EDX, 26, Needs::Read(0x48), "IBRS and IBPB"),
    (7, 0, EDX, 26, Needs::Write(0x48, 1), "IBRS and IBPB"),
    (7, 0, EDX, 26, Needs::Write(0x49, 1), "IBRS and IBPB"),
    (7, 0, EDX, 27, Needs::Read(0x48), "STIBP"),
    (7, 0, EDX, 27, Needs::Write(0x48, 1 << 1), "STIBP"),
    (7, 0, EDX, 28, Needs::Write(0x10B, 1), "L1D_FLUSH"),
    (7, 0, EDX, 29, Needs::Read(0x10A), "ARCH_CAPABILITIES"),
    (7, 0, EDX, 30, Needs::Read(0xCF), "CORE_CAPABILITIES"),
    (7, 0, EDX, 31, Needs::Read(0x48), "SSBD"),
    (7, 0, EDX, 31, Needs::Write(0x48, 1 << 2), "SSBD"),
    (7, 2, EDX, 0, Needs::Write(0x48, 1 << 7), "PSFD"),
    (7, 2, EDX, 1, Needs::Write(0x48, 3 << 3), "IPRED_CTRL"),
    (7, 2, EDX, 2, Needs::Write(0x48, 3 << 5), "RRSBA_CTRL"),
    (7, 2, EDX, 3, Needs::Write(0x48, 1 << 8), "DDPD_U"),
    (7, 2, EDX, 4, Needs::Write(0x48, 1 << 10), "BHI_CTRL"),
    (0xD, 1, EAX, 3, Needs::Read(0xDA0), "XSAVES"),
    (0x8000_0001, 0, ECX, 2, Needs::Read(0xC001_0114), "SVM"),
    (0x8000_0001, 0, ECX, 9, Needs::Read(0xC001_0140), "OSVW"),
    (0x8000_0001, 0, ECX, 10, Needs::Read(0xC001_1030), "IBS"),
    (0x8000_0001, 0, ECX, 19, Needs::Read(0xC001_100C), "NodeId"),
    (0x8000_0001, 0, ECX, 23, Needs::Read(0xC001_0200), "core performance counters"),
    (0x8000_0001, 0, EDX, 11, Needs::Read(0xC000_0081), "SYSCALL"),
    (0x8000_0001, 0, EDX, 20, Needs::Efer(1 << 11), "NX"),
    (0x8000_0001, 0, EDX, 27, Needs::Read(0xC000_0103), "RDTSCP"),
    (0x8000_0001, 0, EDX, 27, Needs::Write(0xC000_0103, 0xFFFF_FFFF), "RDTSCP"),
    (0x8000_0001, 0, EDX, 29, Needs::Efer(1 << 8), "long mode"),
    (0x8000_0007, 0, EDX, 7, Needs::Read(0xC001_0061), "hardware P-states"),
    (0x8000_0008, 0, EBX, 1, Needs::Read(0xC000_00E9), "IRPERF"),
    (0x8000_0008, 0, EBX, 12, Needs::Write(0x49, 1), "IBPB"),
    (0x8000_0008, 0, EBX, 14, Needs::Read(0x48), "IBRS"),
    (0x8000_0008, 0, EBX, 14, Needs::Write(0x48, 1), "IBRS"),
    (0x8000_0008, 0, EBX, 15, Needs::Read(0x48), "STIBP"),
    (0x8000_0008, 0, EBX, 15, Needs::Write(0x48, 1 << 1), "STIBP"),
    (0x8000_0008, 0, EBX, 23, Needs::Read(0xC001_02F0), "PPIN"),
    (0x8000_0008, 0, EBX, 24, Needs::Read(0x48), "SSBD"),
    (0x8000_0008, 0, EBX, 24, Needs::Write(0x48, 1 << 2), "SSBD"),
    (0x8000_0008, 0, EBX, 25, Needs::Read(0xC001_011F), "VIRT_SSBD"),
    (0x8000_0008, 0, EBX, 27, Needs::Read(0xC001_02B0), "CPPC"),
    (0x8000_0008, 0, EBX, 28, Needs::Read(0x48), "PSFD"),
    (0x8000_0008, 0, EBX, 28, Needs::Write(0x48, 1 << 7), "PSFD"),
    (0x8000_0008, 0, EBX, 30, Needs::Write(0x49, 1), "IBPB_RET"),
    (0x8000_0021, 0, EAX, 8, Needs::Efer(1 << 21), "AutoIBRS"),
    (0x8000_0021, 0, EAX, 27, Needs::Write(0x49, 1 << 7), "SBPB"),
    (0x8000_0021, 0, EAX, 28, Needs::Write(0x49, 1), "IBPB_BRTYPE"),
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
fn check_implied(asker: &Asker) {
    for &(leaf, sub, reg, bit, needs, name) in IMPLIED {
        let g = words(&policy::cpuid(leaf, sub, asker));
        if g[reg] & (1 << bit) == 0 {
            continue;
        }
        let mut save = blank_save();
        let mut own = Own::default();
        let backed = match needs {
            Needs::Read(msr) => policy::rdmsr(&save, &own, msr).is_some(),
            Needs::Write(msr, value) => policy::wrmsr(&mut save, &mut own, msr, value),
            Needs::Efer(b) => policy::wrmsr(&mut save, &mut own, MSR_EFER, b),
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
    /// IA32_ARCH_CAPABILITIES: read-only, the host's masked to `ARCH_CAP`,
    /// where the host has it; a #GP where it has not.
    ArchCapabilities,
    /// IA32_TSC_AUX: the guest's own, beside its save area, any value of
    /// its low 32 bits.
    TscAux,
    /// IA32_SPEC_CTRL: the guest's own, beside its save area, any of the
    /// bits its CPUID promises (`promised`).
    SpecCtrl,
    /// IA32_PRED_CMD: written alone -- a read is a #GP -- with any of the
    /// bits its CPUID promises; each write of one an IBPB asked for.
    PredCmd,
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
    /// One whose guest RDTSCP or RDPID runs in (`x86::offers`).
    Rdtscp,
    /// One whose guest's CPUID promises a control of IA32_SPEC_CTRL, or of
    /// IA32_PRED_CMD (`promised`).
    SpecCtrl,
    PredCmd,
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
    (0x10A, Of::Every, Kind::ArchCapabilities),
    (0xC001_0015, Of::Amd, Kind::Fixed(1 << 24)),        // HWCR: TscFreqSel
    (0xC001_001F, Of::Amd, Kind::Fixed(1 << 46)),        // NB_CFG: CF8's extended configuration
    (0xC001_1029, Of::Amd, Kind::Fixed(1 << 1)),         // DE_CFG: LFENCE serializing
    (0xC000_0103, Of::Rdtscp, Kind::TscAux),
    (0x48, Of::SpecCtrl, Kind::SpecCtrl),
    (0x49, Of::PredCmd, Kind::PredCmd),
];

/// What the guest's CPUID promises of IA32_SPEC_CTRL and IA32_PRED_CMD,
/// from the manuals: each control's bits, Intel's leaves and AMD's.
fn promised() -> (u64, u64) {
    let a = Asker::RESET;
    let l7 = words(&policy::cpuid(7, 0, &a))[EDX];
    let l72 = words(&policy::cpuid(7, 2, &a))[EDX];
    let e8 = words(&policy::cpuid(0x8000_0008, 0, &a))[EBX];
    let e21 = words(&policy::cpuid(0x8000_0021, 0, &a))[EAX];
    let bit = |word: u32, b: u32| word & (1 << b) != 0;
    let has = |yes: bool, v: u64| if yes { v } else { 0 };
    let spec_ctrl = has(bit(l7, 26) || bit(e8, 14), 1 << 0)   // IBRS
        | has(bit(l7, 27) || bit(e8, 15), 1 << 1)              // STIBP
        | has(bit(l7, 31) || bit(e8, 24), 1 << 2)              // SSBD
        | has(bit(l72, 0) || bit(e8, 28), 1 << 7)              // PSFD
        | has(bit(l72, 1), 3 << 3)                             // IPRED_DIS_U and _S
        | has(bit(l72, 2), 3 << 5)                             // RRSBA_DIS_U and _S
        | has(bit(l72, 3), 1 << 8)                             // DDPD_U
        | has(bit(l72, 4), 1 << 10);                           // BHI_DIS_S
    let pred_cmd = has(bit(l7, 26) || bit(e8, 12), 1 << 0)    // IBPB
        | has(bit(e21, 27), 1 << 7);                           // SBPB
    (spec_ctrl, pred_cmd)
}

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
    let runs = x86::offers();
    let has = |of: Of| match of {
        Of::Every => true,
        Of::Amd => !intel,
        Of::Intel => intel,
        Of::Rdtscp => runs.rdtscp || runs.rdpid,
        Of::SpecCtrl => promised().0 != 0,
        Of::PredCmd => promised().1 != 0,
    };
    MSRS.iter().find(|&&(m, of, _)| m == msr && has(of)).map(|&(_, _, k)| k)
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
        Kind::Tsc | Kind::ReadOnly(_) | Kind::ArchCapabilities => false,
        Kind::Fixed(v) => value == v,
        Kind::MiscEnable => value == misc_enable(),
        Kind::TscAux => value >> 32 == 0,
        Kind::SpecCtrl => value & !promised().0 == 0,
        Kind::PredCmd => value & !promised().1 == 0,
    }
}

fn bytes(save: &Save) -> Vec<u8> {
    // SAFETY: the save area is plain integers and padding, all of it
    // initialised -- it is made zeroed, and only ever written whole fields.
    unsafe { core::slice::from_raw_parts(save as *const Save as *const u8, core::mem::size_of::<Save>()) }.to_vec()
}

/// A read of MSR `msr`, held to the model.
fn check_rdmsr(save: &Save, own: &Own, msr: u32) {
    let got = policy::rdmsr(save, own, msr);
    let ok = match kind(msr) {
        None => got.is_none(),
        Some(Kind::Own(_)) | Some(Kind::Efer) | Some(Kind::Tsc) => got.is_some(),
        Some(Kind::ReadOnly(v)) | Some(Kind::Fixed(v)) => got == Some(v),
        Some(Kind::MiscEnable) => got == Some(misc_enable()),
        Some(Kind::ArchCapabilities) => got == cpu::arch_capabilities().map(|v| v & ARCH_CAP),
        Some(Kind::TscAux) => got == Some(own.tsc_aux),
        Some(Kind::SpecCtrl) => got == Some(own.spec_ctrl),
        Some(Kind::PredCmd) => got.is_none(),
    };
    invariant!(ok, "rdmsr {:#x} answered {:#x?}, where the model has it {:?} (intel {})", msr, got, kind(msr),
               cpu::host_is_intel());
}

/// A write of `value` to MSR `msr`, held to the model: taken or refused as
/// it says, a refusal leaving the guest's state as it was, a write to one
/// MSR changing no other of its own, and what was taken of the guest's own
/// read back.
fn check_wrmsr(save: &mut Save, own: &mut Own, msr: u32, value: u64) {
    let k = kind(msr);
    let before = *save;
    let own_before = *own;
    let efer_before = before.efer;
    let want = k.map_or(false, |k| takes(k, value, &before));
    let took = policy::wrmsr(save, own, msr, value);
    invariant!(took == want, "wrmsr {:#x} <- {:#x} {}, where the model has it {:?} (intel {}, CR0 {:#x}, EFER {:#x})",
               msr, value, if took { "taken" } else { "refused" }, k, cpu::host_is_intel(), before.cr0, before.efer);
    if !took {
        invariant!(bytes(save) == bytes(&before) && *own == own_before,
                   "wrmsr {:#x} <- {:#x}, refused, changed the guest's state", msr, value);
        return;
    }
    /* What of the guest's own beside its save area the write made: the one
     * MSR's value, or an IBPB more asked for -- and nothing else. */
    let own_want = match k {
        Some(Kind::TscAux) => Own { tsc_aux: value, ..own_before },
        Some(Kind::SpecCtrl) => Own { spec_ctrl: value, ..own_before },
        Some(Kind::PredCmd) => Own { ibpbs: own_before.ibpbs.wrapping_add(u64::from(value != 0)), ..own_before },
        _ => own_before,
    };
    let beside = matches!(k, Some(Kind::TscAux) | Some(Kind::SpecCtrl) | Some(Kind::PredCmd));
    invariant!(*own == own_want && (!beside || bytes(save) == bytes(&before)),
               "wrmsr {:#x} <- {:#x} made the guest's own {:x?} from {:x?}, not {:x?}, or changed its save area", msr,
               value, own, own_before, own_want);
    let read = policy::rdmsr(save, own, msr);
    let back = match k {
        Some(Kind::Own(_)) | Some(Kind::TscAux) | Some(Kind::SpecCtrl) => Some(value),
        Some(Kind::PredCmd) => None,
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
            Some(Kind::ArchCapabilities) => cpu::arch_capabilities().unwrap_or(0),
            Some(Kind::Efer) => r.pick(&[EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE, EFER_LME, EFER_SCE | EFER_NXE]),
            Some(Kind::TscAux) => u64::from(r.u32()),
            Some(Kind::SpecCtrl) => promised().0 & r.u64(),
            Some(Kind::PredCmd) => r.pick(&[1, 0, 1 << 7, 0x81]),
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
    /* The asking CPU's CR4 -- OSXSAVE on or off, among anything -- and its
     * XCR0: one an XSETBV could have made it, of the components offered,
     * or out of reset. */
    let xcr0 = fp::XCR0_X87
        | (r.pick(&[0, fp::XCR0_SSE, fp::XCR0_SSE | fp::XCR0_AVX, fp::XCR0_PKRU,
                    fp::XCR0_SSE | fp::XCR0_AVX | fp::XCR0_PKRU]) & fp::offered());
    let asker = Asker { apic_id: r.below(16) as u32, apic: r.bool(), cr4: r.value64(), xcr0 };
    let told = tops(&asker);
    for leaf in (0..=0x30u32).chain(0x4000_0000..=0x4000_0010).chain(0x8000_0000..=0x8000_0030) {
        check_leaf(leaf, r.below(4) as u32, &asker, told);
    }
    for sub in 0..4 {
        check_leaf(0xD, sub, &asker, told);
    }
    check_implied(&asker);

    let mut save = blank_save();
    if r.bool() {
        save.cr0 = CR0_PG | 1;
        save.efer = EFER_SCE | EFER_LME | EFER_LMA | EFER_NXE | EFER_SVME;
    }
    let mut own = Own::default();
    for &(msr, _, _) in MSRS {
        check_rdmsr(&save, &own, msr);
    }
    while let Some(op) = r.op(4) {
        let msr = match op {
            0 => {
                check_leaf(r.u32(), r.u32(), &asker, told);
                continue;
            }
            1 => r.pick(MSRS).0,
            /* Beside one: the MSRs a policy might take for a range. */
            2 => r.pick(MSRS).0.wrapping_add(r.below(9) as u32).wrapping_sub(4),
            _ => r.u32(),
        };
        if r.bool() {
            check_rdmsr(&save, &own, msr);
        } else {
            let value = msr_value(r, kind(msr));
            check_wrmsr(&mut save, &mut own, msr, value);
        }
    }
}
