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
//! The PC is one without an IO-APIC: the 8259 pair is wired through the
//! first CPU's LINT0, in virtual-wire mode, so its interrupts -- the PIT's
//! tick, the serial port's, the disks' and NICs' -- are all the first CPU's.
//! That CPU's task also does the devices' work between its guest's turns:
//! the timer's edges, what the host has for the guest's console, NICs and
//! disks. The others take their interrupts from their local APICs: their
//! timers, and each other's IPIs.
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
use hvarch::{Error, Result};
use kcore::sync::Mutex;
use kcore::time;

use crate::devices::blk::{self, Blk};
use crate::devices::net::{self, Net};
use crate::devices::pci::{self, Function, PciBus};
use crate::devices::virtio::Raise;
use crate::devices::{Pic, Pit, Rtc, Uart};
use crate::lapic::{self, Delivery, Ipi, Lapic, Wrote};
use crate::linux::{self, Header, Layout};
use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::smp::{Doorbells, Mailbox, MAX_VCPUS};
use crate::svm::Exit;
use crate::vm::{Cpu, Refusal};

pub use crate::smp::Doorbell;

/// COM1, the guest's console.
const COM1: u16 = 0x3F8;

/// Where the virtio devices' I/O BARs are put, as a BIOS would put them --
/// the disks' from 0xC000, the NICs' from 0xC100 -- and the IRQs they share:
/// 11 and 10, free on a PC, on the slave PIC.
const DISK_IO_BASE: u16 = 0xC000;
const NIC_IO_BASE: u16 = 0xC100;
const DISK_IRQ: u8 = 11;
const NIC_IRQ: u8 = 10;
/// Where a device's MSI-X table page is, for a guest of more than one CPU:
/// this plus a page for its PCI slot -- in the platform's MMIO window, clear
/// of the local APIC's page, the IO-APIC's address the MP table gives, and
/// the page read of an absent device finds; and of the RAM of a guest of up
/// to 4064 MiB. A guest of more is offered none.
const MSIX_PAGES: u32 = 0xFE00_0000;
/// The MSI-X entries each kind of device offers: one for configuration
/// changes and one a queue -- what Linux's virtio-pci asks for, a vector a
/// queue.
const DISK_VECTORS: u16 = 2;
const NIC_VECTORS: u16 = 3;
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
/// The longest a CPU asleep for good -- halted with interrupts off, or
/// waiting for a start-up IPI -- sleeps before it looks again. Only another
/// CPU can wake one, by ringing it, and a stop rings every CPU; this is a
/// net under that, not how it is woken.
const ASLEEP_WAIT_NS: u64 = kcore::consts::NS_PER_SEC;

/// Room for the register dump of the CPU that stopped a guest.
const DUMP_BYTES: usize = 1024;

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
    /// It took its local APIC out of x2APIC mode into xAPIC mode, a page of
    /// MMIO this hypervisor does not emulate -- a kernel told to use an
    /// IO-APIC this machine has none of, or not to use x2APIC (`nox2apic`).
    Xapic { rip: u64 },
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

impl Stop {
    /// Whether the guest reset itself, one way or another: what a guest
    /// that is to be booted again when it reboots is booted again after.
    pub fn is_reset(&self) -> bool {
        matches!(self, Stop::Reset { .. } | Stop::Shutdown { .. } | Stop::Init { .. })
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
    /// the guest on its way in (`Kick`): handed over first.
    pub kicked: u64,
    /// Interrupts injected from the 8259.
    pub irq: u64,
    pub irq0: u64,
    pub irq4: u64,
    pub edges0: u64,
    /// Interrupts injected from the local APIC: its timer's, IPIs, its own.
    pub apic: u64,
    /// The local APIC timer's interrupts requested.
    pub timer: u64,
    /// IPIs this CPU sent, and fixed ones it was sent.
    pub ipi_sent: u64,
    pub ipi_taken: u64,
    /// Devices' MSI-X messages this CPU sent on their behalf, doing their
    /// work: into its own APIC or another CPU's mailbox.
    pub msi: u64,
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
    /// Instructions the guest was told (by CPUID) it does not have, run
    /// anyway and answered with #UD; WBINVDs stepped past; `mov`s to and
    /// from CR8 answered from the shadow task-priority register (VT-x); and
    /// `mov`s to CR0 that changed a bit VT-x keeps for the host.
    pub ud: u64,
    pub wbinvd: u64,
    pub cr8: u64,
    pub cr0: u64,
    pub exits: u64,
}

impl Counts {
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
        self.irq += o.irq;
        self.irq0 += o.irq0;
        self.irq4 += o.irq4;
        self.edges0 += o.edges0;
        self.apic += o.apic;
        self.timer += o.timer;
        self.ipi_sent += o.ipi_sent;
        self.ipi_taken += o.ipi_taken;
        self.msi += o.msi;
        self.init += o.init;
        self.started += o.started;
        self.nmi += o.nmi;
        self.nmi_lost += o.nmi_lost;
        self.blocked += o.blocked;
        self.hlt += o.hlt;
        self.pause += o.pause;
        self.sleeps += o.sleeps;
        self.slept_ns += o.slept_ns;
        self.ud += o.ud;
        self.wbinvd += o.wbinvd;
        self.cr8 += o.cr8;
        self.cr0 += o.cr0;
        self.exits += o.exits;
    }
}

