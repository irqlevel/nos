#![no_std]

//! The CPU's virtualization extension: what says whether a guest can run
//! here at all, what turns the extension on for a CPU, and -- as the
//! hypervisor grows -- what a guest is entered and left by.
//!
//! This is the one crate of the hypervisor that is allowed to be `unsafe`,
//! and the split is what makes that measurable:
//! `scripts/unsafe-count.py hv hvarch` says in two numbers how much of the
//! hypervisor could corrupt the host. The answer has to stay small, and in
//! here. Everything above -- the VM, its memory, the devices it emulates,
//! the exit dispatcher -- is ordinary safe Rust in `hv`.
//!
//! What is vendor- and architecture-specific lives behind one set of names.
//! On x86-64 that is AMD-V ([`x86::svm`]) or Intel VT-x ([`x86::vmx`]); on
//! arm64 it will be EL2 with stage-2 translation, which today is detected
//! and reported and not yet entered.

#[cfg(target_arch = "x86_64")]
pub mod x86;
#[cfg(target_arch = "x86_64")]
pub use x86::{Caps, Ext};

#[cfg(target_arch = "aarch64")]
pub mod arm64;
#[cfg(target_arch = "aarch64")]
pub use arm64::{Caps, Ext};

mod page;
pub use page::CpuPage;

/// What a machine has to run a guest with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    /// AMD-V: a VMCB, `vmrun`, nested page tables.
    Svm,
    /// Intel VT-x: a VMCS, `vmlaunch`/`vmresume`, extended page tables.
    Vmx,
    /// Arm: EL2 and stage-2 translation.
    El2,
    /// Nothing this crate knows how to use.
    None,
}

impl Vendor {
    /// What to call it in a report: the vendor's name for it, because that
    /// is what the manual on the desk beside the machine calls it too.
    pub fn name(self) -> &'static str {
        match self {
            Vendor::Svm => "AMD-V (SVM)",
            Vendor::Vmx => "Intel VT-x (VMX)",
            Vendor::El2 => "Arm virtualization (EL2)",
            Vendor::None => "none",
        }
    }
}

/// Why no guest can run here, or why a step of turning the extension on
/// failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The CPU has no virtualization extension this crate knows.
    NoExtension,
    /// It has one, and firmware turned it off and locked it off until the
    /// next reset -- `VM_CR.SVMDIS` on AMD, `IA32_FEATURE_CONTROL` on Intel.
    /// Nothing software can do about it; the fix is a BIOS setting.
    FirmwareDisabled,
    /// It has one, but not the nested paging that translates guest physical
    /// addresses in hardware. There are no shadow page tables here and there
    /// will not be: a hypervisor that walks the guest's page tables itself is
    /// most of an instruction emulator, which is the bug surface this whole
    /// design exists to avoid.
    NoNestedPaging,
    /// No memory for the page a CPU needs before its extension can be on.
    NoMemory,
    /// The extension refused to turn on, having said it could.
    EnableFailed,
    /// The host's own CR0 or CR4 is not what entering VMX operation
    /// requires. `VMXON` *faults* on that rather than failing, so it is
    /// checked first and named here; `hv info` says which bits.
    HostState,
    /// The CPU is out of range, or not running.
    NoSuchCpu,
    /// This architecture's backend is not written yet.
    NotImplemented,
    /// No guest memory at that guest physical address.
    Unmapped,
    /// Guest memory there already, or an address no nested table can hold.
    BadAddress,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::NoExtension => "the CPU has no virtualization extension",
            Error::FirmwareDisabled => "virtualization is turned off in firmware",
            Error::NoNestedPaging => "the CPU has no nested paging",
            Error::NoMemory => "out of memory",
            Error::EnableFailed => "the extension would not turn on",
            Error::HostState => "the host's CR0/CR4 are not what VMX operation requires",
            Error::NoSuchCpu => "no such CPU",
            Error::NotImplemented => "not implemented on this architecture",
            Error::Unmapped => "no guest memory at that address",
            Error::BadAddress => "not a guest physical address that can be given memory",
        })
    }
}

pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    /// A number an atomic can carry, for an answer that has to come back
    /// from an IPI handler -- where a value is all that can cross. Never 0,
    /// which is what "it worked" is.
    pub fn code(self) -> u32 {
        match self {
            Error::NoExtension => 1,
            Error::FirmwareDisabled => 2,
            Error::NoNestedPaging => 3,
            Error::NoMemory => 4,
            Error::EnableFailed => 5,
            Error::NoSuchCpu => 6,
            Error::NotImplemented => 7,
            Error::HostState => 8,
            Error::Unmapped => 9,
            Error::BadAddress => 10,
        }
    }

    /// What `code` stood for. A word that was never one reads as
    /// `EnableFailed`: it cannot happen, and if it did, saying so plainly
    /// beats a panic in a kernel that has a guest to lose.
    pub fn from_code(code: u32) -> Error {
        match code {
            1 => Error::NoExtension,
            2 => Error::FirmwareDisabled,
            3 => Error::NoNestedPaging,
            4 => Error::NoMemory,
            6 => Error::NoSuchCpu,
            7 => Error::NotImplemented,
            8 => Error::HostState,
            9 => Error::Unmapped,
            10 => Error::BadAddress,
            _ => Error::EnableFailed,
        }
    }
}
