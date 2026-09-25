//! The VMCS: the structure a guest runs from under Intel VT-x, reached only
//! through `vmread`/`vmwrite` -- never a field at a byte offset, the way a
//! VMCB is. So this module is field *encodings* and the instructions that
//! carry a value to and from them, not a `#[repr(C)]` laid out against a
//! manual. What the fields *mean* -- which controls to set, what a guest
//! starts in -- is policy, and lives in `hv`.
//!
//! Encodings are from the Intel SDM, volume 3, appendix B. A control field
//! is not written raw: each has bits the CPU forces to 0 and bits it forces
//! to 1 ([`adjust`]), read from the capability MSRs, and a value that
//! ignored them would fail VM entry with no field named.

use core::arch::asm;

use kcore::dma::DmaBuffer;

use super::super::cpu;
use crate::{Error, Result};

/* VMCS field encodings. Grouped as the manual groups them: 16/64/32/natural
 * width, and control / read-only / guest / host within each. Only the fields
 * this hypervisor touches are here. */

/* 16-bit control. */
pub const VPID: u32 = 0x0000;

/* 16-bit guest state. */
pub const GUEST_ES_SEL: u32 = 0x0800;
pub const GUEST_CS_SEL: u32 = 0x0802;
pub const GUEST_SS_SEL: u32 = 0x0804;
pub const GUEST_DS_SEL: u32 = 0x0806;
pub const GUEST_FS_SEL: u32 = 0x0808;
pub const GUEST_GS_SEL: u32 = 0x080A;
pub const GUEST_LDTR_SEL: u32 = 0x080C;
pub const GUEST_TR_SEL: u32 = 0x080E;

/* 16-bit host state. */
pub const HOST_ES_SEL: u32 = 0x0C00;
pub const HOST_CS_SEL: u32 = 0x0C02;
pub const HOST_SS_SEL: u32 = 0x0C04;
pub const HOST_DS_SEL: u32 = 0x0C06;
pub const HOST_FS_SEL: u32 = 0x0C08;
pub const HOST_GS_SEL: u32 = 0x0C0A;
pub const HOST_TR_SEL: u32 = 0x0C0C;

/* 64-bit control. */
pub const IO_BITMAP_A: u32 = 0x2000;
pub const IO_BITMAP_B: u32 = 0x2002;
pub const MSR_BITMAP: u32 = 0x2004;
pub const VMEXIT_MSR_STORE_ADDR: u32 = 0x2006;
pub const VMEXIT_MSR_LOAD_ADDR: u32 = 0x2008;
pub const VMENTRY_MSR_LOAD_ADDR: u32 = 0x200A;
pub const TSC_OFFSET: u32 = 0x2010;
pub const EPT_POINTER: u32 = 0x201A;

/* 64-bit read-only data. */
pub const GUEST_PHYSICAL_ADDRESS: u32 = 0x2400;

/* 64-bit guest state. */
pub const VMCS_LINK_POINTER: u32 = 0x2800;
pub const GUEST_IA32_DEBUGCTL: u32 = 0x2802;
pub const GUEST_IA32_PAT: u32 = 0x2804;
pub const GUEST_IA32_EFER: u32 = 0x2806;

/* 64-bit host state. */
pub const HOST_IA32_PAT: u32 = 0x2C00;
pub const HOST_IA32_EFER: u32 = 0x2C02;

/* 32-bit control. */
pub const PIN_BASED_CTLS: u32 = 0x4000;
pub const PROC_BASED_CTLS: u32 = 0x4002;
pub const EXCEPTION_BITMAP: u32 = 0x4004;
pub const PAGE_FAULT_ERRCODE_MASK: u32 = 0x4006;
pub const PAGE_FAULT_ERRCODE_MATCH: u32 = 0x4008;
pub const CR3_TARGET_COUNT: u32 = 0x400A;
pub const VMEXIT_CTLS: u32 = 0x400C;
pub const VMEXIT_MSR_STORE_COUNT: u32 = 0x400E;
pub const VMEXIT_MSR_LOAD_COUNT: u32 = 0x4010;
pub const VMENTRY_CTLS: u32 = 0x4012;
pub const VMENTRY_MSR_LOAD_COUNT: u32 = 0x4014;
pub const VMENTRY_INTR_INFO: u32 = 0x4016;
pub const VMENTRY_EXCEPTION_ERRCODE: u32 = 0x4018;
pub const VMENTRY_INSTRUCTION_LEN: u32 = 0x401A;
pub const PROC_BASED_CTLS2: u32 = 0x401E;

