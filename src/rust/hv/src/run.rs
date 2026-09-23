//! A Linux guest and the loop that runs it: memory, one vCPU, an emulated
//! serial port, and the dispatch of every exit the guest comes back with.
//!
//! This is the safe half of running a real kernel -- it decodes
//! guest-controlled exits and answers them, and holds no reference into
//! guest memory. What it cannot answer (an MMIO device it does not emulate,
//! a triple fault) it stops on and says so.
//!
//! A guest that halts is a vCPU with nothing to do until an interrupt it can
//! take is pending, and the loop treats it as one: the HLT is stepped past,
//! as a CPU an interrupt wakes resumes after it, and the vCPU is not entered
//! again until the PIC has something for it. Meanwhile the task sleeps --
//! to the timer's next edge, the only thing here that becomes pending with
//! time -- and its CPU goes to whatever else can use it.

use hvarch::x86::svm::vmcb::{self, Save};
use hvarch::x86::svm::GuestRegs;
use hvarch::{Error, Result};
use kcore::time;

use crate::devices::{Pic, Pit, Rtc, Uart};
use crate::linux::{self, Header, Layout};
use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::svm::Exit;
use crate::vm::{Refusal, Vm};

/// COM1, the guest's console.
const COM1: u16 = 0x3F8;

/* The two ways a PC guest resets its machine by port I/O. The 8042's
 * command port takes 0xF0-0xFF as "pulse the output lines whose bits are
 * clear", and line 0 is the CPU's reset: Linux writes 0xFE. The chipset's
 * reset control register at 0xCF9 resets when bit 2 is written set -- Linux
 * writes it with SYS_RST first and RST_CPU second. (The third way is a
 * triple fault, `Stop::Shutdown`.) Nothing here emulates an 8042 or a
 * chipset: a read of either port floats to all ones, as on a PC without
 * one, and only the reset is recognised. */
const I8042_COMMAND: u16 = 0x64;
const I8042_PULSE: u8 = 0xF0;
const I8042_RESET_LINE: u8 = 1 << 0;
const RESET_CONTROL: u16 = 0xCF9;
const RESET_CONTROL_RST_CPU: u8 = 1 << 2;

/// What a `Stop::Reset`'s port is, for a person: which of the two ways it was.
pub fn reset_source(port: u16) -> &'static str {
    match port {
        I8042_COMMAND => "the 8042's reset line",
        RESET_CONTROL => "the chipset's reset control",
        _ => "an unknown port",
    }
}

/// RFLAGS.IF: the guest takes maskable interrupts.
const RFLAGS_IF: u64 = 1 << 9;

/// The longest a halted vCPU's task sleeps before it looks again, when no
/// timer edge is due sooner: a guest whose PIT is stopped, or in a one-shot
/// mode this does not raise IRQ0 in, still has its time budget checked. One
/// host tick -- `task::sleep` wakes at one anyway (`Kernel::Sleep` blocks
/// until the deadline, and is woken at its CPU's next scheduling point, the
/// tick at the latest).
const MAX_HALT_WAIT_NS: u64 = 10 * kcore::consts::NS_PER_MS;

/// Why a Linux guest stopped.
#[derive(Clone, Copy, Debug)]
pub enum Stop {
    /// It executed HLT with interrupts off: nothing but an NMI -- and none
    /// is emulated -- can wake it, so it has stopped for good (a panic that
    /// came to rest, a `poweroff` with nowhere to go).
    Halted { rip: u64 },
    /// It touched a guest physical address with no memory behind it: either
    /// a bug, or an MMIO device this hypervisor does not emulate (a local
    /// APIC page, most likely).
    Mmio { gpa: u64, rip: u64 },
    /// It triple-faulted.
    Shutdown { rip: u64 },
    /// It asked the machine to reset: `value` written to `port`, the 8042's
    /// command port or the chipset's reset control register. Its way to
    /// reboot -- a `reboot`, a panic with `panic=N`.
    Reset { port: u16, value: u8, rip: u64 },
    /// An exception the host intercepts (#DB, #AC, #MC) fired.
    Exception { vector: u8, rip: u64 },
    /// The CPU refused the VMCB, or the extension went off under it.
    Refused(Refusal),
    /// `vmrun` refused the VMCB despite the software check.
    Invalid,
    /// Its time ran out.
    Budget,
    /// Whoever runs it asked it to stop (`Host::stop_requested`).
    Requested,
    /// An exit with no handler here.
    Unexpected { exit: Exit, rip: u64 },
}

