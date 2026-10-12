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
        use std::sync::atomic::{AtomicU64, Ordering};

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
        /// out of here; kicks are counted.
        pub struct Kick {
            sent: AtomicU64,
        }

        impl Kick {
            pub fn new() -> Kick {
                Kick { sent: AtomicU64::new(0) }
            }
            pub fn sent(&self) -> u64 {
                self.sent.load(Ordering::Relaxed)
            }
            pub fn prepare(&self) {}
            pub fn cancel(&self) {}
            pub fn kick(&self) {
                self.sent.fetch_add(1, Ordering::Relaxed);
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
        Cr4 { write: bool },
        Dr { write: bool, dr: u8 },
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
    }

    impl Machine {
        pub fn new(vendor: Vendor) -> Machine {
            Machine { caps: Caps { vendor } }
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
        /// VT-x's, as the machine's vendor says: which gives no #DB back.
        vmx: bool,
        /// The CR4 the guest believes it has, as svm.rs keeps it: the
        /// register, `save.cr4`, has MCE besides.
        cr4: u64,
        /// The guest's DR0-DR3, as svm.rs keeps them apart from the CPU's.
        debug_addrs: [u64; 4],
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
                cr4: 0,
                debug_addrs: [0; 4],
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

        /// As svm.rs's: `value` the guest's CR4, and with MCE the register's
        /// -- held to what a write the run loop performed may be, by rules of
        /// the fuzzer's own: no bit the guest's CPUID does not offer, PAE kept
        /// in long mode, and the TLB flushed where a bit a CPU translates by
        /// changed.
        pub fn set_cr4(&mut self, value: u64, flush: bool) {
            invariant!(!self.vmx, "a CR4 write performed for a guest under VT-x, which never stops one at CR4");
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

        /// As svm.rs's: the guest's DR0-DR3.
        pub fn debug_addrs(&self) -> [u64; 4] {
            self.debug_addrs
        }

        /// As svm.rs's: `addrs` the guest's DR0-DR3 -- under AMD-V alone,
        /// the one backend that stops a guest at them.
        pub fn set_debug_addrs(&mut self, addrs: [u64; 4]) {
            invariant!(!self.vmx, "a debug register written for a guest under VT-x, which never stops one there");
            self.debug_addrs = addrs;
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
        }
        pub fn cr8(&self) -> u8 {
            self.cr8
        }
        pub fn set_cr8(&mut self, value: u8) {
            invariant!(value < 16, "CR8 set to {:#x}", value);
            self.cr8 = value;
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
            invariant!(!(self.vmx && vector == 1), "a #DB given back under VT-x, whose DR6 is not put right");
            self.queue(Event::Exception(vector));
        }
        pub fn reflects_debug(&self) -> bool {
            !self.vmx
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
        /// an event leaves the queue, but for INIT's reset.
        pub fn take_event(&mut self) {
            if let Some(e) = self.event.take() {
                self.taken.push(e);
                self.shadow = false;
                if e == Event::Nmi {
                    self.nmi_window = false;
                }
            }
        }
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

    thread_local! {
        static VCPU: Cell<usize> = const { Cell::new(0) };
    }

    fn baton() -> std::sync::MutexGuard<'static, Option<Baton>> {
        BATON.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// This thread runs CPU `i`: and, with the CPUs on threads, it is the
    /// thread a turn for `i` wakes.
    pub fn set_vcpu(i: usize) {
        VCPU.with(|v| v.set(i));
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
        pub fn enter(&mut self, memory: &GuestMemory, _machine: &Machine, _kick: Option<&Kick>)
            -> core::result::Result<(Exit, u32), Refusal>
        {
            let me = VCPU.with(|v| v.get());
            invariant!(crate::sync::held() == 0, "CPU {} enters its guest with {} of the kernel's locks held", me,
                       crate::sync::held());
            loop {
                crate::time::progressed();
                let next = GUEST.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
                    .and_then(|g| g.next(me, &mut self.backend, memory));
                match next {
                    None => return Ok((Exit::Shutdown, me as u32)),
                    Some(Next::Exit(exit)) => return Ok((exit, me as u32)),
                    Some(Next::Switch(to)) => switch_to(me, to),
                }
            }
        }
    }
}
