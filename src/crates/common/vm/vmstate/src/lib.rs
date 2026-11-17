//! A running VM's state, as bytes (plans/vm-migration.md, "The state"):
//! what a VM paused on one host is resumed from on another -- or on the same
//! one, from a file (`hv save`, `hv restore`).
//!
//! ```text
//! "nosvmst1" | version | part VM | part CPU, one a CPU | part PLATFORM
//! part: kind u8, length u32, its bytes -- read whole, and to the byte
//! ```
//!
//! What is here, and what is not:
//!
//! - **The VM** ([`Vm`]): how it was built -- its RAM's ranges, its CPUs'
//!   count and the shape their CPUID tells them, x2APIC or xAPIC, an
//!   IO-APIC, its NICs' offloads -- the extension it ran under, its CPU
//!   model's digest and its TSC's rate, and the guest's TSC at the pause.
//! - **Each CPU** ([`Cpu`]) in a form of neither extension's: its registers,
//!   its control and system registers, its segments with their attributes
//!   as a descriptor packs them, its x87, SSE and AVX registers in XSAVE's
//!   standard form, the event on its way in, its interrupt shadow and NMI
//!   blocking, what it is doing, its local APIC ([`Lapic`]) and what other
//!   CPUs had sent it and it had not taken ([`Mail`]).
//! - **Every device** ([`Platform`]): the UART, the 8259 pair, the PIT, the
//!   RTC's CMOS, ACPI's PM block, pvpanic, the IO-APIC, what the guest wrote
//!   of each PCI function's configuration, and each virtio device -- drained
//!   first, nothing of the host's in flight.
//! - **Times** are relative to the pause: a timer's next edge is how long
//!   after the pause it was due, a count's start how long before it. A
//!   restore puts them back relative to its own now, so the pause is left
//!   out of the guest's time, its TSC's and its devices' alike.
//! - **Not the pages**: they go apart, in rounds, or after the state in a
//!   file. Nor what is the host's: where the CPUs ran, ASIDs and VPIDs, the
//!   mappings of the rings, the counters.
//!
//! Every read is checked, and a field out of its bounds refuses the whole
//! state, naming what it was: a state comes from another host -- which is
//! authenticated -- and from another build, which is not the same code. The
//! version says which; one this does not know is refused, never guessed.
//! What only the host that restores it can say -- a ring inside the guest's
//! RAM, a feature its devices offer, an XSAVE component its CPU has -- the
//! restore checks in `hv` as each piece is put back.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod wire;
mod cpu;
mod apic;
mod devices;

use alloc::vec::Vec;

pub use apic::{Lapic, Mail, LVT_MAX, WORDS};
pub use cpu::{Activity, Cpu, Event, EventKind, Regs, Segment, Table, ERROR_CODE_VECTORS, XSAVE_MAX};
pub use devices::{Blk, Device, Function, IoApic, Msix, Net, Pic, PicChip, Pit, PitChannel, Platform, Pm, PvPanic,
                  Queue, Rtc, Takes, Transport, Uart, FLOWS, IOAPIC_PINS, QUEUES_MAX, REPLY_MAX};

use wire::{In, Out};

/// The format's first eight bytes.
pub const MAGIC: [u8; 8] = *b"nosvmst1";
/// The format's version: the one this writes, and the only one it reads.
pub const VERSION: u16 = 1;
/// The most CPUs a state holds.
pub const CPUS_MAX: usize = 64;
/// The most ranges of RAM.
pub const RANGES_MAX: usize = 8;

/// Why a state was refused: the field, or the rule, it broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// The CPU's extension a VM ran under: a state is restored under the same
/// one, never across -- the other vendor's CPU is another CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    /// AMD-V.
    Svm,
    /// Intel VT-x.
    Vmx,
}

/// A range of guest-physical addresses with RAM behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub base: u64,
    pub size: u64,
}

/// What a guest's CPUs are told of where they sit: `hv::place::Shape`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub cpus: u32,
    /// Threads of one core in twos.
    pub smt: bool,
    /// One cache shared by all.
    pub llc: bool,
}