/* 32-bit read-only data. */
pub const VM_INSTRUCTION_ERROR: u32 = 0x4400;
pub const EXIT_REASON: u32 = 0x4402;
pub const VMEXIT_INTR_INFO: u32 = 0x4404;
pub const VMEXIT_INTR_ERRCODE: u32 = 0x4406;
pub const IDT_VECTORING_INFO: u32 = 0x4408;
pub const IDT_VECTORING_ERRCODE: u32 = 0x440A;
pub const VMEXIT_INSTRUCTION_LEN: u32 = 0x440C;
pub const VMEXIT_INSTRUCTION_INFO: u32 = 0x440E;

/* 32-bit guest state. */
pub const GUEST_ES_LIMIT: u32 = 0x4800;
pub const GUEST_CS_LIMIT: u32 = 0x4802;
pub const GUEST_SS_LIMIT: u32 = 0x4804;
pub const GUEST_DS_LIMIT: u32 = 0x4806;
pub const GUEST_FS_LIMIT: u32 = 0x4808;
pub const GUEST_GS_LIMIT: u32 = 0x480A;
pub const GUEST_LDTR_LIMIT: u32 = 0x480C;
pub const GUEST_TR_LIMIT: u32 = 0x480E;
pub const GUEST_GDTR_LIMIT: u32 = 0x4810;
pub const GUEST_IDTR_LIMIT: u32 = 0x4812;
pub const GUEST_ES_AR: u32 = 0x4814;
pub const GUEST_CS_AR: u32 = 0x4816;
pub const GUEST_SS_AR: u32 = 0x4818;
pub const GUEST_DS_AR: u32 = 0x481A;
pub const GUEST_FS_AR: u32 = 0x481C;
pub const GUEST_GS_AR: u32 = 0x481E;
pub const GUEST_LDTR_AR: u32 = 0x4820;
pub const GUEST_TR_AR: u32 = 0x4822;
pub const GUEST_INTERRUPTIBILITY: u32 = 0x4824;
pub const GUEST_ACTIVITY_STATE: u32 = 0x4826;
pub const GUEST_SYSENTER_CS: u32 = 0x482A;

/* 32-bit host state. */
pub const HOST_IA32_SYSENTER_CS: u32 = 0x4C00;

/* Natural-width control. */
pub const CR0_GUEST_HOST_MASK: u32 = 0x6000;
pub const CR4_GUEST_HOST_MASK: u32 = 0x6002;
pub const CR0_READ_SHADOW: u32 = 0x6004;
pub const CR4_READ_SHADOW: u32 = 0x6006;

/* Natural-width read-only data. */
pub const EXIT_QUALIFICATION: u32 = 0x6400;

/* Natural-width guest state. */
pub const GUEST_CR0: u32 = 0x6800;
pub const GUEST_CR3: u32 = 0x6802;
pub const GUEST_CR4: u32 = 0x6804;
pub const GUEST_ES_BASE: u32 = 0x6806;
pub const GUEST_CS_BASE: u32 = 0x6808;
pub const GUEST_SS_BASE: u32 = 0x680A;
pub const GUEST_DS_BASE: u32 = 0x680C;
pub const GUEST_FS_BASE: u32 = 0x680E;
pub const GUEST_GS_BASE: u32 = 0x6810;
pub const GUEST_LDTR_BASE: u32 = 0x6812;
pub const GUEST_TR_BASE: u32 = 0x6814;
pub const GUEST_GDTR_BASE: u32 = 0x6816;
pub const GUEST_IDTR_BASE: u32 = 0x6818;
pub const GUEST_DR7: u32 = 0x681A;
pub const GUEST_RSP: u32 = 0x681C;
pub const GUEST_RIP: u32 = 0x681E;
pub const GUEST_RFLAGS: u32 = 0x6820;
pub const GUEST_PENDING_DBG: u32 = 0x6822;
pub const GUEST_SYSENTER_ESP: u32 = 0x6824;
pub const GUEST_SYSENTER_EIP: u32 = 0x6826;

