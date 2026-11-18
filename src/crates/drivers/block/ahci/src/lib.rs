//! AHCI: SATA disks behind an AHCI host bus adapter -- the Supermicro
//! X11SSD-F's C236 (8086:a102), QEMU's q35 ICH9 -- through the HBA's
//! registers in its ABAR (BAR5). They are memory-mapped, so the driver is the
//! same on either architecture.
//!
//! The firmware's hold on the HBA is taken first: the BIOS/OS handoff where
//! the HBA has one, then a reset of the whole HBA, which stops every port's
//! DMA -- UEFI's own driver leaves its command lists and FIS areas pointing
//! at memory the kernel has handed out by then -- and CAP and PI written back
//! as they were, which some of Intel's lose to the reset. Each implemented
//! port then gets memory of its own: its command list, the area the HBA puts
//! the FISes it receives in, and a command table a slot (`disk::Queue`).
//!
//! A disk queues up to 31 commands at once where it and the HBA have NCQ --
//! READ and WRITE FPDMA QUEUED, a slot a tag -- and has one out at a time
//! where they do not: straight to and from the caller's buffer through the
//! slot's PRDT, up to 256 KiB a command; a batch's pieces (`read_pieces`,
//! `write_pieces`) that follow one another on the disk one command, the
//! batch's commands out together; an asynchronous request from the block
//! layer (`submit`) a command of its own. The HBA's MSI ends them (`Hba::
//! interrupt`); before there are interrupts to wait on -- at boot, at the
//! halt -- and on an HBA whose MSI does not come, whoever waits polls the
//! port. The disk's own commands -- a flush, a standby -- go with the queue
//! drained, on a slot of their own.
//!
//! A command that fails, times out or loses its link has the port taken
//! back (`AhciDisk::recover`, by the HBA's error-handling task or a waiter):
//! the command the disk names in its NCQ error log -- or the only one out,
//! or each found alone -- failed, the others that went with it sent again,
//! the port reset as AHCI 1.3.1 6.2.2 and 10.4.2 say. A port that does not
//! come back, or `ataproto::MAX_UNFINISHED` commands running that did not
//! finish, and the disk is failed: said once, and every request of it
//! refused at once from then on.
//!
//! Disks are named by where they are connected, `ada` the first HBA's port
//! 0, so that a name is a port whatever else is plugged in.
//!
//! Left for later: TRIM, port multipliers, hot plug, an Intel HBA in RAID
//! mode (class 0104), whose interface is AHCI too.

#![no_std]

extern crate alloc;

mod disk;
mod port;
mod regs;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ataproto::identify::{self, Identify, Rotation};
use block::{BlockDriver, Piece};
use kcore::block::{BlockIo, SubmitError};
use kcore::consts::PAGE_SIZE;
use kcore::dma::{DmaBuffer, PhysMapping};
use kcore::io::MmioRegion;
use kcore::klog;
use kcore::msix::{MsixInterrupt, MsixTable};
use kcore::once::Once;
use kcore::pci::{self, PciDevice};
use kcore::sync::{Event, TryLock};
use kcore::task::TaskHandle;
use kcore::time::{Deadline, Duration};

use disk::{bits, AhciDisk, Queue};
use port::{relax, wait, PortRegs};
use regs::*;

/// Disks named `ada`..`adz`, by their port's place across the HBAs.
const MAX_DISKS: usize = 26;
const MAX_HBAS: usize = 4;

