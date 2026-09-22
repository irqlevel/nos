//! The VMCB: the 4 KiB structure `vmrun` enters a guest from and `#vmexit`
//! writes back into -- a control area that says what the guest may do, and
//! a save area that is the guest's state.
//!
//! Laid out field by field from the AMD64 Architecture Programmer's Manual,
//! volume 2, appendix B, and checked against it at compile time: every
//! field this hypervisor touches has its offset asserted below, so a
//! reserved array one byte short does not build rather than moving every
//! field after it. What the fields *mean* -- which intercepts to set, what a
//! guest starts in -- is policy, and lives in `hv`; this is only where they
//! are.

use core::mem::{offset_of, size_of};

use kcore::dma::DmaBuffer;
use kcore::pod::Pod;

use crate::{Error, Result};

/// A segment register as the VMCB keeps one, hidden part and all: the
/// selector, the attributes packed into twelve bits -- the descriptor's
/// type, S, DPL and P in the low eight, its AVL, L, D/B and G in the high
/// four -- the limit, expanded, and the base.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Segment {
    pub selector: u16,
    pub attrib: u16,
    pub limit: u32,
    pub base: u64,
}

/// Segment attribute bits, in the VMCB's packing.
pub mod attrib {
    /// The type field: for code, bit 3 set, bit 1 readable; for data, bit 1
    /// writable; bit 0 accessed. System segments use the whole field.
    pub const TYPE_MASK: u16 = 0xF;
    pub const ACCESSED: u16 = 1 << 0;
    pub const WRITE_OR_READ: u16 = 1 << 1;
    pub const CODE: u16 = 1 << 3;
    /// Code or data, as opposed to a system segment.
    pub const S: u16 = 1 << 4;
    pub const DPL_SHIFT: u16 = 5;
    pub const P: u16 = 1 << 7;
    pub const AVL: u16 = 1 << 8;
    /// 64-bit code.
    pub const L: u16 = 1 << 9;
    /// 32-bit default operand size (code) or big stack (data).
    pub const DB: u16 = 1 << 10;
    /// The limit counts pages.
    pub const G: u16 = 1 << 11;

    /// System segment types.
    pub const TYPE_LDT: u16 = 0x2;
    pub const TYPE_TSS16_BUSY: u16 = 0x3;
    pub const TYPE_TSS64_BUSY: u16 = 0xB;
}

/// The control area: what the guest may do without the host hearing of it,
/// and what the host is told when it stops.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Control {
    /// Reads of CR0-CR15 in bits 0-15, writes in bits 16-31.
    pub intercept_cr: u32,
    /// The same for DR0-DR15.
    pub intercept_dr: u32,
    /// One bit per exception vector, 0-31.
    pub intercept_exceptions: u32,
    /// [`intercept::misc1`].
    pub intercept_misc1: u32,
    /// [`intercept::misc2`].
    pub intercept_misc2: u32,
    /// INVLPGB, INVPCID and the rest of the newest instructions.
    pub intercept_misc3: u32,
    _reserved_018: [u8; 0x03C - 0x018],
    pub pause_filter_threshold: u16,
    pub pause_filter_count: u16,
    /// Physical address of the I/O permission map: 12 KiB, one bit a port,
    /// a set bit intercepted.
    pub iopm_base_pa: u64,
    /// Physical address of the MSR permission map: 8 KiB, two bits (read,
    /// write) an MSR over three ranges, a set bit intercepted.
    pub msrpm_base_pa: u64,
    pub tsc_offset: u64,
    /// Which address space the guest's TLB entries are tagged with. Never
    /// 0, which is the host's: `vmrun` refuses it.
    pub guest_asid: u32,
    /// [`tlb`]: what to flush on the way in.
    pub tlb_control: u8,
    _reserved_05d: [u8; 3],
    /// [`int_ctl`]: the virtual interrupt controls.
    pub int_ctl: u32,
    pub int_vector: u32,
    /// Bit 0: the guest is in an interrupt shadow (the instruction after
    /// STI or MOV SS). Bit 1: the guest's EFLAGS.IF, as the exit found it.
    pub int_state: u32,
    _reserved_06c: [u8; 4],
    /// Why the guest stopped: one of [`exit`], or -1 when `vmrun` refused
    /// the VMCB outright.
    pub exit_code: u64,
    pub exit_info1: u64,
    pub exit_info2: u64,
    /// An event the guest was being delivered when it stopped, in the
    /// [`event`] format: what has to be injected again for it not to be lost.
    pub exit_int_info: u64,
    /// Bit 0: nested paging.
    pub nested_ctl: u64,
    pub avic_apic_bar: u64,
    pub ghcb_gpa: u64,
    /// An event to deliver to the guest on the way in, in the [`event`]
    /// format.
    pub event_inj: u64,
    /// Physical address of the nested page table's top level.
    pub nested_cr3: u64,
    /// Bit 0: LBR virtualization. Bit 1: virtual VMSAVE/VMLOAD.
    pub virt_ext: u64,
    /// Which parts of the save area the CPU may assume unchanged since the
    /// last `vmrun`; 0 says none, which is always right.
    pub clean: u32,
    _reserved_0c4: u32,
    /// Where the intercepted instruction ends -- written only on CPUs with
    /// next-RIP save, and only for the intercepts it covers.
    pub next_rip: u64,
    /// With decode assists: how many of the intercepted instruction's bytes
    /// were fetched, and the bytes.
    pub insn_len: u8,
    pub insn_bytes: [u8; 15],
    _reserved_0e0: [u8; 0x400 - 0x0E0],
}

