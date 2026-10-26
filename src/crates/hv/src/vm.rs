//! A virtual machine: its memory, and the CPU that runs in it -- under
//! whichever extension the machine has. The two backends ([`crate::svm`],
//! [`crate::vmx`]) are one shape, and [`Backend`] is where the run loop and
//! the guests reach that shape without knowing which is underneath.

use core::sync::atomic::AtomicU64;

use hvarch::x86::svm::vmcb::Save;
use hvarch::x86::svm::{GuestRegs, Kick, Profile};
use hvarch::{Ext, Result};

use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::svm::{Exit, Io, LongMode, ProtectedMode};

/// Why a guest was not entered.
#[derive(Clone, Copy, Debug)]
pub enum Refusal {
    /// The VMCB breaks a rule `vmrun` checks, and this is the rule: the CPU
    /// was never handed it. (AMD-V only; VMX's own consistency failure is an
    /// [`Exit::Invalid`].)
    Vmcb(&'static str),
    /// The extension is not on for this CPU.
    NotOn(u32),
    /// This CPU translates with five levels of page table, which the nested
    /// table would be walked as.
    FiveLevelPaging(u32),
    /// This CPU would not drop what it had cached through the guest's
    /// nested table before the guest's first entry there (VMX's INVEPT).
    Flush(u32),
    /// The guest CPU was handed memory other than the memory it was made
    /// for ([`Cpu::enter`]): the nested table its VMCB or VMCS names would
    /// not be that memory's.
    NotItsMemory,
    /// This CPU -- or a hypervisor under this one -- will not take a VT-x
    /// control the host needs to keep a guest off what is the host's:
    /// `missing`, of the `field` controls.
    Controls { cpu: u32, field: &'static str, missing: u32 },
    /// This CPU does not switch every component of the x87, SSE and AVX
    /// state the guest's XCR0 names: `xcr0`, taken on a CPU that does.
    Xstate { cpu: u32, xcr0: u64 },
}

/// One guest's CPU, of whichever kind the machine runs. Every method the run
/// loop and the built-in guests call is here, forwarded to the backend, so
/// that neither has an `svm` or a `vmx` in it.
pub enum Backend {
    Svm(crate::svm::Vcpu),
    Vmx(crate::vmx::Vcpu),
}

/* One line a method: the forward is the whole of it. A macro would fold the
 * ten into one, but at the cost of the risk the convention names -- a reader
 * could not see what each does -- so they are written out. */
impl Backend {
    fn enter(&mut self, memory: &GuestMemory, host_areas: &[AtomicU64], kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        match self {
            Backend::Svm(v) => v.enter(memory, host_areas, kick),
            Backend::Vmx(v) => v.enter(memory, host_areas, kick),
        }
    }
    pub fn save(&self) -> &Save {
        match self { Backend::Svm(v) => v.save(), Backend::Vmx(v) => v.save() }
    }
    pub fn save_mut(&mut self) -> &mut Save {
        match self { Backend::Svm(v) => v.save_mut(), Backend::Vmx(v) => v.save_mut() }
    }
    pub fn save_and_regs_mut(&mut self) -> (&mut Save, &mut GuestRegs) {
        match self { Backend::Svm(v) => v.save_and_regs_mut(), Backend::Vmx(v) => v.save_and_regs_mut() }
    }
    pub fn regs(&self) -> &GuestRegs {
        match self { Backend::Svm(v) => v.regs(), Backend::Vmx(v) => v.regs() }
    }
    pub fn regs_mut(&mut self) -> &mut GuestRegs {
        match self { Backend::Svm(v) => v.regs_mut(), Backend::Vmx(v) => v.regs_mut() }
    }
    /// Set general-purpose register `n`, numbered as the encoding numbers
    /// them, for an instruction performed for the guest: RSP the VMCB's
    /// under AMD-V, and under VT-x the VMCS's, written at the next entry --
    /// where the shadow's alone would be lost.
    pub fn set_gpr(&mut self, n: u8, value: u64) {
        match self {
            Backend::Svm(v) => {
                let (save, regs) = v.save_and_regs_mut();
                *crate::mmio::gpr_mut(save, regs, n) = value;
            }
            Backend::Vmx(v) => v.set_gpr(n, value),
        }
    }
    pub fn long_mode(&mut self, l: &LongMode) {
        match self { Backend::Svm(v) => v.long_mode(l), Backend::Vmx(v) => v.long_mode(l) }
    }
    /// The PVH entry's state (`crate::pvh`): 32-bit protected mode, paging
    /// off -- under VT-x only with unrestricted guest ([`Self::runs_real_mode`]).
    pub fn protected_mode(&mut self, p: &ProtectedMode) {
        match self { Backend::Svm(v) => v.protected_mode(p), Backend::Vmx(v) => v.protected_mode(p) }
    }
    pub fn skip_io(&mut self, io: &Io) {
        match self { Backend::Svm(v) => v.skip_io(io), Backend::Vmx(v) => v.skip_io(io) }
    }
    /// Step past an instruction the host performed, `len` bytes by its own
    /// decoding (`crate::mmio`), and out of any interrupt shadow.
    pub fn skip_emulated(&mut self, len: u64) {
        match self { Backend::Svm(v) => v.skip_emulated(len), Backend::Vmx(v) => v.skip_emulated(len) }
    }
    pub fn skip_cpuid(&mut self) {
        match self { Backend::Svm(v) => v.skip_cpuid(), Backend::Vmx(v) => v.skip_cpuid() }
    }
    pub fn skip_msr(&mut self) {
        match self { Backend::Svm(v) => v.skip_msr(), Backend::Vmx(v) => v.skip_msr() }
    }
    pub fn skip_vmmcall(&mut self) {
        match self { Backend::Svm(v) => v.skip_vmmcall(), Backend::Vmx(v) => v.skip_vmmcall() }
    }
    pub fn skip_hlt(&mut self) {
        match self { Backend::Svm(v) => v.skip_hlt(), Backend::Vmx(v) => v.skip_hlt() }
    }
    pub fn skip_pause(&mut self) {
        match self { Backend::Svm(v) => v.skip_pause(), Backend::Vmx(v) => v.skip_pause() }
    }
    pub fn skip_wbinvd(&mut self) {
        match self { Backend::Svm(v) => v.skip_wbinvd(), Backend::Vmx(v) => v.skip_wbinvd() }
    }
    pub fn skip_xsetbv(&mut self) {
        match self { Backend::Svm(v) => v.skip_xsetbv(), Backend::Vmx(v) => v.skip_xsetbv() }
    }
    /// The guest's XCR0, which its entries make the CPU's: the x87 alone
    /// out of reset, and what its XSETBVs made it since.
    pub fn xcr0(&self) -> u64 {
        match self { Backend::Svm(v) => v.xcr0(), Backend::Vmx(v) => v.xcr0() }
    }
    /// Make `value` the guest's XCR0, for an XSETBV of it `crate::cr`
    /// performed: false, and nothing changed, for a value a CPU offering
    /// what `hvarch::x86::fp::offered` names refuses.
    pub fn set_xcr0(&mut self, value: u64) -> bool {
        match self { Backend::Svm(v) => v.set_xcr0(value), Backend::Vmx(v) => v.set_xcr0(value) }
    }
    /// How many entries put the guest's x87, SSE and AVX registers back into
    /// a CPU that held another guest's.
    pub fn fp_loads(&self) -> u64 {
        match self { Backend::Svm(v) => v.fp_loads(), Backend::Vmx(v) => v.fp_loads() }
    }
    /// The guest's MSRs that its save area does not hold, which its entries
    /// make the CPU's (`hvarch::x86::msrs`).
    pub fn own_msrs(&self) -> crate::policy::Own {
        let tsc_aux = match self { Backend::Svm(v) => v.tsc_aux(), Backend::Vmx(v) => v.tsc_aux() };
        crate::policy::Own { tsc_aux }
    }
    /// Make `own` the guest's, for a WRMSR the policy performed: false, and
    /// nothing changed, for a value the CPU's MSR cannot hold -- which the
    /// policy never takes, and an entry would fault on.
    pub fn set_own_msrs(&mut self, own: &crate::policy::Own) -> bool {
        match self { Backend::Svm(v) => v.set_tsc_aux(own.tsc_aux), Backend::Vmx(v) => v.set_tsc_aux(own.tsc_aux) }
    }
    /// Answer the guest's `mov` to or from CR8 ([`Exit::Cr8`]) from the
    /// shadow task-priority register and step past it. AMD-V keeps that
    /// shadow itself (`V_TPR`, under `V_INTR_MASKING`) and never exits for
    /// one, so there is nothing for its side to do.
    pub fn cr8_access(&mut self, write: bool, gpr: u8) {
        match self { Backend::Svm(_) => {}, Backend::Vmx(v) => v.cr8_access(write, gpr) }
    }
    /// Whether the CPU can deliver the guest's interrupts itself -- VT-x's
    /// virtual-interrupt delivery, over a virtual APIC -- for the run loop
    /// to hand them to it rather than inject them. AMD-V: no (AVIC is not
    /// used).
    pub fn has_vapic(&self) -> bool {
        match self { Backend::Svm(_) => false, Backend::Vmx(v) => v.has_vapic() }
    }
    /// Virtual-interrupt delivery on for the next entry, from `v`
    /// (`crate::vmx::Vcpu::load_apic`); nothing on AMD-V.
    pub fn load_apic(&mut self, v: &crate::lapic::Vectors) {
        match self { Backend::Svm(_) => {}, Backend::Vmx(c) => c.load_apic(v) }
    }
    /// Whether interrupts posted for this CPU can be taken in its guest
    /// without an exit (VT-x's posted interrupts); AMD-V: no.
    pub fn posted_capable(&self) -> bool {
        match self { Backend::Svm(_) => false, Backend::Vmx(v) => v.posted_capable() }
    }
    /// Where its interrupts are posted, before its first entry: true when
    /// taken (`crate::vmx::Vcpu::set_posted`).
    pub fn set_posted(&mut self, posted: alloc::sync::Arc<hvarch::x86::vmx::PostedInterrupts>) -> bool {
        match self { Backend::Svm(_) => false, Backend::Vmx(v) => v.set_posted(posted) }
    }
    /// Virtual-interrupt delivery off from the next entry.
    pub fn apicv_off(&mut self) {
        match self { Backend::Svm(_) => {}, Backend::Vmx(v) => v.apicv_off() }
    }
    /// What virtual-interrupt delivery left by the exit, taken back for the
    /// local APIC: the requested and in-service vectors, and the task
    /// priority.
    pub fn take_apic(&mut self) -> ([u32; 8], [u32; 8], u8) {
        match self { Backend::Svm(_) => ([0; 8], [0; 8], 0), Backend::Vmx(v) => v.take_apic() }
    }
    /// Whether the CPU can be run in real mode -- where a start-up IPI starts
    /// one: AMD-V always, VT-x with unrestricted guest.
    pub fn runs_real_mode(&self) -> bool {
        match self { Backend::Svm(_) => true, Backend::Vmx(v) => v.unrestricted() }
    }
    /// Put the CPU where INIT and then a start-up IPI with `vector` leave
    /// one: real mode, at `vector` * 4 KiB, everything else as after reset.
    pub fn start_at_sipi(&mut self, vector: u8) {
        match self { Backend::Svm(v) => v.start_at_sipi(vector), Backend::Vmx(v) => v.start_at_sipi(vector) }
    }
    /// What INIT clears of the CPU beside its registers: an event queued for
    /// injection, an interrupt shadow, a request for an interrupt window,
    /// and the task priority.
    pub fn init_reset(&mut self) {
        match self { Backend::Svm(v) => v.init_reset(), Backend::Vmx(v) => v.init_reset() }
    }
    /// The guest's CR8 -- its task priority, bits 7:4 of the local APIC's
    /// TPR -- as it stands: AMD-V's `V_TPR`, which the guest writes without
    /// an exit, or VT-x's shadow of it.
    pub fn cr8(&self) -> u8 {
        match self { Backend::Svm(v) => v.cr8(), Backend::Vmx(v) => v.cr8() }
    }
    /// Set it, for a write of the local APIC's TPR: the two are one register.
    pub fn set_cr8(&mut self, value: u8) {
        match self { Backend::Svm(v) => v.set_cr8(value), Backend::Vmx(v) => v.set_cr8(value) }
    }
    /// Answer the guest's `mov` to CR0 or `lmsw` ([`Exit::Cr0Write`]), which
    /// only VT-x stops a guest at -- for the bits it keeps for itself: PG,
    /// which switches long mode on or off with it, and PE and NE -- and step
    /// past it; or give the guest the #GP a value CR0 cannot take is.
    pub fn cr0_write(&mut self, value: u64) {
        match self { Backend::Svm(_) => {}, Backend::Vmx(v) => v.cr0_write(value) }
    }
    /// The CR4 the guest believes it has, which its reads of CR4 get: what
    /// it last wrote of the bits the host keeps -- MCE among them -- while
    /// the register keeps MCE set. Under AMD-V the whole of it is what it
    /// last wrote ([`Exit::Cr4`]); under VT-x the register's other bits,
    /// which the guest writes without an exit, as of its last exit for a
    /// control or a debug register, a fault, or a stop.
    pub fn cr4(&self) -> u64 {
        match self { Backend::Svm(v) => v.cr4(), Backend::Vmx(v) => v.cr4() }
    }
    /// Make `value` the guest's CR4, for a write of it `crate::cr` performed
    /// -- its TLB flushed with `flush`.
    pub fn set_cr4(&mut self, value: u64, flush: bool) {
        match self { Backend::Svm(v) => v.set_cr4(value, flush), Backend::Vmx(v) => v.set_cr4(value, flush) }
    }
    /// The CR4 the CPU runs the guest with: MCE in it whatever the guest
    /// wrote.
    pub fn cr4_register(&self) -> u64 {
        match self { Backend::Svm(v) => v.save().cr4, Backend::Vmx(v) => v.cr4_register() }
    }
    /// The guest's DR0-DR3, which its reads of them get ([`Exit::Dr`]): its
    /// own, kept apart from the CPU's, which are made them before its every
    /// entry.
    pub fn debug_addrs(&self) -> [u64; 4] {
        match self { Backend::Svm(v) => v.debug_addrs(), Backend::Vmx(v) => v.debug_addrs() }
    }
    /// Make `addrs` the guest's DR0-DR3, for a write of one `crate::dr`
    /// performed: the CPU's from its next entry on.
    pub fn set_debug_addrs(&mut self, addrs: [u64; 4]) {
        match self { Backend::Svm(v) => v.set_debug_addrs(addrs), Backend::Vmx(v) => v.set_debug_addrs(addrs) }
    }
    /// Make `value` the guest's DR7, for a write of it -- or general
    /// detect's clearing of GD -- `crate::dr` performed: the VMCB's, which
    /// `vmrun` loads, or the VMCS's, which the entry loads.
    pub fn set_dr7(&mut self, value: u64) {
        match self { Backend::Svm(v) => v.save_mut().dr7 = value, Backend::Vmx(v) => v.set_dr7(value) }
    }
    /// The length of the instruction the guest stopped at, where the exit
    /// says it -- VT-x's do, for every instruction that exits -- and None
    /// where the host has to decode it to know.
    pub fn exit_insn_len(&self) -> Option<u64> {
        match self { Backend::Svm(_) => None, Backend::Vmx(v) => Some(v.exit_insn_len()) }
    }
    /// Whether the guest was in 64-bit code at the instruction it stopped
    /// at: long mode active, and CS's L bit -- as of the exit, under VT-x
    /// one for a control or a debug register.
    pub fn in_64bit_code(&self) -> bool {
        match self { Backend::Svm(v) => v.in_64bit_code(), Backend::Vmx(v) => v.in_64bit_code() }
    }
    pub fn inject_extint(&mut self, vector: u8) {
        match self { Backend::Svm(v) => v.inject_extint(vector), Backend::Vmx(v) => v.inject_extint(vector) }
    }
    /// Whether the guest can take an NMI now: not in its handler for the
    /// last, nor in an interrupt shadow, nor with an event already queued.
    pub fn nmi_allowed(&self) -> bool {
        match self { Backend::Svm(v) => v.nmi_allowed(), Backend::Vmx(v) => v.nmi_allowed() }
    }
    pub fn inject_nmi(&mut self) {
        match self { Backend::Svm(v) => v.inject_nmi(), Backend::Vmx(v) => v.inject_nmi() }
    }
    /// Ask to be told when an NMI could be taken: VT-x's NMI window. AMD-V
    /// is told by the IRET intercept already on while the guest is in its
    /// handler, and otherwise at the next exit.
    pub fn request_nmi_window(&mut self) {
        match self { Backend::Svm(_) => {}, Backend::Vmx(v) => v.request_nmi_window() }
    }
    pub fn inject_ud(&mut self) {
        match self { Backend::Svm(v) => v.inject_ud(), Backend::Vmx(v) => v.inject_ud() }
    }
    pub fn inject_gp(&mut self) {
        match self { Backend::Svm(v) => v.inject_gp(), Backend::Vmx(v) => v.inject_gp() }
    }
    pub fn reflect_exception(&mut self, vector: u8, error: Option<u32>) {
        match self { Backend::Svm(v) => v.reflect_exception(vector, error), Backend::Vmx(v) => v.reflect_exception(vector, error) }
    }
    /// Step past an ICEBP (`int1`), which stopped the guest -- AMD-V's
    /// intercept of it, or VT-x's of the #DB it raises, which exits at the
    /// instruction -- for its #DB to be given as the trap it is, after it.
    pub fn skip_icebp(&mut self) {
        match self { Backend::Svm(v) => v.skip_icebp(), Backend::Vmx(v) => v.skip_icebp() }
    }
    pub fn request_irq_window(&mut self) {
        match self { Backend::Svm(v) => v.request_irq_window(), Backend::Vmx(v) => v.request_irq_window() }
    }
    pub fn clear_irq_window(&mut self) {
        match self { Backend::Svm(v) => v.clear_irq_window(), Backend::Vmx(v) => v.clear_irq_window() }
    }
    pub fn interruptible(&self) -> bool {
        match self { Backend::Svm(v) => v.interruptible(), Backend::Vmx(v) => v.interruptible() }
    }
    pub fn event_queued(&self) -> bool {
        match self { Backend::Svm(v) => v.event_queued(), Backend::Vmx(v) => v.event_queued() }
    }
    pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self { Backend::Svm(v) => v.dump(out), Backend::Vmx(v) => v.dump(out) }
    }
    /// Have the whole of the last exit's state in hand for a [`dump`]: under
    /// VT-x an exit reads only what its handling needs, and the rest is read
    /// now if the VMCS is still current here; the VMCB is memory, and whole
    /// always. False if it could not be read -- and the dump says so.
    ///
    /// [`dump`]: Backend::dump
    pub fn read_whole_state(&mut self) -> bool {
        match self { Backend::Svm(_) => true, Backend::Vmx(v) => v.read_whole_state() }
    }
    pub fn set_flush_always(&mut self, on: bool) {
        match self { Backend::Svm(v) => v.set_flush_always(on), Backend::Vmx(v) => v.set_flush_always(on) }
    }
    pub fn set_profile(&mut self, on: bool) {
        match self { Backend::Svm(v) => v.set_profile(on), Backend::Vmx(v) => v.set_profile(on) }
    }
    pub fn profile(&self) -> Option<Profile> {
        match self { Backend::Svm(v) => v.profile(), Backend::Vmx(v) => v.profile() }
    }
    pub fn asid(&self) -> Option<u32> {
        match self { Backend::Svm(v) => v.asid(), Backend::Vmx(v) => v.asid() }
    }
}

/// One guest CPU: made for the memory of one guest, and entered with that
/// memory and no other. The nested table its VMCB or VMCS names is that
/// memory's -- VT-x takes the EPT pointer into the VMCS at the first entry
/// and never looks again -- so a CPU entered with another guest's memory
/// would run over a table that may since have been freed. [`Cpu::enter`]
/// refuses that, so the pairing is kept by the type rather than by care.
pub struct Cpu {
    backend: Backend,
    /// The identity of the nested table it was made for.
    table: u64,
}

impl Cpu {
    /// A CPU for the guest whose memory is `memory`, of whichever kind the
    /// machine runs, in no state yet, that stops at `exceptions`.
    pub fn new(machine: &Machine, memory: &GuestMemory, exceptions: u32) -> Result<Self> {
        let caps = machine.caps();
        let backend = match machine.ext()? {
            Ext::Svm => Backend::Svm(crate::svm::Vcpu::new(caps, exceptions)?),
            Ext::Vmx => Backend::Vmx(crate::vmx::Vcpu::new(caps, exceptions)?),
        };
        Ok(Self { backend, table: memory.nested().id })
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut Backend {
        &mut self.backend
    }

    /// Run the guest on the CPU this is called on until it next stops, and
    /// say why it did and which CPU it was. `memory` is the memory this CPU
    /// was made for, and is borrowed for as long as the guest runs: the
    /// pages its nested table maps stay the guest's until it has left. With
    /// `kick`, the entry is the vCPU's in `Kick`'s sense, and one a kick
    /// refused is `Exit::Kicked`.
    pub fn enter(&mut self, memory: &GuestMemory, machine: &Machine, kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        if memory.nested().id != self.table {
            return Err(Refusal::NotItsMemory);
        }
        self.backend.enter(memory, machine.host_areas(), kick)
    }
}

/// One guest: its memory and its one CPU -- what the built-in guests are.
/// A guest of several CPUs shares its memory between them instead
/// (`crate::run::LinuxGuest`), each a [`Cpu`] of its own.
pub struct Vm {
    memory: GuestMemory,
    cpu: Cpu,
}

impl Vm {
    /// An empty machine for a guest of whichever kind this machine runs: no
    /// memory yet, and a CPU in no state yet that stops at `exceptions`.
    pub fn new(machine: &Machine, exceptions: u32) -> Result<Self> {
        let memory = GuestMemory::new(machine.caps().vendor())?;
        let cpu = Cpu::new(machine, &memory, exceptions)?;
        Ok(Self { memory, cpu })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut GuestMemory {
        &mut self.memory
    }

    /// Whether this guest runs under Intel VT-x -- which a built-in guest
    /// needs to know only where an instruction differs between the two, the
    /// hypercall (`vmcall` on Intel, `vmmcall` on AMD).
    pub fn is_vmx(&self) -> bool {
        matches!(self.cpu.backend, Backend::Vmx(_))
    }

    pub fn vcpu(&self) -> &Backend {
        &self.cpu.backend
    }

    pub fn vcpu_mut(&mut self) -> &mut Backend {
        &mut self.cpu.backend
    }

    /// Run the guest on the CPU this is called on until it next stops, and
    /// say why it did and which CPU it was. With `kick`, the entry is the
    /// vCPU's in `Kick`'s sense, and one a kick refused is `Exit::Kicked`.
    pub fn enter(&mut self, machine: &Machine, kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        self.cpu.enter(&self.memory, machine, kick)
    }
}
