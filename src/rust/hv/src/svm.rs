//! A guest's CPU under AMD-V: the VMCB as this hypervisor fills it in, the
//! exits it comes back with, and what to say when the CPU refuses one.
//!
//! Everything here is policy -- which instructions a guest is stopped at,
//! what state it starts in -- and none of it can hand the guest the host:
//! what keeps the host's CPU the host's, `hvarch::x86::svm::Guest::run` sets
//! on every entry whatever this module wrote.

use core::fmt::Write;

use hvarch::x86::svm::vmcb::{self, attrib, Segment};
use hvarch::x86::svm::{self as arch, Guest, GuestRegs};
use hvarch::{Caps, Result};

/* Control register and EFER bits a guest's starting state is made of. */
pub const CR0_PE: u64 = 1 << 0;
pub const CR0_MP: u64 = 1 << 1;
pub const CR0_ET: u64 = 1 << 4;
pub const CR0_NE: u64 = 1 << 5;
pub const CR0_WP: u64 = 1 << 16;
pub const CR0_NW: u64 = 1 << 29;
pub const CR0_CD: u64 = 1 << 30;
pub const CR0_PG: u64 = 1 << 31;
pub const CR4_PAE: u64 = 1 << 5;
pub const CR4_MCE: u64 = 1 << 6;
pub const EFER_LME: u64 = 1 << 8;
pub const EFER_LMA: u64 = 1 << 10;
pub const EFER_SVME: u64 = 1 << 12;

/// Every bit EFER may have: SCE, LME, LMA, NXE, SVME, LMSLE, FFXSR, TCE.
const EFER_DEFINED: u64 = 0xFD01;
/// Every CR4 bit an AMD part may have set: VME through OSXMMEXCPT, UMIP,
/// LA57, FSGSBASE, PCIDE, OSXSAVE, SMEP, SMAP, PKE, CET. VMXE and SMXE are
/// Intel's and reserved here.
const CR4_DEFINED: u64 = 0x00F7_1FFF;

/// The PAT every x86 CPU comes out of reset with: WB, WT, UC-, UC, twice.
pub const PAT_RESET: u64 = 0x0007_0406_0007_0406;
const RFLAGS_RESERVED_ONE: u64 = 1 << 1;
const DR6_RESET: u64 = 0xFFFF_0FF0;
const DR7_RESET: u64 = 0x400;

/// Lengths of the instructions an intercept stops a guest at, for a CPU
/// without next-RIP save: each intercept is for exactly one instruction, so
/// its length is a constant and not a decoder's answer. A prefix the guest
/// put on one of them is the guest's own undoing, not the host's.
const LEN_HLT: u64 = 1;
const LEN_CPUID: u64 = 2;
const LEN_MSR: u64 = 2;
const LEN_VMMCALL: u64 = 3;
const LEN_WBINVD: u64 = 2;

/// A port access that stopped the guest.
#[derive(Clone, Copy, Debug)]
pub struct Io {
    pub port: u16,
    /// 1, 2 or 4 bytes.
    pub size: u8,
    /// IN rather than OUT.
    pub input: bool,
    /// INS or OUTS, which move memory and not RAX.
    pub string: bool,
    pub rep: bool,
    /// Where the instruction ends: every CPU says so for an I/O intercept.
    pub next_rip: u64,
}

/// Why a guest stopped.
#[derive(Clone, Copy, Debug)]
pub enum Exit {
    /// One of the host's own interrupts -- an IRQ, an NMI, an SMI, an INIT --
    /// took the CPU back. The host has handled it by the time this is read;
    /// the guest goes straight back in.
    Host,
    Io(Io),
    Hlt,
    Cpuid,
    Msr { write: bool },
    /// VMMCALL.
    Hypercall,
    /// The guest reached a point where it could take a virtual interrupt --
    /// the interrupt window this hypervisor asked for (`request_irq_window`).
    IrqWindow,
    /// The guest touched a guest physical address with no memory behind it
    /// -- or with memory it may not use that way.
    NestedFault { gpa: u64, error: u64 },
    Exception { vector: u8, error: Option<u32> },
    /// A machine check, taken while the guest ran: the host's hardware
    /// reporting an error, stopped on its way into the guest.
    MachineCheck,
    /// The guest triple-faulted.
    Shutdown,
    /// `vmrun` refused the VMCB.
    Invalid,
    Other(u64),
}