/// The state save area: the guest's registers, as `vmrun` loads them and
/// `#vmexit` stores them -- and as `vmload` and `vmsave` move the part of
/// them that neither of those touches (FS, GS, TR, LDTR and the MSRs from
/// STAR on).
///
/// Offsets in the comments are from the start of the VMCB, as the manual
/// gives them.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Save {
    pub es: Segment, // 0x400
    pub cs: Segment,
    pub ss: Segment,
    pub ds: Segment,
    pub fs: Segment,
    pub gs: Segment,
    pub gdtr: Segment,
    pub ldtr: Segment,
    pub idtr: Segment,
    pub tr: Segment,
    _reserved_4a0: [u8; 0x4CB - 0x4A0],
    /// The current privilege level. Not derived from CS: `vmrun` takes it
    /// from here.
    pub cpl: u8, // 0x4CB
    _reserved_4cc: [u8; 4],
    pub efer: u64, // 0x4D0
    _reserved_4d8: [u8; 0x548 - 0x4D8],
    pub cr4: u64, // 0x548
    pub cr3: u64,
    pub cr0: u64,
    pub dr7: u64,
    pub dr6: u64,
    pub rflags: u64,
    pub rip: u64, // 0x578
    _reserved_580: [u8; 0x5D8 - 0x580],
    pub rsp: u64, // 0x5D8
    pub s_cet: u64,
    pub ssp: u64,
    pub isst_addr: u64,
    pub rax: u64, // 0x5F8
    pub star: u64,
    pub lstar: u64,
    pub cstar: u64,
    pub sfmask: u64,
    pub kernel_gs_base: u64,
    pub sysenter_cs: u64,
    pub sysenter_esp: u64,
    pub sysenter_eip: u64,
    pub cr2: u64, // 0x640
    _reserved_648: [u8; 0x668 - 0x648],
    /// The guest's PAT, which nested paging combines with the host's: only
    /// read with nested paging on, and then every entry has to be a valid
    /// memory type.
    pub g_pat: u64, // 0x668
    pub dbgctl: u64,
    pub br_from: u64,
    pub br_to: u64,
    pub last_excp_from: u64,
    pub last_excp_to: u64, // 0x690
    _reserved_698: [u8; 0x1000 - 0x698],
}

/// The whole of it: one page, page-aligned.
#[repr(C, align(4096))]
#[derive(Clone, Copy)]
pub struct Vmcb {
    pub control: Control,
    pub save: Save,
}

/* A structure of integers and arrays of bytes, laid out with no padding --
 * every gap is a named reserved array, and the size assertion below is what
 * says the arrays add up -- so every bit pattern is a VMCB, if not one the
 * CPU will take. */
unsafe impl Pod for Vmcb {}

/* The offsets the manual gives, for every field anything reads or writes. A
 * mistake here is a guest that runs with the wrong intercepts, or a host
 * that reads its exit reason from a reserved field. */
