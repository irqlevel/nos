//! A Linux guest and the loops that run it: memory, one or more vCPUs, the
//! devices a PC has, and the dispatch of every exit a guest comes back with.
//!
//! This is the safe half of running a real kernel -- it decodes
//! guest-controlled exits and answers them, and holds no reference into
//! guest memory. What it cannot answer (an MMIO device it does not emulate,
//! a triple fault) it stops on and says so.
//!
//! A guest of several CPUs is one [`LinuxGuest`] -- its memory, its devices
//! -- shared by as many tasks as it has CPUs, each running one [`GuestCpu`]
//! on a host CPU of its own ([`LinuxGuest::run`]). What they share is
//! guarded by what it is: the memory is copied into and out of by every CPU
//! at once, as the guest's own CPUs write it (`GuestMemory`); the devices
//! are one platform under one lock, taken for each port access and each
//! round of the boot CPU's device work; and what one CPU sends another --
//! an IPI, a start-up -- goes into the target's mailbox, lock-free, with a
//! ring of its doorbell (`crate::smp`). Each CPU's own state -- its registers,
//! its local APIC (`crate::lapic`) -- is its task's alone.
//!
//! The 8259 pair is wired through the first CPU's LINT0, in virtual-wire
//! mode, so its interrupts -- the PIT's tick, the serial port's, the disks'
//! and NICs' -- are all the first CPU's. That CPU's task also does the
//! devices' work between its guest's turns: the timer's edges, what the
//! host has for the guest's console, NICs and disks. The others take their
//! interrupts from their local APICs: their timers, and each other's IPIs.
//!
//! A guest can have an IO-APIC as well (`devices::ioapic`), with each of
//! those lines on one of its pins as a PC wires them -- the timer on pin 2,
//! the others on the pins of their IRQs -- and then the tables say so and
//! its kernel routes them to any CPU through it, the 8259 masked. Without
//! one, the tables say there is none to use. A pin's message is delivered
//! as a device's MSI is -- into the local APIC of the CPU sending it, or
//! through another's mailbox -- and a level-triggered one's end comes back
//! from whichever CPU took it, as its APIC's EOI (`LinuxGuest::level_eoi`).
//!
//! The platform has ACPI's fixed hardware too (`crate::devices::pm`), and
//! the tables that describe it all (`crate::acpi`): a kernel with ACPI turns
//! the machine off through it -- `Stop::PowerOff` -- and hears its power
//! button, which whoever runs the guest presses (`Host::power_button`) to
//! ask it to shut down; the SCI it raises is the 8259's IRQ 9.
//!
//! A CPU that halts is a vCPU with nothing to do until an interrupt it can
//! take is pending, and the loop treats it as one: the HLT is stepped past,
//! as a CPU an interrupt wakes resumes after it, and the vCPU is not entered
//! again until something is pending for it. Meanwhile the task sleeps -- to
//! the next timer edge, or until another CPU or the host rings it -- and its
//! CPU goes to whatever else can use it. A CPU halted with interrupts off,
//! or waiting for a start-up IPI, sleeps until another CPU sends it one; and
//! when every CPU of a guest is asleep that way, nothing can wake any of
//! them, and the guest has stopped.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use hvarch::x86::svm::vmcb::{self, Save};
use hvarch::x86::svm::GuestRegs;
use hvarch::x86::vmx::PostedInterrupts;
use hvarch::{Error, Result};
use kcore::sync::Mutex;
use kcore::time;
use kcore::trace::{self, HvHalt, HvWake, InjectKind, RingHow, TimerHow, WakeHow};

use crate::acpi;
use crate::smbios;
use crate::cr;
use crate::dr;
use crate::devices::blk::{self, Blk};
use crate::devices::host::{self, HostFunction};
use crate::devices::ioapic::{self, Pins};
use crate::devices::net::{self, Net};
use crate::devices::pci::{self, Function, PciBus};
use crate::devices::port::{self, RootPort};
use crate::devices::pm;
use crate::devices::pvpanic::{self, PvPanic};
use crate::devices::virtio::Raise;
use crate::devices::{IoApic, Pic, Pit, Pm, Rtc, Uart};
use crate::lapic::{self, Addressing, Delivery, Ipi, Lapic, Wrote};
use crate::linux::{self, Header, Layout};
use crate::pvh;
use crate::tp;
use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::mmio;
use crate::place::Shape;
use crate::smp::{Doorbells, Mailbox, MAX_VCPUS};
use crate::svm::{Exit, Instr};
use crate::vm::{Cpu, Refusal};

pub use crate::smp::Doorbell;

/// COM1, the guest's console, and its IRQ.
const COM1: u16 = 0x3F8;
const COM1_IRQ: u8 = 4;
/// The exceptions of a guest's own the host intercepts -- so that the CPU
/// cannot be made to deliver one inside another for good -- and gives back.
const VECTOR_DB: u8 = 1;
const VECTOR_AC: u8 = 17;
/// An NMI's vector, as `hv_inject` says one.
const VECTOR_NMI: u8 = 2;

/// Where the virtio devices' I/O BARs are put, as a BIOS would put them --
/// the disks' from 0xC000, the NICs' past every disk's -- and the IRQs they
/// share: 11 and 10, free on a PC, on the slave PIC.
const DISK_IO_BASE: u16 = 0xC000;
const NIC_IO_BASE: u16 = DISK_IO_BASE + (MAX_DISKS as u16) * (blk::BAR_SIZE as u16);
const DISK_IRQ: u8 = 11;
const NIC_IRQ: u8 = 10;
/// The line a host's function's INTx pin is said to be wired to, in the MP
/// table and the DSDT: never raised -- the function's interrupt is its MSI,
/// its INTx off at the device -- but there, for a driver that wants a legacy
/// IRQ to exist (NVIDIA's). Free on a PC, and on the master PIC.
const HOST_IRQ: u8 = 5;
/// What the 64-bit window the DSDT gives a host's functions' BARs is
/// rounded up to, past the last of them (`write_firmware`).
const BARS64_ROUNDING: u64 = 1 << 30;
/// Where a device's MSI-X table page is, for a guest of more than one CPU:
/// this plus a page for its PCI slot -- in the platform's MMIO window, clear
/// of the local APIC's page, the IO-APIC's address the MP table gives, and
/// the page read of an absent device finds.
const MSIX_PAGES: u32 = 0xFE00_0000;
/// The most RAM a guest has: the most this kernel can take of a machine,
/// one PDPT of 1 GiB blocks (`BuiltinPageTable::map_high_ram`) -- no guest is
/// bigger than its host. Laid out as `linux::Ram` says: up to the devices'
/// pages from 0, the rest from 4 GiB, and all of it below what the guest is
/// told a physical address is (`policy::phys_bits`).
pub const MAX_MEM_BYTES: u64 = 512 << 30;
/* The RAM below 4 GiB ends where the devices' pages begin: past them it
 * would cover them, and the guest would read and write its devices'
 * registers as memory, none of them hearing of it -- an APIC in xAPIC mode
 * that is not there, with nothing to say so. */
const _: () = assert!(linux::LOW_RAM_END <= MSIX_PAGES as u64);
/// The PCI Express configuration space of a guest with the BARs' hole
/// (`linux::ECAM_BASE`), as its MCFG says it: its base and its last bus.
const ECAM: (u32, u8) = (linux::ECAM_BASE as u32, (linux::ECAM_BUSES - 1) as u8);
const _: () = assert!(linux::ECAM_BASE + linux::ECAM_BYTES <= MSIX_PAGES as u64);
/// The MSI-X entries each kind of device offers: one for configuration
/// changes and one a queue -- what Linux's virtio-pci asks for, a vector a
/// queue.
const DISK_VECTORS: u16 = 2;
/// The most disks, and NICs, a guest has.
pub const MAX_DISKS: usize = 4;
pub const MAX_NICS: usize = 2;
/// The most CPUs a guest has.
pub const MAX_CPUS: usize = MAX_VCPUS;

/// A device on the guest's PCI bus, by its slot.
enum PciDev {
    Disk(Blk),
    Nic(Net),
}

/* The two ways a PC guest resets its machine by port I/O. The 8042's
 * command port takes 0xF0-0xFF as "pulse the output lines whose bits are
 * clear", and line 0 is the CPU's reset: Linux writes 0xFE. The chipset's
 * reset control register at 0xCF9 resets when bit 2 is written set -- Linux
 * writes it with SYS_RST first and RST_CPU second, and it is the FADT's
 * reset register, which a kernel with ACPI tries first. (The third way is a
 * triple fault, `Stop::Shutdown`.) Nothing here emulates an 8042 or a
 * chipset: a read of either port floats to all ones, as on a PC without
 * one, and only the reset is recognised. */
const I8042_COMMAND: u16 = 0x64;
const I8042_PULSE: u8 = 0xF0;
const I8042_RESET_LINE: u8 = 1 << 0;
pub(crate) const RESET_CONTROL: u16 = 0xCF9;
const RESET_CONTROL_SYS_RST: u8 = 1 << 1;
const RESET_CONTROL_RST_CPU: u8 = 1 << 2;
/// What the FADT tells an OS to write to the reset control register: a full
/// reset, the system's and the CPU's.
pub(crate) const RESET_VALUE: u8 = RESET_CONTROL_SYS_RST | RESET_CONTROL_RST_CPU;

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
/// mode this does not raise IRQ0 in, still has its time budget checked. The
/// wait is the doorbell's `Event::wait_for`, whose deadline arms its CPU's
/// own timer -- woken at it, not at a tick -- so this is a bound on a look,
/// not the wait's resolution.
const MAX_HALT_WAIT_NS: u64 = 10 * kcore::consts::NS_PER_MS;
/// Halt polling, as KVM has it: a halted vCPU's task spins this long for a
/// ring before it sleeps (`Doorbell::poll`) -- grown, doubling from the
/// first, after a halt that was short and longer than the spin, and taken
/// back to none after one longer than the most: so a guest whose CPUs wake
/// each other within microseconds -- a pipe between two processes, a
/// database's client and server -- is caught spinning, and one that idles
/// until its next tick spins not at all. On the EX44 KVM's polls caught 36
/// million of a Linux guest's 40 million halts in a benchmark's run, and its
/// pipe's round trip took 6 us where nos's sleeping vCPUs took 14.
const POLL_FIRST_NS: u64 = 10 * kcore::consts::NS_PER_US;
const POLL_MAX_NS: u64 = 200 * kcore::consts::NS_PER_US;

/// The longest a HLT is waited out in the world switch's own loop
/// (`FastMsr::hlt`), with the host's interrupts held off meanwhile: past it
/// the halt is the loop's, its spin with interrupts on and its sleep.
const HLT_SPIN_NS: u64 = 50 * kcore::consts::NS_PER_US;

/// How far short of its own timer's edge a halted vCPU's sleep ends, the
/// rest spun: what a wake costs the host's sleeper -- its timer interrupt, a
/// schedule, a switch -- which made the EX44's guests' timer interrupts 9-10
/// us late on average. Spun for at most this a wake, on a CPU that is the
/// guest's own.
const TIMER_ADVANCE_NS: u64 = 10 * kcore::consts::NS_PER_US;

/// How late past its edge a timer's interrupt is counted as waited for by
/// its CPU's task -- an edge lost, rather than a wake's cost -- and by what
/// the task did last (`Counts::late_slept`), as is a sleep that ended this
/// long past its time: where the 10 ms that the EX44's guests' timers came
/// at most past their edges were spent.
const LATE_NS: u64 = kcore::consts::NS_PER_MS;

/// The latest timer interrupt of a run, past its edge, and how it was
/// waited for: `how` in `Counts::late_*`'s order, 1 to 8 -- a sleep that
/// woke late, one that woke on time, one without the edge, a spin or a
/// sleep for good, a round late after an exit, a host's interrupt, an exit
/// of the guest's own, anything else. After the guest ran, when it was
/// entered, what the host's timer was asked for as it was, and when it left
/// -- host nanoseconds on the boot clock, as `due` is.
#[derive(Clone, Copy, Default)]
pub struct Worst {
    pub late: u64,
    pub how: u8,
    pub due: u64,
    pub entered: u64,
    pub asked: u64,
    pub left: u64,
}

/// The sooner of two times, either of which may be none.
fn sooner(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A guest CPU's exits the world switch's own loop answers
/// (`hvarch::x86::fast`), the guest entered again at once past them: its
/// writes of the x2APIC's ICR that send a fixed IPI to other CPUs alone --
/// posted or mailed, and their CPUs notified or rung, as `send` does -- and
/// of its TSC deadline, the host's timer armed for the edge as the run
/// loop's round arms it. KVM answers the same two so
/// (`handle_fastpath_set_msr_irqoff`). Anything else -- an IPI to itself
/// too, or of another kind, a guest with no APIC -- is refused, and goes the
/// way it always went. What comes meanwhile for the CPU kicks it, and the
/// kick refuses the entry after the write (`Kick::entering`): the run
/// loop's round then looks.
///
/// With interrupts off and the guest's registers the CPU's: what it calls
/// are atomics, IPIs and doorbells, which an interrupt handler may ring.
struct FastMsr<'a> {
    guest: &'a LinuxGuest,
    me: u32,
    pid: u32,
    lapic: &'a mut Lapic,
    counts: &'a mut Counts,
    kick: &'a hvarch::x86::svm::Kick,
    /// The CPU's deadlines but its APIC timer's (`GuestCpu::other_due`)
    other_due: Option<u64>,
    /// The deadline its host timer is armed for (`GuestCpu::asked`)
    asked: &'a mut Option<u64>,
    /// How long its halts have lately been ending within
    /// (`GuestCpu::poll_ns`): the most `hlt` waits, to `HLT_SPIN_NS`.
    poll_ns: u64,
}

impl hvarch::x86::fast::FastExit for FastMsr<'_> {
    fn wrmsr(&mut self, msr: u32, value: u64, rip: u64, ran: hvarch::x86::svm::Run) -> bool {
        if !self.guest.has_apic() {
            return false;
        }
        let answered = if msr == lapic::MSR_X2APIC_ICR {
            self.ipi(value)
        } else if msr == lapic::MSR_TSC_DEADLINE && crate::policy::tsc_deadline() {
            self.tsc_deadline(value)
        } else {
            false
        };
        if answered {
            self.counts.msr_write += 1;
            self.counts.answered += 1;
            if trace::on::<trace::HvExit>() {
                let regs = tp::ExitRegs { rip, rax: value & 0xFFFF_FFFF, rcx: u64::from(msr) };
                tp::exit(self.me, self.pid, &Exit::Msr { write: true }, regs, ran, None);
            }
        }
        answered
    }

    /// A HLT with interrupts on, nothing waiting in the virtual APIC and
    /// posted interrupts in use (`hvarch::x86::fast`): what is posted for
    /// this CPU spun for, interrupts off, for as long as its halts have
    /// lately been ending within (`poll_ns`, to `HLT_SPIN_NS`), and true
    /// once something is -- its notification pending here, or on its way,
    /// for the CPU to take as it enters. False, and the halt the loop's, for
    /// a CPU without posted interrupts, halts that have lately been long, a
    /// kick (a ring: mail, a device, a stop), the edge the host's timer is
    /// armed for, or the spin's time up.
    fn hlt(&mut self, rip: u64, ran: hvarch::x86::svm::Run) -> bool {
        let Some(shared) = self.guest.cpus.get(self.me as usize) else { return false };
        let (true, Some(posted)) = (shared.posted_on.load(Ordering::Acquire), shared.posted.as_ref()) else {
            return false;
        };
        let limit = self.poll_ns.min(HLT_SPIN_NS);
        if limit == 0 {
            return false;
        }
        let edge = self.kick.edge();
        let from = time::boot_time_ns();
        while !posted.outstanding() {
            if self.kick.refuses() {
                return false;
            }
            let now = time::boot_time_ns();
            if now.saturating_sub(from) >= limit || edge.is_some_and(|edge| now >= edge) {
                return false;
            }
            core::hint::spin_loop();
        }
        self.counts.hlt += 1;
        self.counts.hlt_answered += 1;
        if trace::on::<trace::HvExit>() {
            let regs = tp::ExitRegs { rip, rax: 0, rcx: 0 };
            tp::exit(self.me, self.pid, &Exit::Hlt, regs, ran, None);
        }
        true
    }
}

impl FastMsr<'_> {
    /// A fixed IPI, to other CPUs alone, sent as `send` sends one.
    fn ipi(&mut self, value: u64) -> bool {
        let me = self.me;
        let Some(ipi) = self.lapic.fixed_ipi(value, me) else { return false };
        self.counts.ipi_sent += 1;
        for (i, target) in self.guest.cpus.iter().enumerate() {
            let id = i as u32;
            if id == me || !ipi.reaches(id, target.addressing(), me) {
                continue;
            }
            self.guest.post_edge(i, target, ipi.vector);
            /* Lowest priority is one CPU's, of those it names: the first. */
            if ipi.delivery == Delivery::LowestPriority {
                break;
            }
        }
        true
    }

    /// The TSC deadline written, and the host's timer armed for the
    /// soonest of its edge and the CPU's other deadlines, as the round's
    /// look before an entry made without it arms it. An edge come already
    /// -- the timer's, or a devices' deadline that has passed, which was
    /// once dropped here as no deadline at all -- refuses the entry after
    /// (`Kick::entering`), for the round to take.
    fn tsc_deadline(&mut self, value: u64) -> bool {
        self.lapic.set_tsc_deadline(value, tsc_ns(value, self.guest.tsc_offset.load(Ordering::Relaxed)));
        *self.asked = sooner(self.lapic.next_timer_ns(), self.other_due);
        self.kick.arm_at(*self.asked);
        true
    }
}

/// What a CPU's task did last, before the round that looks at its timers:
/// where an edge found late was waited for (`LATE_NS`).
#[derive(Clone, Copy)]
enum Step {
    /// Its guest ran -- or the entry was turned back -- and was left at
    /// `at`: by an interrupt of the host's, or by an exit of its own. It was
    /// entered at `entered`, the host's timer asked for `asked` as it was
    /// (`Kick::arm_at`; 0 for nothing).
    Ran { at: u64, host: bool, entered: u64, asked: u64 },
    /// The task slept until `wake_at` -- its timer's edge then `edge` --
    /// and woke at `woke`, rung or not.
    Slept { wake_at: u64, edge: Option<u64>, woke: u64, rung: bool },
    /// The task spun through a halt until rung, or slept until another CPU
    /// woke it -- halted with interrupts off, or waiting for a start-up IPI.
    Spun,
    /// Neither: the round came again for something else.
    Other,
}

/// The longest a CPU asleep for good -- halted with interrupts off, or
/// waiting for a start-up IPI -- sleeps before it looks again. Only another
/// CPU can wake one, by ringing it, and a stop rings every CPU; this is a
/// net under that, not how it is woken.
const ASLEEP_WAIT_NS: u64 = kcore::consts::NS_PER_SEC;

/// Room for the register dump of the CPU that stopped a guest.
const DUMP_BYTES: usize = 1024;
/// A page of the guest's memory: what its dirty log marks (`crate::dirty`).
const GUEST_PAGE: u64 = 4096;
/// The most write faults in a row one CPU takes at one page with no round of
/// the dirty log between (`LinuxGuest::dirty_write`): one is the log's, a
/// few more a TLB's spurious ones, and past this many it is no fault the
/// log made -- stopped and said, not run again for good.
const DIRTY_REFAULTS: u32 = 64;

/// Why a Linux guest stopped.
#[derive(Clone, Copy, Debug)]
pub enum Stop {
    /// Every CPU executed HLT with interrupts off, or waits for a start-up
    /// IPI: nothing but an NMI or an INIT from another of its CPUs can wake
    /// one, and none is awake to send it -- it has stopped for good (a panic
    /// that came to rest, a `poweroff` with nowhere to go). `rip` is where
    /// the last of them halted.
    Halted { rip: u64 },
    /// It touched a guest physical address with no memory behind it: either
    /// a bug, or an MMIO device this hypervisor does not emulate.
    Mmio { gpa: u64, rip: u64 },
    /// The CPU, delivering an interrupt or an exception, touched a device's
    /// page -- the guest's stack or IDT put on the APIC's, the IO-APIC's, a
    /// host's function's or ECAM's -- which no instruction did, and so none
    /// to emulate.
    MmioDelivering { gpa: u64, rip: u64 },
    /// An INS or an OUTS, to `port`: string port I/O, which no device here
    /// emulates.
    StringIo { port: u16, input: bool, rip: u64 },
    /// It triple-faulted.
    Shutdown { rip: u64 },
    /// It asked the machine to reset: `value` written to `port`, the 8042's
    /// command port or the chipset's reset control register. Its way to
    /// reboot -- a `reboot`, a panic with `panic=N`.
    Reset { port: u16, value: u8, rip: u64 },
    /// It sent INIT to its boot CPU, which resets the machine on a PC: the
    /// boot CPU starts again at the reset vector, in firmware this machine
    /// does not have.
    Init { rip: u64 },
    /// It turned itself off: S5's sleep type written to its PM1 control
    /// register with the sleep enable bit -- ACPI's soft off, a `poweroff`.
    PowerOff { rip: u64 },
    /// Its kernel panicked, and said so to its panic device
    /// (`devices::pvpanic`) at `rip`: stopped there rather than left to spin
    /// in `panic` on a host CPU.
    Panicked { rip: u64 },
    /// An MMIO access of a device's that could not be performed for it:
    /// the instruction at `rip` that reached `gpa`, as far as it was read
    /// (`bytes`), and why (`error`).
    MmioInsn { gpa: u64, rip: u64, error: mmio::Error, bytes: mmio::Bytes },
    /// A `mov` to or from CR4 that could not be performed for it (`cr`):
    /// the instruction at `rip`, as far as it was read, and why.
    CrInsn { rip: u64, error: cr::Error, bytes: mmio::Bytes },
    /// A `mov` to or from DR0-DR3 that could not be performed for it
    /// (`dr`): the instruction at `rip`, as far as it was read, and why.
    DrInsn { rip: u64, error: dr::Error, bytes: mmio::Bytes },
    /// An exception the host intercepts fired that no guest is given back:
    /// a machine check, or a built-in guest's own exception -- a Linux
    /// guest's #DB and #AC go back to it.
    Exception { vector: u8, rip: u64 },
    /// The CPU refused the VMCB, or the extension went off under it.
    Refused(Refusal),
    /// `vmrun` refused the VMCB despite the software check.
    Invalid,
    /// Its time ran out.
    Budget,
    /// Whoever runs it asked it to stop (`Host::stop_requested`).
    Requested,
    /// Its state was taken -- to another host, or into a file -- and it goes
    /// on there, or from there: here it ends, its memory given back.
    Migrated,
    /// An exit with no handler here.
    Unexpected { exit: Exit, rip: u64 },
}

impl Stop {
    /// Whether the guest reset itself, one way or another -- or panicked,
    /// which with `panic=` it would have rebooted after: what a guest that
    /// is to be booted again when it reboots is booted again after.
    pub fn is_reset(&self) -> bool {
        matches!(self, Stop::Reset { .. } | Stop::Shutdown { .. } | Stop::Init { .. } | Stop::Panicked { .. })
    }
}

/// How a guest stopped, and a CPU's state then, for a report: the CPU's that
/// stopped it -- or, when that one had none to show, waiting for a start-up
/// IPI, or the guest was stopped from outside, the first CPU's out of its
/// loop that had.
pub struct Stopped {
    pub stop: Stop,
    pub cpu: u32,
    pub dump: String,
}