static NEXT_POSITION: AtomicUsize = AtomicUsize::new(0);
static HBA_COUNT: AtomicUsize = AtomicUsize::new(0);
static HBAS: [Once<&'static Hba>; MAX_HBAS] = [const { Once::new() }; MAX_HBAS];

/// An HBA, for good: its registers, its disks, its interrupt and the task
/// that handles its disks' failures.
pub(crate) struct Hba {
    regs: MmioRegion,
    pub(crate) cap: u32,
    at: (u16, u16, u16),
    disks: [Once<&'static AhciDisk>; MAX_PORTS],
    msi: Once<MsixTable>,
    irq: Once<MsixInterrupt>,
    /// Its MSI is registered and was seen to come (`check_interrupt`):
    /// waiters may leave a command's end to it, and it has an asynchronous
    /// path
    irq_ok: AtomicBool,
    isr_count: AtomicU64,
    /// Interrupts found pending with no MSI taken for them (`look`)
    missed: AtomicU64,
    /// What wakes the error-handling task before its tick: a failure seen
    wake: Option<Event>,
    task: TryLock<Option<TaskHandle>>,
}

impl Hba {
    pub(crate) fn interrupts_work(&self) -> bool {
        self.irq_ok.load(Ordering::Acquire)
    }

    fn disks(&self) -> impl Iterator<Item = &'static AhciDisk> + '_ {
        self.disks.iter().filter_map(|d| d.get().copied())
    }

    /// The HBA's MSI. Interrupt context, maybe on two CPUs at once.
    fn interrupt(&'static self) {
        self.isr_count.fetch_add(1, Ordering::Release);
        self.take_interrupt(self.regs.read32(HBA_IS));
    }

    /// What an interrupt is taken as: each port in `is` has its commands
    /// ended -- which clears its PxIS first, as 10.7.2 has it -- then IS;
    /// the error handler woken where a port saw a failure. What it touches
    /// is the disks' locks' and atomics, so the ISR and the error handler's
    /// look (`look`) may take one at once.
    fn take_interrupt(&self, is: u32) {
        let mut wake = false;
        for i in bits(is) {
            if let Some(disk) = self.disks.get(i).and_then(|d| d.get()) {
                wake |= disk.reap();
            }
        }
        if is != 0 {
            self.regs.write32(HBA_IS, is);
        }
        if wake {
            if let Some(event) = &self.wake {
                event.signal();
            }
        }
    }

    /// The error handler's look at IS: an interrupt pending that no MSI is
    /// taken for in MSI_GRACE_US -- one lost, or not let in by the CPU it
    /// went to, or IS held latched by an HBA that raises no other while it
    /// is -- said, and taken here as the ISR would take it: its commands
    /// ended, and IS cleared, so that the ports' next interrupts come.
    /// Without it a lost MSI would leave every end after it to this task's
    /// tick, a tenth of a second each, and nobody told. What is said is
    /// exact: a bit counts only while it is set at every read of the grace,
    /// and all of it only while no MSI at all is taken -- an ISR under way as
    /// IS is first read (counted before `before`) clears what it read, an
    /// MSI on its way or for an event just come is taken in microseconds.
    fn look(&self) {
        if !self.interrupts_work() {
            return;
        }
        let before = self.isr_count.load(Ordering::Acquire);
        let mut pending = self.regs.read32(HBA_IS);
        if pending == 0 {
            return;
        }
        let mut grace = Deadline::after_us(MSI_GRACE_US);
        while !grace.passed() {
            pending &= self.regs.read32(HBA_IS);
            if pending == 0 || self.isr_count.load(Ordering::Acquire) != before {
                return;
            }
            core::hint::spin_loop();
        }
        let n = self.missed.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_power_of_two() {
            let (bus, slot, func) = self.at;
            klog!(0, "ahci: {:02x}:{:02x}.{}: an interrupt pending {} us (IS {:#x}) and no MSI taken for it: taken \
                      by the error handler instead ({} times now)", bus, slot, func, MSI_GRACE_US, pending, n);
        }
        self.take_interrupt(pending);
    }

    /// Whether the MSI comes: a command that changes nothing, seen done by
    /// polling, and its interrupt waited for. One that never comes is said,
    /// and the HBA polled from then on -- with no asynchronous path, whose
    /// ends would otherwise wait for the error handler's tick.
    fn check_interrupt(&'static self) {
        let Some(disk) = self.disks().next() else { return };
        let before = self.isr_count.load(Ordering::Acquire);
        if !disk.check_power_mode() {
            return;
        }
        let mut deadline = Deadline::after_ms(INTERRUPT_CHECK_MS);
        while self.isr_count.load(Ordering::Acquire) == before && !deadline.passed() {
            kcore::task::yield_to_runnable();
        }
        let (bus, slot, func) = self.at;
        if self.isr_count.load(Ordering::Acquire) == before {
            self.irq_ok.store(false, Ordering::Release);
            for d in self.disks() {
                d.port.write(PX_IE, 0);
            }
            klog!(0, "ahci: {:02x}:{:02x}.{}: the HBA's MSI never came for a command it finished: its disks are \
                      polled from here on, with no asynchronous path", bus, slot, func);
        } else {
            klog!(1, "ahci: {:02x}:{:02x}.{}: its MSI comes", bus, slot, func);
        }
    }
}

/// The error-handling task: woken by a failure the ISR saw, or every
/// EH_TICK_MS -- for a command out past its time, and for an interrupt that
/// never came (`Hba::look`) -- until it is told to stop.
fn eh_main(hba: &'static Hba) {
    while !kcore::task::stopping() {
        match &hba.wake {
            Some(event) => {
                let _ = event.wait_for(Duration::from_millis(EH_TICK_MS));
            }
            None => kcore::task::sleep_ms(EH_TICK_MS),
        }
        hba.look();
        for disk in hba.disks() {
            disk.reap();
            disk.service();
        }
    }
}

/* ---- bring-up ---- */

/// The HBA the firmware leaves: the handoff where it has one, AHCI mode, a
/// reset of the whole HBA, AHCI mode again and CAP and PI as they were. Its
/// CAP and PI, or None for an HBA that would not reset.
fn take_hba(regs: &MmioRegion, at: (u16, u16, u16)) -> Option<(u32, u32)> {
    let (bus, slot, func) = at;
    let cap = regs.read32(HBA_CAP);
    let pi = regs.read32(HBA_PI);
    let version = regs.read32(HBA_VS);

    if version >= VS_1_2 && regs.read32(HBA_CAP2) & CAP2_BOH != 0 {
        regs.write32(HBA_BOHC, regs.read32(HBA_BOHC) | BOHC_OOS);
        let released = wait(HANDOFF_MS, || regs.read32(HBA_BOHC) & BOHC_BOS == 0)
            || (regs.read32(HBA_BOHC) & BOHC_BB != 0
                && wait(HANDOFF_BUSY_MS, || regs.read32(HBA_BOHC) & BOHC_BOS == 0));
        if !released {
            klog!(0, "ahci: {:02x}:{:02x}.{}: the firmware did not hand the HBA over: taken all the same", bus, slot,
                func);
        }
    }

    regs.write32(HBA_GHC, regs.read32(HBA_GHC) | GHC_AE);
    regs.write32(HBA_GHC, GHC_AE | GHC_HR);
    if !wait(HBA_RESET_MS, || regs.read32(HBA_GHC) & GHC_HR == 0) {
        klog!(0, "ahci: {:02x}:{:02x}.{}: the HBA did not come out of its reset", bus, slot, func);
        return None;
    }
    /* AHCI mode on again, no interrupt until the disks are there to take it
     * (`setup_interrupt`), and what firmware set in CAP and PI back where the
     * reset took it: both are write-once on Intel's, set by the BIOS, and
     * some reset them */
    regs.write32(HBA_GHC, GHC_AE);
    if regs.read32(HBA_CAP) != cap {
        regs.write32(HBA_CAP, cap);
    }
    if regs.read32(HBA_PI) != pi {
        regs.write32(HBA_PI, pi);
    }
    Some((cap, pi))
}

/// A port made ready to find its device: stopped, its command list and FIS
/// area given it, its FIS receive on, spun up where the HBA staggers that,
/// its link kept from power states. Every port is this first, so that all
/// the links come up together and are waited for once (`finish_port`).
/// False where the port cannot be taken.
fn prepare_port(port: &PortRegs, queue: &Queue, name: (u16, u16, u16, u32)) -> bool {
    let (bus, slot, func, index) = name;
    if !port.stop() {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: would not stop", bus, slot, func, index);
        return false;
    }
    let clb = queue.command_list();
    let fb = queue.fis_area();
    port.write(PX_CLB, clb as u32);
    port.write(PX_CLBU, (clb >> 32) as u32);
    port.write(PX_FB, fb as u32);
    port.write(PX_FBU, (fb >> 32) as u32);
    port.write(PX_IE, 0);
    port.clear_errors();

    let mut command = port.read(PX_CMD) & !CMD_ALPE;
    if port.cap & CAP_SSS != 0 {
        command |= CMD_SUD;
    }
    if command & CMD_CPD != 0 {
        command |= CMD_POD;
    }
    port.write(PX_CMD, command | CMD_FRE);
    let control = port.read(PX_SCTL);
    port.write(PX_SCTL, (control & !(SCTL_IPM_MASK | SCTL_DET_MASK)) | SCTL_IPM_NONE);
    true
}

/// A prepared port's link -- waited for as long as `links` has left, the
/// one wait every port of the HBA shares -- and its device: an ATA disk,
/// out of its reset, the port started. False, said where there is something
/// to say, for anything else.
fn finish_port(port: &PortRegs, name: (u16, u16, u16, u32), links: &mut Deadline) -> bool {
    let (bus, slot, func, index) = name;
    while !port.linked() && !links.passed() {
        relax();
    }
    if !port.linked() {
        let det = port.read(PX_SSTS) & SSTS_DET_MASK;
        if det == 0 || !port.comreset() {
            klog!(1, "ahci: {:02x}:{:02x}.{} port {}: no link (ssts {:#x})", bus, slot, func, index,
                port.read(PX_SSTS));
            let _ = port.stop();
            return false;
        }
    }
    if !wait(DEVICE_READY_MS, || port.ready()) {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: the device stays busy (tfd {:#x})", bus, slot, func, index,
            port.read(PX_TFD));
        let _ = port.stop();
        return false;
    }
    let signature = port.read(PX_SIG);
    if signature != SIG_ATA {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: {} (signature {:#x}) passed over", bus, slot, func, index,
            if signature == SIG_ATAPI { "a packet device" } else { "not an ATA disk" }, signature);
        let _ = port.stop();
        return false;
    }
    port.clear_errors();
    if !port.start() {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: would not start", bus, slot, func, index);
        let _ = port.stop();
        return false;
    }
    true
}

/// The HBA's MSI, once its disks are there to take it: registered, every
/// port asked to interrupt for its commands' ends and failures, the HBA's
/// interrupt on. An HBA with no MSI -- or none to be had -- is polled.
fn setup_interrupt(dev: &PciDevice, hba: &'static Hba) {
    let (bus, slot, func) = hba.at;
    let Some(table) = MsixTable::new_msi(dev) else {
        klog!(0, "ahci: {:02x}:{:02x}.{}: no MSI to be had: its disks are polled", bus, slot, func);
        return;
    };
    if hba.msi.set(table).is_err() {
        return;
    }
    let Some(table) = hba.msi.get() else { return };
    let Some(irq) = MsixInterrupt::register_for(table, 0, hba, Hba::interrupt) else {
        klog!(0, "ahci: {:02x}:{:02x}.{}: its MSI would not register: its disks are polled", bus, slot, func);
        return;
    };
    let _ = hba.irq.set(irq);
    for disk in hba.disks() {
        disk.port.write(PX_IS, u32::MAX);
        disk.port.write(PX_IE, IE_ALL);
    }
    hba.regs.write32(HBA_IS, u32::MAX);
    hba.regs.write32(HBA_GHC, hba.regs.read32(HBA_GHC) | GHC_IE);
    hba.irq_ok.store(true, Ordering::Release);
}

fn register(disk: &'static AhciDisk) {
    let speed = (disk.port.read(PX_SSTS) >> SSTS_SPD_SHIFT) & SSTS_SPD_MASK;
    let mut model = [0u8; 40];
    let model_len = identify::printable(identify::trimmed(&disk.id.model), &mut model);
    let model = core::str::from_utf8(&model[..model_len]).unwrap_or("?");
    let mut firmware = [0u8; 8];
    let firmware_len = identify::printable(identify::trimmed(&disk.id.firmware), &mut firmware);
    let firmware = core::str::from_utf8(&firmware[..firmware_len]).unwrap_or("?");
    let spins = match disk.id.rotation {
        Rotation::Solid => String::from("solid state"),
        Rotation::NotReported => String::from("rotation not said"),
        Rotation::Rpm(rpm) => alloc::format!("{} rpm", rpm),
    };
    let link = match speed {
        1 => "1.5 Gbps",
        2 => "3 Gbps",
        3 => "6 Gbps",
        _ => "speed not said",
    };
    let queue = if disk.depth() > 1 {
        alloc::format!("NCQ {} deep", disk.depth())
    } else {
        String::from("a command at a time")
    };
    let (bus, slot, func, port_index) = disk.at;

    match block::register_driver(disk.name(), disk) {
        Some(_) => {
            klog!(0, "ahci: {} at {:02x}:{:02x}.{} port {}: {} ({}), {} sectors of {} bytes (physical {}), {}, \
                      write cache {}, {}{}{}, link {}, {}, {}",
                disk.name(), bus, slot, func, port_index, model, firmware, disk.id.sectors, disk.id.sector_size,
                disk.id.physical_sector_size, spins,
                match disk.id.write_cache { Some(true) => "on", Some(false) => "off", None => "none" },
                if disk.id.lba48 { "LBA48" } else { "LBA28" },
                if disk.id.trim { ", TRIM" } else { "" },
                if disk.id.fua { ", FUA" } else { "" },
                link, queue, if disk.hba.interrupts_work() { "MSI" } else { "polled" });
        }
        None => klog!(0, "ahci: the device table would not take {}", disk.name()),
    }
}

fn take(dev: &PciDevice) {
    let at = (dev.bus, dev.slot, dev.func);
    let bar = dev.get_bar(ABAR);
    let phys = u64::from(bar & BAR_ADDRESS);
    if bar & BAR_IO != 0 || phys == 0 {
        klog!(0, "ahci: {:02x}:{:02x}.{}: no memory BAR 5 (ABAR {:#x})", dev.bus, dev.slot, dev.func, bar);
        return;
    }
    let command = dev.read_config16(PCI_COMMAND);
    if command & PCI_COMMAND_MEMORY == 0 {
        dev.write_config16(PCI_COMMAND, command | PCI_COMMAND_MEMORY);
    }
    dev.enable_bus_mastering();

    /* Enough for every port the HBA can have -- 0x100 and 0x80 a port, 32 --
     * from the page the ABAR is in: it need only be 2 KiB aligned, and a
     * mapping starts at a page */
    let page = phys & !(PAGE_SIZE as u64 - 1);
    let offset = (phys - page) as usize;
    let pages = (offset + PORTS_BASE + MAX_PORTS * PORT_STRIDE).div_ceil(PAGE_SIZE);
    let Some(mapping) = PhysMapping::map(page, pages) else {
        klog!(0, "ahci: {:02x}:{:02x}.{}: ABAR {:#x} would not map", dev.bus, dev.slot, dev.func, phys);
        return;
    };
    let regs = MmioRegion::new(mapping.as_mut_ptr().wrapping_add(offset), pages * PAGE_SIZE - offset);
    /* For good: the ports' registers are windows of it */
    let mapping: &'static PhysMapping = Box::leak(Box::new(mapping));
    let _ = mapping;

    let Some((cap, pi)) = take_hba(&regs, at) else { return };
    let ports = (cap & CAP_NP_MASK) as usize + 1;
    let slots = ((cap >> CAP_NCS_SHIFT) & CAP_NCS_MASK) as usize + 1;
    klog!(0, "ahci: HBA {:04x}:{:04x} at {:02x}:{:02x}.{}, version {:#x}, {} ports (implemented {:#x}), {} slots, \
              {}, {}, {}",
        dev.vendor, dev.device, dev.bus, dev.slot, dev.func, regs.read32(HBA_VS), ports, pi, slots,
        if cap & CAP_SNCQ != 0 { "NCQ" } else { "no NCQ" },
        if cap & CAP_S64A != 0 { "64-bit" } else { "32-bit" },
        if cap & CAP_SSS != 0 { "staggered spin-up" } else { "no staggered spin-up" });

    let index = HBA_COUNT.fetch_add(1, Ordering::Relaxed);
    let hba: &'static Hba = Box::leak(Box::new(Hba {
        regs,
        cap,
        at,
        disks: [const { Once::new() }; MAX_PORTS],
        msi: Once::new(),
        irq: Once::new(),
        irq_ok: AtomicBool::new(false),
        isr_count: AtomicU64::new(0),
        missed: AtomicU64::new(0),
        wake: Event::new(),
        task: TryLock::new(None),
    }));
    if let Some(entry) = HBAS.get(index) {
        let _ = entry.set(hba);
    }

    let base = NEXT_POSITION.fetch_add(ports, Ordering::Relaxed);
    let mut prepared: Vec<(usize, PortRegs, Queue)> = Vec::new();
    for index in 0..ports.min(MAX_PORTS) {
        if pi & (1 << index) == 0 {
            continue;
        }
        let Some(window) = hba.regs.window(PORTS_BASE + index * PORT_STRIDE) else { continue };
        let Some(queue) = Queue::new(slots, cap & CAP_S64A != 0) else {
            klog!(0, "ahci: {:02x}:{:02x}.{} port {}: no memory for its command list and tables, or none the HBA \
                      reaches", dev.bus, dev.slot, dev.func, index);
            continue;
        };
        let port = PortRegs::new(window, cap);
        if prepare_port(&port, &queue, (dev.bus, dev.slot, dev.func, index as u32)) && prepared.try_reserve(1).is_ok() {
            prepared.push((index, port, queue));
        }
    }

    /* One wait for every port's link: a port with nothing on it waits
     * out what is left of it, not a second of its own */
    let mut links = Deadline::after_ms(LINK_MS);
    let mut found: Vec<&'static AhciDisk> = Vec::new();
    for (index, port, mut queue) in prepared {
        let name = (dev.bus, dev.slot, dev.func, index as u32);
        if !finish_port(&port, name, &mut links) {
            continue;
        }
        let id = match disk::identify(&port, &mut queue) {
            Ok(words) => match Identify::parse(&words, PAGE_SIZE) {
                Ok(id) => id,
                Err(why) => {
                    klog!(0, "ahci: {:02x}:{:02x}.{} port {}: not taken: {:?}", dev.bus, dev.slot, dev.func, index, why);
                    let _ = port.stop();
                    continue;
                }
            },
            Err(fault) => {
                klog!(0, "ahci: {:02x}:{:02x}.{} port {}: IDENTIFY {}", dev.bus, dev.slot, dev.func, index,
                    fault.describe());
                let _ = port.stop();
                continue;
            }
        };
        let position = base + index;
        if position >= MAX_DISKS || found.try_reserve(1).is_err() {
            klog!(0, "ahci: no room for another disk");
            let _ = port.stop();
            continue;
        }
        let Some(disk) = AhciDisk::new(hba, port, queue, slots, name, id, [b'a', b'd', b'a' + position as u8]) else {
            klog!(0, "ahci: no memory for a disk's lock");
            continue;
        };
        /* For good: the ISR and the block table are pointed at it */
        let disk: &'static AhciDisk = Box::leak(Box::new(disk));
        if let Some(entry) = hba.disks.get(index) {
            let _ = entry.set(disk);
        }
        found.push(disk);
    }

    setup_interrupt(dev, hba);
    for disk in found {
        register(disk);
    }
}

/// The AHCI HBAs, their ports and the disks on them. From `rust_init`,
/// before the second partition probe.
pub fn init() {
    for i in 0..pci::device_count() {
        let Some(dev) = pci::get_device(i) else { continue };
        if dev.class != CLASS_STORAGE || dev.subclass != SUBCLASS_SATA || dev.prog_if != PROGIF_AHCI {
            continue;
        }
        if HBA_COUNT.load(Ordering::Relaxed) >= MAX_HBAS {
            klog!(0, "ahci: {:02x}:{:02x}.{}: past the {} HBAs taken", dev.bus, dev.slot, dev.func, MAX_HBAS);
            break;
        }
        take(&dev);
    }
}

fn hbas() -> impl Iterator<Item = &'static Hba> {
    HBAS.iter().filter_map(|h| h.get().copied())
}

