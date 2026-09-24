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

pub mod vmcs;

use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use super::svm::{FxArea, GuestRegs, Kick, NotRun, VECTOR_AC, VECTOR_DB, VECTOR_MC};
use super::svm::vmcb::{Save, Segment};

/* CR0 bits VMX may force in the guest; named here so `write_guest_state` can
 * lift them out of the fixed set when unrestricted guest relaxes them. */
const CR0_PE: u64 = 1 << 0;
const CR0_PG: u64 = 1 << 31;

/// XCR0 while a guest runs: x87 alone, as under AMD-V. `vmlaunch` does not
/// switch XCR0 and the guest is given no XSAVE, so nothing of another
/// guest's extended state is reachable.
const GUEST_XCR0: u64 = 1;
/// CR4.LA57: five levels of paging (see [`NotRun::FiveLevelPaging`]).
const CR4_LA57: u64 = 1 << 12;
/// CR4.VMXE, which VMX forces set in the guest's CR0/CR4 fixed bits even
/// though the guest is told it has no VMX: masked so the guest reads 0.
const CR4_VMXE_BIT: u64 = 1 << 13;

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
    /// Read every host field off the CPU this runs on.
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
/// (`Save`), the registers the launch stub moves, and the x87/SSE state
/// `vmlaunch` does not switch.
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

pub struct Guest {
    vmcs: vmcs::VmcsPage,
    /// One page holding the VM-entry MSR-load list (the guest's values of
    /// [`SWAP_MSRS`]) and the VM-exit MSR-load list (the host's), each an
    /// array of 16-byte entries the CPU reads.
    msr_area: kcore::dma::DmaBuffer,
    /// The guest state, kept in the same shape as an AMD-V save area so the
    /// policy layer is one set of code: synced to the VMCS before an entry
    /// and read back from it after.
    save: Save,
    regs: GuestRegs,
    /// One element, on the heap: a `Vec` because it can be made fallibly.
    fx: alloc::vec::Vec<FxArea>,
    xsave: bool,

    /* Control values, decided once from the CPU's capabilities. */
    basic: u64,
    pin: u32,
    proc1: u32,
    proc2: u32,
    exit_ctls: u32,
    entry_ctls: u32,
    exceptions: u32,
    unrestricted: bool,
    cr0_fixed0: u64,
    cr0_fixed1: u64,
    cr4_fixed0: u64,
    cr4_fixed1: u64,

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
    /// The next entry writes the whole guest state, not only what the policy
    /// changes: true until the first entry, and again whenever `long_mode`
    /// resets the state from scratch.
    full_sync: bool,
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
    /// A VMLAUNCH that failed outright (VMfail), with the instruction error.
    vm_instruction_error: u32,
    entry_failed: bool,
}

