//! The targets: each turns its input into what a guest -- and the host
//! around it -- does to one device, within what the hypervisor's dispatch
//! guarantees the device (a port access of 1, 2 or 4 bytes, and only to its
//! own ports; an MSR of its own; an MMIO access inside its page, of 1 to 8
//! bytes; a clock that only goes forward) and with everything else the
//! guest's to choose. What the host would be handed that it must never be --
//! a disk request outside the disk, a frame longer than a frame -- is
//! checked where it would be handed over, by the backends here.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::devices::blk::{self, Blk};
use crate::devices::ioapic::{self, IoApic};
use crate::devices::net::{self, Net};
use crate::devices::pci::{self, Function, PciBus};
use crate::devices::pic::Pic;
use crate::devices::pit::Pit;
use crate::devices::pm::Pm;
use crate::devices::rtc::Rtc;
use crate::devices::uart::Uart;
use crate::lapic::{self, Addressing, Lapic, Mode, Wrote};
use crate::memory::GuestMemory;
use crate::{time, Target};

pub static ALL: &[Target] = &[
    Target { name: "uart", run: uart, max_len: 2048 },
    Target { name: "pic", run: pic, max_len: 2048 },
    Target { name: "pit", run: pit, max_len: 2048 },
    Target { name: "rtc", run: rtc, max_len: 1024 },
    Target { name: "pm", run: pm, max_len: 2048 },
    Target { name: "pci", run: pci_bus, max_len: 2048 },
    Target { name: "lapic", run: lapic_target, max_len: 4096 },
    Target { name: "ioapic", run: ioapic_target, max_len: 4096 },
    Target { name: "msi", run: msi, max_len: 1024 },
    Target { name: "blk", run: blk_target, max_len: 8192 },
    Target { name: "net", run: net_target, max_len: 8192 },
    Target { name: "walk", run: walk, max_len: 2048 },
    Target { name: "insn", run: insn, max_len: 256 },
    Target { name: "linux", run: linux, max_len: 4096 },
    Target { name: "acpi", run: acpi, max_len: 512 },
    Target { name: "dhcp", run: dhcp, max_len: 1024 },
    Target { name: "platform", run: crate::platform::platform, max_len: 16384 },
];

/// The input, read as a stream of choices: past its end every read is 0, and
/// `op` says there is no more.
pub struct Input<'a> {
    data: &'a [u8],
    at: usize,
}

/* Values that are where code goes wrong: the edges of each width, powers of
 * two and their neighbours, all ones. */
const EDGES32: [u32; 14] = [0, 1, 2, 3, 0x7F, 0x80, 0xFF, 0x100, 0xFFFF, 0x1_0000, 0x7FFF_FFFF, 0x8000_0000,
                            0xFFFF_FFFE, 0xFFFF_FFFF];
const EDGES64: [u64; 10] = [0, 1, 0xFFF, 0x1000, 0xFFFF_FFFF, 0x1_0000_0000, 0x7FFF_FFFF_FFFF_FFFF,
                            0x8000_0000_0000_0000, u64::MAX - 1, u64::MAX];

impl<'a> Input<'a> {
    pub fn new(data: &'a [u8]) -> Input<'a> {
        Input { data, at: 0 }
    }

    /// Read on from `at`.
    pub fn resume(data: &'a [u8], at: usize) -> Input<'a> {
        Input { data, at }
    }

    pub fn position(&self) -> usize {
        self.at
    }

    /// What is left of it.
    pub fn rest(&self) -> &'a [u8] {
        self.data.get(self.at..).unwrap_or(&[])
    }

    /// The next operation, of `n`, or None at the end of the input.
    pub fn op(&mut self, n: u8) -> Option<u8> {
        if self.at >= self.data.len() {
            return None;
        }
        Some(self.u8() % n)
    }

    pub fn u8(&mut self) -> u8 {
        let b = self.data.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }
    pub fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    pub fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | (u64::from(self.u32()) << 32)
    }
    pub fn bool(&mut self) -> bool {
        self.u8() & 1 != 0
    }
    /// True about once in 4096: for what ends a run, which has to be much
    /// rarer than a run is long for the rest to be reached. Never past the
    /// end of the input, whose zeros are the ordinary choice.
    pub fn rare(&mut self) -> bool {
        self.u16() >= 0xFFF0
    }
    /// A number below `n` (0 for none).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else if n <= 256 { u64::from(self.u8()) % n } else if n <= 1 << 16 {
            u64::from(self.u16()) % n
        } else { self.u64() % n }
    }
    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
    /// A 32-bit value: an edge, a power of two, or anything.
    pub fn value32(&mut self) -> u32 {
        match self.u8() % 4 {
            0 => self.pick(&EDGES32),
            1 => 1u32 << (self.u8() % 32),
            _ => self.u32(),
        }
    }
    pub fn value64(&mut self) -> u64 {
        match self.u8() % 4 {
            0 => self.pick(&EDGES64),
            1 => 1u64 << (self.u8() % 64),
            _ => self.u64(),
        }
    }
    /// A port access's size, as the CPU reports one.
    pub fn io_size(&mut self) -> u8 {
        self.pick(&[1, 2, 4])
    }
    /// An MMIO access's size, as the decoder gives one.
    pub fn mmio_size(&mut self) -> u8 {
        self.pick(&[1, 2, 4, 8])
    }
    /// How far the host's clock moves: mostly a little, sometimes a lot --
    /// a vCPU the host did not run for a long while.
    pub fn time_step(&mut self) -> u64 {
        match self.u8() % 8 {
            0 => 0,
            1..=4 => self.below(10_000_000),
            5 | 6 => self.below(10_000_000_000),
            _ => self.value64() / 4,
        }
    }
}

/* ---- the chipset's devices, over port I/O ---- */

/// What a guest's console writes that the serial port follows as a terminal
/// would: the cursor query a shell's line editor sends and waits on, other
/// escape sequences, and what moves the column.
const TERMINAL: [&[u8]; 9] = [b"\x1b[6n", b"\x1b[1;2H", b"\x1b[6", b"\x1b[", b"\x1b", b"\r\n", b"\t", b"\x08",
                              b"\xc3\xa9"];

