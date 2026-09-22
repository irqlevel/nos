//! The instructions a probe is made of: CPUID, the control MSRs, CR0 and
//! CR4. Nothing here knows what the words mean -- that is `svm` and `vmx`
//! above it -- and nothing else in the crate writes these instructions out
//! again.

use core::arch::asm;
use core::arch::x86_64::{CpuidResult, __cpuid};

/// CPUID leaf `leaf`, or None when the CPU does not have that leaf.
///
/// The check is the point. CPUID answers a leaf past its own maximum with
/// whatever the highest leaf it does have returns, so an unchecked read of
/// `0x8000_000A` on a CPU that stops at `0x8000_0008` reports a feature
/// bitmap that is really a cache descriptor -- a "yes" from a CPU that never
/// heard the question.
pub fn cpuid(leaf: u32) -> Option<CpuidResult> {
    /* Leaf 0 gives the top of the basic range, leaf 0x8000_0000 the top of
     * the extended one -- and on a CPU with no extended leaves at all it
     * answers with a number below 0x8000_0000, so the same comparison
     * covers that too. */
    let base = if leaf >= 0x8000_0000 { 0x8000_0000 } else { 0 };
    /* Leaf 0 and leaf 0x8000_0000 exist on every CPU this kernel runs on --
     * CPUID itself is checked for at boot. */
    let max = __cpuid(base).eax;
    if leaf > max || max < base {
        return None;
    }
    Some(__cpuid(leaf))
}

/// The vendor string as CPUID leaf 0 spells it: "GenuineIntel",
/// "AuthenticAMD", or what a hypervisor says it is.
pub fn vendor_id() -> [u8; 12] {
    let r = __cpuid(0);
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&r.ebx.to_le_bytes());
    out[4..8].copy_from_slice(&r.edx.to_le_bytes());
    out[8..12].copy_from_slice(&r.ecx.to_le_bytes());
    out
}

/// # Safety
/// The CPU has this MSR. Reading one it does not have is a #GP, and this
/// kernel's #GP handler panics -- so every caller here reads the CPUID bit
/// that says the MSR exists first.
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// # Safety
/// The CPU has this MSR, and `value` is one it takes: a reserved bit set, or
/// a physical address with bits above the CPU's width, is a #GP. And the
/// write changes the CPU, not a variable -- what it turns on stays on for
/// whatever runs on this CPU next.
///
/// No `nomem`, unlike the read: some of these MSRs are what hands the CPU a
/// page of memory to write (`VM_HSAVE_PA`), so the stores that prepared it
/// have to be behind the write and not free to sink past it.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nostack, preserves_flags));
    }
}

#[inline]
pub fn read_cr0() -> u64 {
    let v: u64;
    /* A read of a control register: no memory, no flags, no side effect. */
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
pub fn read_cr4() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// CR4 is the CPU's, not this task's: what is set here is set for everything
/// that runs on this CPU afterwards, and clearing a bit the kernel depends on
/// -- PAE, PGE, OSFXSR -- takes the machine down. Only ever read it, change
/// the one bit, and write it back.
#[inline]
pub unsafe fn write_cr4(value: u64) {
    unsafe { asm!("mov cr4, {}", in(reg) value, options(nomem, nostack, preserves_flags)) };
}

/// CR4.OSFXSR: FXSAVE and FXRSTOR move the XMM registers and MXCSR too.
pub const CR4_OSFXSR: u64 = 1 << 9;
/// CR4.OSXSAVE: XGETBV and XSETBV may be executed.
pub const CR4_OSXSAVE: u64 = 1 << 18;
/// CPUID.1:ECX.XSAVE.
pub const ECX_XSAVE: u32 = 1 << 26;

/// Whether this CPU has XCR0 at all.
pub fn has_xsave() -> bool {
    cpuid(1).map_or(false, |r| r.ecx & ECX_XSAVE != 0)
}

/// # Safety
/// CR4.OSXSAVE is set on this CPU.
#[inline]
pub unsafe fn xgetbv0() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi,
             options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// # Safety
/// CR4.OSXSAVE is set on this CPU, and `value` is an XCR0 it takes: bit 0
/// set, and no component the CPU lacks. What it enables is enabled for
/// whatever runs here next.
#[inline]
pub unsafe fn xsetbv0(value: u64) {
    unsafe {
        asm!("xsetbv", in("ecx") 0u32, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nomem, nostack, preserves_flags));
    }
}