impl Guest {
    /// A guest CPU on a machine whose VMX `caps` say what its controls may
    /// be. `exceptions` is the vector bitmap the guest stops at, over the
    /// ones the host always takes.
    pub fn new(caps: &Caps, exceptions: u32) -> Result<Self> {
        let mut fx = alloc::vec::Vec::new();
        fx.try_reserve_exact(1).map_err(|_| Error::NoMemory)?;
        fx.push(FxArea::reset());
        let vmcs = vmcs::VmcsPage::new(caps.revision())?;
        let mut msr_area = kcore::dma::DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        msr_area.as_mut_slice().fill(0);
        let unrestricted = caps.has(SEC_UNRESTRICTED_GUEST);

        Ok(Self {
            vmcs,
            msr_area,
            save: unsafe { core::mem::zeroed() },
            regs: GuestRegs::default(),
            fx,
            xsave: cpu::has_xsave(),
            basic: caps.basic,
            pin: 0,
            proc1: 0,
            proc2: 0,
            exit_ctls: 0,
            entry_ctls: 0,
            exceptions,
            unrestricted,
            cr0_fixed0: caps.cr0_fixed0,
            cr0_fixed1: caps.cr0_fixed1,
            cr4_fixed0: caps.cr4_fixed0,
            cr4_fixed1: caps.cr4_fixed1,
            loaded_cpu: AtomicI32::new(-1),
            launched: AtomicBool::new(false),
            cleared: false,
            full_sync: true,
            configured: false,
            host_cpu: -1,
            inject: 0,
            inject_errcode: 0,
            irq_window: false,
            exit_reason: 0,
            exit_qual: 0,
            exit_intr_info: 0,
            exit_intr_errcode: 0,
            exit_instr_len: 0,
            idt_vectoring_info: 0,
            idt_vectoring_errcode: 0,
            guest_phys: 0,
            interruptibility: 0,
            vm_instruction_error: 0,
            entry_failed: false,
        })
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
    pub fn fx(&self) -> &FxArea {
        &self.fx[0]
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

    /// Whether to exit as soon as the guest can take an interrupt.
    pub fn set_irq_window(&mut self, on: bool) {
        self.irq_window = on;
    }

    /// Compute and record the control values this CPU allows, once.
    fn decide_controls(&mut self, basic: u64) {
        use vmcs::*;
        self.pin = adjust(
            PIN_EXTINT_EXITING | PIN_NMI_EXITING,
            ctls_msr(basic, MSR_VMX_PINBASED_CTLS, MSR_VMX_TRUE_PINBASED_CTLS),
        );
        self.proc1 = adjust(
            PROC_HLT_EXITING | PROC_UNCOND_IO_EXITING | PROC_SECONDARY_CTLS,
            ctls_msr(basic, MSR_VMX_PROCBASED_CTLS, MSR_VMX_TRUE_PROCBASED_CTLS),
        );
        let mut want2 = PROC2_ENABLE_EPT;
        if self.unrestricted {
            want2 |= PROC2_UNRESTRICTED_GUEST;
        }
        self.proc2 = adjust(want2, MSR_VMX_PROCBASED_CTLS2);
        self.exit_ctls = adjust(
            EXIT_HOST_ADDR_SPACE_SIZE | EXIT_LOAD_IA32_EFER | EXIT_SAVE_IA32_EFER
                | EXIT_LOAD_IA32_PAT | EXIT_SAVE_IA32_PAT,
            ctls_msr(basic, MSR_VMX_EXIT_CTLS, MSR_VMX_TRUE_EXIT_CTLS),
        );
        self.entry_ctls = adjust(
            ENTRY_IA32E_MODE_GUEST | ENTRY_LOAD_IA32_EFER | ENTRY_LOAD_IA32_PAT,
            ctls_msr(basic, MSR_VMX_ENTRY_CTLS, MSR_VMX_TRUE_ENTRY_CTLS),
        );
    }

    /// Write the fields that do not change entry to entry: the controls, the
    /// exception bitmap, the EPT pointer, the CR0/CR4 masks and shadows, and
    /// the VMCS link pointer. The VMCS is current on this CPU.
    unsafe fn configure(&mut self, eptp: u64) {
        use vmcs::*;
        let basic = self.basic;
        self.decide_controls(basic);
        unsafe {
            vmwrite(PIN_BASED_CTLS, self.pin as u64);
            vmwrite(PROC_BASED_CTLS, self.proc1 as u64);
            vmwrite(PROC_BASED_CTLS2, self.proc2 as u64);
            vmwrite(VMEXIT_CTLS, self.exit_ctls as u64);
            vmwrite(VMENTRY_CTLS, self.entry_ctls as u64);
            /* Intercept #DB, #AC and #MC always -- the same delivery-loop
             * denial-of-service the AMD side guards against -- over whatever
             * else the policy asked. */
            let host_exc = (1u32 << VECTOR_DB) | (1u32 << VECTOR_AC) | (1u32 << VECTOR_MC);
            vmwrite(EXCEPTION_BITMAP, (self.exceptions | host_exc) as u64);
            vmwrite(PAGE_FAULT_ERRCODE_MASK, 0);
            vmwrite(PAGE_FAULT_ERRCODE_MATCH, 0);
            vmwrite(CR3_TARGET_COUNT, 0);
            /* All MSR and I/O accesses exit: no bitmaps, so nothing the
             * guest reads or writes reaches the host's real MSRs or ports. */
            vmwrite(EPT_POINTER, eptp);
            vmwrite(VMCS_LINK_POINTER, u64::MAX);
            vmwrite(VPID, 0);
            let msr_phys = self.msr_area.phys();
            vmwrite(VMENTRY_MSR_LOAD_ADDR, msr_phys + MSR_ENTRY_BASE as u64);
            vmwrite(VMENTRY_MSR_LOAD_COUNT, SWAP_MSRS.len() as u64);
            vmwrite(VMEXIT_MSR_LOAD_ADDR, msr_phys + MSR_EXIT_BASE as u64);
            vmwrite(VMEXIT_MSR_LOAD_COUNT, SWAP_MSRS.len() as u64);
            vmwrite(VMEXIT_MSR_STORE_ADDR, msr_phys + MSR_STORE_BASE as u64);
            vmwrite(VMEXIT_MSR_STORE_COUNT, 1);
            vmwrite(TSC_OFFSET, 0);
            /* The guest owns all of CR0 but for what VMX forces; CR4.VMXE is
             * forced set in hardware but read as 0 by the guest, which is
             * told it has no VMX. */
            vmwrite(CR0_GUEST_HOST_MASK, 0);
            vmwrite(CR4_GUEST_HOST_MASK, CR4_VMXE_BIT);
            vmwrite(GUEST_IA32_DEBUGCTL, 0);
        }
        self.configured = true;
    }

    /// Sync the shadow guest state into the VMCS current on this CPU, before
    /// an entry: every field the policy layer may have changed.
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
        if self.full_sync {
            let seg = |sel_f: u32, base_f: u32, lim_f: u32, ar_f: u32, g: &Segment| unsafe {
                vmwrite(sel_f, g.selector as u64);
                vmwrite(base_f, g.base);
                vmwrite(lim_f, g.limit as u64);
                vmwrite(ar_f, ar_from_attrib(g.attrib) as u64);
            };
            unsafe {
                /* CR0/CR4 with the bits VMX forces; with unrestricted guest,
                 * PE and PG are not forced. Written once: from here the guest
                 * changes its own CR0/3/4 in the VMCS, unintercepted. */
                let mut cr0_f0 = self.cr0_fixed0;
                if self.unrestricted {
                    cr0_f0 &= !(CR0_PE | CR0_PG);
                }
                let guest_cr0 = (s.cr0 | cr0_f0) & self.cr0_fixed1;
                let guest_cr4 = (s.cr4 | self.cr4_fixed0 | CR4_VMXE_BIT) & self.cr4_fixed1;
                vmwrite(GUEST_CR0, guest_cr0);
                vmwrite(GUEST_CR3, s.cr3);
                vmwrite(GUEST_CR4, guest_cr4);
                /* The read shadow masks only VMXE, so its VMXE=0 is what the
                 * guest reads there forever; the other bits are unmasked and
                 * read from the live CR, so this too is a one-time write. */
                vmwrite(CR0_GUEST_HOST_MASK, 0);
                vmwrite(CR0_READ_SHADOW, s.cr0);
                vmwrite(CR4_READ_SHADOW, s.cr4 & !CR4_VMXE_BIT);

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
            self.full_sync = false;
        }
        unsafe {
            /* RIP the policy moves past an instruction; the system MSRs it
             * changes only through an intercepted `wrmsr`, so writing them
             * from the shadow each entry is cheap and always current. */
            vmwrite(GUEST_RIP, s.rip);
            vmwrite(GUEST_FS_BASE, s.fs.base);
            vmwrite(GUEST_GS_BASE, s.gs.base);
            vmwrite(GUEST_IA32_EFER, s.efer & !super::svm::EFER_SVME);
            vmwrite(GUEST_IA32_PAT, s.g_pat);
            vmwrite(GUEST_SYSENTER_CS, s.sysenter_cs);
            vmwrite(GUEST_SYSENTER_ESP, s.sysenter_esp);
            vmwrite(GUEST_SYSENTER_EIP, s.sysenter_eip);
            vmwrite(GUEST_INTERRUPTIBILITY, self.interruptibility as u64);
        }
    }

    /// Fill the VM-entry MSR-load list with the guest's `SWAP_MSRS` (from the
    /// shadow, where the policy keeps them) and the VM-exit MSR-load list with
    /// the host's (off the CPU this runs on), so the CPU loads the guest's on
    /// entry and puts the host's back on exit.
    fn fill_msr_lists(&mut self) {
        let guest = [
            self.save.star, self.save.lstar, self.save.cstar,
            self.save.sfmask, self.save.kernel_gs_base,
        ];
        for (i, (&msr, &gval)) in SWAP_MSRS.iter().zip(guest.iter()).enumerate() {
            let host = unsafe { cpu::rdmsr(msr) };
            let e = MSR_ENTRY_BASE + i * 16;
            let x = MSR_EXIT_BASE + i * 16;
            self.msr_area.store::<u32>(e, msr);
            self.msr_area.store::<u32>(e + 4, 0);
            self.msr_area.store::<u64>(e + 8, gval);
            self.msr_area.store::<u32>(x, msr);
            self.msr_area.store::<u32>(x + 4, 0);
            self.msr_area.store::<u64>(x + 8, host);
        }
        /* The exit-store list, one entry: the CPU writes the guest's live
         * KERNEL_GS_BASE (which its `swapgs` may have changed) at +8. */
        self.msr_area.store::<u32>(MSR_STORE_BASE, MSR_KERNEL_GS_BASE);
        self.msr_area.store::<u32>(MSR_STORE_BASE + 4, 0);
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

    /// Read the guest state the policy layer reads back out of the VMCS into
    /// the shadow, after an exit.
    /// Read back the little the policy reads every exit: where the guest
    /// stopped, its stack and flags (the run loop asks whether it can take an
    /// interrupt), and the interruptibility the CPU set. The heavy state --
    /// the control registers and segments -- is left in the VMCS and read
    /// only when a guest is being stopped and dumped ([`read_guest_heavy`]).
    unsafe fn read_guest_state(&mut self) {
        use vmcs::*;
        unsafe {
            self.save.rip = vmread(GUEST_RIP);
            self.save.rsp = vmread(GUEST_RSP);
            self.save.rflags = vmread(GUEST_RFLAGS);
            /* FS and GS base: the guest changes them both ways -- `wrfsbase`
             * un-intercepted, and `wrmsr` intercepted, which the MSR policy
             * reads and writes here -- so unlike the rest of a segment they
             * are synced every exit. Linux keeps its per-CPU data at the GS
             * base, as this kernel does; a stale one is the guest lost. */
            self.save.fs.base = vmread(GUEST_FS_BASE);
            self.save.gs.base = vmread(GUEST_GS_BASE);
            self.interruptibility = vmread(GUEST_INTERRUPTIBILITY) as u32;
        }
    }

    /// The control registers and segments, into the shadow, for a report: read
    /// while the VMCS is still current, on the exits that end a guest. After
    /// this the shadow holds the whole guest state, as it did every exit
    /// before the sync was made lazy.
    unsafe fn read_guest_heavy(&mut self) {
        use vmcs::*;
        let s = &mut self.save;
        let rd = |sel_f: u32, base_f: u32, lim_f: u32, ar_f: u32, g: &mut Segment| unsafe {
            g.selector = vmread(sel_f) as u16;
            g.base = vmread(base_f);
            g.limit = vmread(lim_f) as u32;
            g.attrib = attrib_from_ar(vmread(ar_f) as u32);
        };
        unsafe {
            s.cr0 = vmread(GUEST_CR0);
            s.cr3 = vmread(GUEST_CR3);
            s.cr4 = vmread(GUEST_CR4) & !CR4_VMXE_BIT;
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
    }

    /// Whether an exit ends the guest -- one the policy will dump, so the
    /// heavy state is worth reading. The common exits (I/O, CPUID, MSR, HLT,
    /// the host's interrupt, an interrupt window) are not among them.
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
        )
    }

    /// Read the exit information the decoder needs, after an exit.
    unsafe fn read_exit(&mut self) {
        use vmcs::*;
        unsafe {
            self.exit_reason = vmread(EXIT_REASON) as u32;
            self.exit_qual = vmread(EXIT_QUALIFICATION);
            self.exit_intr_info = vmread(VMEXIT_INTR_INFO) as u32;
            self.exit_intr_errcode = vmread(VMEXIT_INTR_ERRCODE) as u32;
            self.exit_instr_len = vmread(VMEXIT_INSTRUCTION_LEN) as u32;
            self.idt_vectoring_info = vmread(IDT_VECTORING_INFO) as u32;
            self.idt_vectoring_errcode = vmread(IDT_VECTORING_ERRCODE) as u32;
            self.guest_phys = vmread(GUEST_PHYSICAL_ADDRESS);
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
        let xsave = self.xsave;
        let fx: *mut FxArea = &mut self.fx[0];

        loop {
            /* Load phase, interrupts on: if our VMCS is still current on
             * another CPU (the guest's task has migrated since the last
             * entry), VMCLEAR it there first. That is an IPI that waits for
             * the other CPU to answer -- safe only from task context, never
             * with interrupts off -- after which the VMCS is current nowhere,
             * so the VMPTRLD below cannot make it current on two CPUs at once.
             * `loaded_cpu` may go stale the instant we read it (the task can
             * move again); the recheck under `irq_save` below closes that. */
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
                self.loaded_cpu.store(-1, Ordering::Release);
                self.launched.store(false, Ordering::Release);
            }

            /* Pinned to this CPU with interrupts off, so the CPU the checks
             * see and the VMCS is made current on is the CPU the entry runs
             * on. Interrupts off first, then the CPU id -- the other order
             * could read one CPU and run on another. */
            let flags = kcore::cpu::irq_save();
            let cpu = kcore::cpu::id();
            /* The task may have moved between the load phase and here. If the
             * VMCS is current on a CPU that is not this one, go back and evict
             * it with interrupts on, rather than VMPTRLD a second copy. */
            let stale = {
                let l = self.loaded_cpu.load(Ordering::Acquire);
                l >= 0 && l as u32 != cpu
            };
            if stale {
                unsafe { kcore::cpu::irq_restore(flags) };
                continue;
            }
            let ran: core::result::Result<u32, NotRun> = (|| {
            let expected = host_areas.get(cpu as usize).map_or(0, |a| a.load(Ordering::Acquire));
            if expected == 0 || !enabled() {
                return Err(NotRun::Off { cpu });
            }
            if cpu::read_cr4() & CR4_LA57 != 0 {
                return Err(NotRun::FiveLevelPaging { cpu });
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
            /* Make our VMCS current on this CPU -- cheap if it already is,
             * from an earlier entry that left it current here (no per-exit
             * VMCLEAR). The entry then VMRESUMEs it if it is in the launched
             * state, VMLAUNCHes it if not. It stays current after the exit so
             * the next entry here can resume; a migration VMCLEARs it off
             * this CPU (above) before another can make it current. */
            if !unsafe { vmcs::vmptrld(vmcs_phys) } {
                if let Some(k) = kick { k.left(); }
                return Err(NotRun::Off { cpu });
            }
            self.loaded_cpu.store(cpu as i32, Ordering::Release);

            if !self.configured {
                unsafe { self.configure(nested.root) };
            }
            if self.host_cpu != cpu as i32 {
                unsafe { HostRegs::capture().write() };
                self.host_cpu = cpu as i32;
            }
            unsafe { self.write_guest_state() };
            self.fill_msr_lists();
            unsafe {
                /* The interrupt-window control is dynamic: on only while the
                 * policy is waiting to inject an IRQ the guest cannot take
                 * yet. One vmwrite an entry over the configured value. */
                let proc1 = self.proc1
                    | if self.irq_window { vmcs::PROC_INTR_WINDOW_EXITING } else { 0 };
                vmcs::vmwrite(vmcs::PROC_BASED_CTLS, proc1 as u64);
                vmcs::vmwrite(vmcs::VMENTRY_INTR_INFO, self.inject);
                vmcs::vmwrite(vmcs::VMENTRY_EXCEPTION_ERRCODE, self.inject_errcode as u64);
                if self.inject as u32 & vmcs::intr::VALID != 0 {
                    vmcs::vmwrite(vmcs::VMENTRY_INSTRUCTION_LEN, self.exit_instr_len as u64);
                }
            }

            /* The x87/SSE state and XCR0 are the guest's from here to the
             * FXSAVE after the exit, exactly as under AMD-V: `vmlaunch` does
             * not switch them. CR2 is the guest's likewise -- VMX keeps no
             * guest CR2 -- so save the host's and restore it after. */
            let host_cr4 = cpu::read_cr4();
            let window = host_cr4 | cpu::CR4_OSFXSR | if xsave { cpu::CR4_OSXSAVE } else { 0 };
            unsafe { cpu::write_cr4(window) };
            let host_xcr0 = if xsave { unsafe { cpu::xgetbv0() } } else { GUEST_XCR0 };
            let switch_xcr0 = host_xcr0 != GUEST_XCR0;
            if switch_xcr0 {
                unsafe { cpu::xsetbv0(GUEST_XCR0) };
            }
            unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) fx, options(nostack, preserves_flags)) };
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
            let failed = unsafe { vmx_launch_stub(&mut gpr, resume as u64) };

            self.save.cr2 = cpu::read_cr2();
            unsafe { cpu::write_cr2(host_cr2) };
            unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) fx, options(nostack, preserves_flags)) };
            if switch_xcr0 {
                unsafe { cpu::xsetbv0(host_xcr0) };
            }
            unsafe { cpu::write_cr4(host_cr4) };
            if let Some(k) = kick { k.left(); }

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
                self.launched.store(false, Ordering::Release);
                self.loaded_cpu.store(-1, Ordering::Release);
            } else {
                self.entry_failed = false;
                /* The VMCS is launched now: the next entry here resumes it. */
                self.launched.store(true, Ordering::Release);
                unsafe { self.read_exit() };
                unsafe { self.read_guest_state() };
                self.read_stored_kernel_gs_base();
                /* The control registers and segments only when the exit is
                 * one that ends and dumps the guest -- the VMCS is current
                 * here (it stays current after the exit). */
                if self.exit_is_stopping() {
                    unsafe { self.read_guest_heavy() };
                }
                /* A machine check taken while the guest ran is the host's,
                 * and the CPU did not deliver it: raise it, as the AMD side
                 * does, to the handler that treats one as fatal. */
                if self.exit_reason & vmcs::reason::BASIC_MASK == vmcs::reason::EXCEPTION_NMI
                    && self.exit_intr_info & vmcs::intr::VECTOR_MASK == VECTOR_MC
                    && self.exit_intr_info & vmcs::intr::VALID != 0
                {
                    unsafe { core::arch::asm!("int 0x12") };
                }
            }
            /* No per-exit VMCLEAR: the VMCS stays current on this CPU, ready
             * for the next entry to VMRESUME. It is evicted only on a
             * migration (the load phase above) or when the guest is dropped. */
            Ok(cpu)
            })();
            unsafe { kcore::cpu::irq_restore(flags) };
            return ran;
        }
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
    e.launched.store(false, Ordering::Release);
    e.loaded_cpu.store(-1, Ordering::Release);
}

