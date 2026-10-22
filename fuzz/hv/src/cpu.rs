//! The CPU, as the fuzzer has it: hvarch's types -- the VMCB's layout from
//! its own source -- and in place of AMD-V and VT-x a CPU whose guest is a
//! script (`vm::set_guest`). Entering it asks the script what the guest does
//! next: an exit, with whatever registers and memory the guest had set up
//! for it. What the run loop asks of the CPU in between -- an event
//! injected, a window requested -- is kept as the real backends keep it, and
//! the rules both of them live by are checked where they are broken: an
//! interrupt injected into a guest that cannot take one, or over an event
//! already on its way in, is an interrupt lost.

#![allow(clippy::new_without_default)]

/// hvarch's error, every variant of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    NoExtension,
    FirmwareDisabled,
    NoNestedPaging,
    NoMemory,
    EnableFailed,
    HostState,
    NoSuchCpu,
    NotImplemented,
    Unmapped,
    BadAddress,
    NoUnrestrictedGuest,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    Svm,
    Vmx,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ext {
    Svm,
    Vmx,
}

pub struct Caps {
    vendor: Vendor,
}

impl Caps {
    pub fn vendor(&self) -> Vendor {
        self.vendor
    }
}

#[path = "../../../src/rust/hvarch/src/x86/svm/vmcb.rs"]
pub mod vmcb;

/// `kcore::cpu::timer_at`: the host CPU's own timer armed to interrupt it by
/// `deadline_ns` -- the vCPU on this thread, entered, leaves its guest at it
/// with a host interrupt's exit, as on a host (`vm::Cpu::enter`). Nothing
/// for a deadline past, as the kernel has it.
pub fn timer_at(deadline_ns: u64) {
    vm::arm_timer(deadline_ns);
}

pub mod x86 {
    pub mod cpu {
        use std::sync::atomic::{AtomicU64, Ordering};

        /// A host CPU's CPUID leaf, as policy.rs asks for one.
        #[derive(Clone, Copy, Debug)]
        pub struct CpuidResult {
            pub eax: u32,
            pub ebx: u32,
            pub ecx: u32,
            pub edx: u32,
        }

        /// What every AMD64 CPU's leaf 1 EDX has: FPU, DE, PSE, TSC, MSR,
        /// PAE, MCE, CX8, APIC, SEP, MTRR, PGE, MCA, CMOV, PAT, PSE-36,
        /// CLFLUSH, MMX, FXSR, SSE and SSE2. A host without PAE has no long
        /// mode, and its guest's CR4 -- PAE set from its start -- could take
        /// no write at all.
        const AMD64_LEAF1_EDX: u32 = 0x078B_FBFD;

        /// The host CPU the policy asks about: one an input, set before the
        /// input makes a guest (`set_host`) -- which vendor's, the top of
        /// each range of leaves, and every bit of every leaf made up from
        /// it, so that over a campaign each bit is the host's now and then,
        /// and the policy has to keep from a guest every one it does not
        /// name.
        static HOST: AtomicU64 = AtomicU64::new(0);

        /// The tops of the basic and the extended range a host may have:
        /// Zen 2's (0x10 and 0x8000_001F), Raptor Lake's (0x20 and
        /// 0x8000_0008), Zen 4's (0x10 and 0x8000_0028), and around them --
        /// below the policy's own tops and above.
        const BASIC_TOPS: [u32; 6] = [0x0D, 0x10, 0x15, 0x16, 0x20, 0x28];
        const EXT_TOPS: [u32; 5] = [0x8000_0008, 0x8000_001F, 0x8000_0021, 0x8000_0026, 0x8000_0028];

        pub fn set_host(seed: u64) {
            HOST.store(seed, Ordering::Relaxed);
        }

        fn host() -> u64 {
            HOST.load(Ordering::Relaxed)
        }

        /// Whether this input's host is an Intel CPU; an AMD one otherwise.
        pub fn host_is_intel() -> bool {
            host() & 1 != 0
        }

        pub fn vendor_id() -> [u8; 12] {
            if host_is_intel() { *b"GenuineIntel" } else { *b"AuthenticAMD" }
        }

        /// Whether the host runs its string instructions fast: an Intel
        /// host's choice, made up with the rest of it.
        pub fn fast_strings() -> bool {
            host_is_intel() && host() & 2 != 0
        }

        /// The host's IA32_ARCH_CAPABILITIES, where its CPUID offers it (leaf
        /// 7, EDX bit 29): every bit made up with the rest of the host.
        pub fn arch_capabilities() -> Option<u64> {
            const EDX_ARCH_CAPABILITIES: u32 = 1 << 29;
            if cpuid_count(7, 0)?.edx & EDX_ARCH_CAPABILITIES == 0 {
                return None;
            }
            Some(host().rotate_left(29) ^ 0x5851_F42D_4C95_7F2D)
        }

        /// The host's TSC: a count that goes up with every read.
        pub fn rdtsc() -> u64 {
            static TSC: AtomicU64 = AtomicU64::new(0);
            TSC.fetch_add(1, Ordering::Relaxed)
        }

        /// The tops of this input's host's two ranges.
        pub fn host_tops() -> (u32, u32) {
            let h = host();
            (BASIC_TOPS[(h >> 4) as usize % BASIC_TOPS.len()], EXT_TOPS[(h >> 8) as usize % EXT_TOPS.len()])
        }

