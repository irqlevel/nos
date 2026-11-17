//! A guest's platform: every device it has, as the guest left it -- drained
//! of the host's work first, nothing in flight.

use alloc::vec::Vec;

use crate::wire::{list, In, Out};
use crate::{Error, Result};

/// The IO-APIC's pins.
pub const IOAPIC_PINS: usize = 24;
/// A NIC's receive steering table's entries.
pub const FLOWS: usize = 256;
/// The most queues a virtio device has: a NIC's eight pairs and its control
/// queue, with room.
pub const QUEUES_MAX: usize = 32;
/// The most PCI functions on the bus, the host bridge's among them.
pub const FUNCTIONS_MAX: usize = 16;
/// The most virtio devices.
pub const DEVICES_MAX: usize = FUNCTIONS_MAX - 1;
/// The longest answer the UART keeps to a cursor-position query.
pub const REPLY_MAX: usize = 12;

/// The 16550A.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Uart {
    pub dll: u8,
    pub dlm: u8,
    pub ier: u8,
    pub lcr: u8,
    pub mcr: u8,
    pub scr: u8,
    pub written: u64,
    pub loopback: Option<u8>,
    pub rx: Option<u8>,
    /// Where the guest's output is in an escape sequence (0-4), and the
    /// column it left the cursor at.
    pub esc: u8,
    pub col: u16,
    /// The answer to the last cursor-position query -- its first
    /// `reply_len` bytes -- and how much of it the guest has read.
    pub reply: [u8; REPLY_MAX],
    pub reply_len: u8,
    pub reply_pos: u8,
    pub prompt_seen: bool,
    pub thri: bool,
    pub pulse: bool,
}

/// One 8259.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PicChip {
    pub irr: u8,
    pub isr: u8,
    pub imr: u8,
    pub vector_base: u8,
    pub init_step: u8,
    pub expect_icw4: bool,
    pub read_isr: bool,
}

/// The 8259 pair, and the edge/level control registers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pic {
    pub master: PicChip,
    pub slave: PicChip,
    pub elcr: u16,
}

/// One of the PIT's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PitChannel {
    pub reload: u16,
    pub access: u8,
    pub mode: u8,
    /// For lo/hi access, whether the high byte comes next.
    pub write_high: bool,
    pub read_high: bool,
    pub low: u8,
    pub latched: Option<u16>,
    pub loaded: bool,
    pub counted: u64,
    /// How long before the pause it began counting, or None while it does
    /// not count.
    pub counting_for_ns: Option<u64>,
}

/// The 8254.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pit {
    pub ch: [PitChannel; 3],
    pub port61: u8,
    pub ch0_edges_given: u64,
    pub ch0_armed: bool,
}

/// The RTC's CMOS: the clock itself is the wall clock, read again where the
/// guest is restored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rtc {
    pub index: u8,
    pub ram: [u8; 128],
}

/// ACPI's fixed hardware: PM1's registers, and its timer as how long it had
/// counted at the pause.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pm {
    pub status: u16,
    pub enable: u16,
    pub control: u16,
    pub elapsed_ns: u64,
    pub carries_seen: u64,
    pub scis: u64,
    pub presses: u32,
    pub used: bool,
    pub other_sleep: Option<u8>,
}

/// pvpanic's register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PvPanic {
    pub crash_loaded: u32,
}

/// The IO-APIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoApic {
    pub id: u32,
    pub select: u32,
    pub rte: [u64; IOAPIC_PINS],
    pub lines: u32,
    pub remote_irr: u32,
}

/// What a guest wrote of a PCI function's configuration -- what it may
/// write: the rest is the device's, which the host that restores it has
/// built alike and keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Function {
    pub command: u16,
    pub bar0: u32,
    pub bar1: u32,
    pub interrupt_line: u8,
    pub msix_control: u16,
}

/// A split virtqueue, as its device keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    pub size: u16,
    /// Its rings' page frame number; 0 while it is not set up.
    pub pfn: u32,
    pub next_avail: u16,
    pub next_used: u16,
    pub seen: u16,
    pub signalled_used: Option<u16>,
}

/// A function's MSI-X vectors, as the virtio header has them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Msix {
    pub entries: u16,
    pub enabled: bool,
    pub config: u16,
    pub queues: Vec<u16>,
}

/// Virtio's legacy transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transport {
    pub device_features: u32,
    pub driver_features: u32,
    pub status: u8,
    pub isr: u8,
    pub select: u16,
    pub queues: Vec<Queue>,
    pub msix: Option<Msix>,
}

/// A virtio disk, drained: no request with its backend, none parked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blk {
    pub transport: Transport,
    /// Its ring found one it will not follow (`hv::devices::virtio::Broken`),
    /// by its code: it takes nothing more until the driver resets it.
    pub broken: Option<u8>,
}