/// Exceptions that push an error code, which an intercept of them leaves
/// in EXITINFO1.
const HAS_ERROR_CODE: u32 = (1 << 8) | (1 << 10) | (1 << 11) | (1 << 12) | (1 << 13)
    | (1 << 14) | (1 << 17) | (1 << 21) | (1 << 29) | (1 << 30);

/// One guest CPU.
pub struct Vcpu {
    guest: Guest,
    /// The CPU writes the next instruction's address into the VMCB itself.
    nrip: bool,
}

/// What a long-mode guest starts from: its page table, a GDT whose entries
/// match the segments below, a TSS, and where to begin.
pub struct LongMode {
    pub entry: u64,
    pub stack: u64,
    pub cr3: u64,
    pub gdt: u64,
    pub gdt_limit: u16,
    /// The IDT is at 0 with this limit; 0 for none at all, where the first
    /// interrupt or exception the guest does not have intercepted is a
    /// triple fault.
    pub idt_limit: u16,
    pub code_selector: u16,
    pub data_selector: u16,
    pub tss_selector: u16,
    pub tss: u64,
}

impl Vcpu {
    /// A CPU that stops at the instructions a guest of this hypervisor is
    /// never allowed to run unseen, and at `exceptions` -- one bit per
    /// vector, for a guest whose every fault is the host's business.
    pub fn new(caps: &Caps, exceptions: u32) -> Result<Self> {
        let nrip = match &caps.detail {
            hvarch::x86::Detail::Svm(svm) => svm.has(arch::NRIP_SAVE),
            _ => false,
        };
        let mut guest = Guest::new()?;
        let c = &mut guest.vmcb_mut().control;
        use vmcb::intercept::{misc1, misc2};
        c.intercept_misc1 = misc1::CPUID | misc1::HLT | misc1::RDPMC | misc1::RSM
            | misc1::TASK_SWITCH | misc1::FERR_FREEZE;
        c.intercept_misc2 = misc2::VMMCALL | misc2::RDTSCP | misc2::ICEBP | misc2::WBINVD
            | misc2::MONITOR | misc2::MWAIT | misc2::MWAIT_ARMED | misc2::XSETBV | misc2::RDPRU;
        c.intercept_exceptions = exceptions;
        /* Any identifier but the host's 0: every entry flushes the TLB (see
         * `Guest::run`), so no two guests can meet in one. */
        c.guest_asid = 1;
        Ok(Self { guest, nrip })
    }

    pub fn save(&self) -> &vmcb::Save {
        &self.guest.vmcb().save
    }

    pub fn save_mut(&mut self) -> &mut vmcb::Save {
        &mut self.guest.vmcb_mut().save
    }

    pub fn control(&self) -> &vmcb::Control {
        &self.guest.vmcb().control
    }

    pub fn regs(&self) -> &GuestRegs {
        self.guest.regs()
    }

    pub fn regs_mut(&mut self) -> &mut GuestRegs {
        self.guest.regs_mut()
    }

    pub fn save_and_regs_mut(&mut self) -> (&mut vmcb::Save, &mut GuestRegs) {
        self.guest.save_and_regs_mut()
    }

    pub(crate) fn guest_mut(&mut self) -> &mut Guest {
        &mut self.guest
    }

    pub fn next_rip_saved(&self) -> bool {
        self.nrip
    }