        pub fn cpuid_count(leaf: u32, sub: u32) -> Option<CpuidResult> {
            let (basic_top, ext_top) = host_tops();
            let (base, top) = if leaf >= 0x8000_0000 { (0x8000_0000, ext_top) } else { (0, basic_top) };
            if leaf > top {
                return None;
            }
            let mut h = (u64::from(leaf) << 32 | u64::from(sub)) ^ 0x9E37_79B9_7F4A_7C15 ^ host().rotate_left(17);
            let mut word = || {
                h = (h ^ (h >> 31)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                h = h ^ (h >> 29);
                h as u32
            };
            let mut r = CpuidResult { eax: word(), ebx: word(), ecx: word(), edx: word() };
            if leaf == base {
                /* The top of the range, and the vendor -- which AMD's
                 * extended range says again and Intel's does not. */
                let v = vendor_id();
                let word = |i: usize| u32::from_le_bytes([v[i], v[i + 1], v[i + 2], v[i + 3]]);
                let says = base == 0 || !host_is_intel();
                r = CpuidResult { eax: top, ebx: if says { word(0) } else { 0 }, ecx: if says { word(8) } else { 0 },
                                  edx: if says { word(4) } else { 0 } };
            }
            if leaf == 1 {
                r.edx |= AMD64_LEAF1_EDX;
            }
            Some(r)
        }
    }

    pub mod svm {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

        pub use crate::cpu::vmcb;

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

        /// What takes a vCPU out of its guest. There is no guest to take it
        /// out of here; kicks are counted. Whether the vCPU is in its guest,
        /// or on its way in, as hvarch's keeps it: what a posted interrupt's
        /// notification goes to (`notify`), the CPU model taking what was
        /// posted when the vCPU's turn comes again (`vm::Cpu::enter`).
        pub struct Kick {
            sent: AtomicU64,
            in_guest: AtomicBool,
        }

        impl Kick {
            pub fn new() -> Kick {
                Kick { sent: AtomicU64::new(0), in_guest: AtomicBool::new(false) }
            }
            pub fn sent(&self) -> u64 {
                self.sent.load(Ordering::Relaxed)
            }
            pub fn prepare(&self) {
                self.in_guest.store(true, Ordering::SeqCst);
            }
            pub fn cancel(&self) {
                self.in_guest.store(false, Ordering::SeqCst);
            }
            pub fn kick(&self) {
                self.sent.fetch_add(1, Ordering::Relaxed);
            }
            pub fn notify(&self) -> bool {
                self.in_guest.load(Ordering::SeqCst)
            }
            /// The vCPU left its guest (hvarch's, after an exit).
            pub fn left(&self) {
                self.in_guest.store(false, Ordering::SeqCst);
            }
        }
    }

    /// hvarch's posted-interrupt descriptor: the same atomics, as the run
    /// loop and the posting CPUs use it.
    pub mod vmx {
        use std::sync::atomic::{AtomicU64, Ordering};

        pub struct PostedInterrupts {
            pir: [AtomicU64; 4],
            control: AtomicU64,
        }

        const PI_ON: u64 = 1;

        impl PostedInterrupts {
            pub fn new() -> PostedInterrupts {
                PostedInterrupts { pir: [const { AtomicU64::new(0) }; 4], control: AtomicU64::new(0) }
            }
            pub fn post(&self, vector: u8) -> bool {
                self.pir[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::SeqCst);
                self.control.fetch_or(PI_ON, Ordering::SeqCst) & PI_ON == 0
            }
            pub fn take(&self) -> Option<[u64; 4]> {
                if self.control.fetch_and(!PI_ON, Ordering::SeqCst) & PI_ON == 0 {
                    return None;
                }
                let mut words = [0u64; 4];
                for (w, p) in words.iter_mut().zip(self.pir.iter()) {
                    *w = p.swap(0, Ordering::SeqCst);
                }
                Some(words)
            }
            /// The model's: whether a notification is outstanding.
            pub fn outstanding(&self) -> bool {
                self.control.load(Ordering::SeqCst) & PI_ON != 0
            }
        }
    }
}

/// hv's own `svm`: the exits as the backends decode them, as they are there.
pub mod svm {
    #[derive(Clone, Copy, Debug)]
    pub struct Io {
        pub port: u16,
        pub size: u8,
        pub input: bool,
        pub string: bool,
        pub rep: bool,
        pub next_rip: u64,
    }

    #[derive(Clone, Copy, Debug)]
    pub enum Exit {
        Host,
        Kicked,
        Io(Io),
        Hlt,
        Cpuid,
        Msr { write: bool },
        Hypercall,
        IrqWindow,
        NmiWindow,
        NestedFault { gpa: u64, error: u64 },
        Exception { vector: u8, error: Option<u32> },
        MachineCheck,
        Shutdown,
        Invalid,
        Cr8 { write: bool, gpr: u8 },
        Cr0Write { value: u64 },
        Cr4 { write: bool, gpr: Option<u8> },
        Dr { write: bool, dr: u8, gpr: Option<u8> },
        Eoi { vector: u8 },
        Pause,
        Undefined,
        Other(u64),
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct LongMode {
        pub entry: u64,
        pub stack: u64,
        pub cr3: u64,
        pub gdt: u64,
        pub gdt_limit: u16,
        pub idt_limit: u16,
        pub code_selector: u16,
        pub data_selector: u16,
        pub tss_selector: u16,
        pub tss: u64,
    }
}

pub mod machine {
    use crate::{Caps, Ext, Result, Vendor};

    pub struct Machine {
        caps: Caps,
        /// VT-x's: its CPUs have a virtual APIC, and posted-interrupt
        /// processing -- what the target makes up of a machine.
        pub vapic: bool,
        pub posted: bool,
    }

