//! Where the host's CPUs sit: which are threads of one core, and which share
//! the last-level cache -- what a guest's CPUs are placed by.
//!
//! On x86 the kernel numbers a CPU by its APIC ID, and an APIC ID is a
//! topology in itself: shifted right by the width of the SMT level it is the
//! core, by the width of the cache's sharing it is the cache domain. CPUID
//! gives both widths, the same on every CPU of the one package design the
//! kernel runs on. Two CPUs of a guest on one core's two threads share its
//! execution units -- on the AX41 two spinning guest CPUs there took 14.7 s
//! for what one did alone in 9.7, where on two cores they took 9.9.

use core::arch::x86_64::CpuidResult;

use hvarch::x86::cpu;

/// Intel's deterministic cache parameters.
const LEAF_CACHE: u32 = 4;
/// The x2APIC topology leaf: subleaf 0 is the SMT level.
const LEAF_TOPOLOGY: u32 = 0xB;
/// AMD's deterministic cache parameters, in leaf 4's format.
const LEAF_AMD_CACHE: u32 = 0x8000_001D;
/// AMD's extended APIC ID leaf: EBX[15:8] is threads per core less one.
const LEAF_AMD_APIC_ID: u32 = 0x8000_001E;

const TOPOLOGY_SHIFT_MASK: u32 = 0x1F;
const TOPOLOGY_LEVEL_SHIFT: u32 = 8;
const TOPOLOGY_LEVEL_MASK: u32 = 0xFF;
const TOPOLOGY_LEVEL_SMT: u32 = 1;
const AMD_THREADS_SHIFT: u32 = 8;
const AMD_THREADS_MASK: u32 = 0xFF;
const CACHE_TYPE_MASK: u32 = 0x1F;
const CACHE_LEVEL_SHIFT: u32 = 5;
const CACHE_LEVEL_MASK: u32 = 0x7;
const CACHE_SHARING_SHIFT: u32 = 14;
const CACHE_SHARING_MASK: u32 = 0xFFF;
/// More subleaves than any CPU has caches.
const CACHE_SUBLEAVES: u32 = 16;

/// The widths an APIC ID divides into: a core's threads, and the CPUs that
/// share the last-level cache.
#[derive(Clone, Copy, Debug)]
pub struct Topology {
    smt_shift: u32,
    llc_shift: u32,
}

impl Topology {
    /// What CPUID says on the CPU this runs on. Where it says nothing, every
    /// CPU is a core of its own, and all of them share one cache.
    pub fn host() -> Topology {
        Topology { smt_shift: smt_shift(), llc_shift: llc_shift() }
    }

    /// The core CPU `cpu` is a thread of.
    #[inline]
    pub fn core(&self, cpu: u32) -> u32 {
        cpu.checked_shr(self.smt_shift).unwrap_or(0)
    }

    /// The last-level cache CPU `cpu` shares.
    #[inline]
    pub fn llc(&self, cpu: u32) -> u32 {
        cpu.checked_shr(self.llc_shift).unwrap_or(0)
    }
}

/// A leaf the CPU has, or None: `cpu::cpuid_count` checks the range.
fn leaf(leaf: u32, sub: u32) -> Option<CpuidResult> {
    cpu::cpuid_count(leaf, sub)
}

/// The bits a count of `n` IDs takes: 0 for one.
fn width(n: u32) -> u32 {
    match n {
        0 | 1 => 0,
        n => u32::BITS - (n - 1).leading_zeros(),
    }
}

/// The SMT level's width: the topology leaf's, or AMD's threads per core.
fn smt_shift() -> u32 {
    if let Some(r) = leaf(LEAF_TOPOLOGY, 0) {
        let level = (r.ecx >> TOPOLOGY_LEVEL_SHIFT) & TOPOLOGY_LEVEL_MASK;
        if r.ebx != 0 && level == TOPOLOGY_LEVEL_SMT {
            return r.eax & TOPOLOGY_SHIFT_MASK;
        }
    }
    if let Some(r) = leaf(LEAF_AMD_APIC_ID, 0) {
        return width(((r.ebx >> AMD_THREADS_SHIFT) & AMD_THREADS_MASK) + 1);
    }
    0
}

/// The last-level cache's width: of the caches the parameters leaf lists,
/// the highest level's count of the IDs sharing it. All of them, where no
/// leaf says.
fn llc_shift() -> u32 {
    let source = if leaf(LEAF_AMD_CACHE, 0).is_some_and(|r| r.eax & CACHE_TYPE_MASK != 0) {
        LEAF_AMD_CACHE
    } else if leaf(LEAF_CACHE, 0).is_some() {
        LEAF_CACHE
    } else {
        return u32::BITS;
    };
    let (mut level, mut shift) = (0, u32::BITS);
    for sub in 0..CACHE_SUBLEAVES {
        let Some(r) = leaf(source, sub) else { break };
        if r.eax & CACHE_TYPE_MASK == 0 {
            break;
        }
        let this = (r.eax >> CACHE_LEVEL_SHIFT) & CACHE_LEVEL_MASK;
        if this >= level {
            level = this;
            shift = width(((r.eax >> CACHE_SHARING_SHIFT) & CACHE_SHARING_MASK) + 1);
        }
    }
    shift
}