impl Drop for Guest {
    fn drop(&mut self) {
        /* If the VMCS is still current on a CPU, VMCLEAR it there before its
         * page is freed: else that CPU's next VMPTRLD would write this VMCS's
         * cached state into freed memory. An IPI that waits -- a drop runs in
         * task context, interrupts on. */
        let loaded = self.loaded_cpu.load(Ordering::Acquire);
        if loaded < 0 {
            return;
        }
        let evict = Evict {
            phys: self.vmcs.phys(),
            loaded_cpu: &self.loaded_cpu,
            launched: &self.launched,
        };
        kcore::cpu::run_on_with(loaded as u32, &evict, evict_here);
    }
}

/// Enter the guest with the GPRs in `gpr`, and come back at its next exit
/// with them stored back. `resume` is non-zero to VMRESUME a VMCS already in
/// the launched state, zero to VMLAUNCH one that is not. Returns 0 on a VM
/// exit, non-zero when the VMLAUNCH/VMRESUME itself failed (VMfail: the guest
/// never started).
///
/// VMLAUNCH/VMRESUME loads the guest and, at its exit, jumps to the host RIP
/// this writes -- back to the `2:` label, on the host RSP this writes -- so
/// the host's callee-saved registers are on the stack for it to restore.
/// Guest RSP and RIP are VMCS fields, not the stub's to move.
///
/// # Safety
/// A VMCS of ours is current on this CPU, in the launched state iff `resume`
/// is non-zero, VMX is on, interrupts are off, and `gpr` is valid for reads
/// and writes: what [`Guest::run`] holds.
#[unsafe(naked)]
unsafe extern "C" fn vmx_launch_stub(gpr: *mut Gpr, resume: u64) -> u64 {
    core::arch::naked_asm!(
        /* Host callee-saved, then the gpr pointer for the exit path. */
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "push rdi",
        /* HOST_RSP and HOST_RIP: where the exit returns. */
        "mov rax, {host_rsp}",
        "vmwrite rax, rsp",
        "lea rax, [rip + 2f]",
        "mov rdx, {host_rip}",
        "vmwrite rdx, rax",
        /* The launch/resume choice, taken now from `resume` (rsi, arg 2)
         * before the guest GPR loads clobber it. `mov` never touches the
         * flags, so ZF survives every load down to the branch. */
        "test rsi, rsi",
        /* Guest GPRs from [rdi]; rdi itself last, from its own slot. */
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
        "jnz 3f",
        "vmlaunch",
        "jmp 4f",
        "3:",
        "vmresume",
        /* Fall-through from either: VMfail. Guest GPRs are loaded, so restore
         * the pointer and report. */
        "4:",
        "pop rdi",
        "mov rax, 1",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        /* The VM-exit lands here, on HOST_RSP: [rsp] is the gpr pointer, and
         * every register holds the guest's. Save them. */
        "2:",
        "push rax",              // stash guest rax
        "mov rax, [rsp + 8]",    // gpr pointer
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
        "add rsp, 8",            // drop the gpr pointer
        "xor eax, eax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        host_rsp = const vmcs::HOST_RSP,
        host_rip = const vmcs::HOST_RIP,
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