    /// Long mode at CPL 0 with paging on: the state a 64-bit kernel is
    /// handed by a boot loader that has done the work of getting there.
    pub fn long_mode(&mut self, l: &LongMode) {
        let flat = 0xFFFF_FFFF;
        let code = Segment {
            selector: l.code_selector,
            attrib: attrib::P | attrib::S | attrib::CODE | attrib::WRITE_OR_READ | attrib::ACCESSED
                | attrib::L | attrib::G,
            limit: flat,
            base: 0,
        };
        let data = Segment {
            selector: l.data_selector,
            attrib: attrib::P | attrib::S | attrib::WRITE_OR_READ | attrib::ACCESSED | attrib::DB | attrib::G,
            limit: flat,
            base: 0,
        };
        let s = self.save_mut();
        s.cs = code;
        s.ds = data;
        s.es = data;
        s.ss = data;
        s.fs = data;
        s.gs = data;
        s.gdtr = Segment { limit: l.gdt_limit as u32, base: l.gdt, ..Segment::default() };
        s.idtr = Segment { limit: l.idt_limit as u32, ..Segment::default() };
        s.ldtr = Segment { attrib: attrib::P | attrib::TYPE_LDT, limit: 0xFFFF, ..Segment::default() };
        s.tr = Segment {
            selector: l.tss_selector,
            attrib: attrib::P | attrib::TYPE_TSS64_BUSY,
            limit: 0x67,
            base: l.tss,
        };
        s.cpl = 0;
        s.efer = EFER_SVME | EFER_LME | EFER_LMA;
        s.cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_PG;
        s.cr3 = l.cr3;
        /* MCE, as every kernel sets it: with it clear, a machine check
         * while the guest runs is a shutdown, and would be reported as the
         * guest's triple fault. */
        s.cr4 = CR4_PAE | CR4_MCE;
        s.dr6 = DR6_RESET;
        s.dr7 = DR7_RESET;
        s.rflags = RFLAGS_RESERVED_ONE;
        s.rip = l.entry;
        s.rsp = l.stack;
        s.rax = 0;
        s.g_pat = PAT_RESET;
    }

    /// Why the guest stopped, from what `#vmexit` wrote.
    pub fn exit(&self) -> Exit {
        let c = self.control();
        match c.exit_code {
            vmcb::exit::INTR | vmcb::exit::NMI | vmcb::exit::SMI | vmcb::exit::INIT => Exit::Host,
            vmcb::exit::IOIO => {
                let info = c.exit_info1;
                use vmcb::ioio;
                let size = if info & ioio::SZ8 != 0 {
                    1
                } else if info & ioio::SZ16 != 0 {
                    2
                } else {
                    4
                };
                Exit::Io(Io {
                    port: (info >> ioio::PORT_SHIFT) as u16,
                    size,
                    input: info & ioio::IN != 0,
                    string: info & ioio::STRING != 0,
                    rep: info & ioio::REP != 0,
                    next_rip: c.exit_info2,
                })
            }
            vmcb::exit::HLT => Exit::Hlt,
            vmcb::exit::CPUID => Exit::Cpuid,
            vmcb::exit::MSR => Exit::Msr { write: c.exit_info1 & 1 != 0 },
            vmcb::exit::VMMCALL => Exit::Hypercall,
            vmcb::exit::VINTR => Exit::IrqWindow,
            vmcb::exit::NPF => Exit::NestedFault { gpa: c.exit_info2, error: c.exit_info1 },
            vmcb::exit::SHUTDOWN => Exit::Shutdown,
            vmcb::exit::INVALID => Exit::Invalid,
            code if (vmcb::exit::EXCP_BASE..=vmcb::exit::EXCP_LAST).contains(&code) => {
                let vector = (code - vmcb::exit::EXCP_BASE) as u32;
                if vector == arch::VECTOR_MC {
                    Exit::MachineCheck
                } else {
                    Exit::Exception {
                        vector: vector as u8,
                        error: (HAS_ERROR_CODE & (1 << vector) != 0).then_some(c.exit_info1 as u32),
                    }
                }
            }
            code => Exit::Other(code),
        }
    }

