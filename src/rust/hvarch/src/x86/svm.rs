//! AMD-V: what the CPU says about it, and turning it on for one CPU.
//!
//! SVM is the backend written first, for a reason that is about the dev loop
//! rather than the hardware: QEMU's TCG emulates SVM, nested paging
//! included, and emulates no VMX at all -- so on a Mac, where the x86 kernel
//! runs under TCG, AMD-V is the only extension a guest can be brought up on
//! at all. It is also the simpler of the two: the VMCB is a plain structure
//! in memory, with none of `vmread`/`vmwrite`'s ceremony, and the one AMD
//! machine this kernel runs on (the Hetzner AX41) is a real target.

use core::arch::{asm, naked_asm};
use core::mem::offset_of;
use core::sync::atomic::{AtomicU64, Ordering};

use kcore::dma::DmaBuffer;

use super::cpu;
use crate::{Error, Result};

pub mod vmcb;

pub use vmcb::{Vmcb, VmcbPage};

/* CPUID.8000_0001:ECX -- is there an SVM at all. */
pub const CPUID_EXT_FEATURES: u32 = 0x8000_0001;
pub const ECX_SVM: u32 = 1 << 2;

/* CPUID.8000_000A: the revision (EAX[7:0]), how many ASIDs (EBX), and what
   of SVM this part has (EDX). */
pub const CPUID_SVM: u32 = 0x8000_000A;

pub const NP: u32 = 1 << 0;
pub const LBR_VIRT: u32 = 1 << 1;
pub const SVM_LOCK: u32 = 1 << 2;
pub const NRIP_SAVE: u32 = 1 << 3;
pub const TSC_RATE_MSR: u32 = 1 << 4;
pub const VMCB_CLEAN: u32 = 1 << 5;
pub const FLUSH_BY_ASID: u32 = 1 << 6;
pub const DECODE_ASSISTS: u32 = 1 << 7;
pub const PAUSE_FILTER: u32 = 1 << 10;
pub const PAUSE_THRESHOLD: u32 = 1 << 12;
pub const AVIC: u32 = 1 << 13;
pub const VMSAVE_VIRT: u32 = 1 << 15;
pub const VGIF: u32 = 1 << 16;
pub const VNMI: u32 = 1 << 24;

/// The features worth a line in a report, and why each matters here. The
/// ones with no note are read out and not yet used; they are in the list so
/// that a machine's report says what it could have been given.
pub const REPORTED: &[(u32, &str, &str)] = &[
    (NP, "nested paging", "guest physical addresses translated by the CPU"),
    (NRIP_SAVE, "next-RIP save", "where an intercepted instruction ends"),
    (DECODE_ASSISTS, "decode assists", ""),
    (FLUSH_BY_ASID, "flush by ASID", ""),
    (VMCB_CLEAN, "VMCB clean bits", ""),
    (VMSAVE_VIRT, "virtual VMSAVE", ""),
    (VGIF, "virtual GIF", ""),
    (AVIC, "AVIC", ""),
    (PAUSE_FILTER, "pause filter", ""),
    (SVM_LOCK, "SVM lock", ""),
];

pub const MSR_EFER: u32 = 0xC000_0080;
pub const EFER_SVME: u64 = 1 << 12;

pub const MSR_VM_CR: u32 = 0xC001_0114;
pub const VM_CR_LOCK: u64 = 1 << 3;
pub const VM_CR_SVMDIS: u64 = 1 << 4;

pub const MSR_VM_HSAVE_PA: u32 = 0xC001_0117;

/// What this CPU says about its SVM.
#[derive(Clone, Copy)]
pub struct Caps {
    /// CPUID.8000_000A:EAX[7:0] -- 1 on everything that exists.
    pub revision: u32,
    /// How many address space identifiers a guest may be given. ASID 0 is
    /// the host's, so one guest needs two.
    pub asids: u32,
    /// CPUID.8000_000A:EDX, the bits above.
    pub features: u32,
    /// `VM_CR` as it reads now: whether firmware left SVM on.
    pub vm_cr: u64,
}

