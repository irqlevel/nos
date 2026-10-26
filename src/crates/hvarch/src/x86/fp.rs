//! The x87, SSE and AVX registers and XCR0, which neither `vmrun` nor
//! `vmlaunch` switches: the host CPU's control bits for them while its
//! extension is on, and each guest's registers ([`GuestFp`]).
//!
//! A guest's registers are put into the CPU before its entry and taken back
//! after its exit, by [`super::svm::Guest::run`] and [`super::vmx::Guest::run`]
//! -- XRSTOR and XSAVE, in XSAVE's standard format, where the CPU has XSAVE;
//! FXRSTOR and FXSAVE, the x87 and SSE alone, where it has not -- and its
//! XCR0, which says what of them the guest may use (AVX's upper halves of the
//! YMM registers above all), made the CPU's for the length of its run. A
//! guest cannot change XCR0 itself: its XSETBV always exits (AMD-V's
//! intercept, which `svm` sets on every entry; VT-x's, unconditional), and
//! the run loop in `hv` writes the guest's through [`GuestFp::set_xcr0`].
//!
//! Registers are put back only when the CPU holds another guest's
//! ([`OWNER`]): the host never touches them -- its C++ is built without SSE
//! and its Rust is soft-float. And then all of what the CPU switches is put
//! back, whatever the guest's XCR0 says ([`MASK`]): the upper halves of the
//! YMM registers, which a guest whose XCR0 leaves out AVX cannot reach now
//! but can as soon as it sets it, are never another guest's. They are saved
//! as the guest's XCR0 has them: what XCR0 leaves out the guest cannot have
//! changed -- an AVX instruction faults without it, an SSE one leaves the
//! upper halves alone -- and the area still holds it. Nothing above AVX is
//! switched or offered -- AVX-512, AMX, PKRU, CET -- so none of that state
//! is ever in a guest's reach.
//!
//! This kernel boots with CR4.OSFXSR and CR4.OSXSAVE clear and never sets
//! them: its C++ is built without SSE and its Rust is soft-float, so an SSE
//! or AVX instruction in it is a bug, and with the bits clear it faults.
//! Setting OSFXSR around every entry and clearing it after, as both `run`s
//! once did, cost two serializing writes of CR4 an entry, an XGETBV, and
//! under a nested hypervisor an exit to the outer one for each -- a fifth of
//! an exit on real silicon, more than the world switch itself under nested
//! KVM. So both bits are set once, here, when the extension is turned on for
//! a CPU, and cleared when it is turned off; and XCR0 is whatever the last
//! guest's entry made it, until `off` puts back the one `on` found. While
//! the hypervisor is loaded an SSE or AVX instruction in the kernel would
//! run instead of faulting on those CPUs: the guard is given up for the time
//! the module is on, and for nothing else.
//!
//! What was there is remembered per CPU, so `off` puts back exactly what
//! `on` found: a CPU the extension goes off for is as it booted.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use alloc::vec::Vec;
use kcore::consts::MAX_CPUS;

use super::cpu;
use crate::{Error, Result};