/* Natural-width host state. */
pub const HOST_CR0: u32 = 0x6C00;
pub const HOST_CR3: u32 = 0x6C02;
pub const HOST_CR4: u32 = 0x6C04;
pub const HOST_FS_BASE: u32 = 0x6C06;
pub const HOST_GS_BASE: u32 = 0x6C08;
pub const HOST_TR_BASE: u32 = 0x6C0A;
pub const HOST_GDTR_BASE: u32 = 0x6C0C;
pub const HOST_IDTR_BASE: u32 = 0x6C0E;
pub const HOST_SYSENTER_ESP: u32 = 0x6C10;
pub const HOST_SYSENTER_EIP: u32 = 0x6C12;
pub const HOST_RSP: u32 = 0x6C14;
pub const HOST_RIP: u32 = 0x6C16;

/* The capability MSRs that say which control bits may be 0 and which 1. */
pub const MSR_VMX_PINBASED_CTLS: u32 = 0x481;
pub const MSR_VMX_PROCBASED_CTLS: u32 = 0x482;
pub const MSR_VMX_EXIT_CTLS: u32 = 0x483;
pub const MSR_VMX_ENTRY_CTLS: u32 = 0x484;
pub const MSR_VMX_PROCBASED_CTLS2: u32 = 0x48B;
/* The "true" variants, used when IA32_VMX_BASIC[55] is set: they report the
 * default settings honestly rather than forcing the old default-1 bits. */
pub const MSR_VMX_TRUE_PINBASED_CTLS: u32 = 0x48D;
pub const MSR_VMX_TRUE_PROCBASED_CTLS: u32 = 0x48E;
pub const MSR_VMX_TRUE_EXIT_CTLS: u32 = 0x48F;
pub const MSR_VMX_TRUE_ENTRY_CTLS: u32 = 0x490;
/* IA32_VMX_BASIC bit 55: the TRUE_* MSRs exist and are authoritative. */
pub const BASIC_TRUE_CTLS: u64 = 1 << 55;

/* Pin-based controls. */
pub const PIN_EXTINT_EXITING: u32 = 1 << 0;
pub const PIN_NMI_EXITING: u32 = 1 << 3;

/* Primary processor-based controls. */
pub const PROC_INTR_WINDOW_EXITING: u32 = 1 << 2;
pub const PROC_HLT_EXITING: u32 = 1 << 7;
pub const PROC_MWAIT_EXITING: u32 = 1 << 10;
pub const PROC_RDPMC_EXITING: u32 = 1 << 11;
/// Without these two a guest's `mov cr8` reaches the local APIC's task
/// priority register itself -- the host's -- and a TPR of 15 keeps every
/// interrupt but an NMI away from the CPU, the host's tick and kick among
/// them, for as long as the guest likes and after it has gone.
pub const PROC_CR8_LOAD_EXITING: u32 = 1 << 19;
pub const PROC_CR8_STORE_EXITING: u32 = 1 << 20;
pub const PROC_UNCOND_IO_EXITING: u32 = 1 << 24;
pub const PROC_USE_MSR_BITMAPS: u32 = 1 << 28;
pub const PROC_MONITOR_EXITING: u32 = 1 << 29;
pub const PROC_SECONDARY_CTLS: u32 = 1 << 31;

/* Secondary processor-based controls. */
pub const PROC2_ENABLE_EPT: u32 = 1 << 1;
pub const PROC2_ENABLE_VPID: u32 = 1 << 5;
/// Without it WBINVD runs in the guest as it would on the host: the whole
/// of the package's shared cache written back and dropped, every core
/// stalled for milliseconds, as often as the guest cares to.
pub const PROC2_WBINVD_EXITING: u32 = 1 << 6;
pub const PROC2_UNRESTRICTED_GUEST: u32 = 1 << 7;

/* VM-exit controls. */
pub const EXIT_SAVE_DEBUG: u32 = 1 << 2;
pub const EXIT_HOST_ADDR_SPACE_SIZE: u32 = 1 << 9;
pub const EXIT_SAVE_IA32_PAT: u32 = 1 << 18;
pub const EXIT_LOAD_IA32_PAT: u32 = 1 << 19;
pub const EXIT_SAVE_IA32_EFER: u32 = 1 << 20;
pub const EXIT_LOAD_IA32_EFER: u32 = 1 << 21;

/* VM-entry controls. */
pub const ENTRY_LOAD_DEBUG: u32 = 1 << 2;
pub const ENTRY_IA32E_MODE_GUEST: u32 = 1 << 9;
pub const ENTRY_LOAD_IA32_PAT: u32 = 1 << 14;
pub const ENTRY_LOAD_IA32_EFER: u32 = 1 << 15;