const _: () = {
    assert!(size_of::<Segment>() == 16);
    assert!(size_of::<Control>() == 0x400);
    assert!(size_of::<Save>() == 0xC00);
    assert!(size_of::<Vmcb>() == 0x1000);

    assert!(offset_of!(Control, intercept_cr) == 0x000);
    assert!(offset_of!(Control, intercept_exceptions) == 0x008);
    assert!(offset_of!(Control, intercept_misc1) == 0x00C);
    assert!(offset_of!(Control, intercept_misc2) == 0x010);
    assert!(offset_of!(Control, intercept_misc3) == 0x014);
    assert!(offset_of!(Control, pause_filter_threshold) == 0x03C);
    assert!(offset_of!(Control, iopm_base_pa) == 0x040);
    assert!(offset_of!(Control, msrpm_base_pa) == 0x048);
    assert!(offset_of!(Control, tsc_offset) == 0x050);
    assert!(offset_of!(Control, guest_asid) == 0x058);
    assert!(offset_of!(Control, tlb_control) == 0x05C);
    assert!(offset_of!(Control, int_ctl) == 0x060);
    assert!(offset_of!(Control, int_vector) == 0x064);
    assert!(offset_of!(Control, int_state) == 0x068);
    assert!(offset_of!(Control, exit_code) == 0x070);
    assert!(offset_of!(Control, exit_info1) == 0x078);
    assert!(offset_of!(Control, exit_info2) == 0x080);
    assert!(offset_of!(Control, exit_int_info) == 0x088);
    assert!(offset_of!(Control, nested_ctl) == 0x090);
    assert!(offset_of!(Control, event_inj) == 0x0A8);
    assert!(offset_of!(Control, nested_cr3) == 0x0B0);
    assert!(offset_of!(Control, virt_ext) == 0x0B8);
    assert!(offset_of!(Control, clean) == 0x0C0);
    assert!(offset_of!(Control, next_rip) == 0x0C8);
    assert!(offset_of!(Control, insn_len) == 0x0D0);

    const S: usize = 0x400;
    assert!(offset_of!(Vmcb, save) == S);
    assert!(S + offset_of!(Save, es) == 0x400);
    assert!(S + offset_of!(Save, cs) == 0x410);
    assert!(S + offset_of!(Save, ss) == 0x420);
    assert!(S + offset_of!(Save, ds) == 0x430);
    assert!(S + offset_of!(Save, fs) == 0x440);
    assert!(S + offset_of!(Save, gs) == 0x450);
    assert!(S + offset_of!(Save, gdtr) == 0x460);
    assert!(S + offset_of!(Save, ldtr) == 0x470);
    assert!(S + offset_of!(Save, idtr) == 0x480);
    assert!(S + offset_of!(Save, tr) == 0x490);
    assert!(S + offset_of!(Save, cpl) == 0x4CB);
    assert!(S + offset_of!(Save, efer) == 0x4D0);
    assert!(S + offset_of!(Save, cr4) == 0x548);
    assert!(S + offset_of!(Save, cr3) == 0x550);
    assert!(S + offset_of!(Save, cr0) == 0x558);
    assert!(S + offset_of!(Save, dr7) == 0x560);
    assert!(S + offset_of!(Save, dr6) == 0x568);
    assert!(S + offset_of!(Save, rflags) == 0x570);
    assert!(S + offset_of!(Save, rip) == 0x578);
    assert!(S + offset_of!(Save, rsp) == 0x5D8);
    assert!(S + offset_of!(Save, rax) == 0x5F8);
    assert!(S + offset_of!(Save, star) == 0x600);
    assert!(S + offset_of!(Save, kernel_gs_base) == 0x620);
    assert!(S + offset_of!(Save, sysenter_eip) == 0x638);
    assert!(S + offset_of!(Save, cr2) == 0x640);
    assert!(S + offset_of!(Save, g_pat) == 0x668);
    assert!(S + offset_of!(Save, dbgctl) == 0x670);
    assert!(S + offset_of!(Save, last_excp_to) == 0x690);
};

/// A VMCB in a page of its own: what a guest runs from, or -- with only its
/// save area used -- where `vmsave` keeps the host's FS, GS, TR, LDTR and
/// syscall MSRs while a guest runs. The CPU is handed its physical address;
/// the host reads and writes it through the mapping, and only while no
/// `vmrun` of it is under way.
pub struct VmcbPage {
    buf: DmaBuffer,
}

