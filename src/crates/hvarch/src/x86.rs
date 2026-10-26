//! x86-64: AMD-V and Intel VT-x behind one set of names.

pub mod cpu;
pub mod fp;
pub mod msrs;
pub mod spec;
pub mod svm;
pub mod vmx;

use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

use kcore::percpu::{ConstInit, CpuLocal};

use crate::{CpuPage, Error, Result, Vendor};

/// The MSRs a guest owns outright, which both backends let it read and
/// write without an exit: the FS, GS and KERNEL_GS bases. Each is switched
/// with the guest by the extension itself -- VT-x saves and loads the first
/// two as VMCS fields and the third through the exit MSR-store and entry
/// MSR-load lists, AMD-V all three through the `vmsave`/`vmload` around
/// `vmrun` -- so nothing of the host's is reachable through them, and the
/// policy's copy is fresh at every exit. A Linux guest writes two of them at
/// every context switch, the most frequent exits a busy guest made; every
/// other MSR still exits to the policy in `hv`.
pub const PASSTHROUGH_MSRS: [u32; 3] = [0xC000_0100, 0xC000_0101, 0xC000_0102];

/// What a CPU keeps of the guests it enters, under whichever extension it
/// runs them: the ASIDs it hands out (AMD-V's), what its DR0-DR3 hold, and
/// what keeps one guest's speculation from the host and from the next.
///
/// One [`CpuLocal`] for all of it, which each `Guest::run` takes once an
/// entry -- as what holds the entry on its CPU with interrupts off, which
/// both need anyway. A `with` is three calls into the kernel (interrupts
/// off, which CPU this is, interrupts back on): with a `CpuLocal` each,
/// VT-x's entry made three more of them than it needed, 400 of the 2,200 ns
/// an exit took on the EX44.
struct Entries {
    asids: svm::Asids,
    debug_addrs: svm::LoadedDebugAddrs,
    spec: spec::State,
}

impl ConstInit for Entries {
    const INIT: Self = Entries {
        asids: svm::Asids::INIT,
        debug_addrs: svm::LoadedDebugAddrs::INIT,
        spec: spec::State::INIT,
    };
}

static ENTRIES: CpuLocal<Entries> = CpuLocal::new();

/// CPUID.1:ECX.
const ECX_X2APIC: u32 = 1 << 21;
const ECX_HYPERVISOR: u32 = 1 << 31;
/// CPUID.8000_0001:EDX.
const EDX_GB_PAGES: u32 = 1 << 26;
const EDX_RDTSCP: u32 = 1 << 27;
/// CPUID.7.0:EBX and ECX.
const EBX_INVPCID: u32 = 1 << 10;
const ECX_RDPID: u32 = 1 << 22;

/// The instructions a guest runs as the CPU has them that its CPUID offers
/// only where they run: RDTSCP and RDPID, which read the guest's own
/// TSC_AUX ([`msrs`]), and INVPCID, which drops the guest's own
/// translations -- those of the ASID or VPID it runs under. AMD-V runs all
/// three in a guest unasked, RDTSCP once its intercept is off; VT-x only
/// where its secondary controls are there to turn them on (`enable RDTSCP`,
/// which RDPID needs too, and `enable INVPCID`) -- without, each is a #UD.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Offers {
    pub rdtscp: bool,
    pub rdpid: bool,
    pub invpcid: bool,
}

/// What [`offers`] answers, a bit an instruction, with `OFFERS_KNOWN` set
/// once it has been asked.
static OFFERS: AtomicU8 = AtomicU8::new(0);
const OFFERS_KNOWN: u8 = 1 << 7;
const OFFERS_RDTSCP: u8 = 1 << 0;
const OFFERS_RDPID: u8 = 1 << 1;
const OFFERS_INVPCID: u8 = 1 << 2;

/// What of RDTSCP, RDPID and INVPCID a guest is given: what the CPU first
/// asked has and its extension runs in a guest, and that from then on, as a
/// guest's CPUID has to stay. VT-x's controls are asked again of each CPU a
/// guest is configured on, and one without them refuses it
/// ([`vmx::Guest`]'s `NotRun::Controls`) -- none this kernel runs on.
pub fn offers() -> Offers {
    let mut bits = OFFERS.load(Ordering::Relaxed);
    if bits & OFFERS_KNOWN == 0 {
        let found = OFFERS_KNOWN | find_offers();
        bits = match OFFERS.compare_exchange(0, found, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => found,
            Err(first) => first,
        };
    }
    Offers {
        rdtscp: bits & OFFERS_RDTSCP != 0,
        rdpid: bits & OFFERS_RDPID != 0,
        invpcid: bits & OFFERS_INVPCID != 0,
    }
}