/* Guest interruptibility-state bits. */
pub const INTR_BLOCK_STI: u32 = 1 << 0;
pub const INTR_BLOCK_MOV_SS: u32 = 1 << 1;

/* VM-entry / VM-exit interruption-information format. */
pub mod intr {
    pub const VECTOR_MASK: u32 = 0xFF;
    pub const TYPE_SHIFT: u32 = 8;
    pub const TYPE_MASK: u32 = 0x7 << TYPE_SHIFT;
    pub const TYPE_EXTINT: u32 = 0 << TYPE_SHIFT;
    pub const TYPE_NMI: u32 = 2 << TYPE_SHIFT;
    pub const TYPE_HW_EXCEPTION: u32 = 3 << TYPE_SHIFT;
    pub const TYPE_SOFT_INT: u32 = 4 << TYPE_SHIFT;
    /// INT1 (ICEBP): a software event like the two below it, delivered
    /// with an instruction length.
    pub const TYPE_PRIV_SOFT_EXCEPTION: u32 = 5 << TYPE_SHIFT;
    pub const TYPE_SOFT_EXCEPTION: u32 = 6 << TYPE_SHIFT;
    pub const DELIVER_ERRCODE: u32 = 1 << 11;
    pub const VALID: u32 = 1 << 31;
}

/* EXIT_QUALIFICATION of a control-register access. */
pub mod cr_access {
    /// Bits 3:0: which control register.
    pub const CR_MASK: u64 = 0xF;
    /// Bits 5:4: what was done to it.
    pub const TYPE_SHIFT: u64 = 4;
    pub const TYPE_MASK: u64 = 0x3 << TYPE_SHIFT;
    pub const MOV_TO_CR: u64 = 0 << TYPE_SHIFT;
    pub const MOV_FROM_CR: u64 = 1 << TYPE_SHIFT;
    /// Bits 11:8: the general-purpose register of a `mov`, numbered as the
    /// instruction encoding numbers them: RAX, RCX, RDX, RBX, RSP, RBP, RSI,
    /// RDI, R8-R15.
    pub const GPR_SHIFT: u64 = 8;
    pub const GPR_MASK: u64 = 0xF << GPR_SHIFT;
}

/* Basic exit reasons (EXIT_REASON, bits 15:0). */
pub mod reason {
    pub const EXCEPTION_NMI: u32 = 0;
    pub const EXTERNAL_INTERRUPT: u32 = 1;
    pub const TRIPLE_FAULT: u32 = 2;
    pub const INIT: u32 = 3;
    pub const SIPI: u32 = 4;
    pub const INTERRUPT_WINDOW: u32 = 7;
    pub const NMI_WINDOW: u32 = 8;
    pub const TASK_SWITCH: u32 = 9;
    pub const CPUID: u32 = 10;
    pub const HLT: u32 = 12;
    pub const INVD: u32 = 13;
    pub const INVLPG: u32 = 14;
    pub const RDPMC: u32 = 15;
    pub const RDTSC: u32 = 16;
    pub const VMCALL: u32 = 18;
    pub const VMCLEAR: u32 = 19;
    pub const VMLAUNCH: u32 = 20;
    pub const VMPTRLD: u32 = 21;
    pub const VMPTRST: u32 = 22;
    pub const VMREAD: u32 = 23;
    pub const VMRESUME: u32 = 24;
    pub const VMWRITE: u32 = 25;
    pub const VMXOFF: u32 = 26;
    pub const VMXON: u32 = 27;
    pub const CR_ACCESS: u32 = 28;
    pub const DR_ACCESS: u32 = 29;
    pub const IO_INSTRUCTION: u32 = 30;
    pub const RDMSR: u32 = 31;
    pub const WRMSR: u32 = 32;
    pub const ENTRY_FAIL_GUEST_STATE: u32 = 33;
    pub const ENTRY_FAIL_MSR_LOAD: u32 = 34;
    pub const MWAIT: u32 = 36;
    pub const MONITOR: u32 = 39;
    pub const PAUSE: u32 = 40;
    pub const ENTRY_FAIL_MACHINE_CHECK: u32 = 41;
    pub const EPT_VIOLATION: u32 = 48;
    pub const EPT_MISCONFIG: u32 = 49;
    pub const RDTSCP: u32 = 51;
    pub const WBINVD: u32 = 54;
    pub const XSETBV: u32 = 55;
    pub const RDRAND: u32 = 57;
    pub const INVPCID: u32 = 58;
    pub const RDSEED: u32 = 61;
    /// EXIT_REASON bit 31: VM entry failed rather than a guest exit.
    pub const ENTRY_FAILURE: u32 = 1 << 31;
    /// Bits 15:0.
    pub const BASIC_MASK: u32 = 0xFFFF;
}

