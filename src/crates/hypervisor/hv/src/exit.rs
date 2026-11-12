//! Why a guest stopped, as both backends decode it (`crate::svm::Vcpu::exit`,
//! `crate::vmx::Vcpu::exit`) and the run loop answers it: plain data, and
//! the one copy -- the host's fuzzer compiles this file as it is, where it
//! once kept a copy of its own of the enum.

use hvarch::x86::svm::vmcb;

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
    /// Not entered at all: another CPU kicked the vCPU on its way in
    /// (`hvarch::x86::svm::Kick`), and what it has for the guest -- a frame
    /// -- is to be handed over first.
    Kicked,
    Io(Io),
    Hlt,
    Cpuid,
    Msr { write: bool },
    /// VMMCALL.
    Hypercall,
    /// The guest reached a point where it could take a virtual interrupt --
    /// the interrupt window this hypervisor asked for (`request_irq_window`).
    IrqWindow,
    /// The guest may be able to take an NMI now: VT-x's NMI window, or on
    /// AMD-V the IRET that ends the guest's NMI handler.
    NmiWindow,
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
    /// A `mov` to (`write`) or from CR8 -- the task-priority register --
    /// with general-purpose register `gpr` (numbered as the encoding does:
    /// RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8-R15). VT-x stops the guest
    /// there, since the real CR8 is the host's; AMD-V gives the guest a
    /// shadow of its own, and stops it at a write only while an interrupt
    /// the task priority alone holds waits (`request_tpr_window`) -- its
    /// exit naming no register, the instruction decoded for it (`gpr`
    /// None), as for [`Exit::Cr4`].
    Cr8 { write: bool, gpr: Option<u8> },
    /// A `mov` to CR0, or an `lmsw`, that changes a bit VT-x keeps for the
    /// host -- PG, PE, NE -- with `value` what CR0 would become (the whole
    /// register named, or for `lmsw` its low four bits over CR0's). AMD-V
    /// intercepts no CR0 write, and never exits for one.
    Cr0Write { value: u64 },
    /// A `mov` to (`write`) or from CR4, which AMD-V stops a guest at every
    /// time and VT-x at a write that changes a bit the host keeps -- MCE
    /// among them: so that the CR4 it runs with keeps MCE, and it reads back
    /// the one it wrote (`crate::cr`). `gpr` is the general-purpose register
    /// where the exit names it -- VT-x's always -- and None where the
    /// instruction is to be decoded for it.
    Cr4 { write: bool, gpr: Option<u8> },
    /// A `mov` to (`write`) or from debug register `dr`, which AMD-V stops
    /// a guest at for DR0-DR3, the breakpoint addresses, and VT-x for every
    /// one: they are the CPU's, not the VMCB's or the VMCS's, and the
    /// guest's are kept apart from them and answered from there
    /// (`crate::dr`) -- under VT-x DR6 too, which it does not switch, and
    /// DR7, which it does but would not otherwise see written. `gpr` as for
    /// [`Exit::Cr4`].
    Dr { write: bool, dr: u8, gpr: Option<u8> },
    /// The guest ended an interrupt in service by its x2APIC's EOI, which
    /// the CPU performed -- VT-x's virtual-interrupt delivery -- and stopped
    /// it after, `vector` being one whose end the host must hear of: a
    /// level-triggered one, for the IO-APIC.
    Eoi { vector: u8 },
    /// PAUSE -- the guest spin-waiting: stopped so that whatever it waits
    /// for that the host hands over, a timer's tick above all, is handed
    /// over now rather than at the host's next interrupt.
    Pause,
    /// An instruction of a virtualization extension's own -- VMX's or
    /// SVM's -- or GETSEC, which CPUID tells the guest it has not got and a
    /// CPU without them faults #UD on: what the guest is given.
    Undefined,
    /// An instruction both backends stop the guest at and the run loop
    /// answers the same way: named here by each backend from its own exit,
    /// so that the run loop matches a name and never one extension's number
    /// for it. A VT-x exit reason and an AMD-V exit code are numbers of two
    /// lists, and one taken for the other is another instruction's answer.
    Instr(Instr),
    /// A task switch -- a `jmp`, `call` or interrupt through a task gate or
    /// a TSS, `iret` with NT set -- which neither extension performs in a
    /// guest and the host does not either: a 32-bit kernel's.
    TaskSwitch,
    /// AMD-V's FERR# freeze: an x87 error with CR0.NE clear, the CPU frozen
    /// for an interrupt the way a PC wires its FERR# -- not emulated.
    FerrFreeze,
    /// An exit no arm above names: `code` the extension's own number for it
    /// -- VT-x's basic exit reason where `vmx`, AMD-V's exit code otherwise.
    /// The run loop stops the guest at one, saying which.
    Other { code: u64, vmx: bool },
}

/// The instructions [`Exit::Instr`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instr {
    /// ICEBP (`int1`): AMD-V's intercept of it, or VT-x's of the #DB it
    /// raises, which exits at the instruction -- stepped past, and its #DB
    /// given as the trap it is.
    Icebp,
    /// RDPMC, of counters CPUID does not say are there: #GP.
    Rdpmc,
    /// XSETBV, performed for the guest (`crate::cr::xsetbv`).
    Xsetbv,
    /// WBINVD and INVD: the caches written back, or thrown away -- the
    /// host's dirty lines in them as well as the guest's -- stepped past.
    Wbinvd,
    Invd,
    /// MONITOR, MWAIT -- armed or not -- RDTSCP where it is not offered,
    /// RDPRU, and RSM outside SMM, which is everywhere a guest is: what a
    /// CPU without them, or outside SMM, gives -- #UD, at any CPL.
    Monitor,
    Mwait,
    Rdtscp,
    Rdpru,
    Rsm,
}

impl Instr {
    /// AMD-V's exit code for it, what `hv_exit` records for one under
    /// either backend.
    pub fn svm_code(self) -> u64 {
        match self {
            Instr::Icebp => vmcb::exit::ICEBP,
            Instr::Rdpmc => vmcb::exit::RDPMC,
            Instr::Xsetbv => vmcb::exit::XSETBV,
            Instr::Wbinvd => vmcb::exit::WBINVD,
            Instr::Invd => vmcb::exit::INVD,
            Instr::Monitor => vmcb::exit::MONITOR,
            Instr::Mwait => vmcb::exit::MWAIT,
            Instr::Rdtscp => vmcb::exit::RDTSCP,
            Instr::Rdpru => vmcb::exit::RDPRU,
            Instr::Rsm => vmcb::exit::RSM,
        }
    }
}