    /// Give back to the guest an event the exit interrupted on its way in:
    /// an interrupt, an NMI or an exception whose delivery was cut short by
    /// the exit -- the host's interrupt arriving mid-delivery, a nested
    /// fault on the IDT or the stack -- is in `exit_int_info`, and lost
    /// unless the next entry injects it. The two fields have one format.
    ///
    /// Not a software interrupt, nor INT3 or INTO: the guest's RIP still
    /// points at the instruction that raised it, and running it again raises
    /// it again. (Retrying is what KVM did for years; it is wrong only for a
    /// code breakpoint on the instruction, which then fires twice.)
    ///
    /// Called after every exit. An exit that is itself an exception the
    /// host means to give the guest, with another event in `exit_int_info`,
    /// is the policy's to combine by the double-fault rules; nothing does
    /// that yet, and the built-in guests stop at any exception.
    pub fn requeue_event(&mut self) {
        use vmcb::event;
        let c = &mut self.guest.vmcb_mut().control;
        c.event_inj = 0;
        let info = c.exit_int_info;
        if info & event::VALID == 0 {
            return;
        }
        let kind = info & event::TYPE_MASK;
        let vector = info & event::VECTOR_MASK;
        let software = kind == event::TYPE_SOFT_INT
            || (kind == event::TYPE_EXCEPTION && (vector == 3 || vector == 4));
        if !software {
            c.event_inj = info;
        }
    }

    /// Step past the instruction the guest stopped at, `len` bytes long --
    /// or to wherever the CPU said it ends, when it says. Stepping past an
    /// instruction also steps out of the interrupt shadow it may have been
    /// in: the one instruction after STI has now run.
    pub fn skip(&mut self, len: u64) {
        let nrip = self.nrip;
        let v = self.guest.vmcb_mut();
        v.save.rip = if nrip { v.control.next_rip } else { v.save.rip.wrapping_add(len) };
        v.control.int_state &= !vmcb::int_state::SHADOW;
    }

    /// Step past an I/O instruction: its end is always known.
    pub fn skip_io(&mut self, io: &Io) {
        let v = self.guest.vmcb_mut();
        v.save.rip = io.next_rip;
        v.control.int_state &= !vmcb::int_state::SHADOW;
    }

    pub fn skip_hlt(&mut self) {
        self.skip(LEN_HLT);
    }

    pub fn skip_cpuid(&mut self) {
        self.skip(LEN_CPUID);
    }

    pub fn skip_msr(&mut self) {
        self.skip(LEN_MSR);
    }

    pub fn skip_vmmcall(&mut self) {
        self.skip(LEN_VMMCALL);
    }

    /// Step past a WBINVD, which every x86 CPU has and a guest may run: the
    /// guest's caches are the host's, coherent, and with no device of its
    /// own doing DMA there is nothing its flush would be for. (WBNOINVD is the
    /// same instruction with an F3 prefix, a byte longer, and only next-RIP
    /// save tells them apart; the CPUID policy does not offer it.)
    pub fn skip_wbinvd(&mut self) {
        self.skip(LEN_WBINVD);
    }

    /// Whether an event is queued for injection on the next entry -- one
    /// given back from `exit_int_info` by [`requeue_event`](Self::requeue_event),
    /// or one the policy injected. A vCPU with one has somewhere to go, halted
    /// or not.
    pub fn event_queued(&self) -> bool {
        self.guest.vmcb().control.event_inj & vmcb::event::VALID != 0
    }

    /// Whether the guest can take a maskable interrupt right now: its
    /// RFLAGS.IF is set, it is not in the shadow of a STI or MOV SS, and no
    /// event is already queued for injection.
    pub fn interruptible(&self) -> bool {
        let v = self.guest.vmcb();
        const IF: u64 = 1 << 9;
        v.save.rflags & IF != 0
            && v.control.int_state & vmcb::int_state::SHADOW == 0
            && v.control.event_inj & vmcb::event::VALID == 0
    }