/// What a NIC's backend was told the guest takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Takes {
    pub partial: bool,
    pub whole: bool,
}

/// A virtio NIC, drained: no frame held for want of a buffer, no interrupt
/// owed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Net {
    pub transport: Transport,
    pub mac: [u8; 6],
    pub pairs: u16,
    pub active: u16,
    pub offloads: bool,
    pub mtu: u16,
    pub flows: [u8; FLOWS],
    pub told: Takes,
    pub broken: Option<u8>,
}

/// A device on the PCI bus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Device {
    Disk(Blk),
    Nic(Net),
}

/// The guest's devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub uart: Uart,
    pub pic: Pic,
    pub pit: Pit,
    pub rtc: Rtc,
    pub pm: Pm,
    pub pvpanic: PvPanic,
    pub ioapic: Option<IoApic>,
    /// The configuration address register (0xCF8) as last written.
    pub pci_address: u32,
    /// Every function, its slot its index: the host bridge's, then each
    /// device's in `devices`' order.
    pub functions: Vec<Function>,
    pub devices: Vec<Device>,
    /// Each device's MSI-X entries that came while masked, a bit each.
    pub msix_pending: Vec<u32>,
}

impl Uart {
    fn put(&self, o: &mut Out) {
        for b in [self.dll, self.dlm, self.ier, self.lcr, self.mcr, self.scr] {
            o.u8(b);
        }
        o.u64(self.written);
        o.opt(self.loopback, |o, b| o.u8(b));
        o.opt(self.rx, |o, b| o.u8(b));
        o.u8(self.esc);
        o.u16(self.col);
        o.u8(self.reply_len);
        o.fixed(&self.reply);
        o.u8(self.reply_pos);
        o.bool(self.prompt_seen);
        o.bool(self.thri);
        o.bool(self.pulse);
    }

    fn get(r: &mut In) -> Result<Uart> {
        let regs = r.fixed::<6>("the UART's registers")?;
        let written = r.u64("the UART's count")?;
        let loopback = r.opt("the UART's loopback", |r| r.u8("the UART's loopback"))?;
        let rx = r.opt("the UART's receiver", |r| r.u8("the UART's receiver"))?;
        let esc = r.u8("the UART's escape")?;
        if esc > 4 {
            return Err(Error("the UART in no escape state it has"));
        }
        let col = r.u16("the UART's column")?;
        let reply_len = r.u8("the UART's reply")?;
        let reply = r.fixed::<REPLY_MAX>("the UART's reply")?;
        let reply_pos = r.u8("the UART's reply")?;
        if usize::from(reply_len) > REPLY_MAX || reply_pos > reply_len {
            return Err(Error("a reply longer than the UART keeps, or read past its end"));
        }
        Ok(Uart {
            dll: regs[0], dlm: regs[1], ier: regs[2], lcr: regs[3], mcr: regs[4], scr: regs[5], written, loopback, rx,
            esc, col, reply, reply_len, reply_pos, prompt_seen: r.bool("prompt")?, thri: r.bool("THRI")?, pulse: r.bool("pulse")?,
        })
    }
}

impl PicChip {
    fn put(&self, o: &mut Out) {
        for b in [self.irr, self.isr, self.imr, self.vector_base, self.init_step] {
            o.u8(b);
        }
        o.bool(self.expect_icw4);
        o.bool(self.read_isr);
    }

    fn get(r: &mut In) -> Result<PicChip> {
        let b = r.fixed::<5>("an 8259's registers")?;
        if b[4] > 4 {
            return Err(Error("an 8259 at no step of its initialisation"));
        }
        Ok(PicChip { irr: b[0], isr: b[1], imr: b[2], vector_base: b[3], init_step: b[4],
                     expect_icw4: r.bool("ICW4")?, read_isr: r.bool("read ISR")? })
    }
}

impl PitChannel {
    fn put(&self, o: &mut Out) {
        o.u16(self.reload);
        o.u8(self.access);
        o.u8(self.mode);
        o.bool(self.write_high);
        o.bool(self.read_high);
        o.u8(self.low);
        o.opt(self.latched, |o, v| o.u16(v));
        o.bool(self.loaded);
        o.u64(self.counted);
        o.opt(self.counting_for_ns, |o, v| o.u64(v));
    }

    fn get(r: &mut In) -> Result<PitChannel> {
        let reload = r.u16("a PIT count")?;
        let access = r.u8("a PIT access")?;
        let mode = r.u8("a PIT mode")?;
        if !(1..=3).contains(&access) || mode > 7 {
            return Err(Error("a PIT channel in an access or mode the 8254 has not"));
        }
        Ok(PitChannel {
            reload, access, mode, write_high: r.bool("PIT half")?, read_high: r.bool("PIT half")?,
            low: r.u8("PIT low")?, latched: r.opt("PIT latch", |r| r.u16("PIT latch"))?, loaded: r.bool("PIT loaded")?,
            counted: r.u64("PIT ticks")?, counting_for_ns: r.opt("PIT start", |r| r.u64("PIT start"))?,
        })
    }
}