impl Caps {
    /// None when the CPU has no SVM -- which is the only state in which the
    /// SVM MSRs must not be read, since reading one a CPU does not have is a
    /// #GP and this kernel's #GP handler panics.
    pub fn probe() -> Option<Self> {
        if cpu::cpuid(CPUID_EXT_FEATURES)?.ecx & ECX_SVM == 0 {
            return None;
        }
        let r = cpu::cpuid(CPUID_SVM)?;
        /* Both MSRs exist on any part that reports SVM in CPUID. */
        let vm_cr = unsafe { cpu::rdmsr(MSR_VM_CR) };
        Some(Self {
            revision: r.eax & 0xFF,
            asids: r.ebx,
            features: r.edx,
            vm_cr,
        })
    }

    #[inline]
    pub fn has(&self, feature: u32) -> bool {
        self.features & feature != 0
    }

    /// Firmware turned SVM off. `SVM_LOCK` says whether it can be turned
    /// back on with the key MSR at all -- without it, nothing but a reset
    /// and a different BIOS setting will do.
    pub fn firmware_disabled(&self) -> bool {
        self.vm_cr & VM_CR_SVMDIS != 0
    }

    /// Whether a guest can be run on this CPU, and what is missing when it
    /// cannot.
    pub fn usable(&self) -> Result<()> {
        if self.firmware_disabled() {
            return Err(Error::FirmwareDisabled);
        }
        if !self.has(NP) {
            return Err(Error::NoNestedPaging);
        }
        if self.asids < 2 {
            /* ASID 0 is the host's; a guest needs one of its own. No part
             * that exists says this, but a CPU that did would fail at the
             * first vmrun rather than here. */
            return Err(Error::EnableFailed);
        }
        Ok(())
    }
}

/// Turn SVM on for the CPU this runs on.
///
/// # Safety
/// Runs on the CPU it is turning SVM on for -- so with preemption off, or
/// from that CPU's own IPI handler -- and `host_area_phys` is a page that
/// stays allocated, and is touched by nothing else, until [`disable`] runs
/// on that same CPU. It is where `vmrun` puts the host's own state before it
/// takes the CPU away, and where `#vmexit` gets it back from: freeing it
/// under a running guest loses the host.
pub unsafe fn enable(host_area_phys: u64) -> Result<()> {
    if unsafe { cpu::rdmsr(MSR_VM_CR) } & VM_CR_SVMDIS != 0 {
        return Err(Error::FirmwareDisabled);
    }

    let efer = unsafe { cpu::rdmsr(MSR_EFER) };
    unsafe { cpu::wrmsr(MSR_EFER, efer | EFER_SVME) };
    unsafe { cpu::wrmsr(MSR_VM_HSAVE_PA, host_area_phys) };

    /* EFER.SVME reads back as zero on a CPU that would not take it, and
     * says so here rather than at the first vmrun -- which is an
     * undefined-opcode fault, in a kernel whose handler panics. The error
     * path gives back what it took: the caller is about to free that page,
     * and the CPU must not be left naming it. */
    if !enabled() {
        unsafe { cpu::wrmsr(MSR_VM_HSAVE_PA, 0) };
        return Err(Error::EnableFailed);
    }
    Ok(())
}

/// Turn it off again, on the CPU this runs on.
///
/// # Safety
/// Runs on the CPU it is turning SVM off for, and no guest is running there:
/// a `vmrun` after this faults. The host save area may be freed once this
/// has returned.
pub unsafe fn disable() {
    /* KVM's order: the address, then the enable bit. Either order has a
     * window -- SVM on with no save area, or SVM off still naming the page
     * -- and neither matters, because this runs with interrupts off and
     * nothing executes `vmrun` in it. What matters is the state on return:
     * SVM off and the page named nowhere, so the caller may free it. */
    unsafe { cpu::wrmsr(MSR_VM_HSAVE_PA, 0) };
    let efer = unsafe { cpu::rdmsr(MSR_EFER) };
    if efer & EFER_SVME == 0 {
        return;
    }

    /* GIF to 1 before SVM goes: with it clear, INIT and NMI stay blocked,
     * and `stgi` is an undefined opcode once SVME is off, so after this
     * there would be no way to set it. The run stub clears GIF only for its
     * own length and sets it again before it returns, and this runs from an
     * IPI, which the stub's clear GIF holds off -- so it finds GIF set. It
     * sets it anyway, so that the off switch stays right whatever the run
     * loop comes to do. */
    unsafe { asm!("stgi", options(nomem, nostack)) };
    unsafe { cpu::wrmsr(MSR_EFER, efer & !EFER_SVME) };
}