/// What a run counted, for a report: one vCPU's, or -- summed with
/// [`Counts::add`] -- a guest's.
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
    /// Entries refused because a frame, a disk's answer or an IPI came for
    /// the guest on its way in (`Kick`): handed over first -- and those of
    /// them turned back for an edge of its timers come by the entry
    /// (`Kick::arm_at`), taken a round later.
    pub kicked: u64,
    pub edges_passed: u64,
    /// Interrupts injected from the 8259.
    pub irq: u64,
    pub irq0: u64,
    pub irq4: u64,
    pub edges0: u64,
    /// Interrupts injected from the local APIC: its timer's, IPIs, its own.
    pub apic: u64,
    /// The local APIC timer's interrupts requested, and how late past their
    /// edges: in all, and the latest.
    pub timer: u64,
    pub timer_late_ns: u64,
    pub timer_late_max: u64,
    /// The latest of them, and what came before it.
    pub worst: Worst,
    /// Entries not made because an edge of its timers came between the look
    /// at them and the arming of the host's timer -- which arms none for an
    /// edge gone by -- the loop round again to take it.
    pub timer_raced: u64,
    /// Sleeps to its own timer's edge woken short of it and spun the rest
    /// (`TIMER_ADVANCE_NS`).
    pub advanced: u64,
    /// The timer's interrupts requested more than `LATE_NS` past their
    /// edges, by what the task did before the round that found them (`Step`):
    /// slept to a time with that edge in it, and woke more than `LATE_NS` past
    /// it -- rung, or not; woke on time, the edge in the sleep; slept without
    /// that edge -- none armed, or another; spun or slept for good; ran its
    /// guest, and came round more than `LATE_NS` after the exit -- off its
    /// CPU, or busy; ran its guest until an interrupt of the host's took it
    /// out; until an exit of its own; anything else.
    pub late_woke_late: u64,
    pub late_woke_late_rung: u64,
    pub late_woke_on_time: u64,
    pub late_not_slept_for: u64,
    pub late_spun: u64,
    pub late_off_cpu: u64,
    pub late_host: u64,
    pub late_exit: u64,
    pub late_other: u64,
    /// Sleeps to a time that ended more than `LATE_NS` past it -- by their
    /// time, or by a ring that came after it -- those of them rung, and the
    /// most any ended past its time.
    pub overslept: u64,
    pub overslept_rung: u64,
    pub overslept_max: u64,
    /// Entries made at once after a write of its TSC deadline or of an IPI
    /// to other CPUs, without the loop's round.
    pub fast: u64,
    /// Such writes answered inside the world switch's own loop, and the
    /// guest entered again from there (`FastMsr`): exits counted among
    /// `exits`, none of them seen by the loop.
    pub answered: u64,
    /// HLTs ended there too, by an interrupt posted for the CPU while the
    /// world switch's loop waited for one (`FastMsr::hlt`): among `exits`
    /// and `hlt`, none of them a halt of the loop's.
    pub hlt_answered: u64,
    /// Entries with the APIC's interrupts the CPU's to deliver, and its EOIs
    /// to take (VT-x's virtual-interrupt delivery); and those EOIs it
    /// stopped the guest after, of level-triggered vectors.
    pub vid: u64,
    pub eoi_exits: u64,
    /// IPIs this CPU sent, and fixed ones it was sent.
    pub ipi_sent: u64,
    pub ipi_taken: u64,
    /// Devices' MSI-X messages this CPU sent on their behalf, doing their
    /// work: into its own APIC or another CPU's mailbox.
    pub msi: u64,
    /// The IO-APIC's messages this CPU sent, its pins' interrupts.
    pub ioapic: u64,
    /// MMIO accesses performed for the guest: its xAPIC page's.
    pub mmio_done: u64,
    /// INITs and start-up IPIs that reset and started this CPU.
    pub init: u64,
    pub started: u64,
    /// NMIs another CPU sent this one that it took; and those it dropped,
    /// sent while it waited for a start-up IPI -- a CPU there takes none.
    pub nmi: u64,
    pub nmi_lost: u64,
    pub blocked: u64,
    pub hlt: u64,
    /// PAUSEs the guest was stopped at, spin-waiting.
    pub pause: u64,
    /// Sleeps of the task while the vCPU was halted, and the time they took:
    /// the host CPU this guest gave back while it had nothing to do.
    pub sleeps: u64,
    pub slept_ns: u64,
    /// Halts the task spun for a ring before it slept (`Doorbell::poll`),
    /// and those a ring ended in the spin, no sleep at all.
    pub polls: u64,
    pub polled: u64,
    /// The host's time between a guest's exit and its next entry, in two
    /// parts: the exit handled, up to the top of the loop -- of which the
    /// writes of an MSR alone, the guest's IPIs and timer deadlines among
    /// them -- and the loop's look at what is waiting, up to the entry. No
    /// sleep of a halted vCPU is in either: what an exit costs the host
    /// past the world switch, which `hv bench` times.
    pub handled_ns: u64,
    pub msr_write_ns: u64,
    pub to_entry_ns: u64,
    /// Instructions the guest was told (by CPUID) it does not have, run
    /// anyway and answered with #UD -- or, RDPMC, the #GP of a counter not
    /// there; WBINVDs stepped past; `mov`s to and from CR8 answered from the
    /// shadow task-priority register (VT-x); and `mov`s to CR0 that changed
    /// a bit VT-x keeps for the host.
    pub ud: u64,
    /// Exceptions of the guest's own the host intercepted and gave back:
    /// #DB -- a single step, `int1` -- and #AC.
    pub reflected: u64,
    pub wbinvd: u64,
    pub cr8: u64,
    pub cr0: u64,
    /// `mov`s to and from CR4 performed for the guest (AMD-V, `crate::cr`),
    /// and of those writes the ones refused with #GP: a bit its CPUID does
    /// not offer, or one a CPU would not take where it was.
    pub cr4: u64,
    pub cr4_gp: u64,
    /// EOIs the guest made through its PV EOI word (KVM's), each done here
    /// at its next exit in place of one of its own.
    pub pv_eoi: u64,
    /// `mov`s to and from DR0-DR3 the guest stopped at (AMD-V,
    /// `crate::dr`), and of those the ones not made: a #GP above CPL 0, or
    /// a #DB with DR7's general detect on.
    pub dr: u64,
    pub dr_refused: u64,
    /// XSETBVs the guest stopped at (`crate::cr::xsetbv`), and of those the
    /// ones refused: a #UD without CR4.OSXSAVE, a #GP for an XCR0 a CPU
    /// does not take.
    pub xsetbv: u64,
    pub xsetbv_refused: u64,
    /// Entries made, each ended by an exit -- or turned back on its way in
    /// by a kick (`Exit::Kicked`), which is counted here too: what `hv
    /// list` calls exits is entries attempted.
    pub exits: u64,
}

impl Counts {
    /// The timer's interrupts requested more than `LATE_NS` past their
    /// edges, however they were waited for.
    pub fn late(&self) -> u64 {
        self.late_woke_late + self.late_woke_on_time + self.late_not_slept_for + self.late_spun + self.late_off_cpu
            + self.late_host + self.late_exit + self.late_other
    }

    /// Another CPU's counts into these: a guest's, from its CPUs'.
    pub fn add(&mut self, o: &Counts) {
        self.port_in += o.port_in;
        self.port_out += o.port_out;
        self.cpuid += o.cpuid;
        self.msr_read += o.msr_read;
        self.msr_write += o.msr_write;
        self.msr_gp += o.msr_gp;
        self.mmio += o.mmio;
        self.host += o.host;
        self.kicked += o.kicked;
        self.edges_passed += o.edges_passed;
        self.irq += o.irq;
        self.irq0 += o.irq0;
        self.irq4 += o.irq4;
        self.edges0 += o.edges0;
        self.apic += o.apic;
        self.timer += o.timer;
        self.vid += o.vid;
        self.eoi_exits += o.eoi_exits;
        self.ipi_sent += o.ipi_sent;
        self.ipi_taken += o.ipi_taken;
        self.msi += o.msi;
        self.ioapic += o.ioapic;
        self.mmio_done += o.mmio_done;
        self.init += o.init;
        self.started += o.started;
        self.nmi += o.nmi;
        self.nmi_lost += o.nmi_lost;
        self.blocked += o.blocked;
        self.hlt += o.hlt;
        self.pause += o.pause;
        self.sleeps += o.sleeps;
        self.slept_ns += o.slept_ns;
        self.timer_late_ns += o.timer_late_ns;
        self.timer_late_max = self.timer_late_max.max(o.timer_late_max);
        if o.worst.late > self.worst.late {
            self.worst = o.worst;
        }
        self.timer_raced += o.timer_raced;
        self.advanced += o.advanced;
        self.late_woke_late += o.late_woke_late;
        self.late_woke_late_rung += o.late_woke_late_rung;
        self.late_woke_on_time += o.late_woke_on_time;
        self.late_not_slept_for += o.late_not_slept_for;
        self.late_spun += o.late_spun;
        self.late_off_cpu += o.late_off_cpu;
        self.late_host += o.late_host;
        self.late_exit += o.late_exit;
        self.late_other += o.late_other;
        self.overslept += o.overslept;
        self.overslept_rung += o.overslept_rung;
        self.overslept_max = self.overslept_max.max(o.overslept_max);
        self.fast += o.fast;
        self.answered += o.answered;
        self.hlt_answered += o.hlt_answered;
        self.polls += o.polls;
        self.polled += o.polled;
        self.handled_ns += o.handled_ns;
        self.msr_write_ns += o.msr_write_ns;
        self.to_entry_ns += o.to_entry_ns;
        self.ud += o.ud;
        self.reflected += o.reflected;
        self.wbinvd += o.wbinvd;
        self.cr8 += o.cr8;
        self.cr0 += o.cr0;
        self.cr4 += o.cr4;
        self.cr4_gp += o.cr4_gp;
        self.pv_eoi += o.pv_eoi;
        self.dr += o.dr;
        self.dr_refused += o.dr_refused;
        self.xsetbv += o.xsetbv;
        self.xsetbv_refused += o.xsetbv_refused;
        self.exits += o.exits;
    }
}

/// The most MSR accesses answered with #GP kept for a report.
const MSR_FAULTS_KEPT: usize = 8;

/// How far apart a NIC's receive interrupts are held at least, unless a
/// guest asks otherwise (`rxmod=`), and the most it may ask: a frame after a
/// quiet spell is still interrupted for at once (`net::Net::set_rx_moderation`).
/// 100 us, from what the EX44 measured (docs/hypervisor.md): two guests'
/// TCP through the switch 18% faster, ten frames an interrupt where there
/// was one, and a hundred small HTTP fetches between them as quick as
/// without; 200 us was 23% faster but made each fetch a quarter slower.
pub const RX_MODERATION_DEFAULT_NS: u64 = 100_000;
pub const MAX_RX_MODERATION_NS: u64 = 1_000_000;
/// How many exits go by between reports of progress while the guest does
/// not halt: a busy VM's counters are still seen moving.
const PROGRESS_EVERY: u64 = 4096;

/// What runs a guest gives its loops: where the guest's console goes, what
/// is typed at it, and whether to stop. Shared by every CPU's task, each of
/// which calls it between entries, in task context -- never with the guest
/// running, never with interrupts off. `output` and `input` are called
/// with the guest's devices locked, so the bytes of its console come in the
/// order the guest wrote them, from whichever CPU.
pub trait Host: Sync {
    /// A byte the guest wrote to its serial console.
    fn output(&self, byte: u8);
    /// The next byte typed at the guest's console, if there is one. Asked
    /// when the serial port's receive register is free; `at_prompt` says
    /// whether the guest has reached a shell prompt yet -- it has asked
    /// where its cursor is -- before which a line typed by a script is
    /// swallowed by the boot and is better held back. A person typing at
    /// the console knows when to, and is not.
    fn input(&self, at_prompt: bool) -> Option<u8>;
    /// Whether the guest is to be stopped: asked before every entry, and at
    /// least every host tick while a CPU is halted. Whoever says yes rings
    /// the guest's doorbells too, so that a CPU asleep for good looks.
    fn stop_requested(&self) -> bool;
    /// What CPU `cpu`'s loop has counted so far: when it halts, every
    /// `PROGRESS_EVERY` exits, and once more at the end. For a VM that runs
    /// until it is stopped, how anyone else sees it doing.
    fn progress(&self, _cpu: u32, _counts: &Counts) {}
    /// Whether the guest's power button has been pressed since this last
    /// said so: asked on every round of the first CPU's loop, with the
    /// guest's devices locked, and yes once a press. Whoever presses it
    /// rings the first CPU's doorbell, so that a halted guest hears it now.
    fn power_button(&self) -> bool {
        false
    }
    /// What became of the press: whether the guest's OS has the power
    /// button's event enabled, and so is told of it by an SCI -- or has no
    /// ACPI, or has not got that far, and never hears of it.
    fn power_button_heard(&self, _heard: bool) {}
}

/// What a guest CPU is doing, as its loop sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    /// Its guest runs.
    Running,
    /// It executed HLT with interrupts on: not entered until an interrupt it
    /// can take is pending.
    Halted,
    /// It executed HLT with interrupts off: only an NMI or an INIT from
    /// another CPU would wake it.
    Asleep,
    /// It has been reset, or never started: it waits for a start-up IPI.
    WaitForSipi,
}

impl Activity {
    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(code: u8) -> Activity {
        match code {
            0 => Activity::Running,
            1 => Activity::Halted,
            2 => Activity::Asleep,
            _ => Activity::WaitForSipi,
        }
    }

    /// For a report.
    pub fn name(self) -> &'static str {
        match self {
            Activity::Running => "running",
            Activity::Halted => "halted",
            Activity::Asleep => "halted with interrupts off",
            Activity::WaitForSipi => "waiting for a start-up IPI",
        }
    }
}

/// One of a guest's CPUs, as the task that runs it has it: its registers
/// and the extension's state for it (`Cpu`), its local APIC, and what it is
/// doing. Nothing here is shared: another CPU reaches this one only through
/// its mailbox.
pub struct GuestCpu {
    index: u32,
    cpu: Cpu,
    lapic: Lapic,
    activity: Activity,
    /// An NMI another CPU sent it, not yet injected: an NMI is latched, one
    /// at a time, until the CPU can take it.
    nmi_pending: bool,
    /// An exception of the guest's own -- #DB, #AC -- the host intercepted
    /// and gives back, with its error code: injected before anything else
    /// once nothing an exit cut short is on its way in (`deliver`).
    exception: Option<(u8, Option<u32>)>,
    /// The last interrupt injected came from the 8259: when it and the APIC
    /// both have one, they take turns (`LinuxGuest::deliver`).
    last_from_pic: bool,
    /// MSR_KVM_PV_EOI_EN as the guest wrote it: its PV EOI word's address
    /// and, in bit 0, whether it is on; and whether the word was set before
    /// the last entry (`pv_eoi_arm`), for the exit after to look at.
    pv_eoi: u64,
    pv_eoi_armed: bool,
    /// How long its task spins for a ring when it halts (`POLL_MAX_NS`).
    poll_ns: u64,
    /// Its last exit a write the next entry may follow without the loop's
    /// round (`msr`): its TSC deadline, an IPI to other CPUs.
    fast: bool,
    /// What its task did last (`LATE_NS`).
    step: Step,
    /// The first CPU's other timers' next edge, as its last round armed it
    /// besides its APIC's -- the PIT's, the devices' (`devices_due`) -- kept
    /// for an entry made without the round, which arms the host's timer for
    /// it too.
    other_due: Option<u64>,
    /// What the host's timer was last asked for (`Kick::arm_at`).
    asked: Option<u64>,
    /// Its local APIC as it last published it (`publish`): its addressing,
    /// and whether it was software-enabled -- what decides whether a message
    /// it was sent, or one that reached nobody, would be taken now.
    published: (u64, bool),
    counts: Counts,
    /// When a slowed guest's CPU next leaves it for its share of a slice
    /// (`LinuxGuest::throttle`), host nanoseconds.
    throttle_at: u64,
    /// Its last dirty-log fault (`LinuxGuest::dirty_write`): the page, the
    /// table's identity then, and how many in a row.
    refault: (u64, u64, u32),
}

/* KVM's PV EOI (`policy::MSR_KVM_PV_EOI_EN`): the MSR's on bit, the bit of
 * the guest's word set while its EOI need not exit, and the MSR's bit that
 * a word aligned to 4 bytes leaves clear -- set, a #GP, as KVM gives. */
const PV_EOI_ON: u64 = 1;
const PV_EOI_PENDING: u8 = 1;
const PV_EOI_RESERVED: u64 = 1 << 1;
/// What every hypercall is answered in RAX: -KVM_ENOSYS, as KVM answers one
/// it does not know -- none is offered.
const KVM_ENOSYS: u64 = (-1000i64) as u64;

/// What a CPU asleep for good does next (`LinuxGuest::sleep_for_good`).
enum Sleep {
    /// Mail came as it went to sleep: it takes it.
    Woken,
    /// Wait to be rung.
    Wait,
    /// It is the last of the guest's CPUs asleep, and no mail is on its way
    /// to any: the guest has stopped.
    Last,
}

impl GuestCpu {
    /// Its number among the guest's CPUs, which is its APIC ID.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Its registers and the last exit, for a report.
    pub fn dump(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        self.cpu.backend().dump(out)
    }

    /// Its CPU, for a guest of this crate's own that starts it somewhere
    /// other than a kernel's entry (`crate::guests`).
    pub(crate) fn backend_mut(&mut self) -> &mut crate::vm::Backend {
        self.cpu.backend_mut()
    }
}

/// What a guest's CPUs share of each one: its mailbox, and what the others
/// and a report read of it.
struct CpuShared {
    mail: Mailbox,
    /// Its local APIC takes the 8259's interrupts: a CPU whose port access
    /// raised one rings the CPUs this is set for.
    extint: AtomicBool,
    activity: AtomicU8,
    /// The host CPU it last entered its guest on; `u32::MAX` before its first.
    host_cpu: AtomicU32,
    /// How a message finds its local APIC -- its mode, and in xAPIC mode its
    /// logical ID and model -- as it last published ([`Addressing::pack`]).
    addressing: AtomicU64,
    /// Where other CPUs post its fixed, edge-triggered interrupts for the
    /// CPU to take in its guest (VT-x's posted interrupts), where it can;
    /// and whether it does -- its guest asked for its interrupts to be the
    /// CPU's (`LinuxGuest::set_apicv`), and its CPU took the descriptor
    /// before its first entry. Its mailbox takes them otherwise.
    posted: Option<Arc<PostedInterrupts>>,
    posted_on: AtomicBool,
    /// The nested table's identity as the CPU last passed the top of its
    /// loop, outside its guest: every entry after it is over that table or a
    /// later one, which drops what the TLB kept of the table before. What a
    /// migration's dirty log waits on (`sync_tlbs`).
    epoch: AtomicU64,
}

impl CpuShared {
    /// Its local APIC's addressing, as it last published it.
    fn addressing(&self) -> Addressing {
        Addressing::unpack(self.addressing.load(Ordering::Relaxed))
    }
}

/// The guest's devices: what its CPUs share under one lock.
struct Platform {
    uart: Uart,
    pit: Pit,
    rtc: Rtc,
    pic: Pic,
    pm: Pm,
    pvpanic: PvPanic,
    /// The IO-APIC, for a guest given one.
    ioapic: Option<IoApic>,
    pci: PciBus,
    /// What is on the bus, by slot: slot `i + 1` is `pci_devs[i]`. Disks and
    /// NICs in the order they were added, `vda` and `eth0` first.
    pci_devs: Vec<PciDev>,
    disks: usize,
    nics: usize,
    /// Each device's MSI-X entries that came while masked, a bit each, sent
    /// once unmasked; as `pci_devs`, device by device.
    msix_pending: Vec<u32>,
    /// A tally of reads of the low ports, to find a guest spinning on one.
    port_hist: Box<[u32; 1024]>,
    /// The first MSR accesses the policy refused with #GP: (MSR, value
    /// written, whether a write). A guest's `rdmsr_safe` takes the fault in
    /// silence, so the report is the only place such a thing shows.
    msr_faults: Vec<(u32, u64, bool)>,
    /// The host's functions the guest holds (`add_host_function`), each
    /// behind its device's root port or on a slot of its own, after every
    /// emulated device's.
    hosts: Vec<Hosted>,
    /// The root ports above them (`devices::port`), one a device.
    ports: Vec<Port>,
}

/// A function of the host's as the module names it: the host's bus, device
/// and function -- which say the functions that are one device, behind one
/// port at their own numbers (`LinuxGuest::add_host_function`). Ordered as
/// the host's bus orders them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostAddress {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

/// A root port above a host's device: its slot on bus 0, the host's bus and
/// device it is above, the host's number of the device's first function
/// given -- function 0 on the bus below -- and the spans its BARs took of
/// each window, which its windows are made to hold.
struct Port {
    slot: usize,
    device: (u8, u8),
    first: u8,
    port: RootPort,
    low: Option<(u64, u64)>,
    high: Option<(u64, u64)>,
}

/// What a configuration access of a host's slot or of a bus below a port
/// names (`Platform::host_config`): a port, or a host's function, by its
/// index in `Platform::ports` or `hosts`.
#[derive(Clone, Copy, Debug)]
enum HostConfig {
    Port(usize),
    Function(usize),
}

impl Platform {
    /// What `t` names: on bus 0, a port at its slot or a host's function on
    /// a slot of its own, function 0 alone; on the bus below a port, as the
    /// guest numbers it now, device 0's function of that number; None for
    /// anything else -- nothing answers, and a read reads all ones.
    fn host_config(&self, t: &pci::HostTarget) -> Option<HostConfig> {
        if t.bus == 0 {
            let slot = usize::from(t.device);
            if t.function != 0 {
                return None;
            }
            if let Some(i) = self.ports.iter().position(|q| q.slot == slot) {
                return Some(HostConfig::Port(i));
            }
            return self.hosts.iter().position(|h| h.port.is_none() && h.slot == slot).map(HostConfig::Function);
        }
        let port = self.ports.iter().position(|q| q.port.below(t.bus))?;
        if t.device != 0 {
            return None;
        }
        self.hosts.iter().position(|h| h.port == Some(port) && h.number == t.function).map(HostConfig::Function)
    }

    /// The link of the device below port `i`, as its function 0 says it now.
    fn port_link(&self, i: usize) -> port::Link {
        self.hosts.iter().find(|h| h.port == Some(i) && h.number == 0).map(|h| h.function.link()).unwrap_or_default()
    }

    /// A configuration read of `size` bytes at `t`.
    fn host_config_read(&self, t: &pci::HostTarget, size: usize) -> u32 {
        let none = if size >= 4 { u32::MAX } else { (1u32 << (8 * size)) - 1 };
        match self.host_config(t) {
            Some(HostConfig::Port(i)) => self.ports.get(i).map_or(none, |q| q.port.read(t.at, size, &self.port_link(i))),
            Some(HostConfig::Function(i)) => self.hosts.get(i).map_or(none, |h| h.function.config_read(t.at, size)),
            None => none,
        }
    }
}

/// A host's function on the guest's bus, and where its BARs are.
struct Hosted {
    /// Where it is: behind its device's port -- an index in `ports` -- as
    /// function `number` of the bus below, or None, alone on bus 0 at
    /// `slot`; `slot` is the port's where it has one.
    port: Option<usize>,
    number: u8,
    slot: usize,
    /// Which of the guest's host functions it is, for its ROM's frames
    /// (`GuestMemory::map_rom`).
    id: usize,
    function: HostFunction,
    /// Each BAR's mapping in the guest's space now: its address, none while
    /// the guest has it nowhere the table can map; the ROM's (`ROM_BAR`)
    /// where it is on and trapped there.
    mapped: [Option<u64>; host::ROM_BAR + 1],
    /// Each BAR's pages trapped rather than mapped, by their offsets in it:
    /// NVIDIA's configuration space mirror, MSI-X's table and PBA
    /// (`HostFunction::traps`).
    holes: [Vec<u64>; 6],
}

impl Hosted {
    /// The BAR and the offset in it of `gpa`, where it is in a page of a BAR
    /// mapped now that traps.
    fn trapped_at(&self, gpa: u64) -> Option<(usize, u64)> {
        const PAGE: u64 = kcore::consts::PAGE_SIZE as u64;
        let bars = self.function.bars();
        (0..6).find_map(|bar| {
            let (base, b) = (self.mapped[bar]?, bars[bar]?);
            let offset = gpa.checked_sub(base).filter(|&o| o < b.size)?;
            self.holes[bar].contains(&(offset & !(PAGE - 1))).then_some((bar, offset))
        })
    }

    /// The offset of `gpa` in the ROM, where the guest has it on there.
    fn rom_at(&self, gpa: u64) -> Option<u64> {
        let (base, size) = (self.mapped[host::ROM_BAR]?, self.function.rom_size()?);
        gpa.checked_sub(base).filter(|&o| o < size)
    }
}

/// Where the host's functions' BARs go: the 32-bit window below the
/// platform's pages and the 64-bit one above RAM, each with the next free
/// address.
#[derive(Clone, Copy, Debug)]
struct Windows {
    low: u64,
    low_end: u64,
    high: u64,
    high_end: u64,
}

/// A Linux guest: its memory, its devices, and what its CPUs share.
pub struct LinuxGuest {
    memory: GuestMemory,
    platform: Mutex<Platform>,
    cpus: Vec<CpuShared>,
    doorbells: Arc<Doorbells>,
    /// A bit a CPU asleep for good: halted with interrupts off, or waiting
    /// for a start-up IPI. Each CPU's bit is its own: set as it goes to
    /// wait, and cleared as it wakes, before it takes its mail -- so a set
    /// bit is a CPU that has taken nothing and sent nothing since it last
    /// found its mailbox empty (`sleep_for_good`). When a CPU's own bit
    /// makes it every CPU's, and every mailbox is empty, the guest has
    /// stopped: nobody is left awake to send anybody anything.
    asleep: AtomicU64,
    /// Every CPU's bit.
    all: u64,
    /// Set by the CPU that stops the guest, for every other to see.
    stopping: AtomicBool,
    stopped: Mutex<Option<Stopped>>,
    /// How many CPUs a start-up IPI has started, for a report.
    started: AtomicU64,
    /// Where the last CPU to halt with interrupts off halted, for the report
    /// of a guest every CPU of which is asleep: the one that finds so may be
    /// one never started, with no such address of its own. Stored before
    /// that CPU sets its bit in `asleep`, whose read-modify-write orders it
    /// for the CPU that sets the last.
    last_halt: AtomicU64,
    /// Its furniture is laid out (`load`): the ACPI tables describe the
    /// devices it had then, and it takes no more.
    loaded: bool,
    /// Where a host function's BARs go (`add_host_function`): None for a
    /// guest made without the PCI hole they need (`new_holed`), and with
    /// what the windows were when made, for the tables.
    windows: Option<(Windows, Windows)>,
    /// Where its RAM below 4 GiB ends (`linux::Ram::of_below`): what a
    /// layout loaded into it was planned for, or it is refused (`load`) --
    /// a memory map of another guest's RAM would hand its kernel the BARs'
    /// hole as RAM.
    low_end: u64,
    /// The CR4 bits its CPUs may set, by the CPUID they are told
    /// (`policy::cr4_allowed`): asked of the host once, not at every write.
    cr4_allowed: u64,
    /// Whether its CPUs' interrupts are the CPU's to deliver where it can
    /// (VT-x's virtual-interrupt delivery): as `set_apicv` said, or -- None,
    /// the default -- where the CPU takes another CPU's interrupts posted too.
    apicv: Option<bool>,
    /// Interrupts posted for a CPU to take in its guest, and of those the
    /// ones whose notification was sent to the CPU it was in its guest on.
    posted: AtomicU64,
    notified: AtomicU64,
    /// MSIs a disk's task sent from outside every CPU (`serve_disk`).
    served_msi: AtomicU64,
    /// MSIs of its host's functions, sent by the host's handlers of their
    /// real ones (`deliver_msi`), and those no CPU took.
    host_msi: AtomicU64,
    host_msi_lost: AtomicU64,
    /// How far apart its NICs' receive interrupts are held at least
    /// (`set_rx_moderation`), and whether they offer their transmit
    /// offloads (`set_offloads`).
    rx_moderation_ns: u64,
    offloads: bool,
    /// Where its CPUs sit, as their CPUID tells them (`set_shape`).
    shape: Shape,
    /// What its SMBIOS says it is (`set_identity`).
    identity: Identity,
    /// The extension it runs under, and whether its local APICs came out of
    /// reset in x2APIC mode: what a state of it says it was built with.
    vendor: hvarch::Vendor,
    x2apic: bool,
    /// What its TSC is ahead of the host's: 0 for a guest that booted here,
    /// and for one restored from a state what makes its TSC go on from the
    /// one it had (`restore`, `rebase`) -- the same for every CPU. Set before
    /// its CPUs run, and read by them.
    tsc_offset: AtomicU64,
    /// When a state was put into it, host nanoseconds: what `rebase` moves
    /// its clocks on from, the time it took to start it left out. 0 for a
    /// guest that booted.
    restored_at: AtomicU64,
    /// The TSC rate its guest was told, Hz: this host's for a guest that
    /// booted here, the state's for one restored -- what it goes on
    /// counting its time by, and what a state taken of it says
    /// (`capture`): a guest moved twice is held to the rate it booted
    /// with, not to the rate of the host before, so that the tolerance does
    /// not add up along the way.
    told_hz: AtomicU64,
    /// How much of their time its CPUs are kept out of it, percent: a
    /// migration's that the guest writes faster than it copies (`throttle`).
    /// 0 for none.
    throttle: AtomicU32,
    /// A pause asked for (`pause`), by its number -- 0 for none: every CPU
    /// parks at the top of its loop, its state read into its slot of
    /// `parked`, until the pause is over -- resumed, or the guest ended.
    pause: AtomicU64,
    parked: Mutex<Vec<Option<Parked>>>,
}