fn find_offers() -> u8 {
    let has = |yes: bool, bit: u8| if yes { bit } else { 0 };
    let leaf7 = cpu::cpuid_count(7, 0);
    let cpu = has(cpu::cpuid(0x8000_0001).is_some_and(|r| r.edx & EDX_RDTSCP != 0), OFFERS_RDTSCP)
        | has(leaf7.is_some_and(|r| r.ecx & ECX_RDPID != 0), OFFERS_RDPID)
        | has(leaf7.is_some_and(|r| r.ebx & EBX_INVPCID != 0), OFFERS_INVPCID);
    let runs = match Caps::probe().detail {
        Detail::Svm(_) => OFFERS_RDTSCP | OFFERS_RDPID | OFFERS_INVPCID,
        Detail::Vmx(caps) => has(caps.has(vmx::SEC_ENABLE_RDTSCP), OFFERS_RDTSCP | OFFERS_RDPID)
            | has(caps.has(vmx::SEC_ENABLE_INVPCID), OFFERS_INVPCID),
        Detail::None => 0,
    };
    cpu & runs
}

/// Whichever extension this CPU has, in that vendor's own words.
pub enum Detail {
    None,
    Svm(svm::Caps),
    Vmx(vmx::Caps),
}

/// What this machine has to run a guest with, as the CPU itself says it.
///
/// Read once, at load: none of it changes while the machine is up, and all
/// of it is the same on every CPU of a machine -- this kernel does not run
/// on one with two different parts in it, and a hypervisor that tried would
/// have to say which CPU a guest may run on.
pub struct Caps {
    vendor_id: [u8; 12],
    /// We are a guest ourselves: CPUID.1:ECX[31]. Then whatever extension
    /// is reported is the host's emulation of it -- which is exactly the
    /// development loop, and worth saying out loud, because it is also the
    /// answer to "why is this so slow".
    pub under_hypervisor: bool,
    /// The host's own local APIC can be an x2APIC. Not a condition of the
    /// guest's: a guest is given an x2APIC whatever the host has -- its
    /// accesses are MSR exits the hypervisor answers, which is what keeps
    /// an instruction emulator out of it, xAPIC's page of MMIO being the
    /// alternative -- and this says only what the hardware could help with.
    pub x2apic: bool,
    /// 1 GiB pages: a gigabyte of guest memory as one nested page-table
    /// entry rather than five hundred.
    pub gb_pages: bool,
    pub detail: Detail,
}

impl Caps {
    /// Ask the CPU. Safe from anywhere: CPUID has no side effects, and the
    /// MSRs behind each vendor's answer are read only once its CPUID bit
    /// says they are there.
    pub fn probe() -> Self {
        let leaf1 = cpu::cpuid(1);
        let ext1 = cpu::cpuid(0x8000_0001);

        /* Ask the vendor that claims the CPU first, so that a part which
         * somehow reported both is taken at the word of its own name. */
        let detail = match svm::Caps::probe() {
            Some(caps) => Detail::Svm(caps),
            None => match vmx::Caps::probe() {
                Some(caps) => Detail::Vmx(caps),
                None => Detail::None,
            },
        };

        Self {
            vendor_id: cpu::vendor_id(),
            under_hypervisor: leaf1.map_or(false, |r| r.ecx & ECX_HYPERVISOR != 0),
            x2apic: leaf1.map_or(false, |r| r.ecx & ECX_X2APIC != 0),
            gb_pages: ext1.map_or(false, |r| r.edx & EDX_GB_PAGES != 0),
            detail,
        }
    }

    pub fn vendor(&self) -> Vendor {
        match self.detail {
            Detail::Svm(_) => Vendor::Svm,
            Detail::Vmx(_) => Vendor::Vmx,
            Detail::None => Vendor::None,
        }
    }

    /// The nested table may map 2 MiB of a guest's memory with one entry:
    /// always under AMD-V, whose nested table is the host's own long-mode
    /// format and its PS bit with it; under VT-x where the CPU says its EPT
    /// walk takes them.
    pub fn nested_2m(&self) -> bool {
        match &self.detail {
            Detail::Svm(_) => true,
            Detail::Vmx(caps) => caps.ept_2m(),
            Detail::None => false,
        }
    }

    /// The CPUID vendor string, or "?" if it is not text.
    pub fn vendor_id(&self) -> &str {
        core::str::from_utf8(&self.vendor_id).unwrap_or("?")
    }

