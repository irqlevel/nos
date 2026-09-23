//! A Linux guest and the loop that runs it: memory, one vCPU, an emulated
//! serial port, and the dispatch of every exit the guest comes back with.
//!
//! This is the safe half of running a real kernel -- it decodes
//! guest-controlled exits and answers them, and holds no reference into
//! guest memory. What it cannot yet answer (a local APIC, a timer) it stops
//! on and says so, which is how far the third of the four demos in
//! `plans/03-hypervisor.md` reaches: a `bzImage` printing its early console.

use hvarch::x86::svm::vmcb::Save;
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

/// Why a Linux guest stopped.
#[derive(Clone, Copy, Debug)]
pub enum Stop {
    /// It executed HLT: idle with interrupts it will never get, or a panic
    /// that came to rest -- either way, as far as it goes without a timer.
    Halted { rip: u64 },
    /// It touched a guest physical address with no memory behind it: either
    /// a bug, or an MMIO device this hypervisor does not emulate (a local
    /// APIC page, most likely).
    Mmio { gpa: u64, rip: u64 },
    /// It triple-faulted.
    Shutdown { rip: u64 },
    /// An exception the host intercepts (#DB, #AC, #MC) fired.
    Exception { vector: u8, rip: u64 },
    /// The CPU refused the VMCB, or the extension went off under it.
    Refused(Refusal),
    /// `vmrun` refused the VMCB despite the software check.
    Invalid,
    /// Its time ran out.
    Budget,
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
    pub exits: u64,
}

/// A Linux guest: its memory, its one vCPU, and its console.
pub struct LinuxGuest {
    vm: Vm,
    uart: Uart,
    pit: Pit,
    rtc: Rtc,
    pic: Pic,
    /// Bytes to hand the guest's console once it is idle, and how far in.
    input: alloc::vec::Vec<u8>,
    input_pos: usize,
    /// A tally of reads of the low ports, to find a guest spinning on one.
    port_hist: alloc::boxed::Box<[u32; 1024]>,
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
        Ok(Self {
            vm,
            uart: Uart::new(),
            pit: Pit::new(),
            rtc: Rtc::new(),
            pic: Pic::new(),
            input: alloc::vec::Vec::new(),
            input_pos: 0,
            port_hist,
        })
    }

    pub fn memory_mut(&mut self) -> &mut GuestMemory {
        self.vm.memory_mut()
    }

    /// Type `bytes` at the guest's console once it is up: fed one at a time,
    /// as the shell reads each, so a whole line reaches a prompt in order.
    pub fn set_input(&mut self, bytes: &[u8]) {
        self.input.clear();
        self.input.extend_from_slice(bytes);
        self.input_pos = 0;
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

    pub fn output(&self) -> &str {
        self.uart.output()
    }

    /// How much of the typed input the guest has taken, and how much there
    /// was: for a report on whether the shell read it.
    pub fn input_progress(&self) -> (usize, usize) {
        (self.input_pos, self.input.len())
    }

    pub fn uart_ier(&self) -> u8 {
        self.uart.ier()
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

    /// Run the guest on the CPU this is called on until it stops, for at
    /// most `budget_ns`, handing every console byte to `on_byte` as it is
    /// written. Returns why it stopped and what it did.
    pub fn run(
        &mut self,
        machine: &Machine,
        budget_ns: u64,
        mut on_byte: impl FnMut(u8),
    ) -> (Stop, Counts) {
        let mut counts = Counts::default();
        let start = time::boot_time_ns();

        let stop = loop {
            if time::boot_time_ns().saturating_sub(start) >= budget_ns {
                break Stop::Budget;
            }

            /* Feed the console's receive register whenever it is free, on
             * every iteration and not only at an idle HLT: while a shell's
             * line editor reads the answer to its cursor query it spins
             * polling the port rather than halting, so a byte offered only at
             * HLT would never arrive and the editor would time out. */
            self.feed_console();

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
            self.deliver_interrupt(&mut counts);

            let (exit, _cpu) = match self.vm.enter(machine) {
                Ok(entered) => entered,
                Err(refusal) => break Stop::Refused(refusal),
            };
            counts.exits += 1;
            let rip = self.vm.vcpu().save().rip;

            match exit {
                Exit::Host => counts.host += 1,
                Exit::Io(io) => {
                    self.io(&io, &mut counts, &mut on_byte);
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
                     * Re-enter -- deliver_interrupt at the top of the loop
                     * injects the timer IRQ when its period comes, and the
                     * guest wakes and runs on. Only a HLT with interrupts off
                     * is a guest that has stopped for good. */
                    const IF: u64 = 1 << 9;
                    if self.vm.vcpu().save().rflags & IF == 0 {
                        break Stop::Halted { rip };
                    }
                    counts.hlt += 1;
                    /* The HLT is the instruction after the idle loop's STI,
                     * so it sits in that STI's interrupt shadow -- but it is
                     * waiting for the very interrupt the shadow would block.
                     * Clear it, so the next timer tick can wake the guest. */
                    self.vm.vcpu_mut().clear_interrupt_shadow();
                }
                Exit::NestedFault { gpa, .. } => {
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

        (stop, counts)
    }

    /// Hand the guest's receive register its next byte when it is free:
    /// first any answer it is waiting for to a terminal query (the cursor
    /// position a shell's line editor asks for before it reads), then a byte
    /// of what was typed -- held back until the guest has reached a prompt,
    /// so the boot does not swallow it.
    fn feed_console(&mut self) {
        if !self.uart.rx_empty() {
            return;
        }
        if let Some(byte) = self.uart.take_reply() {
            self.uart.set_rx(byte);
        } else if self.uart.prompt_seen() && self.input_pos < self.input.len() {
            let byte = self.input[self.input_pos];
            self.input_pos += 1;
            self.uart.set_rx(byte);
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

    fn io(&mut self, io: &crate::svm::Io, counts: &mut Counts, on_byte: &mut impl FnMut(u8)) {
        if io.input && !Uart::owns(COM1, io.port) && (io.port as usize) < self.port_hist.len() {
            let slot = &mut self.port_hist[io.port as usize];
            *slot = slot.saturating_add(1);
        }
        let v = self.vm.vcpu_mut();
        if io.string {
            /* No string I/O device is emulated; step past it. INS/OUTS to
             * the console is not how a kernel drives a UART. */
            v.skip_io(io);
            return;
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
                    on_byte(out);
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
