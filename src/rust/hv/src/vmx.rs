//! A guest's CPU under Intel VT-x: the VMCS as this hypervisor fills it in
//! (through `hvarch`), the exits it comes back with, and the state it starts
//! in -- the same policy the AMD-V [`crate::svm`] module is, over a different
//! machine underneath. What keeps the host's CPU the host's is set by
//! `hvarch::x86::vmx::Guest` on every entry whatever this wrote; here is only
//! which instructions a guest is stopped at and how its exits are read.

use core::fmt::Write;

use hvarch::x86::vmx::{vmcs, Guest};
use hvarch::x86::svm::vmcb::{attrib, Save, Segment};
use hvarch::x86::svm::{GuestRegs, Kick, NotRun};
use hvarch::{Caps, Result};

use crate::svm::{Exit, Io, LongMode};
use crate::vm::Refusal;

/* Control-register and EFER bits a guest's starting state is made of. The
 * AMD side exports these; reuse them, since a guest's CR0 is a guest's CR0. */
use crate::svm::{CR0_ET, CR0_MP, CR0_NE, CR0_PE, CR0_PG, CR0_WP, CR4_MCE, CR4_PAE, EFER_LMA, EFER_LME};

/// Exit codes this decoder hands the run loop for instructions it does not
/// answer itself, in the AMD-V namespace the loop already speaks -- so one
/// set of `Exit::Other` arms serves both backends.
use hvarch::x86::svm::vmcb::exit as svm_exit;
/// Nested-fault error bits, likewise the AMD-V encoding the loop reads.
use hvarch::x86::svm::vmcb::npf;

const RFLAGS_RESERVED_ONE: u64 = 1 << 1;
const DR6_RESET: u64 = 0xFFFF_0FF0;
const DR7_RESET: u64 = 0x400;
/// The PAT every x86 CPU comes out of reset with.
const PAT_RESET: u64 = 0x0007_0406_0007_0406;

/// Exceptions that push an error code, which VMX leaves in the exit's
/// interruption error-code field -- but it also tells us with a bit, so this
/// is only for injection.
const VECTOR_MC: u8 = 18;

/// One guest CPU under VT-x.
pub struct Vcpu {
    guest: Guest,
    /// The guest's task-priority register, CR8: a shadow, since the real
    /// one is the host's local APIC's, and the guest is given no APIC for
    /// the value to mean anything to. A `mov` to or from CR8 stops the
    /// guest and is answered from here.
    tpr: u8,
}

/// What CR8 can hold: bits 3:0. A `mov` of more into it is a #GP.
const TPR_MASK: u64 = 0xF;

impl Vcpu {
    /// A CPU that stops at the instructions a guest of this hypervisor may
    /// not run unseen, and at `exceptions` -- one bit per vector.
    pub fn new(caps: &Caps, exceptions: u32) -> Result<Self> {
        let vmx = match &caps.detail {
            hvarch::x86::Detail::Vmx(v) => v,
            _ => return Err(hvarch::Error::NoExtension),
        };
        let guest = Guest::new(vmx, exceptions)?;
        Ok(Self { guest, tpr: 0 })
    }

    pub fn save(&self) -> &Save {
        self.guest.save()
    }
    pub fn save_mut(&mut self) -> &mut Save {
        self.guest.save_mut()
    }
    pub fn regs(&self) -> &GuestRegs {
        self.guest.regs()
    }
    pub fn regs_mut(&mut self) -> &mut GuestRegs {
        self.guest.regs_mut()
    }
    pub fn save_and_regs_mut(&mut self) -> (&mut Save, &mut GuestRegs) {
        self.guest.save_and_regs_mut()
    }

    /// The VMX backend keeps no ASID or profile of its own yet; the shape of
    /// the bench and asid checks is the AMD side's. These make the surface
    /// the run loop calls one for both.
    pub fn set_flush_always(&mut self, _on: bool) {}
    pub fn set_profile(&mut self, _on: bool) {}
    pub fn profile(&self) -> Option<hvarch::x86::svm::Profile> {
        None
    }
    pub fn asid(&self) -> Option<u32> {
        None
    }

