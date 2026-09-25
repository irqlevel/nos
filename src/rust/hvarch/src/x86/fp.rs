//! The host CPU's x87/SSE control bits while its extension is on.
//!
//! Neither `vmrun` nor `vmlaunch` switches the x87 and SSE registers or
//! XCR0, so [`super::svm::Guest::run`] and [`super::vmx::Guest::run`] move
//! the guest's through FXSAVE and FXRSTOR -- which move the XMM registers
//! and MXCSR only with `CR4.OSFXSR` set -- and run it under an XCR0 of the
//! x87 alone, so that no register the FXSAVE area does not hold (AVX's) is
//! reachable from a guest. This kernel boots with OSFXSR clear and never
//! sets it: its C++ is built without SSE and its Rust is soft-float, so an
//! SSE instruction in it is a bug, and with the bit clear it faults.
//!
//! Setting the bit around every entry and clearing it after, as both `run`s
//! did, cost two serializing writes of CR4 an entry, an XGETBV, and under a
//! nested hypervisor an exit to the outer one for each -- a fifth of an exit
//! on real silicon, more than the world switch itself under nested KVM. So
//! now the bit is set once, here, when the extension is turned on for a CPU,
//! and cleared when it is turned off; and XCR0, if firmware left it with
//! more than the x87 in it, is made the x87 alone for the same span and put
//! back after. While the hypervisor is loaded an SSE instruction in the
//! kernel would run instead of faulting on those CPUs: the guard is given
//! up for the time the module is on, and for nothing else.
//!
//! What was there is remembered per CPU, so `off` puts back exactly what
//! `on` found: a CPU the extension goes off for is as it booted.

use core::sync::atomic::{AtomicU64, Ordering};

use kcore::consts::MAX_CPUS;

use super::cpu;

/// Whose x87/SSE registers each CPU holds: the guest whose FXRSTOR was the
/// last on it, by a number of its own ([`owner_id`]), or 0. What lets an
/// entry skip the FXRSTOR: the host never touches those registers -- its
/// C++ is built without them and its Rust is soft-float -- so between a
/// guest's exit on a CPU and its next entry there they still hold what its
/// FXSAVE at the exit saved, unless another guest's entry loaded its own in
/// between, which the number then says. A number is never given twice, so
/// a guest dropped and another made in its memory cannot be taken for it.
static OWNER: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// A number no guest has had, for the guest being made.
pub fn owner_id() -> u64 {
    NEXT_OWNER.fetch_add(1, Ordering::Relaxed)
}

/// Whether the registers of `cpu` are guest `id`'s already -- asked with
/// interrupts off on `cpu`, before its entry.
#[inline]
pub fn registers_are(cpu: u32, id: u64) -> bool {
    OWNER.get(cpu as usize).map_or(false, |o| o.load(Ordering::Relaxed) == id)
}

/// Guest `id` has just loaded its registers on `cpu`.
#[inline]
pub fn registers_loaded(cpu: u32, id: u64) {
    if let Some(o) = OWNER.get(cpu as usize) {
        o.store(id, Ordering::Relaxed);
    }
}

/// The XCR0 every CPU with XSAVE takes, and the one the guests run under.
pub const XCR0_X87: u64 = 1;

/// Per CPU: the CR4 bits `on` added (to take away again), and the XCR0 it
/// found, with [`XCR0_SAVED`] set when it changed it. Written by `on` and
/// `off` on that CPU, with interrupts off, and read by nothing else.
struct Saved {
    cr4_added: AtomicU64,
    xcr0: AtomicU64,
}

const XCR0_SAVED: u64 = 1 << 63;

static SAVED: [Saved; MAX_CPUS] =
    [const { Saved { cr4_added: AtomicU64::new(0), xcr0: AtomicU64::new(0) } }; MAX_CPUS];

/// Set `CR4.OSFXSR` on the CPU this runs on, and make its XCR0 the x87
/// alone, remembering both for [`off`].
///
/// # Safety
/// On the CPU it is for, with interrupts off, as the extension is being
/// turned on there; `off` runs on the same CPU before the extension is off.
pub unsafe fn on() {
    let cpu = kcore::cpu::id() as usize;
    let Some(saved) = SAVED.get(cpu) else { return };
    let cr4 = cpu::read_cr4();
    let added = cpu::CR4_OSFXSR & !cr4;
    unsafe { cpu::write_cr4(cr4 | cpu::CR4_OSFXSR) };
    if cpu::has_xsave() {
        /* XGETBV and XSETBV want OSXSAVE; it is set for their length only,
         * and stays clear as the kernel booted with it, unless it was on. */
        let with = cpu::read_cr4() | cpu::CR4_OSXSAVE;
        unsafe { cpu::write_cr4(with) };
        let xcr0 = unsafe { cpu::xgetbv0() };
        if xcr0 != XCR0_X87 {
            unsafe { cpu::xsetbv0(XCR0_X87) };
            saved.xcr0.store(xcr0 | XCR0_SAVED, Ordering::Relaxed);
        }
        if cr4 & cpu::CR4_OSXSAVE == 0 {
            unsafe { cpu::write_cr4(with & !cpu::CR4_OSXSAVE) };
        }
    }
    saved.cr4_added.store(added, Ordering::Relaxed);
}

/// Put back what [`on`] changed on the CPU this runs on.
///
/// # Safety
/// On the CPU `on` ran on, with interrupts off, before its extension goes
/// off.
pub unsafe fn off() {
    let cpu = kcore::cpu::id() as usize;
    let Some(saved) = SAVED.get(cpu) else { return };
    let xcr0 = saved.xcr0.swap(0, Ordering::Relaxed);
    if xcr0 & XCR0_SAVED != 0 {
        let cr4 = cpu::read_cr4();
        unsafe { cpu::write_cr4(cr4 | cpu::CR4_OSXSAVE) };
        unsafe { cpu::xsetbv0(xcr0 & !XCR0_SAVED) };
        unsafe { cpu::write_cr4(cr4) };
    }
    let added = saved.cr4_added.swap(0, Ordering::Relaxed);
    unsafe { cpu::write_cr4(cpu::read_cr4() & !added) };
    /* Whatever a guest left in the registers stays, and is nobody's. */
    if let Some(o) = OWNER.get(cpu) {
        o.store(0, Ordering::Relaxed);
    }
}
