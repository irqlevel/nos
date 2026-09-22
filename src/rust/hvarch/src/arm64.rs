//! arm64: what the machine has for a guest, and the one thing it does not.
//!
//! On Arm the hypervisor is not an extension a kernel turns on -- it is an
//! exception level a kernel runs *at*. `nos` boots at EL2 when firmware
//! hands it the machine there, and drops straight to EL1
//! (`arch/arm64/boot.S`), where it has been ever since. A loadable module
//! cannot climb back up: EL2 is entered by taking an exception to it, and a
//! kernel already at EL1 has no way to ask.
//!
//! So this backend reports and does not run: what exception level we are at,
//! and what stage-2 translation the CPU would offer if we were at EL2. The
//! EL2 hypervisor is a change to the boot path -- stay at EL2, run the
//! kernel there with VHE, install stage-2 -- and that is where it will be
//! written, with the safe half in `hv` unchanged.

use core::arch::asm;
use core::fmt::Write;

use crate::{CpuPage, Error, Result, Vendor};

/// What the machine says about running a guest.
pub struct Caps {
    /// 0..3, from `CurrentEL`. A hypervisor needs 2.
    pub current_el: u8,
    /// `ID_AA64MMFR1_EL1.VMIDBits`: how many VMIDs stage-2 would offer.
    pub vmid_bits: u8,
    /// `ID_AA64MMFR1_EL1.VH`: the virtualization host extensions, which let
    /// a kernel run at EL2 with EL1's register names.
    pub vhe: bool,
    /// `ID_AA64MMFR0_EL1.PARange`: the physical address width a stage-2
    /// table would translate into.
    pub pa_bits: u8,
}

impl Caps {
    pub fn probe() -> Self {
        /* All three are readable at EL1 and have no side effects. */
        let current_el: u64;
        let mmfr0: u64;
        let mmfr1: u64;
        unsafe {
            asm!("mrs {}, currentel", out(reg) current_el, options(nomem, nostack, preserves_flags));
            asm!("mrs {}, id_aa64mmfr0_el1", out(reg) mmfr0, options(nomem, nostack, preserves_flags));
            asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1, options(nomem, nostack, preserves_flags));
        }

        Self {
            current_el: ((current_el >> 2) & 0x3) as u8,
            vmid_bits: match (mmfr1 >> 4) & 0xF {
                2 => 16,
                _ => 8,
            },
            vhe: (mmfr1 >> 8) & 0xF != 0,
            pa_bits: match mmfr0 & 0xF {
                0 => 32,
                1 => 36,
                2 => 40,
                3 => 42,
                4 => 44,
                5 => 48,
                6 => 52,
                _ => 0,
            },
        }
    }

    pub fn vendor(&self) -> Vendor {
        /* The CPU has EL2 or it does not; that it is not ours to use is a
         * fact about this kernel's boot path, which `ext` and the report
         * say, not about the machine. */
        Vendor::El2
    }

    pub fn ext(&self) -> Result<Ext> {
        Err(Error::NotImplemented)
    }

    pub fn cpu_page(&self) -> Result<CpuPage> {
        Err(Error::NotImplemented)
    }

    pub fn report(&self, out: &mut dyn Write) -> core::fmt::Result {
        writeln!(out, "hv: {} -- not implemented on this architecture", Vendor::El2.name())?;
        writeln!(out, "  running at           EL{}", self.current_el)?;
        if self.current_el < 2 {
            writeln!(out, "                       -- boot.S drops EL2 to EL1; a module cannot climb back")?;
        }
        writeln!(out, "  VMIDs                {} bits", self.vmid_bits)?;
        writeln!(out, "  VHE                  {}", if self.vhe { "yes" } else { "no" })?;
        writeln!(out, "  stage-2 output       {} bits", self.pa_bits)
    }
}

/// There is nothing to turn on here yet; the type exists so that everything
/// above this crate is written once for both architectures.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ext {
    El2,
}

impl Ext {
    pub fn vendor(self) -> Vendor {
        Vendor::El2
    }

    /// # Safety
    /// Never sound to call, because it is never reached: [`Caps::ext`] hands
    /// out no `Ext` on this architecture yet.
    pub unsafe fn enable(self, _page_phys: u64) -> Result<()> {
        Err(Error::NotImplemented)
    }

    /// # Safety
    /// As [`Ext::enable`].
    pub unsafe fn disable(self) {}

    pub fn enabled(self) -> bool {
        false
    }
}
