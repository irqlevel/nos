//! Intel VT-x: what the CPU says about it, and entering root operation on
//! one CPU.
//!
//! VMX is the second backend, not the first, because nothing emulates it:
//! QEMU's TCG has no VMX at all, so on the development machine a VMX guest
//! cannot be brought up even slowly. It is written for the hardware this
//! kernel actually runs on -- the Hetzner EX44 and the Dell laptop are both
//! Intel -- where it is native and fast.

use core::arch::asm;

use super::cpu;
use crate::{Error, Result};

/* CPUID.1:ECX bit 5. */
pub const ECX_VMX: u32 = 1 << 5;

pub const MSR_FEATURE_CONTROL: u32 = 0x3A;
pub const FC_LOCK: u64 = 1 << 0;
pub const FC_VMXON_IN_SMX: u64 = 1 << 1;
pub const FC_VMXON: u64 = 1 << 2;

pub const MSR_VMX_BASIC: u32 = 0x480;
pub const MSR_VMX_PROCBASED_CTLS: u32 = 0x482;
pub const MSR_VMX_CR0_FIXED0: u32 = 0x486;
pub const MSR_VMX_CR0_FIXED1: u32 = 0x487;
pub const MSR_VMX_CR4_FIXED0: u32 = 0x488;
pub const MSR_VMX_CR4_FIXED1: u32 = 0x489;
pub const MSR_VMX_PROCBASED_CTLS2: u32 = 0x48B;
pub const MSR_VMX_EPT_VPID_CAP: u32 = 0x48C;

pub const CR4_VMXE: u64 = 1 << 13;

/// The primary processor-based control that says the secondary ones exist:
/// without it `IA32_VMX_PROCBASED_CTLS2` must not even be read.
pub const PROC_SECONDARY_CTLS: u32 = 1 << 31;

pub const SEC_EPT: u32 = 1 << 1;
pub const SEC_VIRTUALIZE_X2APIC: u32 = 1 << 4;
pub const SEC_VPID: u32 = 1 << 5;
pub const SEC_UNRESTRICTED_GUEST: u32 = 1 << 7;
pub const SEC_VIRTUAL_INTR_DELIVERY: u32 = 1 << 9;

/// The primary control the two above are built on: CR8 and the virtual
/// APIC's task priority in a page of the guest's own.
pub const PROC_TPR_SHADOW: u32 = 1 << 21;

/// The pin-based control that takes a posted interrupt into the guest
/// without an exit, and the VM-exit control it needs.
pub const PIN_POSTED_INTERRUPTS: u32 = 1 << 7;
pub const EXIT_ACK_INTR_ON_EXIT: u32 = 1 << 15;
const MSR_VMX_PINBASED: u32 = 0x481;
const MSR_VMX_EXIT: u32 = 0x483;

/* IA32_VMX_EPT_VPID_CAP: INVEPT and INVVPID are there, and which of their
 * types. */
/// The EPT walk takes an entry of the level above the last as a 2 MiB page.
pub const EPT_CAP_2M_PAGES: u64 = 1 << 16;
pub const EPT_CAP_INVEPT: u64 = 1 << 20;
pub const EPT_CAP_INVEPT_SINGLE_CONTEXT: u64 = 1 << 25;
pub const EPT_CAP_INVEPT_ALL_CONTEXT: u64 = 1 << 26;
pub const EPT_CAP_INVVPID: u64 = 1 << 32;
pub const EPT_CAP_INVVPID_SINGLE_CONTEXT: u64 = 1 << 41;
pub const EPT_CAP_INVVPID_ALL_CONTEXT: u64 = 1 << 42;

/// The secondary controls worth a line in a report.
pub const REPORTED: &[(u32, &str, &str)] = &[
    (SEC_EPT, "extended page tables", "guest physical addresses translated by the CPU"),
    (SEC_VPID, "VPID", "a guest's TLB entries need not be flushed on every entry"),
    (SEC_UNRESTRICTED_GUEST, "unrestricted guest", "a guest may start in real mode"),
];

/// The control-register bits worth naming when one of them is what stands
/// between this kernel and VMX operation.
pub const CR0_BITS: &[(u64, &str)] = &[
    (1 << 0, "PE"), (1 << 1, "MP"), (1 << 2, "EM"), (1 << 3, "TS"), (1 << 4, "ET"),
    (1 << 5, "NE"), (1 << 16, "WP"), (1 << 18, "AM"), (1 << 29, "NW"), (1 << 30, "CD"),
    (1 << 31, "PG"),
];

pub const CR4_BITS: &[(u64, &str)] = &[
    (1 << 0, "VME"), (1 << 1, "PVI"), (1 << 2, "TSD"), (1 << 3, "DE"), (1 << 4, "PSE"),
    (1 << 5, "PAE"), (1 << 6, "MCE"), (1 << 7, "PGE"), (1 << 8, "PCE"), (1 << 9, "OSFXSR"),
    (1 << 10, "OSXMMEXCPT"), (1 << 11, "UMIP"), (1 << 12, "LA57"), (1 << 13, "VMXE"),
    (1 << 14, "SMXE"), (1 << 16, "FSGSBASE"), (1 << 17, "PCIDE"), (1 << 18, "OSXSAVE"),
    (1 << 20, "SMEP"), (1 << 21, "SMAP"), (1 << 22, "PKE"), (1 << 23, "CET"), (1 << 24, "PKS"),
];

/// What VMX operation requires of the host's own CR0 and CR4, against what
/// they hold: the bits that are missing and the bits that are forbidden.
/// All four zero means the CPU would take a `vmxon` now.
///
/// This is worth a type rather than a boolean because `vmxon` does not fail
/// on it -- it *faults*, and this kernel's #GP handler panics. And the bit
/// most likely to be missing is `CR0.NE`, which every other kernel sets at
/// boot and GRUB leaves to whatever the firmware had: a machine where VMX
/// is perfectly present and one bit of CR0 is why no guest will start is
/// exactly the failure that has to name itself.
#[derive(Clone, Copy)]
pub struct HostState {
    pub cr0_missing: u64,
    pub cr0_forbidden: u64,
    pub cr4_missing: u64,
    pub cr4_forbidden: u64,
}

impl HostState {
    pub fn ok(&self) -> bool {
        self.cr0_missing | self.cr0_forbidden | self.cr4_missing | self.cr4_forbidden == 0
    }
}

/// Write-back: the only memory type a VMXON region or a VMCS may be in.
const MEM_TYPE_WB: u64 = 6;

/// What this CPU says about its VMX.
#[derive(Clone, Copy)]
pub struct Caps {
    /// `IA32_VMX_BASIC`: the revision identifier the CPU checks a VMXON
    /// region and a VMCS against, the size it wants them, and what memory
    /// type they may be in.
    pub basic: u64,
    /// `IA32_FEATURE_CONTROL`: whether firmware left VMX usable.
    pub feature_control: u64,
    /// The primary processor-based controls this CPU allows to be 1.
    pub primary: u32,
    /// The pin-based and the VM-exit controls this CPU allows to be 1.
    pub pin: u32,
    pub exit: u32,
    /// The secondary processor-based controls this CPU allows to be 1, or 0
    /// when it has no secondary controls at all.
    pub secondary: u32,
    /// `IA32_VMX_EPT_VPID_CAP`, or 0 when there is no EPT and no VPID.
    pub ept_vpid: u64,
    /// What CR0 and CR4 must and must not hold in VMX operation: the fixed0
    /// MSR names the bits that have to be 1, the fixed1 MSR the bits that
    /// may be.
    pub cr0_fixed0: u64,
    pub cr0_fixed1: u64,
    pub cr4_fixed0: u64,
    pub cr4_fixed1: u64,
}

impl Caps {
    /// None when the CPU has no VMX, which is the only state in which none
    /// of these MSRs may be read.
    pub fn probe() -> Option<Self> {
        if cpu::cpuid(1)?.ecx & ECX_VMX == 0 {
            return None;
        }
        let basic = unsafe { cpu::rdmsr(MSR_VMX_BASIC) };
        let feature_control = unsafe { cpu::rdmsr(MSR_FEATURE_CONTROL) };

        /* A capability MSR's high half is what the CPU allows to be 1 -- the
         * same in the TRUE MSRs. */
        let primary = (unsafe { cpu::rdmsr(MSR_VMX_PROCBASED_CTLS) } >> 32) as u32;
        let pin = (unsafe { cpu::rdmsr(MSR_VMX_PINBASED) } >> 32) as u32;
        let exit = (unsafe { cpu::rdmsr(MSR_VMX_EXIT) } >> 32) as u32;
        let secondary = if primary & PROC_SECONDARY_CTLS != 0 {
            (unsafe { cpu::rdmsr(MSR_VMX_PROCBASED_CTLS2) } >> 32) as u32
        } else {
            0
        };
        let ept_vpid = if secondary & (SEC_EPT | SEC_VPID) != 0 {
            unsafe { cpu::rdmsr(MSR_VMX_EPT_VPID_CAP) }
        } else {
            0
        };

        Some(Self {
            basic,
            feature_control,
            primary,
            pin,
            exit,
            secondary,
            ept_vpid,
            cr0_fixed0: unsafe { cpu::rdmsr(MSR_VMX_CR0_FIXED0) },
            cr0_fixed1: unsafe { cpu::rdmsr(MSR_VMX_CR0_FIXED1) },
            cr4_fixed0: unsafe { cpu::rdmsr(MSR_VMX_CR4_FIXED0) },
            cr4_fixed1: unsafe { cpu::rdmsr(MSR_VMX_CR4_FIXED1) },
        })
    }

    /// What goes in the first word of a VMXON region and of every VMCS: the
    /// CPU compares it with its own and refuses a structure from another
    /// model.
    pub fn revision(&self) -> u32 {
        (self.basic & 0x7FFF_FFFF) as u32
    }

    /// How many bytes of the region the CPU uses.
    pub fn region_size(&self) -> u32 {
        ((self.basic >> 32) & 0x1FFF) as u32
    }

    /// The memory type the region has to be in; anything but write-back and
    /// the CPU will not take it.
    pub fn memory_type(&self) -> u64 {
        (self.basic >> 50) & 0xF
    }

    /// Set when the CPU takes only 32-bit physical addresses for VMXON and
    /// VMCS pages -- true on nothing this kernel runs on, and fatal to
    /// assume the other way round on a machine with memory above 4 GiB.
    pub fn addresses_limited_to_32_bits(&self) -> bool {
        self.basic & (1 << 48) != 0
    }

    #[inline]
    pub fn has(&self, control: u32) -> bool {
        self.secondary & control != 0
    }

    /// The CPU delivers a guest's interrupts and takes its EOIs itself, in
    /// x2APIC mode: the TPR shadow, x2APIC virtualization and
    /// virtual-interrupt delivery are all there to be had.
    pub fn apicv(&self) -> bool {
        self.primary & PROC_TPR_SHADOW != 0
            && self.has(SEC_VIRTUALIZE_X2APIC)
            && self.has(SEC_VIRTUAL_INTR_DELIVERY)
    }

    /// The CPU takes an interrupt posted for a guest in its guest, without
    /// an exit: APIC virtualization, posted-interrupt processing, and the
    /// exit control that acknowledges the host's interrupts it needs.
    pub fn posted(&self) -> bool {
        self.apicv() && self.pin & PIN_POSTED_INTERRUPTS != 0 && self.exit & EXIT_ACK_INTR_ON_EXIT != 0
    }

    /// INVEPT can drop one EPT's translations alone; failing that, a guest's
    /// first entry on a CPU drops every EPT's there.
    pub fn invept_single_context(&self) -> bool {
        self.ept_vpid & EPT_CAP_INVEPT_SINGLE_CONTEXT != 0
    }

    /// Guests can be given VPIDs: the control is there, and so is INVVPID
    /// with the all-context type that turning the extension on needs.
    /// Without, a guest runs under VPID 0 with the host, and every
    /// transition flushes both.
    pub fn vpid(&self) -> bool {
        self.has(SEC_VPID)
            && self.ept_vpid & (EPT_CAP_INVVPID | EPT_CAP_INVVPID_ALL_CONTEXT)
                == EPT_CAP_INVVPID | EPT_CAP_INVVPID_ALL_CONTEXT
    }

    /// An EPT may map 2 MiB with one entry.
    pub fn ept_2m(&self) -> bool {
        self.ept_vpid & EPT_CAP_2M_PAGES != 0
    }

    pub fn invvpid_single_context(&self) -> bool {
        self.ept_vpid & EPT_CAP_INVVPID_SINGLE_CONTEXT != 0
    }

    /// Firmware locked `IA32_FEATURE_CONTROL` without allowing VMXON. Left
    /// unlocked, this kernel sets the bit itself; locked the wrong way, only
    /// a BIOS setting will do.
    pub fn firmware_disabled(&self) -> bool {
        self.feature_control & FC_LOCK != 0 && self.feature_control & FC_VMXON == 0
    }

    /// The control registers of the CPU this runs on, as VMX operation
    /// wants them. CR4 is read with `VMXE` already set, because that is how
    /// it will be when `vmxon` looks.
    pub fn host_state(&self) -> HostState {
        let cr0 = cpu::read_cr0();
        let cr4 = cpu::read_cr4() | CR4_VMXE;
        HostState {
            cr0_missing: self.cr0_fixed0 & !cr0,
            cr0_forbidden: cr0 & !self.cr0_fixed1,
            cr4_missing: self.cr4_fixed0 & !cr4,
            cr4_forbidden: cr4 & !self.cr4_fixed1,
        }
    }

    /// Whether a guest can be run on this machine. Not whether the host's
    /// control registers allow it on the CPU asking: those are per CPU --
    /// the APs come out of INIT with CR0.NE clear, the BSP has whatever
    /// firmware left -- and a machine-wide answer that changed with the CPU
    /// the question happened to run on would be no answer. [`enable`]
    /// checks them on each CPU, where the fault would be.
    pub fn usable(&self) -> Result<()> {
        if self.firmware_disabled() {
            return Err(Error::FirmwareDisabled);
        }
        if !self.has(SEC_EPT) {
            return Err(Error::NoNestedPaging);
        }
        /* And the instruction that drops what the CPU cached through an
         * EPT (`vmcs::invept`): without it a freed EPT's translations live
         * on under the next EPT made at the same address. Every CPU with
         * EPT has both types; one that had not could run no guest safely. */
        if self.ept_vpid & (EPT_CAP_INVEPT | EPT_CAP_INVEPT_ALL_CONTEXT)
            != EPT_CAP_INVEPT | EPT_CAP_INVEPT_ALL_CONTEXT
        {
            return Err(Error::NoNestedPaging);
        }
        if self.memory_type() != MEM_TYPE_WB || self.region_size() as usize > kcore::consts::PAGE_SIZE {
            return Err(Error::EnableFailed);
        }
        Ok(())
    }
}

/// Enter VMX root operation on the CPU this runs on.
///
/// # Safety
/// Runs on the CPU it is entering root operation on, and `vmxon_phys` is a
/// page whose first word is this CPU's VMX revision identifier, which stays
/// allocated and untouched until [`disable`] runs on that same CPU.
///
/// One consequence worth knowing before calling it: a CPU in VMX root
/// operation ignores INIT, so it cannot be brought back up through the
/// INIT/SIPI sequence the kernel starts APs with until VMXOFF.
pub unsafe fn enable(vmxon_phys: u64) -> Result<()> {
    let fc = unsafe { cpu::rdmsr(MSR_FEATURE_CONTROL) };
    if fc & FC_LOCK != 0 && fc & FC_VMXON == 0 {
        return Err(Error::FirmwareDisabled);
    }

    /* CR0 and CR4 have to be inside what VMX allows before VMXON, or the
     * instruction *faults* rather than failing -- a #GP, which this kernel
     * panics on. So it is checked, here on the CPU itself because the
     * registers differ from CPU to CPU, and `hv info` names the bit. */
    let caps = Caps::probe().ok_or(Error::NoExtension)?;
    if !caps.host_state().ok() {
        return Err(Error::HostState);
    }

    /* Only now, with every check passed, the one write here that cannot be
     * undone: the lock bit holds until the next reset, so a refusal above
     * must not come after it. Firmware left the MSR open, which makes it
     * ours to close -- VMXON outside SMX, and the lock, because the CPU
     * refuses VMXON while the MSR is unlocked. Every other OS leaves it the
     * same way, and almost every firmware locks it before any OS runs; but
     * it is the one thing `disable` cannot give back. */
    if fc & FC_LOCK == 0 {
        unsafe { cpu::wrmsr(MSR_FEATURE_CONTROL, fc | FC_LOCK | FC_VMXON) };
    }
    unsafe { cpu::write_cr4(cpu::read_cr4() | CR4_VMXE) };

    /* vmxon takes the address of a word holding the physical address, so
     * the operand has to be in memory: this local is it. Failure is in the
     * flags -- carry for "the CPU would not look at it", zero for "it
     * looked and said no" -- and setna covers both. */
    let operand = vmxon_phys;
    let failed: u8;
    unsafe {
        asm!("vmxon qword ptr [{addr}]", "setna {failed}",
             addr = in(reg) &operand, failed = out(reg_byte) failed,
             options(nostack));
    }
    if failed != 0 {
        unsafe { cpu::write_cr4(cpu::read_cr4() & !CR4_VMXE) };
        return Err(Error::EnableFailed);
    }

    /* Nothing cached through any EPT survives into this session: VMXOFF and
     * VMXON drop no guest-physical mapping, so what an earlier load's guests
     * left in this CPU's TLB is dropped here, by hand. `usable` checked the
     * type is there; a refusal now is a CPU that is not what it said. */
    if !unsafe { vmcs::invept(vmcs::INVEPT_ALL_CONTEXT, 0) } {
        unsafe { asm!("vmxoff", options(nostack)) };
        unsafe { cpu::write_cr4(cpu::read_cr4() & !CR4_VMXE) };
        return Err(Error::EnableFailed);
    }
    /* And nothing tagged with any VPID: a VPID this load hands out may be
     * one an earlier load's guest had on this CPU. */
    if caps.vpid() && !unsafe { vmcs::invvpid(vmcs::INVVPID_ALL_CONTEXT, 0) } {
        unsafe { asm!("vmxoff", options(nostack)) };
        unsafe { cpu::write_cr4(cpu::read_cr4() & !CR4_VMXE) };
        return Err(Error::EnableFailed);
    }
    /* The host's x87, SSE and AVX control bits as every entry wants them,
     * once, and IA32_XSS 0 (`super::fp`); and
     * what keeps a guest's speculation off the host's (`super::spec`). */
    unsafe { super::fp::on() };
    unsafe { super::spec::on(true) };
    Ok(())
}