    impl Machine {
        pub fn new(vendor: Vendor) -> Machine {
            Machine { caps: Caps { vendor }, vapic: false, posted: false }
        }
        pub fn caps(&self) -> &Caps {
            &self.caps
        }
        pub fn ext(&self) -> Result<Ext> {
            Ok(match self.caps.vendor {
                Vendor::Svm => Ext::Svm,
                Vendor::Vmx => Ext::Vmx,
            })
        }
    }
}

pub mod vm {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::machine::Machine;
    use crate::memory::GuestMemory;
    use crate::svm::{Exit, Io, LongMode};
    use crate::x86::svm::vmcb::{attrib, Save, Segment, Vmcb};
    use crate::x86::svm::{GuestRegs, Kick};

    #[derive(Clone, Copy, Debug)]
    pub enum Refusal {
        Vmcb(&'static str),
        NotOn(u32),
        FiveLevelPaging(u32),
        Flush(u32),
        NotItsMemory,
        Controls { cpu: u32, field: &'static str, missing: u32 },
    }

    /// An event on its way into the guest.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Event {
        Interrupt(u8),
        Nmi,
        Exception(u8),
    }

    const RFLAGS_IF: u64 = 1 << 9;
    const EFER_LMA: u64 = 1 << 10;
    const CR4_PSE: u64 = 1 << 4;
    const CR4_PAE: u64 = 1 << 5;
    const CR4_MCE: u64 = 1 << 6;
    const CR4_PGE: u64 = 1 << 7;
    /// DR6 and DR7 as reset leaves them, as both backends start a CPU.
    const DR6_RESET: u64 = 0xFFFF_0FF0;
    const DR7_RESET: u64 = 0x400;

    /// A guest CPU's state, as the run loop sees it through a backend.
    pub struct Backend {
        save: Save,
        regs: GuestRegs,
        /// Queued for the next entry.
        pub event: Option<Event>,
        pub irq_window: bool,
        pub nmi_window: bool,
        /// In the shadow of an STI or a MOV SS.
        pub shadow: bool,
        /// In its NMI handler: no NMI until its IRET.
        pub nmi_masked: bool,
        cr8: u8,
        /// Every event the guest took, in order.
        pub taken: Vec<Event>,
        /// VT-x's, as the machine's vendor says: whose exits name the
        /// register of a `mov` of a control or a debug register, and the
        /// instruction's length (`exit_len`), where AMD-V's have it decoded.
        vmx: bool,
        /// The length of the instruction the guest stopped at, as a VT-x
        /// exit says it: set by the script with the exit.
        exit_len: u64,
        /// The CR4 the guest believes it has, as svm.rs keeps it: the
        /// register, `save.cr4`, has MCE besides.
        cr4: u64,
        /// The guest's DR0-DR3, as svm.rs keeps them apart from the CPU's.
        debug_addrs: [u64; 4],
        /// VT-x's virtual APIC, as the machine is said to have one: CR8 its
        /// VTPR, never an exit, and while the run loop has virtual-interrupt
        /// delivery on (`apicv`), this model is the CPU that delivers VIRR's
        /// interrupts, performs the guest's EOIs, TPR writes and self-IPIs,
        /// and stops it after an EOI the EOI-exit bitmap names.
        pub vapic: bool,
        apicv: bool,
        virr: [u32; 8],
        visr: [u32; 8],
        vtpr: u8,
        eoi_exit: [u64; 4],
        /// Entered with virtual-interrupt delivery on, and what the CPU did
        /// not yet taken back (`take_apic`); and what was, for the next
        /// load to keep -- every request and service the CPU left, but
        /// across an INIT.
        apicv_entered: bool,
        stored: Option<([u32; 8], [u32; 8])>,
        /// Posted interrupts, as the machine is said to have them: where the
        /// run loop's CPUs post this one's, which the model takes into VIRR
        /// when the vCPU's turn comes in its guest with virtual-interrupt
        /// delivery on, and exits for otherwise, as a notification does.
        pub pi_capable: bool,
        posted: Option<Arc<crate::x86::vmx::PostedInterrupts>>,
    }

    /// What the modelled virtual APIC did, for `HV_FUZZ_STATS`.
    pub static APICV: Mutex<std::collections::BTreeMap<&'static str, u64>> =
        Mutex::new(std::collections::BTreeMap::new());

    fn apicv_did(what: &'static str) {
        *APICV.lock().unwrap_or_else(|e| e.into_inner()).entry(what).or_insert(0) += 1;
    }

    impl Default for Backend {
        fn default() -> Backend {
            Backend::new()
        }
    }

    impl Backend {
        fn new() -> Backend {
            let vmcb: Vmcb = crate::pod::zeroed();
            Backend {
                save: vmcb.save,
                regs: GuestRegs::default(),
                event: None,
                irq_window: false,
                nmi_window: false,
                shadow: false,
                nmi_masked: false,
                cr8: 0,
                taken: Vec::new(),
                vmx: false,
                exit_len: 0,
                cr4: 0,
                debug_addrs: [0; 4],
                vapic: false,
                apicv: false,
                virr: [0; 8],
                visr: [0; 8],
                vtpr: 0,
                eoi_exit: [0; 4],
                apicv_entered: false,
                stored: None,
                pi_capable: false,
                posted: None,
            }
        }