/// What a CPU parked with (`LinuxGuest::park`): the pause's number, its
/// state as it read it -- on the host CPU it ran on, where VT-x's VMCS is
/// current -- and when.
struct Parked {
    pause: u64,
    state: vmstate::Result<vmstate::Cpu>,
    at: u64,
}

/// How long a pause waits for every CPU to park, and the disks to answer
/// what they were asked: past it, the pause is given up and the guest goes
/// on.
const PARK_NS: u64 = 5 * kcore::consts::NS_PER_SEC;
/// A slice of a slowed guest's CPU (`throttle`): in its guest for one, then
/// out for its share of it.
const THROTTLE_SLICE_NS: u64 = 10 * kcore::consts::NS_PER_MS;
/// The most a guest is slowed: it never stops for good.
pub const THROTTLE_MAX: u32 = 90;
const DRAIN_NS: u64 = 30 * kcore::consts::NS_PER_SEC;
/// How long a parked CPU waits for a ring before it looks again.
const PARKED_WAIT_NS: u64 = 10 * kcore::consts::NS_PER_MS;
/// The most two hosts' TSC rates may differ by for a guest to move between
/// them, parts per million: KVM's `tsc_tolerance_ppm`. Past it a guest's
/// clock, told one rate at its boot, would run off the other's.
pub const TSC_TOLERANCE_PPM: u64 = 250;
/// The next pause's number.
static NEXT_PAUSE: AtomicU64 = AtomicU64::new(1);

/* The SMBIOS tables after the ACPI ones in the BIOS area, laid out in the
 * same buffer. */
const _: () = assert!(acpi::AREA + acpi::MAX_BYTES as u64 <= smbios::AREA
                      && smbios::AREA + smbios::MAX_BYTES as u64 <= acpi::AREA_END
                      && smbios::MAX_BYTES <= acpi::MAX_BYTES);

/// What a guest's SMBIOS tables say of it (`smbios`): its UUID, as RFC 4122
/// writes it -- all zeros, SMBIOS's "not set", until it is given one -- its
/// serial number, and the hypervisor's version.
pub struct Identity {
    pub uuid: [u8; 16],
    pub serial: String,
    pub version: String,
}

/// What a guest's OS did with its ACPI, for a report.
#[derive(Clone, Copy, Debug)]
pub struct AcpiStats {
    /// It wrote its event enables -- which an OS taking ACPI starts by, all
    /// of them off: its kernel took the ACPI it was given.
    pub used: bool,
    pub scis: u64,
    pub presses: u32,
    /// A sleep type other than S5's it asked for, which nothing was done
    /// about.
    pub other_sleep: Option<u8>,
}

impl LinuxGuest {
    /// A guest of `cpus` CPUs, rung by `doorbells` (one each), with
    /// `mem_bytes` of RAM and nothing loaded yet: the guest, and its CPUs
    /// for their tasks to run -- the first ready to be put at the kernel's
    /// entry (`load`), the rest waiting for a start-up IPI. The CPUs stop at
    /// no exception of their own -- a Linux guest has an IDT and handles its
    /// own faults -- and the host still intercepts #DB, #AC and #MC.
    ///
    /// Under VT-x, a guest of more than one CPU needs unrestricted guest:
    /// its other CPUs start in real mode.
    ///
    /// Their local APICs come out of reset in x2APIC mode, or -- `x2apic`
    /// false -- in xAPIC mode, reached through the page: for a kernel that
    /// reads the page before it looks at the mode firmware left, which a
    /// Linux of 6.5 or older without ACPI does.
    ///
    /// With `ioapic`, the machine has an IO-APIC too, its ID the one after
    /// the CPUs' -- a guest of more than one CPU only, since it delivers to
    /// local APICs -- and its APICs come out of reset in xAPIC mode, as a
    /// PC's firmware leaves them: x2APIC mode with an IO-APIC and no
    /// interrupt remapping is not one the architecture has, and a Linux
    /// built without KVM_GUEST, finding its firmware left one so, turns it
    /// off -- through a page it has not mapped, 6.1 and 6.18 alike, and
    /// faults. One built with it keeps it here, the CPUID's hypervisor range
    /// being KVM's (`policy`), but xAPIC mode is the one every kernel takes.
    ///
    /// Its RAM is laid out as `linux::Ram` says, and is the huge pool's
    /// when `huge` -- every byte of it, 2 MiB a block, or `NoHugePages` --
    /// and frames of its own when not.
    #[allow(clippy::too_many_arguments)]
    pub fn new(machine: &Machine, mem_bytes: u64, huge: bool, cpus: u32, doorbells: Arc<Doorbells>, x2apic: bool,
               ioapic: bool) -> Result<(Self, Vec<GuestCpu>)>
    {
        Self::build(machine, mem_bytes, huge, cpus, doorbells, x2apic, ioapic, linux::LOW_RAM_END, None)
    }

    /// As [`new`](Self::new), its RAM from NUMA node `node` first where it
    /// names one ([`GuestMemory::place_on`]): its CPUs' node.
    #[allow(clippy::too_many_arguments)]
    pub fn new_on(machine: &Machine, mem_bytes: u64, huge: bool, cpus: u32, doorbells: Arc<Doorbells>, x2apic: bool,
                  ioapic: bool, node: Option<usize>) -> Result<(Self, Vec<GuestCpu>)>
    {
        Self::build(machine, mem_bytes, huge, cpus, doorbells, x2apic, ioapic, linux::LOW_RAM_END, node)
    }

    /// As [`new`](Self::new), for a guest that is to hold a host's function
    /// (`add_host_function`): its RAM below 4 GiB ending at
    /// `linux::HOLE_RAM_END`, the rest above, so that a 32-bit BAR has room
    /// below the platform's pages, and a 64-bit one above its RAM.
    #[allow(clippy::too_many_arguments)]
    pub fn new_holed(machine: &Machine, mem_bytes: u64, huge: bool, cpus: u32, doorbells: Arc<Doorbells>,
                     x2apic: bool, ioapic: bool) -> Result<(Self, Vec<GuestCpu>)>
    {
        Self::build(machine, mem_bytes, huge, cpus, doorbells, x2apic, ioapic, linux::HOLE_RAM_END, None)
    }

    /// As [`new_holed`](Self::new_holed), its RAM from NUMA node `node`
    /// first where it names one.
    #[allow(clippy::too_many_arguments)]
    pub fn new_holed_on(machine: &Machine, mem_bytes: u64, huge: bool, cpus: u32, doorbells: Arc<Doorbells>,
                        x2apic: bool, ioapic: bool, node: Option<usize>) -> Result<(Self, Vec<GuestCpu>)>
    {
        Self::build(machine, mem_bytes, huge, cpus, doorbells, x2apic, ioapic, linux::HOLE_RAM_END, node)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(machine: &Machine, mem_bytes: u64, huge: bool, cpus: u32, doorbells: Arc<Doorbells>, x2apic: bool,
             ioapic: bool, low_end: u64, node: Option<usize>) -> Result<(Self, Vec<GuestCpu>)>
    {
        let n = cpus as usize;
        if n == 0 || n > MAX_CPUS || doorbells.len() != n || (ioapic && n < 2) || mem_bytes > MAX_MEM_BYTES {
            return Err(Error::BadAddress);
        }
        let ram = linux::Ram::of_below(mem_bytes, low_end);
        let windows = (low_end != linux::LOW_RAM_END).then(|| {
            /* A GiB past the end of RAM, and up to the width the guest is
             * told, MAX_GPA at most */
            let top = (1u64 << crate::policy::phys_bits()).min(crate::memory::MAX_GPA);
            let high = ram.end().unwrap_or(top).max(linux::HIGH_RAM_START).div_ceil(1 << 30) << 30;
            let w = Windows { low: low_end, low_end: linux::ECAM_BASE, high, high_end: top };
            (w, w)
        });
        if ram.end().is_none_or(|end| end > 1u64 << crate::policy::phys_bits()) {
            return Err(Error::BadAddress);
        }
        let x2apic = x2apic && !ioapic;
        let mut memory = GuestMemory::new(machine.caps().vendor())?;
        if let Some(node) = node {
            memory.place_on(node);
        }
        for (base, size) in ram.regions() {
            if huge {
                memory.add_huge(base, size, machine.caps().nested_2m())?;
            } else {
                memory.add(base, size)?;
            }
        }
        /* The PCI Express configuration space, every access of it the run
         * loop's (`ecam`), where the guest has the BARs' hole */
        if windows.is_some() {
            memory.trap_range(linux::ECAM_BASE, linux::ECAM_BYTES)?;
        }

        let mut vcpus = Vec::new();
        vcpus.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        let mut shared = Vec::new();
        shared.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        for i in 0..cpus {
            let cpu = Cpu::new(machine, &memory, 0, i)?;
            if i > 0 && !cpu.backend().runs_real_mode() {
                return Err(Error::NoUnrestrictedGuest);
            }
            let posted = cpu.backend().posted_capable().then(|| Arc::new(PostedInterrupts::new()));
            let activity = if i == 0 { Activity::Running } else { Activity::WaitForSipi };
            let lapic = Lapic::new(i, i == 0, x2apic);
            let addressing = lapic.addressing().pack();
            let published = (addressing, lapic.software_enabled());
            vcpus.push(GuestCpu {
                index: i,
                cpu,
                lapic,
                activity,
                nmi_pending: false,
                exception: None,
                last_from_pic: false,
                pv_eoi: 0,
                pv_eoi_armed: false,
                poll_ns: 0,
                fast: false,
                step: Step::Other,
                other_due: None,
                asked: None,
                published,
                counts: Counts::default(),
                throttle_at: 0,
                refault: (0, 0, 0),
            });
            shared.push(CpuShared {
                mail: Mailbox::new(),
                extint: AtomicBool::new(i == 0),
                activity: AtomicU8::new(activity.code()),
                host_cpu: AtomicU32::new(u32::MAX),
                addressing: AtomicU64::new(addressing),
                posted,
                posted_on: AtomicBool::new(false),
                epoch: AtomicU64::new(0),
            });
        }

        let port_hist = alloc::vec![0u32; 1024].into_boxed_slice().try_into()
            .map_err(|_| Error::NoMemory)?;
        /* Its whole capacity now, fallibly, so that recording a fault later
         * never allocates. */
        let mut msr_faults = Vec::new();
        msr_faults.try_reserve_exact(MSR_FAULTS_KEPT).map_err(|_| Error::NoMemory)?;
        let mut pci_devs = Vec::new();
        pci_devs.try_reserve_exact(MAX_DISKS + MAX_NICS).map_err(|_| Error::NoMemory)?;
        let mut msix_pending = Vec::new();
        msix_pending.try_reserve_exact(MAX_DISKS + MAX_NICS).map_err(|_| Error::NoMemory)?;
        let platform = Platform {
            uart: Uart::new(),
            pit: Pit::new(),
            rtc: Rtc::new(),
            pic: Pic::new(),
            pm: Pm::new(time::boot_time_ns()),
            pvpanic: PvPanic::default(),
            ioapic: ioapic.then(|| IoApic::new(cpus as u8)),
            pci: PciBus::new()?,
            pci_devs,
            disks: 0,
            nics: 0,
            msix_pending,
            port_hist,
            msr_faults,
            hosts: Vec::new(),
            ports: Vec::new(),
        };
        let all = if n == u64::BITS as usize { u64::MAX } else { (1u64 << n) - 1 };
        let mut parked = Vec::new();
        parked.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        parked.resize_with(n, || None);
        let guest = LinuxGuest {
            memory,
            platform: Mutex::new(platform).ok_or(Error::NoMemory)?,
            cpus: shared,
            doorbells,
            asleep: AtomicU64::new(0),
            all,
            stopping: AtomicBool::new(false),
            stopped: Mutex::new(None).ok_or(Error::NoMemory)?,
            started: AtomicU64::new(0),
            last_halt: AtomicU64::new(0),
            loaded: false,
            windows,
            low_end,
            cr4_allowed: crate::policy::cr4_allowed(),
            apicv: None,
            posted: AtomicU64::new(0),
            notified: AtomicU64::new(0),
            served_msi: AtomicU64::new(0),
            host_msi: AtomicU64::new(0),
            host_msi_lost: AtomicU64::new(0),
            rx_moderation_ns: RX_MODERATION_DEFAULT_NS,
            offloads: true,
            shape: Shape::cores(cpus),
            identity: Identity { uuid: [0; 16], serial: String::new(), version: String::new() },
            vendor: machine.caps().vendor(),
            x2apic,
            tsc_offset: AtomicU64::new(0),
            restored_at: AtomicU64::new(0),
            told_hz: AtomicU64::new(time::cycle_counter_hz().unwrap_or(0)),
            throttle: AtomicU32::new(0),
            pause: AtomicU64::new(0),
            parked: Mutex::new(parked).ok_or(Error::NoMemory)?,
        };
        Ok((guest, vcpus))
    }

    /// What its SMBIOS tables say it is: set before it boots, and the same
    /// at every boot -- a UUID, above all, that an OS takes for the
    /// machine's own.
    pub fn set_identity(&mut self, identity: Identity) {
        self.identity = identity;
    }

    /// What its CPUs' CPUID tells them of where they sit -- what their
    /// placement made of them (`place::Placement::shape`) -- set before it
    /// boots; each a core of its own, sharing the cache, unless told. A
    /// shape of another number of CPUs is not this guest's, and changes
    /// nothing.
    pub fn set_shape(&mut self, shape: Shape) {
        if shape.cpus() == self.cpus() {
            self.shape = shape;
        }
    }

    /// Its memory: for the kernel and the initrd to be streamed into.
    pub fn memory(&self) -> &GuestMemory {
        &self.memory
    }

    /// How many CPUs it has.
    pub fn cpus(&self) -> u32 {
        self.cpus.len() as u32
    }

    /// Whether its CPUs' interrupts are the CPU's to deliver, where it can
    /// (VT-x's virtual-interrupt delivery, `crate::vmx::Vcpu::load_apic`),
    /// set before it runs; off, the run loop injects them. Unset, they are
    /// where the CPU also takes another CPU's interrupts posted for it in
    /// its guest (`Vcpu::posted_capable`): without, an interrupt for a CPU
    /// in its guest still takes a kick to hand over, and with no EOI exit to
    /// find it at, more of them do -- docs/hypervisor.md has what that costs.
    pub fn set_apicv(&mut self, on: bool) {
        self.apicv = Some(on);
    }

    /// Its NICs' transmit offloads offered (the default) or not
    /// (`net::Net::set_offloads`), for the NICs it has and those it is given
    /// after; set before it boots.
    pub fn set_offloads(&mut self, on: bool) {
        self.offloads = on;
        for d in self.platform.lock().pci_devs.iter_mut() {
            if let PciDev::Nic(n) = d {
                n.set_offloads(on);
            }
        }
    }

    /// Its NICs' receive interrupts no closer than `ns` apart -- 0 for each
    /// as its frame comes (`net::Net::set_rx_moderation`); the default is
    /// [`RX_MODERATION_DEFAULT_NS`]. For the NICs it has and those it is
    /// given after.
    pub fn set_rx_moderation(&mut self, ns: u64) {
        let ns = ns.min(MAX_RX_MODERATION_NS);
        self.rx_moderation_ns = ns;
        for d in self.platform.lock().pci_devs.iter_mut() {
            if let PciDev::Nic(n) = d {
                n.set_rx_moderation(ns);
            }
        }
    }


    /// Interrupts posted for its CPUs to take in their guest, and of those
    /// the ones a notification was sent for (VT-x's posted interrupts).
    pub fn posted_stats(&self) -> (u64, u64) {
        (self.posted.load(Ordering::Relaxed), self.notified.load(Ordering::Relaxed))
    }

    /// MSIs its disks' tasks sent from outside every CPU (`serve_disk`).
    pub fn served_msis(&self) -> u64 {
        self.served_msi.load(Ordering::Relaxed)
    }

    /// MSIs of its host's functions sent on to it (`deliver_msi`), and of
    /// those how many no CPU took -- a message the guest programmed to no
    /// CPU of its, or of a kind not taken.
    pub fn host_msis(&self) -> (u64, u64) {
        (self.host_msi.load(Ordering::Relaxed), self.host_msi_lost.load(Ordering::Relaxed))
    }

    /// Whether its CPUs have local APICs: a guest of more than one CPU. One
    /// of one is the PC it always was -- an 8259 on the CPU's interrupt pin,
    /// no APIC in CPUID, no MP table, the APIC's MSRs none of its business
    /// -- which any kernel boots; the local APIC, its MP table and its x2APIC
    /// mode take a Linux of 6.6 or later.
    pub fn has_apic(&self) -> bool {
        self.cpus.len() > 1
    }

    /// Whether it has an IO-APIC.
    pub fn has_ioapic(&self) -> bool {
        self.platform.lock().ioapic.is_some()
    }

    /// What its IO-APIC has done, when it has one.
    pub fn ioapic_stats(&self) -> Option<ioapic::Stats> {
        self.platform.lock().ioapic.as_ref().map(|io| io.stats)
    }

    /// Give it another disk, over `backend`: `vda`, `vdb`, ... in the order
    /// they are added, each a virtio block device on the PCI bus.
    pub fn add_disk(&mut self, backend: Box<dyn blk::Backend>, id: &str) -> Result<()> {
        if self.loaded {
            /* The tables have the devices of `load`'s time. */
            return Err(Error::BadAddress);
        }
        let apic = self.has_apic();
        let mut p = self.platform.lock();
        if p.disks >= MAX_DISKS {
            return Err(Error::NoMemory);
        }
        /* `p.disks` is under MAX_DISKS, checked above. */
        let mut disk = Blk::new(backend, id, u8::try_from(p.disks).map_err(|_| Error::NoMemory)?)?;
        let io_base = DISK_IO_BASE + (p.disks as u16) * (blk::BAR_SIZE as u16);
        let mut f = Function::device(&Blk::identity(), io_base, blk::BAR_SIZE, DISK_IRQ);
        if let Some(page) = msix_page(&mut self.memory, apic, p.pci_devs.len() + 1)? {
            disk.offer_msix(DISK_VECTORS).ok_or(Error::NoMemory)?;
            f = f.with_msix(DISK_VECTORS, page);
        }
        p.pci.add(f)?;
        /* Into the room taken at `new`. */
        p.pci_devs.push(PciDev::Disk(disk));
        p.msix_pending.push(0);
        p.disks += 1;
        Ok(())
    }

    /// Give it a function of the host's (`devices::host`), the host's `at`: a
    /// PCIe endpoint behind a root port of its device's (`devices::port`) --
    /// the one of the function before it, where that one is of the same
    /// device, else a new one on the next slot of bus 0, its bus below
    /// numbered after the ports before it -- at its number less that of the
    /// device's first function given, so that the first is function 0, as
    /// Linux scans for; any other function alone on the next slot of bus 0.
    /// A device's functions come one after another and in order (the module
    /// sorts them), or are refused. Each BAR is placed where firmware would
    /// have: a 64-bit prefetchable one above RAM, any other in the window
    /// below the platform's pages -- a port's memory window is 32 bits wide
    /// -- each at its size's alignment, a device's from a fresh MiB of each,
    /// so that its port's windows, which are whole MiBs, hold its BARs and
    /// nobody else's; and mapped into the nested table straight, but for the
    /// pages that trap. Refused for a guest made without the PCI hole
    /// (`new_holed`), once its tables are laid out, or where a BAR does not
    /// fit its window.
    pub fn add_host_function(&mut self, mut function: HostFunction, at: HostAddress) -> Result<()> {
        if self.loaded {
            return Err(Error::BadAddress);
        }
        let Some((ref mut free, _)) = self.windows else { return Err(Error::BadAddress) };
        let mut p = self.platform.lock();
        let endpoint = matches!(function.pcie_type(), Some(host::PCIE_ENDPOINT | host::PCIE_LEGACY_ENDPOINT));
        let device = (at.bus, at.device);
        /* The last port where it is of that one's device, past every
         * function behind it; no other port may be its device's */
        let joins = endpoint && p.ports.last().is_some_and(|q| q.device == device);
        if p.ports.iter().rev().skip(usize::from(joins)).any(|q| q.device == device) {
            return Err(Error::BadAddress);
        }
        let number = match p.ports.last() {
            Some(q) if joins => {
                let n = at.function.checked_sub(q.first).ok_or(Error::BadAddress)?;
                let behind = p.ports.len() - 1;
                if p.hosts.iter().any(|h| h.port == Some(behind) && h.number >= n) {
                    return Err(Error::BadAddress);
                }
                n
            }
            _ => 0,
        };
        if !joins {
            /* Another device: from a fresh MiB of each window */
            for next in [&mut free.low, &mut free.high] {
                *next = next.div_ceil(port::WINDOW_ALIGN).checked_mul(port::WINDOW_ALIGN).ok_or(Error::BadAddress)?;
            }
        }
        let bars = *function.bars();
        let mut placed = [None; host::ROM_BAR + 1];
        let (mut low, mut high) = (None, None);
        let widen = |span: &mut Option<(u64, u64)>, start: u64, end: u64| {
            *span = Some(span.map_or((start, end), |(s, e)| (s.min(start), e.max(end))));
        };
        for (i, bar) in bars.iter().enumerate() {
            let Some(b) = bar else { continue };
            if b.size < kcore::consts::PAGE_SIZE as u64 || !b.size.is_power_of_two() {
                return Err(Error::BadAddress);
            }
            /* A 64-bit prefetchable BAR above RAM, where the big ones fit;
             * any other below 4 GiB, where a port's memory window is */
            let above = b.wide && b.prefetchable;
            let (next, end) = if above { (&mut free.high, free.high_end) } else { (&mut free.low, free.low_end) };
            let at = next.div_ceil(b.size).checked_mul(b.size).ok_or(Error::BadAddress)?;
            if at.checked_add(b.size).is_none_or(|e| e > end) || !function.place(i, at) {
                return Err(Error::BadAddress);
            }
            *next = at + b.size;
            placed[i] = Some(at);
            widen(if above { &mut high } else { &mut low }, at, at + b.size);
        }
        /* Its ROM below 4 GiB, as firmware places one, and off: it is mapped
         * only once the guest turns it on (`move_bar`) */
        if let Some(size) = function.rom_size() {
            let at = free.low.div_ceil(size).checked_mul(size).ok_or(Error::BadAddress)?;
            if at.checked_add(size).is_none_or(|e| e > free.low_end) || !function.place_rom(at) {
                return Err(Error::BadAddress);
            }
            free.low = at + size;
            widen(&mut low, at, at + size);
        }
        /* The pages that trap rather than map, by BAR: NVIDIA's mirror,
         * MSI-X's table and PBA (`HostFunction::traps`) */
        let mut holes: [Vec<u64>; 6] = Default::default();
        for t in function.traps() {
            let Some(h) = holes.get_mut(t.bar) else { return Err(Error::BadAddress) };
            h.try_reserve(1).map_err(|_| Error::NoMemory)?;
            h.push(t.offset);
        }
        p.hosts.try_reserve(1).map_err(|_| Error::NoMemory)?;
        /* Its slot first -- its port's, or its own -- then the mappings: a
         * failure past here leaves mapped BARs of a slot that is there,
         * which go with the guest */
        let (port, slot) = if joins {
            let i = p.ports.len() - 1;
            (Some(i), p.ports[i].slot)
        } else if endpoint {
            let bus = u8::try_from(p.ports.len() + 1).map_err(|_| Error::NoMemory)?;
            p.ports.try_reserve(1).map_err(|_| Error::NoMemory)?;
            let slot = usize::from(p.pci.add_host()?);
            p.ports.push(Port { slot, device, first: at.function, port: RootPort::new(bus, bus), low: None, high: None });
            (Some(p.ports.len() - 1), slot)
        } else {
            (None, usize::from(p.pci.add_host()?))
        };
        for (i, at) in placed.iter().enumerate() {
            let (Some(at), Some(b), Some(holes)) = (*at, bars.get(i).copied().flatten(), holes.get(i)) else { continue };
            self.memory.map_device(at, b.host, b.size, holes)?;
        }
        if let Some(q) = port.and_then(|i| p.ports.get_mut(i)) {
            for (span, add) in [(&mut q.low, low), (&mut q.high, high)] {
                if let Some((start, end)) = add {
                    widen(span, start, end);
                }
            }
            q.port.set_windows(q.low, q.high);
        }
        /* More than one function behind its port: each says so */
        if joins {
            function.set_multifunction(true);
            for h in p.hosts.iter_mut().filter(|h| h.port == port) {
                h.function.set_multifunction(true);
            }
        }
        let id = p.hosts.len();
        p.hosts.push(Hosted { port, number, slot, id, function, mapped: placed, holes });
        Ok(())
    }

    /// An MSI of `address` and `data` from a host's function the guest holds,
    /// as the host's handler of its interrupt takes it: delivered as a
    /// device's message is, from outside every CPU -- interrupt context
    /// included, nothing here sleeps or takes the platform's lock. Whether a
    /// CPU took it.
    pub fn deliver_msi(&self, address: u64, data: u32) -> bool {
        self.host_msi.fetch_add(1, Ordering::Relaxed);
        let taken = lapic::msi(address, data).is_some_and(|ipi| self.post_message(None, &ipi, false));
        if !taken {
            self.host_msi_lost.fetch_add(1, Ordering::Relaxed);
        }
        taken
    }

    /// Whether it holds a host's function: a guest whose state cannot be
    /// taken -- the device's DMA reaches no dirty log, and its state is the
    /// device's -- so neither saved nor moved.
    pub fn holds_host_function(&self) -> bool {
        !self.platform.lock().hosts.is_empty()
    }

    /// Give it a NIC with `mac`, its frames carried by `backend`: `eth0`,
    /// `eth1`, a virtio network device on the PCI bus -- with a queue pair
    /// for each of its CPUs, up to `net::MAX_PAIRS`, where it has MSI-X to
    /// give each queue an interrupt of its own.
    pub fn add_nic(&mut self, backend: Box<dyn net::Backend>, mac: [u8; 6]) -> Result<()> {
        self.add_nic_with(backend, mac, 0)
    }

    /// `add_nic`, the NIC telling its driver the MTU `mtu` -- a cluster
    /// network's (`net::Net::set_mtu`) -- or none, 0.
    pub fn add_nic_with(&mut self, backend: Box<dyn net::Backend>, mac: [u8; 6], mtu: u16) -> Result<()> {
        if self.loaded {
            return Err(Error::BadAddress);
        }
        let apic = self.has_apic();
        let mut p = self.platform.lock();
        if p.nics >= MAX_NICS {
            return Err(Error::NoMemory);
        }
        let page = msix_page(&mut self.memory, apic, p.pci_devs.len() + 1)?;
        let pairs = if page.is_some() { u16::try_from(self.cpus()).unwrap_or(net::MAX_PAIRS) } else { 1 };
        let mut nic = Net::new(backend, mac, pairs)?;
        nic.set_rx_moderation(self.rx_moderation_ns);
        nic.set_offloads(self.offloads);
        nic.set_mtu(mtu);
        let io_base = NIC_IO_BASE + (p.nics as u16) * (net::BAR_SIZE as u16);
        let mut f = Function::device(&Net::identity(), io_base, net::BAR_SIZE, NIC_IRQ);
        if let Some(page) = page {
            let vectors = net::vectors(nic.queue_pairs().0);
            nic.offer_msix(vectors).ok_or(Error::NoMemory)?;
            f = f.with_msix(vectors, page);
        }
        p.pci.add(f)?;
        p.pci_devs.push(PciDev::Nic(nic));
        p.msix_pending.push(0);
        p.nics += 1;
        Ok(())
    }

    /// What each disk has done: (sectors, statistics, whether a ring of the
    /// driver's stopped it, whether its rings are copied rather than mapped).
    /// Empty when there is no memory to say it in.
    pub fn disk_stats(&self) -> Vec<(u64, blk::Stats, bool, bool)> {
        let p = self.platform.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(p.disks).is_ok() {
            for d in &p.pci_devs {
                if let PciDev::Disk(d) = d {
                    v.push((d.sectors(), d.stats, d.broken().is_some(), d.rings_copied()));
                }
            }
        }
        v
    }

    /// What each NIC has done: (statistics, its queue pairs and how many
    /// its driver uses, whether a ring stopped it, whether its rings are
    /// copied rather than mapped).
    pub fn nic_stats(&self) -> Vec<(net::Stats, (u16, u16), bool, bool)> {
        let p = self.platform.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(p.nics).is_ok() {
            for d in &p.pci_devs {
                if let PciDev::Nic(n) = d {
                    v.push((n.stats, n.queue_pairs(), n.broken().is_some(), n.rings_copied()));
                }
            }
        }
        v
    }

    /// Write the guest's furniture -- the zero page, the command line, the
    /// memory map, the page tables, the GDT, the MP table and the ACPI
    /// tables -- and put its first CPU, `bsp`, at the kernel's entry. The
    /// kernel and initrd bytes must already be in memory at the addresses
    /// `layout` names; the caller streams those in. Its disks and NICs must
    /// have been added already: the tables describe them, and none is taken
    /// after this.
    pub fn load(&mut self, bsp: &mut GuestCpu, header: &Header, first: &[u8], layout: Layout, cmdline: &[u8])
        -> Result<()>
    {
        if bsp.index != 0 || self.loaded || layout.low_end != self.low_end {
            return Err(Error::BadAddress);
        }
        let mut routes = [acpi::Route::default(); pci::MAX_SLOTS];
        let (n, msix) = self.pci_routes(&mut routes)?;
        let ioapic = self.has_ioapic();
        let firmware = self.write_firmware(&routes[..n], msix, ioapic)?;
        linux::build(&self.memory, header, first, &layout, cmdline, self.cpus(), Some(firmware), &routes[..n], ioapic)?;
        linux::set_entry(bsp.cpu.backend_mut(), &layout);
        self.loaded = true;
        Ok(())
    }

    /// As [`Self::load`], for a PVH image (`crate::pvh`) whose segments, and
    /// initrd, the caller has streamed in: its start info, command line and
    /// memory map, the MP and ACPI tables, and its first CPU at the image's
    /// 32-bit entry. Paging is off there, which under VT-x takes
    /// unrestricted guest: without it, `NoUnrestrictedGuest`.
    pub fn load_pvh(&mut self, bsp: &mut GuestCpu, image: &pvh::Image, layout: pvh::Layout, cmdline: &[u8])
        -> Result<()>
    {
        if bsp.index != 0 || self.loaded || layout.low_end != self.low_end {
            return Err(Error::BadAddress);
        }
        if !bsp.cpu.backend().runs_real_mode() {
            return Err(Error::NoUnrestrictedGuest);
        }
        let mut routes = [acpi::Route::default(); pci::MAX_SLOTS];
        let (n, msix) = self.pci_routes(&mut routes)?;
        let ioapic = self.has_ioapic();
        let firmware = self.write_firmware(&routes[..n], msix, ioapic)?;
        pvh::build(&self.memory, &layout, cmdline, self.cpus(), Some(firmware), &routes[..n], ioapic)?;
        pvh::set_entry(bsp.cpu.backend_mut(), image);
        self.loaded = true;
        Ok(())
    }

    /// Each device on the bus with the IRQ its INTA is wired to, into
    /// `routes` -- a host's function's to one nothing raises (`HOST_IRQ`):
    /// how many, and whether any function has an MSI-X table.
    fn pci_routes(&self, routes: &mut [acpi::Route]) -> Result<(usize, bool)> {
        let p = self.platform.lock();
        let mut n = 0;
        let devices = p.pci_devs.iter().enumerate().map(|(i, d)| {
            (i + 1, match d {
                PciDev::Disk(_) => DISK_IRQ,
                PciDev::Nic(_) => NIC_IRQ,
            })
        });
        /* A port's slot for every function behind it: Linux swizzles the
         * pin of a function below a bridge to the bridge's */
        let hosts = p.ports.iter().map(|q| q.slot).chain(p.hosts.iter().filter(|h| h.port.is_none()).map(|h| h.slot));
        for (slot, irq) in devices.chain(hosts.map(|slot| (slot, HOST_IRQ))) {
            let (Some(route), Ok(slot)) = (routes.get_mut(n), u8::try_from(slot)) else {
                return Err(Error::BadAddress);
            };
            *route = acpi::Route { slot, irq };
            n += 1;
        }
        let msix = (0..p.pci_devs.len()).any(|i| p.pci.msix(i + 1).is_some());
        Ok((n, msix))
    }

    /// The tables a PC's firmware leaves, laid out in the BIOS area: the
    /// ACPI ones, for the machine as it is now -- its CPUs, its IO-APIC when
    /// `ioapic`, and each device on the bus, `routes` -- and the SMBIOS ones,
    /// what it is (`set_identity`).
    fn write_firmware(&self, routes: &[acpi::Route], msix: bool, ioapic: bool) -> Result<linux::Firmware> {
        /* The window covers every slot's page, whichever have one. */
        let window = msix.then_some((MSIX_PAGES, pci::MAX_SLOTS as u32 * pci::MSIX_PAGE));
        /* The host's functions' windows, whole, for a guest made with them:
         * an OS keeps a BAR inside one where firmware put it */
        let (bars32, bars64) = match self.windows {
            /* Either empty where the guest's addresses end short of it -- a
             * CPUID of few address bits -- and said as none. The 64-bit one
             * only as far as the BARs placed in it reach, a GiB at a time:
             * the rest of the guest's addresses are its OS's own -- Linux's
             * HMM takes a GPU's VRAM's worth of them for device memory
             * (NVIDIA's UVM), and with a window to the top of them none was
             * left, and CUDA was refused */
            Some((placed, w)) => (
                u32::try_from(w.low).ok().zip(w.low_end.checked_sub(w.low).and_then(|n| u32::try_from(n).ok())),
                placed.high.checked_sub(w.high).filter(|&n| n != 0).map(|n| {
                    let room = w.high_end.saturating_sub(w.high);
                    (w.high, n.div_ceil(BARS64_ROUNDING).saturating_mul(BARS64_ROUNDING).min(room))
                }),
            ),
            None => (None, None),
        };
        let machine = acpi::Machine {
            cpus: self.cpus(),
            apic: self.has_apic(),
            ioapic: ioapic.then_some(self.cpus() as u8),
            pci: routes,
            mmio: window,
            bars32,
            bars64,
            ecam: self.windows.is_some().then_some(ECAM),
        };

        let mut buf = Vec::new();
        buf.try_reserve_exact(acpi::MAX_BYTES).map_err(|_| Error::NoMemory)?;
        buf.resize(acpi::MAX_BYTES, 0);
        let len = acpi::build(&machine, &mut buf).ok_or(Error::NoMemory)?;
        self.memory.write(acpi::AREA, &buf[..len])?;

        let machine = smbios::Machine {
            uuid: self.identity.uuid,
            serial: &self.identity.serial,
            version: &self.identity.version,
        };
        buf.fill(0);
        let len = smbios::build(&machine, &mut buf[..smbios::MAX_BYTES]).ok_or(Error::NoMemory)?;
        self.memory.write(smbios::AREA, &buf[..len])?;
        Ok(linux::Firmware { rsdp: acpi::AREA, start: acpi::AREA, end: acpi::AREA_END })
    }

    /// How many times its kernel said it panicked with a crash kernel
    /// loaded, which it then booted (`devices::pvpanic`).
    pub fn crash_kernels(&self) -> u32 {
        self.platform.lock().pvpanic.crash_loaded
    }

    /// What the guest's OS did with its ACPI.
    pub fn acpi_stats(&self) -> AcpiStats {
        let p = self.platform.lock();
        AcpiStats { used: p.pm.used, scis: p.pm.scis, presses: p.pm.presses, other_sleep: p.pm.other_sleep }
    }

    pub fn uart_ier(&self) -> u8 {
        self.platform.lock().uart.ier()
    }

    /// Where the guest read an absent device and was answered with all ones.
    pub fn absent_pages(&self) -> Vec<u64> {
        self.memory.absent_pages()
    }

    /// The MSR accesses answered with #GP: (MSR, value, write).
    pub fn msr_faults(&self) -> Vec<(u32, u64, bool)> {
        let p = self.platform.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(p.msr_faults.len()).is_ok() {
            v.extend_from_slice(&p.msr_faults);
        }
        v
    }

    /// The interrupt state at the end, for a diagnostic: the master PIC's
    /// (IRR, ISR, IMR) and the PIT channel 0's (mode, reload, running).
    pub fn irq_debug(&self) -> ((u8, u8, u8), (u8, u16, bool)) {
        let p = self.platform.lock();
        (p.pic.master_state(), p.pit.ch0_state())
    }

    /// The busiest few low ports the guest read, for diagnosing a spin:
    /// (port, count), most first, at most `n`.
    pub fn hot_ports(&self, n: usize) -> Vec<(u16, u32)> {
        let p = self.platform.lock();
        let mut v = Vec::new();
        if v.try_reserve(n + 1).is_err() {
            return v;
        }
        /* The top n kept as they are found: a sort of every port touched
         * would allocate as many as there are. */
        for (port, &count) in p.port_hist.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let at = v.iter().position(|&(_, c)| c < count).unwrap_or(v.len());
            if at < n {
                v.insert(at, (port as u16, count));
                v.truncate(n);
            }
        }
        v
    }