fn uart(r: &mut Input) {
    let mut u = Uart::new();
    while let Some(op) = r.op(9) {
        let offset = r.below(8) as u16;
        match op {
            8 => {
                /* To the transmit register, the divisor latch closed. */
                u.write(3, 0x03);
                for &b in r.pick(&TERMINAL) {
                    u.write(0, b);
                }
                while u.reply_pending() {
                    if u.take_reply().is_none() {
                        break;
                    }
                }
            }
            0 | 1 => {
                u.read(offset);
            }
            2 | 3 => {
                u.write(offset, r.u8());
            }
            4 => {
                if u.rx_empty() {
                    u.set_rx(r.u8());
                }
            }
            5 => {
                u.take_reply();
            }
            6 => {
                u.take_pulse();
            }
            _ => {
                u.irq_line();
                u.prompt_seen();
                u.reply_pending();
                u.written();
                u.ier();
            }
        }
    }
}

fn pic(r: &mut Input) {
    const PORTS: [u16; 6] = [0x20, 0x21, 0xA0, 0xA1, 0x4D0, 0x4D1];
    let mut p = Pic::new();
    while let Some(op) = r.op(8) {
        match op {
            0 => {
                p.read(r.pick(&PORTS));
            }
            1 | 2 => p.write(r.pick(&PORTS), r.u8()),
            3 => p.raise(r.below(16) as u8),
            4 => p.lower(r.below(16) as u8),
            5 => {
                if let Some((irq, _)) = p.pending() {
                    invariant!(irq < 16, "the 8259 has IRQ {} pending", irq);
                    p.acknowledge(irq);
                }
            }
            6 => {
                let irq = r.below(16) as u8;
                p.busy(irq);
                p.masked(irq);
            }
            _ => {
                p.master_state();
            }
        }
    }
}

fn pit(r: &mut Input) {
    const PORTS: [u16; 5] = [0x40, 0x41, 0x42, 0x43, 0x61];
    let mut p = Pit::new();
    while let Some(op) = r.op(7) {
        match op {
            0 => {
                p.read(r.pick(&PORTS));
            }
            1 | 2 => p.write(r.pick(&PORTS), r.u8()),
            3 => time::advance(r.time_step()),
            4 => {
                p.ch0_fire();
            }
            5 => {
                if let Some(at) = p.next_ch0_edge_ns() {
                    /* Due now or later -- never in the past it has given. */
                    let _ = at;
                }
            }
            _ => {
                p.ch0_state();
            }
        }
    }
}

fn rtc(r: &mut Input) {
    let mut c = Rtc::new();
    while let Some(op) = r.op(4) {
        match op {
            0 => {
                c.read(r.pick(&[0x70, 0x71]));
            }
            1 | 2 => c.write(r.pick(&[0x70, 0x71]), r.u8()),
            _ => time::advance(r.time_step()),
        }
    }
}

fn pm(r: &mut Input) {
    let mut now = time::boot_time_ns();
    let mut p = Pm::new(now);
    while let Some(op) = r.op(6) {
        /* A port of the block's, as `Pm::owns` has them dispatched. */
        let port = 0x600 + r.below(12) as u16;
        match op {
            0 => {
                p.read(port, r.io_size(), now);
            }
            1 | 2 => {
                p.write(port, r.io_size(), r.value32(), now);
            }
            3 => now = time::later(now, r.time_step()),
            4 => {
                p.sci(now);
            }
            _ => {
                p.press_power_button();
            }
        }
    }
}

/* Configuration registers a guest's PCI code touches: the IDs, command and
 * status, the BARs, the capability pointer, the interrupt line and pin, and
 * the MSI-X capability's header, control word, table and PBA. */
const PCI_REGS: [u32; 12] = [0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x2C, 0x34, 0x3C, 0x40, 0x44, 0x48];

/// CONFIG_ADDRESS for `reg` of the function in `slot`, enabled.
fn pci_address(slot: u32, reg: u32) -> u32 {
    0x8000_0000 | (slot << 11) | (reg & 0xFC)
}

fn pci_bus(r: &mut Input) {
    let mut bus = PciBus::new().expect("a bus");
    bus.add(Function::device(&Blk::identity(), 0xC000, blk::BAR_SIZE, 11).with_msix(2, 0xFE00_1000))
        .expect("a disk");
    bus.add(Function::device(&Net::identity(), 0xC100, net::BAR_SIZE, 10)).expect("a NIC");
    while let Some(op) = r.op(7) {
        match op {
            0 => {
                /* A configuration address: mostly one of bus 0's functions'
                 * registers, sometimes past them, or anything. */
                let address = match r.u8() {
                    0..=199 => pci_address(r.below(4) as u32, r.pick(&PCI_REGS)),
                    200..=239 => pci_address(r.below(32) as u32, r.below(64) as u32 * 4) | (r.below(8) as u32) << 8,
                    _ => r.u32(),
                };
                bus.write(pci::CONFIG_ADDRESS, 4, address);
            }
            1 => {
                bus.read(pci::CONFIG_DATA + r.below(4) as u16, r.io_size());
            }
            2 => bus.write(pci::CONFIG_DATA + r.below(4) as u16, r.io_size(), r.value32()),
            3 => {
                bus.read(pci::CONFIG_ADDRESS + r.below(8) as u16, r.io_size());
            }
            4 => {
                /* A driver's probe of a BAR: all ones in, the size out, the
                 * base back -- and I/O decoding turned on. */
                let slot = r.below(3) as u32;
                for (reg, value) in [(0x10, u32::MAX), (0x14, u32::MAX), (0x10, r.value32()), (0x14, r.value32()),
                                     (0x04, 1 | u32::from(r.u8() & 6))] {
                    bus.write(pci::CONFIG_ADDRESS, 4, pci_address(slot, reg));
                    bus.read(pci::CONFIG_DATA, 4);
                    bus.write(pci::CONFIG_DATA, 4, value);
                }
            }
            5 => {
                let port = if r.bool() { 0xC000 + r.below(0x200) as u16 } else { r.u16() };
                if let Some((slot, offset)) = bus.io_target(port) {
                    invariant!(slot != 0 && usize::from(slot) < pci::MAX_SLOTS, "port {:#x} in slot {}", port, slot);
                    invariant!(u32::from(offset) < 0x100, "port {:#x} at {:#x} into its BAR", port, offset);
                }
            }
            _ => {
                bus.msix(r.below(12) as usize);
            }
        }
    }
}