/// Leave VMX root operation on the CPU this runs on -- unless a guest's VMCS
/// is still current here, in which case nothing is done and this says so:
/// VMXOFF under a current VMCS leaves the CPU's cached copy of it nowhere
/// good (the manual wants every active VMCS `vmclear`ed first), and the
/// guest that owns it would find the extension gone at its next entry. Only
/// [`Guest::run`] and the evictions it and [`Guest::drop`] send make a VMCS
/// current or not, so the count checked here is exact.
///
/// # Safety
/// Runs on the CPU it is leaving root operation on, and that CPU is in root
/// operation. The VMXON region may be freed once this has returned true.
pub unsafe fn disable() -> bool {
    let cpu = kcore::cpu::id() as usize;
    if LOADED.get(cpu).map_or(false, |n| n.load(Ordering::Acquire) != 0) {
        return false;
    }
    unsafe { super::spec::off() };
    unsafe { super::fp::off() };
    unsafe { asm!("vmxoff", options(nostack)) };
    /* Out of VMX operation, nothing is current. */
    if let Some(c) = CURRENT_VMCS.get(cpu) {
        c.store(0, Ordering::Release);
    }
    unsafe { cpu::write_cr4(cpu::read_cr4() & !CR4_VMXE) };
    true
}

/// Whether this CPU is in VMX root operation. There is no flag that says so
/// directly; CR4.VMXE is what sets it up and what `disable` clears, and
/// nothing else in this kernel ever touches that bit.
pub fn enabled() -> bool {
    cpu::read_cr4() & CR4_VMXE != 0
}

pub mod vmcs;

use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use kcore::consts::MAX_CPUS;

use super::fp::GuestFp;
use super::svm::{GuestRegs, Kick, NotRun, Profile, VECTOR_AC, VECTOR_DB, VECTOR_MC};
use super::svm::vmcb::{Save, Segment};

/// How many guests' VMCSs are current on each CPU: what [`disable`] refuses
/// VMXOFF over. Counted up where a VMCS is made current ([`now_loaded_on`]) and
/// down where it is `vmclear`ed off ([`now_unloaded`]), both on the CPU in
/// question with interrupts off -- as is [`disable`], which is why the count
/// it reads cannot change under it.
static LOADED: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// A guest's VMCS is current on `cpu` from here.
fn now_loaded_on(loaded_cpu: &AtomicI32, cpu: u32) {
    loaded_cpu.store(cpu as i32, Ordering::Release);
    if let Some(n) = LOADED.get(cpu as usize) {
        n.fetch_add(1, Ordering::AcqRel);
    }
}