    /// Long mode at CPL 0 with paging on: the state a 64-bit kernel is handed
    /// by a boot loader. The same shape as the AMD side's, into the shadow
    /// save area `hvarch` syncs into the VMCS.
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
        /* No SVME: this is VMX. Long mode is LME and LMA. */
        s.efer = EFER_LME | EFER_LMA;
        s.cr0 = CR0_PE | CR0_MP | CR0_ET | CR0_NE | CR0_WP | CR0_PG;
        s.cr3 = l.cr3;
        s.cr4 = CR4_PAE | CR4_MCE;
        s.dr6 = DR6_RESET;
        s.dr7 = DR7_RESET;
        s.rflags = RFLAGS_RESERVED_ONE;
        s.rip = l.entry;
        s.rsp = l.stack;
        s.rax = 0;
        s.g_pat = PAT_RESET;
        /* The whole shadow was just set: the next entry loads all of it,
         * not only the fields a policy changes between entries. */
        self.guest.mark_full_sync();
    }

    /// Enter the guest and come back at its next exit, decoded. The exit is
    /// the neutral [`Exit`]; a VM-entry the CPU refused is [`Exit::Invalid`].
    pub fn enter(&mut self, nested: hvarch::x86::svm::Nested, host_areas: &[core::sync::atomic::AtomicU64], kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        let cpu = match unsafe { self.guest.run(nested, host_areas, kick) } {
            Ok(cpu) => cpu,
            Err(NotRun::Kicked { cpu }) => return Ok((Exit::Kicked, cpu)),
            Err(NotRun::Off { cpu }) => return Err(Refusal::NotOn(cpu)),
            Err(NotRun::FiveLevelPaging { cpu }) => return Err(Refusal::FiveLevelPaging(cpu)),
            Err(NotRun::Flush { cpu }) => return Err(Refusal::Flush(cpu)),
        };
        if self.guest.entry_failed() {
            /* VMLAUNCH itself failed: the guest never started, and the
             * instruction error says why -- a consistency check this
             * hypervisor got wrong, reported like AMD-V's VMEXIT_INVALID. */
            return Ok((Exit::Invalid, cpu));
        }
        self.requeue_event();
        Ok((self.exit(), cpu))
    }

    /// Why the guest stopped, from what the exit wrote.
    pub fn exit(&self) -> Exit {
        let g = &self.guest;
        let reason = g.exit_reason();
        if reason & vmcs::reason::ENTRY_FAILURE != 0 {
            /* A VM-entry failure that still took the exit path: bad guest
             * state, an MSR load, a machine check on entry. */
            return Exit::Invalid;
        }
        use vmcs::reason as r;
        match reason & vmcs::reason::BASIC_MASK {
            r::EXTERNAL_INTERRUPT | r::INIT | r::SIPI | r::NMI_WINDOW => Exit::Host,
            r::EXCEPTION_NMI => {
                let info = g.exit_intr_info();
                if info & vmcs::intr::VALID == 0 {
                    return Exit::Host;
                }
                if info & vmcs::intr::TYPE_MASK == vmcs::intr::TYPE_NMI {
                    /* An NMI, which is the host's -- as INTR is. */
                    return Exit::Host;
                }
                let vector = (info & vmcs::intr::VECTOR_MASK) as u8;
                if vector == VECTOR_MC {
                    return Exit::MachineCheck;
                }
                let error = (info & vmcs::intr::DELIVER_ERRCODE != 0).then(|| g.exit_intr_errcode());
                Exit::Exception { vector, error }
            }
            r::IO_INSTRUCTION => {
                let q = g.exit_qualification();
                use vmcs::io;
                let size = ((q & io::SIZE_MASK) + 1) as u8;
                Exit::Io(Io {
                    port: (q >> io::PORT_SHIFT) as u16,
                    size,
                    input: q & io::IN != 0,
                    string: q & io::STRING != 0,
                    rep: q & io::REP != 0,
                    next_rip: g.save().rip + g.exit_instr_len() as u64,
                })
            }
            r::HLT => Exit::Hlt,
            r::CPUID => Exit::Cpuid,
            r::RDMSR => Exit::Msr { write: false },
            r::WRMSR => Exit::Msr { write: true },
            r::VMCALL => Exit::Hypercall,
            r::INTERRUPT_WINDOW => Exit::IrqWindow,
            r::CR_ACCESS => {
                /* CR8 alone is intercepted by choice; the other exit this
                 * reason can be is a `mov` to CR4 setting VMXE, which the
                 * guest was told it has not got, and which stops it. */
                let q = g.exit_qualification();
                use vmcs::cr_access as cr;
                let kind = q & cr::TYPE_MASK;
                if q & cr::CR_MASK == 8 && (kind == cr::MOV_TO_CR || kind == cr::MOV_FROM_CR) {
                    Exit::Cr8 {
                        write: kind == cr::MOV_TO_CR,
                        gpr: ((q & cr::GPR_MASK) >> cr::GPR_SHIFT) as u8,
                    }
                } else {
                    Exit::Other(r::CR_ACCESS as u64)
                }
            }
            r::EPT_VIOLATION => {
                let q = g.exit_qualification();
                use vmcs::ept_viol;
                /* Into the AMD-V nested-fault encoding the run loop reads:
                 * present is "the page had some permission", the rest the
                 * access that violated. */
                let mut error = 0u64;
                if q & ept_viol::PERM_MASK != 0 {
                    error |= npf::PRESENT;
                }
                if q & ept_viol::WRITE != 0 {
                    error |= npf::WRITE;
                }
                if q & ept_viol::FETCH != 0 {
                    error |= npf::FETCH;
                }
                if q & ept_viol::FINAL != 0 {
                    error |= npf::FINAL;
                }
                Exit::NestedFault { gpa: g.guest_physical_address(), error }
            }
            r::EPT_MISCONFIG => {
                /* A malformed EPT entry -- this hypervisor's own bug, not the
                 * guest's; stop, naming the address. */
                Exit::NestedFault { gpa: g.guest_physical_address(), error: npf::PRESENT }
            }
            r::TRIPLE_FAULT => Exit::Shutdown,
            /* The instructions the run loop answers in one place for both
             * backends: hand it the AMD-V code it already matches. */
            r::WBINVD => Exit::Other(svm_exit::WBINVD),
            r::XSETBV => Exit::Other(svm_exit::XSETBV),
            r::MONITOR => Exit::Other(svm_exit::MONITOR),
            r::MWAIT => Exit::Other(svm_exit::MWAIT),
            r::RDTSCP => Exit::Other(svm_exit::RDTSCP),
            r::RDPMC => Exit::Other(svm_exit::RDPMC),
            other => Exit::Other(other as u64),
        }
    }

    /// Give back an event whose delivery an exit cut short: VMX leaves it in
    /// IDT_VECTORING_INFO, and it is lost unless the next entry injects it.
    /// Not a software interrupt or exception (INT n, INT1, INT3, INTO),
    /// whose RIP still points at the instruction (as on the AMD side).
    pub fn requeue_event(&mut self) {
        use vmcs::intr;
        let info = self.guest.idt_vectoring_info();
        if info & intr::VALID == 0 {
            return;
        }
        let typ = info & intr::TYPE_MASK;
        let vector = info & intr::VECTOR_MASK;
        let software = typ == intr::TYPE_SOFT_INT
            || typ == intr::TYPE_PRIV_SOFT_EXCEPTION
            || typ == intr::TYPE_SOFT_EXCEPTION
            || (typ == intr::TYPE_HW_EXCEPTION && (vector == 3 || vector == 4));
        if software {
            return;
        }
        /* Made from its parts, not copied: the word the exit wrote has an
         * undefined bit 12, and the entry field wants bits 30:12 clear, or
         * the entry fails with nothing named. */
        let mut event = intr::VALID | typ | vector;
        let errcode = if info & intr::DELIVER_ERRCODE != 0 {
            event |= intr::DELIVER_ERRCODE;
            self.guest.idt_vectoring_errcode()
        } else {
            0
        };
        self.guest.set_inject(event, errcode);
    }

    /// Answer the guest's `mov` to or from CR8 from its shadow task-priority
    /// register and step past it: the real CR8 is the host's local APIC's,
    /// and a guest let at it could hold every interrupt but an NMI off the
    /// host's CPU. A value CR8 cannot hold is a #GP, as on the silicon, and
    /// the instruction is not stepped past.
    pub fn cr8_access(&mut self, write: bool, gpr: u8) {
        if write {
            let value = self.gpr(gpr);
            if value & !TPR_MASK != 0 {
                self.inject_gp();
                return;
            }
            self.tpr = value as u8;
        } else {
            self.set_gpr(gpr, u64::from(self.tpr));
        }
        self.skip();
    }

    /// General-purpose register `n`, as the encoding numbers them.
    fn gpr(&self, n: u8) -> u64 {
        let s = self.guest.save();
        let r = self.guest.regs();
        match n {
            0 => s.rax,
            1 => r.rcx,
            2 => r.rdx,
            3 => r.rbx,
            4 => s.rsp,
            5 => r.rbp,
            6 => r.rsi,
            7 => r.rdi,
            8 => r.r8,
            9 => r.r9,
            10 => r.r10,
            11 => r.r11,
            12 => r.r12,
            13 => r.r13,
            14 => r.r14,
            _ => r.r15,
        }
    }

    fn set_gpr(&mut self, n: u8, value: u64) {
        /* RSP the guest keeps in the VMCS: told to the backend, which
         * writes it there at the next entry. */
        if n == 4 {
            self.guest.set_rsp(value);
            return;
        }
        let (s, r) = self.guest.save_and_regs_mut();
        match n {
            0 => s.rax = value,
            1 => r.rcx = value,
            2 => r.rdx = value,
            3 => r.rbx = value,
            5 => r.rbp = value,
            6 => r.rsi = value,
            7 => r.rdi = value,
            8 => r.r8 = value,
            9 => r.r9 = value,
            10 => r.r10 = value,
            11 => r.r11 = value,
            12 => r.r12 = value,
            13 => r.r13 = value,
            14 => r.r14 = value,
            _ => r.r15 = value,
        }
    }

    /// Step past the instruction the guest stopped at: VMX reports its length
    /// for every exit that has one, so there is no per-instruction constant.
    fn skip(&mut self) {
        let len = self.guest.exit_instr_len() as u64;
        let s = self.guest.save_mut();
        s.rip = s.rip.wrapping_add(len);
        /* Out of any interrupt shadow the stepped instruction was in. */
        self.guest.set_interruptibility(0);
    }

    pub fn skip_io(&mut self, io: &Io) {
        let s = self.guest.save_mut();
        s.rip = io.next_rip;
        self.guest.set_interruptibility(0);
    }
    pub fn skip_cpuid(&mut self) {
        self.skip();
    }
    pub fn skip_msr(&mut self) {
        self.skip();
    }
    pub fn skip_vmmcall(&mut self) {
        self.skip();
    }
    pub fn skip_hlt(&mut self) {
        self.skip();
    }
    pub fn skip_wbinvd(&mut self) {
        self.skip();
    }

    /// Whether an event is queued for injection on the next entry.
    pub fn event_queued(&self) -> bool {
        self.guest.inject_valid()
    }

    /// Whether the guest can take a maskable interrupt right now: RFLAGS.IF
    /// set, not in a STI or MOV SS shadow, and nothing already queued.
    pub fn interruptible(&self) -> bool {
        const IF: u64 = 1 << 9;
        let blocked = vmcs::INTR_BLOCK_STI | vmcs::INTR_BLOCK_MOV_SS;
        self.guest.save().rflags & IF != 0
            && self.guest.interruptibility() & blocked == 0
            && !self.guest.inject_valid()
    }

    pub fn inject_extint(&mut self, vector: u8) {
        self.guest.set_inject(vmcs::intr::VALID | vmcs::intr::TYPE_EXTINT | vector as u32, 0);
    }

    pub fn inject_ud(&mut self) {
        const VECTOR_UD: u32 = 6;
        self.guest.set_inject(vmcs::intr::VALID | vmcs::intr::TYPE_HW_EXCEPTION | VECTOR_UD, 0);
    }

    pub fn inject_gp(&mut self) {
        const VECTOR_GP: u32 = 13;
        self.guest.set_inject(
            vmcs::intr::VALID | vmcs::intr::TYPE_HW_EXCEPTION | vmcs::intr::DELIVER_ERRCODE | VECTOR_GP,
            0,
        );
    }

    /// Ask the CPU to exit the moment the guest can take an interrupt: VMX's
    /// interrupt-window control.
    pub fn request_irq_window(&mut self) {
        self.guest.set_irq_window(true);
    }

    pub fn clear_irq_window(&mut self) {
        self.guest.set_irq_window(false);
    }

    /// The guest's state and the last exit, for a report.
    pub fn dump(&self, out: &mut dyn Write) -> core::fmt::Result {
        let g = &self.guest;
        let s = g.save();
        let r = g.regs();
        writeln!(out, "  rip {:#018x}  rsp {:#018x}  rflags {:#x}  cpl {}", s.rip, s.rsp, s.rflags, s.cpl)?;
        writeln!(out, "  rax {:#018x}  rbx {:#018x}  rcx {:#018x}  rdx {:#018x}", s.rax, r.rbx, r.rcx, r.rdx)?;
        writeln!(out, "  rsi {:#018x}  rdi {:#018x}  rbp {:#018x}", r.rsi, r.rdi, r.rbp)?;
        writeln!(out, "  cr0 {:#x}  cr2 {:#x}  cr3 {:#x}  cr4 {:#x}  efer {:#x}", s.cr0, s.cr2, s.cr3, s.cr4, s.efer)?;
        for (name, seg) in [("cs", &s.cs), ("ss", &s.ss), ("ds", &s.ds), ("tr", &s.tr)] {
            writeln!(out, "  {}  {:#06x} attrib {:#05x} limit {:#x} base {:#x}",
                     name, seg.selector, seg.attrib, seg.limit, seg.base)?;
        }
        writeln!(out, "  exit {:#x}  qual {:#x}  intr {:#x}  idtv {:#x}  instr_err {}",
                 g.exit_reason(), g.exit_qualification(), g.exit_intr_info(),
                 g.idt_vectoring_info(), g.vm_instruction_error())?;
        Ok(())
    }
}