/* ---- the local APIC and the IO-APIC ---- */

/* The local APIC's registers, by their x2APIC MSR's offset from 0x800
 * (lapic.rs): the ID, version, TPR, APR, PPR, EOI, RRD, LDR, DFR, SVR, the
 * first words of ISR, TMR and IRR, ESR, the ICR's two halves, the six LVT
 * entries, the timer's initial and current counts and divider, self-IPI. */
pub const APIC_REGS: [u32; 27] = [0x02, 0x03, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x18, 0x20,
                              0x28, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3E, 0x3F, 0x3F];
pub const APIC_EOI: u32 = 0x0B;
const APIC_SVR: u32 = 0x0F;
const APIC_ICR: u32 = 0x30;
const APIC_ICR_HIGH: u32 = 0x31;
const APIC_LVT_TIMER: u32 = 0x32;
const APIC_TIMER_INITIAL: u32 = 0x38;
const APIC_TIMER_DIVIDE: u32 = 0x3E;

/// A register: one of the APIC's, mostly -- a word of the 256-bit ones, or
/// anywhere in the range.
fn apic_reg(r: &mut Input) -> u32 {
    match r.u8() {
        0..=199 => r.pick(&APIC_REGS),
        200..=231 => r.pick(&[0x10u32, 0x18, 0x20]) + r.below(8) as u32,
        _ => r.below(0x100) as u32,
    }
}

/// A value for `reg` that means something to it, mostly.
pub fn apic_value(r: &mut Input, reg: u32) -> u64 {
    if r.u8() < 48 {
        return r.value64();
    }
    let vector = if r.u8() < 240 { 16 + r.below(240) } else { r.below(16) };
    match reg {
        APIC_SVR => 0x100 | vector | u64::from(r.u8() & 0x10) << 8,
        APIC_LVT_TIMER | 0x33..=0x37 => {
            vector | (r.below(8) << 8) | u64::from(r.bool()) << 16 | u64::from(r.bool()) << 17
                | u64::from(r.bool()) << 13 | u64::from(r.bool()) << 15
        }
        APIC_TIMER_INITIAL => r.pick(&[1u64, 2, 100, 10_000, 1_000_000, 0xFFFF_FFFF]),
        APIC_TIMER_DIVIDE => r.below(16),
        APIC_ICR => {
            /* Delivery mode, logical, level assert, level trigger, a
             * shorthand -- and in x2APIC mode the destination above. */
            let low = vector | (r.below(8) << 8) | u64::from(r.bool()) << 11 | u64::from(r.bool()) << 14
                | u64::from(r.bool()) << 15 | (r.below(4) << 18);
            low | (u64::from(r.pick(&[0u32, 1, 2, 3, 0xF, 0xFF, 0xFFFF_FFFF])) << 32)
        }
        APIC_ICR_HIGH => u64::from(r.u8()) << 24,
        APIC_EOI => 0,
        _ => u64::from(r.u32()),
    }
}

/// Write `reg` as a guest would: by its MSR, or through the page -- a whole
/// register mostly, and sometimes an access the page drops.
fn apic_write(a: &mut Lapic, r: &mut Input, reg: u32, value: u64, now: u64) {
    let wrote = if r.bool() {
        a.wrmsr(lapic::MSR_X2APIC_FIRST + reg, value, now).ok()
    } else if r.u8() < 240 {
        Some(a.mmio_write(reg << 4, 4, value as u32, now))
    } else {
        Some(a.mmio_write((reg << 4) + r.below(16) as u32, r.mmio_size(), value as u32, now))
    };
    match wrote {
        Some(Wrote::Eoi(vector)) => invariant!(vector >= 16, "an EOI of a level-triggered vector {}", vector),
        Some(Wrote::Ipi(ipi)) => {
            let target = Addressing { mode: r.pick(&[Mode::Off, Mode::Xapic, Mode::X2apic]), ldr: r.u8(), flat: r.bool() };
            ipi.reaches(r.below(32) as u32, target, a.id());
        }
        _ => {}
    }
}

fn lapic_target(r: &mut Input) {
    let (id, bsp, x2apic) = (r.below(16) as u32, r.bool(), r.bool());
    let mut a = Lapic::new(id, bsp, x2apic);
    let mut now = time::boot_time_ns();
    while let Some(op) = r.op(13) {
        match op {
            0 => {
                let reg = apic_reg(r);
                if r.bool() {
                    let _ = a.rdmsr(lapic::MSR_X2APIC_FIRST + reg, now);
                } else {
                    let (offset, size) = if r.u8() < 224 { (reg << 4, 4) } else { ((reg << 4) + r.below(16) as u32, r.mmio_size()) };
                    a.mmio_read(offset, size, now);
                }
            }
            1..=3 => {
                let reg = apic_reg(r);
                let value = apic_value(r, reg);
                apic_write(&mut a, r, reg, value, now);
            }
            4 => {
                /* The timer armed: its entry, its divider, its count. */
                for reg in [APIC_LVT_TIMER, APIC_TIMER_DIVIDE, APIC_TIMER_INITIAL] {
                    let value = apic_value(r, reg);
                    apic_write(&mut a, r, reg, value, now);
                }
            }
            5 => {
                now = time::later(now, r.time_step());
                a.timer(now);
                a.next_timer_ns();
            }
            6 => {
                a.accept(r.u8());
                a.accept_level(r.u8());
            }
            7 => {
                let (w, l) = ([r.u64(), r.u64(), r.u64(), r.u64()], [r.u64(), r.u64(), r.u64(), r.u64()]);
                a.accept_all(&w, &l);
            }
            8 => {
                /* The run loop injecting what is pending, and the guest's
                 * handler ending it. */
                if let Some(v) = a.pending() {
                    invariant!(v >= 16, "the APIC would inject vector {}, an exception's", v);
                    a.acknowledge(v);
                    if r.bool() {
                        apic_write(&mut a, r, APIC_EOI, 0, now);
                    }
                }
            }
            9 => {
                /* The base MSR: the page where it is, in a mode -- or anything. */
                let value = if r.u8() < 224 { lapic::DEFAULT_BASE | (u64::from(r.u8() & 0x0D) << 8) } else { r.value64() };
                let _ = a.wrmsr(lapic::MSR_APIC_BASE, value, now);
                let _ = a.rdmsr(lapic::MSR_APIC_BASE, now);
            }
            10 => a.init(),
            11 => {
                apic_write(&mut a, r, APIC_SVR, 0x1FF, now);
            }
            _ => {
                a.sync_cr8(r.u8() & 0xF);
                let x = a.addressing();
                let back = Addressing::unpack(x.pack());
                invariant!(back.mode == x.mode && back.ldr == x.ldr && back.flat == x.flat,
                           "addressing packed and unpacked changed");
                a.accepts_extint();
                a.state();
                a.requested(r.u8());
            }
        }
    }
}