/// What a run counted, for a report.
#[derive(Clone, Copy, Default)]
pub struct Counts {
    pub port_in: u64,
    pub port_out: u64,
    pub cpuid: u64,
    pub msr_read: u64,
    pub msr_write: u64,
    pub msr_gp: u64,
    pub mmio: u64,
    pub host: u64,
    pub irq: u64,
    pub irq0: u64,
    pub irq4: u64,
    pub edges0: u64,
    pub blocked: u64,
    pub hlt: u64,
    /// Sleeps of the task while the vCPU was halted, and the time they took:
    /// the host CPU this guest gave back while it had nothing to do.
    pub sleeps: u64,
    pub slept_ns: u64,
    /// Instructions the guest was told (by CPUID) it does not have, run
    /// anyway and answered with #UD; WBINVDs stepped past.
    pub ud: u64,
    pub wbinvd: u64,
    pub exits: u64,
}

/// The most MSR accesses answered with #GP kept for a report.
const MSR_FAULTS_KEPT: usize = 8;
/// How many exits go by between reports of progress while the guest does
/// not halt: a busy VM's counters are still seen moving.
const PROGRESS_EVERY: u64 = 4096;

/// What runs a guest gives the loop: where the guest's console goes, what is
/// typed at it, and whether to stop. The loop calls it between entries, in
/// task context -- never with the guest running, never with interrupts off.
pub trait Host {
    /// A byte the guest wrote to its serial console.
    fn output(&mut self, byte: u8);
    /// The next byte typed at the guest's console, if there is one. Asked
    /// only once the guest is at a prompt (it has asked where its cursor is),
    /// and only when the serial port's receive register is free.
    fn input(&mut self) -> Option<u8>;
    /// Whether the guest is to be stopped: asked before every entry, and at
    /// least every host tick while the guest is halted.
    fn stop_requested(&mut self) -> bool;
    /// What the loop has counted so far: when the vCPU halts, every
    /// `PROGRESS_EVERY` exits, and once more at the end. For a VM that runs
    /// until it is stopped, how anyone else sees it doing.
    fn progress(&mut self, _counts: &Counts) {}
}

/// A Linux guest: its memory, its one vCPU, and its console.
pub struct LinuxGuest {
    vm: Vm,
    uart: Uart,
    pit: Pit,
    rtc: Rtc,
    pic: Pic,
    /// A tally of reads of the low ports, to find a guest spinning on one.
    port_hist: alloc::boxed::Box<[u32; 1024]>,
    /// The first MSR accesses the policy refused with #GP: (MSR, value
    /// written, whether a write). A guest's `rdmsr_safe` takes the fault in
    /// silence, so the report is the only place such a thing shows.
    msr_faults: alloc::vec::Vec<(u32, u64, bool)>,
}

impl LinuxGuest {
    /// A guest with `mem_bytes` of RAM and nothing loaded yet. Its vCPU
    /// stops at no exception of its own -- a Linux guest has an IDT and
    /// handles its own faults -- so `exceptions` is empty; the host still
    /// intercepts #DB, #AC and #MC whatever this says.
    pub fn new(machine: &Machine, mem_bytes: u64) -> Result<Self> {
        let mut vm = Vm::new(machine, 0)?;
        vm.memory_mut().add(0, mem_bytes)?;
        let port_hist = alloc::vec![0u32; 1024].into_boxed_slice().try_into()
            .map_err(|_| Error::NoMemory)?;
        /* Its whole capacity now, fallibly, so that recording a fault later
         * never allocates. */
        let mut msr_faults = alloc::vec::Vec::new();
        msr_faults.try_reserve_exact(MSR_FAULTS_KEPT).map_err(|_| Error::NoMemory)?;
        Ok(Self {
            vm,
            uart: Uart::new(),
            pit: Pit::new(),
            rtc: Rtc::new(),
            pic: Pic::new(),
            port_hist,
            msr_faults,
        })
    }

