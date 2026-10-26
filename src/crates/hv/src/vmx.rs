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

use crate::memory::GuestMemory;
use crate::svm::{Exit, Io, LongMode, ProtectedMode};
use crate::vm::Refusal;

/* Control-register and EFER bits a guest's starting state is made of. The
 * AMD side exports these; reuse them, since a guest's CR0 is a guest's CR0. */
use crate::svm::{
    CR0_CD, CR0_ET, CR0_MP, CR0_NE, CR0_NW, CR0_PE, CR0_PG, CR0_WP, CR4_MCE, CR4_PAE, CR4_PCIDE,
    EFER_LMA, EFER_LME,
};

/// CS's access rights as VMX keeps them: L, the 64-bit code bit, is bit 13.
const AR_L: u32 = 1 << 13;
/// CR0's architectural half: bits 63:32 are reserved, and a write setting
/// one is a #GP.
const CR0_LOW: u64 = 0xFFFF_FFFF;
/// The CR0 bits LMSW loads: PE, MP, EM and TS.
const LMSW_BITS: u64 = 0xF;

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
const VECTOR_DB: u8 = 1;
/// RFLAGS.TF: a single step after every instruction.
const RFLAGS_TF: u64 = 1 << 8;
/// DR6's bits a #DB sets -- B0-B3 -- which a new one clears first; and the
/// bits it reads as 1 when nothing has cleared them, RTM's among them, which
/// is the other way up in the exit's qualification; and bit 12, which DR6
/// has as 0 where the qualification has another meaning for it.
const DR6_TRAP_BITS: u64 = 0xF;
const DR6_ACTIVE_LOW: u64 = 0xFFFF_0FF0;
const DR6_BIT12: u64 = 1 << 12;
/// DR7's general-detect bit, which a #DB's delivery clears.
const DR7_GD: u64 = 1 << 13;

/* The CR4 bits a guest writes without an exit, where its CPUID offers
 * them: each the guest's own to have or not -- its virtual-8086 mode, its
 * TSC and debug extensions at CPL 3, its own paging (a `mov` clearing PAE in
 * long mode is the CPU's own #GP), the global pages Linux turns over at
 * every global TLB flush, RDPMC at CPL 3 (which exits anyway), and its SSE.
 * Every other bit is the host's to keep or to refuse: MCE above all, VMXE,
 * and every bit a feature the guest's CPUID does not offer would let it
 * set -- a `mov` changing one exits, and `crate::cr` answers it. */
const CR4_VME: u64 = 1 << 0;
const CR4_PVI: u64 = 1 << 1;
const CR4_TSD: u64 = 1 << 2;
const CR4_DE: u64 = 1 << 3;
const CR4_PSE: u64 = 1 << 4;
const CR4_PGE: u64 = 1 << 7;
const CR4_PCE: u64 = 1 << 8;
const CR4_OSFXSR: u64 = 1 << 9;
const CR4_OSXMMEXCPT: u64 = 1 << 10;
const CR4_GUEST_OWNS: u64 = CR4_VME | CR4_PVI | CR4_TSD | CR4_DE | CR4_PSE | CR4_PAE | CR4_PGE | CR4_PCE
    | CR4_OSFXSR | CR4_OSXMMEXCPT;

/// One guest CPU under VT-x.
pub struct Vcpu {
    guest: Guest,
    /// The guest's task-priority register, CR8, where the CPU gives it no
    /// virtual APIC (`Guest::has_vapic`): a shadow, since the real one is
    /// the host's local APIC's. A `mov` to or from CR8 then stops the guest
    /// and is answered from here; with a virtual APIC its VTPR is CR8, and
    /// no `mov` of it exits.
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
        /* Of the bits a guest may own, those its CPUID offers it: any other
         * write of one exits, and is the #GP its CPUID promises. And a
         * virtual APIC wherever the CPU has one to give: CR8 the guest's own
         * without an exit, and virtual-interrupt delivery for the run loop
         * to turn on while the guest's APIC is in x2APIC mode. */
        let guest = Guest::new(vmx, exceptions, CR4_GUEST_OWNS & crate::policy::cr4_allowed(), true)?;
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