    /// Each CPU's activity and the host CPU it last ran on (`None` before
    /// its first entry), for a report.
    pub fn cpu_states(&self) -> Vec<(Activity, Option<u32>)> {
        let mut v = Vec::new();
        if v.try_reserve_exact(self.cpus.len()).is_ok() {
            for c in &self.cpus {
                let host = c.host_cpu.load(Ordering::Relaxed);
                v.push((Activity::from_code(c.activity.load(Ordering::Relaxed)),
                        (host != u32::MAX).then_some(host)));
            }
        }
        v
    }

    /// How many of its CPUs a start-up IPI has started: all but the first
    /// have been, in a kernel that brought them up.
    pub fn cpus_started(&self) -> u64 {
        self.started.load(Ordering::Relaxed)
    }

    /// How it stopped, once one of its CPUs has stopped it; `None` while it
    /// runs. Taken: asked once, after every CPU's task is done.
    pub fn take_stopped(&self) -> Option<Stopped> {
        self.stopped.lock().take()
    }

    /// Stop the guest from outside it -- `why` being `Requested` or
    /// `Budget` -- as its own CPUs would: every one is rung and leaves its
    /// loop. For whoever runs it, when its CPUs' tasks cannot all be started.
    pub fn stop(&self, why: Stop) {
        {
            let mut stopped = self.stopped.lock();
            if stopped.is_none() {
                *stopped = Some(Stopped { stop: why, cpu: 0, dump: String::new() });
            }
        }
        self.stopping.store(true, Ordering::Release);
        self.doorbells.ring_all();
    }