/// Whether SVM is on for the CPU this runs on. EFER exists on every x86-64
/// CPU -- it is how long mode was turned on -- so this is sound anywhere.
pub fn enabled() -> bool {
    let efer = unsafe { cpu::rdmsr(MSR_EFER) };
    efer & EFER_SVME != 0
}

/// The host save area this CPU was given, or 0: what `VM_HSAVE_PA` holds.
/// Only on a CPU with SVM, where the MSR exists: what [`Guest::run`] asks,
/// and a `Guest` is made only for a machine CPUID says has SVM.
fn host_area() -> u64 {
    unsafe { cpu::rdmsr(MSR_VM_HSAVE_PA) }
}

/// The guest's general-purpose registers that neither `vmrun` nor
/// `#vmexit` touches: every one but RAX and RSP, which live in the VMCB's
/// save area. The run stub loads them on the way in and stores them on the
/// way out.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct GuestRegs {
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
}

/// The I/O and MSR permission maps a guest runs under: a set bit is an
/// access the host hears of. Both are all ones -- every port and every MSR
/// intercepted -- and there is deliberately no way to clear a bit yet: a
/// port the guest reaches directly is one of the host's devices, and an MSR
/// it reaches directly is the host's CPU. Passing some through (FS and GS
/// base, which are the guest's own) is a decision for when a guest needs
/// the speed, one MSR at a time.
pub struct Permissions {
    iopm: DmaBuffer,
    msrpm: DmaBuffer,
}

impl Permissions {
    pub fn intercept_all() -> Result<Self> {
        let pages = |bytes: usize| bytes.div_ceil(kcore::consts::PAGE_SIZE);
        let mut iopm = DmaBuffer::new(pages(vmcb::IOPM_BYTES)).ok_or(Error::NoMemory)?;
        let mut msrpm = DmaBuffer::new(pages(vmcb::MSRPM_BYTES)).ok_or(Error::NoMemory)?;
        iopm.as_mut_slice().fill(0xFF);
        msrpm.as_mut_slice().fill(0xFF);
        Ok(Self { iopm, msrpm })
    }
}

/// The intercepts every guest runs with, whatever else its VMCB asks for:
/// what keeps the host's CPU the host's. [`Guest::run`] sets them on every
/// entry, so the policy above this crate -- which exits a guest makes, how
/// it starts -- can be wrong about the guest without being wrong about the
/// host.
///
/// - INTR, NMI, SMI, INIT: the host's interrupts end the guest's turn and are
///   taken by the host, with the host's state back.
/// - SHUTDOWN: a guest's triple fault would otherwise shut the CPU down.
/// - IOIO_PROT, MSR_PROT: without them every port and every MSR is the
///   guest's, whatever the maps say.
/// - INVD: invalidating the caches without writing them back loses the
///   host's dirty lines.
/// - INVLPGA: flushes another address space's TLB entries.
/// - VMRUN: `vmrun` refuses a VMCB without it.
/// - VMLOAD, VMSAVE: executed by a guest without virtual VMLOAD they take a
///   host physical address.
/// - STGI, CLGI: the physical GIF, which masks every interrupt the host has.
/// - SKINIT: reinitialises the CPU into a secure loader.
/// - XSETBV: XCR0 is the CPU's, and `vmrun` does not switch it.
/// - #DB and #AC: a guest can make delivering either raise it again --
///   a data breakpoint on the stack its own #DB frame is pushed to, an #AC
///   handler whose frame is misaligned -- and the CPU then loops inside the
///   delivery, where no instruction ever ends and so no interrupt, NMI
///   included, is ever taken: the host has lost the CPU (CVE-2015-8104,
///   CVE-2015-5307). Intercepted, each is an exit, and giving it back to
///   the guest is the policy's to do.
/// - #MC: a machine check is the host's to know of. An intercepted one is
///   not delivered to the host by the CPU, so `run` raises it itself.
///
/// And what the CPU would otherwise do on its own at physical addresses the
/// VMCB names -- AVIC's backing page and tables, SEV's state, virtual
/// VMSAVE -- is off, until this crate owns the pages it would take.
const HOST_MISC1: u32 = vmcb::intercept::misc1::INTR
    | vmcb::intercept::misc1::NMI
    | vmcb::intercept::misc1::SMI
    | vmcb::intercept::misc1::INIT
    | vmcb::intercept::misc1::INVD
    | vmcb::intercept::misc1::INVLPGA
    | vmcb::intercept::misc1::IOIO_PROT
    | vmcb::intercept::misc1::MSR_PROT
    | vmcb::intercept::misc1::SHUTDOWN;