    /// Read what the last exit left in the VMCS into the shadow, for a
    /// report: an exit reads only what its handling needs, and one that
    /// ends the guest after all -- an EPT violation no device answered --
    /// has the rest read here, while the VMCS is still current on this CPU.
    /// False if it no longer is ([`Guest::read_whole_state`]).
    pub fn read_whole_state(&mut self) -> bool {
        self.guest.read_whole_state()
    }

    /// The bench's flush is the AMD side's -- an entry with no ASID kept;
    /// under VT-x there is no such entry to ask for -- and this makes the
    /// surface one for both.
    pub fn set_flush_always(&mut self, _on: bool) {}

    /// Time every entry from now on, or stop (`hvarch::x86::svm::Profile`,
    /// with its VMCS halves filled in here).
    pub fn set_profile(&mut self, on: bool) {
        self.guest.set_profile(on);
    }
    pub fn profile(&self) -> Option<hvarch::x86::svm::Profile> {
        self.guest.profile()
    }
    /// The VPID the guest's translations are tagged with -- VT-x's ASID,
    /// held for the guest's life rather than handed out per CPU -- or None
    /// on a CPU without them.
    pub fn asid(&self) -> Option<u32> {
        self.guest.vpid().map(u32::from)
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
        *self.guest.debug_addrs_mut() = [0; 4];
        /* The whole shadow was just set: the next entry loads all of it,
         * not only the fields a policy changes between entries. */
        self.guest.mark_full_sync();
    }

    /// The PVH entry's state, as the AMD side's (`crate::svm::protected_mode`)
    /// into the shadow: paging off, which only unrestricted guest runs --
    /// the caller has asked (`crate::vm::Backend::runs_real_mode`).
    pub fn protected_mode(&mut self, p: &ProtectedMode) {
        crate::svm::protected_mode(self.guest.save_mut(), p, 0);
        *self.guest.debug_addrs_mut() = [0; 4];
        self.guest.mark_full_sync();
    }

    /// Enter the guest over `memory`'s nested table and come back at its
    /// next exit, decoded. The exit is the neutral [`Exit`]; a VM-entry the
    /// CPU refused is [`Exit::Invalid`]. The crate's own: a table comes
    /// only from the memory that owns every page it maps, and only
    /// [`crate::vm::Cpu::enter`] calls this -- with the memory the CPU was
    /// made for, which the VMCS's EPT pointer, taken at the first entry,
    /// goes on naming.
    pub(crate) fn enter(&mut self, memory: &GuestMemory, host_areas: &[core::sync::atomic::AtomicU64], kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        /* The table is `memory`'s, which owns every page it maps and only
         * ever adds to it; `host_areas` never names a freed page
         * (`crate::machine`). */
        let cpu = match unsafe { self.guest.run(memory.nested(), host_areas, kick) } {
            Ok(cpu) => cpu,
            Err(NotRun::Kicked { cpu }) => return Ok((Exit::Kicked, cpu)),
            Err(NotRun::Off { cpu }) => return Err(Refusal::NotOn(cpu)),
            Err(NotRun::FiveLevelPaging { cpu }) => return Err(Refusal::FiveLevelPaging(cpu)),
            Err(NotRun::Flush { cpu }) => return Err(Refusal::Flush(cpu)),
            Err(NotRun::Controls { cpu, field, missing }) => return Err(Refusal::Controls { cpu, field, missing }),
            Err(NotRun::Xstate { cpu, xcr0 }) => return Err(Refusal::Xstate { cpu, xcr0 }),
        };
        if self.guest.entry_failed() {
            /* VMLAUNCH itself failed: the guest never started, and the
             * instruction error says why -- a consistency check this
             * hypervisor got wrong, reported like AMD-V's VMEXIT_INVALID. */
            return Ok((Exit::Invalid, cpu));
        }
        self.requeue_event();
        let exit = self.exit();
        if matches!(exit, Exit::Exception { vector: VECTOR_DB, .. }) {
            self.debug_exit();
        }
        Ok((exit, cpu))
    }