    /// Pause the guest: every CPU parked at the top of its loop, outside
    /// its guest, its state read there (`park`) -- for the VM's state to be
    /// taken (`capture`), and the guest then ended (`stop(Stop::Migrated)`)
    /// or let go on (`resume`). Waits for every CPU, `PARK_NS` at most: one
    /// that has not parked by then, or the guest stopping meanwhile, and the
    /// pause is given up, the guest going on as it was. Called from a task
    /// of the host's, never a CPU's own.
    pub fn pause(&self) -> core::result::Result<(), &'static str> {
        if self.stopping.load(Ordering::Acquire) {
            return Err("the guest is stopping");
        }
        let n = NEXT_PAUSE.fetch_add(1, Ordering::Relaxed);
        if self.pause.compare_exchange(0, n, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err("the guest is paused already");
        }
        let from = time::boot_time_ns();
        loop {
            /* Rung again each time round: a ring is taken by whatever looks
             * first, and a CPU that took one before it saw the pause waits
             * for the next. */
            self.doorbells.ring_all();
            let parked = self.parked.lock().iter().filter(|p| p.as_ref().is_some_and(|p| p.pause == n)).count();
            if parked == self.cpus.len() {
                return Ok(());
            }
            if self.stopping.load(Ordering::Acquire) {
                self.resume();
                return Err("the guest stopped as it was being paused");
            }
            if time::boot_time_ns().saturating_sub(from) >= PARK_NS {
                self.resume();
                return Err("a CPU of the guest did not park in time");
            }
            kcore::task::sleep_ms(1);
        }
    }

    /// Let a paused guest go on: its disks taking requests again, and every
    /// CPU out of its park as it was -- its own state never changed by the
    /// pause. What the disks answered meanwhile waits in the mailboxes.
    pub fn resume(&self) {
        {
            let mut p = self.platform.lock();
            for d in p.pci_devs.iter_mut() {
                if let PciDev::Disk(b) = d {
                    b.quiesce(false);
                }
            }
        }
        self.pause.store(0, Ordering::Release);
        self.doorbells.ring_all();
    }

    /// Whether it is paused, or being.
    pub fn paused(&self) -> bool {
        self.pause.load(Ordering::Acquire) != 0
    }

    /// CPU `gc`'s write at `gpa` faulted on the nested table: the dirty log's
    /// -- a page of its RAM the log made read-only, marked and made writable
    /// again (`GuestMemory::write_fault`) -- so the write is to run again;
    /// true. False for anything else, and for a write that faults on one
    /// page `DIRTY_REFAULTS` times in a row with no round between, which
    /// re-protects it and is the only way the log makes a CPU fault there
    /// twice: run again, it would fault for good, a VM that hangs with
    /// nothing said, where the MMIO path stops it and says where.
    fn dirty_write(&self, gc: &mut GuestCpu, gpa: u64) -> bool {
        let page = gpa & !(GUEST_PAGE - 1);
        let table = self.memory.nested().id;
        let (last, at, n) = gc.refault;
        let n = if last == page && at == table { n.saturating_add(1) } else { 1 };
        gc.refault = (page, table, n);
        n <= DIRTY_REFAULTS && self.memory.write_fault(gpa)
    }

    /// Every CPU of the guest past the top of its loop -- outside its guest
    /// -- since the nested table took identity `id`, rung until it is: each
    /// entry from then on drops what its TLB kept of the table before, so a
    /// page read after this is what the guest wrote, and its next write to
    /// a page made read-only faults. A migration's dirty log's, after it
    /// narrowed the table (`log_start`, `log_round`).
    fn sync_tlbs(&self, id: u64) -> core::result::Result<(), &'static str> {
        let from = time::boot_time_ns();
        loop {
            self.doorbells.ring_all();
            if self.cpus.iter().all(|c| c.epoch.load(Ordering::Acquire) >= id) {
                return Ok(());
            }
            if self.stopping.load(Ordering::Acquire) {
                return Err("the guest stopped");
            }
            if time::boot_time_ns().saturating_sub(from) >= PARK_NS {
                return Err("a CPU of the guest did not leave it in time");
            }
            kcore::task::sleep_ms(1);
        }
    }

    /// A migration's dirty log begun (`crate::dirty`): every page of the
    /// guest's RAM read-only to it, and every CPU over the narrowed table
    /// before this returns -- a page read from here on is what the guest
    /// wrote, and a write after it a mark. Refused, and nothing left
    /// protected, where its 2 MiB pages could not be split.
    pub fn log_start(&self) -> core::result::Result<(), &'static str> {
        let from = time::boot_time_ns();
        let id = self.memory.log_start().map_err(|_| "no memory to split its 2 MiB pages into 4 KiB ones")?;
        let narrowed = time::boot_time_ns();
        let synced = self.sync_tlbs(id);
        let pages = self.memory.ranges().map(|(_, size)| size / GUEST_PAGE).sum();
        tp::mig_sync(id, kcore::trace::MigSyncWhat::Start, narrowed - from, time::boot_time_ns() - narrowed, pages,
                     self.cpus.len());
        if let Err(e) = synced {
            self.log_stop();
            return Err(e);
        }
        Ok(())
    }

    /// A round of the dirty log, the guest running: each page written since
    /// the round before handed to `out` -- which only notes it: it is read
    /// once this returns -- each read-only again, and every CPU past that.
    /// How many.
    pub fn log_round(&self, out: &mut dyn FnMut(u64)) -> core::result::Result<u64, &'static str> {
        let from = time::boot_time_ns();
        let (n, id) = self.memory.log_take(out).map_err(|_| "no memory to protect a page again")?;
        let narrowed = time::boot_time_ns();
        let synced = self.sync_tlbs(id);
        tp::mig_sync(id, kcore::trace::MigSyncWhat::Round, narrowed - from, time::boot_time_ns() - narrowed, n,
                     self.cpus.len());
        synced.map(|()| n)
    }

    /// The last round, the guest paused and drained (`pause`, `settle`):
    /// each page written since the round before handed to `out`. How many.
    pub fn log_last(&self, out: &mut dyn FnMut(u64)) -> u64 {
        self.memory.log_last(out)
    }

    /// How many pages a round would take now.
    pub fn log_marked(&self) -> u64 {
        self.memory.log_marked()
    }

    /// The dirty log over -- the migration given up, the guest going on
    /// here: every page writable to it again, and its CPUs no longer
    /// slowed.
    pub fn log_stop(&self) {
        let from = time::boot_time_ns();
        self.memory.log_stop();
        self.throttle.store(0, Ordering::Relaxed);
        let pages = self.memory.ranges().map(|(_, size)| size / GUEST_PAGE).sum();
        tp::mig_sync(self.memory.nested().id, kcore::trace::MigSyncWhat::Stop, time::boot_time_ns() - from, 0, pages,
                     self.cpus.len());
    }

    /// Its CPUs kept out of it `percent` of their time, `THROTTLE_MAX` at
    /// most: a migration's, while the guest writes its memory faster than
    /// it is copied (plans/vm-migration.md, D4). 0 for none.
    pub fn throttle(&self, percent: u32) {
        self.throttle.store(percent.min(THROTTLE_MAX), Ordering::Relaxed);
    }

    /// CPU `gc` parked for pause `pause`, on its own task: its state read on
    /// the host CPU it ran on -- where VT-x's VMCS is current -- into its
    /// slot, and then a wait until the pause is over, or the guest ends.
    fn park(&self, gc: &mut GuestCpu, bell: &Doorbell, pause: u64) {
        let at = time::boot_time_ns();
        let state = self.cpu_state(gc, at);
        let old = {
            let mut parked = self.parked.lock();
            match parked.get_mut(gc.index as usize) {
                Some(slot) => slot.replace(Parked { pause, state, at }),
                None => None,
            }
        };
        /* What the slot held, freed off the lock. */
        drop(old);
        while self.pause.load(Ordering::Acquire) == pause && !self.stopping.load(Ordering::Acquire) {
            /* Out of its guest, and over the table as it is now when it
             * enters again: as the top of its loop says it (`sync_tlbs`) */
            if let Some(s) = self.cpus.get(gc.index as usize) {
                s.epoch.store(self.memory.nested().id, Ordering::Release);
            }
            bell.wait(PARKED_WAIT_NS);
        }
    }

    /// CPU `gc`'s state, as of `at`: its registers -- read where they are,
    /// on the host CPU it ran on -- its local APIC, and what its loop keeps
    /// of it. Its mailbox is read after the devices are drained
    /// (`capture`).
    fn cpu_state(&self, gc: &mut GuestCpu, at: u64) -> vmstate::Result<vmstate::Cpu> {
        let regs = gc.cpu.backend_mut().save_regs()?;
        Ok(vmstate::Cpu {
            regs,
            nmi_pending: gc.nmi_pending,
            exception: gc.exception,
            activity: match gc.activity {
                Activity::Running => vmstate::Activity::Running,
                Activity::Halted => vmstate::Activity::Halted,
                Activity::Asleep => vmstate::Activity::Asleep,
                Activity::WaitForSipi => vmstate::Activity::WaitForSipi,
            },
            last_from_pic: gc.last_from_pic,
            pv_eoi: gc.pv_eoi,
            lapic: gc.lapic.save(at),
            mail: vmstate::Mail::default(),
        })
    }

    /// Its disks drained, the guest paused: each takes nothing more off its
    /// ring, and what it has with its backend comes back -- served here as
    /// well as by its task, so that none waits on a parked first CPU's
    /// round -- `DRAIN_NS` at most.
    fn drain(&self) -> vmstate::Result<()> {
        let mut disks = [0u8; MAX_DISKS];
        let mut n = 0;
        {
            let mut p = self.platform.lock();
            for d in p.pci_devs.iter_mut() {
                if let PciDev::Disk(b) = d {
                    b.quiesce(true);
                    if n < MAX_DISKS {
                        disks[n] = b.index();
                        n += 1;
                    }
                }
            }
        }
        let from = time::boot_time_ns();
        loop {
            for &disk in &disks[..n] {
                self.serve_disk(disk);
            }
            let drained = self.platform.lock().pci_devs.iter().all(|d| match d {
                PciDev::Disk(b) => b.drained(),
                PciDev::Nic(_) => true,
            });
            if drained {
                return Ok(());
            }
            if time::boot_time_ns().saturating_sub(from) >= DRAIN_NS {
                return Err(vmstate::Error("a disk did not answer what it was asked in time"));
            }
            kcore::task::sleep_ms(1);
        }
    }

    /// A paused guest's disks drained (`pause`): what it asked of them done
    /// and given back into its memory, nothing more taken until it ends or
    /// goes on -- so that its memory is still from here on, and a page read
    /// of it now is the page a state taken later finds. A migration's, before
    /// it reads the pages: a read the disks answered after its page went
    /// would be missing on the other host, and the used ring that says it
    /// was answered with it.
    pub fn settle(&self) -> vmstate::Result<()> {
        if self.pause.load(Ordering::Acquire) == 0 {
            return Err(vmstate::Error("a guest settled that is not paused"));
        }
        self.drain()
    }

    /// The VM's state, of a guest paused (`pause`): its disks drained first,
    /// then every CPU's as it parked, each one's mailbox and what was posted
    /// for it, and every device's -- all as of one moment, now: the guest's
    /// TSC then, its timers' edges from then. Its CPUs' states are taken out
    /// of their slots, once; its disks stay drained until the guest ends or
    /// goes on (`resume`).
    pub fn capture(&self) -> vmstate::Result<vmstate::VmState> {
        let pause = self.pause.load(Ordering::Acquire);
        if pause == 0 {
            return Err(vmstate::Error("a state taken of a guest not paused"));
        }
        if self.holds_host_function() {
            /* Its device's DMA reaches no dirty log, and what the device is
             * in is the device's own: no state of it to take, nor to move */
            return Err(vmstate::Error("a guest holding a host's device: its state cannot be taken"));
        }
        self.drain()?;
        let ioapic = self.has_ioapic();
        let at = time::boot_time_ns();
        let tsc = time::cycle_counter().wrapping_add(self.tsc_offset.load(Ordering::Relaxed));
        let tsc_hz = Some(self.told_hz.load(Ordering::Relaxed)).filter(|&hz| hz != 0)
            .ok_or(vmstate::Error("this host's TSC rate is not known"))?;

        let mut cpus = Vec::new();
        cpus.try_reserve_exact(self.cpus.len()).map_err(|_| vmstate::Error("no memory for the CPUs' states"))?;
        let mut taken = Vec::new();
        taken.try_reserve_exact(self.cpus.len()).map_err(|_| vmstate::Error("no memory for the CPUs' states"))?;
        {
            let mut parked = self.parked.lock();
            for slot in parked.iter_mut() {
                match slot.take() {
                    Some(p) if p.pause == pause => taken.push(p),
                    other => {
                        *slot = other;
                        return Err(vmstate::Error("a CPU of the guest that did not park"));
                    }
                }
            }
        }
        for (p, shared) in taken.into_iter().zip(&self.cpus) {
            let mut cpu = p.state?;
            /* Its timers, read as it parked, made times from now. */
            let since = at.saturating_sub(p.at);
            let l = &mut cpu.lapic;
            l.timer_due_in_ns = l.timer_due_in_ns.map(|d| d.saturating_sub(since));
            l.timer_loaded_ago_ns = l.timer_loaded_ago_ns.saturating_add(since);
            /* What was posted for it and not taken is what its mailbox
             * holds besides: put there, so that a guest that goes on has it
             * still, and read with the rest. */
            if let Some(words) = shared.posted.as_ref().and_then(|posted| posted.take()) {
                for (w, &word) in words.iter().enumerate() {
                    let mut bits = word;
                    while bits != 0 {
                        let bit = bits.trailing_zeros();
                        bits &= bits - 1;
                        shared.mail.post_fixed((w as u32 * 64 + bit) as u8);
                    }
                }
            }
            cpu.mail = shared.mail.save();
            cpus.push(cpu);
        }

        let p = self.platform.lock();
        let (pci_address, functions) = p.pci.save().ok_or(vmstate::Error("no memory for PCI's state"))?;
        let mut devices = Vec::new();
        devices.try_reserve_exact(p.pci_devs.len()).map_err(|_| vmstate::Error("no memory for the devices' states"))?;
        for d in &p.pci_devs {
            devices.push(match d {
                PciDev::Disk(b) => vmstate::Device::Disk(b.save()?),
                PciDev::Nic(n) => vmstate::Device::Nic(n.save()?),
            });
        }
        let mut msix_pending = Vec::new();
        msix_pending.try_reserve_exact(p.msix_pending.len()).map_err(|_| vmstate::Error("no memory"))?;
        msix_pending.extend_from_slice(&p.msix_pending);
        let platform = vmstate::Platform {
            uart: p.uart.save(), pic: p.pic.save(), pit: p.pit.save(at), rtc: p.rtc.save(), pm: p.pm.save(at),
            pvpanic: p.pvpanic.save(), ioapic: p.ioapic.as_ref().map(|io| io.save()), pci_address, functions,
            devices, msix_pending,
        };
        drop(p);

        let mut ranges = Vec::new();
        ranges.try_reserve_exact(8).map_err(|_| vmstate::Error("no memory"))?;
        for (base, size) in self.memory.ranges() {
            if ranges.len() == vmstate::RANGES_MAX {
                return Err(vmstate::Error("more ranges of RAM than a state holds"));
            }
            ranges.push(vmstate::Range { base, size });
        }
        ranges.sort_unstable_by_key(|r| r.base);
        let (n, smt, llc) = self.shape.parts();
        let vm = vmstate::Vm {
            vendor: match self.vendor {
                hvarch::Vendor::Vmx => vmstate::Vendor::Vmx,
                _ => vmstate::Vendor::Svm,
            },
            uuid: self.identity.uuid, ranges, x2apic: self.x2apic, ioapic, apicv: self.apicv, offloads: self.offloads,
            rx_moderation_ns: self.rx_moderation_ns, shape: vmstate::Shape { cpus: n, smt, llc },
            model: crate::policy::model_digest(self.shape), tsc_hz, tsc,
            started: self.started.load(Ordering::Relaxed),
        };
        Ok(vmstate::VmState { vm, cpus, platform })
    }

    /// This guest -- built as the one `s` was saved from was: as much
    /// memory laid out alike, as many CPUs, x2APIC or not, an IO-APIC or
    /// not, the same devices in the same order, and no kernel loaded --
    /// made the guest `s` says, and `cpus` its CPUs: every device's state
    /// and every CPU's put back, each checked as it is, the guest's TSC going
    /// on from where it stood and its timers from now. Its memory is the
    /// caller's to fill. Refused whole at the first thing that does not hold
    /// -- the guest is no use then, and is dropped.
    pub fn restore(&mut self, s: &vmstate::VmState, cpus: &mut [GuestCpu]) -> vmstate::Result<()> {
        let bad = |what| Err(vmstate::Error(what));
        if self.loaded {
            return bad("a state restored over a guest that has booted");
        }
        let vendor = match s.vm.vendor {
            vmstate::Vendor::Svm => hvarch::Vendor::Svm,
            vmstate::Vendor::Vmx => hvarch::Vendor::Vmx,
        };
        if vendor != self.vendor {
            return bad("a state of a guest of the other extension's");
        }
        if s.cpus.len() != cpus.len() || cpus.len() != self.cpus.len() {
            return bad("a state of a guest of another number of CPUs");
        }
        if s.vm.x2apic != self.x2apic || s.vm.ioapic != self.has_ioapic() {
            return bad("a state of a guest whose APICs were built otherwise");
        }
        let mut ranges = [(0u64, 0u64); vmstate::RANGES_MAX];
        let mut n = 0;
        for r in self.memory.ranges() {
            if n == ranges.len() {
                return bad("more ranges of RAM than a state holds");
            }
            ranges[n] = r;
            n += 1;
        }
        ranges[..n].sort_unstable_by_key(|r| r.0);
        if n != s.vm.ranges.len() || ranges[..n].iter().zip(&s.vm.ranges).any(|(a, b)| a.0 != b.base || a.1 != b.size) {
            return bad("a state of a guest whose memory is laid out otherwise");
        }
        let hz = time::cycle_counter_hz().filter(|&hz| hz != 0).ok_or(vmstate::Error("this host's TSC rate is not known"))?;
        if u128::from(hz.abs_diff(s.vm.tsc_hz)) * 1_000_000 > u128::from(TSC_TOLERANCE_PPM) * u128::from(s.vm.tsc_hz) {
            return bad("a state of a guest whose TSC ran at another rate than this host's");
        }
        if s.vm.model != crate::policy::model_digest(Shape::from_parts(s.vm.shape.cpus, s.vm.shape.smt, s.vm.shape.llc)) {
            return bad("a state of a guest whose CPU this host's is not: its CPUID would change under it");
        }

        self.apicv = s.vm.apicv;
        self.set_offloads(s.vm.offloads);
        self.set_rx_moderation(s.vm.rx_moderation_ns);
        self.set_shape(Shape::from_parts(s.vm.shape.cpus, s.vm.shape.smt, s.vm.shape.llc));
        let now = time::boot_time_ns();
        let tsc_offset = s.vm.tsc.wrapping_sub(time::cycle_counter());
        self.tsc_offset.store(tsc_offset, Ordering::Relaxed);
        self.restored_at.store(now, Ordering::Relaxed);
        self.told_hz.store(s.vm.tsc_hz, Ordering::Relaxed);
        {
            let p = &mut *self.platform.lock();
            let f = &s.platform;
            p.uart.restore(&f.uart)?;
            p.pic.restore(&f.pic)?;
            p.pit.restore(&f.pit, now)?;
            p.rtc.restore(&f.rtc)?;
            p.pm.restore(&f.pm, now)?;
            p.pvpanic.restore(&f.pvpanic);
            match (&mut p.ioapic, &f.ioapic) {
                (Some(io), Some(from)) => io.restore(from)?,
                (None, None) => {}
                _ => return bad("an IO-APIC where the guest built here has none, or none where it has one"),
            }
            p.pci.restore(f.pci_address, &f.functions)?;
            if p.pci_devs.len() != f.devices.len() || p.msix_pending.len() != f.msix_pending.len() {
                return bad("a state of a guest of other devices");
            }
            for (d, from) in p.pci_devs.iter_mut().zip(&f.devices) {
                match (d, from) {
                    (PciDev::Disk(b), vmstate::Device::Disk(from)) => b.restore(from, &self.memory, now)?,
                    (PciDev::Nic(n), vmstate::Device::Nic(from)) => n.restore(from, &self.memory, now)?,
                    _ => return bad("a device of another kind in its slot"),
                }
            }
            p.msix_pending.copy_from_slice(&f.msix_pending);
            sync_msix(p);
        }
        for ((gc, c), shared) in cpus.iter_mut().zip(&s.cpus).zip(&self.cpus) {
            let b = gc.cpu.backend_mut();
            b.restore_regs(&c.regs)?;
            b.set_tsc_offset(tsc_offset);
            gc.lapic.restore(&c.lapic, now)?;
            gc.activity = match c.activity {
                vmstate::Activity::Running => Activity::Running,
                vmstate::Activity::Halted => Activity::Halted,
                vmstate::Activity::Asleep => Activity::Asleep,
                vmstate::Activity::WaitForSipi => Activity::WaitForSipi,
            };
            gc.nmi_pending = c.nmi_pending;
            gc.exception = c.exception;
            gc.last_from_pic = c.last_from_pic;
            gc.pv_eoi = c.pv_eoi;
            gc.pv_eoi_armed = false;
            shared.mail.restore(&c.mail);
            let addressing = gc.lapic.addressing().pack();
            gc.published = (addressing, gc.lapic.software_enabled());
            shared.addressing.store(addressing, Ordering::Relaxed);
            shared.activity.store(gc.activity.code(), Ordering::Relaxed);
            shared.extint.store(gc.lapic.accepts_extint(), Ordering::Relaxed);
        }
        self.started.store(s.vm.started, Ordering::Relaxed);
        self.loaded = true;
        Ok(())
    }

    /// What another host is told of it before it takes it
    /// (`migwire::Offer`): the extension it runs under, its CPU model's
    /// digest, the TSC rate it was told and its shape's parts -- what a
    /// state taken of it will say, and the other host refuses where it
    /// cannot give the same.
    pub fn facts(&self) -> (vmstate::Vendor, [u8; 32], u64, (u32, bool, bool)) {
        let vendor = match self.vendor {
            hvarch::Vendor::Vmx => vmstate::Vendor::Vmx,
            _ => vmstate::Vendor::Svm,
        };
        (vendor, crate::policy::model_digest(self.shape), self.told_hz.load(Ordering::Relaxed), self.shape.parts())
    }

    /// The extension a guest built on `machine` runs under, as a state says
    /// it.
    pub fn vendor_of(machine: &Machine) -> vmstate::Vendor {
        match machine.caps().vendor() {
            hvarch::Vendor::Vmx => vmstate::Vendor::Vmx,
            _ => vmstate::Vendor::Svm,
        }
    }

    /// What its TSC is ahead of the host's (`restore`).
    pub fn tsc_offset(&self) -> u64 {
        self.tsc_offset.load(Ordering::Relaxed)
    }

    /// A guest of a state, its clocks moved on to now, just before its CPUs
    /// -- `cpus` -- first run: its TSC, its local APICs' timers, the PIT, the
    /// PM timer, its NICs' held interrupts all as they stood when the state
    /// was put into it (`restore`), the time since -- its memory filled, a
    /// migration's handover -- left out of the guest's, as the pause was.
    /// Nothing for a guest that booted.
    pub fn rebase(&self, cpus: &mut [GuestCpu]) {
        let since = self.restored_at.swap(0, Ordering::AcqRel);
        if since == 0 {
            return;
        }
        let now = time::boot_time_ns();
        let delta = now.saturating_sub(since);
        let ticks = time::cycle_counter_hz().map_or(0, |hz| {
            (u128::from(delta) * u128::from(hz) / u128::from(kcore::consts::NS_PER_SEC)) as u64
        });
        let offset = self.tsc_offset.load(Ordering::Relaxed).wrapping_sub(ticks);
        self.tsc_offset.store(offset, Ordering::Relaxed);
        {
            let mut p = self.platform.lock();
            p.pit.shift(delta);
            p.pm.shift(delta);
            for d in p.pci_devs.iter_mut() {
                if let PciDev::Nic(n) = d {
                    n.shift(delta);
                }
            }
        }
        for gc in cpus {
            gc.lapic.shift(delta);
            gc.cpu.backend_mut().set_tsc_offset(offset);
        }
    }

    /// Each disk as its guest sees it, in the order it has them: what a
    /// migration's new host makes stand-ins of.
    pub fn disk_shapes(&self) -> Vec<blk::Shape> {
        let p = self.platform.lock();
        let mut out = Vec::new();
        if out.try_reserve_exact(p.disks).is_ok() {
            for d in &p.pci_devs {
                if let PciDev::Disk(b) = d {
                    out.push(b.shape());
                }
            }
        }
        out
    }

    /// Each NIC's MAC and MTU, in the order it has them.
    pub fn nic_shapes(&self) -> Vec<([u8; 6], u16)> {
        let p = self.platform.lock();
        let mut out = Vec::new();
        if out.try_reserve_exact(p.nics).is_ok() {
            for d in &p.pci_devs {
                if let PciDev::Nic(n) = d {
                    out.push(n.shape());
                }
            }
        }
        out
    }

    /// Each disk's volume and writer's generation, in its order, for one
    /// that is a volume of the cluster's (`blk::Backend::writer`).
    pub fn disk_writers(&self, out: &mut dyn FnMut(usize, &str, u64)) {
        let p = self.platform.lock();
        let mut i = 0;
        for d in &p.pci_devs {
            if let PciDev::Disk(b) = d {
                if let Some((name, generation)) = b.writer() {
                    out(i, name, generation);
                }
                i += 1;
            }
        }
    }

    /// Disk `index`'s backend made `backend` -- the volume a handover took,
    /// in place of the stand-in it was built over: the one it replaces
    /// handed back; or `backend` itself, where the guest would not see the
    /// disk alike through it (`Blk::set_backend`), or it has no such disk.
    pub fn set_disk_backend(&self, index: usize, backend: Box<dyn blk::Backend>)
        -> core::result::Result<Box<dyn blk::Backend>, Box<dyn blk::Backend>>
    {
        let mut p = self.platform.lock();
        let disk = p.pci_devs.iter_mut().filter_map(|d| match d {
            PciDev::Disk(b) => Some(b),
            PciDev::Nic(_) => None,
        }).nth(index);
        match disk {
            Some(b) => b.set_backend(backend),
            None => Err(backend),
        }
    }

    /// NIC `index`'s backend made `backend` -- the port a handover took: the
    /// one it replaces handed back; or `backend` itself, where it has no
    /// such NIC.
    pub fn set_nic_backend(&self, index: usize, backend: Box<dyn net::Backend>)
        -> core::result::Result<Box<dyn net::Backend>, Box<dyn net::Backend>>
    {
        let mut p = self.platform.lock();
        let nic = p.pci_devs.iter_mut().filter_map(|d| match d {
            PciDev::Nic(n) => Some(n),
            PciDev::Disk(_) => None,
        }).nth(index);
        match nic {
            Some(n) => Ok(n.set_backend(backend)),
            None => Err(backend),
        }
    }

    /// The guest-physical ranges its devices' queues have their rings in,
    /// each up: what a device writes through a mapping no log of the
    /// guest's writes sees, and what a migration's last round sends
    /// whatever its log says.
    pub fn ring_ranges(&self, out: &mut dyn FnMut(u64, u64)) {
        let p = self.platform.lock();
        for d in &p.pci_devs {
            match d {
                PciDev::Disk(b) => b.rings(out),
                PciDev::Nic(n) => n.rings(out),
            }
        }
    }

    /// Run guest CPU `gc` on the CPU this is called on until the guest
    /// stops -- for good, on `host`'s request, by one of its CPUs, or at
    /// `deadline` (host nanoseconds since boot; `u64::MAX` for none) -- its
    /// console going to and coming from `host`. Every CPU of a guest is run
    /// by a task of its own, all at once; each returns when the guest has
    /// stopped, and says what it counted. How the guest stopped is
    /// [`take_stopped`](Self::take_stopped)'s to say.
    ///
    /// Whoever hands the guest a frame, or has served what its disks asked,
    /// rings the first CPU's doorbell: a vCPU in its guest then leaves it at
    /// once to take the frame or the answer, rather than at the host's next
    /// interrupt.
    pub fn run(&self, gc: &mut GuestCpu, machine: &Machine, deadline: u64, host: &dyn Host) -> Counts {
        /* This task runs the CPU now: its records' pid */
        gc.cpu.note_task();
        let me = gc.index as usize;
        let (Some(bell), Some(shared)) = (self.doorbells.get(me), self.cpus.get(me)) else {
            /* `new` made one of each for every CPU it made. */
            return gc.counts;
        };
        let kick = bell.kick();
        /* Its interrupts the CPU's to deliver where it can -- by default
         * where it takes them posted too -- and posted where the CPU takes
         * them in its guest: the descriptor its CPU names from its first
         * entry. */
        let apicv = self.apicv.unwrap_or(shared.posted.is_some());
        if apicv {
            if let Some(posted) = shared.posted.as_ref() {
                if gc.cpu.backend_mut().set_posted(posted.clone()) {
                    shared.posted_on.store(true, Ordering::Release);
                }
            }
        }

        /* When the last exit came, and whether it was a write of an MSR: the
         * time to the next look is the exit's handling (`handled_ns`). */
        let mut exited: Option<(u64, bool)> = None;
        /* Whether its last entry had the CPU deliver the APIC's interrupts:
         * what an entry made without the round's reload of them needs. */
        let mut entered_apicv = false;
        loop {
            let now = time::boot_time_ns();
            if let Some((at, msr_write)) = exited.take() {
                let handled = now.saturating_sub(at);
                gc.counts.handled_ns += handled;
                if msr_write {
                    gc.counts.msr_write_ns += handled;
                }
            }
            if self.stopping.load(Ordering::Acquire) {
                break;
            }
            if now >= deadline {
                self.stop_with(Stop::Budget, gc);
                break;
            }
            if host.stop_requested() {
                self.stop_with(Stop::Requested, gc);
                break;
            }
            /* The nested table as this CPU enters next: a migration's dirty
             * log narrowed it, and waits for every CPU to be past here before
             * it reads a page (`sync_tlbs`) -- each entry from here on drops
             * what the TLB kept of the table before. */
            shared.epoch.store(self.memory.nested().id, Ordering::Release);
            /* A pause: parked here, outside the guest, until it is over --
             * and then round again, to see whether the guest goes on. */
            let pause = self.pause.load(Ordering::Acquire);
            if pause != 0 {
                self.park(gc, bell, pause);
                continue;
            }
            /* Slowed, for a migration the guest writes faster than it copies:
             * out of its guest for its share of each slice. */
            let slowed = self.throttle.load(Ordering::Relaxed);
            if slowed != 0 && now >= gc.throttle_at {
                gc.throttle_at = now + THROTTLE_SLICE_NS;
                let off = THROTTLE_SLICE_NS * u64::from(slowed) / u64::from(100 - slowed);
                kcore::task::sleep_ms(off / kcore::consts::NS_PER_MS);
                continue;
            }

            /* From here a ring kicks this vCPU out of its guest, or turns its
             * entry back; so what is looked at below -- its mail, the
             * devices -- is what arrived before the ring, and anything later
             * rings it. */
            kick.prepare();
            /* What rang since the last look is what the round looks at. And
             * the round left out, the entry made at once, after a write of
             * its TSC deadline or of an IPI to other CPUs -- KVM's fast path
             * for the same two -- where its last entry had the CPU deliver
             * the APIC's interrupts and nothing waits: no ring, no mail,
             * nothing posted that the CPU has not taken, no edge of its own
             * timer due, nothing to give back. The host's timer is armed for
             * that timer's edge, which the write may have moved; one that
             * came meanwhile is the round's to take. The devices, the mail
             * and the APIC's reload are left for when they have something:
             * each rings it, or is armed for. */
            let rung = bell.take_rung();
            let mut fast = core::mem::take(&mut gc.fast) && entered_apicv && !rung
                && gc.activity == Activity::Running && !shared.mail.has_mail()
                && shared.posted.as_ref().is_none_or(|p| !p.outstanding())
                && gc.exception.is_none() && !gc.nmi_pending
                && gc.lapic.next_timer_ns().is_none_or(|edge| edge > now)
                /* A devices' deadline that has come -- the PIT's edge, a
                 * NIC's held interrupts -- is a reason for the round, not no
                 * deadline: dropped, its host timer had fired already, and
                 * nothing rang for it until the next exit. */
                && gc.other_due.is_none_or(|due| due > now);
            if fast {
                let edge = gc.lapic.next_timer_ns();
                gc.asked = sooner(edge, gc.other_due);
                kick.arm_at(gc.asked);
                fast = edge.is_none_or(|edge| edge > time::boot_time_ns());
            }
            let apicv = if fast {
                gc.counts.fast += 1;
                true
            } else {
            if matches!(gc.activity, Activity::Asleep | Activity::WaitForSipi) {
                /* Awake while it looks, before it takes anything. */
                self.asleep.fetch_and(!(1u64 << me), Ordering::AcqRel);
            }
            self.take_mail(gc);
            self.take_posted(gc, shared);
            let due = gc.lapic.next_timer_ns();
            if gc.lapic.timer(now) {
                gc.counts.timer += 1;
                /* How late its interrupt is requested, past the edge it was
                 * due at: what a guest's timer latency is made of here, the
                 * host's wakeup of a halted vCPU above all. */
                let late = now.saturating_sub(due.unwrap_or(now));
                gc.counts.timer_late_ns += late;
                gc.counts.timer_late_max = gc.counts.timer_late_max.max(late);
                let how = match gc.step {
                    Step::Slept { edge, .. } if edge != due => TimerHow::NotSleptFor,
                    Step::Slept { wake_at, woke, .. } if woke > wake_at + LATE_NS => TimerHow::WokeLate,
                    Step::Slept { .. } => TimerHow::WokeOnTime,
                    Step::Spun => TimerHow::Spun,
                    Step::Ran { at, .. } if now.saturating_sub(at) > LATE_NS => TimerHow::OffCpu,
                    Step::Ran { host: true, .. } => TimerHow::Host,
                    Step::Ran { host: false, .. } => TimerHow::Exit,
                    Step::Other => TimerHow::Other,
                };
                tp::timer(gc.index, gc.cpu.pid(), how, late);
                if late > LATE_NS {
                    let c = &mut gc.counts;
                    match how {
                        TimerHow::WokeLate => {
                            c.late_woke_late += 1;
                            c.late_woke_late_rung += u64::from(matches!(gc.step, Step::Slept { rung: true, .. }));
                        }
                        TimerHow::WokeOnTime => c.late_woke_on_time += 1,
                        TimerHow::NotSleptFor => c.late_not_slept_for += 1,
                        TimerHow::Spun => c.late_spun += 1,
                        TimerHow::OffCpu => c.late_off_cpu += 1,
                        TimerHow::Host => c.late_host += 1,
                        TimerHow::Exit => c.late_exit += 1,
                        TimerHow::Nothing | TimerHow::Other => c.late_other += 1,
                    }
                }
                if late > gc.counts.worst.late {
                    let (entered, asked, left) = match gc.step {
                        Step::Ran { at, entered, asked, .. } => (entered, asked, at),
                        _ => (0, 0, 0),
                    };
                    gc.counts.worst = Worst { late, how: how as u8, due: due.unwrap_or(0), entered, asked, left };
                }
            }
            shared.extint.store(gc.lapic.accepts_extint(), Ordering::Relaxed);

            /* The devices' work, on the first CPU's loop, whatever that CPU
             * is doing: its console fed, the PIT's edges and the serial
             * port's interrupt raised on the 8259, and what the NICs and
             * disks have for the guest given to it -- raising their lines.
             * The 8259 then has an interrupt for a CPU that takes them; and
             * an IO-APIC, what its pins send, delivered from here. */
            let mut extint = false;
            let mut ring_takers = false;
            let mut pit_edge = None;
            let mut devices_due = None;
            if me == 0 {
                let mut p = self.platform.lock();
                feed_console(&mut p, host);
                /* The power button, when it has been pressed: its status
                 * latched, and whoever pressed it told whether the guest
                 * will hear of it. Then the SCI's line as the PM registers
                 * have it -- raised again if the guest ended its interrupt
                 * with an event still set, as a level is, and raised when
                 * the timer's carry is due. */
                if host.power_button() {
                    let heard = p.pm.press_power_button();
                    host.power_button_heard(heard);
                }
                sync_sci(&mut p, now);
                /* The timer: a channel-0 period elapsed is an IRQ0 edge --
                 * one owed edge at a time, and only once the last has been
                 * taken: an IRQ0 still requested or in service would swallow
                 * the next, and a tick the guest never saw is time it never
                 * counts (`Pit::ch0_fire`). */
                let mut pins = 0;
                if !timer_busy(&p, &gc.lapic) && p.pit.ch0_fire() {
                    p.pic.raise(0);
                    if let Some(io) = p.ioapic.as_mut() {
                        pins |= io.edge(ioapic::TIMER_PIN);
                    }
                    gc.counts.edges0 += 1;
                }
                /* COM1's interrupt, for as long as it asserts it: which is
                 * how the serial driver sends past the first byte, an
                 * interrupt at a time. */
                if p.uart.irq_line() {
                    p.pic.raise(COM1_IRQ);
                }
                self.poll_devices(&mut p, me as u32, &mut gc.lapic, &mut gc.counts, now);
                /* When the devices' round has to come again on its own: a
                 * receive interrupt held, and frames that woke nobody while
                 * it is (`Net::rx_due`); a pass that ended at its bound, at
                 * once (`more`) -- slept to, or armed before the guest is
                 * entered, below. */
                devices_due = p.pci_devs.iter().filter_map(|d| match d {
                    PciDev::Nic(n) if n.more() => Some(now),
                    PciDev::Nic(n) => n.rx_due(now),
                    PciDev::Disk(d) => d.more().then_some(now),
                }).min();
                pins |= sync_ioapic(&mut p, now);
                self.ioapic_send(&mut p, pins, me as u32, &mut gc.lapic, &mut gc.counts);
                let pic_pending = p.pic.pending().is_some();
                extint = gc.lapic.accepts_extint() && pic_pending;
                /* The 8259's interrupt, when this CPU is not the one whose
                 * LINT0 takes it: the CPUs that are rung for it -- the
                 * round's own raise (the PIT's tick, a device's line) once
                 * rang nobody, and a crash kernel on the second CPU took its
                 * timer a halt's wait late. */
                ring_takers = pic_pending && !gc.lapic.accepts_extint();
                /* Not while IRQ0 is still requested or in service -- masked,
                 * say: none is handed over until the guest takes that one,
                 * and an owed edge, already due, would have the vCPU wake
                 * without sleeping for good. */
                pit_edge = if timer_busy(&p, &gc.lapic) { None } else { p.pit.next_ch0_edge_ns() };
            } else if gc.lapic.accepts_extint() {
                extint = self.platform.lock().pic.pending().is_some();
            }
            /* Off the platform's lock */
            if ring_takers {
                self.ring_extint(me);
            }

            match gc.activity {
                Activity::Asleep | Activity::WaitForSipi => {
                    /* Nothing but another CPU wakes it. Not entering: a ring
                     * from here on wakes the task out of its wait instead. */
                    kick.cancel();
                    match self.sleep_for_good(me, shared) {
                        Sleep::Woken => continue,
                        Sleep::Last => {
                            /* The last CPU of the guest asleep, and none left
                             * to wake any: stopped for good. */
                            self.stop_with(Stop::Halted { rip: self.last_halt.load(Ordering::Relaxed) }, gc);
                            break;
                        }
                        Sleep::Wait => {
                            /* The first CPU's task does the devices' work
                             * whatever its CPU does: asleep, it wakes for
                             * their next edge too, as a halted one does --
                             * a crash kernel's dump through a NIC held a
                             * second a frame once, the boot CPU parked with
                             * interrupts off in its NMI handler. */
                            let until = pit_edge.unwrap_or(u64::MAX)
                                .min(devices_due.unwrap_or(u64::MAX))
                                .min(now.saturating_add(ASLEEP_WAIT_NS))
                                .min(deadline);
                            bell.wait(until.saturating_sub(now).max(1));
                            gc.step = Step::Spun;
                            continue;
                        }
                    }
                }
                Activity::Halted => {
                    if !self.wakes(gc, extint) {
                        kick.cancel();
                        /* Nothing for it yet. Sleep until the next timer
                         * edge -- the PIT's, for the first CPU, and its own
                         * APIC's -- or a ring, and give the CPU to whatever
                         * else can use it, rather than enter a guest that
                         * would only halt again. */
                        let until = pit_edge
                            .unwrap_or(u64::MAX)
                            .min(devices_due.unwrap_or(u64::MAX))
                            .min(gc.lapic.next_timer_ns().unwrap_or(u64::MAX))
                            .min(now.saturating_add(MAX_HALT_WAIT_NS))
                            .min(deadline);
                        if until > now {
                            /* A ring that comes within the spin is caught in
                             * it; one that does not, the task sleeps for, to
                             * the edge -- never spun to: an edge the spin ran
                             * into would have the loop look again with no
                             * sleep at all, and one that wakes nothing look
                             * for good. The spin is grown after a halt that
                             * was short, and dropped after one that was long,
                             * KVM's way. */
                            let spin = gc.poll_ns != 0 && until - now > gc.poll_ns;
                            if trace::on::<HvHalt>() {
                                let of = tp::HaltFor {
                                    timer: gc.lapic.next_timer_ns(), pit: pit_edge, devices: devices_due, deadline,
                                };
                                tp::halt(gc.index, gc.cpu.pid(), now, until, &of, if spin { gc.poll_ns } else { 0 });
                            }
                            if spin {
                                gc.counts.polls += 1;
                                if bell.poll(gc.poll_ns) {
                                    gc.counts.polled += 1;
                                    gc.step = Step::Spun;
                                    if trace::on::<HvWake>() {
                                        let halted = time::boot_time_ns().saturating_sub(now);
                                        tp::wake(gc.index, gc.cpu.pid(), WakeHow::Polled, halted, 0);
                                    }
                                    continue;
                                }
                            }
                            /* Its own timer the next to wake it, an edge that
                             * raises an interrupt (`next_timer_ns`): slept to
                             * a little short of it and the rest spun, so that
                             * what a wake from a sleep costs the host -- its
                             * timer's interrupt, a schedule, a switch, 9-10 us
                             * on the EX44 -- comes before the edge rather than
                             * after it, and the guest's interrupt on time.
                             * KVM's timer advance, fixed. */
                            let edge = gc.lapic.next_timer_ns();
                            let advance = edge == Some(until) && until - now > TIMER_ADVANCE_NS;
                            let wake_at = if advance { until - TIMER_ADVANCE_NS } else { until };
                            let slept_from = time::boot_time_ns();
                            let rung = bell.wait(wake_at.saturating_sub(slept_from).max(1));
                            let woke = time::boot_time_ns();
                            gc.counts.sleeps += 1;
                            gc.counts.slept_ns += woke.saturating_sub(slept_from);
                            let wake_at = wake_at.max(slept_from + 1);
                            let over = woke.saturating_sub(wake_at);
                            if over > LATE_NS {
                                gc.counts.overslept += 1;
                                gc.counts.overslept_rung += u64::from(rung);
                            }
                            gc.counts.overslept_max = gc.counts.overslept_max.max(over);
                            gc.step = Step::Slept { wake_at, edge, woke, rung };
                            let mut how = if rung { WakeHow::Rung } else { WakeHow::Timed };
                            if advance && !rung {
                                let at = time::boot_time_ns();
                                if at < until {
                                    gc.counts.advanced += 1;
                                    bell.poll(until - at);
                                    how = WakeHow::Advanced;
                                }
                            }
                            let halt = time::boot_time_ns().saturating_sub(now);
                            tp::wake(gc.index, gc.cpu.pid(), how, halt, over);
                            gc.poll_ns = if halt > POLL_MAX_NS {
                                0
                            } else if halt > gc.poll_ns {
                                gc.poll_ns.saturating_mul(2).clamp(POLL_FIRST_NS, POLL_MAX_NS)
                            } else {
                                gc.poll_ns
                            };
                        }
                        continue;
                    }
                    self.set_activity(gc, Activity::Running);
                }
                Activity::Running => {}
            }
            /* Its timers' next edge, if one is coming: the host CPU's own
             * timer armed for it as the entry is made (`Kick::arm_at`), so
             * that a guest computing with no exit is taken out of it then, as
             * a halted one is woken then -- not at the host's next tick, ten
             * edges of a 1000 Hz guest late. An edge of its own timers' that
             * came since they were looked at above is one the host's timer is
             * not armed for -- it arms none gone by -- and the guest would run
             * on to its next exit with it owed: round again to take it, before
             * anything is handed in for this entry; the entry itself turns
             * back for one that comes after this. */
            let timers = pit_edge.unwrap_or(u64::MAX).min(gc.lapic.next_timer_ns().unwrap_or(u64::MAX));
            let edge = timers.min(devices_due.unwrap_or(u64::MAX));
            gc.asked = (edge != u64::MAX).then_some(edge);
            kick.arm_at(gc.asked);
            gc.other_due = sooner(pit_edge, devices_due);
            if timers <= time::boot_time_ns() {
                kick.cancel();
                gc.counts.timer_raced += 1;
                gc.step = Step::Other;
                continue;
            }

            /* The CPU delivers the APIC's interrupts itself where it can,
             * and the guest's APIC is in x2APIC mode and on: handed where
             * the APIC stands, and given back what it did at the exit, before
             * anything here looks at the APIC again. */
            let apicv = apicv && self.has_apic() && gc.cpu.backend().has_vapic() && gc.lapic.delivers_itself();
            self.deliver(gc, extint, apicv);
            if apicv {
                let vectors = gc.lapic.vectors();
                gc.cpu.backend_mut().load_apic(&vectors);
                gc.counts.vid += 1;
            } else {
                gc.cpu.backend_mut().apicv_off();
                self.pv_eoi_arm(gc);
            }
            apicv
            };

            gc.counts.to_entry_ns += time::boot_time_ns().saturating_sub(now);
            entered_apicv = apicv;
            /* The kick's count of entries turned back for an edge is the
             * VM's, across its boots; this run's are told from it entry by
             * entry. */
            let edges_passed = kick.edges_passed();
            let entered_at = time::boot_time_ns();
            let answered_before = gc.counts.answered + gc.counts.hlt_answered;
            let entered = {
                /* Its writes of the ICR and of the TSC deadline answered in
                 * the world switch's own loop where they can be (`FastMsr`):
                 * the entry after each made then and there. */
                let pid = gc.cpu.pid();
                let GuestCpu { index, cpu, lapic, counts, other_due, asked, poll_ns, .. } = &mut *gc;
                let mut fast = FastMsr {
                    guest: self, me: *index, pid, lapic, counts, kick, other_due: *other_due, asked, poll_ns: *poll_ns,
                };
                cpu.enter(&self.memory, machine, Some(kick), Some(&mut fast))
            };
            let exit_at = time::boot_time_ns();
            gc.counts.exits += gc.counts.answered + gc.counts.hlt_answered - answered_before;
            if apicv {
                let (irr, isr, tpr) = gc.cpu.backend_mut().take_apic();
                gc.lapic.from_cpu(&irr, &isr, tpr);
            }
            /* What the guest did with its PV EOI word, first: an EOI made
             * through it comes before whatever the exit is -- an EOI written
             * to the register after it, which ends the vector beneath. */
            self.pv_eoi_sync(gc);
            let (exit, host_cpu) = match entered {
                Ok(entered) => entered,
                Err(refusal) => {
                    self.stop_with(Stop::Refused(refusal), gc);
                    break;
                }
            };
            shared.host_cpu.store(host_cpu, Ordering::Relaxed);
            gc.step = Step::Ran { at: exit_at, host: matches!(exit, Exit::Host), entered: entered_at,
                                  asked: gc.asked.unwrap_or(0) };
            exited = Some((exit_at, matches!(exit, Exit::Msr { write: true })));
            gc.counts.exits += 1;
            if gc.counts.exits % PROGRESS_EVERY == 0 {
                host.progress(gc.index, &gc.counts);
            }
            let rip = gc.cpu.backend().save().rip;

            let stop = match exit {
                Exit::Host => {
                    gc.counts.host += 1;
                    None
                }
                /* Not entered: round again, and what came goes in. */
                Exit::Kicked => {
                    gc.counts.kicked += 1;
                    gc.counts.edges_passed += u64::from(kick.edges_passed() != edges_passed);
                    None
                }
                Exit::Io(io) => self.io(gc, &io, host),
                Exit::Cpuid => {
                    gc.counts.cpuid += 1;
                    let apic = self.has_apic();
                    let v = gc.cpu.backend_mut();
                    let sub = v.regs().rcx as u32;
                    let leaf = v.save().rax as u32;
                    let asker = crate::policy::Asker {
                        apic_id: gc.lapic.id(), apic, apic_on: gc.lapic.enabled(), cr4: v.cr4(), xcr0: v.xcr0(),
                        shape: self.shape,
                    };
                    let answer = crate::policy::cpuid(leaf, sub, &asker);
                    let (save, regs) = v.save_and_regs_mut();
                    crate::policy::apply_cpuid(save, regs, &answer);
                    v.skip_cpuid();
                    None
                }
                Exit::Msr { write } => self.msr(gc, write),
                Exit::Eoi { vector } => {
                    /* An EOI the CPU performed, of a vector whose end the
                     * IO-APIC has to hear: out of service already, by the
                     * page the exit gave back. */
                    gc.counts.eoi_exits += 1;
                    if let Some(level) = gc.lapic.virtual_eoi(vector) {
                        self.level_eoi(gc, level);
                    }
                    None
                }
                Exit::Hlt => {
                    /* Step past it either way: a CPU an interrupt wakes
                     * from HLT resumes at the instruction after it -- Linux's
                     * `sti; hlt; cli` returns to the `cli` and on to its idle
                     * loop's need_resched check -- and one an NMI wakes
                     * returns there from its handler. Entered again at the
                     * HLT, the interrupt's handler would return to the HLT,
                     * and a kernel that does not preempt on the way out of
                     * an interrupt would never leave its idle task. Stepping
                     * also takes the vCPU out of the STI's interrupt shadow,
                     * which covers the HLT and would hold the wake-up off. */
                    let with_interrupts = gc.cpu.backend().save().rflags & RFLAGS_IF != 0;
                    gc.cpu.backend_mut().skip_hlt();
                    if with_interrupts {
                        /* A booted kernel idles on HLT, waking on the timer:
                         * with interrupts on it is waiting for the next one,
                         * not dead. */
                        gc.counts.hlt += 1;
                        self.set_activity(gc, Activity::Halted);
                        host.progress(gc.index, &gc.counts);
                    } else {
                        /* Asleep until another CPU wakes it -- and if none is
                         * awake to, the guest has stopped for good. */
                        self.last_halt.store(rip, Ordering::Relaxed);
                        self.set_activity(gc, Activity::Asleep);
                    }
                    None
                }
                /* A write to a page of its RAM a migration's dirty log made
                 * read-only -- a permission's fault, no reserved bit's:
                 * marked and writable again, and the write runs again
                 * (`crate::dirty`, `dirty_write`). */
                Exit::NestedFault { gpa, error }
                    if error & (vmcb::npf::PRESENT | vmcb::npf::WRITE | vmcb::npf::RESERVED)
                        == vmcb::npf::PRESENT | vmcb::npf::WRITE
                        && self.dirty_write(gc, gpa) =>
                {
                    None
                }
                Exit::NestedFault { gpa, error } => {
                    /* The local APIC's page, in xAPIC mode, and the
                     * IO-APIC's, when there is one: the access performed for
                     * the guest. Else a read of the platform's MMIO window
                     * that nothing answers is a probe for a device that is
                     * not there: map all ones and let the instruction run
                     * again. Anything else -- a write, a fetch, a walk of the
                     * guest's own tables, an address outside the window, the
                     * APIC's page in x2APIC mode -- stops it. */
                    use vmcb::npf;
                    let data = error & (npf::PRESENT | npf::FETCH) == 0 && error & npf::FINAL != 0;
                    let plain_read = data && error & npf::WRITE == 0;
                    let apic_page = gpa & !(lapic::XAPIC_PAGE_SIZE - 1) == lapic::DEFAULT_BASE;
                    /* An access the CPU made itself, delivering an event --
                     * the pushes of an interrupt or exception onto a stack
                     * the guest put on a device's page -- is no instruction
                     * at RIP, which emulating would decode and perform in its
                     * place. Stopped, said for what it is, as KVM gives up on
                     * such a VM: the event was given back (`requeue_event`)
                     * and is queued still. */
                    let delivering = gc.cpu.backend().event_queued();
                    if data && delivering && ((apic_page && self.has_apic() && gc.lapic.xapic())
                        || (IoApic::owns(gpa) && self.has_ioapic()) || self.memory.is_device(gpa))
                    {
                        gc.counts.mmio += 1;
                        Some(Stop::MmioDelivering { gpa, rip })
                    } else if data && apic_page && self.has_apic() && gc.lapic.xapic() {
                        self.xapic_access(gc, gpa, error & npf::WRITE != 0)
                    } else if data && IoApic::owns(gpa) && self.has_ioapic() {
                        self.ioapic_access(gc, gpa, error & npf::WRITE != 0)
                    } else if data && !delivering && self.memory.is_device(gpa) {
                        /* A page of a host's function's BAR left out of the
                         * table: NVIDIA's mirror of its configuration space */
                        self.host_mmio(gc, gpa, error & npf::WRITE != 0)
                    } else if plain_read && self.memory.map_absent(gpa).is_ok() {
                        None
                    } else {
                        gc.counts.mmio += 1;
                        Some(Stop::Mmio { gpa, rip })
                    }
                }
                Exit::Shutdown => Some(Stop::Shutdown { rip }),
                /* #AC and #DB are intercepted only so that a guest cannot
                 * have the CPU deliver one inside another for good
                 * (CVE-2015-5307 and 8104): the guest's own -- a user
                 * process's single step, its unaligned access with AC set --
                 * goes back to it, not a stop of its whole VM. */
                Exit::Exception { vector, error } if vector == VECTOR_AC || vector == VECTOR_DB => {
                    gc.counts.reflected += 1;
                    gc.exception = Some((vector, error));
                    None
                }
                Exit::Exception { vector, .. } => Some(Stop::Exception { vector, rip }),
                /* `int1` -- AMD-V's intercept of it, or VT-x's of the #DB
                 * it raises, which exits at the instruction: stepped past,
                 * and its #DB given as the trap it is, after it. */
                Exit::Instr(Instr::Icebp) => {
                    gc.counts.reflected += 1;
                    gc.cpu.backend_mut().skip_icebp();
                    gc.exception = Some((VECTOR_DB, None));
                    None
                }
                Exit::Undefined => {
                    gc.counts.ud += 1;
                    gc.cpu.backend_mut().inject_ud();
                    None
                }
                /* RDPMC, of counters CPUID does not say are there: the #GP
                 * a CPU gives for a counter it has not got. */
                Exit::Instr(Instr::Rdpmc) => {
                    gc.counts.ud += 1;
                    gc.cpu.backend_mut().inject_gp();
                    None
                }
                Exit::MachineCheck => Some(Stop::Exception { vector: 18, rip }),
                Exit::Invalid => Some(Stop::Invalid),
                /* The guest can take an interrupt, or an NMI, now; the next
                 * entry injects it. Nothing to do here. */
                Exit::IrqWindow | Exit::NmiWindow => None,
                Exit::Instr(Instr::Xsetbv) => {
                    gc.counts.xsetbv += 1;
                    if cr::xsetbv(gc.cpu.backend_mut()) != cr::Xcr::Written {
                        gc.counts.xsetbv_refused += 1;
                    }
                    None
                }
                Exit::Instr(Instr::Monitor | Instr::Mwait | Instr::Rdtscp | Instr::Rdpru | Instr::Rsm) => {
                    /* Intercepted, and not offered by CPUID -- or, for RSM,
                     * outside SMM, where every guest is: the guest gets what
                     * a CPU without them gives it, at any CPL. */
                    gc.counts.ud += 1;
                    gc.cpu.backend_mut().inject_ud();
                    None
                }
                /* WBINVD, and INVD -- which would throw away the host's
                 * dirty lines with the guest's: stepped past, KVM's answer
                 * to both. Counted together. */
                Exit::Instr(Instr::Wbinvd | Instr::Invd) => {
                    gc.counts.wbinvd += 1;
                    gc.cpu.backend_mut().skip_wbinvd();
                    None
                }
                Exit::Cr8 { write, gpr: Some(gpr) } => {
                    gc.counts.cr8 += 1;
                    let v = gc.cpu.backend_mut();
                    v.cr8_access(write, gpr);
                    /* CR8 is the APIC's task priority: a write of it is
                     * one of the TPR too. */
                    gc.lapic.sync_cr8(v.cr8());
                    None
                }
                /* AMD-V's write, stopped at while an interrupt the task
                 * priority alone held waited: decoded and performed, and
                 * the round looks again at what it lets in. */
                Exit::Cr8 { write: true, gpr: None } => {
                    gc.counts.cr8 += 1;
                    let mut bytes = mmio::Bytes::default();
                    match cr::cr8_write(gc.cpu.backend_mut(), &self.memory, &mut bytes) {
                        Ok(()) => {
                            gc.lapic.sync_cr8(gc.cpu.backend().cr8());
                            None
                        }
                        Err(error) => Some(Stop::CrInsn { rip, error, bytes }),
                    }
                }
                Exit::Cr0Write { value } => {
                    gc.counts.cr0 += 1;
                    gc.cpu.backend_mut().cr0_write(value);
                    None
                }
                Exit::Cr4 { write, gpr } => {
                    gc.counts.cr4 += 1;
                    let mut bytes = mmio::Bytes::default();
                    match cr::cr4_access(gc.cpu.backend_mut(), &self.memory, write, self.cr4_allowed, gpr, &mut bytes) {
                        Ok(cr::Done::Refused) => {
                            gc.counts.cr4_gp += 1;
                            None
                        }
                        Ok(cr::Done::Read | cr::Done::Written) => None,
                        Err(error) => Some(Stop::CrInsn { rip, error, bytes }),
                    }
                }
                Exit::Dr { write, dr: n, gpr } => {
                    gc.counts.dr += 1;
                    let mut bytes = mmio::Bytes::default();
                    match dr::dr_access(gc.cpu.backend_mut(), &self.memory, write, n, gpr, &mut bytes) {
                        Ok(dr::Done::Refused | dr::Done::Undefined | dr::Done::Detected) => {
                            gc.counts.dr_refused += 1;
                            None
                        }
                        Ok(dr::Done::Read | dr::Done::Written) => None,
                        Err(error) => Some(Stop::DrInsn { rip, error, bytes }),
                    }
                }
                /* Spin-waiting: stepped past, and round the loop, where a
                 * tick it may be waiting for is handed over. */
                Exit::Pause => {
                    gc.counts.pause += 1;
                    gc.cpu.backend_mut().skip_pause();
                    None
                }
                Exit::Hypercall => {
                    /* KVM's hypercalls: none is offered, and each is answered
                     * -KVM_ENOSYS in RAX, as KVM answers one it does not
                     * know -- a driver that probes for one, as ptp_kvm does
                     * for its clock pairing, reads that as not there. */
                    let v = gc.cpu.backend_mut();
                    v.save_mut().rax = KVM_ENOSYS;
                    v.skip_vmmcall();
                    None
                }
                other => Some(Stop::Unexpected { exit: other, rip }),
            };
            if let Some(stop) = stop {
                self.stop_with(stop, gc);
                break;
            }
        }
        /* However it stopped -- a refusal may come after `prepare` -- the
         * vCPU is out, and a ring kicks it no longer. */
        kick.cancel();
        self.fill_dump(gc);
        host.progress(gc.index, &gc.counts);
        gc.counts
    }

    /// Stop the guest, `gc` being the CPU that stops it: how it stopped is
    /// the first CPU's to say that does, with its state for the report, and
    /// every CPU is rung to see it.
    fn stop_with(&self, stop: Stop, gc: &mut GuestCpu) {
        {
            let mut stopped = self.stopped.lock();
            if stopped.is_none() {
                let mut dump = String::new();
                if gc.activity != Activity::WaitForSipi && dump.try_reserve(DUMP_BYTES).is_ok() {
                    gc.cpu.backend_mut().read_whole_state();
                    let _ = gc.dump(&mut dump);
                }
                *stopped = Some(Stopped { stop, cpu: gc.index, dump });
            }
        }
        self.stopping.store(true, Ordering::Release);
        self.doorbells.ring_all();
    }

    /// Give the stop `gc`'s state if it was recorded with none: by a CPU
    /// waiting for a start-up IPI -- never started, or reset -- whose
    /// registers are nothing the guest did, or from outside (`stop`). Called
    /// by every CPU as it leaves its loop, so the first out that has state
    /// to show shows it.
    fn fill_dump(&self, gc: &mut GuestCpu) {
        if gc.activity == Activity::WaitForSipi {
            return;
        }
        let mut stopped = self.stopped.lock();
        if let Some(s) = stopped.as_mut() {
            if s.dump.is_empty() && s.dump.try_reserve(DUMP_BYTES).is_ok() {
                gc.cpu.backend_mut().read_whole_state();
                let _ = gc.dump(&mut s.dump);
                s.cpu = gc.index;
            }
        }
    }

    fn set_activity(&self, gc: &mut GuestCpu, activity: Activity) {
        gc.activity = activity;
        if let Some(s) = self.cpus.get(gc.index as usize) {
            s.activity.store(activity.code(), Ordering::Relaxed);
        }
    }

    /// CPU `me`, halted with interrupts off or waiting for a start-up IPI,
    /// having taken its mail and found nothing in it to wake it, goes to
    /// sleep: its bit set first, then its mailbox looked at again, for mail
    /// that came after it took what there was.
    ///
    /// The bit is what another CPU sees of it, and the order is what makes
    /// that true. A CPU that finds its own bit makes the set every CPU's
    /// still has to find every mailbox empty, and then the set still whole
    /// -- and it does only if nobody is awake to send anything. Whoever
    /// posted mail was awake, its bit clear, until after it posted, so a
    /// set with its bit in it was read after the post (the set's
    /// read-modify-writes carry that order) and the mail is found; and a
    /// CPU that has taken its mail since cleared its bit first, so the
    /// second look at the set finds it gone.
    fn sleep_for_good(&self, me: usize, shared: &CpuShared) -> Sleep {
        let bit = 1u64 << me;
        let before = self.asleep.fetch_or(bit, Ordering::AcqRel);
        if shared.mail.has_mail() {
            self.asleep.fetch_and(!bit, Ordering::AcqRel);
            return Sleep::Woken;
        }
        let last = before & bit == 0 && before | bit == self.all;
        if last
            && self.cpus.iter().all(|c| !c.mail.has_mail())
            && self.asleep.load(Ordering::Acquire) == self.all
        {
            return Sleep::Last;
        }
        Sleep::Wait
    }

    /// Take what other CPUs sent this one, in the order a CPU would act on
    /// it: an INIT resets it to wait for a start-up IPI, a start-up IPI
    /// starts one that waits, and fixed interrupts go into its APIC's
    /// request register.
    /// What was posted for `gc` and its CPU has not taken in the guest:
    /// into its APIC, edge-triggered, before it next looks at the APIC.
    fn take_posted(&self, gc: &mut GuestCpu, shared: &CpuShared) {
        let Some(posted) = shared.posted.as_ref() else { return };
        if let Some(words) = posted.take() {
            let taken: u32 = words.iter().map(|w| w.count_ones()).sum();
            gc.counts.ipi_taken += u64::from(taken);
            gc.lapic.accept_all(&words, &[0; 4]);
        }
    }

    /// A fixed, edge-triggered interrupt of `vector` for CPU `i` -- not the
    /// sender's own -- whose shared state is `target`: posted, where it
    /// takes them so, and the notification sent to the CPU it is in its
    /// guest on, which takes it there without an exit; into its mailbox
    /// otherwise. What is not notified rings it.
    fn post_edge(&self, i: usize, target: &CpuShared, vector: u8) {
        let bell = self.doorbells.get(i);
        if let (true, Some(posted)) = (target.posted_on.load(Ordering::Acquire), target.posted.as_ref()) {
            self.posted.fetch_add(1, Ordering::Relaxed);
            /* With a notification outstanding it is taken with that one. */
            if !posted.post(vector) {
                return;
            }
            if let Some(d) = bell {
                if d.kick().notify() {
                    self.notified.fetch_add(1, Ordering::Relaxed);
                    /* After the notification, the one hand-off without a
                     * record of its own: the CPU takes it in the guest, and
                     * nothing of the target's is recorded for it to come
                     * before. A ring, below, records itself first. */
                    tp::ring(u32::try_from(i).unwrap_or(u32::MAX), RingHow::Notified);
                } else {
                    d.ring();
                }
            }
            return;
        }
        target.mail.post_fixed(vector);
        if let Some(d) = bell {
            d.ring();
        }
    }

    fn take_mail(&self, gc: &mut GuestCpu) {
        let Some(shared) = self.cpus.get(gc.index as usize) else { return };
        let mail = shared.mail.take();
        if mail.is_empty() {
            return;
        }
        if mail.init {
            /* The boot CPU's INIT stops the guest where it is sent
             * (`send`), and never comes this far. */
            gc.counts.init += 1;
            gc.lapic.init();
            /* MSR_KVM_PV_EOI_EN back to off, as KVM's reset has it. */
            gc.pv_eoi = 0;
            gc.pv_eoi_armed = false;
            self.publish(gc);
            gc.cpu.backend_mut().init_reset();
            /* What the CPU had to deliver goes with the reset -- an NMI
             * latched before it too, which INIT discards as KVM's reset
             * does: kept, the reset's clearing of the NMI mask let it in at
             * the CPU's first instruction after its start-up IPI, through a
             * real-mode vector table that is whatever low memory holds. */
            gc.exception = None;
            gc.nmi_pending = false;
            gc.last_from_pic = false;
            gc.fast = false;
            self.set_activity(gc, Activity::WaitForSipi);
        }
        if let Some(vector) = mail.sipi {
            /* Ignored by a CPU not waiting for one, as the silicon ignores
             * it: Linux sends two, and the second finds the CPU started. */
            if gc.activity == Activity::WaitForSipi {
                gc.cpu.backend_mut().start_at_sipi(vector);
                gc.counts.started += 1;
                self.started.fetch_add(1, Ordering::Relaxed);
                self.set_activity(gc, Activity::Running);
            }
        }
        if mail.nmi {
            if gc.activity == Activity::WaitForSipi {
                gc.counts.nmi_lost += 1;
            } else {
                /* Latched, and taken when the CPU can: an NMI wakes a CPU
                 * halted with interrupts off, which returns to the
                 * instruction after its HLT when its handler is done. */
                gc.nmi_pending = true;
                if gc.activity == Activity::Asleep {
                    self.set_activity(gc, Activity::Running);
                }
            }
        }
        let taken: u32 = mail.fixed.iter().map(|w| w.count_ones()).sum();
        if taken != 0 {
            gc.counts.ipi_taken += u64::from(taken);
            let refused = gc.lapic.accept_all(&mail.fixed, &mail.level);
            if refused != [0; 4] {
                self.levels_refused(&refused);
            }
        }
    }

    /// Level-triggered interrupts the IO-APIC posted to this CPU, `vectors`
    /// a bit each, that its APIC refused -- software-disabled, say, or reset
    /// by an INIT taken with them: the IO-APIC told, which waits for no EOI
    /// of them and sends them again once a CPU's APIC changes so that one
    /// may be taken (`publish`). Told nothing, the pins were dead for good.
    fn levels_refused(&self, vectors: &[u64; 4]) {
        let mut p = self.platform.lock();
        let Some(io) = p.ioapic.as_mut() else { return };
        for (w, &bits) in vectors.iter().enumerate() {
            let mut left = bits;
            while left != 0 {
                let bit = left.trailing_zeros();
                left &= left - 1;
                io.refused((w as u32 * u64::BITS + bit) as u8);
            }
        }
    }

    /// Whether a halted vCPU has something to be entered for: an interrupt
    /// the 8259 would deliver now to a CPU that takes them, one its APIC
    /// would let through, or an event already queued for injection. It
    /// halted with interrupts on and has not run since, so any is one it
    /// can take.
    fn wakes(&self, gc: &mut GuestCpu, extint: bool) -> bool {
        let cr8 = gc.cpu.backend().cr8();
        gc.lapic.sync_cr8(cr8);
        extint || gc.nmi_pending || gc.lapic.pending().is_some() || gc.cpu.backend().event_queued()
    }

    /// Give the guest the event it is to take next, if it can take one: an
    /// NMI first, whatever its flags; then an interrupt -- the 8259's,
    /// through LINT0, which the APIC's priorities do not hold back, or the
    /// highest its APIC lets through. Otherwise ask the CPU to exit when it
    /// can.
    ///
    /// When the 8259 and the APIC both have one, they take turns. The two
    /// timers behind them -- the PIT's and the APIC's -- are owed their
    /// periods and hand them over as the guest takes them, which a guest
    /// that runs between the host's ticks takes several at a time: with the
    /// 8259 always first, the PIT's would all go in before the APIC's, and a
    /// kernel that checks the one against the other while it waits for a
    /// tick -- Linux, which disables an APIC timer whose count strays two
    /// jiffies from its PIT's -- would see them apart when they are not.
    ///
    /// With `apicv` the APIC's interrupts are the CPU's to deliver (VT-x's
    /// virtual-interrupt delivery): only an exception, an NMI and the
    /// 8259's go in from here.
    fn deliver(&self, gc: &mut GuestCpu, extint: bool, apicv: bool) {
        /* An exception given back first: it is the guest's last instruction's.
         * Not over an event an exit cut short and has put back, which goes
         * in first -- the two one after the other, as the CPU delivers a
         * benign exception and the event it interrupted -- and nothing else
         * this entry. */
        if let Some((vector, error)) = gc.exception {
            let v = gc.cpu.backend_mut();
            if !v.event_queued() {
                v.reflect_exception(vector, error);
                gc.exception = None;
                tp::inject(gc.index, gc.cpu.pid(), InjectKind::Exception, vector);
            }
            return;
        }
        if gc.nmi_pending {
            let v = gc.cpu.backend_mut();
            if v.nmi_allowed() {
                v.inject_nmi();
                gc.nmi_pending = false;
                gc.counts.nmi += 1;
                tp::inject(gc.index, gc.cpu.pid(), InjectKind::Nmi, VECTOR_NMI);
                return;
            }
            /* In its handler for the last one, or in a shadow: told when it
             * can -- and meanwhile an interrupt may go in. */
            v.request_nmi_window();
        }
        let cr8 = gc.cpu.backend().cr8();
        gc.lapic.sync_cr8(cr8);
        let apic = if apicv { None } else { gc.lapic.pending() };
        if !extint && apic.is_none() {
            let v = gc.cpu.backend_mut();
            v.clear_irq_window();
            /* One held by the task priority alone goes in when the guest
             * lowers it -- which a `mov` to CR8 under AMD-V's V_TPR or
             * VT-x's TPR shadow does without an exit: stopped there, rather
             * than at its next exit of another kind, a host tick on. */
            if let Some(class) = (!apicv).then(|| gc.lapic.held_by_tpr()).flatten() {
                v.request_tpr_window(class);
            }
            return;
        }
        let v = gc.cpu.backend_mut();
        if !v.interruptible() {
            gc.counts.blocked += 1;
            v.request_irq_window();
            tp::inject(gc.index, gc.cpu.pid(), InjectKind::Window, apic.unwrap_or(0));
            return;
        }
        if extint && (apic.is_none() || !gc.last_from_pic) {
            /* Looked at again under the lock: another CPU may have masked
             * it meanwhile, and the 8259 acknowledged here is the one the
             * vector injected is. */
            let mut p = self.platform.lock();
            if let Some((irq, vector)) = p.pic.pending() {
                v.inject_extint(vector);
                p.pic.acknowledge(irq);
                drop(p);
                v.clear_irq_window();
                gc.last_from_pic = true;
                gc.counts.irq += 1;
                tp::inject(gc.index, gc.cpu.pid(), InjectKind::Pic, vector);
                if irq == 0 {
                    gc.counts.irq0 += 1;
                }
                if irq == 4 {
                    gc.counts.irq4 += 1;
                }
                return;
            }
        }
        match apic {
            Some(vector) => {
                v.inject_extint(vector);
                gc.lapic.acknowledge(vector);
                v.clear_irq_window();
                gc.last_from_pic = false;
                gc.counts.apic += 1;
                tp::inject(gc.index, gc.cpu.pid(), InjectKind::Apic, vector);
            }
            None => v.clear_irq_window(),
        }
    }

    /// Send `ipi`, which CPU `from` wrote to its interrupt command register:
    /// into the mailbox of each CPU it is for, whose doorbell then rings --
    /// a CPU asleep wakes, and takes it. One for the sender itself goes
    /// straight into its own APIC.
    fn send(&self, from: &mut GuestCpu, ipi: Ipi) -> bool {
        from.counts.ipi_sent += 1;
        let me = from.index;
        let mut own = false;
        for (i, target) in self.cpus.iter().enumerate() {
            let id = i as u32;
            let addressing = if id == me { from.lapic.addressing() } else { target.addressing() };
            if !ipi.reaches(id, addressing, me) {
                continue;
            }
            match ipi.delivery {
                Delivery::Fixed | Delivery::LowestPriority if id == me => {
                    from.lapic.accept(ipi.vector);
                    own = true;
                }
                Delivery::Fixed | Delivery::LowestPriority => {
                    /* Posted or mailed, and its CPU notified or rung. */
                    self.post_edge(i, target, ipi.vector);
                    if ipi.delivery == Delivery::LowestPriority {
                        return own;
                    }
                    continue;
                }
                Delivery::Nmi => target.mail.post_nmi(),
                Delivery::Init if id == 0 => {
                    /* INIT to the boot CPU is a PC's reset: it would start
                     * again at the reset vector, in firmware there is none
                     * of. */
                    let rip = from.cpu.backend().save().rip;
                    self.stop_with(Stop::Init { rip }, from);
                    return true;
                }
                Delivery::Init => target.mail.post_init(),
                Delivery::Startup => target.mail.post_sipi(ipi.vector),
                /* No SMM here; and the INIT de-assert resets nothing. */
                Delivery::Smi | Delivery::InitDeassert => continue,
            }
            if id != me {
                if let Some(d) = self.doorbells.get(i) {
                    d.ring();
                }
            }
            /* Lowest priority is one CPU's, of those it names: the first. */
            if ipi.delivery == Delivery::LowestPriority {
                return own;
            }
        }
        own
    }

    /// Ring the CPUs that take the 8259's interrupts, after an access or a
    /// round of CPU `me`'s left one pending there: the first CPU, whose loop
    /// does the devices' work, and any other whose APIC takes them -- `me`
    /// not, which looks at it again itself.
    fn ring_extint(&self, me: usize) {
        for (i, c) in self.cpus.iter().enumerate() {
            if i != me && (i == 0 || c.extint.load(Ordering::Relaxed)) {
                if let Some(d) = self.doorbells.get(i) {
                    d.ring();
                }
            }
        }
    }

    /// What the disks' backends have served, given back to the guest, and
    /// what the NICs' backends have for it, into its buffers: each kind's
    /// line raised when that calls for an interrupt.
    fn poll_devices(&self, p: &mut Platform, me: u32, lapic: &mut Lapic, counts: &mut Counts, now: u64) {
        let mut raised = [Raise::default(); MAX_DISKS + MAX_NICS];
        for (d, r) in p.pci_devs.iter_mut().zip(raised.iter_mut()) {
            *r = match d {
                PciDev::Disk(d) => d.poll(&self.memory),
                PciDev::Nic(n) => n.poll(&self.memory, now),
            };
        }
        for (dev, r) in raised.iter().enumerate() {
            if r.any() {
                self.raise(p, me, lapic, counts, dev, *r);
            }
        }
        /* And what a mask held back, now its mask may be off. */
        for dev in 0..p.msix_pending.len() {
            let held = p.msix_pending[dev];
            if held != 0 {
                let still = self.msix(p, dev, held, &mut |ipi| {
                    counts.msi += 1;
                    self.post_msi(me, lapic, ipi);
                });
                if still != held {
                    p.msix_pending[dev] = still;
                    self.write_pba(p, dev);
                }
            }
        }
    }

    /// What device `dev` has for the guest: its line up on the 8259, and
    /// each MSI-X entry sent.
    fn raise(&self, p: &mut Platform, me: u32, lapic: &mut Lapic, counts: &mut Counts, dev: usize, r: Raise) {
        if r.line {
            let line = match p.pci_devs.get(dev) {
                Some(PciDev::Disk(_)) => DISK_IRQ,
                Some(PciDev::Nic(_)) => NIC_IRQ,
                None => return,
            };
            p.pic.raise(line);
        }
        if r.vectors != 0 {
            let held = self.msix(p, dev, r.vectors, &mut |ipi| {
                counts.msi += 1;
                self.post_msi(me, lapic, ipi);
            });
            self.hold_msix(p, dev, held);
        }
    }

    /// Device `dev`'s MSI-X entries in `held`, masked as they were sent:
    /// kept pending, for `poll_devices` to send once their mask is off.
    fn hold_msix(&self, p: &mut Platform, dev: usize, held: u32) {
        if let Some(pending) = p.msix_pending.get_mut(dev) {
            if *pending | held != *pending {
                *pending |= held;
                self.write_pba(p, dev);
            }
        }
    }

    /// What disk `disk` -- the guest's vda 0 and on -- has for the guest,
    /// given to it from outside every CPU: requests its backend has served
    /// put on the used ring, what waited on the ring taken, and its MSI-X
    /// vectors posted to the CPUs they name, each through its mailbox --
    /// in its guest by its posted-interrupt notification, without an exit.
    /// What the disk's own task calls as it has served something, as QEMU's
    /// iothread with irqfd does: the first CPU, whose round the devices'
    /// work is, need not be woken -- nor taken out of its guest, where the
    /// guest's job may be running -- to hand a completion over. True when
    /// that round has to come anyway: a line raised, which is the 8259's
    /// and the first CPU's, or a pass that ended at its bound.
    pub fn serve_disk(&self, disk: u8) -> bool {
        let mut p = self.platform.lock();
        let Some(dev) = p.pci_devs.iter().position(|d| matches!(d, PciDev::Disk(b) if b.index() == disk)) else {
            return true;
        };
        let (r, more) = match p.pci_devs.get_mut(dev) {
            Some(PciDev::Disk(d)) => (d.poll(&self.memory), d.more()),
            _ => return true,
        };
        /* Its line raised here, where the poll that raised it ran -- the
         * first CPU's round, rung for it, finds the 8259's request and
         * nothing new of the disk's */
        if r.line {
            p.pic.raise(DISK_IRQ);
        }
        if r.vectors != 0 {
            let held = self.msix(&p, dev, r.vectors, &mut |ipi| {
                self.served_msi.fetch_add(1, Ordering::Relaxed);
                self.post_message(None, ipi, false);
            });
            self.hold_msix(&mut p, dev, held);
        }
        r.line || more
    }

    /// Device `dev`'s pending bits, as the guest reads them: a word of them
    /// in its table page, after the table.
    fn write_pba(&self, p: &Platform, dev: usize) {
        let (Some((_, page)), Some(&pending)) = (p.pci.msix(dev + 1), p.msix_pending.get(dev)) else { return };
        let pba = u64::from(page) + u64::from(pci::MSIX_PBA_OFFSET);
        let _ = self.memory.write(pba, &u64::from(pending).to_le_bytes());
    }

    /// Device `dev`'s MSI-X entries in `entries`, a bit each: each sent as
    /// the message its table entry holds -- read from the table page, which
    /// is the guest's to write -- unless it or the whole function is masked.
    /// Those are held, and returned, for the caller to keep pending and send
    /// from `poll_devices` once unmasked. One the table has not got, or a
    /// message that is no interrupt, goes nowhere.
    fn msix(&self, p: &Platform, dev: usize, entries: u32, send: &mut dyn FnMut(&Ipi)) -> u32 {
        let Some((control, page)) = p.pci.msix(dev + 1) else { return 0 };
        if control & pci::MSIX_ENABLE == 0 {
            return 0;
        }
        let size = u32::from(control & pci::MSIX_SIZE_MASK) + 1;
        let mut held = 0u32;
        let mut left = entries;
        while left != 0 {
            let entry = left.trailing_zeros();
            left &= left - 1;
            if entry >= size {
                continue;
            }
            let at = u64::from(page) + u64::from(entry * pci::MSIX_ENTRY);
            let mut e = [0u8; pci::MSIX_ENTRY as usize];
            if self.memory.read(at, &mut e).is_err() {
                continue;
            }
            let word = |i: usize| u32::from_le_bytes([e[i], e[i + 1], e[i + 2], e[i + 3]]);
            if control & pci::MSIX_MASK_ALL != 0 || word(12) & pci::MSIX_ENTRY_MASKED != 0 {
                held |= 1 << entry;
                continue;
            }
            let address = u64::from(word(0)) | (u64::from(word(4)) << 32);
            if let Some(ipi) = lapic::msi(address, word(8)) {
                send(&ipi);
            }
        }
        held
    }

    /// A device's MSI, as `ipi` decodes it: its vector into the request
    /// register of each CPU it names -- the first of them only, for lowest
    /// priority -- straight into `lapic` for the CPU `me` doing the device's
    /// work, and through the mailbox and a ring for any other.
    fn post_msi(&self, me: u32, lapic: &mut Lapic, ipi: &Ipi) {
        self.post_message(Some((me, lapic)), ipi, false);
    }

    /// A device's message on the APIC bus -- an MSI, an IO-APIC pin's -- to
    /// the CPUs it names, as `post_msi` delivers one; `level`, a fixed
    /// interrupt the local APIC takes as level-triggered, its EOI coming
    /// back to the IO-APIC. An NMI goes into the mailbox of each CPU named,
    /// the sender's own included, for its loop to take -- rung, so that one
    /// already on its way into its guest turns back for it. Whether a local
    /// APIC took it: the sender's own says so as it takes it, and one posted
    /// to another CPU counts as taken -- the sender cannot ask, and that
    /// CPU tells the IO-APIC of a level it refuses (`levels_refused`) --
    /// while one that reaches no CPU is taken by none. A device has no other
    /// delivery mode here (`lapic::msi`, the IO-APIC's `send`).
    fn post_message(&self, mut by: Option<(u32, &mut Lapic)>, ipi: &Ipi, level: bool) -> bool {
        /* From outside every CPU -- a disk's task (`serve_disk`) -- no CPU
         * is the sender's own: each one named takes it through its mailbox */
        let me = by.as_ref().map_or(u32::MAX, |(m, _)| *m);
        let mut taken = false;
        for (i, target) in self.cpus.iter().enumerate() {
            let id = i as u32;
            let addressing = match by.as_ref() {
                Some((m, lapic)) if *m == id => lapic.addressing(),
                _ => target.addressing(),
            };
            if !ipi.reaches(id, addressing, me) {
                continue;
            }
            let own = by.as_mut().filter(|(m, _)| *m == id).map(|(_, lapic)| lapic);
            let posted = match (ipi.delivery, own) {
                (Delivery::Fixed | Delivery::LowestPriority, Some(lapic)) => {
                    taken |= if level { lapic.accept_level(ipi.vector) } else { lapic.accept(ipi.vector) };
                    false
                }
                (Delivery::Fixed | Delivery::LowestPriority, None) if !level => {
                    /* Posted or mailed, and its CPU notified or rung. */
                    self.post_edge(i, target, ipi.vector);
                    taken = true;
                    false
                }
                (Delivery::Fixed | Delivery::LowestPriority, None) => {
                    target.mail.post_level(ipi.vector);
                    taken = true;
                    true
                }
                (Delivery::Nmi, _) => {
                    target.mail.post_nmi();
                    taken = true;
                    true
                }
                _ => return taken,
            };
            if posted {
                if let Some(d) = self.doorbells.get(i) {
                    d.ring();
                }
            }
            if ipi.delivery == Delivery::LowestPriority {
                return taken;
            }
        }
        taken
    }

    /// The interrupts the IO-APIC has to send for `pins`, delivered as its
    /// messages say, by CPU `me`, whose local APIC is `lapic` -- the IO-APIC
    /// told of each whether a local APIC took it, which is what its remote
    /// IRR waits on (`IoApic::send`).
    fn ioapic_send(&self, p: &mut Platform, pins: Pins, me: u32, lapic: &mut Lapic, counts: &mut Counts) {
        let Some(io) = p.ioapic.as_mut() else { return };
        let mut left = pins;
        while left != 0 {
            let pin = left.trailing_zeros() as usize;
            left &= left - 1;
            io.send(pin, &mut |m| {
                counts.ioapic += 1;
                self.post_message(Some((me, &mut *lapic)), &m.ipi, m.level)
            });
        }
    }

    /// The IO-APIC's level-triggered interrupts no local APIC took, sent
    /// again by CPU `gc`, whose APIC has just changed so that it may take
    /// one (`publish`): the chip tries such a message again on its own.
    fn send_untaken(&self, gc: &mut GuestCpu) {
        let mut p = self.platform.lock();
        let Some(pins) = p.ioapic.as_ref().map(|io| io.untaken()) else { return };
        if pins != 0 {
            self.ioapic_send(&mut p, pins, gc.index, &mut gc.lapic, &mut gc.counts);
        }
    }

    /// `gc`'s local APIC ended a level-triggered interrupt of `vector`: its
    /// EOI broadcast to the IO-APIC, which ends the interrupt that pin sent
    /// -- and sends it again if its line is still up.
    fn level_eoi(&self, gc: &mut GuestCpu, vector: u8) {
        let mut p = self.platform.lock();
        let Some(io) = p.ioapic.as_mut() else { return };
        let pins = io.eoi(vector);
        self.ioapic_send(&mut p, pins, gc.index, &mut gc.lapic, &mut gc.counts);
    }

    /// Perform the guest's access of the IO-APIC's page -- a `write` or a
    /// read of `gpa` -- by the instruction that faulted: its select, window
    /// or EOI register read or written, a load's value in its register, the
    /// instruction stepped past; and what a write sends -- an unmasked level
    /// that is up, an EOI's interrupt again -- delivered. An instruction
    /// that cannot be performed stops the guest, showing its bytes.
    fn ioapic_access(&self, gc: &mut GuestCpu, gpa: u64, write: bool) -> Option<Stop> {
        let mut bytes = mmio::Bytes::default();
        let op = match mmio::begin(gc.cpu.backend(), &self.memory, gpa, write, &mut bytes) {
            Ok(op) => op,
            Err(error) => {
                let rip = gc.cpu.backend().save().rip;
                return Some(Stop::MmioInsn { gpa, rip, error, bytes });
            }
        };
        gc.counts.mmio_done += 1;
        let offset = (gpa & (ioapic::PAGE_SIZE - 1)) as u32;
        let mut p = self.platform.lock();
        let read = match p.ioapic.as_mut() {
            Some(io) if op.is_write() => {
                let pins = io.mmio_write(offset, op.size(), op.value as u32);
                self.ioapic_send(&mut p, pins, gc.index, &mut gc.lapic, &mut gc.counts);
                0
            }
            Some(io) => u64::from(io.mmio_read(offset, op.size())),
            /* The caller found one: never. */
            None => 0,
        };
        drop(p);
        mmio::finish(gc.cpu.backend_mut(), &op, read);
        None
    }

    /// Answer a port access and step past it -- or, for a write that resets
    /// the machine, leave the guest where it is and say which it was.
    fn io(&self, gc: &mut GuestCpu, io: &crate::svm::Io, host: &dyn Host) -> Option<Stop> {
        let me = gc.index;
        let v = gc.cpu.backend_mut();
        if io.string {
            /* No string I/O is emulated: it was stepped past with nothing
             * done -- RCX not counted down, RSI and RDI where they were,
             * an INS's buffer not filled -- a guest left to read bytes it
             * never got as if it had. Stopped instead, naming the port:
             * no kernel this is for drives a device by INS or OUTS (Linux's
             * 8250 and virtio drivers do not), and one that does is told. */
            return Some(Stop::StringIo { port: io.port, input: io.input, rip: v.save().rip });
        }
        if !io.input && io.size == 1 {
            let value = v.save().rax as u8;
            let reset = match io.port {
                I8042_COMMAND => value & I8042_PULSE == I8042_PULSE && value & I8042_RESET_LINE == 0,
                RESET_CONTROL => value & RESET_CONTROL_RST_CPU != 0,
                _ => false,
            };
            if reset {
                gc.counts.port_out += 1;
                return Some(Stop::Reset { port: io.port, value, rip: v.save().rip });
            }
        }

        /* Whether the first CPU, whose loop does the devices' rounds, has to
         * look at them again for what this access did. */
        let mut first = false;
        let mut p = self.platform.lock();
        if io.input && !Uart::owns(COM1, io.port) && (io.port as usize) < p.port_hist.len() {
            let slot = &mut p.port_hist[io.port as usize];
            *slot = slot.saturating_add(1);
        }
        if Uart::owns(COM1, io.port) && io.size == 1 {
            let offset = io.port - COM1;
            if io.input {
                let byte = p.uart.read(offset);
                let s = v.save_mut();
                s.rax = (s.rax & !0xFF) | byte as u64;
                gc.counts.port_in += 1;
            } else {
                let byte = v.save().rax as u8;
                if let Some(out) = p.uart.write(offset, byte) {
                    host.output(out);
                }
                gc.counts.port_out += 1;
            }
        } else if (Pit::owns(io.port) || Rtc::owns(io.port) || Pic::owns(io.port)) && io.size == 1 {
            let byte = v.save().rax as u8;
            if io.input {
                let value = if Pit::owns(io.port) {
                    p.pit.read(io.port)
                } else if Rtc::owns(io.port) {
                    p.rtc.read(io.port)
                } else {
                    p.pic.read(io.port)
                };
                let s = v.save_mut();
                s.rax = (s.rax & !0xFF) | value as u64;
                gc.counts.port_in += 1;
            } else {
                if Pit::owns(io.port) {
                    p.pit.write(io.port, byte);
                } else if Rtc::owns(io.port) {
                    p.rtc.write(io.port, byte);
                } else {
                    p.pic.write(io.port, byte);
                }
                gc.counts.port_out += 1;
            }
        } else if PvPanic::owns(io.port) && io.size == 1 {
            if io.input {
                let value = p.pvpanic.read();
                set_in(v.save_mut(), io.size, u32::from(value));
                gc.counts.port_in += 1;
            } else {
                gc.counts.port_out += 1;
                if p.pvpanic.write(v.save().rax as u8) == Some(pvpanic::Event::Panicked) {
                    return Some(Stop::Panicked { rip: v.save().rip });
                }
            }
        } else if Pm::owns(io.port) {
            let now = time::boot_time_ns();
            if io.input {
                let value = p.pm.read(io.port, io.size, now);
                set_in(v.save_mut(), io.size, value);
                gc.counts.port_in += 1;
            } else {
                gc.counts.port_out += 1;
                let value = v.save().rax as u32;
                if p.pm.write(io.port, io.size, value, now) {
                    /* Off: the guest stays where it wrote it, as a machine
                     * that has lost its power does. */
                    return Some(Stop::PowerOff { rip: v.save().rip });
                }
                sync_sci(&mut p, now);
            }
        } else if let Some(t) = p.pci.host_target(io.port) {
            /* A host's slot, or a bus below a port: the port's configuration
             * space, or the function's own as it lays the guest's view over
             * the device's */
            let size = usize::from(io.size);
            if io.input {
                let value = p.host_config_read(&t, size);
                set_in(v.save_mut(), io.size, value);
                gc.counts.port_in += 1;
            } else {
                let value = v.save().rax as u32;
                self.host_config_write(&mut p, &t, size, value);
                gc.counts.port_out += 1;
            }
        } else if PciBus::owns(io.port) {
            if io.input {
                let value = p.pci.read(io.port, io.size);
                set_in(v.save_mut(), io.size, value);
                gc.counts.port_in += 1;
            } else {
                let value = v.save().rax as u32;
                p.pci.write(io.port, io.size, value);
                sync_msix(&mut p);
                gc.counts.port_out += 1;
            }
        } else if let Some((slot, offset)) = p.pci.io_target(io.port) {
            /* A device's registers: slot n is pci_devs[n - 1]. */
            let index = usize::from(slot).checked_sub(1);
            let dev = index.and_then(|i| p.pci_devs.get_mut(i));
            if io.input {
                let value = match dev {
                    Some(PciDev::Disk(d)) => d.io_read(offset, io.size),
                    Some(PciDev::Nic(n)) => n.io_read(offset, io.size),
                    None => u32::MAX,
                };
                set_in(v.save_mut(), io.size, value);
                gc.counts.port_in += 1;
            } else {
                let value = v.save().rax as u32;
                let raised = match dev {
                    Some(PciDev::Disk(d)) => {
                        let raised = d.io_write(offset, io.size, value, &self.memory, time::boot_time_ns());
                        /* A pass that ended at its bound on another CPU's
                         * notify is the first CPU's round to go on with,
                         * which may be asleep or in its guest: rung, it
                         * looks. */
                        first |= me != 0 && d.more();
                        raised
                    }
                    Some(PciDev::Nic(n)) => {
                        let window = n.rx_window();
                        let raised = n.io_write(offset, io.size, value, &self.memory, time::boot_time_ns());
                        /* A receive interrupt raised from another CPU's
                         * notify -- a driver refilling its ring where its
                         * interrupt is taken -- holds the next one back, and
                         * frames that come meanwhile wake nobody: the first
                         * CPU's loop comes back at the end for them. It may
                         * be asleep, or in its guest, with an older end in
                         * mind, and would sleep to its cap with the frames
                         * waiting: rung, it looks again. Pings between two
                         * guests on the EX44 had a 10 ms answer every few
                         * hundred without this, and none in 2,400 with it. */
                        first |= me != 0 && (n.rx_window() != window || n.more());
                        raised
                    }
                    None => Raise::default(),
                };
                if let Some(dev) = index.filter(|_| raised.any()) {
                    self.raise(&mut p, me, &mut gc.lapic, &mut gc.counts, dev, raised);
                }
                gc.counts.port_out += 1;
            }
        } else if io.input {
            /* A port nothing here answers: the bus floats to all ones,
             * which is what a read of an absent device gives. */
            set_in(v.save_mut(), io.size, u32::MAX);
            gc.counts.port_in += 1;
        } else {
            gc.counts.port_out += 1;
        }
        /* What the access did to the IO-APIC's pins -- the serial port's,
         * the SCI's, a device's line -- sent from here. */
        let pins = sync_ioapic(&mut p, time::boot_time_ns());
        self.ioapic_send(&mut p, pins, me, &mut gc.lapic, &mut gc.counts);
        /* An interrupt left pending on the 8259 by an access -- a
         * device's, the serial port's -- is for the CPUs whose LINT0 takes
         * it, which may be asleep or in their guests: rung, from another
         * CPU's access, or from the first CPU's when it is not the taker. */
        let pending = p.pic.pending().is_some() || p.uart.irq_line();
        let ring = pending && (me != 0 || !gc.lapic.accepts_extint());
        drop(p);
        v.skip_io(io);
        if ring {
            self.ring_extint(me as usize);
        } else if first {
            if let Some(d) = self.doorbells.get(0) {
                d.ring();
            }
        }
        None
    }

    /// `gc`'s local APIC's addressing, where the other CPUs match their
    /// messages against it: after anything that may change it -- its mode,
    /// its xAPIC logical ID and model, an INIT. And when that, or whether it
    /// is software-enabled, did change, the IO-APIC's level-triggered
    /// interrupts that no APIC took are sent again: one sent to nobody, or
    /// to an APIC still off, may be this one's to take now.
    fn publish(&self, gc: &mut GuestCpu) {
        let addressing = gc.lapic.addressing().pack();
        if let Some(s) = self.cpus.get(gc.index as usize) {
            s.addressing.store(addressing, Ordering::Relaxed);
        }
        let now = (addressing, gc.lapic.software_enabled());
        if core::mem::replace(&mut gc.published, now) != now {
            self.send_untaken(gc);
        }
    }

    /// Perform the guest's access of its local APIC's page, in xAPIC mode --
    /// a `write` or a read of `gpa` -- by the instruction that faulted: the
    /// register read or written, a load's value in its register, the
    /// instruction stepped past; and what a write does beyond the register,
    /// done -- the task priority's CR8, an IPI sent. An instruction that
    /// cannot be performed stops the guest, showing its bytes.
    fn xapic_access(&self, gc: &mut GuestCpu, gpa: u64, write: bool) -> Option<Stop> {
        let mut bytes = mmio::Bytes::default();
        let op = match mmio::begin(gc.cpu.backend(), &self.memory, gpa, write, &mut bytes) {
            Ok(op) => op,
            Err(error) => {
                let rip = gc.cpu.backend().save().rip;
                return Some(Stop::MmioInsn { gpa, rip, error, bytes });
            }
        };
        gc.counts.mmio_done += 1;
        let offset = (gpa & (lapic::XAPIC_PAGE_SIZE - 1)) as u32;
        let now = time::boot_time_ns();
        if op.is_write() {
            let wrote = gc.lapic.mmio_write(offset, op.size(), op.value as u32, now);
            let v = gc.cpu.backend_mut();
            mmio::finish(v, &op, 0);
            match wrote {
                Wrote::Done => {}
                Wrote::Tpr(tpr) => v.set_cr8(tpr >> 4),
                Wrote::Ipi(ipi) => {
                    self.send(gc, ipi);
                }
                Wrote::Eoi(vector) => self.level_eoi(gc, vector),
            }
            /* The logical ID and the model are this page's to write. */
            self.publish(gc);
        } else {
            let cr8 = gc.cpu.backend().cr8();
            gc.lapic.sync_cr8(cr8);
            let value = gc.lapic.mmio_read(offset, op.size(), now);
            mmio::finish(gc.cpu.backend_mut(), &op, u64::from(value));
        }
        None
    }

    /// An access of a trapped page of a host's function's BAR -- NVIDIA's
    /// mirror of the configuration space -- decoded and performed on the
    /// function's view of its configuration space (`HostFunction::mirror`).
    /// A page of a BAR that is not one trapped is no access this answers:
    /// the guest stops, said.
    fn host_mmio(&self, gc: &mut GuestCpu, gpa: u64, write: bool) -> Option<Stop> {
        if self.windows.is_some() && (linux::ECAM_BASE..linux::ECAM_BASE + linux::ECAM_BYTES).contains(&gpa) {
            return self.ecam(gc, gpa, write);
        }
        let rip = gc.cpu.backend().save().rip;
        let mut p = self.platform.lock();
        /* Its ROM, where the guest has it on: mapped read-only, a copy -- a
         * write is what comes here, and is nothing */
        if let Some((i, offset)) = p.hosts.iter().enumerate().find_map(|(i, h)| h.rom_at(gpa).map(|o| (i, o))) {
            let mut bytes = mmio::Bytes::default();
            let op = match mmio::begin(gc.cpu.backend(), &self.memory, gpa, write, &mut bytes) {
                Ok(op) => op,
                Err(error) => return Some(Stop::MmioInsn { gpa, rip, error, bytes }),
            };
            gc.counts.mmio_done += 1;
            let value = if op.is_write() { 0 } else { p.hosts[i].function.rom_read(offset, usize::from(op.size())) };
            drop(p);
            mmio::finish(gc.cpu.backend_mut(), &op, value);
            return None;
        }
        let found = p.hosts.iter().enumerate().find_map(|(i, h)| h.trapped_at(gpa).map(|(bar, at)| (i, bar, at)));
        let Some((i, bar, offset)) = found else {
            gc.counts.mmio += 1;
            return Some(Stop::Mmio { gpa, rip });
        };
        let mut bytes = mmio::Bytes::default();
        let op = match mmio::begin(gc.cpu.backend(), &self.memory, gpa, write, &mut bytes) {
            Ok(op) => op,
            Err(error) => return Some(Stop::MmioInsn { gpa, rip, error, bytes }),
        };
        gc.counts.mmio_done += 1;
        let h = &mut p.hosts[i];
        let (value, moves) = h.function.trapped(bar, offset, usize::from(op.size()), op.is_write(), op.value);
        for m in moves.iter() {
            self.move_bar(h, m);
        }
        /* What the access let out of what was held pending: under the
         * platform's lock, which delivering takes nothing of */
        h.function.messages().send_pending(|address, data| {
            self.deliver_msi(address, data);
        });
        drop(p);
        mmio::finish(gc.cpu.backend_mut(), &op, value);
        None
    }

    /// An access of the PCI Express configuration space (`ECAM`): the
    /// function and register its offset names, answered as an access by the
    /// ports is -- bus 0's emulated functions by the bus
    /// (`PciBus::function_read`), a host's slot or a bus below a port by
    /// `Platform::host_config` -- the 4 KiB of the function's space reached,
    /// the extended space past the 256 bytes the ports reach. An access of
    /// another size than 1, 2 or 4 bytes, or not aligned to it, reads all
    /// ones and writes nothing, as one of no function does.
    fn ecam(&self, gc: &mut GuestCpu, gpa: u64, write: bool) -> Option<Stop> {
        let rip = gc.cpu.backend().save().rip;
        let mut bytes = mmio::Bytes::default();
        let op = match mmio::begin(gc.cpu.backend(), &self.memory, gpa, write, &mut bytes) {
            Ok(op) => op,
            Err(error) => return Some(Stop::MmioInsn { gpa, rip, error, bytes }),
        };
        gc.counts.mmio_done += 1;
        let t = pci::ecam_target(gpa - linux::ECAM_BASE);
        let size = usize::from(op.size());
        let value = if !matches!(size, 1 | 2 | 4) {
            if op.is_write() { 0 } else { u64::MAX }
        } else {
            let mut p = self.platform.lock();
            let emulated = t.bus == 0 && !p.pci.is_host(t.device);
            match (op.is_write(), emulated) {
                (true, true) => {
                    p.pci.function_write(t.device, t.function, t.at, size, op.value as u32);
                    sync_msix(&mut p);
                    0
                }
                (true, false) => {
                    self.host_config_write(&mut p, &t, size, op.value as u32);
                    0
                }
                (false, true) => u64::from(p.pci.function_read(t.device, t.function, t.at, size)),
                (false, false) => u64::from(p.host_config_read(&t, size)),
            }
        };
        mmio::finish(gc.cpu.backend_mut(), &op, value);
        None
    }

    /// A configuration write of `size` bytes of `value` at `t`: a port's
    /// kept, a host's function's laid over the device's -- what it moved of
    /// the BARs mapped anew, what it let out of what was held pending sent,
    /// under the platform's lock, which delivering takes nothing of.
    fn host_config_write(&self, p: &mut Platform, t: &pci::HostTarget, size: usize, value: u32) {
        match p.host_config(t) {
            Some(HostConfig::Port(i)) => {
                if let Some(q) = p.ports.get_mut(i) {
                    q.port.write(t.at, size, value);
                }
            }
            Some(HostConfig::Function(i)) => {
                let Some(h) = p.hosts.get_mut(i) else { return };
                if let Some(m) = h.function.config_write(t.at, size, value) {
                    self.move_bar(h, m);
                }
                h.function.messages().send_pending(|address, data| {
                    self.deliver_msi(address, data);
                });
            }
            None => {}
        }
    }

    /// A host's function's BAR the guest moved: unmapped where it was, and
    /// mapped where it is now when the table can have it there -- not over
    /// RAM, another BAR or the platform's pages; else left unmapped, and an
    /// access of it stops the guest.
    fn move_bar(&self, h: &mut Hosted, m: host::Moved) {
        if m.bar == host::ROM_BAR {
            /* Its ROM: the host's copy, mapped read-only where it is on */
            let Some(image) = h.function.rom_image() else { return };
            let size = image.len() as u64;
            if let Some(at) = h.mapped[host::ROM_BAR].take() {
                let _ = self.memory.unmap_device(at, size);
            }
            if m.to != 0 && m.to % size == 0 && self.memory.map_rom(h.id, m.to, image).is_ok() {
                h.mapped[host::ROM_BAR] = Some(m.to);
            }
            return;
        }
        let Some(b) = h.function.bars().get(m.bar).copied().flatten() else { return };
        if let Some(at) = h.mapped[m.bar].take() {
            let _ = self.memory.unmap_device(at, b.size);
        }
        if m.to == 0 || m.to % b.size != 0 {
            return;
        }
        if self.memory.map_device(m.to, b.host, b.size, &h.holes[m.bar]).is_ok() {
            h.mapped[m.bar] = Some(m.to);
        }
    }

    /// Answer an `rdmsr` or `wrmsr`: the local APIC's registers and its base
    /// MSR from the APIC, the rest from the policy.
    fn msr(&self, gc: &mut GuestCpu, write: bool) -> Option<Stop> {
        let msr = gc.cpu.backend().regs().rcx as u32;
        let value = {
            let v = gc.cpu.backend();
            ((v.regs().rdx as u32 as u64) << 32) | (v.save().rax as u32 as u64)
        };
        if write {
            gc.counts.msr_write += 1;
        } else {
            gc.counts.msr_read += 1;
        }

        /* KVM's PV EOI: where the guest's word is, and whether it is on --
         * a word in its memory, aligned to 4 bytes, or a #GP. */
        if self.has_apic() && msr == crate::policy::MSR_KVM_PV_EOI_EN {
            if write {
                let word = value & !(PV_EOI_ON | PV_EOI_RESERVED);
                if value & PV_EOI_RESERVED == 0
                    && (value & PV_EOI_ON == 0 || self.memory.read(word, &mut [0u8]).is_ok())
                {
                    gc.pv_eoi = value;
                    gc.cpu.backend_mut().skip_msr();
                } else {
                    self.msr_fault(gc, msr, value, true);
                }
            } else {
                let value = gc.pv_eoi;
                let b = gc.cpu.backend_mut();
                let (save, regs) = b.save_and_regs_mut();
                set_msr_read(save, regs, value);
                b.skip_msr();
            }
            return None;
        }

        /* The APIC timer's deadline, where the guest is offered the
         * TSC-deadline timer: the TSC is the host's, and the deadline it
         * names is turned into the host's time as it is written. */
        if self.has_apic() && msr == lapic::MSR_TSC_DEADLINE && crate::policy::tsc_deadline() {
            if write {
                gc.lapic.set_tsc_deadline(value, tsc_ns(value, self.tsc_offset.load(Ordering::Relaxed)));
                gc.cpu.backend_mut().skip_msr();
                gc.fast = true;
            } else {
                let deadline = gc.lapic.tsc_deadline();
                let b = gc.cpu.backend_mut();
                let (save, regs) = b.save_and_regs_mut();
                set_msr_read(save, regs, deadline);
                b.skip_msr();
            }
            return None;
        }

        if self.has_apic() && lapic::owns(msr) {
            let now = time::boot_time_ns();
            if write {
                match gc.lapic.wrmsr(msr, value, now) {
                    Ok(Wrote::Done) => gc.cpu.backend_mut().skip_msr(),
                    Ok(Wrote::Tpr(tpr)) => {
                        let v = gc.cpu.backend_mut();
                        v.set_cr8(tpr >> 4);
                        v.skip_msr();
                    }
                    Ok(Wrote::Ipi(ipi)) => {
                        gc.cpu.backend_mut().skip_msr();
                        /* To other CPUs alone, the next entry may follow
                         * at once: its own APIC is as it left it. */
                        gc.fast = !self.send(gc, ipi);
                    }
                    Ok(Wrote::Eoi(vector)) => {
                        gc.cpu.backend_mut().skip_msr();
                        self.level_eoi(gc, vector);
                    }
                    Err(lapic::Refused) => self.msr_fault(gc, msr, value, true),
                }
                /* The base MSR moves it between modes. */
                self.publish(gc);
            } else {
                let cr8 = gc.cpu.backend().cr8();
                gc.lapic.sync_cr8(cr8);
                match gc.lapic.rdmsr(msr, now) {
                    Ok(v) => {
                        let b = gc.cpu.backend_mut();
                        let (save, regs) = b.save_and_regs_mut();
                        set_msr_read(save, regs, v);
                        b.skip_msr();
                    }
                    Err(lapic::Refused) => self.msr_fault(gc, msr, 0, false),
                }
            }
            return None;
        }

        let v = gc.cpu.backend_mut();
        let mut own = v.own_msrs();
        if write {
            if crate::policy::wrmsr(v.save_mut(), &mut own, msr, value) && v.set_own_msrs(&own) {
                v.skip_msr();
            } else {
                self.msr_fault(gc, msr, value, true);
            }
        } else {
            match crate::policy::rdmsr(v.save(), &own, msr) {
                Some(value) => {
                    let (save, regs) = v.save_and_regs_mut();
                    set_msr_read(save, regs, value);
                    v.skip_msr();
                }
                None => self.msr_fault(gc, msr, 0, false),
            }
        }
        None
    }

    /// Before an entry with the run loop delivering the APIC's interrupts:
    /// the end of the interrupt in service left to the guest's PV EOI word
    /// where it may be (`Lapic::eoi_may_be_quiet`) -- its bit set in guest
    /// memory, which the guest's EOI then clears rather than exiting -- and
    /// noted, for the exit after.
    fn pv_eoi_arm(&self, gc: &mut GuestCpu) {
        if gc.pv_eoi & PV_EOI_ON == 0 || gc.pv_eoi_armed || !gc.lapic.eoi_may_be_quiet() {
            return;
        }
        let word = gc.pv_eoi & !(PV_EOI_ON | PV_EOI_RESERVED);
        if self.memory.write(word, &[PV_EOI_PENDING]).is_ok() {
            gc.pv_eoi_armed = true;
        }
    }

    /// After an exit: what the guest did with its PV EOI word, if it was set
    /// -- cleared, its EOI, done here as a write of the register would do it;
    /// still set, none yet, and the word cleared, so that an EOI it makes
    /// from here exits (it is set again before the next entry where it still
    /// may be).
    fn pv_eoi_sync(&self, gc: &mut GuestCpu) {
        if !core::mem::take(&mut gc.pv_eoi_armed) {
            return;
        }
        let word = gc.pv_eoi & !(PV_EOI_ON | PV_EOI_RESERVED);
        let mut bits = [0u8];
        match self.memory.read(word, &mut bits).map(|()| bits[0]) {
            Ok(bits) if bits & PV_EOI_PENDING == 0 => {
                gc.counts.pv_eoi += 1;
                if let Some(vector) = gc.lapic.quiet_eoi() {
                    self.level_eoi(gc, vector);
                }
            }
            Ok(_) => {
                let _ = self.memory.write(word, &[0]);
            }
            Err(_) => {}
        }
    }

    /// An MSR access refused: the #GP the CPU would give, and a note for
    /// the report.
    fn msr_fault(&self, gc: &mut GuestCpu, msr: u32, value: u64, write: bool) {
        gc.counts.msr_gp += 1;
        gc.cpu.backend_mut().inject_gp();
        let mut p = self.platform.lock();
        if p.msr_faults.len() < MSR_FAULTS_KEPT {
            /* Into the room taken at `new`. */
            p.msr_faults.push((msr, value, write));
        }
    }
}