impl Function {
    fn put(&self, o: &mut Out) {
        o.u16(self.command);
        o.u32(self.bar0);
        o.u32(self.bar1);
        o.u8(self.interrupt_line);
        o.u16(self.msix_control);
    }

    fn get(r: &mut In) -> Result<Function> {
        Ok(Function { command: r.u16("a command register")?, bar0: r.u32("a BAR")?, bar1: r.u32("a BAR")?,
                      interrupt_line: r.u8("an interrupt line")?, msix_control: r.u16("MSI-X's control")? })
    }
}

impl Queue {
    fn put(&self, o: &mut Out) {
        o.u16(self.size);
        o.u32(self.pfn);
        o.u16(self.next_avail);
        o.u16(self.next_used);
        o.u16(self.seen);
        o.opt(self.signalled_used, |o, v| o.u16(v));
    }

    fn get(r: &mut In) -> Result<Queue> {
        let size = r.u16("a queue's size")?;
        if !size.is_power_of_two() || size > 32768 {
            return Err(Error("a queue whose size is no power of two, or past the most"));
        }
        let pfn = r.u32("a queue's address")?;
        Ok(Queue { size, pfn, next_avail: r.u16("a queue's index")?, next_used: r.u16("a queue's index")?,
                   seen: r.u16("a queue's index")?,
                   signalled_used: r.opt("a queue's signal", |r| r.u16("a queue's signal"))? })
    }
}

impl Transport {
    fn put(&self, o: &mut Out) {
        o.u32(self.device_features);
        o.u32(self.driver_features);
        o.u8(self.status);
        o.u8(self.isr);
        o.u16(self.select);
        o.u8(self.queues.len() as u8);
        for q in &self.queues {
            q.put(o);
        }
        o.opt(self.msix.as_ref(), |o, m| {
            o.u16(m.entries);
            o.bool(m.enabled);
            o.u16(m.config);
            for v in &m.queues {
                o.u16(*v);
            }
        });
    }

    fn get(r: &mut In) -> Result<Transport> {
        let device_features = r.u32("a device's features")?;
        let driver_features = r.u32("a driver's features")?;
        if driver_features & !device_features != 0 {
            return Err(Error("a driver taking features its device did not offer"));
        }
        let status = r.u8("a device's status")?;
        let isr = r.u8("a device's interrupt status")?;
        let select = r.u16("a queue select")?;
        let n = usize::from(r.u8("a device's queues")?);
        let queues = list(r, n, QUEUES_MAX, "a device's queues", Queue::get)?;
        let msix = r.opt("MSI-X", |r| {
            let entries = r.u16("MSI-X's entries")?;
            if entries == 0 || entries > 2048 {
                return Err(Error("MSI-X with no entry, or past a table's most"));
            }
            let enabled = r.bool("MSI-X on")?;
            let config = r.u16("a vector")?;
            let queues = list(r, n, QUEUES_MAX, "vectors", |r| r.u16("a vector"))?;
            let valid = |v: u16| v == u16::MAX || v < entries;
            if !valid(config) || !queues.iter().all(|&v| valid(v)) {
                return Err(Error("a vector past MSI-X's table"));
            }
            Ok(Msix { entries, enabled, config, queues })
        })?;
        Ok(Transport { device_features, driver_features, status, isr, select, queues, msix })
    }
}

impl Device {
    fn put(&self, o: &mut Out) {
        match self {
            Device::Disk(b) => {
                o.u8(1);
                b.transport.put(o);
                o.opt(b.broken, |o, v| o.u8(v));
            }
            Device::Nic(n) => {
                o.u8(2);
                n.transport.put(o);
                o.fixed(&n.mac);
                o.u16(n.pairs);
                o.u16(n.active);
                o.bool(n.offloads);
                o.u16(n.mtu);
                o.fixed(&n.flows);
                o.bool(n.told.partial);
                o.bool(n.told.whole);
                o.opt(n.broken, |o, v| o.u8(v));
            }
        }
    }

