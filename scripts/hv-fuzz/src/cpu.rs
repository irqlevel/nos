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
        /// A host CPU's CPUID leaf, as policy.rs asks for one: made up from
        /// the leaf, the same every time -- what the policy passes through
        /// of it is the policy's business.
        #[derive(Clone, Copy, Debug)]
        pub struct CpuidResult {
            pub eax: u32,
            pub ebx: u32,
            pub ecx: u32,
            pub edx: u32,
        }

        pub fn cpuid_count(leaf: u32, sub: u32) -> Option<CpuidResult> {
            let base = if leaf >= 0x8000_0000 { 0x8000_0000 } else { 0 };
            if leaf - base > 0x28 {
                return None;
            }
            let mut h = (u64::from(leaf) << 32 | u64::from(sub)) ^ 0x9E37_79B9_7F4A_7C15;
            let mut word = || {
                h = (h ^ (h >> 31)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                h as u32
            };
            Some(CpuidResult { eax: word(), ebx: word(), ecx: word(), edx: word() })
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
        Pause,
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
    use std::sync::{Condvar, Mutex};

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
            s.cr4 = 0x20 | 0x40;
            s.rip = l.entry;
            s.rsp = l.stack;
            s.rflags = 2;
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
            self.save.efer = 1 << 12;
            self.save.rflags = 2;
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
     * kernel runs each on a task -- one at a time, the one holding the baton:
     * so a run is the same every time, and the order the CPUs run in is the
     * script's to choose, where a CPU would leave the host -- entering its
     * guest (`Next::Switch`), or waiting to be woken (`sleep`). */
    struct Baton {
        holder: usize,
        alive: Vec<bool>,
        /// A thread panicked: the rest leave their runs, for it to be reported.
        abort: bool,
    }

    static BATON: Mutex<Option<Baton>> = Mutex::new(None);
    static TURN: Condvar = Condvar::new();

    thread_local! {
        static VCPU: Cell<usize> = const { Cell::new(0) };
    }

    /// This thread runs CPU `i`.
    pub fn set_vcpu(i: usize) {
        VCPU.with(|v| v.set(i));
    }

    /// `n` CPUs on threads of their own from here, the first to run first.
    pub fn begin_threads(n: usize) {
        *BATON.lock().unwrap_or_else(|e| e.into_inner()) = Some(Baton { holder: 0, alive: vec![true; n], abort: false });
    }

    pub fn end_threads() {
        *BATON.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Wait for CPU `me`'s turn.
    pub fn wait_turn(me: usize) {
        let mut b = BATON.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match b.as_ref() {
                None => return,
                Some(b) if b.abort => panic!("aborted: another CPU's thread panicked"),
                Some(b) if b.holder == me => return,
                Some(_) => {}
            }
            b = TURN.wait(b).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Hand the turn to CPU `to`, if it still runs.
    fn pass(to: usize) {
        let mut b = BATON.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(b) = b.as_mut() {
            if b.alive.get(to).copied().unwrap_or(false) {
                b.holder = to;
            }
        }
        TURN.notify_all();
    }

    /// The CPU after `me` that still runs, round the ring.
    fn after(me: usize) -> Option<usize> {
        let b = BATON.lock().unwrap_or_else(|e| e.into_inner());
        let b = b.as_ref()?;
        let n = b.alive.len();
        (1..=n).map(|k| (me + k) % n).find(|&i| b.alive[i])
    }

    /// CPU `me` waits to be woken: the next that runs has its turn, and this
    /// one its own again when the ring comes round.
    pub fn sleep() {
        let me = VCPU.with(|v| v.get());
        match after(me) {
            Some(next) if next != me => {
                pass(next);
                wait_turn(me);
            }
            _ => {}
        }
    }

    /// CPU `me`'s thread is done: the next has its turn.
    pub fn finished(me: usize) {
        let next = {
            let mut b = BATON.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(b) = b.as_mut() {
                b.alive[me] = false;
            }
            drop(b);
            after(me)
        };
        if let Some(next) = next {
            pass(next);
        }
        TURN.notify_all();
    }

    /// A thread panicked: every other leaves its run.
    pub fn abort() {
        if let Some(b) = BATON.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            b.abort = true;
        }
        TURN.notify_all();
    }

    pub struct Cpu {
        backend: Backend,
    }

    impl Cpu {
        pub fn new(_machine: &Machine, _memory: &GuestMemory, _exceptions: u32) -> crate::Result<Cpu> {
            Ok(Cpu { backend: Backend::new() })
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
                    Some(Next::Switch(to)) => {
                        if to != me {
                            pass(to);
                            wait_turn(me);
                        }
                    }
                }
            }
        }
    }
}