impl VmcbPage {
    /// A zeroed VMCB: every intercept off, which is not one `vmrun` accepts
    /// -- the VMRUN intercept has to be set -- so a VMCB nobody filled in is
    /// refused rather than run.
    pub fn new() -> Result<Self> {
        let mut buf = DmaBuffer::new(1).ok_or(Error::NoMemory)?;
        buf.as_mut_slice().fill(0);
        if buf.as_pod::<Vmcb>().is_none() {
            return Err(Error::NoMemory);
        }
        Ok(Self { buf })
    }

    pub fn phys(&self) -> u64 {
        self.buf.phys()
    }

    pub fn get(&self) -> &Vmcb {
        /* `new` checked that a VMCB fits and is aligned: a page of its own,
         * page-aligned. */
        self.buf.as_pod::<Vmcb>().expect("a VMCB page holds a VMCB")
    }

    pub fn get_mut(&mut self) -> &mut Vmcb {
        self.buf.as_pod_mut::<Vmcb>().expect("a VMCB page holds a VMCB")
    }
}

/// Intercept bits, by the word of the control area they are in.
pub mod intercept {
    /// `Control::intercept_misc1`.
    pub mod misc1 {
        pub const INTR: u32 = 1 << 0;
        pub const NMI: u32 = 1 << 1;
        pub const SMI: u32 = 1 << 2;
        pub const INIT: u32 = 1 << 3;
        pub const VINTR: u32 = 1 << 4;
        pub const CR0_SEL_WRITE: u32 = 1 << 5;
        pub const RDTSC: u32 = 1 << 14;
        pub const RDPMC: u32 = 1 << 15;
        pub const CPUID: u32 = 1 << 18;
        pub const RSM: u32 = 1 << 19;
        pub const INVD: u32 = 1 << 22;
        pub const PAUSE: u32 = 1 << 23;
        pub const HLT: u32 = 1 << 24;
        pub const INVLPG: u32 = 1 << 25;
        pub const INVLPGA: u32 = 1 << 26;
        pub const IOIO_PROT: u32 = 1 << 27;
        pub const MSR_PROT: u32 = 1 << 28;
        pub const TASK_SWITCH: u32 = 1 << 29;
        pub const FERR_FREEZE: u32 = 1 << 30;
        pub const SHUTDOWN: u32 = 1 << 31;
    }

    /// `Control::intercept_misc2`.
    pub mod misc2 {
        /// Must be set: `vmrun` refuses a VMCB without it.
        pub const VMRUN: u32 = 1 << 0;
        pub const VMMCALL: u32 = 1 << 1;
        pub const VMLOAD: u32 = 1 << 2;
        pub const VMSAVE: u32 = 1 << 3;
        pub const STGI: u32 = 1 << 4;
        pub const CLGI: u32 = 1 << 5;
        pub const SKINIT: u32 = 1 << 6;
        pub const RDTSCP: u32 = 1 << 7;
        pub const ICEBP: u32 = 1 << 8;
        pub const WBINVD: u32 = 1 << 9;
        pub const MONITOR: u32 = 1 << 10;
        pub const MWAIT: u32 = 1 << 11;
        pub const MWAIT_ARMED: u32 = 1 << 12;
        pub const XSETBV: u32 = 1 << 13;
        pub const RDPRU: u32 = 1 << 14;
    }
}

/// `Control::tlb_control`.
pub mod tlb {
    pub const NOTHING: u8 = 0;
    /// Every TLB entry of every address space, the host's included. The one
    /// value every SVM part takes: flushing only the guest's needs
    /// flush-by-ASID.
    pub const FLUSH_ALL: u8 = 1;
    pub const FLUSH_ASID: u8 = 3;
}

/// `Control::int_ctl`.
pub mod int_ctl {
    pub const V_TPR_MASK: u32 = 0xFF;
    pub const V_IRQ: u32 = 1 << 8;
    pub const V_INTR_PRIO_SHIFT: u32 = 16;
    pub const V_IGN_TPR: u32 = 1 << 20;
    /// The guest's EFLAGS.IF masks only virtual interrupts; physical ones
    /// are masked by the host's, as `vmrun` found it. What keeps a guest
    /// that sits with interrupts off from keeping the host's out.
    pub const V_INTR_MASKING: u32 = 1 << 24;
    /// The CPU's own APIC virtualization, which reads and writes a backing
    /// page and two tables at physical addresses the VMCB gives it.
    pub const X2AVIC_ENABLE: u32 = 1 << 30;
    pub const AVIC_ENABLE: u32 = 1 << 31;
}

