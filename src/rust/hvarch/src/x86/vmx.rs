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
pub const SEC_VPID: u32 = 1 << 5;
pub const SEC_UNRESTRICTED_GUEST: u32 = 1 << 7;

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

        /* A capability MSR's high half is what the CPU allows to be 1. */
        let primary = (unsafe { cpu::rdmsr(MSR_VMX_PROCBASED_CTLS) } >> 32) as u32;
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
    Ok(())
}

/// Leave VMX root operation on the CPU this runs on.
///
/// # Safety
/// Runs on the CPU it is leaving root operation on, that CPU is in root
/// operation, and no VMCS of ours is still current on it. The VMXON region
/// may be freed once this has returned.
pub unsafe fn disable() {
    unsafe { asm!("vmxoff", options(nostack)) };
    unsafe { cpu::write_cr4(cpu::read_cr4() & !CR4_VMXE) };
}

/// Whether this CPU is in VMX root operation. There is no flag that says so
/// directly; CR4.VMXE is what sets it up and what `disable` clears, and
/// nothing else in this kernel ever touches that bit.
pub fn enabled() -> bool {
    cpu::read_cr4() & CR4_VMXE != 0
}
