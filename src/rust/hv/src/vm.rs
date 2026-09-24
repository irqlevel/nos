//! A virtual machine: its memory, and the CPU that runs in it.

use hvarch::x86::svm::{Kick, NotRun, Permissions};
use hvarch::{Error, Ext, Result};

use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::svm::{Exit, Vcpu};

/// Why a guest was not entered.
#[derive(Clone, Copy, Debug)]
pub enum Refusal {
    /// The VMCB breaks a rule `vmrun` checks, and this is the rule: the CPU
    /// was never handed it.
    Vmcb(&'static str),
    /// The extension is not on for this CPU.
    NotOn(u32),
    /// This CPU translates with five levels of page table, which the nested
    /// table would be walked as (`hvarch::x86::svm::NotRun`).
    FiveLevelPaging(u32),
}

/// One guest: its memory, the permission maps it runs under -- every port
/// and every MSR the host's to answer -- and its one CPU.
pub struct Vm {
    memory: GuestMemory,
    perms: Permissions,
    vcpu: Vcpu,
}

impl Vm {
    /// An empty machine for a guest: no memory yet, and a CPU in no state
    /// yet that stops at `exceptions`, one bit per vector.
    pub fn new(machine: &Machine, exceptions: u32) -> Result<Self> {
        match machine.ext()? {
            Ext::Svm => {}
            Ext::Vmx => return Err(Error::NotImplemented),
        }
        Ok(Self {
            memory: GuestMemory::new()?,
            perms: Permissions::intercept_all()?,
            vcpu: Vcpu::new(machine.caps(), exceptions)?,
        })
    }

    pub fn memory(&self) -> &GuestMemory {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut GuestMemory {
        &mut self.memory
    }

    pub fn vcpu(&self) -> &Vcpu {
        &self.vcpu
    }

    pub fn vcpu_mut(&mut self) -> &mut Vcpu {
        &mut self.vcpu
    }

    /// Run the guest on the CPU this is called on until it next stops, and
    /// say why it did and which CPU it was.
    ///
    /// The caller's task may be moved between one call and the next; each
    /// entry checks the CPU it finds itself on. With `kick`, the entry is the
    /// vCPU's in `Kick`'s sense, and one a kick refused is `Exit::Kicked`.
    pub fn enter(&mut self, machine: &Machine, kick: Option<&Kick>)
        -> core::result::Result<(Exit, u32), Refusal>
    {
        self.vcpu.check().map_err(Refusal::Vmcb)?;
        let nested = self.memory.nested();
        /* The nested table is the memory's own and maps nothing but pages
         * the memory owns (`GuestMemory`), and both are this VM's, borrowed
         * for the whole of the call; it only gains entries, and its id is
         * its own (`Npt`). A CPU's entry in the machine's host areas is never
         * the address of a page that has gone (`Machine::host_areas`). */
        let cpu = match unsafe { self.vcpu.guest_mut().run(&self.perms, nested, machine.host_areas(), kick) } {
            Ok(cpu) => cpu,
            /* Nothing ran: no exit to read, and no event of the last one's to
             * queue again -- it is queued already. */
            Err(NotRun::Kicked { cpu }) => return Ok((Exit::Kicked, cpu)),
            Err(NotRun::Off { cpu }) => return Err(Refusal::NotOn(cpu)),
            Err(NotRun::FiveLevelPaging { cpu }) => return Err(Refusal::FiveLevelPaging(cpu)),
        };
        self.vcpu.requeue_event();
        Ok((self.vcpu.exit(), cpu))
    }
}