/// `Control::int_state`.
pub mod int_state {
    pub const SHADOW: u32 = 1 << 0;
}

/// `Control::nested_ctl`.
pub mod nested {
    pub const NP_ENABLE: u64 = 1 << 0;
}

/// The format of `event_inj` and `exit_int_info`.
pub mod event {
    pub const VECTOR_MASK: u64 = 0xFF;
    pub const TYPE_SHIFT: u64 = 8;
    pub const TYPE_MASK: u64 = 0x7 << TYPE_SHIFT;
    pub const TYPE_INTR: u64 = 0 << TYPE_SHIFT;
    pub const TYPE_NMI: u64 = 2 << TYPE_SHIFT;
    pub const TYPE_EXCEPTION: u64 = 3 << TYPE_SHIFT;
    pub const TYPE_SOFT_INT: u64 = 4 << TYPE_SHIFT;
    pub const ERROR_VALID: u64 = 1 << 11;
    pub const VALID: u64 = 1 << 31;
    pub const ERROR_SHIFT: u64 = 32;
}

/// `Control::exit_code`: why the guest stopped (appendix C).
pub mod exit {
    pub const EXCP_BASE: u64 = 0x040;
    pub const EXCP_LAST: u64 = 0x05F;
    pub const INTR: u64 = 0x060;
    pub const NMI: u64 = 0x061;
    pub const SMI: u64 = 0x062;
    pub const INIT: u64 = 0x063;
    pub const VINTR: u64 = 0x064;
    pub const RDTSC: u64 = 0x06E;
    pub const RDPMC: u64 = 0x06F;
    pub const CPUID: u64 = 0x072;
    pub const RSM: u64 = 0x073;
    pub const INVD: u64 = 0x076;
    pub const PAUSE: u64 = 0x077;
    pub const HLT: u64 = 0x078;
    pub const INVLPG: u64 = 0x079;
    pub const INVLPGA: u64 = 0x07A;
    pub const IOIO: u64 = 0x07B;
    pub const MSR: u64 = 0x07C;
    pub const TASK_SWITCH: u64 = 0x07D;
    pub const FERR_FREEZE: u64 = 0x07E;
    pub const SHUTDOWN: u64 = 0x07F;
    pub const VMRUN: u64 = 0x080;
    pub const VMMCALL: u64 = 0x081;
    pub const VMLOAD: u64 = 0x082;
    pub const VMSAVE: u64 = 0x083;
    pub const STGI: u64 = 0x084;
    pub const CLGI: u64 = 0x085;
    pub const SKINIT: u64 = 0x086;
    pub const RDTSCP: u64 = 0x087;
    pub const ICEBP: u64 = 0x088;
    pub const WBINVD: u64 = 0x089;
    pub const MONITOR: u64 = 0x08A;
    pub const MWAIT: u64 = 0x08B;
    pub const MWAIT_ARMED: u64 = 0x08C;
    pub const XSETBV: u64 = 0x08D;
    pub const RDPRU: u64 = 0x08E;
    pub const NPF: u64 = 0x400;
    /// `vmrun` refused the VMCB: a consistency check failed, and the CPU
    /// says no more than that.
    pub const INVALID: u64 = u64::MAX;
}

/// `exit_info1` of an IOIO intercept.
pub mod ioio {
    /// Set for IN, clear for OUT.
    pub const IN: u64 = 1 << 0;
    pub const STRING: u64 = 1 << 2;
    pub const REP: u64 = 1 << 3;
    pub const SZ8: u64 = 1 << 4;
    pub const SZ16: u64 = 1 << 5;
    pub const SZ32: u64 = 1 << 6;
    pub const PORT_SHIFT: u64 = 16;
}

/// `exit_info1` of a nested page fault: the error code.
pub mod npf {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITE: u64 = 1 << 1;
    pub const USER: u64 = 1 << 2;
    pub const RESERVED: u64 = 1 << 3;
    pub const FETCH: u64 = 1 << 4;
    /// The access was to the final guest physical address.
    pub const FINAL: u64 = 1 << 32;
    /// The access was the guest's own page-table walk.
    pub const TABLE_WALK: u64 = 1 << 33;
}

/// Sizes of the two permission maps.
pub const IOPM_BYTES: usize = 12 * 1024;
pub const MSRPM_BYTES: usize = 8 * 1024;