fn ioapic_target(r: &mut Input) {
    let mut io = IoApic::new(r.u8());
    while let Some(op) = r.op(8) {
        /* Its page's registers, mostly -- the select, the window, EOI. */
        let offset = if r.u8() < 200 { r.pick(&[0x00, 0x10, 0x40]) } else { r.below(ioapic::PAGE_SIZE) as u32 };
        let pin = r.below(32) as usize;
        let pins = match op {
            0 => {
                io.mmio_read(offset, r.mmio_size());
                0
            }
            1 | 2 => {
                /* The select register pointed at a real register, mostly. */
                if r.bool() {
                    io.mmio_write(0, 4, r.below(0x40) as u32);
                }
                io.mmio_write(offset, r.mmio_size(), r.value32())
            }
            3 => io.set_line(pin, r.bool()),
            4 => io.edge(pin),
            5 => io.eoi(r.u8()),
            6 => {
                io.route(pin);
                0
            }
            _ => {
                let _ = io.send(pin);
                0
            }
        };
        invariant!(pins >> ioapic::PINS == 0, "pins past the last to send: {:#x}", pins);
        let mut left = pins;
        while left != 0 {
            let p = left.trailing_zeros() as usize;
            left &= left - 1;
            let _ = io.send(p);
        }
    }
}

fn msi(r: &mut Input) {
    while r.op(1).is_some() {
        let address = if r.bool() { 0xFEE0_0000 | u64::from(r.u32() & 0xF_FFFF) } else { r.value64() };
        let data = r.value32();
        let Some(ipi) = lapic::msi(address, data) else { continue };
        let mode = r.pick(&[Mode::Off, Mode::Xapic, Mode::X2apic]);
        let target = Addressing { mode, ldr: r.u8(), flat: r.bool() };
        ipi.reaches(r.below(32) as u32, target, r.below(32) as u32);
        lapic::logical_id(r.u32());
        Addressing::unpack(r.value64());
    }
}

/* ---- virtio: a guest driver's queues, well made and not ---- */

const MEM: u64 = 1 << 20;
const PAGE: u64 = 4096;
/* The legacy header's registers. */
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SELECT: u16 = 0x0E;
const QUEUE_NOTIFY: u16 = 0x10;
const DEVICE_STATUS: u16 = 0x12;
const QUEUE_SIZE: u64 = 256;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;

/// What a guest's driver keeps of one queue: where it is, and where it has
/// got to in its descriptors and its available ring.
#[derive(Clone, Copy, Default)]
pub struct DriverQueue {
    pfn: u64,
    next_desc: u64,
    avail_idx: u16,
}

impl DriverQueue {
    /// A queue the driver has just set up at page `pfn`.
    pub fn at(pfn: u64) -> DriverQueue {
        DriverQueue { pfn, next_desc: 0, avail_idx: 0 }
    }

    fn desc(&self, i: u64) -> u64 {
        self.pfn * PAGE + 16 * (i % QUEUE_SIZE)
    }
    fn avail(&self) -> u64 {
        self.pfn * PAGE + 16 * QUEUE_SIZE
    }
}

/// Where a driver writes what the device is to find: guest memory, of
/// which a plan may keep what is written until it is time to write it.
pub trait Memory {
    fn put(&self, gpa: u64, bytes: &[u8]);
}

impl Memory for GuestMemory {
    fn put(&self, gpa: u64, bytes: &[u8]) {
        let _ = self.write(gpa, bytes);
    }
}

/// A place in guest memory of `limit` bytes: mostly inside it, sometimes at
/// or past its end.
pub fn gpa_in(r: &mut Input, limit: u64) -> u64 {
    match r.u8() % 16 {
        0 => r.value64(),
        1 => limit - r.below(64),
        _ => r.below(limit),
    }
}

fn gpa(r: &mut Input) -> u64 {
    gpa_in(r, MEM)
}

/// Make `segs` -- (address, length, device writes) -- a chain in `q`'s
/// descriptors, put its head in the available ring, and publish it: or, a
/// guest that makes a ring wrong, a chain that points anywhere.
pub fn chain(mem: &dyn Memory, q: &mut DriverQueue, r: &mut Input, segs: &[(u64, u32, bool)]) {
    if q.pfn == 0 || segs.is_empty() {
        return;
    }
    let head = q.next_desc % QUEUE_SIZE;
    for (i, &(addr, len, write)) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let mut flags = if write { DESC_WRITE } else { 0 };
        let mut next = ((q.next_desc + 1) % QUEUE_SIZE) as u16;
        if !last {
            flags |= DESC_NEXT;
        }
        /* Sometimes a descriptor that is wrong: a loop, an index past the
         * ring, an indirect one that was not offered. */
        match r.u8() % 32 {
            0 => next = head as u16,
            1 => next = r.u16(),
            2 => flags |= 4,
            3 => flags = r.u16(),
            _ => {}
        }
        let mut d = [0u8; 16];
        d[..8].copy_from_slice(&addr.to_le_bytes());
        d[8..12].copy_from_slice(&len.to_le_bytes());
        d[12..14].copy_from_slice(&flags.to_le_bytes());
        d[14..16].copy_from_slice(&next.to_le_bytes());
        mem.put(q.desc(q.next_desc), &d);
        q.next_desc += 1;
    }
    let slot = u64::from(q.avail_idx) % QUEUE_SIZE;
    mem.put(q.avail() + 4 + 2 * slot, &(head as u16).to_le_bytes());
    /* The index published: one on, or -- a driver gone wrong -- anything. */
    q.avail_idx = if r.u8() % 32 == 0 { r.u16() } else { q.avail_idx.wrapping_add(1) };
    mem.put(q.avail() + 2, &q.avail_idx.to_le_bytes());
    /* The flags: an interrupt wanted, or not. */
    mem.put(q.avail(), &u16::from(r.bool()).to_le_bytes());
}