/// A guest's VMCS is current nowhere from here, and in the clear state.
fn now_unloaded(loaded_cpu: &AtomicI32, launched: &AtomicBool) {
    let was = loaded_cpu.swap(-1, Ordering::AcqRel);
    launched.store(false, Ordering::Release);
    if was >= 0 {
        if let Some(n) = LOADED.get(was as usize) {
            n.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The VMCS each CPU has current -- the one its last VMPTRLD named, unless
/// a VMCLEAR of that one or a VMXOFF came after -- or 0. What lets an entry
/// skip the VMPTRLD when the VMCS it wants is current already, which after
/// the first entry of a guest that keeps to one CPU it always is. Kept
/// exact by the three instructions that change what is current, all of
/// which run on the CPU in question with interrupts off: [`Guest::run`]'s
/// VMPTRLD sets it, a VMCLEAR of the current VMCS ([`cleared_on`]) and
/// [`disable`]'s VMXOFF clear it.
static CURRENT_VMCS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The VMCS at `phys` was `vmclear`ed on `cpu`: current there no longer, if
/// it was.
fn cleared_on(cpu: usize, phys: u64) {
    if let Some(c) = CURRENT_VMCS.get(cpu) {
        let _ = c.compare_exchange(phys, 0, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// The VPIDs handed out, a bit each, set while a guest has the number. A
/// VPID is what the TLB tags a guest's translations with, so that a VM
/// entry or exit need drop neither the guest's nor the host's (which are
/// tagged 0) -- and so what two guests alive at once must never share, or
/// one would be served the other's translations. Taken with a compare-and-
/// exchange, given back with an and: no lock, since a guest is made and
/// dropped in task context but a lock would have to be made somewhere first.
/// 65535 of them, and 0 is the host's.
const VPID_WORDS: usize = 65536 / u64::BITS as usize;
static VPIDS: [AtomicU64; VPID_WORDS] = [const { AtomicU64::new(0) }; VPID_WORDS];

/// The lowest VPID nobody has, taken; None when every one is out.
fn vpid_take() -> Option<u16> {
    for (w, word) in VPIDS.iter().enumerate() {
        loop {
            let have = word.load(Ordering::Relaxed);
            let free = !have & if w == 0 { !1 } else { u64::MAX };
            if free == 0 {
                break;
            }
            let bit = free.trailing_zeros();
            if word.compare_exchange(have, have | (1 << bit), Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return Some((w * u64::BITS as usize + bit as usize) as u16);
            }
        }
    }
    None
}

/// `vpid` is nobody's again: after the last CPU it was current on has
/// `vmclear`ed the VMCS that named it, so the next guest given the number
/// starts, as every guest does, with an INVVPID on each CPU it runs on.
fn vpid_give_back(vpid: u16) {
    if vpid != 0 {
        let (w, bit) = (usize::from(vpid) / u64::BITS as usize, u32::from(vpid) % u64::BITS);
        VPIDS[w].fetch_and(!(1u64 << bit), Ordering::AcqRel);
    }
}

/* CR0 bits VMX may force in the guest; named here so `write_guest_state` can
 * lift them out of the fixed set when unrestricted guest relaxes them. */
const CR0_PE: u64 = 1 << 0;
const CR0_NE: u64 = 1 << 5;
const CR0_PG: u64 = 1 << 31;
/// EFER.LMA: long mode active, which the "IA-32e mode guest" entry control
/// has to match at every entry.
const EFER_LMA: u64 = 1 << 10;

/// CR4.LA57: five levels of paging (see [`NotRun::FiveLevelPaging`]).
const CR4_LA57: u64 = 1 << 12;
/// CR4.VMXE, which VMX forces set in the guest's CR0/CR4 fixed bits even
/// though the guest is told it has no VMX: masked so the guest reads 0.
const CR4_VMXE_BIT: u64 = 1 << 13;
/// CR4.MCE: a machine check an exception, not a shutdown. With it clear in
/// the CR4 a guest runs with, a machine check while it ran would be a
/// shutdown -- the guest's triple fault, as the exit has it -- and the host
/// would never hear of it. So the CPU's keeps it whatever the guest wrote.
const CR4_MCE: u64 = 1 << 6;
/// The CR4 bits the host keeps whatever is asked of [`Guest::new`]: in its
/// guest/host mask -- a `mov` that would change one of them exits -- and
/// set in the register, the guest reading the read shadow's instead.
const CR4_HOST_KEEPS: u64 = CR4_MCE | CR4_VMXE_BIT;

/* The controls without which a guest would reach what is the host's -- its
 * interrupts and NMIs, its ports, its CR8 (the local APIC's task priority),
 * the MONITOR, MWAIT and RDPMC CPUID denies the guest, its debug registers,
 * a WBINVD of the shared cache -- or run over memory and state that are not
 * its own: nested paging, the 64-bit host, EFER, the PAT and DR7 switched.
 * `adjust` drops a control the CPU, or a hypervisor under this one, does
 * not allow; one of these dropped refuses the guest ([`NotRun::Controls`]),
 * where a guest run without it would be the host's to lose. */
const PIN_REQUIRED: u32 = vmcs::PIN_EXTINT_EXITING | vmcs::PIN_NMI_EXITING;
/* CR8 is kept from the host's APIC by one of two: every `mov` of it exiting
 * (`CR8_EXITING`), or the TPR shadow, where the guest has a virtual APIC. */
const PROC_REQUIRED: u32 = vmcs::PROC_HLT_EXITING | vmcs::PROC_UNCOND_IO_EXITING
    | vmcs::PROC_MWAIT_EXITING | vmcs::PROC_MONITOR_EXITING | vmcs::PROC_RDPMC_EXITING
    | vmcs::PROC_MOV_DR_EXITING | vmcs::PROC_SECONDARY_CTLS;
const CR8_EXITING: u32 = vmcs::PROC_CR8_LOAD_EXITING | vmcs::PROC_CR8_STORE_EXITING;
/// The secondary controls a guest with a virtual APIC has while its APIC is
/// in x2APIC mode ([`Guest::set_apicv`]).
const PROC2_APICV: u32 = vmcs::PROC2_VIRTUALIZE_X2APIC | vmcs::PROC2_VIRTUAL_INTR_DELIVERY;
const PROC2_REQUIRED: u32 = vmcs::PROC2_ENABLE_EPT | vmcs::PROC2_WBINVD_EXITING;
const EXIT_REQUIRED: u32 = vmcs::EXIT_HOST_ADDR_SPACE_SIZE | vmcs::EXIT_LOAD_IA32_EFER
    | vmcs::EXIT_SAVE_IA32_EFER | vmcs::EXIT_LOAD_IA32_PAT | vmcs::EXIT_SAVE_IA32_PAT;
const ENTRY_REQUIRED: u32 = vmcs::ENTRY_LOAD_IA32_EFER | vmcs::ENTRY_LOAD_IA32_PAT | vmcs::ENTRY_LOAD_DEBUG;

/* Pause-loop exiting, where the CPU has it, in TSC ticks. A guest spinning
 * on PAUSE in its kernel -- waiting on a lock, an IPI's answer, a tick it
 * counts while it calibrates its timer -- exits once it has spun
 * `PLE_WINDOW`, about 26 us at the EX44's 2.5 GHz TSC, and is handed what
 * fell due meanwhile: soon enough for a tick a millisecond apart, where an
 * exit at every PAUSE cost a spinning CPU an exit's 1.8 us for each PAUSE
 * of tens of cycles. A PAUSE within `PLE_GAP` of the last is the same loop:
 * a loop's turn on Skylake and later, whose PAUSE alone is some 140 cycles
 * and whose core may run below its TSC's clock, is longer than the 128 KVM
 * takes, and a gap too short finds no loop and makes no exit at all. */
const PLE_GAP_TICKS: u64 = 1024;
const PLE_WINDOW_TICKS: u64 = 65536;

/// The EOI-exit bitmap's four fields, vectors 0-63 first.
const EOI_EXIT_FIELDS: [u32; 4] =
    [vmcs::EOI_EXIT_BITMAP0, vmcs::EOI_EXIT_BITMAP1, vmcs::EOI_EXIT_BITMAP2, vmcs::EOI_EXIT_BITMAP3];

/* What the launch stub does around the world switch (its `flags`). */
/// VMRESUME, the VMCS being in the launched state; else VMLAUNCH.
const RUN_RESUME: u64 = 1 << 0;
/// VERW just before the entry (`super::spec`).
const RUN_VERW: u64 = 1 << 1;
/// IA32_SPEC_CTRL written right after the exit, for legacy IBRS.
const RUN_SPEC_CTRL: u64 = 1 << 2;

/// The guest's general-purpose registers as the launch stub moves them: all
/// of them but RSP, which is a VMCS field. RAX is here too -- VMX, unlike
/// AMD-V, keeps no guest RAX of its own.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Gpr {
    rax: u64,
    rbx: u64,
    rcx: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    rbp: u64,
    r8: u64,
    r9: u64,
    r10: u64,
    r11: u64,
    r12: u64,
    r13: u64,
    r14: u64,
    r15: u64,
}

/// The host state the CPU restores on a VM exit, as this CPU has it now:
/// captured on the CPU the entry runs on, because every field of it is that
/// CPU's own -- its GS base is where its per-CPU data is, its TR its own
/// task register. Written into the VMCS host area before the entry.
struct HostRegs {
    cr0: u64,
    cr3: u64,
    cr4: u64,
    cs: u16,
    ss: u16,
    ds: u16,
    es: u16,
    fs: u16,
    gs: u16,
    tr: u16,
    fs_base: u64,
    gs_base: u64,
    tr_base: u64,
    gdt_base: u64,
    idt_base: u64,
    efer: u64,
    pat: u64,
}

/// Read a 16-byte system descriptor (a 64-bit TSS or LDT) out of the GDT and
/// return its base. The GDT is the host's own, mapped and readable; `sel` is
/// the selector, its index the entry.
unsafe fn descriptor_base(gdt_base: u64, sel: u16) -> u64 {
    let entry = gdt_base + (sel & !0x7) as u64;
    /* Bytes: base 15:0 at +2, 23:16 at +4, 31:24 at +7, 63:32 at +8. */
    let lo = unsafe { core::ptr::read_unaligned((entry + 2) as *const u16) } as u64;
    let mid = unsafe { core::ptr::read_unaligned((entry + 4) as *const u8) } as u64;
    let hi = unsafe { core::ptr::read_unaligned((entry + 7) as *const u8) } as u64;
    let top = unsafe { core::ptr::read_unaligned((entry + 8) as *const u32) } as u64;
    lo | (mid << 16) | (hi << 24) | (top << 32)
}

impl HostRegs {
    /// Read every host field off the CPU this runs on. CR4 among them is
    /// what the exit loads back, so it has to be the CR4 an entry runs under
    /// -- with OSFXSR and OSXSAVE, which the save after the exit needs to
    /// move the XMM and YMM registers at all -- and it is: both are set for
    /// the CPU while VMX is on for it (`super::fp`), not around each entry
    /// as OSFXSR once was, when a capture without it left FXSAVE
    /// implementation-defined and XSETBV an undefined opcode after every
    /// exit.
    fn capture() -> Self {
        let (cs, ss, ds, es, fs, gs, tr): (u16, u16, u16, u16, u16, u16, u16);
        unsafe {
            core::arch::asm!(
                "mov {cs:x}, cs", "mov {ss:x}, ss", "mov {ds:x}, ds",
                "mov {es:x}, es", "mov {fs:x}, fs", "mov {gs:x}, gs",
                "str {tr:x}",
                cs = out(reg) cs, ss = out(reg) ss, ds = out(reg) ds,
                es = out(reg) es, fs = out(reg) fs, gs = out(reg) gs,
                tr = out(reg) tr, options(nomem, nostack, preserves_flags));
        }
        let mut gdtr = [0u8; 10];
        let mut idtr = [0u8; 10];
        unsafe {
            core::arch::asm!("sgdt [{}]", in(reg) gdtr.as_mut_ptr(), options(nostack, preserves_flags));
            core::arch::asm!("sidt [{}]", in(reg) idtr.as_mut_ptr(), options(nostack, preserves_flags));
        }
        let gdt_base = u64::from_le_bytes(gdtr[2..10].try_into().unwrap());
        let idt_base = u64::from_le_bytes(idtr[2..10].try_into().unwrap());
        const MSR_FS_BASE: u32 = 0xC000_0100;
        const MSR_GS_BASE: u32 = 0xC000_0101;
        const MSR_PAT: u32 = 0x277;
        Self {
            cr0: cpu::read_cr0(),
            cr3: cpu::read_cr3(),
            cr4: cpu::read_cr4(),
            cs, ss, ds, es, fs, gs, tr,
            fs_base: unsafe { cpu::rdmsr(MSR_FS_BASE) },
            gs_base: unsafe { cpu::rdmsr(MSR_GS_BASE) },
            tr_base: unsafe { descriptor_base(gdt_base, tr) },
            gdt_base,
            idt_base,
            efer: unsafe { cpu::rdmsr(super::svm::MSR_EFER) },
            pat: unsafe { cpu::rdmsr(MSR_PAT) },
        }
    }

    /// Into the VMCS current on this CPU. A host selector's RPL and table
    /// bits must be clear (`& !0x7`), and CS and TR must not be null.
    unsafe fn write(&self) {
        use vmcs::*;
        unsafe {
            vmwrite(HOST_CR0, self.cr0);
            vmwrite(HOST_CR3, self.cr3);
            vmwrite(HOST_CR4, self.cr4);
            vmwrite(HOST_CS_SEL, (self.cs & !0x7) as u64);
            vmwrite(HOST_SS_SEL, (self.ss & !0x7) as u64);
            vmwrite(HOST_DS_SEL, (self.ds & !0x7) as u64);
            vmwrite(HOST_ES_SEL, (self.es & !0x7) as u64);
            vmwrite(HOST_FS_SEL, (self.fs & !0x7) as u64);
            vmwrite(HOST_GS_SEL, (self.gs & !0x7) as u64);
            vmwrite(HOST_TR_SEL, (self.tr & !0x7) as u64);
            vmwrite(HOST_FS_BASE, self.fs_base);
            vmwrite(HOST_GS_BASE, self.gs_base);
            vmwrite(HOST_TR_BASE, self.tr_base);
            vmwrite(HOST_GDTR_BASE, self.gdt_base);
            vmwrite(HOST_IDTR_BASE, self.idt_base);
            vmwrite(HOST_IA32_EFER, self.efer);
            vmwrite(HOST_IA32_PAT, self.pat);
            /* nos enters the kernel by SYSCALL, not SYSENTER: the host's
             * SYSENTER MSRs are 0, and the CPU need not restore junk. */
            vmwrite(HOST_IA32_SYSENTER_CS, 0);
            vmwrite(HOST_SYSENTER_ESP, 0);
            vmwrite(HOST_SYSENTER_EIP, 0);
        }
    }
}

/// A guest's CPU as VT-x keeps it: one VMCS, a shadow of the guest state
/// that policy above reads and writes as though it were a VMCB's save area
/// (`Save`), the registers the launch stub moves, and the x87, SSE and AVX
/// state and XCR0, which `vmlaunch` does not switch.
/// The MSRs the CPU switches around a guest through the VM-entry and VM-exit
/// load lists, because they are not VMCS fields and a guest that ran with the
/// host's would be catastrophic: `SYSCALL` jumps to the host's `LSTAR`, and
/// `SWAPGS` finds the host's shadow GS base. AMD-V moves these with
/// `vmsave`/`vmload`; VMX has no such instruction, so the CPU loads the
/// guest's on entry and the host's on exit from two lists this fills.
const SWAP_MSRS: [u32; 5] = [
    0xC000_0081, // IA32_STAR
    0xC000_0082, // IA32_LSTAR
    0xC000_0083, // IA32_CSTAR
    0xC000_0084, // IA32_FMASK
    0xC000_0102, // IA32_KERNEL_GS_BASE
];
/// IA32_KERNEL_GS_BASE, the one of [`SWAP_MSRS`] the guest changes without a
/// `wrmsr` the host hears: `swapgs` swaps it with the active GS base, and it
/// is not a VMCS field. So its guest value has to be *stored* on exit, or a
/// guest's `swapgs`-established kernel GS base is lost each round and the
/// next `swapgs` returns junk -- a garbage RSP on the interrupt-return path,
/// which is what Alpine's kernel double-faulted on before this.
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// Where each list sits in the MSR page: 16 bytes an entry (index, reserved,
/// value), the entry-load list at the start, the exit-load list past it, and
/// the exit-store list (KERNEL_GS_BASE alone) past that.
const MSR_ENTRY_BASE: usize = 0;
const MSR_EXIT_BASE: usize = 0x100;
const MSR_STORE_BASE: usize = 0x200;

/// The MSR bitmap: a bit an MSR, set for one whose `rdmsr` or `wrmsr` exits,
/// in four 1 KiB quarters -- reads of 0..0x1FFF, reads of 0xC0000000..,
/// writes of the first range, writes of the second. Every bit is set but
/// those of [`super::PASSTHROUGH_MSRS`], the guest's own bases: the first
/// two VMCS fields the exit saves and the entry loads, the third in the exit
/// MSR-store and entry MSR-load lists above.
const MSR_BITMAP_READ_HIGH: usize = 0x400;
const MSR_BITMAP_WRITE_LOW: usize = 0x800;
const MSR_BITMAP_WRITE_HIGH: usize = 0xC00;
const MSR_HIGH_BASE: u32 = 0xC000_0000;

/// The x2APIC registers whose writes virtual-interrupt delivery performs --
/// TPR, EOI and SELF_IPI -- and which the MSR bitmap lets through while it
/// is on. No read is let through: without APIC-register virtualization a
/// read of anything but TPR would reach the CPU's own APIC.
const APICV_WRITES: [u32; 3] = [0x808, 0x80B, 0x83F];

/* The virtual-APIC page: an APIC's registers at their xAPIC offsets, 16
 * bytes apart -- VTPR at 0x80, VISR's eight words from 0x100, VIRR's from
 * 0x200. */
const VAPIC_TPR: usize = 0x80;
const VAPIC_ISR: usize = 0x100;
const VAPIC_IRR: usize = 0x200;
const VAPIC_STRIDE: usize = 0x10;
pub const VAPIC_WORDS: usize = 8;

/// What the VMCS holds of the fields the policy may change between entries,
/// as of the last time each was written to it or read from it: what an
/// entry compares the shadow with, to write only what changed. Exact for a
/// field the guest cannot change without an exit (RIP, EFER, the PAT, the
/// SYSENTER MSRs, the interruptibility the CPU reports at every exit); the
/// FS and GS bases the guest does change without one (`wrfsbase`, `swapgs`),
/// so those are read on the exits where the policy may touch them -- `rdmsr`
/// and `wrmsr` -- and compared only against that read.
#[derive(Clone, Copy, Default)]
struct Synced {
    rip: u64,
    fs_base: u64,
    gs_base: u64,
    efer: u64,
    pat: u64,
    sysenter_cs: u64,
    sysenter_esp: u64,
    sysenter_eip: u64,
    interruptibility: u32,
    /// The pin-based controls, whose posted-interrupt processing follows
    /// virtual-interrupt delivery.
    pin: u32,
    proc1: u32,
    /// The secondary controls, whose x2APIC virtualization and
    /// virtual-interrupt delivery follow the guest's APIC (`set_apicv`).
    proc2: u32,
    eoi_exit: [u64; 4],
    /// The VM-entry controls, whose "IA-32e mode guest" follows EFER.LMA.
    entry_ctls: u32,
}

pub struct Guest {
    vmcs: vmcs::VmcsPage,
    /// One page holding the VM-entry MSR-load list (the guest's values of
    /// [`SWAP_MSRS`]) and the VM-exit MSR-load list (the host's), each an
    /// array of 16-byte entries the CPU reads.
    msr_area: kcore::dma::DmaBuffer,
    /// One page: the MSR bitmap above.
    msr_bitmap: kcore::dma::DmaBuffer,
    /// One page: the guest's virtual APIC -- VTPR, and its VIRR and VISR --
    /// where the CPU can deliver its interrupts itself and was asked to
    /// ([`Caps::apicv`], `Guest::new`); None where not, and CR8 then exits.
    /// The CPU reads and writes it only inside an entry this owner makes.
    vapic: Option<kcore::dma::DmaBuffer>,
    /// Virtual-interrupt delivery and x2APIC virtualization on from the next
    /// entry ([`set_apicv`](Self::set_apicv)).
    apicv_on: bool,
    /// The EOI-exit bitmap the next entry has: the vectors whose EOI, which
    /// the CPU performs, still exits.
    eoi_exit: [u64; 4],
    /// Where other CPUs post its interrupts, for the CPU to take in the
    /// guest while virtual-interrupt delivery is on ([`set_posted`]): the
    /// VMCS names it from the first entry, and every external interrupt's
    /// exit acknowledges the interrupt then -- `acked`, delivered to the
    /// host by hand after the exit ([`deliver_host_interrupt`]).
    posted: Option<alloc::sync::Arc<PostedInterrupts>>,
    posted_capable: bool,
    acked: Option<u8>,
    /// The guest state, kept in the same shape as an AMD-V save area so the
    /// policy layer is one set of code: synced to the VMCS before an entry
    /// and read back from it after.
    save: Save,
    regs: GuestRegs,
    /// Its x87, SSE and AVX registers and its XCR0 (`super::fp`).
    fp: GuestFp,

    /* Control values, decided once from the CPU's capabilities. */
    basic: u64,
    pin: u32,
    proc1: u32,
    proc2: u32,
    exit_ctls: u32,
    entry_ctls: u32,
    exceptions: u32,
    unrestricted: bool,
    /// INVEPT can name this guest's EPT alone; else it names every EPT.
    invept_single: bool,
    /// The VPID the guest's translations are tagged with, its own for as
    /// long as it lives, or 0 when the CPU has none to give: then it shares
    /// the host's, and every transition flushes both.
    vpid: u16,
    invvpid_single: bool,
    cr0_fixed0: u64,
    cr0_fixed1: u64,
    cr4_fixed0: u64,
    cr4_fixed1: u64,
    /// The CR0 bits the host keeps -- the guest/host mask: a guest's `mov`
    /// to CR0 that would change one exits instead, and the guest reads them
    /// from the read shadow. PG, because turning paging on or off with EFER.LME
    /// set turns long mode on or off, which the "IA-32e mode guest" entry
    /// control has to follow; PE with it; and every bit VMX fixes -- NE
    /// above all, which a CPU coming out of INIT has clear and VMX needs set,
    /// so the guest is let believe it wrote what it wrote.
    cr0_mask: u64,
    /// The policy wrote CR0 (`set_cr0`): the next entry writes the VMCS's
    /// CR0 and its read shadow.
    cr0_dirty: bool,
    /// The CR4 bits the host keeps -- the guest/host mask: a `mov` to CR4
    /// changing one exits, and the guest reads them from the read shadow.
    /// [`CR4_HOST_KEEPS`], and every bit the guest is not let have as its
    /// own ([`Guest::new`]): a bit its CPUID does not offer is then a write
    /// that exits, for the policy to refuse.
    cr4_mask: u64,
    /// What the read shadow holds: the guest's CR4 as it believes it, of
    /// which the mask's bits are what it reads.
    cr4_shadow: u64,
    /// What the VMCS's CR4 holds, as last written or read: the register the
    /// guest runs with, MCE and VMXE in it.
    cr4_hw: u64,
    /// The policy wrote CR4 (`set_cr4`): the next entry writes the VMCS's
    /// CR4 and its read shadow.
    cr4_dirty: bool,
    /// The guest's DR0-DR3: what its `mov`s of them -- every one an exit --
    /// read and write, and what the CPU's are made before each entry. 0 out
    /// of reset.
    debug_addrs: [u64; 4],
    /// The policy wrote DR7 (`set_dr7`): the next entry writes the VMCS's.
    dr7_dirty: bool,
    /// The next entry sets BS in the guest's pending debug exceptions: a
    /// #DB of its own came out with TF set inside an interrupt shadow, which
    /// a CPU taking the exit does not mark and the entry checks it is.
    pending_bs: bool,
    /// The next entry first drops what the CPU cached under this guest's
    /// VPID: its CR0 was written by the policy, not by an instruction the
    /// CPU ran -- which would have flushed as it went -- or it was reset.
    flush_vpid: bool,

    /// The CPU this guest's VMCS was last made current on with `vmptrld`, or
    /// -1. It is left current there after an exit -- no per-exit `vmclear` --
    /// so the next entry on the same CPU can `vmresume` it. Before the guest
    /// runs on any other CPU the VMCS is `vmclear`ed off this one
    /// ([`evict_here`]), so it is never current on two CPUs at once.
    loaded_cpu: AtomicI32,
    /// The current VMCS is in the launched state, so an entry `vmresume`s it
    /// rather than `vmlaunch`es: set after a successful entry, cleared with
    /// the VMCS itself (a migration, a drop, or a VMfail).
    launched: AtomicBool,
    /// The VMCS has been `vmclear`ed at least once. A freshly made VMCS is
    /// plain memory whose launch state is undefined, and `vmlaunch` needs it
    /// "clear"; the first entry `vmclear`s it (once VMX is known on -- a VM is
    /// made where it may be off), and from there every `vmclear` is the
    /// migration/drop/VMfail one, never a per-exit cost. Nested KVM launches
    /// an uncleared VMCS regardless, so this omission would pass every gate
    /// and VMfail only on real Intel silicon.
    cleared: bool,
    /// The fields written between entries, as the VMCS last had them.
    synced: Synced,
    /// The next entry writes the whole guest state, not only what the policy
    /// changes: true until the first entry, and again whenever the policy
    /// has set the whole shadow from scratch ([`mark_full_sync`]) -- the
    /// only time a full write is right, since between entries the shadow's
    /// heavy fields are what the guest had at its first entry, not now.
    full_sync: bool,
    /// The shadow's RSP was set by the policy and goes into the VMCS at the
    /// next entry: the one heavy field it writes, for a `mov rsp, cr8`.
    rsp_dirty: bool,
    /// The static VMCS fields (controls, EPTP, masks) have been written.
    configured: bool,
    /// The CPU whose host state is in the VMCS, or -1: rewritten when the
    /// guest's task has moved.
    host_cpu: i32,
    /// An event to inject on the next entry, in VMENTRY_INTR_INFO's format,
    /// with its error code; consumed by the entry.
    inject: u64,
    inject_errcode: u32,
    /// Ask the CPU to exit the moment the guest could take an interrupt: the
    /// interrupt-window control, toggled entry to entry.
    irq_window: bool,
    /// The same for an NMI: the NMI-window control, which needs virtual NMIs
    /// (`virtual_nmis`).
    nmi_window: bool,

    /* What the last exit wrote, read out before VMCLEAR. */
    exit_reason: u32,
    exit_qual: u64,
    exit_intr_info: u32,
    exit_intr_errcode: u32,
    exit_instr_len: u32,
    idt_vectoring_info: u32,
    idt_vectoring_errcode: u32,
    guest_phys: u64,
    interruptibility: u32,
    /// At a control-register access: CR0 as the guest sees it (the mask's
    /// bits from the read shadow), CR4, and CS's access rights.
    exit_cr0: u64,
    exit_cr4: u64,
    exit_cs_ar: u32,
    /// A VMLAUNCH that failed outright (VMfail), with the instruction error.
    vm_instruction_error: u32,
    entry_failed: bool,
    /// The shadow holds the whole of the guest's state: its control
    /// registers, segments and descriptor tables too, which an exit that
    /// does not end a guest leaves in the VMCS ([`read_guest_heavy`]) -- as
    /// they are before the first entry, and after the policy has set them
    /// all ([`mark_full_sync`]).
    whole_state: bool,
    /// Where each entry's time goes, while a benchmark asks.
    profile: Option<Profile>,
}

impl Guest {
    /// A guest CPU on a machine whose VMX `caps` say what its controls may
    /// be. `exceptions` is the vector bitmap the guest stops at, over the
    /// ones the host always takes. `cr4_own` are the CR4 bits the guest
    /// writes without an exit -- every other one, and MCE and VMXE whatever
    /// this says, a `mov` to CR4 that changes it exits, for the policy to
    /// answer ([`set_cr4`](Self::set_cr4)).
    pub fn new(caps: &Caps, exceptions: u32, cr4_own: u64, apicv: bool) -> Result<Self> {
        let fp = GuestFp::new()?;
        let vmcs = vmcs::VmcsPage::new(caps.revision())?;
        let mut msr_area = kcore::dma::DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        msr_area.as_mut_slice().fill(0);
        let mut msr_bitmap = kcore::dma::DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        msr_bitmap.as_mut_slice().fill(0xFF);
        for msr in super::PASSTHROUGH_MSRS {
            let bit = (msr - MSR_HIGH_BASE) as usize;
            for quarter in [MSR_BITMAP_READ_HIGH, MSR_BITMAP_WRITE_HIGH] {
                msr_bitmap.as_mut_slice()[quarter + bit / 8] &= !(1 << (bit % 8));
            }
        }
        let vapic = if apicv && caps.apicv() {
            let mut page = kcore::dma::DmaBuffer::new(1).ok_or(Error::NoMemory)?;
            page.as_mut_slice().fill(0);
            Some(page)
        } else {
            None
        };
        let unrestricted = caps.has(SEC_UNRESTRICTED_GUEST);
        let vpid = if caps.vpid() { vpid_take().ok_or(Error::NoMemory)? } else { 0 };
        let mut fixed_on = caps.cr0_fixed0;
        if unrestricted {
            fixed_on &= !(CR0_PE | CR0_PG);
        }
        let cr0_mask = CR0_PE | CR0_PG | CR0_NE | fixed_on | (!caps.cr0_fixed1 & 0xFFFF_FFFF);

        Ok(Self {
            vmcs,
            msr_area,
            msr_bitmap,
            posted_capable: vapic.is_some() && caps.posted(),
            vapic,
            apicv_on: false,
            eoi_exit: [0; 4],
            posted: None,
            acked: None,
            save: unsafe { core::mem::zeroed() },
            regs: GuestRegs::default(),
            fp,
            basic: caps.basic,
            pin: 0,
            proc1: 0,
            proc2: 0,
            exit_ctls: 0,
            entry_ctls: 0,
            exceptions,
            unrestricted,
            invept_single: caps.invept_single_context(),
            vpid,
            invvpid_single: caps.invvpid_single_context(),
            cr0_fixed0: caps.cr0_fixed0,
            cr0_fixed1: caps.cr0_fixed1,
            cr4_fixed0: caps.cr4_fixed0,
            cr4_fixed1: caps.cr4_fixed1,
            cr0_mask,
            cr0_dirty: false,
            cr4_mask: !cr4_own | CR4_HOST_KEEPS,
            cr4_shadow: 0,
            cr4_hw: 0,
            cr4_dirty: false,
            debug_addrs: [0; 4],
            dr7_dirty: false,
            pending_bs: false,
            flush_vpid: false,
            loaded_cpu: AtomicI32::new(-1),
            launched: AtomicBool::new(false),
            cleared: false,
            synced: Synced::default(),
            full_sync: true,
            rsp_dirty: false,
            configured: false,
            host_cpu: -1,
            inject: 0,
            inject_errcode: 0,
            irq_window: false,
            nmi_window: false,
            exit_reason: 0,
            exit_qual: 0,
            exit_intr_info: 0,
            exit_intr_errcode: 0,
            exit_instr_len: 0,
            idt_vectoring_info: 0,
            idt_vectoring_errcode: 0,
            guest_phys: 0,
            interruptibility: 0,
            exit_cr0: 0,
            exit_cr4: 0,
            exit_cs_ar: 0,
            vm_instruction_error: 0,
            entry_failed: false,
            whole_state: true,
            profile: None,
        })
    }

    /// Time each entry from now on ([`Profile`]), or stop.
    pub fn set_profile(&mut self, on: bool) {
        self.profile = if on { Some(Profile::default()) } else { None };
    }

    pub fn profile(&self) -> Option<Profile> {
        self.profile
    }

    /// The VPID the guest runs under, or None where the CPU has none.
    pub fn vpid(&self) -> Option<u16> {
        (self.vpid != 0).then_some(self.vpid)
    }

    pub fn save(&self) -> &Save {
        &self.save
    }
    pub fn save_mut(&mut self) -> &mut Save {
        &mut self.save
    }
    pub fn regs(&self) -> &GuestRegs {
        &self.regs
    }
    pub fn regs_mut(&mut self) -> &mut GuestRegs {
        &mut self.regs
    }
    pub fn save_and_regs_mut(&mut self) -> (&mut Save, &mut GuestRegs) {
        (&mut self.save, &mut self.regs)
    }
    /// The guest's XCR0.
    pub fn xcr0(&self) -> u64 {
        self.fp.xcr0()
    }

    /// Make `value` the guest's XCR0, for its XSETBV the policy performs --
    /// false, and nothing changed, for one a CPU refuses
    /// ([`GuestFp::set_xcr0`]).
    pub fn set_xcr0(&mut self, value: u64) -> bool {
        self.fp.set_xcr0(value)
    }

    /// How many entries put its x87, SSE and AVX registers back into a CPU
    /// that held another guest's.
    pub fn fp_loads(&self) -> u64 {
        self.fp.loads()
    }

    /// The policy has written the whole shadow -- every control register,
    /// segment and stack -- and the next entry is to load all of it, as the
    /// first did. Not for a change to one field after the guest has run: the
    /// rest of the shadow is stale by then, and would overwrite the guest.
    pub fn mark_full_sync(&mut self) {
        self.full_sync = true;
        self.whole_state = true;
    }

    /// Set the guest's RSP, which the guest otherwise keeps in the VMCS
    /// itself: written there at the next entry.
    pub fn set_rsp(&mut self, rsp: u64) {
        self.save.rsp = rsp;
        self.rsp_dirty = true;
    }

    /// Set the guest's CR0 to `value`, as it is to read it: the VMCS's CR0
    /// gets what VMX forces on top, the read shadow `value`, at the next
    /// entry -- which also drops what the CPU cached under the guest's VPID,
    /// as the `mov` this stands for would have. The policy has checked
    /// `value` is one CR0 may hold, and set EFER.LMA by it.
    pub fn set_cr0(&mut self, value: u64) {
        self.save.cr0 = value;
        self.cr0_dirty = true;
        self.flush_vpid = true;
    }

    /// Have the next entry drop what the CPU cached under this guest's VPID:
    /// the guest was reset, and nothing it translated before is to be used.
    pub fn flush_tlb(&mut self) {
        self.flush_vpid = true;
    }

    /// Make `value` the guest's CR4, for a `mov` to it the policy performed
    /// -- one that changed a bit of the mask, and exited: what the guest
    /// reads from now on, and with MCE and what VMX forces the VMCS's CR4,
    /// at the next entry; with `flush`, what the CPU cached under the
    /// guest's VPID dropped, as the instruction would have dropped it. The
    /// policy has checked `value` is one CR4 may hold.
    pub fn set_cr4(&mut self, value: u64, flush: bool) {
        self.save.cr4 = value;
        self.cr4_dirty = true;
        if flush {
            self.flush_vpid = true;
        }
    }

    /// The CR4 the guest runs with -- the VMCS's, as last written or read --
    /// MCE in it whatever the guest wrote.
    pub fn cr4_register(&self) -> u64 {
        self.cr4_hw
    }

    /// The CR4 bits the host keeps: a `mov` to CR4 changing one exits.
    pub fn cr4_mask(&self) -> u64 {
        self.cr4_mask
    }

    /// The guest's CR4 as it reads it, from what the VMCS's holds: the
    /// mask's bits the read shadow's, the rest the register's.
    fn cr4_view(&self, register: u64) -> u64 {
        (register & !self.cr4_mask) | (self.cr4_shadow & self.cr4_mask)
    }

    /// CR4 as the VMCS holds it for a guest that reads `value`: with the
    /// bits the host keeps and VMX forces set, and the ones it forbids clear.
    fn hw_cr4(&self, value: u64) -> u64 {
        (value | self.cr4_fixed0 | CR4_HOST_KEEPS) & self.cr4_fixed1
    }

    /// The guest's DR0-DR3: what its reads of them get, and where the
    /// breakpoints its DR7 enables are while it runs. Any value is sound to
    /// load: it is matched against the guest's own accesses alone, and only
    /// while the DR7 the entry loads for the guest enables it.
    pub fn debug_addrs(&self) -> &[u64; 4] {
        &self.debug_addrs
    }

    pub fn debug_addrs_mut(&mut self) -> &mut [u64; 4] {
        &mut self.debug_addrs
    }

    /// Make `value` the guest's DR7: into the VMCS at the next entry, which
    /// loads it -- its breakpoints armed from there.
    pub fn set_dr7(&mut self, value: u64) {
        self.save.dr7 = value;
        self.dr7_dirty = true;
    }

    /// Have the next entry set BS in the guest's pending debug exceptions:
    /// a #DB of its own exited with TF set inside a STI or MOV SS shadow,
    /// where a CPU's own exit would have marked a single step pending, an
    /// intercepted #DB's does not, and the entry refuses a guest without.
    pub fn set_pending_single_step(&mut self) {
        self.pending_bs = true;
    }

    /// Whether every `mov` of a debug register stops the guest: the control
    /// as its VMCS has it, after its first entry.
    pub fn stops_debug_movs(&self) -> bool {
        self.configured && self.proc1 & vmcs::PROC_MOV_DR_EXITING != 0
    }

    /// Whether the CPU has unrestricted guest: a guest may run in real mode,
    /// or protected mode without paging -- which an application processor
    /// a start-up IPI starts is.
    pub fn unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// At an exit for a control-register access: CR0 as the guest sees it,
    /// CR4, and CS's access rights (VMX's format: L is bit 13).
    pub fn exit_cr0(&self) -> u64 {
        self.exit_cr0
    }
    pub fn exit_cr4(&self) -> u64 {
        self.exit_cr4
    }
    pub fn exit_cs_ar(&self) -> u32 {
        self.exit_cs_ar
    }

    /// CR0 as the VMCS holds it for a guest that reads `value`: with the
    /// bits VMX forces set -- all but PE and PG, with unrestricted guest --
    /// and the ones it forbids clear.
    fn hw_cr0(&self, value: u64) -> u64 {
        let mut fixed_on = self.cr0_fixed0;
        if self.unrestricted {
            fixed_on &= !(CR0_PE | CR0_PG);
        }
        (value | fixed_on) & self.cr0_fixed1
    }

    /// The VM-entry controls for a guest whose EFER is `efer`: "IA-32e mode
    /// guest" exactly when long mode is active, as every entry checks.
    fn entry_ctls_for(&self, efer: u64) -> u32 {
        self.entry_ctls | if efer & EFER_LMA != 0 { vmcs::ENTRY_IA32E_MODE_GUEST } else { 0 }
    }

    /* The last exit, for the policy layer's decoder and its reports. */
    pub fn exit_reason(&self) -> u32 {
        self.exit_reason
    }
    pub fn exit_qualification(&self) -> u64 {
        self.exit_qual
    }
    pub fn exit_intr_info(&self) -> u32 {
        self.exit_intr_info
    }
    pub fn exit_intr_errcode(&self) -> u32 {
        self.exit_intr_errcode
    }
    pub fn exit_instr_len(&self) -> u32 {
        self.exit_instr_len
    }
    pub fn idt_vectoring_info(&self) -> u32 {
        self.idt_vectoring_info
    }
    pub fn idt_vectoring_errcode(&self) -> u32 {
        self.idt_vectoring_errcode
    }
    pub fn guest_physical_address(&self) -> u64 {
        self.guest_phys
    }
    pub fn interruptibility(&self) -> u32 {
        self.interruptibility
    }
    pub fn vm_instruction_error(&self) -> u32 {
        self.vm_instruction_error
    }
    pub fn entry_failed(&self) -> bool {
        self.entry_failed
    }

    /// Queue an event for injection on the next entry: `info` in
    /// VMENTRY_INTR_INFO's format, `errcode` the error code when its
    /// deliver-error bit is set.
    pub fn set_inject(&mut self, info: u32, errcode: u32) {
        self.inject = info as u64;
        self.inject_errcode = errcode;
    }
    pub fn clear_inject(&mut self) {
        self.inject = 0;
    }
    pub fn inject_valid(&self) -> bool {
        self.inject as u32 & vmcs::intr::VALID != 0
    }
    pub fn set_interruptibility(&mut self, bits: u32) {
        self.interruptibility = bits;
    }

    /// Whether the guest has a virtual APIC: CR8 its VTPR, under the TPR
    /// shadow, and virtual-interrupt delivery to be had while its APIC is in
    /// x2APIC mode ([`set_apicv`](Self::set_apicv)).
    pub fn has_vapic(&self) -> bool {
        self.vapic.is_some()
    }

    /// Whether interrupts posted for it can be taken in the guest
    /// ([`set_posted`](Self::set_posted)): a virtual APIC, and the CPU's
    /// posted-interrupt processing.
    pub fn posted_capable(&self) -> bool {
        self.posted_capable
    }

    /// `posted` is where other CPUs post its interrupts: taken by the CPU
    /// in the guest while virtual-interrupt delivery is on, the rest of the
    /// time by the run loop ([`PostedInterrupts::take`]). Before its first
    /// entry only, which names it in the VMCS; false, and nothing kept,
    /// after it or on a CPU without posted-interrupt processing.
    pub fn set_posted(&mut self, posted: alloc::sync::Arc<PostedInterrupts>) -> bool {
        if !self.posted_capable || self.configured {
            return false;
        }
        self.posted = Some(posted);
        true
    }

    /// Virtual-interrupt delivery and x2APIC virtualization, on or off from
    /// the next entry. On, the CPU delivers what VIRR holds itself, when the
    /// guest can take it, and performs the guest's writes of its x2APIC's
    /// TPR, EOI and SELF_IPI registers on the page -- the MSR bitmap letting
    /// those three through, and only while on: off, they exit as every other
    /// x2APIC register's do. Nothing without a virtual APIC.
    pub fn set_apicv(&mut self, on: bool) {
        if self.vapic.is_none() || on == self.apicv_on {
            return;
        }
        self.apicv_on = on;
        let bitmap = self.msr_bitmap.as_mut_slice();
        for msr in APICV_WRITES {
            let (byte, bit) = (MSR_BITMAP_WRITE_LOW + msr as usize / 8, msr % 8);
            if on {
                bitmap[byte] &= !(1 << bit);
            } else {
                bitmap[byte] |= 1 << bit;
            }
        }
    }

    /// Whether the next entry has virtual-interrupt delivery on.
    pub fn apicv(&self) -> bool {
        self.apicv_on
    }

    /// VTPR, the guest's task priority under the TPR shadow: what its `mov`s
    /// of CR8 read and write (bits 7:4) and, under virtual-interrupt
    /// delivery, its writes of the x2APIC's TPR. 0 without a virtual APIC.
    pub fn virtual_tpr(&self) -> u8 {
        self.vapic.as_ref().and_then(|p| p.load::<u32>(VAPIC_TPR)).unwrap_or(0) as u8
    }

    pub fn set_virtual_tpr(&mut self, tpr: u8) {
        if let Some(p) = self.vapic.as_mut() {
            p.store::<u32>(VAPIC_TPR, u32::from(tpr));
        }
    }

    /// VIRR and VISR, a bit a vector in eight words, as the CPU left them --
    /// interrupts delivered, EOIs and self-IPIs performed -- or as the run
    /// loop set them for the next entry. Zero without a virtual APIC.
    pub fn virtual_irr(&self) -> [u32; VAPIC_WORDS] {
        self.vapic_words(VAPIC_IRR)
    }

    pub fn virtual_isr(&self) -> [u32; VAPIC_WORDS] {
        self.vapic_words(VAPIC_ISR)
    }

    pub fn set_virtual_irr(&mut self, words: &[u32; VAPIC_WORDS]) {
        self.set_vapic_words(VAPIC_IRR, words);
    }

    pub fn set_virtual_isr(&mut self, words: &[u32; VAPIC_WORDS]) {
        self.set_vapic_words(VAPIC_ISR, words);
    }

    /// The vectors, a bit each, whose EOI exits though the CPU performs it:
    /// the level-triggered ones, whose end the IO-APIC has to hear of.
    pub fn set_eoi_exit(&mut self, bitmap: [u64; 4]) {
        self.eoi_exit = bitmap;
    }

    fn vapic_words(&self, base: usize) -> [u32; VAPIC_WORDS] {
        let mut words = [0; VAPIC_WORDS];
        if let Some(p) = self.vapic.as_ref() {
            for (i, w) in words.iter_mut().enumerate() {
                *w = p.load::<u32>(base + i * VAPIC_STRIDE).unwrap_or(0);
            }
        }
        words
    }

    fn set_vapic_words(&mut self, base: usize, words: &[u32; VAPIC_WORDS]) {
        if let Some(p) = self.vapic.as_mut() {
            for (i, &w) in words.iter().enumerate() {
                p.store::<u32>(base + i * VAPIC_STRIDE, w);
            }
        }
    }

    /// RVI and SVI as the guest interrupt status has them: the highest
    /// vector in VIRR, and in VISR, each 0 for none.
    fn intr_status(&self) -> u16 {
        let highest = |words: [u32; VAPIC_WORDS]| -> u16 {
            for i in (0..VAPIC_WORDS).rev() {
                if words[i] != 0 {
                    return (i as u16) * 32 + (31 - words[i].leading_zeros() as u16);
                }
            }
            0
        };
        let rvi = highest(self.virtual_irr());
        let svi = highest(self.virtual_isr());
        rvi | (svi << 8)
    }

    /// Whether to exit as soon as the guest can take an interrupt.
    pub fn set_irq_window(&mut self, on: bool) {
        self.irq_window = on;
    }

    /// Whether to exit as soon as the guest can take an NMI. Nothing, on a
    /// CPU without virtual NMIs: its next exit comes anyway.
    pub fn set_nmi_window(&mut self, on: bool) {
        self.nmi_window = on;
    }

    /// Whether the guest's NMI blocking is its own, ended by its IRET: an NMI
    /// is injected into a guest only then. Decided at the first entry.
    pub fn virtual_nmis(&self) -> bool {
        self.pin & vmcs::PIN_VIRTUAL_NMIS != 0
    }

    /// Compute and record the control values this CPU allows, once -- and
    /// refuse, naming what is missing, where a control the host cannot do
    /// without is not among them (`PIN_REQUIRED` and the rest), or one the
    /// guest was promised is not: unrestricted guest, its VPID.
    fn decide_controls(&mut self, basic: u64, cpu: u32) -> core::result::Result<(), NotRun> {
        use vmcs::*;
        /* Virtual NMIs: the guest's NMI blocking is its own, set as an NMI
         * is injected and ended by the guest's IRET -- what an NMI one of its
         * CPUs sends another needs. The host's NMIs exit either way. */
        self.pin = adjust(
            PIN_EXTINT_EXITING | PIN_NMI_EXITING | PIN_VIRTUAL_NMIS,
            ctls_msr(basic, MSR_VMX_PINBASED_CTLS, MSR_VMX_TRUE_PINBASED_CTLS),
        );
        /* The instructions AMD-V's intercept set stops a guest at, where
         * VMX does not stop it unasked: HLT; every port; CR8, which is the
         * host's task priority register; MONITOR, MWAIT and RDPMC, which
         * CPUID says the guest has not got; every `mov` of a debug register,
         * whose four addresses and DR6 are the CPU's; WBINVD, below; and
         * PAUSE, as the policy asks of both -- at a loop of them that has
         * spun a while where the CPU has pause-loop exiting (`PLE_GAP_TICKS`),
         * and at every one where it has not, as under a hypervisor that does
         * not pass it on. CPUID, INVD, VMCALL, XSETBV and a triple fault exit
         * unconditionally. */
        let ple = adjust(PROC2_PAUSE_LOOP_EXITING, MSR_VMX_PROCBASED_CTLS2) & PROC2_PAUSE_LOOP_EXITING != 0;
        /* CR8: the virtual APIC's VTPR under the TPR shadow, where the guest
         * has one; else every `mov` of it exits, for the run loop's shadow. */
        let cr8 = if self.vapic.is_some() { PROC_USE_TPR_SHADOW } else { CR8_EXITING };
        self.proc1 = adjust(
            PROC_HLT_EXITING | PROC_UNCOND_IO_EXITING | cr8
                | PROC_MWAIT_EXITING | PROC_MONITOR_EXITING | PROC_RDPMC_EXITING | PROC_MOV_DR_EXITING
                | PROC_USE_MSR_BITMAPS | if ple { 0 } else { PROC_PAUSE_EXITING } | PROC_SECONDARY_CTLS,
            ctls_msr(basic, MSR_VMX_PROCBASED_CTLS, MSR_VMX_TRUE_PROCBASED_CTLS),
        );
        /* Without the bitmap control every MSR exits, which is the safe way
         * round: the bitmap then lets nothing through, and the CPU ignores
         * the address. */
        if self.proc1 & PROC_USE_MSR_BITMAPS == 0 {
            self.msr_bitmap.as_mut_slice().fill(0xFF);
        }
        let mut want2 = PROC2_ENABLE_EPT | PROC2_WBINVD_EXITING;
        if ple {
            want2 |= PROC2_PAUSE_LOOP_EXITING;
        }
        if self.unrestricted {
            want2 |= PROC2_UNRESTRICTED_GUEST;
        }
        if self.vpid != 0 {
            want2 |= PROC2_ENABLE_VPID;
        }
        self.proc2 = adjust(want2, MSR_VMX_PROCBASED_CTLS2);
        /* DR7 saved at the exit and loaded at the entry: the guest's own,
         * its breakpoints armed while it runs and none of them after. */
        /* With posted interrupts, every external interrupt's exit
         * acknowledges it, for the host to be handed by hand: what
         * posted-interrupt processing needs. */
        let ack = if self.posted.is_some() { EXIT_ACK_INTR_ON_EXIT } else { 0 };
        self.exit_ctls = adjust(
            EXIT_HOST_ADDR_SPACE_SIZE | EXIT_LOAD_IA32_EFER | EXIT_SAVE_IA32_EFER
                | EXIT_LOAD_IA32_PAT | EXIT_SAVE_IA32_PAT | EXIT_SAVE_DEBUG | ack,
            ctls_msr(basic, MSR_VMX_EXIT_CTLS, MSR_VMX_TRUE_EXIT_CTLS),
        );
        /* "IA-32e mode guest" is not among them: it follows the guest's
         * EFER.LMA entry to entry (`entry_ctls_for`). */
        self.entry_ctls = adjust(
            ENTRY_LOAD_IA32_EFER | ENTRY_LOAD_IA32_PAT | ENTRY_LOAD_DEBUG,
            ctls_msr(basic, MSR_VMX_ENTRY_CTLS, MSR_VMX_TRUE_ENTRY_CTLS),
        ) & !ENTRY_IA32E_MODE_GUEST;

        /* What a guest is run with is what it was promised: the host's
         * controls, and the guest's own -- a VPID handed out, a real-mode
         * CPU -- where the flags above say it has them. */
        let promised2 = want2 & (PROC2_UNRESTRICTED_GUEST | PROC2_ENABLE_VPID);
        /* With a virtual APIC: the TPR shadow, and the two controls its
         * x2APIC mode turns on, which the CPU said it had (`Caps::apicv`) --
         * asked again here, of the CPU the guest runs on. */
        let (cr8_needed, apicv_got, apicv_needed) = if self.vapic.is_some() {
            (PROC_USE_TPR_SHADOW, adjust(want2 | PROC2_APICV, MSR_VMX_PROCBASED_CTLS2), PROC2_APICV)
        } else {
            (CR8_EXITING, 0, 0)
        };
        /* With posted interrupts: their processing, which the entries turn
         * on with virtual-interrupt delivery, and the exit's acknowledgement. */
        let (posted_got, posted_needed, ack_needed) = if self.posted.is_some() {
            let got = adjust(self.pin | PIN_POSTED_INTERRUPTS,
                             ctls_msr(basic, MSR_VMX_PINBASED_CTLS, MSR_VMX_TRUE_PINBASED_CTLS));
            (got, PIN_POSTED_INTERRUPTS, EXIT_ACK_INTR_ON_EXIT)
        } else {
            (0, 0, 0)
        };
        let checks: [(&'static str, u32, u32); 9] = [
            ("pin-based", self.pin, PIN_REQUIRED),
            ("pin-based (posted interrupts)", posted_got, posted_needed),
            ("VM-exit (posted interrupts)", self.exit_ctls, ack_needed),
            ("processor-based", self.proc1, PROC_REQUIRED),
            ("processor-based (CR8)", self.proc1, cr8_needed),
            ("secondary processor-based", self.proc2, PROC2_REQUIRED | promised2),
            ("secondary processor-based (APIC virtualization)", apicv_got, apicv_needed),
            ("VM-exit", self.exit_ctls, EXIT_REQUIRED),
            ("VM-entry", self.entry_ctls, ENTRY_REQUIRED),
        ];
        for (field, got, required) in checks {
            if got & required != required {
                return Err(NotRun::Controls { cpu, field, missing: required & !got });
            }
        }
        Ok(())
    }

    /// Write the fields that do not change entry to entry: the controls, the
    /// exception bitmap, the EPT pointer, the CR0/CR4 masks and shadows, and
    /// the VMCS link pointer. The VMCS is current on this CPU, `cpu`. A CPU
    /// that will not take the controls the host needs refuses the guest
    /// ([`NotRun::Controls`]), nothing written.
    unsafe fn configure(&mut self, eptp: u64, cpu: u32) -> core::result::Result<(), NotRun> {
        use vmcs::*;
        let basic = self.basic;
        self.decide_controls(basic, cpu)?;
        unsafe {
            vmwrite(PIN_BASED_CTLS, self.pin as u64);
            vmwrite(PROC_BASED_CTLS, self.proc1 as u64);
            vmwrite(PROC_BASED_CTLS2, self.proc2 as u64);
            vmwrite(VMEXIT_CTLS, self.exit_ctls as u64);
            /* VMENTRY_CTLS the first entry writes, from the guest's EFER. */
            /* Intercept #DB, #AC and #MC always -- the same delivery-loop
             * denial-of-service the AMD side guards against -- over whatever
             * else the policy asked. */
            let host_exc = (1u32 << VECTOR_DB) | (1u32 << VECTOR_AC) | (1u32 << VECTOR_MC);
            vmwrite(EXCEPTION_BITMAP, (self.exceptions | host_exc) as u64);
            vmwrite(PAGE_FAULT_ERRCODE_MASK, 0);
            vmwrite(PAGE_FAULT_ERRCODE_MATCH, 0);
            vmwrite(CR3_TARGET_COUNT, 0);
            if self.proc2 & PROC2_PAUSE_LOOP_EXITING != 0 {
                vmwrite(PLE_GAP, PLE_GAP_TICKS);
                vmwrite(PLE_WINDOW, PLE_WINDOW_TICKS);
            }
            /* Every port exits, and every MSR but the three bases the bitmap
             * lets through: nothing else the guest reads or writes reaches
             * the host's real MSRs or ports. */
            vmwrite(MSR_BITMAP, self.msr_bitmap.phys());
            vmwrite(EPT_POINTER, eptp);
            vmwrite(VMCS_LINK_POINTER, u64::MAX);
            vmwrite(VPID, u64::from(self.vpid));
            let msr_phys = self.msr_area.phys();
            vmwrite(VMENTRY_MSR_LOAD_ADDR, msr_phys + MSR_ENTRY_BASE as u64);
            vmwrite(VMENTRY_MSR_LOAD_COUNT, SWAP_MSRS.len() as u64);
            vmwrite(VMEXIT_MSR_LOAD_ADDR, msr_phys + MSR_EXIT_BASE as u64);
            vmwrite(VMEXIT_MSR_LOAD_COUNT, SWAP_MSRS.len() as u64);
            vmwrite(VMEXIT_MSR_STORE_ADDR, msr_phys + MSR_STORE_BASE as u64);
            vmwrite(VMEXIT_MSR_STORE_COUNT, 1);
            vmwrite(TSC_OFFSET, 0);
            /* The guest owns CR0 but for the bits of `cr0_mask`, and CR4
             * but for those of `cr4_mask` -- MCE, kept set in the register,
             * and VMXE, forced set in hardware and read as 0 by a guest told
             * it has no VMX, among them -- which a `mov` changing one exits
             * for. */
            vmwrite(CR0_GUEST_HOST_MASK, self.cr0_mask);
            vmwrite(CR4_GUEST_HOST_MASK, self.cr4_mask);
            vmwrite(GUEST_IA32_DEBUGCTL, 0);
            /* The virtual APIC's page, and its fields as an APIC out of
             * reset has them: written whatever the guest's mode, since a
             * VMCS field nobody wrote holds whatever it held. No TPR
             * threshold: a `mov` to CR8 never exits for one. */
            if let Some(page) = &self.vapic {
                vmwrite(VIRTUAL_APIC_PAGE_ADDR, page.phys());
                vmwrite(TPR_THRESHOLD, 0);
                vmwrite(GUEST_INTR_STATUS, 0);
                for (field, word) in EOI_EXIT_FIELDS.iter().zip(self.eoi_exit) {
                    vmwrite(*field, word);
                }
            }
            /* Where interrupts are posted for it, and the vector their
             * notification is. */
            if let Some(posted) = &self.posted {
                vmwrite(POSTED_INTR_NV, u64::from(kcore::cpu::posted_interrupt_vector()));
                vmwrite(POSTED_INTR_DESC_ADDR, posted.phys());
            }
        }
        self.synced.pin = self.pin;
        self.synced.proc1 = self.proc1;
        self.synced.proc2 = self.proc2;
        self.synced.eoi_exit = self.eoi_exit;
        self.configured = true;
        Ok(())
    }

    /// Sync the shadow into the VMCS before an entry. The first entry writes
    /// the whole of it; after that only the fields the policy changes between
    /// entries, because everything else -- CR0/3/4, the segments, GDTR/IDTR,
    /// RSP, RFLAGS -- the guest owns and updates in the VMCS itself, without
    /// an exit (no CR or segment interception), so a round-trip through the
    /// shadow every exit would be 60-odd `vmread`/`vmwrite` that, under a
    /// nested hypervisor where each one is an exit to L0, is most of the cost
    /// of running the guest at all. What is read back is what the policy
    /// reads (`read_guest_state`); the heavy fields are read on demand, for a
    /// report (`read_guest_heavy`), on the exits that end a guest.
    unsafe fn write_guest_state(&mut self) {
        use vmcs::*;
        let s = &self.save;
        let efer = s.efer & !super::svm::EFER_SVME;
        if self.full_sync {
            let seg = |sel_f: u32, base_f: u32, lim_f: u32, ar_f: u32, g: &Segment| unsafe {
                vmwrite(sel_f, g.selector as u64);
                vmwrite(base_f, g.base);
                vmwrite(lim_f, g.limit as u64);
                vmwrite(ar_f, ar_from_attrib(g.attrib) as u64);
            };
            let guest_cr0 = self.hw_cr0(s.cr0);
            let guest_cr4 = self.hw_cr4(s.cr4);
            let entry_ctls = self.entry_ctls_for(efer);
            unsafe {
                /* CR0/CR4 with the bits VMX forces -- with unrestricted
                 * guest, PE and PG are not -- and CR4 with MCE. From here the
                 * guest changes CR3 and the bits of CR0 and CR4 outside their
                 * masks in the VMCS itself; the masks' bits exit, and the
                 * policy writes them (`set_cr0`, `set_cr4`). */
                vmwrite(GUEST_CR0, guest_cr0);
                vmwrite(GUEST_CR3, s.cr3);
                vmwrite(GUEST_CR4, guest_cr4);
                /* What the guest reads of the host's bits: CR0 and CR4 as
                 * it wrote them -- MCE as it left it, VMXE clear. */
                vmwrite(CR0_READ_SHADOW, s.cr0);
                vmwrite(CR4_READ_SHADOW, s.cr4);
                vmwrite(VMENTRY_CTLS, entry_ctls as u64);

                seg(GUEST_CS_SEL, GUEST_CS_BASE, GUEST_CS_LIMIT, GUEST_CS_AR, &s.cs);
                seg(GUEST_SS_SEL, GUEST_SS_BASE, GUEST_SS_LIMIT, GUEST_SS_AR, &s.ss);
                seg(GUEST_DS_SEL, GUEST_DS_BASE, GUEST_DS_LIMIT, GUEST_DS_AR, &s.ds);
                seg(GUEST_ES_SEL, GUEST_ES_BASE, GUEST_ES_LIMIT, GUEST_ES_AR, &s.es);
                seg(GUEST_FS_SEL, GUEST_FS_BASE, GUEST_FS_LIMIT, GUEST_FS_AR, &s.fs);
                seg(GUEST_GS_SEL, GUEST_GS_BASE, GUEST_GS_LIMIT, GUEST_GS_AR, &s.gs);
                seg(GUEST_LDTR_SEL, GUEST_LDTR_BASE, GUEST_LDTR_LIMIT, GUEST_LDTR_AR, &s.ldtr);
                seg(GUEST_TR_SEL, GUEST_TR_BASE, GUEST_TR_LIMIT, GUEST_TR_AR, &s.tr);
                vmwrite(GUEST_GDTR_BASE, s.gdtr.base);
                vmwrite(GUEST_GDTR_LIMIT, s.gdtr.limit as u64);
                vmwrite(GUEST_IDTR_BASE, s.idtr.base);
                vmwrite(GUEST_IDTR_LIMIT, s.idtr.limit as u64);
                vmwrite(GUEST_RSP, s.rsp);
                vmwrite(GUEST_RFLAGS, s.rflags);
                vmwrite(GUEST_DR7, s.dr7);
                vmwrite(GUEST_ACTIVITY_STATE, 0);
                vmwrite(vmcs::GUEST_PENDING_DBG, 0);
            }
            unsafe {
                vmwrite(GUEST_RIP, s.rip);
                vmwrite(GUEST_FS_BASE, s.fs.base);
                vmwrite(GUEST_GS_BASE, s.gs.base);
                vmwrite(GUEST_IA32_EFER, efer);
                vmwrite(GUEST_IA32_PAT, s.g_pat);
                vmwrite(GUEST_SYSENTER_CS, s.sysenter_cs);
                vmwrite(GUEST_SYSENTER_ESP, s.sysenter_esp);
                vmwrite(GUEST_SYSENTER_EIP, s.sysenter_eip);
                vmwrite(GUEST_INTERRUPTIBILITY, self.interruptibility as u64);
            }
            self.synced = Synced {
                rip: s.rip,
                fs_base: s.fs.base,
                gs_base: s.gs.base,
                efer,
                pat: s.g_pat,
                sysenter_cs: s.sysenter_cs,
                sysenter_esp: s.sysenter_esp,
                sysenter_eip: s.sysenter_eip,
                interruptibility: self.interruptibility,
                pin: self.synced.pin,
                proc1: self.synced.proc1,
                proc2: self.synced.proc2,
                eoi_exit: self.synced.eoi_exit,
                entry_ctls,
            };
            self.cr4_shadow = s.cr4;
            self.cr4_hw = guest_cr4;
            self.full_sync = false;
            self.rsp_dirty = false;
            self.cr0_dirty = false;
            self.cr4_dirty = false;
            self.dr7_dirty = false;
            self.pending_bs = false;
            return;
        }
        if self.cr0_dirty {
            let guest_cr0 = self.hw_cr0(s.cr0);
            unsafe {
                vmwrite(GUEST_CR0, guest_cr0);
                vmwrite(CR0_READ_SHADOW, s.cr0);
            }
            self.cr0_dirty = false;
        }
        if self.cr4_dirty {
            let guest_cr4 = self.hw_cr4(s.cr4);
            unsafe {
                vmwrite(GUEST_CR4, guest_cr4);
                vmwrite(CR4_READ_SHADOW, s.cr4);
            }
            self.cr4_shadow = s.cr4;
            self.cr4_hw = guest_cr4;
            self.cr4_dirty = false;
        }
        if self.dr7_dirty {
            unsafe { vmwrite(GUEST_DR7, s.dr7) };
            self.dr7_dirty = false;
        }
        if self.pending_bs {
            /* Over whatever else the exit left pending: a breakpoint's bits
             * stay. */
            unsafe { vmwrite(GUEST_PENDING_DBG, vmread(GUEST_PENDING_DBG) | debug::BS) };
            self.pending_bs = false;
        }
        let entry_ctls = self.entry_ctls_for(efer);
        if self.rsp_dirty {
            unsafe { vmwrite(GUEST_RSP, s.rsp) };
            self.rsp_dirty = false;
        }
        /* Only what the policy changed since the VMCS last had it: RIP
         * stepped past an instruction, an interruptibility cleared with it,
         * a system MSR after an intercepted `wrmsr`. A `vmwrite` is tens of
         * cycles on the silicon, and under a nested hypervisor an exit to it
         * for every field it does not shadow -- which these mostly are. */
        let y = &mut self.synced;
        unsafe {
            if s.rip != y.rip {
                vmwrite(GUEST_RIP, s.rip);
                y.rip = s.rip;
            }
            if s.fs.base != y.fs_base {
                vmwrite(GUEST_FS_BASE, s.fs.base);
                y.fs_base = s.fs.base;
            }
            if s.gs.base != y.gs_base {
                vmwrite(GUEST_GS_BASE, s.gs.base);
                y.gs_base = s.gs.base;
            }
            if efer != y.efer {
                vmwrite(GUEST_IA32_EFER, efer);
                y.efer = efer;
            }
            if entry_ctls != y.entry_ctls {
                vmwrite(VMENTRY_CTLS, entry_ctls as u64);
                y.entry_ctls = entry_ctls;
            }
            if s.g_pat != y.pat {
                vmwrite(GUEST_IA32_PAT, s.g_pat);
                y.pat = s.g_pat;
            }
            if s.sysenter_cs != y.sysenter_cs {
                vmwrite(GUEST_SYSENTER_CS, s.sysenter_cs);
                y.sysenter_cs = s.sysenter_cs;
            }
            if s.sysenter_esp != y.sysenter_esp {
                vmwrite(GUEST_SYSENTER_ESP, s.sysenter_esp);
                y.sysenter_esp = s.sysenter_esp;
            }
            if s.sysenter_eip != y.sysenter_eip {
                vmwrite(GUEST_SYSENTER_EIP, s.sysenter_eip);
                y.sysenter_eip = s.sysenter_eip;
            }
            if self.interruptibility != y.interruptibility {
                vmwrite(GUEST_INTERRUPTIBILITY, self.interruptibility as u64);
                y.interruptibility = self.interruptibility;
            }
        }
    }

    /// Fill the VM-exit MSR-load list with the host's `SWAP_MSRS`, off the
    /// CPU this runs on, so the CPU puts them back on exit -- and the index
    /// halves of the other two lists, which never change. Once per CPU the
    /// guest runs on, with the host state ([`HostRegs`]): the values are that
    /// CPU's own, set at boot and never changed, and reading them on every
    /// entry was five `rdmsr`s -- a tenth of an exit on real silicon, and
    /// under a nested hypervisor, which intercepts the syscall MSRs, four
    /// exits to it.
    fn fill_host_msrs(&mut self) {
        for (i, &msr) in SWAP_MSRS.iter().enumerate() {
            let host = unsafe { cpu::rdmsr(msr) };
            let e = MSR_ENTRY_BASE + i * 16;
            let x = MSR_EXIT_BASE + i * 16;
            self.msr_area.store::<u32>(e, msr);
            self.msr_area.store::<u32>(e + 4, 0);
            self.msr_area.store::<u32>(x, msr);
            self.msr_area.store::<u32>(x + 4, 0);
            self.msr_area.store::<u64>(x + 8, host);
        }
        /* The exit-store list, one entry: the CPU writes the guest's live
         * KERNEL_GS_BASE (which its `swapgs` may have changed) at +8. */
        self.msr_area.store::<u32>(MSR_STORE_BASE, MSR_KERNEL_GS_BASE);
        self.msr_area.store::<u32>(MSR_STORE_BASE + 4, 0);
    }

    /// Fill the VM-entry MSR-load list's values with the guest's `SWAP_MSRS`
    /// from the shadow, where the policy keeps them: five stores an entry.
    fn fill_guest_msrs(&mut self) {
        let guest = [
            self.save.star, self.save.lstar, self.save.cstar,
            self.save.sfmask, self.save.kernel_gs_base,
        ];
        for (i, &gval) in guest.iter().enumerate() {
            self.msr_area.store::<u64>(MSR_ENTRY_BASE + i * 16 + 8, gval);
        }
    }

    /// The guest's KERNEL_GS_BASE the exit stored, back into the shadow, so
    /// the next entry loads what the guest's `swapgs` left, not a stale
    /// `wrmsr` value. Read before the policy handles a `wrmsr` of it, which
    /// then overrides this with the written value.
    fn read_stored_kernel_gs_base(&mut self) {
        if let Some(v) = self.msr_area.load::<u64>(MSR_STORE_BASE + 8) {
            self.save.kernel_gs_base = v;
        }
    }

    /// Read back the little the policy reads every exit: where the guest
    /// stopped, its stack and flags (the run loop asks whether it can take an
    /// interrupt), and the interruptibility the CPU set. The heavy state --
    /// the control registers and segments -- is left in the VMCS and read
    /// only when a guest is being stopped and dumped ([`read_guest_heavy`]).
    unsafe fn read_guest_state(&mut self) {
        use vmcs::*;
        unsafe {
            self.save.rip = vmread(GUEST_RIP);
            self.save.rflags = vmread(GUEST_RFLAGS);
            self.interruptibility = vmread(GUEST_INTERRUPTIBILITY) as u32;
            self.synced.rip = self.save.rip;
            self.synced.interruptibility = self.interruptibility;
            /* FS and GS base the guest changes both ways -- `wrfsbase` and
             * `swapgs` un-intercepted, `wrmsr` intercepted, which the MSR
             * policy reads and writes in the shadow. So they are read on the
             * exits where the policy may look, `rdmsr` and `wrmsr`, and the
             * value it then leaves is compared with what was read: written
             * back if the policy changed it, left to the guest otherwise.
             * Linux keeps its per-CPU data at the GS base, as this kernel
             * does; a stale one written back is the guest lost -- which is
             * why they are never written from a copy older than this exit. */
            let basic = self.exit_reason & reason::BASIC_MASK;
            if basic == reason::RDMSR || basic == reason::WRMSR {
                self.save.fs.base = vmread(GUEST_FS_BASE);
                self.save.gs.base = vmread(GUEST_GS_BASE);
                self.synced.fs_base = self.save.fs.base;
                self.synced.gs_base = self.save.gs.base;
            }
        }
    }

    /// The control registers and segments, into the shadow, for a report: read
    /// while the VMCS is still current, on the exits that end a guest, or
    /// after one for a report ([`read_whole_state`]). After this the shadow
    /// holds the whole guest state, as it did every exit before the sync was
    /// made lazy.
    unsafe fn read_guest_heavy(&mut self) {
        use vmcs::*;
        let rd = |sel_f: u32, base_f: u32, lim_f: u32, ar_f: u32, g: &mut Segment| unsafe {
            g.selector = vmread(sel_f) as u16;
            g.base = vmread(base_f);
            g.limit = vmread(lim_f) as u32;
            g.attrib = attrib_from_ar(vmread(ar_f) as u32);
        };
        let mask = self.cr0_mask;
        self.cr4_hw = unsafe { vmread(GUEST_CR4) };
        let cr4_view = self.cr4_view(self.cr4_hw);
        let s = &mut self.save;
        unsafe {
            s.rsp = vmread(GUEST_RSP);
            /* CR0 and CR4 as the guest reads them: the host's bits from the
             * read shadows. */
            s.cr0 = (vmread(GUEST_CR0) & !mask) | (vmread(CR0_READ_SHADOW) & mask);
            s.cr3 = vmread(GUEST_CR3);
            s.cr4 = cr4_view;
            rd(GUEST_CS_SEL, GUEST_CS_BASE, GUEST_CS_LIMIT, GUEST_CS_AR, &mut s.cs);
            rd(GUEST_SS_SEL, GUEST_SS_BASE, GUEST_SS_LIMIT, GUEST_SS_AR, &mut s.ss);
            rd(GUEST_DS_SEL, GUEST_DS_BASE, GUEST_DS_LIMIT, GUEST_DS_AR, &mut s.ds);
            rd(GUEST_ES_SEL, GUEST_ES_BASE, GUEST_ES_LIMIT, GUEST_ES_AR, &mut s.es);
            rd(GUEST_FS_SEL, GUEST_FS_BASE, GUEST_FS_LIMIT, GUEST_FS_AR, &mut s.fs);
            rd(GUEST_GS_SEL, GUEST_GS_BASE, GUEST_GS_LIMIT, GUEST_GS_AR, &mut s.gs);
            rd(GUEST_LDTR_SEL, GUEST_LDTR_BASE, GUEST_LDTR_LIMIT, GUEST_LDTR_AR, &mut s.ldtr);
            rd(GUEST_TR_SEL, GUEST_TR_BASE, GUEST_TR_LIMIT, GUEST_TR_AR, &mut s.tr);
            s.gdtr.base = vmread(GUEST_GDTR_BASE);
            s.gdtr.limit = vmread(GUEST_GDTR_LIMIT) as u32;
            s.idtr.base = vmread(GUEST_IDTR_BASE);
            s.idtr.limit = vmread(GUEST_IDTR_LIMIT) as u32;
            s.efer = vmread(GUEST_IA32_EFER);
            s.cpl = ((vmread(GUEST_SS_AR) >> 5) & 0x3) as u8;
        }
        self.synced.fs_base = s.fs.base;
        self.synced.gs_base = s.gs.base;
        self.whole_state = true;
    }

    /// What performing the instruction an EPT violation faulted at takes,
    /// into the shadow: CR0 as the guest reads it, CR3 and CR4, to walk its
    /// paging to the instruction; CS's rights, for the mode it is decoded
    /// in, and CS's base where the CPU adds it -- outside 64-bit code; RSP,
    /// the one register an instruction may name that the guest keeps in the
    /// VMCS; and the CPL, SS's DPL. EFER the shadow has already: the guest
    /// changes it only by a `wrmsr` the policy answers, and LMA only with a
    /// CR0.PG that exits.
    ///
    /// Not the whole heavy state: that is forty-odd fields, and under a
    /// nested KVM most of them -- the selectors, the limits, most segments'
    /// rights, GDTR, IDTR, EFER -- are not in the shadow VMCS it keeps, so
    /// each is an exit to it; the control registers, RSP and CS's and SS's
    /// rights are. Read whole, an MMIO access cost five CPUID exits on
    /// nested VT-x. A fault that ends the guest has the rest read for its
    /// report ([`read_whole_state`]).
    unsafe fn read_guest_fault(&mut self) {
        use vmcs::*;
        let mask = self.cr0_mask;
        self.cr4_hw = unsafe { vmread(GUEST_CR4) };
        let cr4_view = self.cr4_view(self.cr4_hw);
        let s = &mut self.save;
        unsafe {
            s.rsp = vmread(GUEST_RSP);
            s.cr0 = (vmread(GUEST_CR0) & !mask) | (vmread(CR0_READ_SHADOW) & mask);
            s.cr3 = vmread(GUEST_CR3);
            s.cr4 = cr4_view;
            s.cs.attrib = attrib_from_ar(vmread(GUEST_CS_AR) as u32);
            s.cpl = ((vmread(GUEST_SS_AR) >> 5) & 0x3) as u8;
            if s.efer & EFER_LMA == 0 || s.cs.attrib & super::svm::vmcb::attrib::L == 0 {
                s.cs.base = vmread(GUEST_CS_BASE);
            }
        }
    }

    /// Read the rest of the guest's state into the shadow, for a report of
    /// the exit it last made -- the control registers, segments and
    /// descriptor tables an exit that does not end a guest leaves in the
    /// VMCS -- if its VMCS is still current on this CPU: it is, unless
    /// another guest's was loaded over it since the exit, or the task has
    /// moved. Nothing has entered the guest since, so the VMCS holds that
    /// exit's state still. False if it could not be read, the shadow's
    /// older values left as they were, for the report to say so rather
    /// than show them as the guest's.
    pub fn read_whole_state(&mut self) -> bool {
        if self.whole_state {
            return true;
        }
        let flags = kcore::cpu::irq_save();
        /* With interrupts off, what is current here cannot change under the
         * check: the VMPTRLD in `run`, a VMCLEAR (`evict_here`, an IPI) and
         * VMXOFF (`disable`, an IPI) all run on this CPU with them off. */
        let cpu = kcore::cpu::id() as usize;
        let phys = self.vmcs.phys();
        let current = enabled()
            && CURRENT_VMCS.get(cpu).is_some_and(|c| c.load(Ordering::Acquire) == phys);
        if current {
            unsafe { self.read_guest_heavy() };
        }
        unsafe { kcore::cpu::irq_restore(flags) };
        current
    }

    /// Whether the shadow holds the whole guest state ([`read_whole_state`]),
    /// or only what its last exit's handling reads.
    pub fn whole_state(&self) -> bool {
        self.whole_state
    }

    /// Whether an exit ends the guest -- one the policy will dump, so the
    /// heavy state is worth reading. The common exits (I/O, CPUID, MSR, HLT,
    /// the host's interrupt, an interrupt window) are not among them, nor an
    /// EPT violation, which is mostly an MMIO access performed for the guest
    /// and reads what that takes ([`read_guest_fault`]).
    fn exit_is_stopping(&self) -> bool {
        use vmcs::reason as r;
        if self.exit_reason & vmcs::reason::ENTRY_FAILURE != 0 {
            return true;
        }
        let basic = self.exit_reason & vmcs::reason::BASIC_MASK;
        if basic == r::HLT {
            /* An idle guest halts with interrupts on, waiting for the timer;
             * one that halts with them off has stopped for good, and is
             * dumped -- so read the heavy state only for the latter. */
            const IF: u64 = 1 << 9;
            return self.save.rflags & IF == 0;
        }
        !matches!(
            basic,
            r::IO_INSTRUCTION | r::CPUID | r::RDMSR | r::WRMSR
                | r::EXTERNAL_INTERRUPT | r::INIT | r::SIPI | r::NMI_WINDOW
                | r::INTERRUPT_WINDOW | r::VMCALL | r::PAUSE | r::RDTSC | r::RDPMC
                | r::CR_ACCESS | r::DR_ACCESS | r::EPT_VIOLATION
        )
    }

    /// Read the exit information the decoder needs, after an exit.
    unsafe fn read_exit(&mut self) {
        use vmcs::reason as r;
        use vmcs::*;
        self.exit_reason = unsafe { vmread(EXIT_REASON) } as u32;
        let basic = self.exit_reason & r::BASIC_MASK;
        /* What the decoder reads of an exit depends on the exit, and the
         * frequent ones need little: the host's interrupt nothing, an
         * instruction its length, port I/O its qualification too. An exit
         * during the delivery of an event -- which none of those is, being
         * the CPU's answer to an instruction or an interrupt at a boundary
         * -- and everything rarer or unforeseen reads the whole of it, the
         * interruption and vectoring words and the guest-physical address
         * included, for the decoder or for the report a stop prints. */
        let (len, qual, all) = if self.exit_reason & r::ENTRY_FAILURE != 0 {
            (true, true, true)
        } else {
            match basic {
                r::EXTERNAL_INTERRUPT | r::INTERRUPT_WINDOW | r::NMI_WINDOW => (false, false, false),
                r::HLT | r::PAUSE | r::CPUID | r::RDMSR | r::WRMSR | r::VMCALL | r::WBINVD | r::RDTSC
                | r::RDPMC | r::XSETBV | r::MONITOR | r::MWAIT | r::RDTSCP | r::INVD => (true, false, false),
                r::IO_INSTRUCTION | r::CR_ACCESS | r::DR_ACCESS => (true, true, false),
                /* Trap-like, past the WRMSR already: its vector alone. */
                r::EOI_INDUCED => (false, true, false),
                _ => (true, true, true),
            }
        };
        if (basic == r::CR_ACCESS || basic == r::DR_ACCESS) && self.exit_reason & r::ENTRY_FAILURE == 0 {
            /* What the policy needs to answer a `mov` to or from a control
             * or a debug register: CR0 as the guest sees it, and CR4 -- the
             * mask's bits the read shadow's, which is what a write of CR4
             * changes the rest of over, and what DR4 and DR5 are by -- and
             * CS's L bit, for the checks a write of CR0 makes and a `mov`'s
             * width; RSP, the one register a `mov` may name that the guest
             * keeps in the VMCS; and the CPL, which a `mov` of a debug
             * register exits before the CPU checks. */
            let mask = self.cr0_mask;
            unsafe {
                self.exit_cr0 = (vmread(GUEST_CR0) & !mask) | (vmread(CR0_READ_SHADOW) & mask);
                self.cr4_hw = vmread(GUEST_CR4);
                self.exit_cs_ar = vmread(GUEST_CS_AR) as u32;
                self.save.rsp = vmread(GUEST_RSP);
                self.save.cpl = ((vmread(GUEST_SS_AR) >> 5) & 0x3) as u8;
                /* And CR3, which a write of CR4 is checked against -- PCIDE
                 * turned on wants its PCID 0 -- as CR0 is, for CET: both the
                 * guest changes without an exit, so the shadow's are old. */
                self.save.cr3 = vmread(GUEST_CR3);
            }
            self.exit_cr4 = self.cr4_view(self.cr4_hw);
            self.save.cr0 = self.exit_cr0;
            self.save.cr4 = self.exit_cr4;
        }
        if basic == r::XSETBV && self.exit_reason & r::ENTRY_FAILURE == 0 {
            /* The CPL, which an XSETBV is checked against: the CPU faults
             * one above CPL 0 before it would exit, and a hypervisor under
             * this one may not. */
            self.save.cpl = unsafe { ((vmread(GUEST_SS_AR) >> 5) & 0x3) as u8 };
        }
        /* An external interrupt the exit acknowledged: the host's, delivered
         * to nothing yet -- `run` hands it over once the CPU's entries are
         * let go of. */
        self.acked = None;
        if basic == r::EXTERNAL_INTERRUPT && self.exit_reason & r::ENTRY_FAILURE == 0
            && self.exit_ctls & EXIT_ACK_INTR_ON_EXIT != 0
        {
            let info = unsafe { vmread(VMEXIT_INTR_INFO) } as u32;
            if info & intr::VALID != 0 {
                self.acked = Some((info & intr::VECTOR_MASK) as u8);
            }
        }
        unsafe {
            self.exit_instr_len = if len { vmread(VMEXIT_INSTRUCTION_LEN) as u32 } else { 0 };
            self.exit_qual = if qual { vmread(EXIT_QUALIFICATION) } else { 0 };
            if all {
                self.exit_intr_info = vmread(VMEXIT_INTR_INFO) as u32;
                self.exit_intr_errcode = vmread(VMEXIT_INTR_ERRCODE) as u32;
                self.idt_vectoring_info = vmread(IDT_VECTORING_INFO) as u32;
                self.idt_vectoring_errcode = vmread(IDT_VECTORING_ERRCODE) as u32;
                self.guest_phys = vmread(GUEST_PHYSICAL_ADDRESS);
            } else {
                self.exit_intr_info = 0;
                self.exit_intr_errcode = 0;
                self.idt_vectoring_info = 0;
                self.idt_vectoring_errcode = 0;
                self.guest_phys = 0;
            }
        }
    }

    /// Enter the guest on the CPU this runs on and come back at its next
    /// exit. `nested.root` is this guest's EPT pointer, already formed with
    /// its memory type and walk length; the exit is read into this `Guest`,
    /// for [`super`]'s decoder to turn into an exit reason.
    ///
    /// # Safety
    /// `nested.root` names an EPT that maps nothing but memory given to this
    /// guest, alive until this returns; `host_areas[cpu]` is 0 or the VMXON
    /// region this CPU was given, and VMX is on for this CPU exactly when
    /// that is non-zero. As [`super::svm::Guest::run`] for the kick.
    pub unsafe fn run(
        &mut self,
        nested: super::svm::Nested,
        host_areas: &[AtomicU64],
        kick: Option<&Kick>,
    ) -> core::result::Result<u32, NotRun> {
        let vmcs_phys = self.vmcs.phys();

        loop {
            /* Load phase, interrupts on: if our VMCS is still current on
             * another CPU (the guest's task has migrated since the last
             * entry), VMCLEAR it there first. That is an IPI that waits for
             * the other CPU to answer -- safe only from task context, never
             * with interrupts off -- after which the VMCS is current nowhere,
             * so the VMPTRLD below cannot make it current on two CPUs at once.
             * `loaded_cpu` may go stale the instant we read it (the task can
             * move again); the recheck with interrupts off below closes
             * that. */
            let here = kcore::cpu::id();
            let loaded = self.loaded_cpu.load(Ordering::Acquire);
            if loaded >= 0 && loaded as u32 != here {
                let evict = Evict {
                    phys: vmcs_phys,
                    loaded_cpu: &self.loaded_cpu,
                    launched: &self.launched,
                };
                kcore::cpu::run_on_with(loaded as u32, &evict, evict_here);
                /* Now current nowhere: either `evict_here` VMCLEARed it, or the
                 * target CPU has exited (its VMX state gone with its VMXOFF) and
                 * the IPI was dropped -- reset here regardless, so this loop
                 * always makes progress and never spins on a departed CPU. Only
                 * this guest's own task runs it, so the store races nothing. */
                now_unloaded(&self.loaded_cpu, &self.launched);
            }

            /* Pinned to this CPU with interrupts off, so the CPU the checks
             * see and the VMCS is made current on is the CPU the entry runs
             * on: `with` turns them off before it asks which CPU this is --
             * the other order could read one CPU and run on another -- and
             * lends the entry what the CPU keeps of its guests
             * (`super::Entries`). */
            let ran = super::ENTRIES.with(|entries, cpu| {
                let cpu = cpu as u32;
                /* The task may have moved between the load phase and here.
                 * If the VMCS is current on a CPU that is not this one, go
                 * back and evict it with interrupts on, rather than VMPTRLD
                 * a second copy. */
                let loaded = self.loaded_cpu.load(Ordering::Acquire);
                if loaded >= 0 && loaded as u32 != cpu {
                    return None;
                }
                Some(unsafe { self.enter(entries, cpu, nested, host_areas, kick) })
            });
            /* The host's interrupt an exit acknowledged, handed to its handler
             * now that the CPU's entries are let go of: its handler may
             * switch tasks. */
            if let Some(vector) = self.acked.take() {
                /* Acknowledged at this CPU's last exit and delivered no
                 * other way; `with` has returned, no borrow held. */
                unsafe { deliver_host_interrupt(vector) };
            }
            match ran {
                Some(Some(ran)) => return ran,
                Some(None) => continue,
                /* `with` is None only on a CPU past the table, which no CPU
                 * VMX is on for is. */
                None => return Err(NotRun::Off { cpu: kcore::cpu::id() }),
            }
        }
    }

    /// One entry of [`Guest::run`]'s, and its exit read back: on CPU `cpu`,
    /// whose `entries` they are.
    ///
    /// # Safety
    /// As [`Guest::run`]; and this runs on `cpu`, with interrupts off until
    /// it returns, and the VMCS is current on no other CPU.
    unsafe fn enter(
        &mut self,
        entries: &mut super::Entries,
        cpu: u32,
        nested: super::svm::Nested,
        host_areas: &[AtomicU64],
        kick: Option<&Kick>,
    ) -> core::result::Result<u32, NotRun> {
        let vmcs_phys = self.vmcs.phys();
        let timed = self.profile.is_some();
        let stamp = |on: bool| if on { cpu::rdtsc() } else { 0 };
        let t0 = stamp(timed);
        let expected = host_areas.get(cpu as usize).map_or(0, |a| a.load(Ordering::Acquire));
        if expected == 0 || !enabled() {
            return Err(NotRun::Off { cpu });
        }
        if cpu::read_cr4() & CR4_LA57 != 0 {
            return Err(NotRun::FiveLevelPaging { cpu });
        }
        if !self.fp.fits(cpu) {
            return Err(NotRun::Xstate { cpu, xcr0: self.fp.xcr0() });
        }
        if let Some(k) = kick {
            if !k.entering(cpu) {
                return Err(NotRun::Kicked { cpu });
            }
        }

        /* Once, on the first entry: put the fresh VMCS into the clear
         * launch state its first VMLAUNCH needs (its memory is otherwise
         * undefined). Every later VMCLEAR is a migration/drop/VMfail one. */
        if !self.cleared {
            unsafe { vmcs::vmclear(vmcs_phys) };
            self.cleared = true;
        }
        /* Make our VMCS current on this CPU -- unless it is, from an
         * earlier entry that left it current here (no per-exit VMCLEAR):
         * VMPTRLD of the VMCS that is current already is a serializing
         * instruction for nothing, and an exit to the outer hypervisor
         * under a nested one. The entry then VMRESUMEs it if it is in the
         * launched state, VMLAUNCHes it if not. It stays current after
         * the exit so the next entry here can resume; a migration
         * VMCLEARs it off this CPU (above) before another can make it
         * current. */
        if CURRENT_VMCS.get(cpu as usize).map_or(0, |c| c.load(Ordering::Acquire)) != vmcs_phys {
            if !unsafe { vmcs::vmptrld(vmcs_phys) } {
                if let Some(k) = kick { k.left(); }
                return Err(NotRun::Off { cpu });
            }
            if let Some(c) = CURRENT_VMCS.get(cpu as usize) {
                c.store(vmcs_phys, Ordering::Release);
            }
        }
        if self.loaded_cpu.load(Ordering::Acquire) != cpu as i32 {
            now_loaded_on(&self.loaded_cpu, cpu);
        }

        if !self.configured {
            if let Err(refused) = unsafe { self.configure(nested.root, cpu) } {
                if let Some(k) = kick { k.left(); }
                return Err(refused);
            }
        }
        if self.host_cpu != cpu as i32 {
            /* A first entry here drops the VPID's translations below, and
             * with them whatever a reset asked to be dropped. */
            self.flush_vpid = false;
            unsafe { HostRegs::capture().write() };
            self.fill_host_msrs();
            /* This guest's first entry on this CPU: whatever the CPU has
             * cached through an EPT at this address -- an earlier guest's
             * translations, the page having been its EPT's before it was
             * this one's -- goes now, before an access could hit it. VM
             * entries and exits drop nothing tagged with an EPT (with
             * VPID off they drop the linear and combined mappings, which
             * is what "the TLB is flushed on every transition" means).
             * Once per CPU a guest runs on, so its cost is nothing. */
            let (kind, eptp) = if self.invept_single {
                (vmcs::INVEPT_SINGLE_CONTEXT, nested.root)
            } else {
                (vmcs::INVEPT_ALL_CONTEXT, 0)
            };
            if !unsafe { vmcs::invept(kind, eptp) } {
                if let Some(k) = kick { k.left(); }
                return Err(NotRun::Flush { cpu });
            }
            /* And nothing tagged with this guest's VPID: an earlier guest
             * given the same number may have run here. */
            if self.vpid != 0 {
                let (kind, vpid) = if self.invvpid_single {
                    (vmcs::INVVPID_SINGLE_CONTEXT, self.vpid)
                } else {
                    (vmcs::INVVPID_ALL_CONTEXT, 0)
                };
                if !unsafe { vmcs::invvpid(kind, vpid) } {
                    if let Some(k) = kick { k.left(); }
                    return Err(NotRun::Flush { cpu });
                }
            }
            self.host_cpu = cpu as i32;
        }
        /* The policy wrote CR0, or reset the guest: what the CPU cached
         * under its VPID goes, as a `mov` to CR0 or an INIT would have
         * taken it. Without VPIDs every entry and exit flushes anyway. */
        if self.flush_vpid {
            if self.vpid != 0 {
                let (kind, vpid) = if self.invvpid_single {
                    (vmcs::INVVPID_SINGLE_CONTEXT, self.vpid)
                } else {
                    (vmcs::INVVPID_ALL_CONTEXT, 0)
                };
                if !unsafe { vmcs::invvpid(kind, vpid) } {
                    if let Some(k) = kick { k.left(); }
                    return Err(NotRun::Flush { cpu });
                }
            }
            self.flush_vpid = false;
        }
        /* The breakpoint addresses are the guest's from here on, armed
         * by its own DR7 once the entry loads it: the host's enables
         * none, nothing in this kernel writing it and every VM exit
         * leaving it with all four off (`svm::LoadedDebugAddrs`). And
         * another guest than the last on this CPU: the predictors it
         * trained are not to steer this one (`super::spec`). */
        unsafe { entries.debug_addrs.load(&self.debug_addrs) };
        entries.spec.entering(nested.id);
        let speculation = entries.spec.vmx_entry();
        let t1 = stamp(timed);
        unsafe { self.write_guest_state() };
        self.fill_guest_msrs();
        unsafe {
            /* The interrupt-window control is dynamic: on only while the
             * policy is waiting to inject an IRQ the guest cannot take
             * yet. Written when it changes. */
            let nmi_window = self.nmi_window && self.pin & vmcs::PIN_VIRTUAL_NMIS != 0;
            let proc1 = self.proc1
                | if self.irq_window { vmcs::PROC_INTR_WINDOW_EXITING } else { 0 }
                | if nmi_window { vmcs::PROC_NMI_WINDOW_EXITING } else { 0 };
            if proc1 != self.synced.proc1 {
                vmcs::vmwrite(vmcs::PROC_BASED_CTLS, proc1 as u64);
                self.synced.proc1 = proc1;
            }
            /* x2APIC virtualization and virtual-interrupt delivery while the
             * guest's APIC is in x2APIC mode and on (`set_apicv`), and then
             * where its interrupts stand -- RVI and SVI, from the page the
             * run loop filled -- and which of their EOIs still exit. */
            if self.vapic.is_some() {
                /* Posted interrupts taken in the guest while, and only
                 * while, virtual-interrupt delivery is on -- which they
                 * need. */
                let pin = self.pin
                    | if self.apicv_on && self.posted.is_some() { vmcs::PIN_POSTED_INTERRUPTS } else { 0 };
                if pin != self.synced.pin {
                    vmcs::vmwrite(vmcs::PIN_BASED_CTLS, pin as u64);
                    self.synced.pin = pin;
                }
                let proc2 = self.proc2 | if self.apicv_on { PROC2_APICV } else { 0 };
                if proc2 != self.synced.proc2 {
                    vmcs::vmwrite(vmcs::PROC_BASED_CTLS2, proc2 as u64);
                    self.synced.proc2 = proc2;
                }
                if self.apicv_on {
                    vmcs::vmwrite(vmcs::GUEST_INTR_STATUS, u64::from(self.intr_status()));
                    if self.eoi_exit != self.synced.eoi_exit {
                        for (field, word) in EOI_EXIT_FIELDS.iter().zip(self.eoi_exit) {
                            vmcs::vmwrite(*field, word);
                        }
                        self.synced.eoi_exit = self.eoi_exit;
                    }
                }
            }
            /* The event to inject, when there is one: the CPU clears the
             * field's valid bit as it takes the event, so with none to
             * inject the field is already what it should be. No
             * instruction length with it: the CPU reads that only for a
             * software interrupt or exception, and nothing here injects
             * one -- an external interrupt, #UD, #GP, or an interrupted
             * hardware event given back, none of which has a length. */
            if self.inject as u32 & vmcs::intr::VALID != 0 {
                vmcs::vmwrite(vmcs::VMENTRY_INTR_INFO, self.inject);
                if self.inject as u32 & vmcs::intr::DELIVER_ERRCODE != 0 {
                    vmcs::vmwrite(vmcs::VMENTRY_EXCEPTION_ERRCODE, self.inject_errcode as u64);
                }
            }
        }

        let t2 = stamp(timed);

        /* The x87, SSE and AVX registers and XCR0 are the guest's from
         * here to the save after the exit, exactly as under AMD-V:
         * `vmlaunch` switches none of them, and CR4.OSFXSR and OSXSAVE
         * were set for the CPU when VMX was turned on for it
         * (`super::fp`) -- the CR4 the exit loads back (`HostRegs`) -- and
         * the registers are loaded only when the CPU's are not this
         * guest's already from its last exit here. `fits` held above,
         * with interrupts off since. CR2 is the guest's likewise -- VMX
         * keeps no guest CR2 -- so save the host's and restore it after. */
        unsafe { self.fp.load(cpu) };
        let host_cr2 = cpu::read_cr2();
        unsafe { cpu::write_cr2(self.save.cr2) };

        /* GPRs into the block the stub moves, RAX included. */
        let mut gpr = Gpr {
            rax: self.save.rax,
            rbx: self.regs.rbx, rcx: self.regs.rcx, rdx: self.regs.rdx,
            rsi: self.regs.rsi, rdi: self.regs.rdi, rbp: self.regs.rbp,
            r8: self.regs.r8, r9: self.regs.r9, r10: self.regs.r10, r11: self.regs.r11,
            r12: self.regs.r12, r13: self.regs.r13, r14: self.regs.r14, r15: self.regs.r15,
        };
        let resume = self.launched.load(Ordering::Acquire);
        /* What the world switch is to do for this CPU's speculation, at
         * the last moment: its L1 data cache flushed now, where L1TF can
         * read it; VERW in the stub, just before the entry; and legacy
         * IBRS written again just after the exit (`super::spec`). */
        let spec_ctrl = speculation.spec_ctrl();
        let flags = if resume { RUN_RESUME } else { 0 }
            | if speculation.verw() { RUN_VERW } else { 0 }
            | if spec_ctrl.is_some() { RUN_SPEC_CTRL } else { 0 };
        let t3 = stamp(timed);
        speculation.flush_l1d();
        let failed = unsafe { vmx_launch_stub(&mut gpr, flags, spec_ctrl.unwrap_or(0)) };
        let t4 = stamp(timed);

        self.save.cr2 = cpu::read_cr2();
        unsafe { cpu::write_cr2(host_cr2) };
        /* XCR0 still the guest's: its XSETBV always exits. */
        unsafe { self.fp.save(cpu) };
        if let Some(k) = kick { k.left(); }
        let t5 = stamp(timed);

        /* The event, if any, was delivered on entry: not again. */
        self.inject = 0;

        self.save.rax = gpr.rax;
        self.regs = GuestRegs {
            rbx: gpr.rbx, rcx: gpr.rcx, rdx: gpr.rdx, rsi: gpr.rsi, rdi: gpr.rdi, rbp: gpr.rbp,
            r8: gpr.r8, r9: gpr.r9, r10: gpr.r10, r11: gpr.r11,
            r12: gpr.r12, r13: gpr.r13, r14: gpr.r14, r15: gpr.r15,
        };

        if failed != 0 {
            /* VMLAUNCH/VMRESUME did not start entry (VMfail): the guest
             * state is as it was, and the error says why. VMCLEAR the VMCS
             * -- the guest stops on this, and its launch state is now
             * unknown -- so it is left clear, not current, not launched. */
            self.vm_instruction_error = unsafe { vmcs::vmread(vmcs::VM_INSTRUCTION_ERROR) } as u32;
            self.entry_failed = true;
            self.exit_reason = 0;
            unsafe { vmcs::vmclear(vmcs_phys) };
            cleared_on(cpu as usize, vmcs_phys);
            now_unloaded(&self.loaded_cpu, &self.launched);
        } else {
            self.entry_failed = false;
            /* The VMCS is launched now: the next entry here resumes it. */
            self.launched.store(true, Ordering::Release);
            unsafe { self.read_exit() };
            unsafe { self.read_guest_state() };
            self.read_stored_kernel_gs_base();
            /* The control registers and segments only when the exit is
             * one that ends and dumps the guest -- the VMCS is current
             * here (it stays current after the exit); an EPT violation
             * what performing its instruction takes. */
            self.whole_state = false;
            if self.exit_is_stopping() {
                unsafe { self.read_guest_heavy() };
            } else if self.exit_reason & vmcs::reason::BASIC_MASK == vmcs::reason::EPT_VIOLATION {
                unsafe { self.read_guest_fault() };
            }
            /* A machine check taken while the guest ran is the host's,
             * and the CPU did not deliver it: raise it, as the AMD side
             * does, to the handler that treats one as fatal. An NMI the
             * same, and for a reason AMD-V has not: there the NMI stays
             * pending until STGI and the host takes it then; here the
             * exit *is* its delivery -- the NMI is gone, and NMIs stay
             * blocked until an IRET. So it goes to the host's handler by
             * hand, through vector 2's gate, whose IRET is what unblocks
             * the next one: a panic on another CPU collecting this one's
             * stack, or the profiler's counter overflow, is answered
             * rather than lost -- which is what KVM does too. */
            if self.exit_reason & vmcs::reason::BASIC_MASK == vmcs::reason::EXCEPTION_NMI
                && self.exit_intr_info & vmcs::intr::VALID != 0
            {
                if self.exit_intr_info & vmcs::intr::TYPE_MASK == vmcs::intr::TYPE_NMI {
                    unsafe { core::arch::asm!("int 2") };
                } else if self.exit_intr_info & vmcs::intr::VECTOR_MASK == VECTOR_MC {
                    unsafe { core::arch::asm!("int 0x12") };
                }
            }
        }
        if let Some(p) = self.profile.as_mut() {
            let t6 = stamp(timed);
            p.entries += 1;
            p.checks += t1.saturating_sub(t0);
            p.sync_in += t2.saturating_sub(t1);
            p.switch_in += t3.saturating_sub(t2);
            p.world += t4.saturating_sub(t3);
            p.switch_out += t5.saturating_sub(t4);
            p.sync_out += t6.saturating_sub(t5);
        }
        /* No per-exit VMCLEAR: the VMCS stays current on this CPU, ready
         * for the next entry to VMRESUME. It is evicted only on a
         * migration (the load phase above) or when the guest is dropped. */
        Ok(cpu)
    }
}

/// What another CPU posts a guest CPU's interrupts into, for the CPU to
/// take in the guest (VT-x's posted interrupts): PIR, a request bit a
/// vector, and ON, a notification outstanding -- the descriptor as the SDM
/// lays it out (Vol. 3C, 30.6), 64 bytes aligned to 64, so one physical
/// range: the CPU reads and writes it with locked operations while the guest
/// runs, as the posting CPUs do, and as the vCPU's own task does when it
/// takes what was posted ([`take`](Self::take)). A plain object of atomics,
/// its physical address what the VMCS names; whoever holds the guest CPU
/// holds an `Arc` of it, so it outlives every entry that names it.
#[repr(C, align(64))]
pub struct PostedInterrupts {
    pir: [AtomicU64; 4],
    control: AtomicU64,
    _rest: [AtomicU64; 3],
}

/// ON: a notification is outstanding, and the posting CPU that set it sent
/// it or woke the vCPU.
const PI_ON: u64 = 1 << 0;

impl PostedInterrupts {
    pub fn new() -> PostedInterrupts {
        PostedInterrupts {
            pir: [const { AtomicU64::new(0) }; 4],
            control: AtomicU64::new(0),
            _rest: [const { AtomicU64::new(0) }; 3],
        }
    }

    /// Post `vector`: true when ON was clear, and the caller is to send the
    /// notification (`Kick::notify`) or wake the vCPU; false when one is
    /// outstanding already, which the vCPU takes this one with.
    pub fn post(&self, vector: u8) -> bool {
        self.pir[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::SeqCst);
        self.control.fetch_or(PI_ON, Ordering::SeqCst) & PI_ON == 0
    }

    /// What was posted and is not taken yet -- by an earlier look, or by the
    /// CPU in the guest -- a bit a vector in four words; ON cleared first,
    /// so that what is posted after is notified again. None while ON is
    /// clear: a poster sets it after its bit.
    pub fn take(&self) -> Option<[u64; 4]> {
        if self.control.fetch_and(!PI_ON, Ordering::SeqCst) & PI_ON == 0 {
            return None;
        }
        let mut words = [0u64; 4];
        for (w, p) in words.iter_mut().zip(self.pir.iter()) {
            *w = p.swap(0, Ordering::SeqCst);
        }
        Some(words)
    }

    /// Where the CPU finds it.
    fn phys(&self) -> u64 {
        kcore::dma::virt_to_phys(self as *const PostedInterrupts as *const u8)
    }
}

impl Default for PostedInterrupts {
    fn default() -> Self {
        Self::new()
    }
}

/* The host's interrupt handlers by vector, each an `int` and a `ret`: what
 * delivers an interrupt the CPU acknowledged at a VM exit -- and so to
 * nothing -- through the host's IDT, as the CPU would have: the frame, the
 * stack aligned, interrupts off in the handler and as they were after its
 * IRET (`deliver_host_interrupt`). Four bytes each, from vector 32: CD ib,
 * C3, and an INT3 nothing reaches. */
const FIRST_EXTERNAL_VECTOR: u8 = 32;
const INT_STUB_BYTES: u64 = 4;
core::arch::global_asm!(
    ".pushsection .text.hvarch_int_vectors, \"ax\"",
    ".balign 16",
    ".globl hvarch_int_vectors",
    ".hidden hvarch_int_vectors",
    "hvarch_int_vectors:",
    ".set hvarch_int_n, 32",
    ".rept 224",
    ".byte 0xCD, hvarch_int_n",
    "ret",
    "int3",
    ".set hvarch_int_n, hvarch_int_n + 1",
    ".endr",
    ".popsection",
);

/// Run the host's handler for `vector`, an interrupt the CPU acknowledged
/// at a VM exit (the exit control above) and so delivered to nothing: by an
/// `int` of it, through the host's IDT, as the CPU would have delivered it.
///
/// # Safety
/// `vector` is one the CPU acknowledged for the host at this CPU's last
/// exit, delivered no other way and only this once; and no per-CPU borrow is
/// held -- the handler may switch tasks (`Preempt`), as an interrupt taken
/// with interrupts on may.
unsafe fn deliver_host_interrupt(vector: u8) {
    /* An external interrupt has a vector above the exceptions'; an `int` of
     * one of those would be the exception's handler with no error code. */
    assert!(vector >= FIRST_EXTERNAL_VECTOR, "an interrupt acknowledged for the host with vector {}", vector);
    let offset = u64::from(vector - FIRST_EXTERNAL_VECTOR) * INT_STUB_BYTES;
    unsafe {
        asm!(
            "lea rcx, [rip + hvarch_int_vectors]",
            "add rcx, rax",
            "call rcx",
            in("rax") offset,
            clobber_abi("C"),
        );
    }
}

/// What [`evict_here`] needs to VMCLEAR a guest's VMCS off the CPU it is
/// current on: its physical address, and the guest's `loaded_cpu`/`launched`
/// to reset once it is clear. Shared by reference across an IPI, so `Sync`.
struct Evict<'a> {
    phys: u64,
    loaded_cpu: &'a AtomicI32,
    launched: &'a AtomicBool,
}

/// VMCLEAR a guest's VMCS off the CPU this runs on -- the target of the IPI
/// [`Guest::run`]'s load phase (and [`Guest::drop`]) sends when the VMCS must
/// leave a CPU before it can be made current on another, or before its page
/// is freed. Runs in IPI context on that CPU; VMCLEAR needs VMX on there,
/// which it is while any guest exists.
fn evict_here(e: &Evict) {
    if enabled() {
        unsafe { vmcs::vmclear(e.phys) };
    }
    cleared_on(kcore::cpu::id() as usize, e.phys);
    now_unloaded(e.loaded_cpu, e.launched);
}

impl Drop for Guest {
    fn drop(&mut self) {
        /* If the VMCS is still current on a CPU, VMCLEAR it there before its
         * page is freed: else that CPU's next VMPTRLD would write this VMCS's
         * cached state into freed memory. An IPI that waits -- a drop runs in
         * task context, interrupts on. */
        let loaded = self.loaded_cpu.load(Ordering::Acquire);
        if loaded >= 0 {
            let evict = Evict {
                phys: self.vmcs.phys(),
                loaded_cpu: &self.loaded_cpu,
                launched: &self.launched,
            };
            kcore::cpu::run_on_with(loaded as u32, &evict, evict_here);
            /* As in `run`: current nowhere now, whether the IPI ran or the
             * CPU it was for has gone. */
            now_unloaded(&self.loaded_cpu, &self.launched);
        }
        /* Only now, with no VMCS naming the number current anywhere. */
        vpid_give_back(self.vpid);
    }
}

/// Enter the guest with the GPRs in `gpr`, and come back at its next exit
/// with them stored back. `flags` says how: [`RUN_RESUME`] to VMRESUME a
/// VMCS already in the launched state, else VMLAUNCH one that is not;
/// [`RUN_VERW`] to VERW just before the entry; [`RUN_SPEC_CTRL`] to write
/// `spec_ctrl` into IA32_SPEC_CTRL right after the exit. Returns 0 on a VM
/// exit, non-zero when the VMLAUNCH/VMRESUME itself failed (VMfail: the
/// guest never started).
///
/// VMLAUNCH/VMRESUME loads the guest and, at its exit, jumps to the host RIP
/// this writes -- back to the `2:` label, on the host RSP this writes -- so
/// the host's callee-saved registers are on the stack for it to restore,
/// and below them what the exit path needs: the `gpr` pointer, `flags`,
/// `spec_ctrl`, and VERW's operand. Guest RSP and RIP are VMCS fields, not
/// the stub's to move.
///
/// Around the world switch, for the guest's speculation (`super::spec`):
///
/// - VERW, the last thing before the entry: the CPU's buffers cleared of
///   what the host had in them, where MDS, TAA or RFDS would let the guest
///   sample them. Only the memory form clears them; its operand is a
///   selector of 0 -- this kernel's GDT has no data segment -- which works
///   with any selector, a writable data one being merely the fastest.
/// - After the exit, before the host's first indirect branch or `ret`:
///   IA32_SPEC_CTRL written again where IBRS is legacy, a barrier only when
///   written; the guest's values cleared out of every register the caller is
///   not given back; and the return stack refilled -- calls of the stub's
///   own, each over a trap that a return predicted to it lands on, the stack
///   put back after every two, and a fence for the loop's last branch:
///   KVM's FILL_RETURN_BUFFER. Interrupts are off, and a VM exit leaves
///   them so.
///
/// # Safety
/// A VMCS of ours is current on this CPU, in the launched state iff
/// `flags` has [`RUN_RESUME`], VMX is on, interrupts are off, `gpr` is
/// valid for reads and writes, and [`RUN_SPEC_CTRL`] is given only on a CPU
/// with IA32_SPEC_CTRL and `spec_ctrl` a value it takes: what
/// [`Guest::run`] holds, the last from `super::spec`.
#[unsafe(naked)]
unsafe extern "C" fn vmx_launch_stub(gpr: *mut Gpr, flags: u64, spec_ctrl: u64) -> u64 {
    core::arch::naked_asm!(
        /* Host callee-saved; then for the exit path the gpr pointer, the
         * flags and the SPEC_CTRL value; and VERW's operand, a selector of
         * 0. The layout from RSP up: operand, spec_ctrl, flags, gpr. */
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push rdi",
        "push rsi",
        "push rdx",
        "push 0",
        /* HOST_RSP and HOST_RIP: where the exit returns. */
        "mov rax, {host_rsp}",
        "vmwrite rax, rsp",
        "lea rax, [rip + 2f]",
        "mov rdx, {host_rip}",
        "vmwrite rdx, rax",
        /* Guest GPRs from [rdi]; rdi itself last, from its own slot. From
         * here on the flags are read from the stack, and every test of
         * them is a compare with memory: no register is the host's. */
        "mov rax, [rdi + {rax}]",
        "mov rbx, [rdi + {rbx}]",
        "mov rcx, [rdi + {rcx}]",
        "mov rdx, [rdi + {rdx}]",
        "mov rsi, [rdi + {rsi}]",
        "mov rbp, [rdi + {rbp}]",
        "mov r8, [rdi + {r8}]",
        "mov r9, [rdi + {r9}]",
        "mov r10, [rdi + {r10}]",
        "mov r11, [rdi + {r11}]",
        "mov r12, [rdi + {r12}]",
        "mov r13, [rdi + {r13}]",
        "mov r14, [rdi + {r14}]",
        "mov r15, [rdi + {r15}]",
        "mov rdi, [rdi + {rdi}]",
        "test qword ptr [rsp + 16], {run_verw}",
        "jz 5f",
        "verw word ptr [rsp]",
        "5:",
        "test qword ptr [rsp + 16], {run_resume}",
        "jnz 3f",
        "vmlaunch",
        "jmp 4f",
        "3:",
        "vmresume",
        /* Fall-through from either: VMfail. The guest never ran, but its
         * GPRs are loaded: cleared, and the failure reported. */
        "4:",
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        "add rsp, 32",
        "mov eax, 1",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        /* The VM exit lands here, on HOST_RSP: [rsp + 24] is the gpr
         * pointer, and every register holds the guest's. Save them. */
        "2:",
        "push rax",              // stash guest rax; the gpr pointer now at +32
        "mov rax, [rsp + 32]",
        "mov [rax + {rbx}], rbx",
        "mov [rax + {rcx}], rcx",
        "mov [rax + {rdx}], rdx",
        "mov [rax + {rsi}], rsi",
        "mov [rax + {rdi}], rdi",
        "mov [rax + {rbp}], rbp",
        "mov [rax + {r8}], r8",
        "mov [rax + {r9}], r9",
        "mov [rax + {r10}], r10",
        "mov [rax + {r11}], r11",
        "mov [rax + {r12}], r12",
        "mov [rax + {r13}], r13",
        "mov [rax + {r14}], r14",
        "mov [rax + {r15}], r15",
        "pop rcx",               // guest rax
        "mov [rax + {rax}], rcx",
        /* Legacy IBRS: IA32_SPEC_CTRL written again, before any indirect
         * branch of the host's. */
        "test qword ptr [rsp + 16], {run_spec_ctrl}",
        "jz 6f",
        "mov ecx, {msr_spec_ctrl}",
        "mov eax, dword ptr [rsp + 8]",
        "mov edx, dword ptr [rsp + 12]",
        "wrmsr",
        "6:",
        /* The guest's values out of every register the caller is not given
         * back by the pops below: none reaches host code running past a
         * branch it has not resolved. */
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        /* The return stack refilled before the first `ret` could be
         * predicted from what the guest left in it. */
        "mov ecx, {rsb_pairs}",
        "7:",
        "call 8f",
        "int3",
        "8:",
        "call 9f",
        "int3",
        "9:",
        "add rsp, 16",
        "dec ecx",
        "jnz 7b",
        "lfence",
        "xor eax, eax",
        "add rsp, 32",           // drop the operand, spec_ctrl, flags, gpr
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        host_rsp = const vmcs::HOST_RSP,
        host_rip = const vmcs::HOST_RIP,
        run_verw = const RUN_VERW,
        run_resume = const RUN_RESUME,
        run_spec_ctrl = const RUN_SPEC_CTRL,
        msr_spec_ctrl = const super::spec::MSR_SPEC_CTRL,
        rsb_pairs = const super::spec::RSB_ENTRIES / 2,
        rax = const offset_of!(Gpr, rax),
        rbx = const offset_of!(Gpr, rbx),
        rcx = const offset_of!(Gpr, rcx),
        rdx = const offset_of!(Gpr, rdx),
        rsi = const offset_of!(Gpr, rsi),
        rdi = const offset_of!(Gpr, rdi),
        rbp = const offset_of!(Gpr, rbp),
        r8 = const offset_of!(Gpr, r8),
        r9 = const offset_of!(Gpr, r9),
        r10 = const offset_of!(Gpr, r10),
        r11 = const offset_of!(Gpr, r11),
        r12 = const offset_of!(Gpr, r12),
        r13 = const offset_of!(Gpr, r13),
        r14 = const offset_of!(Gpr, r14),
        r15 = const offset_of!(Gpr, r15),
    );
}