/// When the guest's TSC -- the host's plus `offset` -- reaches `tsc`, in host
/// nanoseconds: now, if it has already, or if the TSC's rate is not known.
fn tsc_ns(tsc: u64, offset: u64) -> u64 {
    let (now_tsc, now) = (time::cycle_counter().wrapping_add(offset), time::boot_time_ns());
    let Some(hz) = time::cycle_counter_hz() else { return now };
    let ahead = u128::from(tsc.saturating_sub(now_tsc)) * u128::from(kcore::consts::NS_PER_SEC) / u128::from(hz);
    now.saturating_add(u64::try_from(ahead).unwrap_or(u64::MAX))
}

/// The MSI-X table page of the device in PCI slot `slot`, made guest memory:
/// the page the function's BAR 1 names, where the guest writes its table
/// and the device reads it. None for a guest with no APIC to take a
/// message. RAM never reaches it (`linux::LOW_RAM_END`).
fn msix_page(memory: &mut GuestMemory, apic: bool, slot: usize) -> Result<Option<u32>> {
    if !apic {
        return Ok(None);
    }
    let Some(page) = u32::try_from(slot).ok().and_then(|s| s.checked_mul(pci::MSIX_PAGE))
        .and_then(|off| MSIX_PAGES.checked_add(off)) else { return Ok(None) };
    memory.add(u64::from(page), u64::from(pci::MSIX_PAGE))?;
    Ok(Some(page))
}