/// Set queue `index` up where the guest put it: a page inside its memory,
/// mostly, the ring's three pages clear of its end.
fn setup_queue(r: &mut Input, write: &mut dyn FnMut(u16, u8, u32), q: &mut DriverQueue, index: u16) {
    let pfn = if r.u8() < 230 { 1 + r.below(MEM / PAGE - 4) } else { u64::from(r.u32()) };
    write(QUEUE_SELECT, 2, u32::from(index));
    write(QUEUE_PFN, 4, pfn as u32);
    *q = DriverQueue { pfn: pfn & 0xFFFF_FFFF, next_desc: 0, avail_idx: 0 };
}

/// The disk the virtio disk is served by, as a backend is: what it is handed
/// checked against what the device promises it.
pub struct DiskState {
    size: u64,
    read_only: bool,
    queue: VecDeque<blk::Request>,
    /// Requests the fuzzer has said are served, to be taken.
    pub ready: usize,
    pub fail: bool,
}

impl DiskState {
    pub fn new(size: u64, read_only: bool) -> DiskState {
        DiskState { size, read_only, queue: VecDeque::new(), ready: 0, fail: false }
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

pub struct Disk(pub Arc<Mutex<DiskState>>);

impl blk::Backend for Disk {
    fn size(&self) -> u64 {
        self.0.lock().unwrap().size
    }
    fn read_only(&self) -> bool {
        self.0.lock().unwrap().read_only
    }
    fn submit(&mut self, mut req: blk::Request) {
        let mut s = self.0.lock().unwrap();
        invariant!(s.queue.len() < blk::IN_FLIGHT, "a request past the {} in flight", blk::IN_FLIGHT);
        let len = req.data().len() as u64;
        match req.op() {
            blk::Op::Read | blk::Op::Write => {
                invariant!(req.offset() % blk::SECTOR == 0, "a request at byte {}, not a sector", req.offset());
                invariant!(req.offset().checked_add(len).is_some_and(|end| end <= s.size),
                           "a request of {} bytes at {} on a disk of {}", len, req.offset(), s.size);
            }
            blk::Op::Flush => {}
        }
        invariant!(!(s.read_only && req.op() == blk::Op::Write), "a write handed to a read-only disk");
        if req.op() == blk::Op::Read {
            req.data_mut().fill(0xA5);
        }
        s.queue.push_back(req);
    }
    fn take(&mut self) -> Option<blk::Request> {
        let mut s = self.0.lock().unwrap();
        if s.ready == 0 {
            return None;
        }
        s.ready -= 1;
        let fail = s.fail;
        let mut req = s.queue.pop_front()?;
        req.done(!fail);
        Some(req)
    }
}

/// A request, as a driver makes one: its header -- type and sector -- the
/// data, and the status byte the device writes; in memory of `limit` bytes.
pub fn blk_segments(mem: &dyn Memory, r: &mut Input, limit: u64) -> Vec<(u64, u32, bool)> {
    let header = gpa_in(r, limit) & !7;
    let kind = r.pick(&[0u32, 1, 4, 8, 0xFFFF_FFFF]);
    let sector = if r.bool() { r.below(1 << 20) } else { r.value64() };
    let mut h = [0u8; 16];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    h[8..].copy_from_slice(&sector.to_le_bytes());
    mem.put(header, &h);
    let mut segs = vec![(header, if r.u8() < 240 { 16 } else { r.value32() }, false)];
    for _ in 0..r.below(4) {
        let len = if r.bool() { 512 * (1 + r.below(128)) as u32 } else { r.value32() };
        segs.push((gpa_in(r, limit), len, kind != 1));
    }
    segs.push((gpa_in(r, limit), if r.u8() < 240 { 1 } else { r.value32() }, true));
    segs
}

fn blk_request(mem: &GuestMemory, q: &mut DriverQueue, r: &mut Input) {
    let segs = blk_segments(mem, r, MEM);
    chain(mem, q, r, &segs);
}

fn blk_target(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let size = if r.bool() { blk::SECTOR * r.below(1 << 16) } else { r.value64() & !(blk::SECTOR - 1) };
    let state = Arc::new(Mutex::new(DiskState::new(size, r.bool())));
    let mut disk = Blk::new(Box::new(Disk(state.clone())), "fuzz").expect("a disk");
    if r.bool() {
        disk.offer_msix(2);
    }
    let mut q = DriverQueue::default();
    while let Some(op) = r.op(12) {
        match op {
            0 => {
                disk.io_read(r.below(u64::from(blk::BAR_SIZE)) as u16, r.io_size());
            }
            1 => {
                let (offset, size, value) = (r.below(u64::from(blk::BAR_SIZE)) as u16, r.io_size(), r.value32());
                disk.io_write(offset, size, value, &mem);
            }
            2 => {
                let mut write = |offset: u16, size: u8, value: u32| {
                    disk.io_write(offset, size, value, &mem);
                };
                let index = if r.u8() < 240 { 0 } else { r.u16() };
                setup_queue(r, &mut write, &mut q, index);
            }
            3 => {
                let status = r.pick(&[0, 1, 3, 7, 15, 0x80, 0xFF]);
                disk.io_write(DEVICE_STATUS, 1, status, &mem);
            }
            4 => {
                let queue = if r.u8() < 240 { 0 } else { r.u16() };
                disk.io_write(QUEUE_NOTIFY, 2, u32::from(queue), &mem);
            }
            5 => {
                let (at, n) = (gpa(r), r.below(64) as usize);
                let bytes: Vec<u8> = (0..n).map(|_| r.u8()).collect();
                let _ = mem.write(at, &bytes);
            }
            6 => blk_request(&mem, &mut q, r),
            7 => {
                /* More requests at once than the device has in flight: the
                 * rest wait on the ring for a buffer to come back. */
                for _ in 0..1 + r.below(2 * blk::IN_FLIGHT as u64) {
                    blk_request(&mem, &mut q, r);
                }
                disk.io_write(QUEUE_NOTIFY, 2, 0, &mem);
            }
            8 | 9 => {
                disk.poll(&mem);
            }
            10 => {
                let mut s = state.lock().unwrap();
                s.ready += 1 + r.below(4) as usize;
                s.fail = r.u8() % 8 == 0;
            }
            _ => {
                disk.set_msix_enabled(r.bool());
                disk.line();
                disk.broken();
            }
        }
    }
}

/// The guests' switch as a NIC's backend sees it: frames sent checked, and
/// frames to deliver, as the fuzzer makes them.
#[derive(Default)]
pub struct NicState {
    pub rx: VecDeque<Vec<u8>>,
}

pub struct Nic(pub Arc<Mutex<NicState>>);

impl net::Backend for Nic {
    fn send(&mut self, frame: &[u8]) {
        invariant!(frame.len() <= net::MAX_FRAME, "a frame of {} bytes sent, past {}", frame.len(), net::MAX_FRAME);
    }
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let f = self.0.lock().unwrap().rx.pop_front()?;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
}

fn net_target(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let state = Arc::new(Mutex::new(NicState::default()));
    let mut nic = Net::new(Box::new(Nic(state.clone())), [2, 0, 0, 0, 0x64, 2]).expect("a NIC");
    if r.bool() {
        nic.offer_msix(3);
    }
    let mut queues = [DriverQueue::default(); 2];
    while let Some(op) = r.op(12) {
        let which = r.below(2) as usize;
        match op {
            0 => {
                nic.io_read(r.below(u64::from(net::BAR_SIZE)) as u16, r.io_size());
            }
            1 => {
                let (offset, size, value) = (r.below(u64::from(net::BAR_SIZE)) as u16, r.io_size(), r.value32());
                nic.io_write(offset, size, value, &mem);
            }
            2 => {
                let mut write = |offset: u16, size: u8, value: u32| {
                    nic.io_write(offset, size, value, &mem);
                };
                let index = if r.u8() < 240 { which as u16 } else { r.u16() };
                setup_queue(r, &mut write, &mut queues[which], index);
            }
            3 => {
                let status = r.pick(&[0, 1, 3, 7, 15, 0x80, 0xFF]);
                nic.io_write(DEVICE_STATUS, 1, status, &mem);
            }
            4 => {
                let queue = if r.u8() < 240 { which as u16 } else { r.u16() };
                nic.io_write(QUEUE_NOTIFY, 2, u32::from(queue), &mem);
            }
            5 => {
                let (at, n) = (gpa(r), r.below(64) as usize);
                let bytes: Vec<u8> = (0..n).map(|_| r.u8()).collect();
                let _ = mem.write(at, &bytes);
            }
            6 => {
                /* Receive buffers: writable, of any length. */
                let mut segs = Vec::new();
                for _ in 0..1 + r.below(3) {
                    let len = if r.bool() { 1526 } else { r.value32() };
                    segs.push((gpa(r), len, true));
                }
                chain(&mem, &mut queues[0], r, &segs);
            }
            7 => {
                /* A frame to send: the header, then the frame, in pieces. */
                let mut segs = vec![(gpa(r), if r.u8() < 230 { 10 } else { r.value32() }, false)];
                for _ in 0..r.below(4) {
                    let len = if r.bool() { r.below(1600) as u32 } else { r.value32() };
                    segs.push((gpa(r), len, false));
                }
                chain(&mem, &mut queues[1], r, &segs);
            }
            8 | 9 => {
                nic.poll(&mem);
            }
            10 => {
                let n = r.pick(&[0usize, 13, 14, 60, 64, 1500, 1514, 1515, 4000]);
                let frame: Vec<u8> = (0..n).map(|_| r.u8()).collect();
                state.lock().unwrap().rx.push_back(frame);
            }
            _ => {
                nic.set_msix_enabled(r.bool());
                nic.line();
                nic.broken();
            }
        }
    }
}

/* ---- the MMIO path: the page walk and the decoder ---- */

fn walk(r: &mut Input) {
    use crate::walk::{fetch, translate, Paging};
    let mem = GuestMemory::with_size(MEM);
    /* Page tables, mostly shaped like ones: present, pointing at a page
     * inside memory, sometimes large. */
    for _ in 0..r.below(128) {
        let at = r.below(MEM) & !7;
        let entry = if r.u8() < 200 {
            (r.below(MEM / PAGE) << 12) | 1 | (u64::from(r.u8() & 0x82))
        } else {
            r.value64()
        };
        let _ = mem.write(at, &entry.to_le_bytes());
    }
    let paging = Paging {
        cr0: r.pick(&[0x8000_0011u64, 0x11, 0, 0xFFFF_FFFF]) | if r.bool() { r.value64() } else { 0 },
        cr3: if r.bool() { r.below(MEM) & !0xFFF } else { r.value64() },
        cr4: r.pick(&[0x20u64, 0x1020, 0, 0x10, 0x30]) | if r.u8() < 16 { r.value64() } else { 0 },
        efer: r.pick(&[0x500u64, 0x100, 0, 0xD01]),
    };
    let la = if r.bool() { r.below(MEM) } else { r.value64() };
    let read = |gpa: u64| {
        let mut q = [0u8; 8];
        mem.read(gpa, &mut q).ok().map(|_| u64::from_le_bytes(q))
    };
    let _ = translate(&paging, la, read);
    let mut buf = [0u8; 15];
    let n = r.below(16) as usize;
    if let Ok(got) = fetch(&paging, la, &mut buf[..n], read, |gpa, b| mem.read(gpa, b).is_ok()) {
        invariant!(got <= n, "fetched {} bytes into {}", got, n);
    }
}

/* What an instruction is made of, for the decoder: the prefixes, and the
 * opcodes it takes (the moves Linux's MMIO accessors are) beside a few it
 * must refuse. */
const PREFIXES: [u8; 12] = [0x66, 0x67, 0xF2, 0xF3, 0xF0, 0x26, 0x2E, 0x36, 0x3E, 0x64, 0x65, 0x40];
const OPCODES: [&[u8]; 16] = [&[0x88], &[0x89], &[0x8A], &[0x8B], &[0xC6], &[0xC7], &[0x0F, 0xB6], &[0x0F, 0xB7],
                              &[0x0F, 0xBE], &[0x0F, 0xBF], &[0xA1], &[0xA3], &[0x01], &[0x87], &[0x0F, 0xC3], &[0x0F]];

fn insn(r: &mut Input) {
    use crate::insn::{decode, Access, Mode};
    let mut bytes = Vec::new();
    if r.u8() < 192 {
        /* Prefixes, a REX, an opcode, then ModRM, SIB, displacement and
         * immediate as whatever follows. */
        for _ in 0..r.below(4) {
            bytes.push(r.pick(&PREFIXES));
        }
        if r.bool() {
            bytes.push(0x40 | (r.u8() & 0xF));
        }
        bytes.extend_from_slice(r.pick(&OPCODES));
    }
    for _ in 0..r.below(12) {
        bytes.push(r.u8());
    }
    bytes.truncate(15);
    let n = bytes.len();
    for mode in [Mode::Long, Mode::Protected32] {
        if let Some(i) = decode(&bytes, mode) {
            invariant!(usize::from(i.len) <= n && i.len >= 1, "an instruction of {} bytes out of {}", i.len, n);
            let size = match i.access {
                Access::Load { size, .. } | Access::Store { size, .. } | Access::StoreImm { size, .. } => size,
            };
            invariant!(matches!(size, 1 | 2 | 4 | 8), "an access of {} bytes", size);
        }
    }
}

/* ---- what a guest's image decides: the Linux loader ---- */

fn linux(r: &mut Input) {
    use crate::linux::{build, plan, set_entry, Firmware, Header};
    let mut first = vec![0u8; 0x270 + r.below(0x400) as usize];
    for b in first.iter_mut() {
        *b = r.u8();
    }
    /* A header that parses more often than random bytes would: its magic,
     * its version and its 64-bit entry; and mostly a kernel's values in the
     * fields that decide the layout, or else the input's. */
    if r.u8() < 230 {
        first[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
        first[0x202..0x206].copy_from_slice(b"HdrS");
        let version: u16 = r.pick(&[0x20C, 0x20F, 0x215, 0xFFFF]);
        first[0x206..0x208].copy_from_slice(&version.to_le_bytes());
        first[0x236] |= 1;
        if r.u8() < 200 {
            let pref: u64 = r.pick(&[0x100_0000, 0x100_0000, 0x20_0000, 0x10_0000, 0xF_F000, 0]);
            first[0x258..0x260].copy_from_slice(&pref.to_le_bytes());
            let init_size = (1 + r.below(64) as u32) << 20;
            first[0x260..0x264].copy_from_slice(&init_size.to_le_bytes());
            let initrd_max: u32 = r.pick(&[0x7FFF_FFFF, 0xFFFF_FFFF, 0x37FF_FFFF, 0x100_0000]);
            first[0x22C..0x230].copy_from_slice(&initrd_max.to_le_bytes());
            let cmdline_size: u32 = r.pick(&[2047, 255, 4095, 0, 1]);
            first[0x238..0x23C].copy_from_slice(&cmdline_size.to_le_bytes());
        }
    }
    let Ok(h) = Header::parse(&first) else { return };
    h.pm_offset();
    /* Whole pages, as guest memory is made of; `mem=` is in MiB. */
    let mem_bytes = match r.u8() % 8 {
        0 => r.value64() & !0xFFF,
        1 => 1 << 40,
        _ => (64 + r.below(4096 - 64 + 1)) << 20,
    };
    let kernel_len = if r.u8() < 224 { r.below(32 << 20) } else { r.value64() };
    let initrd_len = match r.u8() % 8 {
        0 => 0,
        1 => r.value64(),
        _ => r.below(64 << 20),
    };
    let Ok(l) = plan(&h, mem_bytes, kernel_len, initrd_len) else { return };
    /* What the loader streams the kernel and the initrd by, and writes its
     * own tables around: inside memory, apart, and clear of the tables. */
    let kernel_end = l.kernel_addr.checked_add(l.kernel_len);
    invariant!(kernel_end.is_some_and(|e| e <= mem_bytes), "a kernel at {:#x} of {:#x} bytes in {:#x}",
               l.kernel_addr, l.kernel_len, mem_bytes);
    if l.initrd_len != 0 {
        invariant!(l.initrd_addr.checked_add(l.initrd_len).is_some_and(|e| e <= mem_bytes),
                   "an initrd at {:#x} of {:#x} bytes in {:#x}", l.initrd_addr, l.initrd_len, mem_bytes);
        invariant!(kernel_end.is_some_and(|e| l.initrd_addr >= e), "the initrd at {:#x} inside the kernel, to {:#x}",
                   l.initrd_addr, kernel_end.unwrap_or(0));
    }
    invariant!(l.kernel_addr >= 1 << 20,
               "the kernel at {:#x}, over the tables the loader writes below 1 MiB", l.kernel_addr);
    if mem_bytes > 4096 << 20 {
        return;
    }
    let memory = GuestMemory::with_size(mem_bytes);
    let cmdline: Vec<u8> = (0..r.below(512)).map(|_| r.u8()).collect();
    let firmware = r.bool().then_some(Firmware { rsdp: crate::acpi::AREA, start: crate::acpi::AREA,
                                                 end: crate::acpi::AREA_END });
    let routes = [crate::acpi::Route { slot: 1, irq: 11 }, crate::acpi::Route { slot: 2, irq: 10 }];
    let cpus = 1 + r.below(16) as u32;
    let _ = build(&memory, &h, &first, &l, &cmdline, cpus, firmware, &routes[..r.below(3) as usize], r.bool());
    let mut vcpu = crate::vm::Backend::default();
    set_entry(&mut vcpu, &l);
}

/* ---- the ACPI tables, for any machine the host could describe ---- */

fn acpi(r: &mut Input) {
    use crate::acpi::{build, Machine, Route, MAX_BYTES};
    let routes: Vec<Route> = (0..r.below(10)).map(|_| Route { slot: r.u8(), irq: r.u8() }).collect();
    let m = Machine {
        cpus: if r.bool() { r.below(17) as u32 } else { r.u32() },
        apic: r.bool(),
        ioapic: r.bool().then(|| r.u8()),
        pci: &routes,
        mmio: r.bool().then(|| (r.u32(), r.u32())),
    };
    let mut buf = vec![0u8; if r.u8() < 224 { MAX_BYTES } else { r.below(MAX_BYTES as u64) as usize }];
    if let Some(n) = build(&m, &mut buf) {
        invariant!(n <= buf.len(), "tables of {} bytes in {}", n, buf.len());
    }
}

/* ---- the guests' DHCP server, on what a guest's client sends ---- */

/* DHCP (RFC 2131): the ports, the fixed part's size and the magic cookie
 * that ends it, and the options a client's message carries. */
const DHCP_SERVER_PORT: u16 = 67;
const DHCP_CLIENT_PORT: u16 = 68;
const DHCP_OPTIONS: usize = 240;
const DHCP_COOKIE: u32 = 0x6382_5363;
const DHCP_CODES: [u8; 8] = [53, 50, 54, 0, 255, 12, 55, 61];

fn dhcp(r: &mut Input) {
    use crate::dhcp::{answer, is_request, Network, REPLY_MAX};
    use netwire::udp;
    let net = Network { server_ip: 0x0A00_6401, server_mac: [2, 0, 0, 0, 0x64, 1], mask: 0xFFFF_FF00,
                        dns: if r.bool() { 0x0808_0808 } else { 0 } };
    let yours = if r.u8() < 200 { 0x0A00_6402 } else { r.u32() };
    let mut frame = vec![0u8; netwire::MAX_FRAME];
    let len = if r.u8() < 224 {
        /* A client's message, mostly as one is made: a BOOTREQUEST for an
         * Ethernet address, the cookie, then options -- the type, the
         * address asked for, the server -- well formed or cut short. */
        let mut m = vec![0u8; DHCP_OPTIONS + r.below(300) as usize];
        m[0] = if r.u8() < 250 { 1 } else { r.u8() };
        m[1] = 1;
        m[2] = 6;
        for b in &mut m[4..12] {
            *b = r.u8();
        }
        let any = r.u32();
        let ciaddr = r.pick(&[0, yours, any]);
        m[12..16].copy_from_slice(&ciaddr.to_be_bytes());
        for b in &mut m[28..44] {
            *b = r.u8();
        }
        let cookie = if r.u8() < 250 { DHCP_COOKIE } else { r.u32() };
        m[236..240].copy_from_slice(&cookie.to_be_bytes());
        let mut at = DHCP_OPTIONS;
        while at < m.len() && r.u8() < 220 {
            let code = r.pick(&DHCP_CODES);
            let data: Vec<u8> = match code {
                53 => vec![r.pick(&[1u8, 3, 4, 7, 8, 2, 0, 0xFF])],
                50 | 54 => {
                    let any = r.u32();
                    r.pick(&[yours, 0x0A00_6401, any]).to_be_bytes().to_vec()
                }
                _ => (0..r.below(8)).map(|_| r.u8()).collect(),
            };
            let len = if r.u8() < 240 { data.len() as u8 } else { r.u8() };
            for b in [code, len].into_iter().chain(data) {
                if at < m.len() {
                    m[at] = b;
                    at += 1;
                }
            }
        }
        let route = udp::Route {
            src_mac: [2, 0, 0, 0, 0x64, 2], dst_mac: netwire::MAC_BROADCAST,
            src_ip: if r.bool() { 0 } else { r.u32() }, dst_ip: u32::MAX,
            src_port: if r.u8() < 250 { DHCP_CLIENT_PORT } else { r.u16() },
            dst_port: if r.u8() < 250 { DHCP_SERVER_PORT } else { r.u16() },
            dont_fragment: false,
        };
        let Some(n) = udp::write_frame(&mut frame, &route, m.len()) else { return };
        frame[udp::PAYLOAD_AT..n].copy_from_slice(&m);
        /* Now and then a header that lies: its IP header's length, the
         * datagram's, the protocol. */
        if r.u8() < 16 {
            let at = r.pick(&[netwire::ETH_HDR_LEN, netwire::ETH_HDR_LEN + 9, udp::PAYLOAD_AT - 4,
                              udp::PAYLOAD_AT - 3]);
            frame[at] = r.u8();
        }
        if r.u8() < 16 { r.below(n as u64 + 1) as usize } else { n }
    } else {
        let n = r.below(frame.len() as u64) as usize;
        for b in &mut frame[..n] {
            *b = r.u8();
        }
        n
    };
    let f = &frame[..len];
    is_request(f);
    let mut reply = [0u8; REPLY_MAX];
    let Some(n) = answer(f, yours, &net, &mut reply) else { return };
    invariant!(n <= REPLY_MAX, "an answer of {} bytes, past {}", n, REPLY_MAX);
    let Some(d) = udp::parse(&reply[..n]) else { panic!("invariant: an answer that is no UDP frame") };
    invariant!(d.src_port == DHCP_SERVER_PORT && d.dst_port == DHCP_CLIENT_PORT,
               "an answer from port {} to {}", d.src_port, d.dst_port);
    /* The checksum it carries is the one its bytes have. */
    let datagram_at = udp::PAYLOAD_AT - netwire::UDP_HDR_LEN;
    let mut datagram = reply[datagram_at..n].to_vec();
    let sent = u16::from_be_bytes([datagram[udp::CHECKSUM], datagram[udp::CHECKSUM + 1]]);
    datagram[udp::CHECKSUM..udp::CHECKSUM + 2].fill(0);
    invariant!(udp::checksum(d.src_ip, d.dst_ip, &datagram) == sent, "an answer whose checksum is not its own");
}