const HOST_MISC2: u32 = vmcb::intercept::misc2::VMRUN
    | vmcb::intercept::misc2::VMLOAD
    | vmcb::intercept::misc2::VMSAVE
    | vmcb::intercept::misc2::STGI
    | vmcb::intercept::misc2::CLGI
    | vmcb::intercept::misc2::SKINIT
    | vmcb::intercept::misc2::XSETBV;
pub const VECTOR_DB: u32 = 1;
pub const VECTOR_AC: u32 = 17;
pub const VECTOR_MC: u32 = 18;
const HOST_EXCEPTIONS: u32 = (1 << VECTOR_DB) | (1 << VECTOR_AC) | (1 << VECTOR_MC);

/// CR4.LA57: five levels of page table.
const CR4_LA57: u64 = 1 << 12;

/// Why a guest was not entered on `cpu`, the CPU the entry found itself on.
#[derive(Clone, Copy, Debug)]
pub enum NotRun {
    /// The extension is not on there -- or is on with a host save area this
    /// hypervisor did not give it.
    Off { cpu: u32 },
    /// It translates with five levels of page table. A nested table is
    /// walked in the host's own paging mode, and the one this hypervisor
    /// builds has four: walked as five, its top level would be taken for a
    /// fifth and the guest's own memory for the last -- a guest choosing
    /// its own host physical addresses. Nothing in this kernel turns LA57
    /// on; this is where that would be found out, not a guest.
    FiveLevelPaging { cpu: u32 },
}

/// A guest's CPU as SVM keeps it: the VMCB, the registers `vmrun` leaves to
/// software, and the page the host's own FS, GS, TR, LDTR and syscall MSRs
/// wait in while the guest runs.
pub struct Guest {
    vmcb: VmcbPage,
    host: VmcbPage,
    regs: GuestRegs,
}

impl Guest {
    pub fn new() -> Result<Self> {
        Ok(Self { vmcb: VmcbPage::new()?, host: VmcbPage::new()?, regs: GuestRegs::default() })
    }

    pub fn vmcb(&self) -> &Vmcb {
        self.vmcb.get()
    }

    pub fn vmcb_mut(&mut self) -> &mut Vmcb {
        self.vmcb.get_mut()
    }

    pub fn regs(&self) -> &GuestRegs {
        &self.regs
    }

    pub fn regs_mut(&mut self) -> &mut GuestRegs {
        &mut self.regs
    }