        pub fn save(&self) -> &Save {
            &self.save
        }
        pub fn save_mut(&mut self) -> &mut Save {
            &mut self.save
        }
        pub fn save_and_regs_mut(&mut self) -> (&mut Save, &mut GuestRegs) {
            (&mut self.save, &mut self.regs)
        }
        pub fn regs(&self) -> &GuestRegs {
            &self.regs
        }
        pub fn regs_mut(&mut self) -> &mut GuestRegs {
            &mut self.regs
        }
        /// As vm.rs's: register `n`, set for an instruction performed for
        /// the guest.
        pub fn set_gpr(&mut self, n: u8, value: u64) {
            *crate::mmio::gpr_mut(&mut self.save, &mut self.regs, n) = value;
        }
        /// The script's: the length of the instruction its next exit stopped
        /// at, which VT-x's exit says.
        pub fn set_exit_len(&mut self, len: u64) {
            self.exit_len = len;
        }
        /// As vm.rs's: the length the exit says -- VT-x's -- or None.
        pub fn exit_insn_len(&self) -> Option<u64> {
            self.vmx.then_some(self.exit_len)
        }
        /// As svm.rs's and vmx.rs's: long mode active, and CS's L bit.
        pub fn in_64bit_code(&self) -> bool {
            self.save.efer & EFER_LMA != 0 && self.save.cs.attrib & attrib::L != 0
        }

        /// As svm.rs's: flat 64-bit code and data, paging on.
        pub fn long_mode(&mut self, l: &LongMode) {
            let flat = 0xFFFF_FFFF;
            let code = Segment {
                selector: l.code_selector,
                attrib: attrib::P | attrib::S | attrib::CODE | attrib::WRITE_OR_READ | attrib::ACCESSED | attrib::L
                    | attrib::G,
                limit: flat,
                base: 0,
            };
            let data = Segment {
                selector: l.data_selector,
                attrib: attrib::P | attrib::S | attrib::WRITE_OR_READ | attrib::ACCESSED | attrib::DB | attrib::G,
                limit: flat,
                base: 0,
            };
            let s = &mut self.save;
            s.cs = code;
            s.ds = data;
            s.es = data;
            s.ss = data;
            s.fs = data;
            s.gs = data;
            s.efer = (1 << 12) | (1 << 8) | (1 << 10);
            s.cr0 = 0x8005_0033;
            s.cr3 = l.cr3;
            s.cr4 = CR4_PAE | CR4_MCE;
            s.dr6 = DR6_RESET;
            s.dr7 = DR7_RESET;
            s.rip = l.entry;
            s.rsp = l.stack;
            s.rflags = 2;
            self.cr4 = CR4_PAE | CR4_MCE;
            self.debug_addrs = [0; 4];
        }

        /// As svm.rs's: the CR4 the guest believes it has.
        pub fn cr4(&self) -> u64 {
            self.cr4
        }

        /// As svm.rs's and vmx.rs's: `value` the guest's CR4, and with MCE
        /// the register's -- held to what a write the run loop performed may
        /// be, by rules of the fuzzer's own: no bit the guest's CPUID does not
        /// offer, PAE kept in long mode, and the TLB flushed where a bit a CPU
        /// translates by changed.
        pub fn set_cr4(&mut self, value: u64, flush: bool) {
            let allowed = crate::policy::cr4_allowed();
            invariant!(value & !allowed == 0, "CR4 written with {:#x}: bits {:#x} the guest's CPUID does not offer",
                       value, value & !allowed);
            invariant!(self.save.efer & EFER_LMA == 0 || value & CR4_PAE != 0,
                       "CR4 written with {:#x}: PAE cleared in long mode", value);
            invariant!(flush || (self.cr4 ^ value) & (CR4_PSE | CR4_PAE | CR4_PGE) == 0,
                       "CR4 written from {:#x} to {:#x}, and the guest's TLB not flushed", self.cr4, value);
            self.cr4 = value;
            self.save.cr4 = value | CR4_MCE;
        }

        /// As svm.rs's and vmx.rs's: the CR4 the CPU runs the guest with.
        pub fn cr4_register(&self) -> u64 {
            self.save.cr4
        }

        /// The script's, under VT-x: a `mov` to CR4 the CPU performed itself,
        /// changing only bits the guest owns -- no exit, and what the next
        /// exit for a control register reads back.
        pub fn cpu_wrote_cr4(&mut self, value: u64) {
            invariant!(self.vmx, "a CR4 write the CPU performed without an exit, under AMD-V, which stops every one");
            self.cr4 = value;
            self.save.cr4 = value | CR4_MCE;
        }

        /// As svm.rs's: the guest's DR0-DR3.
        pub fn debug_addrs(&self) -> [u64; 4] {
            self.debug_addrs
        }

        /// As svm.rs's and vmx.rs's: `addrs` the guest's DR0-DR3.
        pub fn set_debug_addrs(&mut self, addrs: [u64; 4]) {
            self.debug_addrs = addrs;
        }

        /// As vm.rs's: `value` the guest's DR7 -- held to what DR7 can
        /// hold: bit 10 set, 12, 14, 15 and everything above 31 clear.
        pub fn set_dr7(&mut self, value: u64) {
            invariant!(value & 0x400 != 0 && value & !0xFFFF_2FFF == 0 && value >> 32 == 0,
                       "DR7 made {:#x}, which DR7 cannot hold", value);
            self.save.dr7 = value;
        }