/// The SCI's line on the 8259 as the PM registers have it now: requested
/// when an enabled event is set and IRQ 9 is neither requested nor in
/// service already -- so that one still set after the guest ended the
/// interrupt is requested again, as a level-triggered line is -- and
/// withdrawn when none is. Counted where it is delivered: here while IRQ 9
/// is unmasked, and through the IO-APIC's pin as that sends (`sync_ioapic`).
fn sync_sci(p: &mut Platform, now: u64) {
    if p.pm.sci(now) {
        if !p.pic.busy(pm::SCI_IRQ) {
            p.pic.raise(pm::SCI_IRQ);
            if !p.pic.masked(pm::SCI_IRQ) {
                p.pm.scis += 1;
            }
        }
    } else {
        p.pic.lower(pm::SCI_IRQ);
    }
}

/// Whether the timer's last edge has not been taken yet, where it went: on
/// the 8259, IRQ0 requested or in service -- and while the IO-APIC's timer
/// pin is masked, a masked IRQ0 requested counts too, the guest having
/// nowhere else to take it; through the IO-APIC's unmasked pin, its vector
/// still requested in the first CPU's local APIC, `lapic`, when that is the
/// CPU it goes to -- another CPU's is not this one's to look at, and an edge
/// for one is sent when it is due.
fn timer_busy(p: &Platform, lapic: &Lapic) -> bool {
    let Some(route) = p.ioapic.as_ref().and_then(|io| io.route(ioapic::TIMER_PIN)) else {
        return p.pic.busy(0);
    };
    let pic = !p.pic.masked(0) && p.pic.busy(0);
    let ioapic = route.reaches(0, lapic.addressing(), 0) && lapic.requested(route.vector);
    pic || ioapic
}