/* EXIT_QUALIFICATION of an I/O instruction. */
pub mod io {
    pub const SIZE_MASK: u64 = 0x7;
    pub const IN: u64 = 1 << 3;
    pub const STRING: u64 = 1 << 4;
    pub const REP: u64 = 1 << 5;
    pub const PORT_SHIFT: u64 = 16;
}

/* EXIT_QUALIFICATION of an EPT violation. */
pub mod ept_viol {
    pub const READ: u64 = 1 << 0;
    pub const WRITE: u64 = 1 << 1;
    pub const FETCH: u64 = 1 << 2;
    /// The page's current EPT read/write/execute permissions (bits 5:3):
    /// all clear is a page with no memory behind it.
    pub const PERM_MASK: u64 = 0x7 << 3;
    /// The faulting access was to the final guest-physical address, not a
    /// step of the guest's own page-table walk.
    pub const FINAL: u64 = 1 << 8;
}

/// Turn a control value the hypervisor wants into one the CPU will take:
/// the low half of the capability MSR names bits that must be 1, the high
/// half bits that may be 1. So force the required bits on and the forbidden
/// bits off. A `desired` bit the CPU forbids is dropped -- the caller reads
/// the result back to see what it got.
pub fn adjust(desired: u32, cap_msr: u32) -> u32 {
    let cap = unsafe { cpu::rdmsr(cap_msr) };
    let allowed0 = cap as u32; // must be 1 where this is 1
    let allowed1 = (cap >> 32) as u32; // may be 1 where this is 1
    (desired | allowed0) & allowed1
}

/// Which pin/proc/exit/entry MSR to read `adjust` against: the TRUE variant
/// when the CPU has it, since the default-1 bits it does not force are ones
/// this hypervisor would rather leave off.
pub fn ctls_msr(basic: u64, normal: u32, truev: u32) -> u32 {
    if basic & BASIC_TRUE_CTLS != 0 {
        truev
    } else {
        normal
    }
}

/// The access-rights word a guest segment's VMCS field takes, from the
/// attribute the rest of this crate keeps a segment in (the VMCB packing:
/// type/S/DPL/P in the low byte, AVL/L/DB/G in bits 8-11). VMX puts AVL/L/
/// DB/G at bits 12-15 and adds an "unusable" bit at 16, which is what a
/// segment with no present bit becomes.
pub fn ar_from_attrib(attrib: u16) -> u32 {
    const P: u16 = 1 << 7;
    if attrib == 0 {
        /* A null segment: unusable, so the CPU does not check it. */
        return 1 << 16;
    }
    let low = (attrib & 0xFF) as u32;
    let high = ((attrib & 0x0F00) as u32) << 4;
    let unusable = if attrib & P == 0 { 1 << 16 } else { 0 };
    low | high | unusable
}

/// The reverse: the attribute from a guest segment's access-rights field,
/// for reading state back out after an exit.
pub fn attrib_from_ar(ar: u32) -> u16 {
    if ar & (1 << 16) != 0 {
        return 0;
    }
    let low = (ar & 0xFF) as u16;
    let high = ((ar >> 4) & 0x0F00) as u16;
    low | high
}

/// One VMCS in a page of its own. The first word is the VMX revision
/// identifier the CPU checks against its own, as a VMXON region's is; the
/// rest is opaque, touched only by `vmread`/`vmwrite` while it is current.
pub struct VmcsPage {
    buf: DmaBuffer,
}

impl VmcsPage {
    /// A page whose revision word is `revision`: just memory, nothing
    /// executed. The `vmclear` that puts it in the launch-required state
    /// needs VMX operation, so it waits for the first entry -- a VMCS is
    /// made where VMX may be off, on a CPU that will not be the one that
    /// runs the guest, and a privileged instruction here would fault.
    pub fn new(revision: u32) -> Result<Self> {
        let mut buf = DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        buf.as_mut_slice().fill(0);
        buf.as_mut_slice()[..4].copy_from_slice(&revision.to_le_bytes());
        Ok(Self { buf })
    }