        fn step(&mut self, len: u64) {
            self.save.rip = self.save.rip.wrapping_add(len);
            self.shadow = false;
        }
        pub fn skip_io(&mut self, io: &Io) {
            self.save.rip = io.next_rip;
            self.shadow = false;
        }
        pub fn skip_emulated(&mut self, len: u64) {
            self.step(len);
        }
        pub fn skip_cpuid(&mut self) {
            self.step(2);
        }
        pub fn skip_msr(&mut self) {
            self.step(2);
        }
        pub fn skip_vmmcall(&mut self) {
            self.step(3);
        }
        pub fn skip_hlt(&mut self) {
            self.step(1);
        }
        pub fn skip_pause(&mut self) {
            self.step(2);
        }
        pub fn skip_wbinvd(&mut self) {
            self.step(2);
        }
        /// As VT-x's: CR8 to or from a register, and stepped past.
        pub fn cr8_access(&mut self, write: bool, gpr: u8) {
            let r = &mut self.regs;
            let reg = match gpr & 15 {
                1 => &mut r.rcx,
                2 => &mut r.rdx,
                3 => &mut r.rbx,
                5 => &mut r.rbp,
                6 => &mut r.rsi,
                7 => &mut r.rdi,
                8 => &mut r.r8,
                15 => &mut r.r15,
                _ => &mut r.r9,
            };
            if write {
                self.cr8 = (*reg & 0xF) as u8;
            } else {
                *reg = u64::from(self.cr8);
            }
            self.step(4);
        }
        pub fn runs_real_mode(&self) -> bool {
            true
        }
        pub fn start_at_sipi(&mut self, vector: u8) {
            self.save.cs = Segment { selector: u16::from(vector) << 8, attrib: attrib::P | attrib::S | attrib::CODE,
                                     limit: 0xFFFF, base: u64::from(vector) << 12 };
            self.save.rip = 0;
            self.save.cr0 = 0x10;
            self.save.cr4 = CR4_MCE;
            self.save.efer = 1 << 12;
            self.save.rflags = 2;
            self.save.dr6 = DR6_RESET;
            self.save.dr7 = DR7_RESET;
            self.cr4 = 0;
            self.debug_addrs = [0; 4];
        }
        pub fn init_reset(&mut self) {
            self.event = None;
            self.shadow = false;
            self.irq_window = false;
            self.nmi_window = false;
            self.nmi_masked = false;
            self.cr8 = 0;
            self.vtpr = 0;
            /* What the CPU left is the APIC's no more: INIT resets it. */
            self.stored = None;
            self.apicv_entered = false;
        }
        pub fn cr8(&self) -> u8 {
            if self.vapic { self.vtpr >> 4 } else { self.cr8 }
        }
        pub fn set_cr8(&mut self, value: u8) {
            invariant!(value < 16, "CR8 set to {:#x}", value);
            if self.vapic {
                self.vtpr = value << 4;
            } else {
                self.cr8 = value;
            }
        }