    pub fn memory_mut(&mut self) -> &mut GuestMemory {
        self.vm.memory_mut()
    }

    /// Write the guest's furniture -- the zero page, the command line, the
    /// memory map, the page tables and the GDT -- and put the vCPU at the
    /// kernel's entry. The kernel and initrd bytes must already be in memory
    /// at the addresses `layout` names; the caller streams those in.
    pub fn load(&mut self, header: &Header, first: &[u8], layout: Layout, cmdline: &[u8]) -> Result<()> {
        linux::build(self.vm.memory_mut(), header, first, &layout, cmdline)?;
        linux::set_entry(self.vm.vcpu_mut(), &layout);
        Ok(())
    }

    pub fn uart_ier(&self) -> u8 {
        self.uart.ier()
    }

    /// Where the guest read an absent device and was answered with all ones.
    pub fn absent_pages(&self) -> &[u64] {
        self.vm.memory().absent_pages()
    }

    /// The MSR accesses answered with #GP: (MSR, value, write).
    pub fn msr_faults(&self) -> &[(u32, u64, bool)] {
        &self.msr_faults
    }

    /// The interrupt state at the end, for a diagnostic: the master PIC's
    /// (IRR, ISR, IMR) and the PIT channel 0's (mode, reload, running).
    pub fn irq_debug(&self) -> ((u8, u8, u8), (u8, u16, bool)) {
        (self.pic.master_state(), self.pit.ch0_state())
    }