    pub fn phys(&self) -> u64 {
        self.buf.phys()
    }
}

/* Every VMX instruction reports through the arithmetic flags -- all six
 * cleared on success, CF for "would not look", ZF for "looked and said no"
 * -- so none of the wrappers below may say `preserves_flags`: the compiler
 * would then be free to keep a comparison's result in EFLAGS across the
 * instruction and branch on it after. The two whose answer is read take
 * `setna`, which is CF or ZF: `setnc` alone would report a VMCS the CPU
 * refused for its revision as loaded. */

/// Read a VMCS field of the VMCS current on this CPU.
///
/// # Safety
/// A VMCS of ours is current on this CPU and `field` is a real encoding;
/// a bad encoding sets the failure flags, which this ignores and returns 0.
#[inline]
pub unsafe fn vmread(field: u32) -> u64 {
    let value: u64;
    unsafe {
        asm!("vmread {value}, {field}",
             value = out(reg) value, field = in(reg) field as u64,
             options(nostack));
    }
    value
}

/// Write a VMCS field of the VMCS current on this CPU.
///
/// # Safety
/// A VMCS of ours is current on this CPU, `field` is a real encoding, and
/// `value` is one it takes.
#[inline]
pub unsafe fn vmwrite(field: u32, value: u64) {
    unsafe {
        asm!("vmwrite {field}, {value}",
             field = in(reg) field as u64, value = in(reg) value,
             options(nostack));
    }
}

/// Make the VMCS at `phys` current on this CPU. False when the CPU refuses
/// it -- a wrong revision, or not a page in root operation.
///
/// # Safety
/// This CPU is in VMX root operation and `phys` is a VMCS page of ours.
#[inline]
pub unsafe fn vmptrld(phys: u64) -> bool {
    let failed: u8;
    unsafe {
        asm!("vmptrld qword ptr [{p}]", "setna {failed}",
             p = in(reg) &phys, failed = out(reg_byte) failed, options(nostack));
    }
    failed == 0
}

/// Flush the VMCS at `phys` to memory and make it not current on this CPU,
/// leaving it in the launch-required state. False on failure.
///
/// # Safety
/// This CPU is in VMX root operation and `phys` is a VMCS page of ours.
#[inline]
pub unsafe fn vmclear(phys: u64) -> bool {
    let failed: u8;
    unsafe {
        asm!("vmclear qword ptr [{p}]", "setna {failed}",
             p = in(reg) &phys, failed = out(reg_byte) failed, options(nostack));
    }
    failed == 0
}

/// The INVEPT types: drop the cached translations made through one EPT, or
/// through every EPT there has ever been.
pub const INVEPT_SINGLE_CONTEXT: u64 = 1;
pub const INVEPT_ALL_CONTEXT: u64 = 2;

/// The operand INVEPT reads: the EPT pointer it is about (ignored by the
/// all-context type), and a word that must be 0. In memory, 16 bytes.
#[repr(C, align(16))]
struct InveptDescriptor {
    eptp: u64,
    reserved: u64,
}

/// Invalidate the guest-physical and combined mappings this CPU has cached
/// -- those made through the EPT `eptp` names for [`INVEPT_SINGLE_CONTEXT`],
/// every EPT's for [`INVEPT_ALL_CONTEXT`]. False when the CPU refuses the
/// type, which [`super::Caps::usable`] rules out.
///
/// What VM entries and exits do not do: with VPID off they drop every linear
/// and combined mapping of the guest's, but a guest-physical mapping is
/// tagged with the EPT's own address and kept, across transitions and across
/// VMXOFF and VMXON. So an EPT freed and its page handed to the next guest's
/// EPT gives that guest the old one's translations -- to pages that are the
/// host's again -- unless the tag is dropped first.
///
/// # Safety
/// This CPU is in VMX root operation.
#[inline]
pub unsafe fn invept(kind: u64, eptp: u64) -> bool {
    let desc = InveptDescriptor { eptp, reserved: 0 };
    let failed: u8;
    unsafe {
        asm!("invept {kind}, [{desc}]", "setna {failed}",
             kind = in(reg) kind, desc = in(reg) &desc, failed = out(reg_byte) failed,
             options(nostack));
    }
    failed == 0
}