/// The VM as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vm {
    pub vendor: Vendor,
    /// What its SMBIOS calls it.
    pub uuid: [u8; 16],
    /// Its RAM, in ascending order, none overlapping: what the pages that
    /// go apart are held inside.
    pub ranges: Vec<Range>,
    /// Its local APICs out of reset in x2APIC mode, rather than xAPIC.
    pub x2apic: bool,
    /// It has an IO-APIC.
    pub ioapic: bool,
    /// Its CPUs' interrupts the CPU's to deliver where it can (VT-x's
    /// virtual-interrupt delivery): as asked, or None for the default.
    pub apicv: Option<bool>,
    /// Its NICs offer their transmit offloads.
    pub offloads: bool,
    /// Its NICs' receive interrupts no closer than this apart.
    pub rx_moderation_ns: u64,
    pub shape: Shape,
    /// Its CPU model's digest: what its CPUID and the MSRs read from the
    /// host's answer it with (`hv::policy`). A host that gives another
    /// refuses it.
    pub model: [u8; 32],
    /// Its TSC's rate, Hz: the host's it ran on, which it was told.
    pub tsc_hz: u64,
    /// Its TSC at the pause -- the same on every CPU, the host's being one:
    /// what a restore's TSC offset makes it go on from.
    pub tsc: u64,
    /// How many CPUs a start-up IPI had started, for a report.
    pub started: u64,
}

/// A VM's state: the VM, each of its CPUs, its devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmState {
    pub vm: Vm,
    /// One a CPU, its APIC ID its index: as many as `vm.shape.cpus`.
    pub cpus: Vec<Cpu>,
    pub platform: Platform,
}

/* The parts, by their kind byte. */
const PART_VM: u8 = 1;
const PART_CPU: u8 = 2;
const PART_PLATFORM: u8 = 3;
/// The most bytes a part has: the platform's, of every device's, is a few
/// KiB, a CPU's under two; this is room, and a bound on what a length a
/// state gives can make anything allocate.
const PART_MAX: usize = 1 << 20;

impl VmState {
    /// The state as bytes; None when there was no memory for them.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let mut out = Out::new();
        out.reserve(16 + 2048 * self.cpus.len() + 16 * 1024)?;
        out.fixed(&MAGIC);
        out.u16(VERSION);
        out.part(PART_VM, |o| self.vm.put(o))?;
        for cpu in &self.cpus {
            out.part(PART_CPU, |o| cpu.put(o))?;
        }
        out.part(PART_PLATFORM, |o| self.platform.put(o))?;
        out.finish()
    }

    /// The state `bytes` hold -- every field of it checked, the whole of it
    /// refused at the first that is not one a state could have.
    pub fn decode(bytes: &[u8]) -> Result<VmState> {
        let mut r = In::new(bytes);
        if r.fixed::<8>("magic")? != MAGIC {
            return Err(Error("not a VM's state: its first bytes are another format's"));
        }
        let version = r.u16("version")?;
        if version != VERSION {
            return Err(Error("a VM's state of a version this does not read"));
        }
        let vm = r.part(PART_VM, "the VM's part", Vm::get)?;
        let n = vm.shape.cpus as usize;
        let mut cpus = Vec::new();
        cpus.try_reserve_exact(n).map_err(|_| Error("no memory for the CPUs"))?;
        for i in 0..n {
            let cpu = r.part(PART_CPU, "a CPU's part", Cpu::get)?;
            if cpu.lapic.id as usize != i {
                return Err(Error("a CPU's local APIC ID is not its place"));
            }
            cpus.push(cpu);
        }
        let platform = r.part(PART_PLATFORM, "the platform's part", Platform::get)?;
        if !r.done() {
            return Err(Error("bytes past the state's end"));
        }
        if platform.ioapic.is_some() != vm.ioapic {
            return Err(Error("an IO-APIC where the VM has none, or none where it has one"));
        }
        if platform.msix_pending.len() != platform.devices.len() {
            return Err(Error("MSI-X pending bits for another number of devices"));
        }
        if platform.functions.len() != platform.devices.len() + 1 {
            return Err(Error("PCI functions for another number of devices"));
        }
        Ok(VmState { vm, cpus, platform })
    }
}