    /// Inject an external interrupt of `vector` on the next entry: what the
    /// PIC hands the CPU when it takes an IRQ. Only when [`interruptible`]
    /// (`Self::interruptible`) says it may be taken.
    pub fn inject_extint(&mut self, vector: u8) {
        use vmcb::event;
        self.guest.vmcb_mut().control.event_inj =
            event::VALID | event::TYPE_INTR | vector as u64;
    }

    /// Ask the CPU to exit as soon as the guest could take an interrupt --
    /// its IF is set and it is out of any shadow -- so a pending IRQ that
    /// cannot be injected now is injected the moment it can. Uses SVM's
    /// virtual-interrupt mechanism: a virtual IRQ the guest never really
    /// takes (its vector is irrelevant, the VINTR intercept fires first),
    /// masked by the guest's own IF because `V_INTR_MASKING` is set.
    pub fn request_irq_window(&mut self) {
        use vmcb::{int_ctl, intercept};
        let c = &mut self.guest.vmcb_mut().control;
        c.intercept_misc1 |= intercept::misc1::VINTR;
        /* V_IRQ with a priority the TPR does not mask (V_IGN_TPR), so the
         * only thing holding it is the guest's IF -- which is what we want
         * to be told about. */
        c.int_ctl = (c.int_ctl & !int_ctl::V_TPR_MASK) | int_ctl::V_IRQ | int_ctl::V_IGN_TPR;
    }

    /// Take the interrupt-window request back once it is no longer needed.
    pub fn clear_irq_window(&mut self) {
        use vmcb::{int_ctl, intercept};
        let c = &mut self.guest.vmcb_mut().control;
        c.intercept_misc1 &= !intercept::misc1::VINTR;
        c.int_ctl &= !int_ctl::V_IRQ;
    }

    /// Inject an invalid-opcode fault on the next entry: what a CPU raises for
    /// an instruction it does not have, and so the answer to one the guest
    /// was told by CPUID is not there -- MONITOR, MWAIT, RDTSCP, RDPRU,
    /// XSETBV -- and runs anyway. The guest's RIP stays on the instruction.
    pub fn inject_ud(&mut self) {
        use vmcb::event;
        const VECTOR_UD: u64 = 6;
        self.guest.vmcb_mut().control.event_inj = event::VALID | event::TYPE_EXCEPTION | VECTOR_UD;
    }

    /// Inject a general-protection fault into the guest on the next entry:
    /// what a real CPU raises for a reserved MSR or an instruction the guest
    /// may not run. `#GP` pushes an error code, 0 here. The guest's RIP is
    /// left where it faulted, not stepped past -- the faulting instruction
    /// did not complete.
    pub fn inject_gp(&mut self) {
        use vmcb::event;
        const VECTOR_GP: u64 = 13;
        self.guest.vmcb_mut().control.event_inj =
            event::VALID | event::TYPE_EXCEPTION | event::ERROR_VALID | VECTOR_GP;
    }