    /// Enter the guest on the CPU this runs on, and come back when it
    /// exits: the exit is in the VMCB's control area. Returns the CPU it ran
    /// on, or [`NotRun`] with the CPU it would have and why not -- checked
    /// with interrupts off, so that what was checked is still so when
    /// `vmrun` runs.
    ///
    /// Before it enters, the VMCB is made safe for the host whatever the
    /// caller put in it: the intercepts in `HOST_MISC1`, `HOST_MISC2` and
    /// `HOST_EXCEPTIONS` set, physical interrupts masked by the host's flag
    /// and not the guest's, nested paging on over `nested_cr3` and SEV, AVIC
    /// and virtual VMSAVE off, the permission maps `perms`, and nothing
    /// assumed clean. And every entry flushes the whole
    /// TLB: guests share an address space identifier until there is an
    /// allocator that knows when one may be reused, and a translation left
    /// over from another guest -- or from this one, to a page since freed
    /// and handed back to the host -- is the host's memory in a guest's
    /// hands.
    ///
    /// # Safety
    /// `nested_cr3` is the top of a nested page table that maps nothing but
    /// memory given to this guest, and the table and that memory stay
    /// allocated until this returns.
    ///
    /// `host_areas[cpu]`, for the CPU this runs on, is 0 or the physical
    /// address of the host save area that CPU was given when SVM was turned
    /// on there, a page that stays allocated until this returns: `vmrun`
    /// writes the host's state into whatever `VM_HSAVE_PA` names, and
    /// `#vmexit` reads it back from there.
    pub unsafe fn run(
        &mut self,
        perms: &Permissions,
        nested_cr3: u64,
        host_areas: &[AtomicU64],
    ) -> core::result::Result<u32, NotRun> {
        {
            /* Not the address space identifier: an ASID of 0, the host's,
             * is refused by `vmrun` itself, and the refusal hands the guest
             * nothing. */
            let c = &mut self.vmcb.get_mut().control;
            c.intercept_misc1 |= HOST_MISC1;
            c.intercept_misc2 |= HOST_MISC2;
            c.intercept_exceptions |= HOST_EXCEPTIONS;
            c.int_ctl = (c.int_ctl | vmcb::int_ctl::V_INTR_MASKING)
                & !(vmcb::int_ctl::AVIC_ENABLE | vmcb::int_ctl::X2AVIC_ENABLE);
            c.nested_ctl = vmcb::nested::NP_ENABLE;
            c.nested_cr3 = nested_cr3;
            c.iopm_base_pa = perms.iopm.phys();
            c.msrpm_base_pa = perms.msrpm.phys();
            c.virt_ext = 0;
            c.clean = 0;
            c.tlb_control = vmcb::tlb::FLUSH_ALL;
        }

        let guest = self.vmcb.phys();
        let host = self.host.phys();
        let regs: *mut GuestRegs = &mut self.regs;
        let vmcb = &self.vmcb;
        kcore::cpu::with_interrupts_off(|cpu| {
            let expected = host_areas.get(cpu as usize).map_or(0, |a| a.load(Ordering::Acquire));
            /* SVM on, and the host save area the one this CPU was given --
             * not one some earlier load left behind -- and with interrupts
             * off, still so until the stub returns: turning SVM off is an
             * IPI, which waits. */
            if expected == 0 || !enabled() || host_area() != expected {
                return Err(NotRun::Off { cpu });
            }
            if cpu::read_cr4() & CR4_LA57 != 0 {
                return Err(NotRun::FiveLevelPaging { cpu });
            }
            /* The two VMCBs are pages this guest owns, `regs` is its own
             * field, and the checks above are the rest of what the stub
             * needs. */
            unsafe { vmrun_stub(guest, regs, host) };

            /* Still on this CPU, with interrupts off: an intercepted machine
             * check is one the host's handler has not seen, and the CPU will
             * not deliver it. Raising the vector hands it over as though it
             * had been -- to a handler that panics, which is what a machine
             * check in this kernel is. */
            if vmcb.get().control.exit_code == vmcb::exit::EXCP_BASE + VECTOR_MC as u64 {
                /* Vector 18's gate, as the CPU would have used it; the
                 * handler takes no error code, and `int` pushes none. */
                unsafe { asm!("int 0x12") };
            }
            Ok(cpu)
        })
    }
}