        /* VT-x's virtual APIC, as vmx.rs's `Vcpu` hands it over and the CPU
         * works it (Intel SDM vol. 3C, 30.1-30.5). */
        pub fn has_vapic(&self) -> bool {
            self.vapic
        }
        pub fn posted_capable(&self) -> bool {
            self.vapic && self.pi_capable
        }
        pub fn set_posted(&mut self, posted: Arc<crate::x86::vmx::PostedInterrupts>) -> bool {
            if !self.posted_capable() {
                return false;
            }
            /* Its vCPU's thread, which the run loop takes it on. */
            MY_POSTED.with(|p| *p.borrow_mut() = Some(posted.clone()));
            self.posted = Some(posted);
            true
        }
        /// The model's, as the vCPU's turn comes again in its guest: a
        /// notification outstanding is taken by the CPU -- what was posted
        /// into VIRR -- with virtual-interrupt delivery on, and is an exit
        /// otherwise, an external interrupt's.
        pub fn posted_arrival(&mut self) -> Option<Exit> {
            let p = self.posted.as_ref()?;
            if !p.outstanding() {
                return None;
            }
            if !self.apicv {
                return Some(Exit::Host);
            }
            if let Some(words) = p.take() {
                for (i, w) in words.iter().enumerate() {
                    self.virr[2 * i] |= *w as u32;
                    self.virr[2 * i + 1] |= (*w >> 32) as u32;
                }
                apicv_did("posted interrupts taken in the guest");
            }
            None
        }
        pub fn load_apic(&mut self, v: &crate::lapic::Vectors) {
            invariant!(self.vapic, "virtual-interrupt delivery asked of a CPU with no virtual APIC");
            /* What was posted is taken by the run loop before every entry
             * (nothing else runs here between that and this). */
            invariant!(self.posted.as_ref().is_none_or(|p| !p.outstanding()),
                       "an entry with interrupts posted for the CPU and not taken");
            invariant!(!self.apicv_entered,
                       "an entry with virtual-interrupt delivery on whose interrupts were never taken back");
            if let Some((irr, isr)) = self.stored.take() {
                apicv_did("entries held to what the CPU left at the exit before");
                for w in 0..8 {
                    invariant!(irr[w] & !(v.irr[w] | v.isr[w]) & if w == 0 { !0xFFFF } else { !0 } == 0,
                               "vectors {:#x} of word {} the CPU left requested are neither requested nor in service at the next entry",
                               irr[w] & !(v.irr[w] | v.isr[w]), w);
                    invariant!(isr[w] & !v.isr[w] & if w == 0 { !0xFFFF } else { !0 } == 0,
                               "vectors {:#x} of word {} the CPU left in service are not at the next entry",
                               isr[w] & !v.isr[w], w);
                }
            }
            self.virr = v.irr;
            self.visr = v.isr;
            self.vtpr = v.tpr;
            self.eoi_exit = v.eoi_exit();
            self.apicv = true;
        }
        pub fn apicv_off(&mut self) {
            invariant!(!self.apicv_entered,
                       "an entry with virtual-interrupt delivery on whose interrupts were never taken back");
            self.apicv = false;
            self.stored = None;
        }
        pub fn take_apic(&mut self) -> ([u32; 8], [u32; 8], u8) {
            self.apicv_entered = false;
            self.stored = Some((self.virr, self.visr));
            (self.virr, self.visr, self.vtpr)
        }
        /// The guest's CPU entered: an entry that hands back what the CPU did.
        pub fn entered(&mut self) {
            if self.apicv {
                self.apicv_entered = true;
            }
        }
        /// Whether the guest's writes of x2APIC register `msr` are the CPU's
        /// to perform: TPR, EOI and SELF_IPI, under virtual-interrupt
        /// delivery.
        pub fn virtualizes(&self, msr: u32) -> bool {
            self.apicv && matches!(msr, 0x808 | 0x80B | 0x83F)
        }
        /// The guest's write of `value` to `msr`, which the CPU performs
        /// (`virtualizes`): nothing for one it faults -- a #GP the guest
        /// takes itself -- and an EOI-induced exit for an EOI of a vector the
        /// bitmap names.
        pub fn virtual_wrmsr(&mut self, msr: u32, value: u64) -> Option<Exit> {
            match msr {
                0x808 if value >> 8 == 0 => {
                    apicv_did("TPR writes performed");
                    self.vtpr = value as u8;
                }
                0x80B if value == 0 => {
                    let vector = highest8(&self.visr)?;
                    self.visr[usize::from(vector / 32)] &= !(1 << (vector % 32));
                    if self.eoi_exit[usize::from(vector / 64)] & (1 << (vector % 64)) != 0 {
                        apicv_did("EOIs performed and exited after, level-triggered");
                        return Some(Exit::Eoi { vector });
                    }
                    apicv_did("EOIs performed");
                }
                0x83F if value >> 8 == 0 => {
                    apicv_did("self-IPIs performed");
                    self.virr[usize::from(value as u8 / 32)] |= 1 << (value as u8 % 32);
                }
                _ => apicv_did("writes faulted"),
            }
            None
        }
        /// A `mov` to CR8 the CPU performed, under the TPR shadow.
        pub fn virtual_cr8(&mut self, value: u8) {
            apicv_did("CR8 writes performed");
            self.vtpr = (value & 0xF) << 4;
        }
        /// What virtual-interrupt delivery hands the guest now, if anything:
        /// the highest vector requested, when its class is above the
        /// processor priority's, the guest takes interrupts and no
        /// interrupt window is asked for -- whose exit comes first.
        fn virtual_pending(&self) -> Option<u8> {
            if !self.apicv || self.irq_window || self.save.rflags & RFLAGS_IF == 0 || self.shadow || self.event.is_some() {
                return None;
            }
            let rvi = highest8(&self.virr)?;
            let svi = highest8(&self.visr).unwrap_or(0);
            let ppr = if self.vtpr >> 4 >= svi >> 4 { self.vtpr } else { svi & 0xF0 };
            (rvi >> 4 > ppr >> 4).then_some(rvi)
        }
        pub fn cr0_write(&mut self, value: u64) {
            self.save.cr0 = value;
            self.step(3);
        }
        pub fn interruptible(&self) -> bool {
            self.save.rflags & RFLAGS_IF != 0 && !self.shadow && self.event.is_none()
        }
        pub fn nmi_allowed(&self) -> bool {
            !self.nmi_masked && !self.shadow && self.event.is_none()
        }
        pub fn event_queued(&self) -> bool {
            self.event.is_some()
        }
        /// Queue `e`, which the loop may only do when nothing else is on its
        /// way in: what was would be lost.
        fn queue(&mut self, e: Event) {
            invariant!(self.event.is_none(), "{:?} injected over {:?}, which is lost", e, self.event);
            self.event = Some(e);
        }
        pub fn inject_extint(&mut self, vector: u8) {
            invariant!(self.save.rflags & RFLAGS_IF != 0 && !self.shadow,
                       "interrupt {:#x} injected into a guest that cannot take one", vector);
            self.queue(Event::Interrupt(vector));
        }
        pub fn inject_nmi(&mut self) {
            invariant!(!self.nmi_masked && !self.shadow, "an NMI injected into a guest that cannot take one");
            self.queue(Event::Nmi);
            self.nmi_masked = true;
        }
        pub fn request_nmi_window(&mut self) {
            self.nmi_window = true;
        }
        pub fn inject_ud(&mut self) {
            self.queue(Event::Exception(6));
        }
        pub fn inject_gp(&mut self) {
            self.queue(Event::Exception(13));
        }
        pub fn reflect_exception(&mut self, vector: u8, _error: Option<u32>) {
            invariant!(vector == 1 || vector == 17, "exception {} given back to the guest", vector);
            self.queue(Event::Exception(vector));
        }
        pub fn skip_icebp(&mut self) {
            self.step(1);
        }
        pub fn request_irq_window(&mut self) {
            self.irq_window = true;
        }
        pub fn clear_irq_window(&mut self) {
            self.irq_window = false;
        }
        pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
            write!(out, "rip {:#x} rflags {:#x} event {:?}", self.save.rip, self.save.rflags, self.event)
        }
        pub fn read_whole_state(&mut self) -> bool {
            true
        }

        /// The guest entered: what was queued for it, taken -- the one way
        /// an event leaves the queue, but for INIT's reset -- or, with none,
        /// what virtual-interrupt delivery hands it.
        pub fn take_event(&mut self) {
            if let Some(e) = self.event.take() {
                self.taken.push(e);
                self.shadow = false;
                if e == Event::Nmi {
                    self.nmi_window = false;
                }
            } else if let Some(vector) = self.virtual_pending() {
                self.virr[usize::from(vector / 32)] &= !(1 << (vector % 32));
                self.visr[usize::from(vector / 32)] |= 1 << (vector % 32);
                self.taken.push(Event::Interrupt(vector));
                apicv_did("interrupts delivered");
            }
        }
    }