impl Vendor {
    /// Its byte, in a state and in a migration's offer.
    pub fn code(self) -> u8 {
        match self {
            Vendor::Svm => 1,
            Vendor::Vmx => 2,
        }
    }

    pub fn from_code(code: u8) -> Result<Vendor> {
        match code {
            1 => Ok(Vendor::Svm),
            2 => Ok(Vendor::Vmx),
            _ => Err(Error("an extension neither AMD-V nor VT-x")),
        }
    }
}

impl Vm {
    fn put(&self, o: &mut Out) {
        o.u8(self.vendor.code());
        o.fixed(&self.uuid);
        o.u8(self.ranges.len() as u8);
        for r in &self.ranges {
            o.u64(r.base);
            o.u64(r.size);
        }
        o.bool(self.x2apic);
        o.bool(self.ioapic);
        o.u8(match self.apicv {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
        o.bool(self.offloads);
        o.u64(self.rx_moderation_ns);
        o.u32(self.shape.cpus);
        o.bool(self.shape.smt);
        o.bool(self.shape.llc);
        o.fixed(&self.model);
        o.u64(self.tsc_hz);
        o.u64(self.tsc);
        o.u64(self.started);
    }

    fn get(r: &mut In) -> Result<Vm> {
        let vendor = Vendor::from_code(r.u8("vendor")?)?;
        let uuid = r.fixed::<16>("uuid")?;
        let n = r.u8("ranges")? as usize;
        if n == 0 || n > RANGES_MAX {
            return Err(Error("a VM with no RAM, or more ranges of it than a VM has"));
        }
        let mut ranges = Vec::new();
        ranges.try_reserve_exact(n).map_err(|_| Error("no memory for the ranges"))?;
        let mut end = 0u64;
        for _ in 0..n {
            let base = r.u64("a range's base")?;
            let size = r.u64("a range's size")?;
            const PAGE: u64 = 4096;
            let top = base.checked_add(size).ok_or(Error("a range past the address space"))?;
            if size == 0 || base % PAGE != 0 || size % PAGE != 0 || base < end || top > 1 << 52 {
                return Err(Error("a range of RAM not whole pages, out of order, or past what an address reaches"));
            }
            end = top;
            ranges.push(Range { base, size });
        }
        let x2apic = r.bool("x2apic")?;
        let ioapic = r.bool("ioapic")?;
        let apicv = match r.u8("apicv")? {
            0 => None,
            1 => Some(false),
            2 => Some(true),
            _ => return Err(Error("apicv")),
        };
        let offloads = r.bool("offloads")?;
        let rx_moderation_ns = r.u64("receive moderation")?;
        if rx_moderation_ns > 1_000_000_000 {
            return Err(Error("a receive moderation of more than a second"));
        }
        let cpus = r.u32("cpus")?;
        if cpus == 0 || cpus as usize > CPUS_MAX || (ioapic && cpus < 2) {
            return Err(Error("a VM of no CPU, more than a state holds, or an IO-APIC with one"));
        }
        let shape = Shape { cpus, smt: r.bool("smt")?, llc: r.bool("llc")? };
        let model = r.fixed::<32>("model")?;
        let tsc_hz = r.u64("TSC rate")?;
        if tsc_hz == 0 {
            return Err(Error("a TSC of no rate"));
        }
        let tsc = r.u64("TSC")?;
        let started = r.u64("started")?;
        Ok(Vm { vendor, uuid, ranges, x2apic, ioapic, apicv, offloads, rx_moderation_ns, shape, model, tsc_hz, tsc,
                started })
    }

    /// Whether `[gpa, gpa + len)` is RAM of one range.
    pub fn holds(&self, gpa: u64, len: u64) -> bool {
        self.ranges.iter().any(|r| gpa >= r.base && len <= r.size && gpa - r.base <= r.size - len)
    }

    /// How many bytes of RAM, in all.
    pub fn ram_bytes(&self) -> u64 {
        self.ranges.iter().map(|r| r.size).sum()
    }
}