    /// The extension a guest would run under, or why none can.
    pub fn ext(&self) -> Result<Ext> {
        match &self.detail {
            Detail::Svm(caps) => caps.usable().map(|_| Ext::Svm),
            Detail::Vmx(caps) => caps.usable().map(|_| Ext::Vmx),
            Detail::None => Err(Error::NoExtension),
        }
    }

    /// The page one CPU needs before its extension can be turned on, with
    /// whatever the extension wants in it already written.
    pub fn cpu_page(&self) -> Result<CpuPage> {
        match &self.detail {
            /* The host save area is the CPU's scratch space and starts out
             * zeroed like the rest of the page. */
            Detail::Svm(_) => CpuPage::new(0),
            /* A VMXON region begins with the revision identifier, which the
             * CPU compares with its own before it will use the page. */
            Detail::Vmx(caps) => CpuPage::new(caps.revision()),
            Detail::None => Err(Error::NoExtension),
        }
    }

    /// The whole of what the CPU said, a line a feature -- what to read
    /// first on a machine where a guest will not start.
    pub fn report(&self, out: &mut dyn Write) -> core::fmt::Result {
        let verdict = match self.ext() {
            Ok(_) => "ready",
            Err(e) => match e {
                Error::NoExtension => "nothing to run a guest under",
                Error::FirmwareDisabled => "present, turned off in firmware",
                Error::NoNestedPaging => "present, but without nested paging",
                _ => "present, unusable",
            },
        };
        writeln!(out, "hv: {} -- {}", self.vendor().name(), verdict)?;
        writeln!(out, "  cpu                  {}{}", self.vendor_id(),
                 if self.under_hypervisor { ", itself under a hypervisor" } else { "" })?;

        match &self.detail {
            Detail::Svm(caps) => {
                writeln!(out, "  revision             {}, {} ASIDs", caps.revision, caps.asids)?;
                if caps.firmware_disabled() {
                    writeln!(out, "  VM_CR.SVMDIS         set -- firmware turned SVM off{}",
                             if caps.vm_cr & svm::VM_CR_LOCK != 0 { " and locked it" } else { "" })?;
                }
                for (bit, name, note) in svm::REPORTED {
                    feature(out, name, caps.has(*bit), note)?;
                }
            }
            Detail::Vmx(caps) => {
                writeln!(out, "  revision             0x{:08x}, region {} bytes",
                         caps.revision(), caps.region_size())?;
                if caps.firmware_disabled() {
                    writeln!(out, "  IA32_FEATURE_CONTROL locked with VMXON off -- firmware turned VMX off")?;
                }
                for (bit, name, note) in vmx::REPORTED {
                    feature(out, name, caps.has(*bit), note)?;
                }
                feature(out, "APIC virtualization", caps.apicv(),
                        "an x2APIC guest's interrupts delivered, and its EOIs taken, by the CPU without an exit")?;
                feature(out, "posted interrupts", caps.posted(),
                        "and another CPU's handed to it in its guest, without an exit")?;

                /* The one line on this page that is about this kernel
                 * rather than the CPU: `vmxon` faults, not fails, when the
                 * host's control registers are not what VMX wants, and the
                 * bit most likely to be missing is CR0.NE, which GRUB
                 * leaves as firmware had it. Name it rather than let a
                 * machine with perfectly good VMX say "would not turn on". */
                let host = caps.host_state();
                if host.ok() {
                    feature(out, "host CR0/CR4", true, "on this CPU; hv on checks each")?;
                } else {
                    write!(out, "  {:<21}no   -- on this CPU vmxon would fault:", "host CR0/CR4")?;
                    for (mask, what, table) in [
                        (host.cr0_missing, " CR0 needs", vmx::CR0_BITS),
                        (host.cr0_forbidden, " CR0 must not have", vmx::CR0_BITS),
                        (host.cr4_missing, " CR4 needs", vmx::CR4_BITS),
                        (host.cr4_forbidden, " CR4 must not have", vmx::CR4_BITS),
                    ] {
                        if mask != 0 {
                            write!(out, "{} ", what)?;
                            names(out, mask, table)?;
                        }
                    }
                    writeln!(out)?;
                }
            }
            Detail::None => {}
        }

        feature(out, "x2APIC", self.x2apic, "the host's; a guest's is MSR exits either way")?;
        feature(out, "2 MiB nested pages", self.nested_2m(), "a guest's memory from the huge pool a block an entry")?;
        feature(out, "1 GiB pages", self.gb_pages, "guest memory in one nested entry a gigabyte")?;
        /* What of the x87, SSE and AVX state a guest is given (`fp`): all
         * three where XSAVE switches AVX's, and what it has otherwise. */
        let offered = fp::offered();
        let (avx, note) = match (offered & fp::XCR0_AVX != 0, offered != 0) {
            (true, _) => (true, "its YMM registers and XCR0 switched with its x87 and SSE"),
            (false, true) => (false, "XSAVE with no AVX: the x87 and SSE alone"),
            (false, false) => (false, "no XSAVE: FXSAVE's x87 and SSE alone"),
        };
        feature(out, "AVX in a guest", avx, note)?;
        /* What of RDTSCP, RDPID and INVPCID it runs as the CPU has them
         * (`offers`). */
        let runs = offers();
        feature(out, "RDTSCP in a guest", runs.rdtscp, "the TSC and its own TSC_AUX, switched with it")?;
        feature(out, "RDPID in a guest", runs.rdpid, "its own TSC_AUX")?;
        feature(out, "INVPCID in a guest", runs.invpcid, "the translations of its own ASID or VPID")?;
        /* What of the speculation controls is a guest's own (`spec`). */
        let controls = spec::guest_controls();
        feature(out, "SPEC_CTRL in a guest", controls.spec_ctrl != 0,
                "its own, the CPU's over the host's for its run")?;
        feature(out, "IBPB in a guest", controls.pred_cmd != 0, "done before its next entry on any CPU it trained")?;
        Ok(())
    }
}