    /// What `vmrun` would refuse this VMCB for, if anything -- the checks of
    /// the manual's "canonicalization and consistency checks" that a VMCB
    /// filled in here could fail, and a few of this hypervisor's own.
    ///
    /// The CPU's own answer is `VMEXIT_INVALID` and nothing more: no field,
    /// no rule. So every entry asks this first, and a VMCB it finds wrong
    /// is never handed to the CPU -- the refusal names the rule instead.
    pub fn check(&self) -> core::result::Result<(), &'static str> {
        let v = self.guest.vmcb();
        let s = &v.save;
        if s.efer & EFER_SVME == 0 {
            return Err("EFER.SVME is clear");
        }
        if s.efer & !EFER_DEFINED != 0 {
            return Err("EFER has a reserved bit set");
        }
        if s.cr0 >> 32 != 0 {
            return Err("CR0 has a bit set above 31");
        }
        if s.cr0 & CR0_NW != 0 && s.cr0 & CR0_CD == 0 {
            return Err("CR0.NW is set without CR0.CD");
        }
        if s.cr3 >> 52 != 0 {
            return Err("CR3 has a bit set above 51");
        }
        if s.cr4 & !CR4_DEFINED != 0 {
            return Err("CR4 has a reserved bit set");
        }
        if s.dr6 >> 32 != 0 || s.dr7 >> 32 != 0 {
            return Err("DR6 or DR7 has a bit set above 31");
        }
        let lme = s.efer & EFER_LME != 0;
        let pg = s.cr0 & CR0_PG != 0;
        if lme && pg && s.cr4 & CR4_PAE == 0 {
            return Err("EFER.LME and CR0.PG are set and CR4.PAE is not");
        }
        if lme && pg && s.cr0 & CR0_PE == 0 {
            return Err("EFER.LME and CR0.PG are set and CR0.PE is not");
        }
        if lme && pg && s.cr4 & CR4_PAE != 0 && s.cs.attrib & attrib::L != 0 && s.cs.attrib & attrib::DB != 0 {
            return Err("CS is 64-bit code with D set");
        }
        if (s.efer & EFER_LMA != 0) != (lme && pg) {
            return Err("EFER.LMA is not EFER.LME and CR0.PG");
        }
        for i in 0..8 {
            if !matches!((s.g_pat >> (i * 8)) & 0xFF, 0 | 1 | 4 | 5 | 6 | 7) {
                return Err("G_PAT has an entry that is not a memory type");
            }
        }
        if v.control.guest_asid == 0 {
            return Err("the ASID is the host's");
        }
        if v.control.event_inj & vmcb::event::VALID != 0 {
            use vmcb::event;
            let kind = v.control.event_inj & event::TYPE_MASK;
            let vector = v.control.event_inj & event::VECTOR_MASK;
            if !matches!(kind, event::TYPE_INTR | event::TYPE_NMI | event::TYPE_EXCEPTION | event::TYPE_SOFT_INT) {
                return Err("the injected event is of a reserved type");
            }
            if kind == event::TYPE_EXCEPTION && (vector == 2 || vector > 31) {
                return Err("the injected exception is not an exception");
            }
        }
        Ok(())
    }

    /// The guest's state and the last exit, for a report: what to read
    /// first when a guest did not do what it was told.
    pub fn dump(&self, out: &mut dyn Write) -> core::fmt::Result {
        let v = self.guest.vmcb();
        let s = &v.save;
        let c = &v.control;
        let r = self.guest.regs();
        writeln!(out, "  rip {:#018x}  rsp {:#018x}  rflags {:#x}  cpl {}", s.rip, s.rsp, s.rflags, s.cpl)?;
        writeln!(out, "  rax {:#018x}  rbx {:#018x}  rcx {:#018x}  rdx {:#018x}", s.rax, r.rbx, r.rcx, r.rdx)?;
        writeln!(out, "  rsi {:#018x}  rdi {:#018x}  rbp {:#018x}", r.rsi, r.rdi, r.rbp)?;
        writeln!(out, "  cr0 {:#x}  cr2 {:#x}  cr3 {:#x}  cr4 {:#x}  efer {:#x}", s.cr0, s.cr2, s.cr3, s.cr4, s.efer)?;
        for (name, seg) in [("cs", &s.cs), ("ss", &s.ss), ("ds", &s.ds), ("tr", &s.tr)] {
            writeln!(out, "  {}  {:#06x} attrib {:#05x} limit {:#x} base {:#x}",
                     name, seg.selector, seg.attrib, seg.limit, seg.base)?;
        }
        writeln!(out, "  gdtr base {:#x} limit {:#x}  idtr base {:#x} limit {:#x}",
                 s.gdtr.base, s.gdtr.limit, s.idtr.base, s.idtr.limit)?;
        writeln!(out, "  exit {:#x}  info1 {:#x}  info2 {:#x}  intinfo {:#x}  int_state {:#x}",
                 c.exit_code, c.exit_info1, c.exit_info2, c.exit_int_info, c.int_state)?;
        Ok(())
    }
}