/// Enter the guest whose VMCB is at `guest_vmcb` with the registers in
/// `regs`, and come back at its next `#vmexit` with the registers stored
/// back into `regs`.
///
/// Around `vmrun`, with GIF clear so that nothing at all -- no interrupt, no
/// NMI -- runs until the host's state is back whole:
///
/// - `vmsave` into `host_vmcb`, `vmload` from the guest's: `vmrun` switches
///   neither FS, GS, TR and LDTR nor the syscall MSRs, and this kernel keeps
///   its per-CPU data at the GS base. A guest's GS left in place after an
///   exit is the next interrupt handler reading another CPU's data.
/// - `sti` just before `vmrun`: with the VMCB's interrupt masking on, it is
///   the host's IF that `vmrun` saves which decides whether a physical
///   interrupt ends the guest's turn -- it has to be set -- and with GIF
///   clear it lets nothing in here.
/// - `cli` just after, so that `stgi` at the end opens GIF with interrupts
///   still off; the interrupt that ended the guest's turn is taken when the
///   caller turns them back on.
///
/// The SysV callee-saved registers are the host's to keep and the guest
/// overwrites them all, so they go on the stack, which `#vmexit` gives back
/// with RSP. So does RAX, the VMCB address `vmrun` was given.
///
/// # Safety
/// SVM is on for this CPU, its host save area is live, interrupts are off,
/// both VMCBs are pages that stay allocated, and `regs` is valid for reads
/// and writes: what [`Guest::run`] checks and owns.
#[unsafe(naked)]
unsafe extern "C" fn vmrun_stub(guest_vmcb: u64, regs: *mut GuestRegs, host_vmcb: u64) {
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        /* What is needed after the exit: the host VMCB, then `regs`. */
        "push rdx",
        "push rsi",
        "clgi",
        "mov rax, rdx",
        "vmsave rax",
        "mov rax, rdi",
        "vmload rax",
        "mov rbx, [rsi + {rbx}]",
        "mov rcx, [rsi + {rcx}]",
        "mov rdx, [rsi + {rdx}]",
        "mov rdi, [rsi + {rdi}]",
        "mov rbp, [rsi + {rbp}]",
        "mov r8, [rsi + {r8}]",
        "mov r9, [rsi + {r9}]",
        "mov r10, [rsi + {r10}]",
        "mov r11, [rsi + {r11}]",
        "mov r12, [rsi + {r12}]",
        "mov r13, [rsi + {r13}]",
        "mov r14, [rsi + {r14}]",
        "mov r15, [rsi + {r15}]",
        "mov rsi, [rsi + {rsi}]",
        "sti",
        "vmrun rax",
        /* #vmexit: RAX and RSP are the host's again, every other register
         * the guest's, GIF clear. */
        "cli",
        "push rsi",
        "mov rsi, [rsp + 8]",
        "mov [rsi + {rbx}], rbx",
        "mov [rsi + {rcx}], rcx",
        "mov [rsi + {rdx}], rdx",
        "mov [rsi + {rdi}], rdi",
        "mov [rsi + {rbp}], rbp",
        "mov [rsi + {r8}], r8",
        "mov [rsi + {r9}], r9",
        "mov [rsi + {r10}], r10",
        "mov [rsi + {r11}], r11",
        "mov [rsi + {r12}], r12",
        "mov [rsi + {r13}], r13",
        "mov [rsi + {r14}], r14",
        "mov [rsi + {r15}], r15",
        "pop rdi",
        "mov [rsi + {rsi}], rdi",
        "vmsave rax",
        "mov rax, [rsp + 8]",
        "vmload rax",
        "stgi",
        "add rsp, 16",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
        rbx = const offset_of!(GuestRegs, rbx),
        rcx = const offset_of!(GuestRegs, rcx),
        rdx = const offset_of!(GuestRegs, rdx),
        rsi = const offset_of!(GuestRegs, rsi),
        rdi = const offset_of!(GuestRegs, rdi),
        rbp = const offset_of!(GuestRegs, rbp),
        r8 = const offset_of!(GuestRegs, r8),
        r9 = const offset_of!(GuestRegs, r9),
        r10 = const offset_of!(GuestRegs, r10),
        r11 = const offset_of!(GuestRegs, r11),
        r12 = const offset_of!(GuestRegs, r12),
        r13 = const offset_of!(GuestRegs, r13),
        r14 = const offset_of!(GuestRegs, r14),
        r15 = const offset_of!(GuestRegs, r15),
    );
}