/// The set bits of `mask` by the names a manual gives them -- `NE`, or
/// `NE|WP` -- and in hex whatever the table has no name for.
fn names(out: &mut dyn Write, mask: u64, table: &[(u64, &str)]) -> core::fmt::Result {
    let mut left = mask;
    let mut first = true;
    for (bit, name) in table {
        if mask & bit != 0 {
            write!(out, "{}{}", if first { "" } else { "|" }, name)?;
            first = false;
            left &= !bit;
        }
    }
    if left != 0 {
        write!(out, "{}0x{:x}", if first { "" } else { "|" }, left)?;
    }
    Ok(())
}

fn feature(out: &mut dyn Write, name: &str, have: bool, note: &str) -> core::fmt::Result {
    if note.is_empty() {
        writeln!(out, "  {:<21}{}", name, if have { "yes" } else { "no" })
    } else {
        writeln!(out, "  {:<21}{}  -- {}", name, if have { "yes" } else { "no " }, note)
    }
}

/// The extension a guest runs under on this machine, once it is known to be
/// usable. Turning it on and off is per CPU and stays per CPU: the value
/// says which instructions to write, never which CPU they were written on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ext {
    Svm,
    Vmx,
}

impl Ext {
    pub fn vendor(self) -> Vendor {
        match self {
            Ext::Svm => Vendor::Svm,
            Ext::Vmx => Vendor::Vmx,
        }
    }

    /// # Safety
    /// As [`svm::enable`] and [`vmx::enable`]: on the CPU it is for, with a
    /// page that outlives the extension being on.
    pub unsafe fn enable(self, page_phys: u64) -> Result<()> {
        if self.enabled() {
            /* Already on for this CPU, which nothing but this hypervisor
             * ever does -- so it is a CPU an earlier load left behind, and
             * the page it was given is long gone. Turning it on again is
             * not the way back: a second `vmxon` fails, and the CR4 bit
             * that would be cleared on the way out of that failure cannot
             * be cleared in root operation without a #GP. Turn it off
             * first (`hv off`), which does check. */
            return Err(Error::EnableFailed);
        }
        match self {
            Ext::Svm => unsafe { svm::enable(page_phys) },
            Ext::Vmx => unsafe { vmx::enable(page_phys) },
        }
    }

    /// Whether it is off now. False is VMX with a guest's VMCS still current
    /// on this CPU ([`vmx::disable`]), left on: the guest has to go first.
    ///
    /// # Safety
    /// As [`svm::disable`] and [`vmx::disable`]: on the CPU it is for, with
    /// no guest running there.
    pub unsafe fn disable(self) -> bool {
        match self {
            Ext::Svm => {
                unsafe { svm::disable() };
                true
            }
            Ext::Vmx => unsafe { vmx::disable() },
        }
    }

    /// Whether it is on for the CPU this runs on.
    pub fn enabled(self) -> bool {
        match self {
            Ext::Svm => svm::enabled(),
            Ext::Vmx => vmx::enabled(),
        }
    }
}