/// The most MSR accesses answered with #GP kept for a report.
const MSR_FAULTS_KEPT: usize = 8;
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
    /// The last interrupt injected came from the 8259: when it and the APIC
    /// both have one, they take turns (`LinuxGuest::deliver`).
    last_from_pic: bool,
    counts: Counts,
}

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
}

/// The guest's devices: what its CPUs share under one lock.
struct Platform {
    uart: Uart,
    pit: Pit,
    rtc: Rtc,
    pic: Pic,
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
    pub fn new(machine: &Machine, mem_bytes: u64, cpus: u32, doorbells: Arc<Doorbells>)
        -> Result<(Self, Vec<GuestCpu>)>
    {
        let n = cpus as usize;
        if n == 0 || n > MAX_CPUS || doorbells.len() != n {
            return Err(Error::BadAddress);
        }
        let mut memory = GuestMemory::new(machine.caps().vendor())?;
        memory.add(0, mem_bytes)?;

        let mut vcpus = Vec::new();
        vcpus.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        let mut shared = Vec::new();
        shared.try_reserve_exact(n).map_err(|_| Error::NoMemory)?;
        for i in 0..cpus {
            let cpu = Cpu::new(machine, &memory, 0)?;
            if i > 0 && !cpu.backend().runs_real_mode() {
                return Err(Error::NoUnrestrictedGuest);
            }
            let activity = if i == 0 { Activity::Running } else { Activity::WaitForSipi };
            vcpus.push(GuestCpu {
                index: i,
                cpu,
                lapic: Lapic::new(i, i == 0),
                activity,
                nmi_pending: false,
                last_from_pic: false,
                counts: Counts::default(),
            });
            shared.push(CpuShared {
                mail: Mailbox::new(),
                extint: AtomicBool::new(i == 0),
                activity: AtomicU8::new(activity.code()),
                host_cpu: AtomicU32::new(u32::MAX),
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
            pci: PciBus::new()?,
            pci_devs,
            disks: 0,
            nics: 0,
            msix_pending,
            port_hist,
            msr_faults,
        };
        let all = if n == u64::BITS as usize { u64::MAX } else { (1u64 << n) - 1 };
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
        };
        Ok((guest, vcpus))
    }

    /// Its memory: for the kernel and the initrd to be streamed into.
    pub fn memory(&self) -> &GuestMemory {
        &self.memory
    }

    /// How many CPUs it has.
    pub fn cpus(&self) -> u32 {
        self.cpus.len() as u32
    }

    /// Whether its CPUs have local APICs: a guest of more than one CPU. One
    /// of one is the PC it always was -- an 8259 on the CPU's interrupt pin,
    /// no APIC in CPUID, no MP table, the APIC's MSRs none of its business
    /// -- which any kernel boots; the local APIC, its MP table and its x2APIC
    /// mode take a Linux of 6.6 or later.
    pub fn has_apic(&self) -> bool {
        self.cpus.len() > 1
    }

    /// Give it another disk, over `backend`: `vda`, `vdb`, ... in the order
    /// they are added, each a virtio block device on the PCI bus.
    pub fn add_disk(&mut self, backend: Box<dyn blk::Backend>, id: &str) -> Result<()> {
        let apic = self.has_apic();
        let mut p = self.platform.lock();
        if p.disks >= MAX_DISKS {
            return Err(Error::NoMemory);
        }
        let mut disk = Blk::new(backend, id)?;
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

    /// Give it a NIC with `mac`, its frames carried by `backend`: `eth0`,
    /// `eth1`, a virtio network device on the PCI bus.
    pub fn add_nic(&mut self, backend: Box<dyn net::Backend>, mac: [u8; 6]) -> Result<()> {
        let apic = self.has_apic();
        let mut p = self.platform.lock();
        if p.nics >= MAX_NICS {
            return Err(Error::NoMemory);
        }
        let mut nic = Net::new(backend, mac)?;
        let io_base = NIC_IO_BASE + (p.nics as u16) * (net::BAR_SIZE as u16);
        let mut f = Function::device(&Net::identity(), io_base, net::BAR_SIZE, NIC_IRQ);
        if let Some(page) = msix_page(&mut self.memory, apic, p.pci_devs.len() + 1)? {
            nic.offer_msix(NIC_VECTORS).ok_or(Error::NoMemory)?;
            f = f.with_msix(NIC_VECTORS, page);
        }
        p.pci.add(f)?;
        p.pci_devs.push(PciDev::Nic(nic));
        p.msix_pending.push(0);
        p.nics += 1;
        Ok(())
    }

    /// What each disk has done: (sectors, statistics, whether a ring of the
    /// driver's stopped it). Empty when there is no memory to say it in.
    pub fn disk_stats(&self) -> Vec<(u64, blk::Stats, bool)> {
        let p = self.platform.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(p.disks).is_ok() {
            for d in &p.pci_devs {
                if let PciDev::Disk(d) = d {
                    v.push((d.sectors(), d.stats, d.broken().is_some()));
                }
            }
        }
        v
    }

    /// What each NIC has done: (statistics, whether a ring stopped it).
    pub fn nic_stats(&self) -> Vec<(net::Stats, bool)> {
        let p = self.platform.lock();
        let mut v = Vec::new();
        if v.try_reserve_exact(p.nics).is_ok() {
            for d in &p.pci_devs {
                if let PciDev::Nic(n) = d {
                    v.push((n.stats, n.broken().is_some()));
                }
            }
        }
        v
    }

    /// Write the guest's furniture -- the zero page, the command line, the
    /// memory map, the page tables, the GDT and the MP table -- and put its
    /// first CPU, `bsp`, at the kernel's entry. The kernel and initrd bytes
    /// must already be in memory at the addresses `layout` names; the caller
    /// streams those in.
    pub fn load(&mut self, bsp: &mut GuestCpu, header: &Header, first: &[u8], layout: Layout, cmdline: &[u8])
        -> Result<()>
    {
        if bsp.index != 0 {
            return Err(Error::BadAddress);
        }
        linux::build(&self.memory, header, first, &layout, cmdline, self.cpus())?;
        linux::set_entry(bsp.cpu.backend_mut(), &layout);
        Ok(())
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
        let me = gc.index as usize;
        let (Some(bell), Some(shared)) = (self.doorbells.get(me), self.cpus.get(me)) else {
            /* `new` made one of each for every CPU it made. */
            return gc.counts;
        };
        let kick = bell.kick();

        loop {
            let now = time::boot_time_ns();
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

            /* From here a ring kicks this vCPU out of its guest, or turns its
             * entry back; so what is looked at below -- its mail, the
             * devices -- is what arrived before the ring, and anything later
             * rings it. */
            kick.prepare();
            if matches!(gc.activity, Activity::Asleep | Activity::WaitForSipi) {
                /* Awake while it looks, before it takes anything. */
                self.asleep.fetch_and(!(1u64 << me), Ordering::AcqRel);
            }
            self.take_mail(gc);
            if gc.lapic.timer(now) {
                gc.counts.timer += 1;
            }
            shared.extint.store(gc.lapic.accepts_extint(), Ordering::Relaxed);

            /* The devices' work, on the first CPU's loop, whatever that CPU
             * is doing: its console fed, the PIT's edges and the serial
             * port's interrupt raised on the 8259, and what the NICs and
             * disks have for the guest given to it -- raising their lines.
             * The 8259 then has an interrupt for a CPU that takes them. */
            let mut extint = false;
            let mut pit_edge = None;
            if me == 0 {
                let mut p = self.platform.lock();
                feed_console(&mut p, host);
                /* The timer: a channel-0 period elapsed is an IRQ0 edge --
                 * one owed edge at a time, and only once the last has been
                 * taken: an IRQ0 still requested or in service would swallow
                 * the next, and a tick the guest never saw is time it never
                 * counts (`Pit::ch0_fire`). */
                if !p.pic.busy(0) && p.pit.ch0_fire() {
                    p.pic.raise(0);
                    gc.counts.edges0 += 1;
                }
                /* COM1's transmitter is always ready, so with its THR-empty
                 * interrupt enabled it asserts IRQ4 -- which is how the
                 * serial driver sends past the first byte, an interrupt at
                 * a time. */
                if p.uart.irq_active() {
                    p.pic.raise(4);
                }
                self.poll_devices(&mut p, me as u32, &mut gc.lapic, &mut gc.counts);
                extint = gc.lapic.accepts_extint() && p.pic.pending().is_some();
                /* Not while IRQ0 is still requested or in service -- masked,
                 * say: none is handed over until the guest takes that one,
                 * and an owed edge, already due, would have the vCPU wake
                 * without sleeping for good. */
                pit_edge = if p.pic.busy(0) { None } else { p.pit.next_ch0_edge_ns() };
            } else if gc.lapic.accepts_extint() {
                extint = self.platform.lock().pic.pending().is_some();
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
                            bell.wait(ASLEEP_WAIT_NS.min(deadline.saturating_sub(now)).max(1));
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
                            .min(gc.lapic.next_timer_ns().unwrap_or(u64::MAX))
                            .min(now.saturating_add(MAX_HALT_WAIT_NS))
                            .min(deadline);
                        if until > now {
                            bell.wait(until - now);
                            gc.counts.sleeps += 1;
                            gc.counts.slept_ns += time::boot_time_ns().saturating_sub(now);
                        }
                        continue;
                    }
                    self.set_activity(gc, Activity::Running);
                }
                Activity::Running => {}
            }
            self.deliver(gc, extint);

            let (exit, host_cpu) = match gc.cpu.enter(&self.memory, machine, Some(kick)) {
                Ok(entered) => entered,
                Err(refusal) => {
                    self.stop_with(Stop::Refused(refusal), gc);
                    break;
                }
            };
            shared.host_cpu.store(host_cpu, Ordering::Relaxed);
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
                    None
                }
                Exit::Io(io) => self.io(gc, &io, host),
                Exit::Cpuid => {
                    gc.counts.cpuid += 1;
                    let id = gc.lapic.id();
                    let apic = self.has_apic();
                    let v = gc.cpu.backend_mut();
                    let sub = v.regs().rcx as u32;
                    let leaf = v.save().rax as u32;
                    let answer = crate::policy::cpuid(leaf, sub, id, apic);
                    let (save, regs) = v.save_and_regs_mut();
                    crate::policy::apply_cpuid(save, regs, &answer);
                    v.skip_cpuid();
                    None
                }
                Exit::Msr { write } => self.msr(gc, write),
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
                Exit::NestedFault { gpa, error } => {
                    /* A read of the platform's MMIO window that nothing
                     * answers is a probe for a device that is not there: map
                     * all ones and let the instruction run again. Anything
                     * else -- a write, a fetch, a walk of the guest's own
                     * tables, an address outside the window, the xAPIC's
                     * page -- stops it. */
                    use vmcb::npf;
                    let plain_read = error & (npf::PRESENT | npf::WRITE | npf::FETCH) == 0
                        && error & npf::FINAL != 0;
                    if plain_read && self.memory.map_absent(gpa).is_ok() {
                        None
                    } else {
                        gc.counts.mmio += 1;
                        Some(Stop::Mmio { gpa, rip })
                    }
                }
                Exit::Shutdown => Some(Stop::Shutdown { rip }),
                Exit::Exception { vector, .. } => Some(Stop::Exception { vector, rip }),
                Exit::MachineCheck => Some(Stop::Exception { vector: 18, rip }),
                Exit::Invalid => Some(Stop::Invalid),
                /* The guest can take an interrupt, or an NMI, now; the next
                 * entry injects it. Nothing to do here. */
                Exit::IrqWindow | Exit::NmiWindow => None,
                Exit::Other(code) if matches!(code,
                    vmcb::exit::MONITOR | vmcb::exit::MWAIT | vmcb::exit::MWAIT_ARMED
                    | vmcb::exit::RDTSCP | vmcb::exit::RDPRU | vmcb::exit::XSETBV) =>
                {
                    /* Intercepted, and not offered by CPUID: the guest gets
                     * what a CPU without them gives it. */
                    gc.counts.ud += 1;
                    gc.cpu.backend_mut().inject_ud();
                    None
                }
                Exit::Other(vmcb::exit::WBINVD) => {
                    gc.counts.wbinvd += 1;
                    gc.cpu.backend_mut().skip_wbinvd();
                    None
                }
                Exit::Cr8 { write, gpr } => {
                    gc.counts.cr8 += 1;
                    let v = gc.cpu.backend_mut();
                    v.cr8_access(write, gpr);
                    /* CR8 is the APIC's task priority: a write of it is
                     * one of the TPR too. */
                    gc.lapic.sync_cr8(v.cr8());
                    None
                }
                Exit::Cr0Write { value } => {
                    gc.counts.cr0 += 1;
                    gc.cpu.backend_mut().cr0_write(value);
                    None
                }
                /* Spin-waiting: stepped past, and round the loop, where a
                 * tick it may be waiting for is handed over. */
                Exit::Pause => {
                    gc.counts.pause += 1;
                    gc.cpu.backend_mut().skip_pause();
                    None
                }
                Exit::Hypercall => {
                    /* No paravirtualisation is offered; a VMMCALL is a fault
                     * to the guest. Step past it so a stray one does not
                     * spin, and inject nothing -- the guest that meant it
                     * will notice its result did not change. */
                    gc.cpu.backend_mut().skip_vmmcall();
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
    fn stop_with(&self, stop: Stop, gc: &GuestCpu) {
        {
            let mut stopped = self.stopped.lock();
            if stopped.is_none() {
                let mut dump = String::new();
                if gc.activity != Activity::WaitForSipi && dump.try_reserve(DUMP_BYTES).is_ok() {
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
    fn fill_dump(&self, gc: &GuestCpu) {
        if gc.activity == Activity::WaitForSipi {
            return;
        }
        let mut stopped = self.stopped.lock();
        if let Some(s) = stopped.as_mut() {
            if s.dump.is_empty() && s.dump.try_reserve(DUMP_BYTES).is_ok() {
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
            gc.cpu.backend_mut().init_reset();
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
            gc.lapic.accept_all(&mail.fixed);
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
    fn deliver(&self, gc: &mut GuestCpu, extint: bool) {
        if gc.nmi_pending {
            let v = gc.cpu.backend_mut();
            if v.nmi_allowed() {
                v.inject_nmi();
                gc.nmi_pending = false;
                gc.counts.nmi += 1;
                return;
            }
            /* In its handler for the last one, or in a shadow: told when it
             * can -- and meanwhile an interrupt may go in. */
            v.request_nmi_window();
        }
        let cr8 = gc.cpu.backend().cr8();
        gc.lapic.sync_cr8(cr8);
        let apic = gc.lapic.pending();
        if !extint && apic.is_none() {
            gc.cpu.backend_mut().clear_irq_window();
            return;
        }
        let v = gc.cpu.backend_mut();
        if !v.interruptible() {
            gc.counts.blocked += 1;
            v.request_irq_window();
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
            }
            None => v.clear_irq_window(),
        }
    }

    /// Send `ipi`, which CPU `from` wrote to its interrupt command register:
    /// into the mailbox of each CPU it is for, whose doorbell then rings --
    /// a CPU asleep wakes, and takes it. One for the sender itself goes
    /// straight into its own APIC.
    fn send(&self, from: &mut GuestCpu, ipi: Ipi) {
        from.counts.ipi_sent += 1;
        let me = from.index;
        for (i, target) in self.cpus.iter().enumerate() {
            let id = i as u32;
            if !ipi.reaches(id, me) {
                continue;
            }
            match ipi.delivery {
                Delivery::Fixed | Delivery::LowestPriority if id == me => {
                    from.lapic.accept(ipi.vector);
                }
                Delivery::Fixed | Delivery::LowestPriority => target.mail.post_fixed(ipi.vector),
                Delivery::Nmi => target.mail.post_nmi(),
                Delivery::Init if id == 0 => {
                    /* INIT to the boot CPU is a PC's reset: it would start
                     * again at the reset vector, in firmware there is none
                     * of. */
                    let rip = from.cpu.backend().save().rip;
                    self.stop_with(Stop::Init { rip }, from);
                    return;
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
                return;
            }
        }
    }

    /// Ring the CPUs that take the 8259's interrupts, after a port access of
    /// another CPU's left one pending there: the first CPU, whose loop does
    /// the devices' work, and any other whose APIC takes them.
    fn ring_extint(&self) {
        for (i, c) in self.cpus.iter().enumerate() {
            if i == 0 || c.extint.load(Ordering::Relaxed) {
                if let Some(d) = self.doorbells.get(i) {
                    d.ring();
                }
            }
        }
    }

    /// What the disks' backends have served, given back to the guest, and
    /// what the NICs' backends have for it, into its buffers: each kind's
    /// line raised when that calls for an interrupt.
    fn poll_devices(&self, p: &mut Platform, me: u32, lapic: &mut Lapic, counts: &mut Counts) {
        let mut raised = [Raise::default(); MAX_DISKS + MAX_NICS];
        for (d, r) in p.pci_devs.iter_mut().zip(raised.iter_mut()) {
            *r = match d {
                PciDev::Disk(d) => d.poll(&self.memory),
                PciDev::Nic(n) => n.poll(&self.memory),
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
                let still = self.msix(p, me, lapic, counts, dev, held);
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
            let held = self.msix(p, me, lapic, counts, dev, r.vectors);
            if let Some(pending) = p.msix_pending.get_mut(dev) {
                if *pending | held != *pending {
                    *pending |= held;
                    self.write_pba(p, dev);
                }
            }
        }
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
    fn msix(&self, p: &Platform, me: u32, lapic: &mut Lapic, counts: &mut Counts, dev: usize, entries: u32) -> u32 {
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
                counts.msi += 1;
                self.post_msi(me, lapic, &ipi);
            }
        }
        held
    }

    /// A device's MSI, as `ipi` decodes it: its vector into the request
    /// register of each CPU it names -- the first of them only, for lowest
    /// priority -- straight into `lapic` for the CPU `me` doing the device's
    /// work, and through the mailbox and a ring for any other.
    fn post_msi(&self, me: u32, lapic: &mut Lapic, ipi: &Ipi) {
        for (i, target) in self.cpus.iter().enumerate() {
            let id = i as u32;
            if !ipi.reaches(id, me) {
                continue;
            }
            if id == me {
                lapic.accept(ipi.vector);
            } else {
                target.mail.post_fixed(ipi.vector);
                if let Some(d) = self.doorbells.get(i) {
                    d.ring();
                }
            }
            if ipi.delivery == Delivery::LowestPriority {
                return;
            }
        }
    }

    /// Answer a port access and step past it -- or, for a write that resets
    /// the machine, leave the guest where it is and say which it was.
    fn io(&self, gc: &mut GuestCpu, io: &crate::svm::Io, host: &dyn Host) -> Option<Stop> {
        let me = gc.index;
        let v = gc.cpu.backend_mut();
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
                gc.counts.port_out += 1;
                return Some(Stop::Reset { port: io.port, value, rip: v.save().rip });
            }
        }

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
                    Some(PciDev::Disk(d)) => d.io_write(offset, io.size, value, &self.memory),
                    Some(PciDev::Nic(n)) => n.io_write(offset, io.size, value, &self.memory),
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
        /* An interrupt left pending on the 8259 by another CPU's access --
         * a device's, the serial port's -- is the first CPU's to take, and
         * it may be asleep or in its guest: ring it. */
        let ring = me != 0 && (p.pic.pending().is_some() || p.uart.irq_active());
        drop(p);
        v.skip_io(io);
        if ring {
            self.ring_extint();
        }
        None
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
                        self.send(gc, ipi);
                    }
                    Ok(Wrote::Xapic) => {
                        return Some(Stop::Xapic { rip: gc.cpu.backend().save().rip });
                    }
                    Err(lapic::Refused) => self.msr_fault(gc, msr, value, true),
                }
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
        if write {
            if crate::policy::wrmsr(v.save_mut(), msr, value) {
                v.skip_msr();
            } else {
                self.msr_fault(gc, msr, value, true);
            }
        } else {
            match crate::policy::rdmsr(v.save(), msr) {
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

/// The MSI-X table page of the device in PCI slot `slot`, made guest memory:
/// the page the function's BAR 1 names, where the guest writes its table
/// and the device reads it. None for a guest with no APIC to take a
/// message, or one whose RAM reaches that far.
fn msix_page(memory: &mut GuestMemory, apic: bool, slot: usize) -> Result<Option<u32>> {
    if !apic {
        return Ok(None);
    }
    let Some(page) = u32::try_from(slot).ok().and_then(|s| s.checked_mul(pci::MSIX_PAGE))
        .and_then(|off| MSIX_PAGES.checked_add(off)) else { return Ok(None) };
    match memory.add(u64::from(page), u64::from(pci::MSIX_PAGE)) {
        Ok(()) => Ok(Some(page)),
        /* RAM there already: a guest that large goes without. */
        Err(Error::BadAddress) => Ok(None),
        Err(e) => Err(e),
    }
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
/// typed -- held back until the guest has reached a prompt, so the boot
/// does not swallow it. On every round of the first CPU's loop and not only
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