    /// A #DB of the guest's own stopped it -- one the run loop gives back,
    /// or stops it over -- whose DR6 the exit did not write: its
    /// qualification has what the CPU would have put there, and it goes
    /// into the guest's DR6 as the CPU would have put it, B0-B3 cleared
    /// first and RTM's bit the other way up (KVM's
    /// `kvm_deliver_exception_payload`). And a single step that came out of
    /// a STI or MOV SS shadow with TF set is marked pending, which the CPU
    /// does at every exit but this one, and the entry checks.
    fn debug_exit(&mut self) {
        use vmcs::debug;
        let payload = self.guest.exit_qualification() & (debug::B0_3 | debug::BD | debug::BS | debug::RTM);
        let shadow = self.guest.interruptibility() & (vmcs::INTR_BLOCK_STI | vmcs::INTR_BLOCK_MOV_SS) != 0;
        let s = self.guest.save_mut();
        s.dr6 = (((s.dr6 & !DR6_TRAP_BITS) | DR6_ACTIVE_LOW | payload) ^ (payload & DR6_ACTIVE_LOW)) & !DR6_BIT12;
        let stepping = s.rflags & RFLAGS_TF != 0;
        if stepping && shadow {
            self.guest.set_pending_single_step();
        }
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
            r::EXTERNAL_INTERRUPT | r::INIT | r::SIPI => Exit::Host,
            r::NMI_WINDOW => Exit::NmiWindow,
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
                /* ICEBP: its #DB exits at the instruction, a privileged
                 * software exception -- the run loop steps past it and gives
                 * the #DB as the trap it is, as for AMD-V's intercept. */
                if vector == VECTOR_DB && info & vmcs::intr::TYPE_MASK == vmcs::intr::TYPE_PRIV_SOFT_EXCEPTION {
                    return Exit::Other(svm_exit::ICEBP);
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
                    /* The guest's RIP plus a length: wrapped, as the CPU
                     * wraps it, where an add that checked would be a host
                     * panic for an `in` at the top of the address space. */
                    next_rip: g.save().rip.wrapping_add(u64::from(g.exit_instr_len())),
                })
            }
            r::HLT => Exit::Hlt,
            r::EOI_INDUCED => Exit::Eoi { vector: (g.exit_qualification() & u64::from(u8::MAX)) as u8 },
            r::PAUSE => Exit::Pause,
            r::CPUID => Exit::Cpuid,
            r::RDMSR => Exit::Msr { write: false },
            r::WRMSR => Exit::Msr { write: true },
            r::VMCALL => Exit::Hypercall,
            /* VMX's own instructions and GETSEC exit in non-root operation
             * whatever the CPL, before the CPU checks it: a guest told it
             * has neither is owed the #UD a CPU without them gives -- not a
             * stop of its whole VM for a user process's `vmxoff`. */
            r::VMCLEAR | r::VMLAUNCH | r::VMPTRLD | r::VMPTRST | r::VMREAD | r::VMRESUME | r::VMWRITE
            | r::VMXOFF | r::VMXON | r::INVEPT | r::INVVPID | r::VMFUNC | r::GETSEC => Exit::Undefined,
            r::INTERRUPT_WINDOW => Exit::IrqWindow,
            r::CR_ACCESS => {
                /* CR8, which is the host's; a `mov` to CR0 or an `lmsw`
                 * changing a bit the host keeps of CR0 (`Guest::set_cr0`);
                 * and a `mov` to CR4 changing one it keeps of CR4 -- MCE, a
                 * bit the guest's CPUID does not offer -- which `crate::cr`
                 * answers. A read of CR4 never exits: the read shadow
                 * answers it. */
                let q = g.exit_qualification();
                use vmcs::cr_access as cr;
                let kind = q & cr::TYPE_MASK;
                let gpr = ((q & cr::GPR_MASK) >> cr::GPR_SHIFT) as u8;
                match (q & cr::CR_MASK, kind) {
                    (8, cr::MOV_TO_CR) | (8, cr::MOV_FROM_CR) => Exit::Cr8 { write: kind == cr::MOV_TO_CR, gpr },
                    (0, cr::MOV_TO_CR) => Exit::Cr0Write { value: self.gpr(gpr) },
                    (4, cr::MOV_TO_CR) => Exit::Cr4 { write: true, gpr: Some(gpr) },
                    (0, cr::LMSW) => {
                        /* The low four bits over CR0's, and PE never cleared
                         * by it: LMSW sets PE and cannot take it back. */
                        let old = g.exit_cr0();
                        let src = (q >> cr::LMSW_SOURCE_SHIFT) & LMSW_BITS;
                        Exit::Cr0Write { value: (old & !LMSW_BITS) | src | (old & CR0_PE) }
                    }
                    _ => Exit::Other(r::CR_ACCESS as u64),
                }
            }
            r::DR_ACCESS => {
                /* Every `mov` of a debug register: the register, which way
                 * and the general-purpose register, all in the exit
                 * (`crate::dr`). */
                let q = g.exit_qualification();
                use vmcs::dr_access as dr;
                Exit::Dr {
                    write: q & dr::FROM_DR == 0,
                    dr: (q & dr::DR_MASK) as u8,
                    gpr: Some(((q & dr::GPR_MASK) >> dr::GPR_SHIFT) as u8),
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
            self.set_cr8(value as u8);
        } else {
            self.set_gpr(gpr, u64::from(self.cr8()));
        }
        self.skip();
    }

    /// The guest's `mov` to CR0 or `lmsw` of `value`, which changes a bit
    /// the host keeps (`Exit::Cr0Write`): checked as the CPU checks one --
    /// a #GP, and the instruction not stepped past, for a value CR0 cannot
    /// hold or a move between modes the architecture forbids -- and done:
    /// long mode switched on or off with paging when EFER.LME asks, which
    /// EFER.LMA and so the "IA-32e mode guest" entry control follow, and CR0
    /// written at the next entry with what the TLB kept dropped, as the
    /// instruction would have dropped it.
    pub fn cr0_write(&mut self, value: u64) {
        let g = &self.guest;
        let old = g.exit_cr0();
        let cr4 = g.exit_cr4();
        let efer = g.save().efer;
        let in_64bit = efer & EFER_LMA != 0 && g.exit_cs_ar() & AR_L != 0;
        /* Outside 64-bit code a `mov` moves 32 bits. */
        let value = if in_64bit { value } else { value & CR0_LOW };
        let pg_on = value & CR0_PG != 0 && old & CR0_PG == 0;
        let pg_off = value & CR0_PG == 0 && old & CR0_PG != 0;
        let refused = value & !CR0_LOW != 0
            || (value & CR0_NW != 0 && value & CR0_CD == 0)
            || (value & CR0_PG != 0 && value & CR0_PE == 0)
            || (pg_on && efer & EFER_LME != 0 && cr4 & CR4_PAE == 0)
            /* Paging off from 64-bit code, or with PCIDs on. */
            || (pg_off && (in_64bit || cr4 & CR4_PCIDE != 0));
        if refused {
            self.inject_gp();
            return;
        }
        let mut efer = efer;
        if pg_on && efer & EFER_LME != 0 {
            efer |= EFER_LMA;
        } else if pg_off {
            efer &= !EFER_LMA;
        }
        self.guest.save_mut().efer = efer;
        self.guest.set_cr0(value);
        self.skip();
    }

    /// Where INIT and then a start-up IPI with `vector` leave a CPU: real
    /// mode at `vector` * 4 KiB (`crate::svm::real_mode`), which VT-x runs
    /// only as unrestricted guest -- `crate::run` refuses a guest of more
    /// than one CPU on a CPU without it. The whole state goes into the VMCS
    /// at the next entry, the "IA-32e mode guest" control off with EFER.LMA,
    /// and what the TLB held under the guest's VPID is dropped.
    pub fn start_at_sipi(&mut self, vector: u8) {
        crate::svm::real_mode(self.guest.save_mut(), vector, 0);
        /* And DR0-DR3 0, as INIT leaves them; DR6 and DR7 `real_mode` set,
         * which the whole state's write puts in. */
        *self.guest.debug_addrs_mut() = [0; 4];
        *self.guest.regs_mut() = crate::svm::reset_regs();
        self.init_reset();
        self.guest.mark_full_sync();
        self.guest.flush_tlb();
    }

    /// What INIT clears beside the registers: an event waiting to be
    /// injected, the interrupt shadow, a request for an interrupt window,
    /// and the task priority.
    pub fn init_reset(&mut self) {
        self.guest.clear_inject();
        self.guest.set_interruptibility(0);
        self.guest.set_irq_window(false);
        self.guest.set_nmi_window(false);
        self.set_cr8(0);
    }

    /// Whether the guest can take an NMI now: its NMI blocking is its own
    /// (virtual NMIs) and clear, it is in no interrupt shadow, and no event
    /// is already queued.
    pub fn nmi_allowed(&self) -> bool {
        let blocked = vmcs::INTR_BLOCK_STI | vmcs::INTR_BLOCK_MOV_SS | vmcs::INTR_BLOCK_NMI;
        self.guest.virtual_nmis() && self.guest.interruptibility() & blocked == 0 && !self.guest.inject_valid()
    }

    /// Inject an NMI on the next entry: the CPU blocks the next until the
    /// guest's IRET.
    pub fn inject_nmi(&mut self) {
        const VECTOR_NMI: u32 = 2;
        self.guest.set_inject(vmcs::intr::VALID | vmcs::intr::TYPE_NMI | VECTOR_NMI, 0);
        self.guest.set_nmi_window(false);
    }

    /// Ask the CPU to exit as soon as the guest could take an NMI.
    pub fn request_nmi_window(&mut self) {
        self.guest.set_nmi_window(true);
    }

    /// Whether this CPU can run a guest in real mode: an application
    /// processor, which a start-up IPI starts there.
    pub fn unrestricted(&self) -> bool {
        self.guest.unrestricted()
    }

    /// The guest's CR8: its virtual APIC's VTPR, bits 7:4, or the shadow its
    /// `mov`s are answered from.
    pub fn cr8(&self) -> u8 {
        if self.guest.has_vapic() {
            self.guest.virtual_tpr() >> 4
        } else {
            self.tpr
        }
    }

    /// Whether the CPU can deliver this guest's interrupts itself
    /// ([`load_apic`](Self::load_apic)).
    pub fn has_vapic(&self) -> bool {
        self.guest.has_vapic()
    }

    /// Whether interrupts posted for this guest CPU can be taken in its
    /// guest, without an exit: a virtual APIC and the CPU's posted-interrupt
    /// processing.
    pub fn posted_capable(&self) -> bool {
        self.guest.posted_capable()
    }

    /// `posted` is where its interrupts are posted, for the CPU to take in
    /// the guest while virtual-interrupt delivery is on: before its first
    /// entry, and true when it took it.
    pub fn set_posted(&mut self, posted: alloc::sync::Arc<hvarch::x86::vmx::PostedInterrupts>) -> bool {
        self.guest.set_posted(posted)
    }

    /// Virtual-interrupt delivery for the next entry, from where the guest's
    /// local APIC stands: what it has requested and in service, the
    /// level-triggered of them -- whose EOIs exit -- and its task priority,
    /// into the virtual APIC; and the CPU delivers what the priorities let
    /// through as the guest can take it, and performs its EOIs, task
    /// priority writes and self-IPIs, without an exit. Nothing without a
    /// virtual APIC.
    pub fn load_apic(&mut self, v: &crate::lapic::Vectors) {
        if !self.guest.has_vapic() {
            return;
        }
        self.guest.set_virtual_irr(&v.irr);
        self.guest.set_virtual_isr(&v.isr);
        self.guest.set_virtual_tpr(v.tpr);
        self.guest.set_eoi_exit(v.eoi_exit());
        self.guest.set_apicv(true);
    }

    /// No virtual-interrupt delivery from the next entry: the guest's APIC
    /// out of x2APIC mode, or off, and its interrupts injected by the run
    /// loop. CR8 stays VTPR.
    pub fn apicv_off(&mut self) {
        self.guest.set_apicv(false);
    }

    /// What virtual-interrupt delivery left in the virtual APIC by the exit,
    /// taken back for the local APIC: VIRR, VISR and VTPR.
    pub fn take_apic(&mut self) -> ([u32; hvarch::x86::vmx::VAPIC_WORDS], [u32; hvarch::x86::vmx::VAPIC_WORDS], u8) {
        (self.guest.virtual_irr(), self.guest.virtual_isr(), self.guest.virtual_tpr())
    }

    /// The CR4 the guest believes it has: the host's bits as it last wrote
    /// them -- MCE among them -- the rest the register's as of its last exit
    /// for a control or a debug register, a fault, or a stop, or the last
    /// write the policy performed.
    pub fn cr4(&self) -> u64 {
        self.guest.save().cr4
    }

    /// Make `value` the guest's CR4, for a write of it `crate::cr`
    /// performed: what it reads from now on, and with MCE the CR4 it runs
    /// with, from its next entry; with `flush`, its TLB flushed there.
    pub fn set_cr4(&mut self, value: u64, flush: bool) {
        self.guest.set_cr4(value, flush);
    }

    /// The CR4 the CPU runs the guest with.
    pub fn cr4_register(&self) -> u64 {
        self.guest.cr4_register()
    }

    /// The CR4 bits the host keeps, whose `mov`s stop the guest.
    pub fn cr4_mask(&self) -> u64 {
        self.guest.cr4_mask()
    }

    /// The guest's DR0-DR3, which its reads of them get (`crate::dr`).
    pub fn debug_addrs(&self) -> [u64; 4] {
        *self.guest.debug_addrs()
    }

    /// Make `addrs` the guest's DR0-DR3, for a write of one performed for
    /// the guest (`crate::dr`): what its reads get from now on, and the
    /// CPU's from its next entry.
    pub fn set_debug_addrs(&mut self, addrs: [u64; 4]) {
        *self.guest.debug_addrs_mut() = addrs;
    }

    /// Make `value` the guest's DR7: the VMCS's, which its next entry loads.
    pub fn set_dr7(&mut self, value: u64) {
        self.guest.set_dr7(value);
    }

    /// Whether every `mov` of a debug register stops the guest -- the
    /// control as its VMCS has it, after its first entry.
    pub fn stops_debug_movs(&self) -> bool {
        self.guest.stops_debug_movs()
    }

    /// The length of the instruction the guest stopped at, as the exit
    /// says it.
    pub fn exit_insn_len(&self) -> u64 {
        u64::from(self.guest.exit_instr_len())
    }

    /// Whether the guest was in 64-bit code at its last exit for a control
    /// or a debug register: long mode active, and CS's L bit.
    pub fn in_64bit_code(&self) -> bool {
        self.guest.save().efer & EFER_LMA != 0 && self.guest.exit_cs_ar() & AR_L != 0
    }

    /// Step past the ICEBP whose #DB stopped the guest at it.
    pub fn skip_icebp(&mut self) {
        self.skip();
    }

    /// CR8 written for the guest -- its APIC's task priority, written
    /// through a register the CPU did not perform: VTPR's bits 7:4, the low
    /// four 0 as a `mov` to CR8 leaves them, or the shadow.
    pub fn set_cr8(&mut self, value: u8) {
        let value = value & TPR_MASK as u8;
        if self.guest.has_vapic() {
            self.guest.set_virtual_tpr(value << 4);
        } else {
            self.tpr = value;
        }
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

    pub fn set_gpr(&mut self, n: u8, value: u64) {
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
        self.leave_shadow();
    }

    pub fn skip_io(&mut self, io: &Io) {
        let s = self.guest.save_mut();
        s.rip = io.next_rip;
        self.leave_shadow();
    }

    /// Step past an instruction the host performed for the guest, `len`
    /// bytes long by its own decoding -- an EPT violation reports no length
    /// -- and out of the interrupt shadow, as `skip` does.
    pub fn skip_emulated(&mut self, len: u64) {
        let s = self.guest.save_mut();
        s.rip = s.rip.wrapping_add(len);
        self.leave_shadow();
    }

    /// Out of any interrupt shadow the stepped instruction was in -- a STI's
    /// or a MOV SS's -- and nothing else: an NMI handler the guest is in it
    /// is in still.
    fn leave_shadow(&mut self) {
        let bits = self.guest.interruptibility() & !(vmcs::INTR_BLOCK_STI | vmcs::INTR_BLOCK_MOV_SS);
        self.guest.set_interruptibility(bits);
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
    pub fn skip_pause(&mut self) {
        self.skip();
    }
    pub fn skip_hlt(&mut self) {
        self.skip();
    }
    pub fn skip_wbinvd(&mut self) {
        self.skip();
    }
    pub fn skip_xsetbv(&mut self) {
        self.skip();
    }

    /// The guest's XCR0, which its entries make the CPU's.
    pub fn xcr0(&self) -> u64 {
        self.guest.xcr0()
    }

    /// Make `value` the guest's XCR0, for an XSETBV of it performed for the
    /// guest: false, and nothing changed, for one a CPU refuses.
    pub fn set_xcr0(&mut self, value: u64) -> bool {
        self.guest.set_xcr0(value)
    }

    /// How many entries put the guest's x87, SSE and AVX registers back into
    /// a CPU that held another's.
    pub fn fp_loads(&self) -> u64 {
        self.guest.fp_loads()
    }

    /// The guest's TSC_AUX, which its RDTSCP and RDPID read.
    pub fn tsc_aux(&self) -> u64 {
        self.guest.tsc_aux()
    }

    /// Make `value` the guest's TSC_AUX, for a WRMSR of it the policy
    /// performed: false, and nothing changed, for one the MSR cannot hold.
    pub fn set_tsc_aux(&mut self, value: u64) -> bool {
        self.guest.set_tsc_aux(value)
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

    /// Give the guest an exception of its own the host intercepted -- #DB,
    /// #AC, intercepted always for the delivery loops a guest could make of
    /// them -- as the CPU would have delivered it: the hardware exception,
    /// with its error code. A #DB's DR6 went into the guest's at its exit
    /// (`debug_exit`), or where general detect raised it (`crate::dr`); its
    /// DR7 loses GD, as a CPU entering a #DB handler clears it -- which an
    /// injected one does not do by itself.
    pub fn reflect_exception(&mut self, vector: u8, error: Option<u32>) {
        if vector == VECTOR_DB {
            let dr7 = self.guest.save().dr7;
            if dr7 & DR7_GD != 0 {
                self.guest.set_dr7(dr7 & !DR7_GD);
            }
        }
        let code = if error.is_some() { vmcs::intr::DELIVER_ERRCODE } else { 0 };
        self.guest.set_inject(
            vmcs::intr::VALID | vmcs::intr::TYPE_HW_EXCEPTION | code | u32::from(vector),
            error.unwrap_or(0),
        );
    }

    /// A general-protection fault on the next entry, its error code 0 --
    /// in protected mode. A CPU in real mode pushes none (an AP between its
    /// start-up IPI and PE), and VT-x refuses an entry that would deliver
    /// one to a guest whose CR0.PE is clear: the guest stopped as `Invalid`
    /// for a #GP it was owed.
    pub fn inject_gp(&mut self) {
        const VECTOR_GP: u32 = 13;
        let code = if self.guest.save().cr0 & CR0_PE != 0 { vmcs::intr::DELIVER_ERRCODE } else { 0 };
        self.guest.set_inject(vmcs::intr::VALID | vmcs::intr::TYPE_HW_EXCEPTION | code | VECTOR_GP, 0);
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
        /* The stack pointer, control registers and segments only when they
         * are this exit's ([`Vcpu::read_whole_state`]): the shadow's older
         * values are no report of the guest. */
        let whole = g.whole_state();
        if whole {
            writeln!(out, "  rip {:#018x}  rsp {:#018x}  rflags {:#x}  cpl {}", s.rip, s.rsp, s.rflags, s.cpl)?;
        } else {
            writeln!(out, "  rip {:#018x}  rflags {:#x}", s.rip, s.rflags)?;
        }
        writeln!(out, "  rax {:#018x}  rbx {:#018x}  rcx {:#018x}  rdx {:#018x}", s.rax, r.rbx, r.rcx, r.rdx)?;
        writeln!(out, "  rsi {:#018x}  rdi {:#018x}  rbp {:#018x}", r.rsi, r.rdi, r.rbp)?;
        if whole {
            writeln!(out, "  cr0 {:#x}  cr2 {:#x}  cr3 {:#x}  cr4 {:#x}  efer {:#x}", s.cr0, s.cr2, s.cr3, s.cr4, s.efer)?;
            for (name, seg) in [("cs", &s.cs), ("ss", &s.ss), ("ds", &s.ds), ("tr", &s.tr)] {
                writeln!(out, "  {}  {:#06x} attrib {:#05x} limit {:#x} base {:#x}",
                         name, seg.selector, seg.attrib, seg.limit, seg.base)?;
            }
        } else {
            writeln!(out, "  rsp, the control registers and the segments not read: the VMCS was no longer current here")?;
        }
        writeln!(out, "  exit {:#x}  qual {:#x}  intr {:#x}  idtv {:#x}  instr_err {}",
                 g.exit_reason(), g.exit_qualification(), g.exit_intr_info(),
                 g.idt_vectoring_info(), g.vm_instruction_error())?;
        Ok(())
    }
}