/// Once tasks run (`rust_ahci_start`): each HBA's MSI checked, and its
/// error-handling task started.
pub fn start() {
    for hba in hbas() {
        if hba.interrupts_work() {
            hba.check_interrupt();
        }
        let Some(mut task) = hba.task.try_lock() else { continue };
        if task.is_some() {
            continue;
        }
        match kcore::task::spawn_for("ahci", hba, eh_main) {
            Some(handle) => *task = Some(handle),
            None => {
                let (bus, slot, func) = hba.at;
                klog!(0, "ahci: {:02x}:{:02x}.{}: no task for its failures: a waiter takes the port back, an \
                          asynchronous request's only when one waits", bus, slot, func);
            }
        }
    }
}

/// Before the halt (`rust_ahci_stop`), once nothing writes the disks but the
/// flush and the standby to come: each HBA's error-handling task stopped and
/// waited for. From here on its disks' waiters take a failure back
/// themselves.
pub fn stop() {
    for hba in hbas() {
        let taken = hba.task.try_lock().and_then(|mut task| task.take());
        if let Some(task) = taken {
            task.request_stop();
            if let Some(event) = &hba.wake {
                event.signal();
            }
            drop(task);
        }
    }
}

/// On the way down: every disk's cache flushed -- and with `power_off` the
/// disk put in standby, as the power is about to go: not for a reboot,
/// which leaves an HDD spinning to come back up the sooner -- and every port
/// stopped, so that the HBA does nothing more with memory the kernel is done
/// with; the HBA's interrupt off. Interrupts are off here: every wait polls.
/// A disk given up on is skipped: its port is stopped already.
pub fn shutdown(power_off: bool) {
    for hba in hbas() {
        for disk in hba.disks() {
            if disk.failed() {
                klog!(0, "ahci: {}: failed, so neither flushed nor put in standby", disk.name());
                continue;
            }
            if !disk.flush_cache() {
                klog!(0, "ahci: {}: the write cache was not flushed on the way down", disk.name());
            }
            if power_off {
                if disk.standby() {
                    klog!(0, "ahci: {}: in standby for the power-off", disk.name());
                } else {
                    klog!(0, "ahci: {}: did not go to standby for the power-off", disk.name());
                }
            }
            disk.port.write(PX_IE, 0);
            if !disk.port.stop() {
                klog!(0, "ahci: {}: the port did not stop", disk.name());
            }
        }
        hba.regs.write32(HBA_GHC, hba.regs.read32(HBA_GHC) & !GHC_IE);
    }
}