    /// The highest vector set in eight words of a bit each.
    fn highest8(words: &[u32; 8]) -> Option<u8> {
        (0..8).rev().find(|&w| words[w] != 0).map(|w| (w as u32 * 32 + 31 - words[w].leading_zeros()) as u8)
    }

    /// What a guest does at an entry of one of its CPUs.
    pub enum Next {
        /// This exit, having done what it did to its registers and memory.
        Exit(Exit),
        /// Another of its CPUs runs first, this one in its guest meanwhile.
        Switch(usize),
    }

    /// What a guest is here: asked, at each entry of CPU `cpu`, what it does
    /// next -- the event queued for it taken (`Backend::take_event`), unless
    /// the guest's model is of an entry that did not happen or of a fault on
    /// the event's way in; None when it has nothing more to do.
    pub trait Guest: Send {
        fn next(&mut self, cpu: usize, v: &mut Backend, memory: &GuestMemory) -> Option<Next>;
    }

    static GUEST: Mutex<Option<Box<dyn Guest>>> = Mutex::new(None);

    /// The guest that CPUs entered from here on run, until the next is set.
    pub fn set_guest(g: Option<Box<dyn Guest>>) {
        *GUEST.lock().unwrap_or_else(|e| e.into_inner()) = g;
    }

    /* A guest of several CPUs runs each on a thread of its own, as the
     * kernel runs each on a task -- one at a time, the one with the turn: so
     * a run is the same every time, and the order the CPUs run in is decided
     * here and by the script, where a CPU leaves the host. A CPU that enters
     * its guest may be told another runs first (`Next::Switch`); one that
     * waits to be woken hands the turn on as a host would: to a CPU that can
     * run -- in its guest, or rung -- and when none can, to the sleeper due
     * first, the clock jumped to when it is due. Nothing passes the turn
     * round a ring of sleepers, and a wake-up is the one thread's it is for. */
    #[derive(Clone)]
    enum Turn {
        /// Has the turn.
        Running,
        /// In its guest, or not started: runs when handed the turn.
        Ready,
        /// Waiting to be woken -- rung, or at `until`.
        Asleep { until: u64, rung: Arc<AtomicBool> },
        Done,
    }

    struct Baton {
        cpus: Vec<Turn>,
        threads: Vec<Option<std::thread::Thread>>,
        holder: usize,
        /// A thread panicked: the rest leave their runs, for it to be reported.
        abort: bool,
    }

    impl Baton {
        /// Who runs after `me`, which is waiting (Ready or Asleep) or done: a
        /// CPU that can run -- the first after `me` round the ring, `me`
        /// last -- or else the sleeper due first, the clock moved on to when
        /// it is; None when every CPU is done.
        fn pick(&self, me: usize) -> Option<(usize, u64)> {
            let n = self.cpus.len();
            let order = (1..=n).map(|k| (me + k) % n);
            let runnable = |t: &Turn| match t {
                Turn::Ready => true,
                Turn::Asleep { rung, .. } => rung.load(Ordering::Relaxed),
                _ => false,
            };
            if let Some(i) = order.clone().find(|&i| runnable(&self.cpus[i])) {
                return Some((i, 0));
            }
            order.filter_map(|i| match &self.cpus[i] {
                Turn::Asleep { until, .. } => Some((i, *until)),
                _ => None,
            }).min_by_key(|&(_, until)| until)
        }
    }

    static BATON: Mutex<Option<Baton>> = Mutex::new(None);
    /// Turns handed from one thread to another, for `HV_FUZZ_STATS`.
    pub static HANDOFFS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    pub static SLEEPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Entries the host CPU's timer, armed for a guest's timer edge, took
    /// the guest out of (`timer_at`), for `HV_FUZZ_STATS`.
    pub static TIMER_EXITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// The host CPU's timer, as `kcore::cpu::timer_at` arms it: the deadline
    /// it is armed for, sooner ones winning; spent when it fires.
    pub fn arm_timer(deadline_ns: u64) {
        if deadline_ns > crate::time::peek() {
            ARMED.with(|a| a.set(a.get().min(deadline_ns)));
        }
    }