    fn get(r: &mut In) -> Result<Device> {
        match r.u8("a device's kind")? {
            1 => Ok(Device::Disk(Blk { transport: Transport::get(r)?,
                                       broken: r.opt("a ring broken", |r| r.u8("a ring broken"))? })),
            2 => {
                let transport = Transport::get(r)?;
                let mac = r.fixed::<6>("a MAC")?;
                let pairs = r.u16("a NIC's pairs")?;
                let active = r.u16("a NIC's pairs used")?;
                if pairs == 0 || active == 0 || active > pairs || usize::from(pairs) * 2 >= QUEUES_MAX {
                    return Err(Error("a NIC of no queue pair, or using more than it has"));
                }
                let offloads = r.bool("offloads")?;
                let mtu = r.u16("an MTU")?;
                let flows = r.fixed::<FLOWS>("the steering table")?;
                /* A flow's pair is learned one-based, 0 for none. */
                if flows.iter().any(|&q| u16::from(q) > pairs) {
                    return Err(Error("a flow steered to a pair the NIC has not"));
                }
                let told = Takes { partial: r.bool("takes")?, whole: r.bool("takes")? };
                let broken = r.opt("a ring broken", |r| r.u8("a ring broken"))?;
                Ok(Device::Nic(Net { transport, mac, pairs, active, offloads, mtu, flows, told, broken }))
            }
            _ => Err(Error("a device of a kind there is none of")),
        }
    }
}

impl Platform {
    pub(crate) fn put(&self, o: &mut Out) {
        self.uart.put(o);
        self.pic.master.put(o);
        self.pic.slave.put(o);
        o.u16(self.pic.elcr);
        for c in &self.pit.ch {
            c.put(o);
        }
        o.u8(self.pit.port61);
        o.u64(self.pit.ch0_edges_given);
        o.bool(self.pit.ch0_armed);
        o.u8(self.rtc.index);
        o.fixed(&self.rtc.ram);
        let p = &self.pm;
        o.u16(p.status);
        o.u16(p.enable);
        o.u16(p.control);
        o.u64(p.elapsed_ns);
        o.u64(p.carries_seen);
        o.u64(p.scis);
        o.u32(p.presses);
        o.bool(p.used);
        o.opt(p.other_sleep, |o, v| o.u8(v));
        o.u32(self.pvpanic.crash_loaded);
        o.opt(self.ioapic.as_ref(), |o, a| {
            o.u32(a.id);
            o.u32(a.select);
            for e in a.rte {
                o.u64(e);
            }
            o.u32(a.lines);
            o.u32(a.remote_irr);
        });
        o.u32(self.pci_address);
        o.u8(self.functions.len() as u8);
        for f in &self.functions {
            f.put(o);
        }
        o.u8(self.devices.len() as u8);
        for d in &self.devices {
            d.put(o);
        }
        for p in &self.msix_pending {
            o.u32(*p);
        }
    }

    pub(crate) fn get(r: &mut In) -> Result<Platform> {
        let uart = Uart::get(r)?;
        let pic = Pic { master: PicChip::get(r)?, slave: PicChip::get(r)?, elcr: r.u16("ELCR")? };
        let mut ch = [PitChannel::default(); 3];
        for c in &mut ch {
            *c = PitChannel::get(r)?;
        }
        let pit = Pit { ch, port61: r.u8("port 0x61")?, ch0_edges_given: r.u64("the PIT's edges")?,
                        ch0_armed: r.bool("the PIT armed")? };
        let rtc = Rtc { index: r.u8("the RTC's index")?, ram: r.fixed::<128>("the CMOS")? };
        let pm = Pm {
            status: r.u16("PM1 status")?, enable: r.u16("PM1 enable")?, control: r.u16("PM1 control")?,
            elapsed_ns: r.u64("the PM timer")?, carries_seen: r.u64("the PM timer's carries")?,
            scis: r.u64("SCIs")?, presses: r.u32("presses")?, used: r.bool("ACPI used")?,
            other_sleep: r.opt("a sleep type", |r| r.u8("a sleep type"))?,
        };
        let pvpanic = PvPanic { crash_loaded: r.u32("pvpanic")? };
        let ioapic = r.opt("an IO-APIC", |r| {
            let id = r.u32("the IO-APIC's ID")?;
            let select = r.u32("the IO-APIC's select")?;
            let mut rte = [0u64; IOAPIC_PINS];
            for e in &mut rte {
                *e = r.u64("a redirection entry")?;
            }
            Ok(IoApic { id, select, rte, lines: r.u32("the IO-APIC's lines")?, remote_irr: r.u32("remote IRR")? })
        })?;
        let pci_address = r.u32("PCI's address")?;
        let n = usize::from(r.u8("PCI's functions")?);
        let functions = list(r, n, FUNCTIONS_MAX, "PCI's functions", Function::get)?;
        let n = usize::from(r.u8("the devices")?);
        let devices = list(r, n, DEVICES_MAX, "the devices", Device::get)?;
        let msix_pending = list(r, n, DEVICES_MAX, "MSI-X pending bits", |r| r.u32("MSI-X pending bits"))?;
        Ok(Platform { uart, pic, pit, rtc, pm, pvpanic, ioapic, pci_address, functions, devices, msix_pending })
    }
}