impl BlockDriver for AhciDisk {
    fn capacity(&self) -> u64 {
        self.id.sectors
    }

    fn sector_size(&self) -> u64 {
        u64::from(self.id.sector_size)
    }

    fn read(&'static self, sector: u64, buf: &mut [u8]) -> bool {
        self.transfer(sector, buf.as_mut_ptr() as usize, buf.len(), false)
    }

    /* FUA as a flush after the write: a flush is what makes it durable */
    fn write(&'static self, sector: u64, data: &[u8], fua: bool) -> bool {
        self.transfer(sector, data.as_ptr() as usize, data.len(), true) && (!fua || self.flush_cache())
    }

    fn write_pieces(&'static self, base: u64, buf: &DmaBuffer, pieces: &[Piece]) -> bool {
        self.batch(true, buf.phys(), base, pieces)
    }

    fn read_pieces(&'static self, base: u64, buf: &mut DmaBuffer, pieces: &[Piece]) -> bool {
        self.batch(false, buf.phys(), base, pieces)
    }

    fn flush(&'static self) -> bool {
        self.flush_cache()
    }

    /* The asynchronous path is the MSI's: its ends are told from the ISR */
    fn is_async(&self) -> bool {
        self.hba.interrupts_work()
    }

    fn submit(&'static self, io: &BlockIo, kick: bool) -> Result<(), SubmitError> {
        self.submit_io(io, kick)
    }

    fn kick(&'static self) {
        self.ring_owed();
    }

    fn submit_max(&self) -> usize {
        ASYNC_MAX
    }

    fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }
}