    thread_local! {
        static ARMED: Cell<u64> = const { Cell::new(u64::MAX) };
        static VCPU: Cell<usize> = const { Cell::new(0) };
        /// The descriptor this thread's vCPU takes its posted interrupts
        /// from (`Backend::set_posted`).
        static MY_POSTED: std::cell::RefCell<Option<Arc<crate::x86::vmx::PostedInterrupts>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// A vCPU's task about to wait: what was posted for it is taken by its
    /// loop before it looks at anything -- and here, where nothing runs but
    /// it between that look and this wait, nothing more can have come.
    pub fn check_nothing_posted() {
        MY_POSTED.with(|p| {
            if let Some(p) = p.borrow().as_ref() {
                invariant!(!p.outstanding(), "a CPU waits with an interrupt posted for it and not taken");
            }
        });
    }

    fn baton() -> std::sync::MutexGuard<'static, Option<Baton>> {
        BATON.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This thread runs CPU `i`: and, with the CPUs on threads, it is the
    /// thread a turn for `i` wakes.
    pub fn set_vcpu(i: usize) {
        VCPU.with(|v| v.set(i));
        MY_POSTED.with(|p| *p.borrow_mut() = None);
        if let Some(b) = baton().as_mut() {
            if let Some(t) = b.threads.get_mut(i) {
                *t = Some(std::thread::current());
            }
        }
    }

    /// `n` CPUs on threads of their own from here, the first to run first.
    pub fn begin_threads(n: usize) {
        let mut cpus = vec![Turn::Ready; n];
        cpus[0] = Turn::Running;
        *baton() = Some(Baton { cpus, threads: vec![None; n], holder: 0, abort: false });
    }

    pub fn end_threads() {
        *baton() = None;
    }

    /// Whether the CPUs are on threads of their own.
    pub fn threaded() -> bool {
        baton().is_some()
    }

    /// Wait for CPU `me`'s turn.
    pub fn wait_turn(me: usize) {
        loop {
            {
                let mut b = baton();
                match b.as_mut() {
                    None => return,
                    Some(b) if b.abort => panic!("aborted: another CPU's thread panicked"),
                    Some(b) if b.holder == me => {
                        b.cpus[me] = Turn::Running;
                        return;
                    }
                    Some(_) => {}
                }
            }
            /* An unpark before the park is kept: no turn is missed. */
            std::thread::park();
        }
    }

    /// The turn from `me`, now `state`, to whoever runs next -- `me` itself
    /// when it is the one: true when it is.
    fn hand_on(me: usize, state: Turn) -> bool {
        let mut b = baton();
        let Some(bt) = b.as_mut() else { return true };
        bt.cpus[me] = state;
        let Some((next, due)) = bt.pick(me) else { return true };
        crate::time::reach(due);
        if next == me {
            bt.cpus[me] = Turn::Running;
            return true;
        }
        bt.holder = next;
        let thread = bt.threads[next].clone();
        drop(b);
        HANDOFFS.fetch_add(1, Ordering::Relaxed);
        if let Some(t) = thread {
            t.unpark();
        }
        false
    }

    /// CPU `me` waits to be woken -- by `rung`, or at `until`: the turn goes
    /// on, and this returns when it is this CPU's again.
    pub fn sleep_until(until: u64, rung: &Arc<AtomicBool>) {
        SLEEPS.fetch_add(1, Ordering::Relaxed);
        let me = VCPU.with(|v| v.get());
        if !hand_on(me, Turn::Asleep { until, rung: rung.clone() }) {
            wait_turn(me);
        }
    }

    /// CPU `me`, in its guest, lets `to` run first -- if `to` can run.
    fn switch_to(me: usize, to: usize) {
        {
            let mut b = baton();
            let Some(bt) = b.as_mut() else { return };
            let can = match bt.cpus.get(to) {
                Some(Turn::Ready) => true,
                Some(Turn::Asleep { until, rung }) => rung.load(Ordering::Relaxed) || *until <= crate::time::peek(),
                _ => false,
            };
            if to == me || !can {
                return;
            }
            bt.cpus[me] = Turn::Ready;
            bt.holder = to;
            let thread = bt.threads[to].clone();
            drop(b);
            HANDOFFS.fetch_add(1, Ordering::Relaxed);
            if let Some(t) = thread {
                t.unpark();
            }
        }
        wait_turn(me);
    }

    /// CPU `me`'s thread is done: the next has its turn.
    pub fn finished(me: usize) {
        hand_on(me, Turn::Done);
    }

    /// A thread panicked: every other leaves its run.
    pub fn abort() {
        let threads = match baton().as_mut() {
            Some(b) => {
                b.abort = true;
                b.threads.clone()
            }
            None => Vec::new(),
        };
        for t in threads.into_iter().flatten() {
            t.unpark();
        }
    }

    pub struct Cpu {
        backend: Backend,
    }

    impl Cpu {
        pub fn new(machine: &Machine, _memory: &GuestMemory, _exceptions: u32) -> crate::Result<Cpu> {
            let mut backend = Backend::new();
            backend.vmx = machine.caps().vendor() == crate::Vendor::Vmx;
            backend.vapic = backend.vmx && machine.vapic;
            backend.pi_capable = backend.vapic && machine.posted;
            Ok(Cpu { backend })
        }
        pub fn backend(&self) -> &Backend {
            &self.backend
        }
        pub fn backend_mut(&mut self) -> &mut Backend {
            &mut self.backend
        }

        /// Into the guest, which does what its script says -- to the end of
        /// which it shuts down -- letting the others run first when it says
        /// so.
        pub fn enter(&mut self, memory: &GuestMemory, _machine: &Machine, kick: Option<&Kick>)
            -> core::result::Result<(Exit, u32), Refusal>
        {
            let me = VCPU.with(|v| v.get());
            invariant!(crate::sync::held() == 0, "CPU {} enters its guest with {} of the kernel's locks held", me,
                       crate::sync::held());
            self.backend.entered();
            let exit = loop {
                crate::time::progressed();
                /* A notification of what other CPUs posted while it ran. */
                if let Some(exit) = self.backend.posted_arrival() {
                    break exit;
                }
                /* The host CPU's timer, armed for the guest's timer edge, at
                 * it: a host interrupt, and the edge goes in after it. */
                if ARMED.with(|a| a.get()) <= crate::time::peek() {
                    ARMED.with(|a| a.set(u64::MAX));
                    TIMER_EXITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    break Exit::Host;
                }
                let next = GUEST.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
                    .and_then(|g| g.next(me, &mut self.backend, memory));
                match next {
                    None => break Exit::Shutdown,
                    Some(Next::Exit(exit)) => break exit,
                    Some(Next::Switch(to)) => switch_to(me, to),
                }
            };
            if let Some(k) = kick {
                k.left();
            }
            Ok((exit, me as u32))
        }
    }
}