/// Whose registers each CPU holds: the guest whose load was the last on it,
/// by a number of its own ([`owner_id`]), or 0. What lets an entry skip the
/// load: the host never touches those registers -- its C++ is built without
/// them and its Rust is soft-float -- so between a guest's exit on a CPU and
/// its next entry there they still hold what its save at the exit saved,
/// unless another guest's entry loaded its own in between, which the number
/// then says. A number is never given twice, so a guest dropped and another
/// made in its memory cannot be taken for it; and it is on one CPU at most,
/// the last its guest loaded on ([`registers_loaded`]), since a CPU it ran on
/// before holds what its registers were then.
static OWNER: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// A number no guest has had, for the guest being made.
fn owner_id() -> u64 {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// Whether the registers of `cpu` are guest `id`'s already -- asked with
/// interrupts off on `cpu`, before its entry.
#[inline]
fn registers_are(cpu: u32, id: u64) -> bool {
    OWNER.get(cpu as usize).map_or(false, |o| o.load(Ordering::Relaxed) == id)
}

/// Guest `id` has just loaded its registers on `cpu` -- and so they are its
/// on no other: a CPU it loaded them on before holds what they were when it
/// last left its guest there, older than what it brings back now. A guest's
/// task can move (`hv run` without a CPU is one the scheduler places), and
/// marked on both, a guest that ran on A, then B, then A again skipped its
/// load on A and went on with the registers of its first stay there. Only
/// an entry that loads comes here, so looking at every CPU's mark costs
/// nothing an entry pays every time. The guest runs on one CPU at a time, so
/// no entry of its own races this; another guest's entry on that CPU may
/// put its own mark there meanwhile, which the compare-and-swap leaves.
#[inline]
fn registers_loaded(cpu: u32, id: u64) {
    for (i, o) in OWNER.iter().enumerate() {
        if i != cpu as usize && o.load(Ordering::Relaxed) == id {
            let _ = o.compare_exchange(id, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
    if let Some(o) = OWNER.get(cpu as usize) {
        o.store(id, Ordering::Relaxed);
    }
}

/// XCR0's components, as far as a guest is given them: the x87; SSE's XMM
/// registers and MXCSR; AVX's upper halves of the YMM registers.
pub const XCR0_X87: u64 = 1 << 0;
pub const XCR0_SSE: u64 = 1 << 1;
pub const XCR0_AVX: u64 = 1 << 2;

/// CPUID's leaf of XSAVE's components, and its subleaf of AVX's.
const LEAF_XSAVE: u32 = 0xD;
const SUBLEAF_AVX: u32 = 2;
/// Leaf 1 ECX: AVX.
const ECX_AVX: u32 = 1 << 28;
/// Leaf 0xD, subleaf 1, EAX: XSAVES, and IA32_XSS with it.
const EAX_XSAVES: u32 = 1 << 3;
/// IA32_XSS: the supervisor components XSAVES and XRSTORS move.
const MSR_XSS: u32 = 0xDA0;

/* The area a guest's registers are kept in, in XSAVE's standard format: the
 * legacy region FXSAVE writes, 512 bytes; the header, 64; and AVX's
 * component, 256, at the offset every CPU with AVX gives it -- which
 * `supported` checks, since XSAVE writes a component wherever CPUID says it
 * goes, and nothing past AVX's is ever asked of it. */
const LEGACY_BYTES: usize = 512;
const HEADER_BYTES: usize = 64;
const AVX_OFFSET: usize = LEGACY_BYTES + HEADER_BYTES;
const AVX_BYTES: usize = 256;
const AREA_BYTES: usize = AVX_OFFSET + AVX_BYTES;
/// Where the legacy region keeps the x87 control word and MXCSR, and where
/// the header keeps XSTATE_BV: the components the area holds -- XRSTOR puts
/// a component whose bit is clear into its initial state instead.
const FCW_AT: usize = 0;
const MXCSR_AT: usize = 24;
const XSTATE_BV_AT: usize = LEGACY_BYTES;
/// The control word and MXCSR out of reset.
const FCW_RESET: u16 = 0x0040;
const MXCSR_RESET: u32 = 0x1F80;

/// A guest's registers as the CPU saves them: 64-byte aligned, as XSAVE
/// wants its area, and FXSAVE (16) with it. Written by nothing but those
/// saves, `reset` and the two header fixes in [`GuestFp`] -- so XRSTOR, which
/// faults on an area whose header or MXCSR is not one a save could have
/// left, never does.
#[repr(C, align(64))]
struct Area([u8; AREA_BYTES]);

impl Area {
    /// The registers as a CPU comes out of reset: everything zero but the
    /// control word and MXCSR; the x87 and SSE marked held, so that XRSTOR
    /// takes them from here -- the x87's initial state is FNINIT's, whose
    /// control word is not reset's -- and AVX's not, its upper halves zero.
    fn reset() -> Self {
        let mut a = [0u8; AREA_BYTES];
        a[FCW_AT..FCW_AT + 2].copy_from_slice(&FCW_RESET.to_le_bytes());
        a[MXCSR_AT..MXCSR_AT + 4].copy_from_slice(&MXCSR_RESET.to_le_bytes());
        a[XSTATE_BV_AT..XSTATE_BV_AT + 8].copy_from_slice(&(XCR0_X87 | XCR0_SSE).to_le_bytes());
        Self(a)
    }

    fn xstate_bv(&self) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.0[XSTATE_BV_AT..XSTATE_BV_AT + 8]);
        u64::from_le_bytes(b)
    }

    fn set_xstate_bv(&mut self, value: u64) {
        self.0[XSTATE_BV_AT..XSTATE_BV_AT + 8].copy_from_slice(&value.to_le_bytes());
    }
}

/// The XCR0 components the CPU this runs on can keep a guest's registers
/// in: the x87 and SSE where it has XSAVE -- every CPU with XSAVE has both --
/// and AVX with them where it has that too, laid out where the area has
/// room for it. 0 where it has no XSAVE: its guests have FXSAVE's registers
/// alone.
fn supported() -> u64 {
    const LEAF_FEATURES: u32 = 1;
    let Some(features) = cpu::cpuid(LEAF_FEATURES) else { return 0 };
    if features.ecx & cpu::ECX_XSAVE == 0 {
        return 0;
    }
    let Some(components) = cpu::cpuid_count(LEAF_XSAVE, 0) else { return 0 };
    let has = u64::from(components.eax) | u64::from(components.edx) << 32;
    if has & (XCR0_X87 | XCR0_SSE) != XCR0_X87 | XCR0_SSE {
        return 0;
    }
    let avx_fits = cpu::cpuid_count(LEAF_XSAVE, SUBLEAF_AVX)
        .map_or(false, |c| c.ebx as usize == AVX_OFFSET && c.eax as usize == AVX_BYTES);
    let avx = features.ecx & ECX_AVX != 0 && has & XCR0_AVX != 0 && avx_fits;
    XCR0_X87 | XCR0_SSE | if avx { XCR0_AVX } else { 0 }
}

/// Whether the CPU this runs on has XSAVES, and so IA32_XSS.
fn has_xsaves() -> bool {
    cpu::cpuid_count(LEAF_XSAVE, 1).map_or(false, |c| c.eax & EAX_XSAVES != 0)
}

/// What [`offered`] answers: `UNASKED` until it is first asked.
static OFFERED: AtomicU64 = AtomicU64::new(UNASKED);
const UNASKED: u64 = u64::MAX;

/// The XCR0 components a guest is offered -- its CPUID's leaf 0xD, and what
/// its XSETBVs may turn on: what the CPU first asked can keep, and that from
/// then on, as a guest's CPUID has to stay. A CPU unlike that one -- none
/// this kernel runs on -- is refused an entry whose XCR0 it cannot take
/// ([`GuestFp::fits`]) rather than fault in XSETBV.
pub fn offered() -> u64 {
    let known = OFFERED.load(Ordering::Relaxed);
    if known != UNASKED {
        return known;
    }
    match OFFERED.compare_exchange(UNASKED, supported(), Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => OFFERED.load(Ordering::Relaxed),
        Err(first) => first,
    }
}

/// Per CPU: the XCR0 its guests' registers are put back under -- what it
/// keeps of them, [`supported`] there -- or 0 where it has no XSAVE, or the
/// extension is off. Written by `on` and `off` on that CPU with interrupts
/// off, and read by its entries, with interrupts off.
static MASK: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

fn mask(cpu: u32) -> u64 {
    MASK.get(cpu as usize).map_or(0, |m| m.load(Ordering::Relaxed))
}

/// Per CPU: the CR4 bits `on` added (to take away again), and the XCR0 and
/// IA32_XSS it found, each with [`SAVED`] set when it is to be put back.
/// Written by `on` and `off` on that CPU, with interrupts off, and read by
/// nothing else.
struct Saved {
    cr4_added: AtomicU64,
    xcr0: AtomicU64,
    xss: AtomicU64,
}

const SAVED: u64 = 1 << 63;

static SAVED_STATE: [Saved; MAX_CPUS] = [const {
    Saved { cr4_added: AtomicU64::new(0), xcr0: AtomicU64::new(0), xss: AtomicU64::new(0) }
}; MAX_CPUS];

/// Set `CR4.OSFXSR`, and `CR4.OSXSAVE` where there is XSAVE, on the CPU this
/// runs on; make its XCR0 what its guests' registers are put back under, and
/// its IA32_XSS 0 -- a guest's XSAVES and XRSTORS, which AMD-V does not stop
/// and no CPUID of a guest's offers, would move the supervisor components it
/// names, the host's, and on Intel's processor trace load its MSRs -- all
/// remembered for [`off`].
///
/// # Safety
/// On the CPU it is for, with interrupts off, as the extension is being
/// turned on there; `off` runs on the same CPU before the extension is off.
pub unsafe fn on() {
    let cpu = kcore::cpu::id() as usize;
    let (Some(saved), Some(slot)) = (SAVED_STATE.get(cpu), MASK.get(cpu)) else { return };
    let xsave = cpu::has_xsave();
    let cr4 = cpu::read_cr4();
    let wanted = cpu::CR4_OSFXSR | if xsave { cpu::CR4_OSXSAVE } else { 0 };
    unsafe { cpu::write_cr4(cr4 | wanted) };
    saved.cr4_added.store(wanted & !cr4, Ordering::Relaxed);
    if !xsave {
        slot.store(0, Ordering::Relaxed);
        return;
    }
    let mask = supported();
    /* With no switch of the SSE component -- no CPU with XSAVE lacks it --
     * guests keep to FXSAVE's registers, and XCR0 to the x87 alone. */
    let xcr0 = unsafe { cpu::xgetbv0() };
    saved.xcr0.store(xcr0 | SAVED, Ordering::Relaxed);
    unsafe { set_xcr0(if mask != 0 { mask } else { XCR0_X87 }) };
    if has_xsaves() {
        let xss = unsafe { cpu::rdmsr(MSR_XSS) };
        if xss != 0 {
            unsafe { cpu::wrmsr(MSR_XSS, 0) };
            saved.xss.store(xss | SAVED, Ordering::Relaxed);
        }
    }
    slot.store(mask, Ordering::Relaxed);
}

/// Put back what [`on`] changed on the CPU this runs on.
///
/// # Safety
/// On the CPU `on` ran on, with interrupts off, before its extension goes
/// off.
pub unsafe fn off() {
    let cpu = kcore::cpu::id() as usize;
    let Some(saved) = SAVED_STATE.get(cpu) else { return };
    if let Some(slot) = MASK.get(cpu) {
        slot.store(0, Ordering::Relaxed);
    }
    let xss = saved.xss.swap(0, Ordering::Relaxed);
    if xss & SAVED != 0 {
        unsafe { cpu::wrmsr(MSR_XSS, xss & !SAVED) };
    }
    /* OSXSAVE is still set: `on` set it with the XCR0 it saved. */
    let xcr0 = saved.xcr0.swap(0, Ordering::Relaxed);
    if xcr0 & SAVED != 0 {
        unsafe { set_xcr0(xcr0 & !SAVED) };
    }
    let added = saved.cr4_added.swap(0, Ordering::Relaxed);
    unsafe { cpu::write_cr4(cpu::read_cr4() & !added) };
    /* Whatever a guest left in the registers stays, and is nobody's. */
    if let Some(o) = OWNER.get(cpu) {
        o.store(0, Ordering::Relaxed);
    }
}

/// Make XCR0 `value`, unless it is already: XSETBV serializes, and under a
/// nested hypervisor is an exit to it, where XGETBV is neither.
///
/// # Safety
/// CR4.OSXSAVE is set on this CPU, and `value` is an XCR0 it takes.
unsafe fn set_xcr0(value: u64) {
    if unsafe { cpu::xgetbv0() } != value {
        unsafe { cpu::xsetbv0(value) };
    }
}

/// A guest CPU's x87, SSE and AVX registers and its XCR0: what an entry
/// puts into the CPU ([`GuestFp::load`]) and its exit takes back out
/// ([`GuestFp::save`]).
pub struct GuestFp {
    /// One element, on the heap: a `Vec` because it can be made fallibly.
    area: Vec<Area>,
    /// The guest's XCR0: the x87 alone out of reset, and what its XSETBVs
    /// were let make it since ([`GuestFp::set_xcr0`]). INIT leaves it.
    xcr0: u64,
    /// Which guest's registers a CPU holds, this one being this ([`OWNER`]).
    id: u64,
    /// How many entries put its registers back, the CPU holding another's.
    loads: u64,
}

impl GuestFp {
    pub fn new() -> Result<Self> {
        let mut area = Vec::new();
        area.try_reserve_exact(1).map_err(|_| Error::NoMemory)?;
        area.push(Area::reset());
        Ok(Self { area, xcr0: XCR0_X87, id: owner_id(), loads: 0 })
    }

    pub fn xcr0(&self) -> u64 {
        self.xcr0
    }

    /// Make `value` the guest's XCR0, for an XSETBV of it the run loop
    /// performs -- if it is one a CPU offering [`offered`] takes: the x87 in
    /// it, AVX only with SSE, and nothing else. False otherwise, the XCR0 as
    /// it was: the #GP a CPU raises. What an entry makes XCR0 is never
    /// anything this did not take, so no XSETBV of the host's faults.
    pub fn set_xcr0(&mut self, value: u64) -> bool {
        let valid = value & XCR0_X87 != 0
            && value & !offered() == 0
            && (value & XCR0_AVX == 0 || value & XCR0_SSE != 0);
        if valid {
            self.xcr0 = value;
        }
        valid
    }

    /// How many of its entries put its registers back into a CPU that held
    /// another's: for a report, and for the guest that shows them kept.
    pub fn loads(&self) -> u64 {
        self.loads
    }

    /// Whether CPU `cpu` can run the guest under its XCR0: one the CPU
    /// switches every component of -- the x87 alone on a CPU without XSAVE.
    /// Asked with interrupts off on `cpu`, before anything of the entry is
    /// done.
    pub fn fits(&self, cpu: u32) -> bool {
        match mask(cpu) {
            0 => self.xcr0 == XCR0_X87,
            m => self.xcr0 & !m == 0,
        }
    }

    /// Put the guest's registers into CPU `cpu`, unless they are there from
    /// its last exit there already, and its XCR0 into XCR0.
    ///
    /// # Safety
    /// On `cpu`, the extension on for it, interrupts off from here to the
    /// [`GuestFp::save`] after the exit, and [`GuestFp::fits`] true of it.
    pub unsafe fn load(&mut self, cpu: u32) {
        let mask = mask(cpu);
        if !registers_are(cpu, self.id) {
            if mask == 0 {
                let area: *const Area = &self.area[0];
                unsafe { asm!("fxrstor64 [{}]", in(reg) area, options(nostack, preserves_flags)) };
            } else {
                /* Under XCR0 the whole of what this CPU switches, the guest's
                 * own state or the initial one in every register of it. A
                 * component the area says it holds that this CPU does not
                 * switch would be a #GP: no save on this CPU sets one, and
                 * it is none the guest can reach here. */
                unsafe { set_xcr0(mask) };
                let bv = self.area[0].xstate_bv();
                self.area[0].set_xstate_bv(bv & mask);
                let area: *const Area = &self.area[0];
                unsafe {
                    asm!("xrstor64 [{}]", in(reg) area, in("eax") mask as u32, in("edx") (mask >> 32) as u32,
                         options(nostack, preserves_flags));
                }
            }
            registers_loaded(cpu, self.id);
            self.loads += 1;
        }
        if mask != 0 {
            unsafe { set_xcr0(self.xcr0) };
        }
    }

    /// Take the guest's registers back out of CPU `cpu` after its exit: what
    /// its XCR0 names, which is all it can have changed -- by FXSAVE where
    /// that is the x87 alone, since the SSE registers are the guest's to use
    /// whatever its XCR0 says, and an XSAVE under it would leave them out.
    ///
    /// # Safety
    /// On `cpu`, interrupts off since the [`GuestFp::load`] before the entry,
    /// and nothing since then has written XCR0.
    pub unsafe fn save(&mut self, cpu: u32) {
        let xsave = mask(cpu) != 0;
        let area: *mut Area = &mut self.area[0];
        if xsave && self.xcr0 & XCR0_SSE != 0 {
            unsafe {
                asm!("xsave64 [{}]", in(reg) area, in("eax") self.xcr0 as u32, in("edx") (self.xcr0 >> 32) as u32,
                     options(nostack, preserves_flags));
            }
        } else {
            unsafe { asm!("fxsave64 [{}]", in(reg) area, options(nostack, preserves_flags)) };
            if xsave {
                /* What FXSAVE wrote is what XRSTOR is to take for these two. */
                let bv = self.area[0].xstate_bv();
                self.area[0].set_xstate_bv(bv | XCR0_X87 | XCR0_SSE);
            }
        }
    }
}
