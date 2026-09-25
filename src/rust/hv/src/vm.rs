//! A virtual machine: its memory, and the CPU that runs in it -- under
//! whichever extension the machine has. The two backends ([`crate::svm`],
//! [`crate::vmx`]) are one shape, and [`Backend`] is where the run loop and
//! the guests reach that shape without knowing which is underneath.

use core::sync::atomic::AtomicU64;

use hvarch::x86::svm::vmcb::Save;
use hvarch::x86::svm::{GuestRegs, Kick, Nested, Profile};
use hvarch::{Ext, Result};

use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::svm::{Exit, Io, LongMode};

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
    fn enter(&mut self, nested: Nested, host_areas: &[AtomicU64], kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        match self {
            Backend::Svm(v) => v.enter(nested, host_areas, kick),
            Backend::Vmx(v) => v.enter(nested, host_areas, kick),
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
    pub fn long_mode(&mut self, l: &LongMode) {
        match self { Backend::Svm(v) => v.long_mode(l), Backend::Vmx(v) => v.long_mode(l) }
    }
    pub fn skip_io(&mut self, io: &Io) {
        match self { Backend::Svm(v) => v.skip_io(io), Backend::Vmx(v) => v.skip_io(io) }
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
    pub fn skip_wbinvd(&mut self) {
        match self { Backend::Svm(v) => v.skip_wbinvd(), Backend::Vmx(v) => v.skip_wbinvd() }
    }
    /// Answer the guest's `mov` to or from CR8 ([`Exit::Cr8`]) from the
    /// shadow task-priority register and step past it. AMD-V keeps that
    /// shadow itself (`V_TPR`, under `V_INTR_MASKING`) and never exits for
    /// one, so there is nothing for its side to do.
    pub fn cr8_access(&mut self, write: bool, gpr: u8) {
        match self { Backend::Svm(_) => {}, Backend::Vmx(v) => v.cr8_access(write, gpr) }
    }
    pub fn inject_extint(&mut self, vector: u8) {
        match self { Backend::Svm(v) => v.inject_extint(vector), Backend::Vmx(v) => v.inject_extint(vector) }
    }
    pub fn inject_ud(&mut self) {
        match self { Backend::Svm(v) => v.inject_ud(), Backend::Vmx(v) => v.inject_ud() }
    }
    pub fn inject_gp(&mut self) {
        match self { Backend::Svm(v) => v.inject_gp(), Backend::Vmx(v) => v.inject_gp() }
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

/// One guest: its memory and its one CPU.
pub struct Vm {
    memory: GuestMemory,
    vcpu: Backend,
}

impl Vm {
    /// An empty machine for a guest of whichever kind this machine runs: no
    /// memory yet, and a CPU in no state yet that stops at `exceptions`.
    pub fn new(machine: &Machine, exceptions: u32) -> Result<Self> {
        let caps = machine.caps();
        let vcpu = match machine.ext()? {
            Ext::Svm => Backend::Svm(crate::svm::Vcpu::new(caps, exceptions)?),
            Ext::Vmx => Backend::Vmx(crate::vmx::Vcpu::new(caps, exceptions)?),
        };
        Ok(Self {
            memory: GuestMemory::new(caps.vendor())?,
            vcpu,
        })
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
        matches!(self.vcpu, Backend::Vmx(_))
    }

    pub fn vcpu(&self) -> &Backend {
        &self.vcpu
    }

    pub fn vcpu_mut(&mut self) -> &mut Backend {
        &mut self.vcpu
    }

    /// Run the guest on the CPU this is called on until it next stops, and
    /// say why it did and which CPU it was. With `kick`, the entry is the
    /// vCPU's in `Kick`'s sense, and one a kick refused is `Exit::Kicked`.
    pub fn enter(&mut self, machine: &Machine, kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        let nested = self.memory.nested();
        self.vcpu.enter(nested, machine.host_areas(), kick)
    }
}