    /// The busiest few low ports the guest read, for diagnosing a spin:
    /// (port, count), most first, at most `n`.
    pub fn hot_ports(&self, n: usize) -> alloc::vec::Vec<(u16, u32)> {
        let mut v: alloc::vec::Vec<(u16, u32)> = self.port_hist.iter().enumerate()
            .filter(|(_, &c)| c > 0).map(|(p, &c)| (p as u16, c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v
    }

    /// Run the guest on the CPU this is called on until it stops -- for good,
    /// on `host`'s request, or at the end of `budget_ns` (`u64::MAX` for no
    /// end) -- its console going to and coming from `host`. Returns why it
    /// stopped and what it did.
    pub fn run(&mut self, machine: &Machine, budget_ns: u64, host: &mut dyn Host) -> (Stop, Counts) {
        let mut counts = Counts::default();
        let start = time::boot_time_ns();
        let deadline = start.saturating_add(budget_ns);
        /* The guest executed HLT with interrupts on, and has not been woken
         * since: it is not entered until an interrupt is pending for it. */
        let mut halted = false;

        let stop = loop {
            let now = time::boot_time_ns();
            if now >= deadline {
                break Stop::Budget;
            }
            if host.stop_requested() {
                break Stop::Requested;
            }

            /* Feed the console's receive register whenever it is free, on
             * every iteration and not only at an idle HLT: while a shell's
             * line editor reads the answer to its cursor query it spins
             * polling the port rather than halting, so a byte offered only at
             * HLT would never arrive and the editor would time out. */
            self.feed_console(host);

            /* The timer: a channel-0 period elapsed is an IRQ0 edge. Then,
             * if any interrupt is pending, inject it when the guest can take
             * one and ask to be told when it can when it cannot. */
            if self.pit.ch0_fire() {
                self.pic.raise(0);
                counts.edges0 += 1;
            }
            /* COM1's transmitter is always ready, so with its THR-empty
             * interrupt enabled it asserts IRQ4 -- which is how the serial
             * driver sends past the first byte, an interrupt at a time. */
            if self.uart.irq_active() {
                self.pic.raise(4);
            }

            if halted {
                if !self.wakes() {
                    /* Nothing for it yet. Sleep until the timer's next edge
                     * -- nothing else here becomes pending with time: the
                     * console's input is waiting already or waits on the
                     * guest -- and give the CPU to whatever else can use it,
                     * rather than enter a guest that would only halt again. */
                    let until = self.pit.next_ch0_edge_ns()
                        .unwrap_or(u64::MAX)
                        .min(now.saturating_add(MAX_HALT_WAIT_NS))
                        .min(deadline);
                    if until > now {
                        kcore::task::sleep(time::Duration::from_nanos(until - now));
                        counts.sleeps += 1;
                        counts.slept_ns += time::boot_time_ns().saturating_sub(now);
                    }
                    continue;
                }
                halted = false;
            }
            self.deliver_interrupt(&mut counts);

            let (exit, _cpu) = match self.vm.enter(machine) {
                Ok(entered) => entered,
                Err(refusal) => break Stop::Refused(refusal),
            };
            counts.exits += 1;
            if counts.exits % PROGRESS_EVERY == 0 {
                host.progress(&counts);
            }
            let rip = self.vm.vcpu().save().rip;

            match exit {
                Exit::Host => counts.host += 1,
                Exit::Io(io) => {
                    if let Some((port, value)) = self.io(&io, &mut counts, host) {
                        break Stop::Reset { port, value, rip };
                    }
                }
                Exit::Cpuid => {
                    counts.cpuid += 1;
                    let sub = self.vm.vcpu().regs().rcx as u32;
                    let leaf = self.vm.vcpu().save().rax as u32;
                    let answer = crate::policy::cpuid(leaf, sub);
                    let v = self.vm.vcpu_mut();
                    let (save, regs) = v.save_and_regs_mut();
                    crate::policy::apply_cpuid(save, regs, &answer);
                    v.skip_cpuid();
                }
                Exit::Msr { write } => {
                    self.msr(write, &mut counts);
                }
                Exit::Hlt => {
                    /* A booted kernel idles on HLT, waking on the timer: with
                     * interrupts on it is waiting for the next one, not dead.
                     * Only a HLT with interrupts off has stopped for good. */
                    if self.vm.vcpu().save().rflags & RFLAGS_IF == 0 {
                        break Stop::Halted { rip };
                    }
                    counts.hlt += 1;
                    /* Step past it: a CPU an interrupt wakes from HLT resumes
                     * at the instruction after it -- Linux's `sti; hlt; cli`
                     * returns to the `cli` and on to its idle loop's
                     * need_resched check. Entered again at the HLT, the
                     * interrupt's handler would return to the HLT, and a
                     * kernel that does not preempt on the way out of an
                     * interrupt would never leave its idle task. Stepping
                     * also takes the vCPU out of the STI's interrupt shadow,
                     * which covers the HLT and would hold the wake-up off. */
                    self.vm.vcpu_mut().skip_hlt();
                    halted = true;
                    host.progress(&counts);
                }
                Exit::NestedFault { gpa, error } => {
                    /* A read of the platform's MMIO window that nothing
                     * answers is a probe for a device that is not there: map
                     * all ones and let the instruction run again. Anything
                     * else -- a write, a fetch, a walk of the guest's own
                     * tables, an address outside the window -- stops it. */
                    use vmcb::npf;
                    let plain_read = error & (npf::PRESENT | npf::WRITE | npf::FETCH) == 0
                        && error & npf::FINAL != 0;
                    if plain_read && self.vm.memory_mut().map_absent(gpa).is_ok() {
                        continue;
                    }
                    counts.mmio += 1;
                    break Stop::Mmio { gpa, rip };
                }
                Exit::Shutdown => break Stop::Shutdown { rip },
                Exit::Exception { vector, .. } => break Stop::Exception { vector, rip },
                Exit::MachineCheck => break Stop::Exception { vector: 18, rip },
                Exit::Invalid => break Stop::Invalid,
                Exit::IrqWindow => {
                    /* The guest can take an interrupt now; the next entry
                     * injects it. Nothing to do here. */
                }
                Exit::Other(code) if matches!(code,
                    vmcb::exit::MONITOR | vmcb::exit::MWAIT | vmcb::exit::MWAIT_ARMED
                    | vmcb::exit::RDTSCP | vmcb::exit::RDPRU | vmcb::exit::XSETBV) =>
                {
                    /* Intercepted, and not offered by CPUID: the guest gets
                     * what a CPU without them gives it. */
                    counts.ud += 1;
                    self.vm.vcpu_mut().inject_ud();
                }
                Exit::Other(vmcb::exit::WBINVD) => {
                    counts.wbinvd += 1;
                    self.vm.vcpu_mut().skip_wbinvd();
                }
                Exit::Hypercall => {
                    /* No paravirtualisation is offered; a VMMCALL is a fault
                     * to the guest. Step past it so a stray one does not
                     * spin, and inject nothing -- the guest that meant it
                     * will notice its result did not change. */
                    self.vm.vcpu_mut().skip_vmmcall();
                }
                other => break Stop::Unexpected { exit: other, rip },
            }
        };

        host.progress(&counts);
        (stop, counts)
    }

    /// Whether a halted vCPU has something to be entered for: an interrupt
    /// the PIC would deliver now -- requested, unmasked, not behind one in
    /// service -- or an event already queued for injection. It halted with
    /// interrupts on and has not run since, so either is one it can take.
    fn wakes(&self) -> bool {
        self.pic.pending().is_some() || self.vm.vcpu().event_queued()
    }

    /// Hand the guest's receive register its next byte when it is free:
    /// first any answer it is waiting for to a terminal query (the cursor
    /// position a shell's line editor asks for before it reads), then a byte
    /// of what was typed -- held back until the guest has reached a prompt,
    /// so the boot does not swallow it.
    fn feed_console(&mut self, host: &mut dyn Host) {
        if !self.uart.rx_empty() {
            return;
        }
        if let Some(byte) = self.uart.take_reply() {
            self.uart.set_rx(byte);
        } else if self.uart.prompt_seen() {
            if let Some(byte) = host.input() {
                self.uart.set_rx(byte);
            }
        }
    }

    /// Give the guest the highest-priority interrupt the PIC has for it, if
    /// it can take one; otherwise ask the CPU to exit when it can.
    fn deliver_interrupt(&mut self, counts: &mut Counts) {
        match self.pic.pending() {
            Some((irq, vector)) => {
                if self.vm.vcpu().interruptible() {
                    self.vm.vcpu_mut().inject_extint(vector);
                    self.pic.acknowledge(irq);
                    self.vm.vcpu_mut().clear_irq_window();
                    counts.irq += 1;
                    if irq == 0 { counts.irq0 += 1; }
                    if irq == 4 { counts.irq4 += 1; }
                } else {
                    counts.blocked += 1;
                    self.vm.vcpu_mut().request_irq_window();
                }
            }
            None => self.vm.vcpu_mut().clear_irq_window(),
        }
    }

    /// Answer a port access and step past it -- or, for a write that resets
    /// the machine, leave the guest where it is and say which it was.
    fn io(&mut self, io: &crate::svm::Io, counts: &mut Counts, host: &mut dyn Host) -> Option<(u16, u8)> {
        if io.input && !Uart::owns(COM1, io.port) && (io.port as usize) < self.port_hist.len() {
            let slot = &mut self.port_hist[io.port as usize];
            *slot = slot.saturating_add(1);
        }
        let v = self.vm.vcpu_mut();
        if io.string {
            /* No string I/O device is emulated; step past it. INS/OUTS to
             * the console is not how a kernel drives a UART. */
            v.skip_io(io);
            return None;
        }
        if !io.input && io.size == 1 {
            let value = v.save().rax as u8;
            let reset = match io.port {
                I8042_COMMAND => value & I8042_PULSE == I8042_PULSE && value & I8042_RESET_LINE == 0,
                RESET_CONTROL => value & RESET_CONTROL_RST_CPU != 0,
                _ => false,
            };
            if reset {
                counts.port_out += 1;
                return Some((io.port, value));
            }
        }
        if Uart::owns(COM1, io.port) && io.size == 1 {
            let offset = io.port - COM1;
            if io.input {
                let byte = self.uart.read(offset);
                let s = v.save_mut();
                s.rax = (s.rax & !0xFF) | byte as u64;
                counts.port_in += 1;
            } else {
                let byte = v.save().rax as u8;
                if let Some(out) = self.uart.write(offset, byte) {
                    host.output(out);
                }
                counts.port_out += 1;
            }
        } else if (Pit::owns(io.port) || Rtc::owns(io.port) || Pic::owns(io.port)) && io.size == 1 {
            let byte = v.save().rax as u8;
            if io.input {
                let value = if Pit::owns(io.port) {
                    self.pit.read(io.port)
                } else if Rtc::owns(io.port) {
                    self.rtc.read(io.port)
                } else {
                    self.pic.read(io.port)
                };
                let s = self.vm.vcpu_mut().save_mut();
                s.rax = (s.rax & !0xFF) | value as u64;
                counts.port_in += 1;
            } else {
                if Pit::owns(io.port) {
                    self.pit.write(io.port, byte);
                } else if Rtc::owns(io.port) {
                    self.rtc.write(io.port, byte);
                } else {
                    self.pic.write(io.port, byte);
                }
                counts.port_out += 1;
            }
        } else if io.input {
            /* A port nothing here answers: the bus floats to all ones,
             * which is what a read of an absent device gives. */
            let mask = size_mask(io.size);
            let s = v.save_mut();
            s.rax |= mask;
            counts.port_in += 1;
        } else {
            counts.port_out += 1;
        }
        self.vm.vcpu_mut().skip_io(io);
        None
    }

    fn msr(&mut self, write: bool, counts: &mut Counts) {
        let v = self.vm.vcpu_mut();
        let msr = v.regs().rcx as u32;
        if write {
            counts.msr_write += 1;
            let value = ((v.regs().rdx as u32 as u64) << 32) | (v.save().rax as u32 as u64);
            if crate::policy::wrmsr(v.save_mut(), msr, value) {
                v.skip_msr();
            } else {
                counts.msr_gp += 1;
                v.inject_gp();
                if self.msr_faults.len() < MSR_FAULTS_KEPT {
                    self.msr_faults.push((msr, value, true));
                }
            }
        } else {
            counts.msr_read += 1;
            match crate::policy::rdmsr(v.save(), msr) {
                Some(value) => {
                    let (save, regs) = v.save_and_regs_mut();
                    set_msr_read(save, regs, value);
                    v.skip_msr();
                }
                None => {
                    counts.msr_gp += 1;
                    v.inject_gp();
                    if self.msr_faults.len() < MSR_FAULTS_KEPT {
                        self.msr_faults.push((msr, 0, false));
                    }
                }
            }
        }
    }

    /// The guest's state after it stopped, for a report.
    pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        self.vm.vcpu().dump(out)
    }
}

fn size_mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

/// `rdmsr` puts the low 32 bits in EAX and the high 32 in EDX, each
/// zero-extended into its 64-bit register.
fn set_msr_read(save: &mut Save, regs: &mut GuestRegs, value: u64) {
    save.rax = value & 0xFFFF_FFFF;
    regs.rdx = value >> 32;
}

/// A guard so the module need not repeat the check: a `LinuxGuest` cannot be
/// made where no guest can run.
pub fn ensure_runnable(machine: &Machine) -> Result<()> {
    machine.ext().map(|_| ()).map_err(|_| Error::NotImplemented)
}