/// The IO-APIC's pins as their devices drive them now, when there is one --
/// the serial port's, pulsed for each byte sent with its interrupt enabled
/// and up while it asserts it; the SCI's; each PCI line's, up while a
/// function on it has an interrupt status its driver has not read -- and
/// the pins that has an interrupt to send. The timer's edges are the
/// caller's (`LinuxGuest::run`).
fn sync_ioapic(p: &mut Platform, now: u64) -> Pins {
    let Platform { ioapic, uart, pm, pci_devs, .. } = p;
    let Some(io) = ioapic.as_mut() else { return 0 };
    let serial = ioapic::isa_pin(COM1_IRQ);
    let mut pins = if uart.take_pulse() { io.edge(serial) } else { 0 };
    pins |= io.set_line(serial, uart.irq_line());
    let sci = io.set_line(ioapic::isa_pin(pm::SCI_IRQ), pm.sci(now));
    if sci != 0 {
        pm.scis += 1;
    }
    pins |= sci;
    let (mut disk, mut nic) = (false, false);
    for d in pci_devs.iter() {
        match d {
            PciDev::Disk(d) => disk |= d.line(),
            PciDev::Nic(n) => nic |= n.line(),
        }
    }
    pins |= io.set_line(ioapic::isa_pin(DISK_IRQ), disk);
    pins | io.set_line(ioapic::isa_pin(NIC_IRQ), nic)
}

/// Each device's transport told whether its function has MSI-X on: after
/// every write of configuration space, which is how a driver turns it on --
/// and what moves the device's own configuration in its BAR.
fn sync_msix(p: &mut Platform) {
    let Platform { pci, pci_devs, .. } = p;
    for (i, d) in pci_devs.iter_mut().enumerate() {
        let on = pci.msix(i + 1).is_some_and(|(control, _)| control & pci::MSIX_ENABLE != 0);
        match d {
            PciDev::Disk(d) => d.set_msix_enabled(on),
            PciDev::Nic(n) => n.set_msix_enabled(on),
        }
    }
}

/// Hand the guest's receive register its next byte when it is free: first
/// any answer it is waiting for to a terminal query (the cursor position a
/// shell's line editor asks for before it reads), then a byte of what was
/// typed -- held back until the guest is at a prompt, so the boot does not
/// swallow it, and after each line until the next prompt, so a line is not
/// typed ahead of the shell. On every round of the first CPU's loop and not only
/// at an idle HLT: while a shell's line editor reads the answer to its
/// cursor query it spins polling the port rather than halting, so a byte
/// offered only at HLT would never arrive and the editor would time out.
fn feed_console(p: &mut Platform, host: &dyn Host) {
    if !p.uart.rx_empty() {
        return;
    }
    if let Some(byte) = p.uart.take_reply() {
        p.uart.set_rx(byte);
    } else if let Some(byte) = host.input(p.uart.prompt_seen()) {
        p.uart.set_rx(byte);
        if byte == b'\n' || byte == b'\r' {
            p.uart.line_taken();
        }
    }
}

fn size_mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}

/// What an `in` of `size` bytes leaves in RAX: AL or AX replaced and the
/// rest kept, or -- a write of EAX, in 64-bit mode -- RAX zero-extended.
fn set_in(save: &mut Save, size: u8, value: u32) {
    let value = u64::from(value) & size_mask(size);
    save.rax = if size == 4 { value } else { (save.rax & !size_mask(size)) | value };
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
