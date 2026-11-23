//! The targets: each turns its input into what a guest -- and the host
//! around it -- does to one device, within what the hypervisor's dispatch
//! guarantees the device (a port access of 1, 2 or 4 bytes, and only to its
//! own ports; an MSR of its own; an MMIO access inside its page, of 1 to 8
//! bytes; a clock that only goes forward) and with everything else the
//! guest's to choose. What the host would be handed that it must never be --
//! a disk request outside the disk, a frame longer than a frame -- is
//! checked where it would be handed over, by the backends here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use crate::devices::blk::{self, Blk};
use crate::devices::ioapic::{self, IoApic};
use crate::devices::net::{self, Net};
use crate::devices::offload::{self, TxHeader};
use crate::devices::pci::{self, Function, HostBridge, PciBus};
use crate::devices::pic::Pic;
use crate::devices::pit::Pit;
use crate::devices::pm::{Pm, SLEEP_CONTROL, SLEEP_STATUS};
use crate::devices::rtc::Rtc;
use crate::devices::uart::Uart;
use crate::lapic::{self, Addressing, Lapic, Mode, Wrote};
use crate::memory::{self, GuestMemory};
use crate::{time, Target};

pub static ALL: &[Target] = &[
    Target { name: "uart", run: uart, max_len: 2048 },
    Target { name: "pic", run: pic, max_len: 2048 },
    Target { name: "pit", run: pit, max_len: 2048 },
    Target { name: "rtc", run: rtc, max_len: 1024 },
    Target { name: "pm", run: pm, max_len: 2048 },
    Target { name: "pvpanic", run: pvpanic_target, max_len: 512 },
    Target { name: "pci", run: pci_bus, max_len: 2048 },
    Target { name: "hostfn", run: crate::hostmodel::hostfn_target, max_len: 4096 },
    Target { name: "port", run: crate::portmodel::port_target, max_len: 2048 },
    Target { name: "groups", run: crate::groupsmodel::groups_target, max_len: 1024 },
    Target { name: "lapic", run: lapic_target, max_len: 4096 },
    Target { name: "ioapic", run: ioapic_target, max_len: 4096 },
    Target { name: "msi", run: msi, max_len: 1024 },
    Target { name: "blk", run: blk_target, max_len: 8192 },
    Target { name: "net", run: net_target, max_len: 8192 },
    Target { name: "offload", run: crate::offloadmodel::offload_target, max_len: 4096 },
    Target { name: "netrx", run: crate::netrx::netrx_target, max_len: 2048 },
    Target { name: "virtq", run: crate::virtq::virtq_target, max_len: 4096 },
    Target { name: "walk", run: walk, max_len: 2048 },
    Target { name: "insn", run: insn, max_len: 256 },
    Target { name: "linux", run: linux, max_len: 4096 },
    Target { name: "pvh", run: pvh, max_len: 4096 },
    Target { name: "acpi", run: acpi, max_len: 512 },
    Target { name: "smbios", run: crate::smbiosmodel::smbios_target, max_len: 512 },
    Target { name: "dhcp", run: dhcp, max_len: 1024 },
    Target { name: "switch", run: crate::guestnet::switch_target, max_len: 8192 },
    Target { name: "nat", run: crate::guestnet::nat_target, max_len: 8192 },
    Target { name: "onetoone", run: crate::guestnet::one_target, max_len: 4096 },
    Target { name: "guestnet", run: crate::guestnet::guestnet_target, max_len: 4096 },
    Target { name: "firewall", run: crate::fwmodel::firewall_target, max_len: 4096 },
    Target { name: "fwswitch", run: crate::fwmodel::fwswitch_target, max_len: 2048 },
    Target { name: "vnetswitch", run: crate::vnetmodel::vnetswitch_target, max_len: 4096 },
    Target { name: "flowmove", run: crate::vnetmodel::flowmove_target, max_len: 2048 },
    Target { name: "vnetwire", run: crate::vnetwiremodel::vnetwire_target, max_len: 4096 },
    Target { name: "tunnel", run: crate::tunnelmodel::tunnel_target, max_len: 4096 },
    Target { name: "platform", run: crate::platform::platform, max_len: 16384 },
    Target { name: "policy", run: crate::policymodel::policy_target, max_len: 1024 },
    Target { name: "hyperv", run: hyperv_target, max_len: 2048 },
    Target { name: "place", run: crate::placemodel::place_target, max_len: 2048 },
    Target { name: "config", run: crate::configmodel::config_target, max_len: 4096 },
    Target { name: "relay", run: crate::relaymodel::relay_target, max_len: 4096 },
    Target { name: "vmstate", run: crate::statemodel::vmstate_target, max_len: 8192 },
    Target { name: "handover", run: crate::handovermodel::handover_target, max_len: 1024 },
    Target { name: "migwire", run: crate::migwiremodel::migwire_target, max_len: 4096 },
    Target { name: "dirtylog", run: crate::dirtymodel::dirtylog_target, max_len: 4096 },
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
const TERMINAL: [&[u8]; 16] = [b"\x1b[6n", b"\x1b[1;2H", b"\x1b[6", b"\x1b[", b"\x1b", b"\r\n", b"\t", b"\x08",
                               b"\xc3\xa9", b"\x1b7", b"\x1b8", b"\x1b[32766;32766H", b"\x1b[?2004h", b"\x1b[5C",
                               b"\x1b[3D", b"\x1b[12G"];

/// What a terminal says to `ESC [ 6 n`: `ESC [ row ; column R`.
fn cursor_reply(u: &mut Uart) -> Vec<u8> {
    let mut got = Vec::new();
    while let Some(b) = u.take_reply() {
        got.push(b);
    }
    got
}

fn uart(r: &mut Input) {
    let mut u = Uart::new();
    while let Some(op) = r.op(11) {
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
            7 => {
                /* systemd sizing a serial console as a getty starts: the
                 * cursor saved, moved to a far corner, asked for, put back
                 * -- told a VT100's 24 rows and 80 columns, whatever the
                 * text before; and a shell's prompt, asked for after it, told
                 * where the prompt ends */
                u.write(3, 0x03);
                u.write(4, 0x00);
                let _ = cursor_reply(&mut u);
                let text = r.below(120) as usize;
                for _ in 0..text {
                    u.write(0, b'x');
                }
                for &b in b"\x1b7\x1b[32766;32766H\x1b[6n" {
                    u.write(0, b);
                }
                let got = cursor_reply(&mut u);
                invariant!(got == b"\x1b[24;80R", "a far corner said to be at {:?}", String::from_utf8_lossy(&got));
                for &b in b"\x1b8\r\ndebian@db:~$ \x1b[6n" {
                    u.write(0, b);
                }
                let got = cursor_reply(&mut u);
                invariant!(got == b"\x1b[24;14R", "a prompt's end said to be at {:?}", String::from_utf8_lossy(&got));
            }
            9 => {
                /* A line typed in whole: the next held back until the shell
                 * asks where the cursor is again. */
                u.line_taken();
                invariant!(!u.prompt_seen(), "at a prompt still after a line went in");
                /* The divisor latch closed, and loopback off: the query to
                 * the line, not back to the receiver. */
                u.write(3, 0x03);
                u.write(4, 0x00);
                for &b in b"\x1b[6n" {
                    u.write(0, b);
                }
                invariant!(u.prompt_seen(), "not at a prompt after the shell asked where the cursor is");
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
    while let Some(op) = r.op(8) {
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
            6 => {
                /* Channel 0 programmed as a guest programs its tick: a
                 * control word, then its count's low byte and high byte,
                 * time going by between. The counter waits for the whole
                 * count: no IRQ0 edge before it, none due. The old count ran
                 * on in the new mode, and half a count already counted. */
                const CH0_LOHI: u8 = 0x30;
                const MODE_SHIFT: u8 = 1;
                let mode = r.pick(&[0u8, 2, 3, 4]);
                p.write(0x43, CH0_LOHI | (mode << MODE_SHIFT));
                for byte in [Some(r.u8()), None] {
                    time::advance(r.time_step());
                    invariant!(!p.ch0_fire() && p.next_ch0_edge_ns().is_none(),
                               "channel 0 has an edge in mode {} before its count is whole", mode);
                    if let Some(low) = byte {
                        p.write(0x40, low);
                    }
                }
                p.write(0x40, r.u8());
            }
            _ => {
                p.ch0_state();
            }
        }
    }
}

fn rtc(r: &mut Input) {
    let mut c = Rtc::new();
    c.write(0x70, 0x0A);
    invariant!(c.read(0x71) & 0x80 == 0, "UIP begins during an update");
    time::advance(crate::consts::NS_PER_SEC - crate::consts::NS_PER_MS);
    invariant!(c.read(0x71) & 0x80 != 0, "no UIP before the second turns over");
    time::advance(crate::consts::NS_PER_MS);
    invariant!(c.read(0x71) & 0x80 == 0, "UIP stays set after the update");
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
    let mut p = Pm::new_cloudhv(now);
    invariant!(Pm::owns(SLEEP_CONTROL) && Pm::owns(SLEEP_STATUS), "the reduced sleep registers are not owned");
    invariant!(p.read(SLEEP_CONTROL, 1, now) == 0 && p.read(SLEEP_STATUS, 1, now) == 0,
               "the reduced sleep registers begin nonzero");
    invariant!(p.write(SLEEP_CONTROL, 1, 0x34, now), "CloudHv's firmware S5 was not taken");
    let mut legacy = Pm::new(now);
    invariant!(!legacy.write(SLEEP_CONTROL, 1, 0x34, now), "legacy PM1 status became CloudHv sleep control");
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

/// The panic device, held to what its port promises: a read says the events
/// it takes, PANICKED and CRASH_LOADED; a write with PANICKED is the panic,
/// whatever else it says; one with CRASH_LOADED alone is counted and let
/// go; anything else is nothing.
fn pvpanic_target(r: &mut Input) {
    use crate::devices::pvpanic::{Event, PvPanic, CRASH_LOADED, PANICKED, PORT};
    let mut dev = PvPanic::default();
    let mut crash = 0u32;
    invariant!(PvPanic::owns(PORT) && !PvPanic::owns(PORT + 1) && !PvPanic::owns(PORT - 1), "the port it owns");
    while let Some(op) = r.op(2) {
        if op == 0 {
            invariant!(dev.read() == PANICKED | CRASH_LOADED, "a read of {:#x}", dev.read());
            continue;
        }
        let v = r.u8();
        let want = if v & PANICKED != 0 {
            Some(Event::Panicked)
        } else if v & CRASH_LOADED != 0 {
            crash += 1;
            Some(Event::CrashLoaded)
        } else {
            None
        };
        let got = dev.write(v);
        invariant!(got == want, "{:#x} written, {:?} made of it, {:?} the model", v, got, want);
        invariant!(dev.crash_loaded == crash, "{} crash kernels counted, {} the model", dev.crash_loaded, crash);
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

/// All ones, `size` bytes of them: a read of nothing.
fn size_ones(size: usize) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => u32::MAX,
    }
}

fn pci_msix_moved(bus: &PciBus, memory: &GuestMemory, m: pci::MsixMove, bar: &mut u32, mapped: &mut Option<u64>) {
    const BACKING: u32 = 0xFE00_1000;
    const RAM_END: u64 = 4096;
    const XAPIC: u64 = 0xFEE0_0000;

    invariant!(m.page == BACKING && m.from == *bar, "MSI-X moved {:?}, BAR was {:#x}", m, *bar);
    memory.move_msix(u64::from(m.page), u64::from(m.from), u64::from(m.to)).expect("the MSI-X backing page");
    *bar = m.to;
    let to = u64::from(m.to);
    *mapped = (to == u64::from(BACKING) || (to >= RAM_END && to != XAPIC)).then_some(to);
    invariant!(memory.msix_mapping(u64::from(BACKING)) == *mapped, "MSI-X at {:#x}: mapping {:?}, expected {:?}",
               m.to, memory.msix_mapping(u64::from(BACKING)), *mapped);
    invariant!(bus.function_read(1, 0, 0x14, 4) == m.to, "MSI-X BAR did not move to {:#x}", m.to);
    invariant!(bus.msix(1).is_some_and(|(_, page)| page == BACKING), "MSI-X's backing moved with its BAR");
    let mut bytes = [0u8; 4];
    memory.read(u64::from(BACKING), &mut bytes).expect("the MSI-X backing bytes");
    invariant!(bytes == [0x11, 0x22, 0x33, 0x44], "MSI-X's backing bytes changed in a move");
}

fn pci_bus(r: &mut Input) {
    const BACKING: u32 = 0xFE00_1000;

    let cloudhv = r.bool();
    let mut bus = if cloudhv { PciBus::with_host_bridge(HostBridge::CloudHv) } else { PciBus::new() }.expect("a bus");
    let mut memory = GuestMemory::with_size(4096);
    memory.add_msix(u64::from(BACKING)).expect("an MSI-X page");
    memory.write(u64::from(BACKING), &[0x11, 0x22, 0x33, 0x44]).expect("MSI-X's first bytes");
    let mut bar = BACKING;
    let mut mapped = Some(u64::from(BACKING));
    macro_rules! write_bus {
        ($port:expr, $size:expr, $value:expr) => {
            if let Some(m) = bus.write($port, $size, $value) {
                pci_msix_moved(&bus, &memory, m, &mut bar, &mut mapped);
            }
        };
    }
    macro_rules! write_ecam {
        ($device:expr, $function:expr, $at:expr, $size:expr, $value:expr) => {
            if let Some(m) = bus.function_write($device, $function, $at, $size, $value) {
                pci_msix_moved(&bus, &memory, m, &mut bar, &mut mapped);
            }
        };
    }
    let device = if cloudhv { 0x0D57 } else { 0x29C0 };
    invariant!(bus.function_read(0, 0, 0, 4) == 0x8086 | device << 16,
               "the host bridge's vendor and device are not 8086:{:04x}", device);
    bus.add(Function::device(&Blk::identity(), 0xC000, blk::BAR_SIZE, 11).with_msix(2, BACKING))
        .expect("a disk");
    bus.add(Function::device(&Net::identity(), 0xC100, net::BAR_SIZE, 10)).expect("a NIC");
    /* A host's function on slot 3, whose space is the run loop's */
    let host = bus.add_host().expect("a host's function");

    /* A move while migration's dirty log is on: the new mapping starts
     * read-only, its first write marks the stable backing page, and a move
     * back behaves the same way. */
    const MOVED: u32 = 0xD000_0000;
    memory.log_start().expect("the dirty log");
    invariant!(memory.protected(u64::from(BACKING)), "the initial MSI-X mapping is writable under the log");
    write_ecam!(1, 0, 0x14, 4, MOVED);
    invariant!(memory.protected(u64::from(MOVED)), "a moved MSI-X mapping is writable under the log");
    invariant!(memory.write_fault(u64::from(MOVED) + 4), "the moved MSI-X write was not a dirty-log fault");
    invariant!(!memory.protected(u64::from(MOVED)), "the MSI-X write fault did not make its page writable");
    let mut taken = Vec::new();
    memory.log_take(&mut |gpa| taken.push(gpa)).expect("a dirty-log round");
    invariant!(taken == [u64::from(BACKING)], "the moved MSI-X write marked {:?}, not its backing", taken);
    write_ecam!(1, 0, 0x14, 4, BACKING);
    invariant!(memory.protected(u64::from(BACKING)), "moving back lost the dirty log's protection");
    invariant!(memory.write_fault(u64::from(BACKING)), "the restored MSI-X page cannot be written");
    memory.log_stop();
    invariant!(!memory.protected(u64::from(BACKING)), "the dirty log left MSI-X read-only");

    while let Some(op) = r.op(8) {
        match op {
            7 => {
                /* ECAM: an offset of bus 0 names the function and register
                 * the ports' address does, a read of the 256 bytes reads what
                 * the ports read -- a write of them writes what theirs would
                 * -- and the extended space past them is none's: zeroes for a
                 * function there, all ones for none */
                let (device, function) = (r.below(32) as u32, if r.u8() < 200 { 0 } else { r.below(8) as u32 });
                let reg = if r.bool() { r.pick(&PCI_REGS) } else { r.below(0x1000) as u32 };
                let size = r.pick(&[4usize, 4, 2, 1]);
                let reg = reg & !(size as u32 - 1);
                let offset = (u64::from(device) << 15) | (u64::from(function) << 12) | u64::from(reg);
                let t = pci::ecam_target(offset);
                invariant!((t.bus, u32::from(t.device), u32::from(t.function), t.at as u32) == (0, device, function, reg),
                           "ECAM's offset {:#x} named {:?}", offset, t);
                let got = bus.function_read(t.device, t.function, t.at, size);
                if reg < 0x100 {
                    write_bus!(pci::CONFIG_ADDRESS, 4, pci_address(device, reg) | function << 8);
                    let want = bus.read(pci::CONFIG_DATA + (reg & 3) as u16, size as u8);
                    invariant!(got == want, "ECAM read {:#x} at {:02x}.{} {:#x}, the ports {:#x}", got, device,
                               function, reg, want);
                    if r.bool() {
                        let value = r.value32();
                        let mut by_ports = PciBus::new().expect("a bus");
                        by_ports.add(Function::device(&Blk::identity(), 0xC000, blk::BAR_SIZE, 11)
                            .with_msix(2, BACKING)).expect("a disk");
                        let mut by_ecam = PciBus::new().expect("a bus");
                        by_ecam.add(Function::device(&Blk::identity(), 0xC000, blk::BAR_SIZE, 11)
                            .with_msix(2, BACKING)).expect("a disk");
                        let _ = by_ports.write(pci::CONFIG_ADDRESS, 4, pci_address(device, reg) | function << 8);
                        let by_port = by_ports.write(pci::CONFIG_DATA + (reg & 3) as u16, size as u8, value);
                        let by_memory = by_ecam.function_write(t.device, t.function, t.at, size, value);
                        invariant!(by_port == by_memory, "an MSI-X move differs between the ports and ECAM");
                        for d in (0..0x100).step_by(4) {
                            let a = by_ports.function_read(t.device, 0, d, 4);
                            let b = by_ecam.function_read(t.device, 0, d, 4);
                            invariant!(a == b, "a write of {:#x} at {:02x}.{} {:#x}: {:#x} at {:#x} by the ports, \
                                                {:#x} by ECAM", value, device, function, reg, a, d, b);
                        }
                    }
                } else {
                    let there = function == 0 && device < 4;
                    let want = if there { 0 } else { size_ones(size) };
                    invariant!(got == want, "ECAM read {:#x} past the 256 bytes at {:02x}.{} {:#x}, not {:#x}", got,
                               device, function, reg, want);
                }
            }
            0 => {
                /* A configuration address: mostly one of bus 0's functions'
                 * registers, sometimes past them, or anything. */
                let address = match r.u8() {
                    0..=199 => pci_address(r.below(4) as u32, r.pick(&PCI_REGS)),
                    200..=239 => pci_address(r.below(32) as u32, r.below(64) as u32 * 4) | (r.below(8) as u32) << 8,
                    /* A register past 255, as AMD's extended configuration
                     * puts its number's high bits in 27:24: no function here
                     * has one, so it is nobody's. */
                    240..=247 => pci_address(r.below(4) as u32, r.pick(&PCI_REGS)) | (1 + r.below(15) as u32) << 24,
                    _ => r.u32(),
                };
                write_bus!(pci::CONFIG_ADDRESS, 4, address);
                if address & 0x8000_0000 != 0 && address & 0x7F00_0000 != 0 {
                    let got = bus.read(pci::CONFIG_DATA, 4);
                    invariant!(got == u32::MAX, "configuration address {:#x}, a register past 255, read {:#x}",
                               address, got);
                }
            }
            1 => {
                bus.read(pci::CONFIG_DATA + r.below(4) as u16, r.io_size());
            }
            2 => {
                let port = pci::CONFIG_DATA + r.below(4) as u16;
                let size = r.io_size();
                let value = r.value32();
                write_bus!(port, size, value);
            }
            3 => {
                bus.read(pci::CONFIG_ADDRESS + r.below(8) as u16, r.io_size());
            }
            4 => {
                /* A driver's probe of a BAR: all ones in, the size out, the
                 * base back -- and I/O decoding turned on. */
                let slot = r.below(3) as u32;
                for (reg, value) in [(0x10, u32::MAX), (0x14, u32::MAX), (0x10, r.value32()), (0x14, r.value32()),
                                     (0x04, 1 | u32::from(r.u8() & 6))] {
                    write_bus!(pci::CONFIG_ADDRESS, 4, pci_address(slot, reg));
                    bus.read(pci::CONFIG_DATA, 4);
                    write_bus!(pci::CONFIG_DATA, 4, value);
                }
            }
            5 => {
                let port = if r.bool() { 0xC000 + r.below(0x200) as u16 } else { r.u16() };
                /* Only the configuration data's ports are a host function's,
                 * whatever the address latched names */
                let hosted = bus.host_target(port);
                invariant!(hosted.is_none() || (pci::CONFIG_DATA..pci::CONFIG_DATA + 4).contains(&port),
                           "port {:#x} taken for host function {}'s space at {:?}", port, host, hosted);
                /* And of those, what the address names, as it names it: the
                 * host's slot on bus 0, or any bus past it -- those below
                 * the ports, the run loop's to answer -- and no other */
                let at = pci::CONFIG_DATA + r.below(4) as u16;
                let address = bus.read(pci::CONFIG_ADDRESS, 4);
                let named = (address >> 16 & 0xFF) as u8 != 0 || (address >> 11 & 0x1F) as u8 == host;
                let want = (address & 0x8000_0000 != 0 && address & 0x7F00_0000 == 0 && named).then(|| {
                    pci::HostTarget {
                        bus: (address >> 16) as u8,
                        device: (address >> 11 & 0x1F) as u8,
                        function: (address >> 8 & 0x7) as u8,
                        at: (address & 0xFC) as usize + usize::from(at - pci::CONFIG_DATA),
                    }
                });
                let got = bus.host_target(at);
                invariant!(got == want, "address {:#x} at port {:#x} named {:?}, not {:?}", address, at, got, want);
                if let Some((slot, offset)) = bus.io_target(port) {
                    invariant!(slot != 0 && usize::from(slot) < pci::MAX_SLOTS, "port {:#x} in slot {}", port, slot);
                    invariant!(u32::from(offset) < 0x100, "port {:#x} at {:#x} into its BAR", port, offset);
                }
            }
            6 => {
                /* OVMF's BAR assignment: a known free or occupied page, the
                 * backing itself, the APIC, or any 32-bit address. Sometimes
                 * size the BAR first, which must not move its mapping. */
                let random = r.value32() & !(pci::MSIX_PAGE - 1);
                let to = r.pick(&[0xD000_0000, 0xFE00_0000, BACKING, 0, 0xFEE0_0000, random]);
                let ecam = r.bool();
                if r.bool() {
                    if ecam {
                        write_ecam!(1, 0, 0x14, 4, u32::MAX);
                    } else {
                        write_bus!(pci::CONFIG_ADDRESS, 4, pci_address(1, 0x14));
                        write_bus!(pci::CONFIG_DATA, 4, u32::MAX);
                    }
                    invariant!(bus.function_read(1, 0, 0x14, 4) == !(pci::MSIX_PAGE - 1),
                               "MSI-X's size read changed");
                    invariant!(memory.msix_mapping(u64::from(BACKING)) == mapped, "sizing moved the MSI-X page");
                    if ecam {
                        write_ecam!(1, 0, 0x14, 4, bar);
                    } else {
                        write_bus!(pci::CONFIG_DATA, 4, bar);
                    }
                }
                if ecam {
                    write_ecam!(1, 0, 0x14, 4, to);
                } else {
                    write_bus!(pci::CONFIG_ADDRESS, 4, pci_address(1, 0x14));
                    write_bus!(pci::CONFIG_DATA, 4, to);
                }
                let to = to & !(pci::MSIX_PAGE - 1);
                let address = u64::from(to);
                bar = to;
                mapped = (to == BACKING || (address >= 4096 && address != 0xFEE0_0000)).then_some(address);
                invariant!(bus.function_read(1, 0, 0x14, 4) == to, "the requested BAR {:#x} did not arrive", to);
                invariant!(memory.msix_mapping(u64::from(BACKING)) == mapped,
                           "BAR {:#x} has mapping {:?}, expected {:?}", to,
                           memory.msix_mapping(u64::from(BACKING)), mapped);
            }
            _ => {
                bus.msix(r.below(12) as usize);
            }
        }
        invariant!(memory.msix_mapping(u64::from(BACKING)) == mapped, "an unrelated PCI access moved MSI-X");
        invariant!(bus.msix(1).is_some_and(|(_, page)| page == BACKING), "MSI-X lost its stable backing");
    }
}

/* ---- the local APIC and the IO-APIC ---- */

/* The local APIC's registers, by their x2APIC MSR's offset from 0x800
 * (lapic.rs): the ID, version, TPR, APR, PPR, EOI, RRD, LDR, DFR, SVR, the
 * first words of ISR, TMR and IRR, ESR, the ICR's two halves, the six LVT
 * entries, the timer's initial and current counts and divider, self-IPI. */
pub const APIC_REGS: [u32; 27] = [0x02, 0x03, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x18, 0x20,
                              0x28, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3E, 0x3F, 0x3F];
const APIC_TPR: u32 = 0x08;
pub const APIC_EOI: u32 = 0x0B;
const APIC_SVR: u32 = 0x0F;
const APIC_ICR: u32 = 0x30;
const APIC_ICR_HIGH: u32 = 0x31;
const APIC_LVT_TIMER: u32 = 0x32;
const APIC_TIMER_INITIAL: u32 = 0x38;
const APIC_TIMER_CURRENT: u32 = 0x39;
const APIC_TIMER_DIVIDE: u32 = 0x3E;
const APIC_SELF_IPI: u32 = 0x3F;

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
            /* The timer's TSC-deadline mode now and then: bit 18 with 17,
             * the reserved mode, rarely. */
            let deadline = reg == APIC_LVT_TIMER && r.u8() < 64;
            vector | (r.below(8) << 8) | u64::from(r.bool()) << 16 | u64::from(r.bool() && (!deadline || r.rare())) << 17
                | u64::from(deadline) << 18 | u64::from(r.bool()) << 13 | u64::from(r.bool()) << 15
        }
        APIC_TIMER_INITIAL => r.pick(&[1u64, 2, 100, 10_000, 1_000_000, 0xFFFF_FFFF]),
        APIC_TIMER_DIVIDE => r.below(16),
        APIC_ICR => {
            /* Delivery mode, logical, level assert, level trigger, a
             * shorthand -- and in x2APIC mode the destination above. Half of
             * them a fixed IPI to one CPU, as most a kernel sends are. */
            let fixed = r.bool();
            let mode = if fixed { 0 } else { r.below(8) };
            let shorthand = if fixed { 0 } else { r.below(4) };
            let low = vector | (mode << 8) | u64::from(r.bool() && !fixed) << 11 | u64::from(r.bool()) << 14
                | u64::from(r.bool() && !fixed) << 15 | (shorthand << 18);
            low | (u64::from(r.pick(&[0u32, 1, 2, 3, 0xF, 0xFF, 0xFFFF_FFFF])) << 32)
        }
        APIC_ICR_HIGH => u64::from(r.u8()) << 24,
        APIC_EOI => 0,
        /* A priority, a class at a time as a kernel raises it; a vector. */
        APIC_TPR => u64::from(r.u8() & if r.bool() { 0xF0 } else { 0xFF }),
        APIC_SELF_IPI => vector,
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

/// The timer's mode, LVT bits 18:17, as the guest reads its entry -- through
/// the MSR in x2APIC mode, the page in xAPIC mode; None with the APIC off.
fn timer_mode(a: &Lapic, now: u64) -> Option<u32> {
    apic_read(a, APIC_LVT_TIMER, now).map(|lvt| (lvt >> 17) & 0b11)
}

/// TSC-deadline mode's code in those bits.
const TIMER_DEADLINE: u32 = 0b10;

/// Register `reg` as the guest reads it, in the APIC's mode; None with the
/// APIC off.
fn apic_read(a: &Lapic, reg: u32, now: u64) -> Option<u32> {
    if !a.enabled() {
        return None;
    }
    if a.xapic() {
        Some(a.mmio_read(reg << 4, 4, now))
    } else {
        a.rdmsr(lapic::MSR_X2APIC_FIRST + reg, now).ok().map(|v| v as u32)
    }
}

/// Write `reg` whole as the APIC's mode has it -- its MSR in x2APIC mode, the
/// page in xAPIC mode -- and nothing with the APIC off: a guest's write that
/// is sure to land.
fn apic_set(a: &mut Lapic, reg: u32, value: u64, now: u64) {
    if !a.enabled() {
        return;
    }
    if a.xapic() {
        a.mmio_write(reg << 4, 4, value as u32, now);
    } else {
        let _ = a.wrmsr(lapic::MSR_X2APIC_FIRST + reg, value, now);
    }
}

/* The timer's entry: masked, and periodic. */
const LVT_MASKED: u64 = 1 << 16;
const TIMER_PERIODIC: u64 = 1 << 17;

fn lapic_target(r: &mut Input) {
    let (id, bsp, x2apic) = (r.below(16) as u32, r.bool(), r.bool());
    let mut a = Lapic::new(id, bsp, x2apic);
    let mut now = time::boot_time_ns();
    while let Some(op) = r.op(17) {
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
            13 => {
                /* IA32_TSC_DEADLINE, in whatever mode the timer is: in
                 * TSC-deadline mode a deadline armed and read back, gone
                 * once the timer has run out; in any other, nothing taken
                 * and 0 read (SDM 11.5.4.1). */
                let before = timer_mode(&a, now);
                if r.u8() < 128 {
                    let entry = 0x4_0000 | (16 + r.below(240));
                    apic_write(&mut a, r, APIC_LVT_TIMER, entry, now);
                }
                /* The mode as the guest reads it in the timer's entry. */
                let mode = timer_mode(&a, now);
                if mode == Some(TIMER_DEADLINE) {
                    invariant!(before == mode || a.tsc_deadline() == 0,
                               "IA32_TSC_DEADLINE {:#x} on the way into TSC-deadline mode", a.tsc_deadline());
                    invariant!(apic_read(&a, APIC_TIMER_CURRENT, now) == Some(0),
                               "the current count read {:?} in TSC-deadline mode", apic_read(&a, APIC_TIMER_CURRENT, now));
                }
                let value = if r.u8() < 32 { 0 } else { r.value64() };
                let at = time::later(now, r.time_step());
                let next = a.next_timer_ns();
                a.set_tsc_deadline(value, at);
                let read = a.tsc_deadline();
                match mode {
                    Some(TIMER_DEADLINE) => {
                        invariant!(read == value, "IA32_TSC_DEADLINE read {:#x} after {:#x} was written", read, value);
                        if value != 0 {
                            let later = time::later(at, r.time_step());
                            a.timer(later);
                            invariant!(a.tsc_deadline() == 0, "IA32_TSC_DEADLINE {:#x} once the timer ran out",
                                       a.tsc_deadline());
                            now = later;
                        }
                    }
                    Some(other) => {
                        invariant!(read == 0, "IA32_TSC_DEADLINE read {:#x} in timer mode {}", read, other);
                        invariant!(a.next_timer_ns() == next, "IA32_TSC_DEADLINE written in timer mode {} moved the \
                                   timer from {:?} to {:?}", other, next, a.next_timer_ns());
                    }
                    None => {}
                }
            }
            14 => {
                /* The divide configuration written while a count runs: the
                 * count goes on from where it is, at the new rate -- at the
                 * write's instant it reads what it did before. Kept from the
                 * old start, it read up to 128 times off. */
                let before = apic_read(&a, APIC_TIMER_CURRENT, now);
                let value = apic_value(r, APIC_TIMER_DIVIDE);
                apic_write(&mut a, r, APIC_TIMER_DIVIDE, value, now);
                let after = apic_read(&a, APIC_TIMER_CURRENT, now);
                invariant!(before == after, "the count read {:?} before a divide configuration was written and {:?} \
                           after, at one instant", before, after);
            }
            15 => {
                /* A periodic timer masked while time goes by -- looked at or
                 * not -- then unmasked: what it missed is lost, as on an
                 * APIC, and nothing is handed over at the instant it is
                 * unmasked. Moved a period a look and owed the rest, it came
                 * out of its mask with up to a second of periods, back to
                 * back. */
                a.init();
                let vector = 16 + r.below(240);
                apic_set(&mut a, APIC_SVR, 0x1FF, now);
                apic_set(&mut a, APIC_TIMER_DIVIDE, 0b1011, now);
                apic_set(&mut a, APIC_LVT_TIMER, TIMER_PERIODIC | LVT_MASKED | vector, now);
                apic_set(&mut a, APIC_TIMER_INITIAL, r.pick(&[1u64, 1000, 100_000, 1_000_000, 10_000_000]), now);
                for _ in 0..r.below(8) {
                    now = time::later(now, r.time_step());
                    invariant!(!a.timer(now), "a masked timer requested its interrupt");
                }
                now = time::later(now, r.time_step());
                apic_set(&mut a, APIC_LVT_TIMER, TIMER_PERIODIC | vector, now);
                let mut handed = 0;
                for _ in 0..4 {
                    if !a.timer(now) {
                        break;
                    }
                    handed += 1;
                    if let Some(v) = a.pending() {
                        a.acknowledge(v);
                        apic_set(&mut a, APIC_EOI, 0, now);
                    }
                }
                invariant!(handed == 0, "a periodic timer unmasked handed over {} interrupts at that instant", handed);
            }
            16 => {
                use crate::hyperv::{MSR_APIC_EOI, MSR_APIC_ICR, MSR_APIC_TPR};
                let mut a = Lapic::new(id, bsp, x2apic);
                apic_set(&mut a, APIC_SVR, 0x1FF, now);
                let tpr = r.u8();
                invariant!(matches!(a.hyperv_write(MSR_APIC_TPR, u64::from(tpr), now), Some(Ok(Wrote::Tpr(v))) if v == tpr),
                           "Hyper-V TPR write did not update the APIC");
                invariant!(a.hyperv_read(MSR_APIC_TPR) == Some(u64::from(tpr)), "Hyper-V TPR not read back");
                invariant!(matches!(a.hyperv_write(MSR_APIC_TPR, 0x100, now), Some(Err(_))),
                           "Hyper-V TPR accepted reserved bits");
                invariant!(matches!(a.hyperv_write(MSR_APIC_TPR, 1 << 32, now), Some(Err(_))),
                           "Hyper-V TPR accepted a nonzero high half");
                invariant!(a.hyperv_read(MSR_APIC_EOI) == Some(0), "Hyper-V EOI read not zero");
                let icr = 0x30 | 1 << 18;
                invariant!(matches!(a.hyperv_write(MSR_APIC_ICR, icr, now), Some(Ok(Wrote::Ipi(_)))),
                           "Hyper-V self-IPI not decoded");
                invariant!(a.hyperv_read(MSR_APIC_ICR) == Some(icr), "Hyper-V ICR not read back");
                invariant!(matches!(a.hyperv_write(MSR_APIC_TPR, 0, now), Some(Ok(Wrote::Tpr(0)))),
                           "Hyper-V TPR could not be lowered for the EOI check");
                invariant!(a.accept(0x30), "an interrupt for the Hyper-V EOI check refused");
                let vector = a.pending().expect("an in-service interrupt for Hyper-V EOI");
                a.acknowledge(vector);
                invariant!(matches!(a.hyperv_write(MSR_APIC_EOI, 0, now), Some(Ok(Wrote::Done))),
                           "Hyper-V EOI did not end an edge interrupt");
                invariant!(matches!(a.hyperv_write(MSR_APIC_EOI, 1 << 32, now), Some(Err(_))),
                           "Hyper-V EOI accepted reserved bits");
                let remote = u64::from((id + 1) & 0xFF) << 56 | 0x40;
                invariant!(a.fixed_hyperv_ipi(remote, id).is_some_and(|ipi| ipi.vector == 0x40),
                           "Hyper-V remote fixed IPI not accepted by the fast path");
                invariant!(a.hyperv_read(0x4000_0074).is_none() && a.hyperv_write(0x4000_0074, 0, now).is_none(),
                           "an unknown Hyper-V MSR claimed by the APIC");
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
    io.mmio_write(0, 4, 1);
    let version = io.mmio_read(0x10, 4);
    io.mmio_write(0x10, 4, 0xFFFF_FFFF);
    io.mmio_write(0, 4, 1);
    invariant!(io.mmio_read(0x10, 4) == version, "Windows's probe changed the IO-APIC version register");
    /* Each pin as the target has driven it: a line held up, asserted again,
     * is no edge, and sends nothing -- a level's interrupt went when it rose
     * or when it was ended, an NMI is edge-triggered whatever its entry says
     * (an NMI at every look was a guest's CPU 0 never entered again), and
     * one no local APIC took is sent again only when a CPU's APIC changes
     * (`untaken`). Whether a local APIC takes a message is the input's to
     * say, as the CPUs it names would. */
    let mut up: u32 = 0;
    while let Some(op) = r.op(10) {
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
            3 => {
                let asserted = r.bool();
                let bit = 1u32.checked_shl(pin as u32).unwrap_or(0);
                let pins = io.set_line(pin, asserted);
                invariant!(!(asserted && up & bit != 0) || pins & bit == 0,
                           "pin {}, held up and asserted again, sent again: {:#x}", pin, pins);
                if asserted { up |= bit } else { up &= !bit }
                pins
            }
            4 => {
                up |= 1u32.checked_shl(pin as u32).unwrap_or(0);
                io.edge(pin)
            }
            5 => io.eoi(r.u8()),
            6 => {
                io.route(pin);
                0
            }
            7 => {
                /* A level posted to another CPU's mailbox, refused there. */
                io.refused(r.u8());
                0
            }
            8 => {
                /* A CPU's APIC turned on, or addressed otherwise: what no
                 * APIC took, sent again -- pins held up alone. */
                let again = io.untaken();
                invariant!(again & !up == 0, "pins {:#x} sent again, of which {:#x} are not asserted", again,
                           again & !up);
                again
            }
            _ => {
                ioapic_send(&mut io, pin, r.bool(), up);
                0
            }
        };
        invariant!(pins >> ioapic::PINS == 0, "pins past the last to send: {:#x}", pins);
        let mut left = pins;
        while left != 0 {
            let p = left.trailing_zeros() as usize;
            left &= left - 1;
            ioapic_send(&mut io, p, r.bool(), up);
        }
    }
}

/// Pin `pin`'s redirection entry as a guest reads it through the window,
/// its remote IRR among it.
fn ioapic_entry(io: &mut IoApic, pin: usize) -> u64 {
    const IOREGSEL: u32 = 0x00;
    const IOWIN: u32 = 0x10;
    let low = 0x10 + 2 * pin as u32;
    io.mmio_write(IOREGSEL, 4, low);
    let lo = u64::from(io.mmio_read(IOWIN, 4));
    io.mmio_write(IOREGSEL, 4, low + 1);
    lo | u64::from(io.mmio_read(IOWIN, 4)) << 32
}

/// Pin `pin`'s interrupt sent, a local APIC taking it or not (`took`), the
/// pins asserted being `up`: a level-triggered entry's remote IRR set after
/// exactly when one was taken -- now, or before and not yet ended -- and the
/// pin among those to send again (`untaken`) exactly when, asserted and
/// unmasked, none was. Set as it was made, a level nobody took left its pin
/// dead for good: no EOI was coming.
fn ioapic_send(io: &mut IoApic, pin: usize, took: bool, up: u32) {
    const DELIVERY_FIXED: u64 = 0;
    const DELIVERY_LOWEST: u64 = 1;
    const REMOTE_IRR: u64 = 1 << 14;
    const LEVEL: u64 = 1 << 15;
    const MASKED: u64 = 1 << 16;
    if pin >= ioapic::PINS {
        io.send(pin, &mut |_| took);
        return;
    }
    let before = ioapic_entry(io, pin);
    io.send(pin, &mut |_| took);
    let after = ioapic_entry(io, pin);
    let level = before & LEVEL != 0 && matches!((before >> 8) & 0x7, DELIVERY_FIXED | DELIVERY_LOWEST);
    if !level {
        return;
    }
    let held = took || before & REMOTE_IRR != 0;
    invariant!((after & REMOTE_IRR != 0) == held, "pin {}'s level sent, {} -- its remote IRR {} after, {} before",
               pin, if took { "taken" } else { "taken by none" }, after & REMOTE_IRR != 0, before & REMOTE_IRR != 0);
    let bit = 1u32 << pin;
    let pending = up & bit != 0 && before & MASKED == 0 && !held;
    invariant!((io.untaken() & bit != 0) == pending, "pin {} is {}among those to send again, {} up, masked {}, \
               remote IRR {}", pin, if pending { "not " } else { "" }, if up & bit != 0 { "held" } else { "not" },
               before & MASKED != 0, held);
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
/// What the driver takes of the device's features, legacy virtio's way.
const GUEST_FEATURES: u16 = 0x04;
const QUEUE_PFN: u16 = 0x08;
const QUEUE_SELECT: u16 = 0x0E;
const QUEUE_NOTIFY: u16 = 0x10;
const DEVICE_STATUS: u16 = 0x12;
const QUEUE_SIZE: u64 = 256;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const DESC_INDIRECT: u16 = 4;

/// Chains made with some of their buffers in an indirect table, for
/// `HV_FUZZ_STATS`.
static INDIRECT_MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A descriptor's sixteen bytes.
fn descriptor(addr: u64, len: u32, flags: u16, next: u16) -> [u8; 16] {
    let mut d = [0u8; 16];
    d[..8].copy_from_slice(&addr.to_le_bytes());
    d[8..12].copy_from_slice(&len.to_le_bytes());
    d[12..14].copy_from_slice(&flags.to_le_bytes());
    d[14..16].copy_from_slice(&next.to_le_bytes());
    d
}

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
    /// The used ring: on the page boundary after the available ring's flags,
    /// index, entries and used_event.
    fn used(&self) -> u64 {
        (self.avail() + 6 + 2 * QUEUE_SIZE).next_multiple_of(PAGE)
    }
}

/// How many chains a racing CPU posts before it calls the device's pass one
/// with no end: twice the queue, where a pass takes a queue's worth at most.
const RACE_POSTS: usize = 2 * QUEUE_SIZE as usize;

/// `call` -- a notify, or a round of the devices -- while another CPU of the
/// guest keeps `q` topped up with the chain it published last
/// (`memory::Racer`): the device has to end its pass by itself. The driver's
/// own idea of its index is brought up to date after.
fn racing(mem: &GuestMemory, q: &mut DriverQueue, call: impl FnOnce()) {
    if q.pfn == 0 {
        call();
        return;
    }
    let mut b = [0u8; 2];
    let last = u64::from(q.avail_idx.wrapping_sub(1)) % QUEUE_SIZE;
    let head = if mem.read(q.avail() + 4 + 2 * last, &mut b).is_ok() { u16::from_le_bytes(b) } else { 0 };
    mem.race(q.avail() + 2, q.avail() + 4, q.used() + 2, QUEUE_SIZE as u16, head, RACE_POSTS);
    call();
    mem.stop_race();
    if mem.read(q.avail() + 2, &mut b).is_ok() {
        q.avail_idx = u16::from_le_bytes(b);
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
    /* Now and then the buffers -- all of them, or all past the first few --
     * in an indirect table, as a driver that took indirect descriptors makes
     * a chain: for a device that offers none, a ring it will not follow. */
    let direct = if r.u8() % 8 == 0 { r.below(segs.len() as u64) as usize } else { segs.len() };
    if direct < segs.len() {
        INDIRECT_MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    for (i, &(addr, len, write)) in segs[..direct].iter().enumerate() {
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
            2 => flags |= DESC_INDIRECT,
            3 => flags = r.u16(),
            _ => {}
        }
        mem.put(q.desc(q.next_desc), &descriptor(addr, len, flags, next));
        q.next_desc += 1;
    }
    if direct < segs.len() {
        /* The rest in a table, each entry's next the one after it, and the
         * descriptor that names it last -- or, sometimes, any of it wrong:
         * a table of no whole descriptors, one within it, a next past its
         * end or back to its start, the descriptor that names it with a
         * next of its own (or a write flag, which the device ignores). */
        let rest = &segs[direct..];
        let table = if r.u8() < 230 { gpa_in(r, MEM) & !15 } else { gpa_in(r, MEM) };
        for (i, &(addr, len, write)) in rest.iter().enumerate() {
            let last = i + 1 == rest.len();
            let mut flags = if write { DESC_WRITE } else { 0 } | if last { 0 } else { DESC_NEXT };
            let mut next = i as u16 + 1;
            match r.u8() % 48 {
                0 => next = 0,
                1 => next = r.u16(),
                2 => flags |= DESC_INDIRECT,
                3 => flags = r.u16(),
                _ => {}
            }
            mem.put(table.wrapping_add(16 * i as u64), &descriptor(addr, len, flags, next));
        }
        let mut len = 16 * rest.len() as u32;
        let mut flags = DESC_INDIRECT;
        match r.u8() % 32 {
            0 => len = r.value32(),
            1 => len -= 16,
            2 => flags |= DESC_NEXT,
            3 => flags |= DESC_WRITE,
            _ => {}
        }
        let next = ((q.next_desc + 1) % QUEUE_SIZE) as u16;
        mem.put(q.desc(q.next_desc), &descriptor(table, len, flags, next));
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
    /// Its block (`Backend::block_size`): every request whole ones of it.
    block: u64,
    /// What it holds, a sector at a time, where it keeps it (`keep`): the
    /// 512e scenario's. A sector not here holds zeros.
    store: HashMap<u64, [u8; 512]>,
    keep: bool,
    /// A request's buffer a slot, as a backend has them, and which of them
    /// it has the request of.
    bufs: Vec<Vec<u8>>,
    queue: VecDeque<blk::Request>,
    /// Flushes handed over, including one behind a write whose driver took
    /// no FLUSH (which the device's own stats answer as the write).
    flushes: u64,
    /// Requests the fuzzer has said are served, to be taken.
    pub ready: usize,
    pub fail: bool,
    /// Which of those it has said are served comes back next: a backend
    /// serves several at once, and gives each back as it finishes.
    pub order: u64,
}

impl DiskState {
    pub fn new(size: u64, read_only: bool) -> DiskState {
        DiskState::with_block(size, read_only, blk::SECTOR)
    }

    pub fn with_block(size: u64, read_only: bool, block: u64) -> DiskState {
        let bufs = (0..blk::IN_FLIGHT).map(|_| vec![0u8; blk::REQUEST_BYTES]).collect();
        DiskState { size, read_only, block, store: HashMap::new(), keep: false, bufs, queue: VecDeque::new(), flushes: 0,
                    ready: 0, fail: false, order: 0 }
    }

    /// What sector `n` holds, where the disk keeps what it is written.
    fn sector(&self, n: u64) -> [u8; 512] {
        self.store.get(&n).copied().unwrap_or([0u8; 512])
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Requests handed over and not yet taken back.
    pub fn queued(&self) -> usize {
        self.queue.len()
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
    fn block_size(&self) -> u32 {
        self.0.lock().unwrap().block as u32
    }
    fn with_buffer(&mut self, slot: usize, use_it: &mut dyn FnMut(&mut [u8]) -> bool) -> bool {
        let mut s = self.0.lock().unwrap();
        /* The device reaches a buffer only while the request of it is not
         * with the backend: a buffer the device fills or empties meanwhile
         * is a request's data changed under the backend serving it. */
        invariant!(!s.queue.iter().any(|r| r.slot() == slot), "buffer {} reached while its request is served", slot);
        match s.bufs.get_mut(slot) {
            Some(buf) => use_it(buf),
            None => false,
        }
    }
    fn submit(&mut self, req: blk::Request) {
        let mut s = self.0.lock().unwrap();
        if req.op() == blk::Op::Flush {
            s.flushes += 1;
        }
        invariant!(s.queue.len() < blk::IN_FLIGHT, "a request past the {} in flight", blk::IN_FLIGHT);
        invariant!(req.slot() < blk::IN_FLIGHT && !s.queue.iter().any(|r| r.slot() == req.slot()),
                   "a request handed over in slot {}, which has one", req.slot());
        let len = req.len() as u64;
        match req.op() {
            blk::Op::Read | blk::Op::Write => {
                invariant!(req.offset() % blk::SECTOR == 0, "a request at byte {}, not a sector", req.offset());
                /* Whole blocks of the backend's, a guest's part of one
                 * served through the blocks around it */
                invariant!(req.offset() % s.block == 0 && len % s.block == 0,
                           "a request of {} bytes at {} handed to a backend of {}-byte blocks", len, req.offset(),
                           s.block);
                invariant!(req.offset().checked_add(len).is_some_and(|end| end <= s.size),
                           "a request of {} bytes at {} on a disk of {}", len, req.offset(), s.size);
            }
            blk::Op::Flush => {}
            blk::Op::Discard | blk::Op::WriteZeroes => {
                /* The device's copy, checked: what the backend acts on */
                let ranges: Vec<blk::Range> = req.ranges().collect();
                invariant!(!ranges.is_empty() && ranges.len() <= 8, "{} ranges handed over", ranges.len());
                invariant!(s.block == blk::SECTOR || req.op() == blk::Op::Discard,
                           "a write of zeros handed to a backend of {}-byte blocks, which is not offered one", s.block);
                for r in &ranges {
                    invariant!(r.offset % s.block == 0 && r.len % s.block == 0,
                               "a range of {} bytes at {}, not whole blocks of {}", r.len, r.offset, s.block);
                    invariant!(r.offset.checked_add(r.len).is_some_and(|end| end <= s.size),
                               "a range of {} bytes at {} on a disk of {}", r.len, r.offset, s.size);
                    invariant!(!r.unmap || req.op() == blk::Op::WriteZeroes, "an unmap flag handed over on a discard");
                }
                RANGES_SERVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        invariant!(!(s.read_only && req.op().writes()), "a write, discard or write of zeros handed to a read-only disk");
        if req.op() == blk::Op::Read {
            let slot = req.slot();
            s.bufs[slot][..len as usize].fill(0xA5);
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
        if s.queue.is_empty() {
            return None;
        }
        let at = (s.order % s.queue.len() as u64) as usize;
        s.order = s.order.rotate_left(17).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        if at != 0 {
            DISK_OUT_OF_ORDER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut req = s.queue.remove(at)?;
        req.done(!fail);
        if s.keep && !fail {
            /* Served now, in the order the backend chose: what it holds read
             * or written, a sector at a time */
            let (slot, first, sectors) = (req.slot(), req.offset() / blk::SECTOR, req.len() as u64 / blk::SECTOR);
            for i in 0..sectors {
                let at = (i * blk::SECTOR) as usize..((i + 1) * blk::SECTOR) as usize;
                match req.op() {
                    blk::Op::Read => {
                        let v = s.sector(first + i);
                        s.bufs[slot][at].copy_from_slice(&v);
                    }
                    blk::Op::Write => {
                        let mut v = [0u8; 512];
                        v.copy_from_slice(&s.bufs[slot][at]);
                        s.store.insert(first + i, v);
                    }
                    _ => {}
                }
            }
        }
        Some(req)
    }
}

/// A request, as a driver makes one: its header -- type and sector -- the
/// data, and the status byte the device writes; in memory of `limit` bytes.
pub fn blk_segments(mem: &dyn Memory, r: &mut Input, limit: u64) -> Vec<(u64, u32, bool)> {
    let header = gpa_in(r, limit) & !7;
    let kind = r.pick(&[0u32, 1, 4, 8, 11, 13, 0xFFFF_FFFF]);
    let sector = if r.bool() { r.below(1 << 20) } else { r.value64() };
    let mut h = [0u8; 16];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    h[8..].copy_from_slice(&sector.to_le_bytes());
    mem.put(header, &h);
    let mut segs = vec![(header, if r.u8() < 240 { 16 } else { r.value32() }, false)];
    if (kind == 11 || kind == 13) && r.u8() < 200 {
        /* A discard's or a write of zeros' ranges, as a driver lays them
         * out: a sector, a count and flags each -- mostly plausible ones. */
        let count = if r.u8() < 230 { 1 + r.below(8) } else { r.below(12) };
        let at = gpa_in(r, limit) & !7;
        for i in 0..count {
            let mut range = [0u8; 16];
            let first = if r.u8() < 220 { r.below(1 << 14) } else { r.value64() };
            let sectors = if r.u8() < 220 { r.below(1 << 12) as u32 } else { r.value32() };
            let flags = if r.u8() < 200 { (r.u8() & 1) as u32 } else { r.value32() };
            range[..8].copy_from_slice(&first.to_le_bytes());
            range[8..12].copy_from_slice(&sectors.to_le_bytes());
            range[12..].copy_from_slice(&flags.to_le_bytes());
            mem.put(at.wrapping_add(16 * i), &range);
        }
        let len = if r.u8() < 230 { 16 * count as u32 } else { r.value32() };
        segs.push((at, len, false));
    } else {
        for _ in 0..r.below(4) {
            let len = if r.bool() { 512 * (1 + r.below(128)) as u32 } else { r.value32() };
            segs.push((gpa_in(r, limit), len, kind != 1));
        }
    }
    segs.push((gpa_in(r, limit), if r.u8() < 240 { 1 } else { r.value32() }, true));
    segs
}

fn blk_request(mem: &GuestMemory, q: &mut DriverQueue, r: &mut Input) {
    let segs = blk_segments(mem, r, MEM);
    chain(mem, q, r, &segs);
}

/// The most of the guest's memory one call into the disk -- a notify, a
/// round, each a pass under the platform's lock in the hypervisor -- reads
/// and writes: its data held to `blk::PASS_BYTES` and the request that
/// crosses it, and for each chain of a pass (a queue's worth at most) its
/// descriptors -- a whole table's -- its header, ranges and status, and the
/// rings' words. Unbounded, a notify of a queue full of the largest writes
/// copied every one of them: 16 MiB.
const PASS_MOST: u64 = (blk::PASS_BYTES + blk::REQUEST_BYTES) as u64 + QUEUE_SIZE * (16 * QUEUE_SIZE + 256);

/// `call`, into the disk, held to what one pass of it may move.
fn held_to_a_pass<R>(mem: &GuestMemory, call: impl FnOnce() -> R) -> R {
    let before = mem.moved();
    let result = call();
    let moved = mem.moved() - before;
    invariant!(moved <= PASS_MOST, "a call into the disk moved {} bytes of the guest's memory, past the {} a pass \
               may under the platform's lock", moved, PASS_MOST);
    result
}

fn blk_target(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let since = time::boot_time_ns();
    let size = if r.bool() { blk::SECTOR * r.below(1 << 16) } else { r.value64() & !(blk::SECTOR - 1) };
    /* A backend of 4 KiB blocks, as a volume of NBS's is, on some inputs:
     * shown to the guest in sectors still (`blk::Pad`) */
    let block = if r.u8() < 96 { 4096 } else { blk::SECTOR };
    let state = Arc::new(Mutex::new(DiskState::with_block(size, r.bool(), block)));
    let mut disk = Blk::new(Box::new(Disk(state.clone())), "fuzz", 0).expect("a disk");
    if r.bool() {
        disk.offer_msix(2);
    }
    /* Its rings copied rather than mapped, on some inputs: what a queue is
     * left with when the kernel has no address for them. */
    if r.u8() < 48 {
        mem.refuse_maps(true);
    }
    let mut q = DriverQueue::default();
    while let Some(op) = r.op(19) {
        match op {
            0 => {
                disk.io_read(r.below(u64::from(blk::BAR_SIZE)) as u16, r.io_size());
            }
            1 => {
                let (offset, size, value) = (r.below(u64::from(blk::BAR_SIZE)) as u16, r.io_size(), r.value32());
                held_to_a_pass(&mem, || disk.io_write(offset, size, value, &mem, time::boot_time_ns()));
            }
            2 => {
                let mut write = |offset: u16, size: u8, value: u32| {
                    disk.io_write(offset, size, value, &mem, time::boot_time_ns());
                };
                let index = if r.u8() < 240 { 0 } else { r.u16() };
                setup_queue(r, &mut write, &mut q, index);
            }
            3 => {
                if r.bool() {
                    /* What the driver takes: everything offered, mostly --
                     * discard and a write of zeros among it. */
                    let features = if r.u8() < 200 { u32::MAX } else { r.value32() };
                    disk.io_write(GUEST_FEATURES, 4, features, &mem, time::boot_time_ns());
                }
                let status = r.pick(&[0, 1, 3, 7, 15, 0x80, 0xFF]);
                disk.io_write(DEVICE_STATUS, 1, status, &mem, time::boot_time_ns());
            }
            4 => {
                let queue = if r.u8() < 240 { 0 } else { r.u16() };
                held_to_a_pass(&mem, || disk.io_write(QUEUE_NOTIFY, 2, u32::from(queue), &mem, time::boot_time_ns()));
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
                held_to_a_pass(&mem, || disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns()));
            }
            8 | 9 => {
                held_to_a_pass(&mem, || disk.poll(&mem));
            }
            10 => {
                let mut s = state.lock().unwrap();
                s.ready += 1 + r.below(4) as usize;
                s.fail = r.u8() % 8 == 0;
                s.order = if r.bool() { 0 } else { r.u64() };
            }
            11 => {
                disk.set_msix_enabled(r.bool());
                disk.line();
                disk.broken();
            }
            12 => {
                /* A request, then another CPU posting it again and again
                 * while the device takes from the ring: one answered at once
                 * -- a GET_ID, one past the disk -- takes no buffer, so the
                 * pass is not ended for want of one and has to end itself. */
                blk_request(&mem, &mut q, r);
                let notify = r.bool();
                racing(&mem, &mut q, || {
                    held_to_a_pass(&mem, || {
                        if notify {
                            disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns());
                        } else {
                            disk.poll(&mem);
                        }
                    })
                });
            }
            13 => {
                /* The largest reads and writes the driver may send, more at
                 * once than the device takes: their data copied a pass's
                 * worth at a time (`blk::PASS_BYTES`), the writes' as they
                 * are taken and the reads' as they are given back. */
                let sectors = blk::REQUEST_BYTES as u64 / blk::SECTOR;
                for _ in 0..1 + r.below(2 * blk::IN_FLIGHT as u64) {
                    let kind = if r.bool() { 1u32 } else { 0 };
                    let sector = r.below((size / blk::SECTOR).saturating_sub(sectors).max(1));
                    let header = gpa(r) & !7;
                    let mut h = [0u8; 16];
                    h[..4].copy_from_slice(&kind.to_le_bytes());
                    h[8..].copy_from_slice(&sector.to_le_bytes());
                    mem.put(header, &h);
                    let (data, status) = (r.below(MEM - blk::REQUEST_BYTES as u64), gpa(r));
                    chain(&mem, &mut q, r, &[(header, 16, false), (data, blk::REQUEST_BYTES as u32, kind == 0),
                                              (status, 1, true)]);
                }
                held_to_a_pass(&mem, || disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns()));
            }
            14 => {
                /* Its queue never set up after a reset, EVENT_IDX taken, and
                 * the index a ring at page 0 would have saying a chain is
                 * there: a notify finds no queue to work on. Nothing of page
                 * 0 is read or written, the device is not stopped over what
                 * it found there, and the pass does not end owing more -- it
                 * once asked for a kick at page 0 and came back at once, for
                 * good. */
                blk_reset_drained(&mut disk, &mem, &state);
                disk.io_write(GUEST_FEATURES, 4, u32::MAX, &mem, time::boot_time_ns());
                q = DriverQueue::default();
                let _ = mem.write(PAGE + 2, &1u16.to_le_bytes());
                let mut low = vec![0u8; 3 * PAGE as usize];
                let _ = mem.read(0, &mut low);
                held_to_a_pass(&mem, || disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns()));
                let mut after = vec![0u8; 3 * PAGE as usize];
                let _ = mem.read(0, &mut after);
                invariant!(!disk.more() && disk.broken().is_none() && after == low,
                           "a notify of a queue that is down: more {}, broken {:?}, page 0 written {}", disk.more(),
                           disk.broken(), after != low);
            }
            15 => {
                /* Requests out at a reset, the old driver's; the new one's
                 * queue set up and a request of its made: it is not handed
                 * to the backend until the old ones are back -- beside them,
                 * an old write and a new one to the same blocks could land in
                 * either order. */
                blk_queue_at(&mut disk, &mem, &mut q, u32::MAX);
                for _ in 0..1 + r.below(4) {
                    blk_read(&mem, &mut q, r);
                }
                disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns());
                disk.io_write(DEVICE_STATUS, 1, 0, &mem, time::boot_time_ns());
                let old = state.lock().unwrap().queued();
                blk_queue_at(&mut disk, &mem, &mut q, u32::MAX);
                blk_read(&mem, &mut q, r);
                disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns());
                let now = state.lock().unwrap().queued();
                invariant!(old == 0 || now == old, "{} of the old driver's requests out at its reset, and {} with the \
                           backend after the new driver's notify", old, now);
            }
            16 => {
                /* A driver that took no FLUSH, which may take its disk to be
                 * write-through: a write it makes is flushed before it is
                 * answered -- the flush handed over as the write comes back,
                 * and the write's status still unwritten until it does. */
                blk_reset_drained(&mut disk, &mem, &state);
                state.lock().unwrap().fail = false;
                blk_queue_at(&mut disk, &mem, &mut q, u32::MAX & !(1 << 9));
                let status = blk_write(&mem, &mut q, r);
                disk.io_write(QUEUE_NOTIFY, 2, 0, &mem, time::boot_time_ns());
                if state.lock().unwrap().queued() == 1 {
                    if state.lock().unwrap().block > blk::SECTOR {
                        /* A sector of a larger block: the blocks around it
                         * read first, the write handed over as they come */
                        state.lock().unwrap().ready = 1;
                        disk.poll(&mem);
                    }
                    state.lock().unwrap().ready = 1;
                    disk.poll(&mem);
                    let flush = state.lock().unwrap().queue.front().map(|q| q.op()) == Some(blk::Op::Flush);
                    let mut s = [0u8];
                    let _ = mem.read(status, &mut s);
                    invariant!(flush && s[0] == BLK_UNSET, "a write of a driver with no FLUSH served: {} flush handed \
                               over, its status {:#x}", if flush { "a" } else { "no" }, s[0]);
                    state.lock().unwrap().ready = 1;
                    disk.poll(&mem);
                    let _ = mem.read(status, &mut s);
                    invariant!(s[0] == 0, "a write flushed, answered {:#x}", s[0]);
                }
            }
            17 => blk_512e(&mut disk, &mem, &state, &mut q, r),
            18 => blk_parts(&mut disk, &mem, &state, &mut q, r),
            _ => {}
        }
    }
    PASSES_CUT.fetch_add(disk.stats.cut, std::sync::atomic::Ordering::Relaxed);
    mem.held_to_map_budget(since, 1);
}

/// Where the 512e scenario's requests' buffers go, past `BLK_BUFS`'s: a
/// request's room each, its header, its data and its status.
const E_BUFS: u64 = 0x40000;
const E_ROOM: u64 = 0x4000;
/// The blocks its requests fall in: few, so that they share blocks.
const E_BLOCKS: u64 = 3;

/// A sector as the scenario's write `id` writes it: whose, and which sector.
fn e_sector(id: u8, sector: u64) -> [u8; 512] {
    let mut b = [0u8; 512];
    for (i, x) in b.iter_mut().enumerate() {
        *x = id ^ (i as u8) ^ (sector as u8).wrapping_mul(31);
    }
    b[0] = id;
    b[1..9].copy_from_slice(&sector.to_le_bytes());
    b
}

/// One of the scenario's requests: a read or a write of `count` sectors from
/// the region's `at`, and where its data and its status are.
struct ERequest {
    write: bool,
    at: u64,
    count: u64,
    id: u8,
    data: u64,
    status: u64,
}

/// The disk a guest of 512-byte sectors has on a backend of larger blocks
/// (`blk::Pad`): reads and writes of sectors in a few blocks, all at once,
/// the backend serving them in any order and failing some -- a driver that
/// took FLUSH or one that did not. Then every request is answered; every
/// sector one write alone of them wrote, and that was answered done, holds
/// what it wrote -- a write of part of a block written back over another
/// write beside it would have undone it -- one no write wrote holds what it
/// held, and any other one of those; a read's sectors were one of them too;
/// and the disk read back through the device is what it holds.
fn blk_512e(disk: &mut Blk, mem: &GuestMemory, state: &Arc<Mutex<DiskState>>, q: &mut DriverQueue, r: &mut Input) {
    let (block, size, read_only) = {
        let s = state.lock().unwrap();
        (s.block, s.size, s.read_only)
    };
    let per = block / blk::SECTOR;
    let blocks = size / block;
    if per == 1 || read_only || blocks < E_BLOCKS {
        return;
    }
    blk_reset_drained(disk, mem, state);
    {
        let mut s = state.lock().unwrap();
        s.keep = true;
        s.fail = false;
    }
    let flushes = r.bool();
    blk_queue_at(disk, mem, q, if flushes { u32::MAX } else { u32::MAX & !(1 << 9) });
    let first = r.below(blocks - E_BLOCKS + 1) * per;
    let span = E_BLOCKS * per;
    let before: Vec<[u8; 512]> = {
        let s = state.lock().unwrap();
        (0..span).map(|k| s.sector(first + k)).collect()
    };

    let n = 2 + r.below(7);
    let mut reqs = Vec::new();
    for i in 0..n {
        let write = r.u8() < 170;
        let at = r.below(span);
        let count = (1 + r.below(2 * per)).min(span - at);
        let id = 1 + i as u8;
        let room = E_BUFS + i * E_ROOM;
        let (header, data) = (room, room + 0x100);
        let status = data + count * blk::SECTOR;
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&u32::from(write).to_le_bytes());
        h[8..].copy_from_slice(&(first + at).to_le_bytes());
        mem.put(header, &h);
        for k in 0..count {
            let bytes = if write { e_sector(id, first + at + k) } else { [0xEEu8; 512] };
            mem.put(data + k * blk::SECTOR, &bytes);
        }
        mem.put(status, &[BLK_UNSET]);
        chain_made_well(mem, q, &[(header, 16, false), (data, (count * blk::SECTOR) as u32, !write), (status, 1, true)]);
        reqs.push(ERequest { write, at, count, id, data, status });
    }
    disk.io_write(QUEUE_NOTIFY, 2, 0, mem, time::boot_time_ns());

    let status_of = |e: &ERequest| {
        let mut b = [0u8];
        let _ = mem.read(e.status, &mut b);
        b[0]
    };
    /* Each request is three of the backend's at most -- a span read, its
     * write, a flush behind it -- and a round serves one at least */
    let mut rounds = 0u64;
    while reqs.iter().any(|e| status_of(e) == BLK_UNSET) && rounds < 3 * n + 8 {
        rounds += 1;
        {
            let mut s = state.lock().unwrap();
            s.ready += 1 + r.below(3) as usize;
            s.order = r.u64();
            s.fail = r.u8() % 16 == 0;
        }
        disk.poll(mem);
    }
    let statuses: Vec<u8> = reqs.iter().map(&status_of).collect();
    invariant!(statuses.iter().all(|&st| st == 0 || st == 1), "the 512e batch on {}-byte blocks: {} rounds, statuses \
               {:?} -- a request never answered, with the backend serving all it was handed", block, rounds, statuses);

    let s = state.lock().unwrap();
    for k in 0..span {
        let now = s.sector(first + k);
        let over: Vec<&ERequest> = reqs.iter().filter(|e| e.write && e.at <= k && k < e.at + e.count).collect();
        let done: Vec<&&ERequest> = over.iter().filter(|e| status_of(e) == 0).collect();
        if over.is_empty() {
            invariant!(now == before[k as usize], "sector {} of a {}-byte block, which no write of the batch wrote, \
                       changed", first + k, block);
        } else if over.len() == 1 && done.len() == 1 {
            E_ALONE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            invariant!(now == e_sector(over[0].id, first + k), "sector {} of a {}-byte block, written by write {} \
                       alone and answered done, holds {}: written back over by another write of its block",
                       first + k, block, over[0].id, if now == before[k as usize] { "what it held before" } else {
                       "something else" });
        } else {
            invariant!(now == before[k as usize] || over.iter().any(|e| now == e_sector(e.id, first + k)),
                       "sector {} of a {}-byte block holds what neither it held nor a write over it wrote",
                       first + k, block);
        }
    }
    for e in reqs.iter().filter(|e| !e.write && status_of(e) == 0) {
        for k in 0..e.count {
            let mut got = [0u8; 512];
            let _ = mem.read(e.data + k * blk::SECTOR, &mut got);
            let sector = e.at + k;
            let over: Vec<&ERequest> =
                reqs.iter().filter(|w| w.write && w.at <= sector && sector < w.at + w.count).collect();
            invariant!(got == before[sector as usize] || over.iter().any(|w| got == e_sector(w.id, first + sector)),
                       "a read of sector {} of a {}-byte block got what neither it held nor a write over it wrote",
                       first + sector, block);
        }
    }
    drop(s);

    /* Read back through the device, the batch over: what the disk holds */
    let room = E_BUFS + n * E_ROOM;
    let (header, data) = (room, room + 0x100);
    let status = data + span * blk::SECTOR;
    let mut h = [0u8; 16];
    h[8..].copy_from_slice(&first.to_le_bytes());
    mem.put(header, &h);
    mem.put(status, &[BLK_UNSET]);
    chain_made_well(mem, q, &[(header, 16, false), (data, (span * blk::SECTOR) as u32, true), (status, 1, true)]);
    disk.io_write(QUEUE_NOTIFY, 2, 0, mem, time::boot_time_ns());
    for _ in 0..4 {
        {
            let mut s = state.lock().unwrap();
            s.ready += 1;
            s.fail = false;
        }
        disk.poll(mem);
    }
    let mut st = [0u8];
    let _ = mem.read(status, &mut st);
    invariant!(st[0] == 0, "the region read back answered {:#x}", st[0]);
    let s = state.lock().unwrap();
    for k in 0..span {
        let mut got = [0u8; 512];
        let _ = mem.read(data + k * blk::SECTOR, &mut got);
        invariant!(got == s.sector(first + k), "sector {} of a {}-byte block read back unlike what the disk holds",
                   first + k, block);
    }
    drop(s);
    state.lock().unwrap().keep = false;
    E_BATCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// EDK2's VirtioBlkDxe puts a whole buffer in one descriptor and does not
/// take SIZE_MAX or SEG_MAX. Its 896 KiB read is served in backend-sized
/// parts, with the guest's chain and one status retained across all of them.
fn blk_parts(disk: &mut Blk, mem: &GuestMemory, state: &Arc<Mutex<DiskState>>, q: &mut DriverQueue, r: &mut Input) {
    const DATA: u64 = 0x20000;
    const HEADER: u64 = 0x5000;
    const STATUS: u64 = 0x6000;
    const BYTES: usize = 0xE0000;

    blk_reset_drained(disk, mem, state);
    let flushes = r.bool();
    blk_queue_at(disk, mem, q, if flushes { u32::MAX } else { u32::MAX & !(1 << 9) });
    mem.put(q.used() + 2, &0u16.to_le_bytes());
    let split = r.bool();
    let fail = r.bool();
    let block = state.lock().unwrap().block;
    {
        let mut s = state.lock().unwrap();
        s.size = 2 * MEM;
        s.read_only = false;
        s.keep = true;
        s.store.clear();
    }
    let original: Vec<u8> = (0..BYTES).map(|i| (i as u8).wrapping_mul(29) ^ (i >> 8) as u8).collect();
    mem.put(DATA, &original);
    let before = disk.stats;
    let flushes_before = state.lock().unwrap().flushes;
    let status = blk_part_request(disk, mem, state, q, HEADER, STATUS, DATA, BYTES, 1, split, None);
    invariant!(status == 0, "a large write answered {:#x}", status);
    invariant!(disk.stats.writes == before.writes + 1 && disk.stats.written_bytes == before.written_bytes + BYTES as u64,
               "a large write counted {} times and {} bytes", disk.stats.writes - before.writes,
               disk.stats.written_bytes - before.written_bytes);
    invariant!(flushes || state.lock().unwrap().flushes == flushes_before + 1,
               "a large write without FLUSH was not flushed once");

    mem.put(DATA, &vec![0; BYTES]);
    let status = blk_part_request(disk, mem, state, q, HEADER, STATUS, DATA, BYTES, 0, !split, None);
    invariant!(status == 0, "a large read answered {:#x}", status);
    let mut got = vec![0; BYTES];
    let _ = mem.read(DATA, &mut got);
    invariant!(got == original, "a large read differs from what its large write stored");
    invariant!(disk.stats.reads == before.reads + 1 && disk.stats.read_bytes == before.read_bytes + BYTES as u64,
               "a large read counted {} times and {} bytes", disk.stats.reads - before.reads,
               disk.stats.read_bytes - before.read_bytes);

    if fail {
        let changed: Vec<u8> = original.iter().map(|b| b ^ 0xFF).collect();
        mem.put(DATA, &changed);
        let errors = disk.stats.errors;
        let round = if block == blk::SECTOR { 1 } else { 2 };
        let status = blk_part_request(disk, mem, state, q, HEADER, STATUS, DATA, BYTES, 1, split, Some(round));
        invariant!(status == 1 && disk.stats.errors == errors + 1,
                   "a failed part answered {:#x}, errors {} -> {}", status, errors, disk.stats.errors);
        mem.put(DATA, &vec![0; BYTES]);
        let status = blk_part_request(disk, mem, state, q, HEADER, STATUS, DATA, BYTES, 0, split, None);
        invariant!(status == 0, "a read after a failed part answered {:#x}", status);
        let _ = mem.read(DATA, &mut got);
        let first = blk::REQUEST_BYTES - if block == blk::SECTOR { 0 } else { blk::SECTOR as usize };
        invariant!(got[..first] == changed[..first] && got[first..] == original[first..],
                   "a failed part did not stop the rest of its request");
    }
    let mut s = state.lock().unwrap();
    s.keep = false;
    s.fail = false;
    s.ready = 0;
}

/// One request of `bytes` from sector 1, in one data descriptor as EDK2 makes
/// it or split in two. `fail_round` fails that backend completion.
#[allow(clippy::too_many_arguments)]
fn blk_part_request(disk: &mut Blk, mem: &GuestMemory, state: &Arc<Mutex<DiskState>>, q: &mut DriverQueue,
                    header: u64, status: u64, data: u64, bytes: usize, kind: u32, split: bool,
                    fail_round: Option<usize>) -> u8 {
    let mut h = [0u8; 16];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    h[8..].copy_from_slice(&1u64.to_le_bytes());
    mem.put(header, &h);
    mem.put(status, &[BLK_UNSET]);
    let write = kind == 0;
    if split {
        chain_made_well(mem, q, &[(header, 16, false), (data, (bytes / 2) as u32, write),
                                  (data + (bytes / 2) as u64, (bytes / 2) as u32, write), (status, 1, true)]);
    } else {
        chain_made_well(mem, q, &[(header, 16, false), (data, bytes as u32, write), (status, 1, true)]);
    }
    let mut used = [0u8; 2];
    let _ = mem.read(q.used() + 2, &mut used);
    let before = u16::from_le_bytes(used);
    disk.io_write(QUEUE_NOTIFY, 2, 0, mem, time::boot_time_ns());
    for round in 0..16 {
        let left = state.lock().unwrap().queued();
        if left == 0 {
            break;
        }
        {
            let mut s = state.lock().unwrap();
            s.ready = left;
            s.fail = fail_round == Some(round);
            if s.fail {
                let front = s.queue.front().expect("a queued request");
                invariant!(front.len() <= blk::REQUEST_BYTES, "a backend request of {} bytes", front.len());
            }
        }
        disk.poll(mem);
        state.lock().unwrap().fail = false;
        let mut answer = [0u8];
        let _ = mem.read(status, &mut answer);
        if answer[0] != BLK_UNSET {
            let _ = mem.read(q.used() + 2, &mut used);
            invariant!(u16::from_le_bytes(used) == before.wrapping_add(1), "one guest request returned more than one chain");
            invariant!(state.lock().unwrap().queued() == 0, "a request answered with backend work still out");
            return answer[0];
        }
    }
    panic!("a request of {} bytes did not finish", bytes);
}

/// What a status byte holds before the device writes it.
const BLK_UNSET: u8 = 0xEE;

/// The disk reset, and what its backend had of the old driver's taken back.
fn blk_reset_drained(disk: &mut Blk, mem: &GuestMemory, state: &Arc<Mutex<DiskState>>) {
    disk.io_write(DEVICE_STATUS, 1, 0, mem, time::boot_time_ns());
    for _ in 0..blk::IN_FLIGHT {
        let left = state.lock().unwrap().queued();
        if left == 0 {
            break;
        }
        {
            let mut s = state.lock().unwrap();
            s.ready += left;
            s.fail = false;
        }
        disk.poll(mem);
    }
    state.lock().unwrap().ready = 0;
}

/// The disk's queue set up at page 1 by a driver taking `features`.
fn blk_queue_at(disk: &mut Blk, mem: &GuestMemory, q: &mut DriverQueue, features: u32) {
    disk.io_write(GUEST_FEATURES, 4, features, mem, time::boot_time_ns());
    disk.io_write(QUEUE_SELECT, 2, 0, mem, time::boot_time_ns());
    disk.io_write(QUEUE_PFN, 4, 1, mem, time::boot_time_ns());
    *q = DriverQueue::at(1);
}

/// Where well-made requests' buffers go: past the rings at page 1.
const BLK_BUFS: u64 = 0x8000;

/// A read of one sector, well made, on `q`.
fn blk_read(mem: &GuestMemory, q: &mut DriverQueue, r: &mut Input) {
    blk_one(mem, q, r, 0);
}

/// A write of one sector, well made, on `q`: where its status byte is.
fn blk_write(mem: &GuestMemory, q: &mut DriverQueue, r: &mut Input) -> u64 {
    blk_one(mem, q, r, 1)
}

fn blk_one(mem: &GuestMemory, q: &mut DriverQueue, r: &mut Input, kind: u32) -> u64 {
    let at = BLK_BUFS + (q.avail_idx % 16) as u64 * PAGE;
    let (header, data, status) = (at, at + 0x100, at + 0x300);
    let mut h = [0u8; 16];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    mem.put(header, &h);
    mem.put(status, &[BLK_UNSET]);
    let _ = r;
    chain_made_well(mem, q, &[(header, 16, false), (data, blk::SECTOR as u32, kind == 0), (status, 1, true)]);
    status
}

/// `segs` a chain in `q`, as a driver that makes no mistake makes one:
/// direct descriptors, each pointing at the next, the index one on.
fn chain_made_well(mem: &dyn Memory, q: &mut DriverQueue, segs: &[(u64, u32, bool)]) {
    if q.pfn == 0 || segs.is_empty() {
        return;
    }
    let head = q.next_desc % QUEUE_SIZE;
    for (i, &(addr, len, write)) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let flags = if write { DESC_WRITE } else { 0 } | if last { 0 } else { DESC_NEXT };
        let next = ((q.next_desc + 1) % QUEUE_SIZE) as u16;
        mem.put(q.desc(q.next_desc), &descriptor(addr, len, flags, next));
        q.next_desc += 1;
    }
    let slot = u64::from(q.avail_idx) % QUEUE_SIZE;
    mem.put(q.avail() + 4 + 2 * slot, &(head as u16).to_le_bytes());
    q.avail_idx = q.avail_idx.wrapping_add(1);
    mem.put(q.avail() + 2, &q.avail_idx.to_le_bytes());
    mem.put(q.avail(), &0u16.to_le_bytes());
}

/// The guests' switch as a NIC's backend sees it: frames sent checked, and
/// frames to deliver, as the fuzzer makes them.
#[derive(Default)]
pub struct NicState {
    /// Frames for the guest -- whole, their checksum left to finish, or
    /// sends carried whole.
    pub rx: VecDeque<(Vec<u8>, crate::offload::Rx)>,
    /// What the device last told it its guest takes (`Backend::takes`),
    /// which `netrx` holds to the driver's features.
    pub takes: crate::offload::Takes,
    /// Frames sent: each a frame on the switch.
    pub sent: u64,
}

pub struct Nic(pub Arc<Mutex<NicState>>);

impl net::Backend for Nic {
    fn send(&mut self, frame: &[u8]) {
        invariant!(frame.len() <= net::MAX_FRAME, "a frame of {} bytes sent, past {}", frame.len(), net::MAX_FRAME);
        self.0.lock().unwrap().sent += 1;
    }
    /// The next frame finished -- whole, before it is cut to `buf` -- as
    /// the switch's own `recv` hands one; a send carried whole, which no
    /// frame's buffer holds, dropped.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        use crate::offload::Rx;
        let mut s = self.0.lock().unwrap();
        loop {
            let (mut f, rx) = s.rx.pop_front()?;
            match rx {
                Rx::Whole(_) => continue,
                Rx::Partial(p) => p.complete(&mut f),
                Rx::Done => {}
            }
            let n = f.len().min(buf.len());
            buf[..n].copy_from_slice(&f[..n]);
            return Some(n);
        }
    }
    /// What waits, as the switch hands it over: each as it was put in,
    /// whatever the device has said its guest takes since -- a frame put in
    /// before its driver took other features is the device's to finish, or
    /// drop -- and a send carried whole only when it fits.
    fn recv_with(&mut self, buf: &mut [u8]) -> Option<(usize, crate::offload::Rx)> {
        let mut s = self.0.lock().unwrap();
        loop {
            let (f, rx) = s.rx.pop_front()?;
            if matches!(rx, crate::offload::Rx::Whole(_)) && f.len() > buf.len() {
                continue;
            }
            let n = f.len().min(buf.len());
            buf[..n].copy_from_slice(&f[..n]);
            return Some((n, rx));
        }
    }
    fn takes(&mut self, takes: crate::offload::Takes) {
        self.0.lock().unwrap().takes = takes;
    }
}

/// The most of the guest's memory one transmit pass reads: its bound's bytes
/// and the send that crosses it, and for each chain of it -- a queue's worth
/// at most -- its descriptors and the rings' words.
const NET_PASS_MOST: u64 = (net::TX_PASS_BYTES + TxHeader::LEN + offload::MAX_SEND) as u64 + QUEUE_SIZE * 64;

/// A send of TCP over IPv4 with `payload` bytes behind its headers, as a
/// driver that leaves the cutting to the device hands it over: its
/// checksums the device's to make.
fn tcp4_send(payload: usize) -> Vec<u8> {
    let mut f = vec![0u8; 54 + payload];
    f[..6].copy_from_slice(&[2, 0, 0, 0, 0x64, 1]);
    f[6..12].copy_from_slice(&[2, 0, 0, 0, 0x64, 2]);
    f[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
    f[14] = 0x45;
    f[16..18].copy_from_slice(&((40 + payload) as u16).to_be_bytes());
    f[20..22].copy_from_slice(&0x4000u16.to_be_bytes());
    f[22] = 64;
    f[23] = 6;
    f[26..30].copy_from_slice(&[10, 0, 100, 2]);
    f[30..34].copy_from_slice(&[10, 0, 100, 1]);
    f[34..36].copy_from_slice(&40000u16.to_be_bytes());
    f[36..38].copy_from_slice(&80u16.to_be_bytes());
    f[46] = 5 << 4;
    f[47] = 0x18;
    for (i, b) in f[54..].iter_mut().enumerate() {
        *b = i as u8;
    }
    f
}

/// After a round of the NIC at `now` (`poll`): no receive interrupt still
/// owed with its time up -- the round raises what is due -- and what it
/// promised its backend, the next time it looks, ahead of `now`. Not after a
/// notify: a transmit queue's runs no receive round, and what is due waits
/// for the next, which a due time already past makes at once.
fn rx_promise_kept(nic: &Net, now: u64) {
    if let Some(due) = nic.rx_due(now) {
        invariant!(due > now, "a receive interrupt owed since {} ns, and the round at {} did not raise it", due, now);
    }
}

fn net_target(r: &mut Input) {
    let mem = GuestMemory::with_size(MEM);
    let since = time::boot_time_ns();
    let state = Arc::new(Mutex::new(NicState::default()));
    /* One queue pair, or several and a control queue after them. */
    let pairs = r.pick(&[1u16, 1, 2, 4, net::MAX_PAIRS]);
    let mut nic = Net::new(Box::new(Nic(state.clone())), [2, 0, 0, 0, 0x64, 2], pairs).expect("a NIC");
    if r.bool() {
        nic.offer_msix(net::vectors(pairs));
    }
    /* Receive interrupts moderated, on most inputs: what is owed comes by
     * the time the device said it would look again (`rx_due`). */
    if r.u8() < 192 {
        nic.set_rx_moderation(r.pick(&[1_000u64, 20_000, 100_000, 1_000_000]));
    }
    /* Its transmit offloads offered, mostly: a driver that accepts them may
     * send what is to be cut through the ring (`offload` holds the cutting
     * to a model). */
    if r.u8() < 48 {
        nic.set_offloads(false);
    }
    /* Its rings copied rather than mapped, on some inputs, as the disk's. */
    if r.u8() < 48 {
        mem.refuse_maps(true);
    }
    /* A cluster network's MTU told the driver, on some (F_MTU, bit 3; the
     * configuration's MTU after the MAC, the status and the pairs): offered
     * and read where it is one, neither where none. */
    let mtu = if r.u8() < 64 { r.pick(&[576u16, 1400, 1440]) } else { 0 };
    nic.set_mtu(mtu);
    let offered = nic.io_read(0, 4) & (1 << 3) != 0;
    let told = nic.io_read(crate::devices::virtio::DEVICE_CONFIG + 10, 2);
    invariant!(offered == (mtu != 0) && told == u32::from(mtu),
               "a NIC of MTU {} offers F_MTU {} and says {}", mtu, offered, told);
    let count = 2 * usize::from(pairs) + usize::from(pairs > 1);
    let mut queues = vec![DriverQueue::default(); count];
    while let Some(op) = r.op(17) {
        let which = r.below(count as u64) as usize;
        /* A pair's receive queue and its transmit queue. */
        let rx = 2 * r.below(u64::from(pairs)) as usize;
        let tx = 2 * r.below(u64::from(pairs)) as usize + 1;
        match op {
            0 => {
                nic.io_read(r.below(u64::from(net::BAR_SIZE)) as u16, r.io_size());
            }
            1 => {
                let (offset, size, value) = (r.below(u64::from(net::BAR_SIZE)) as u16, r.io_size(), r.value32());
                nic.io_write(offset, size, value, &mem, time::boot_time_ns());
            }
            2 => {
                let mut write = |offset: u16, size: u8, value: u32| {
                    nic.io_write(offset, size, value, &mem, time::boot_time_ns());
                };
                let index = if r.u8() < 240 { which as u16 } else { r.u16() };
                setup_queue(r, &mut write, &mut queues[which], index);
            }
            3 => {
                let status = r.pick(&[0, 1, 3, 7, 15, 0x80, 0xFF]);
                nic.io_write(DEVICE_STATUS, 1, status, &mem, time::boot_time_ns());
            }
            4 => {
                let queue = if r.u8() < 240 { which as u16 } else { r.u16() };
                nic.io_write(QUEUE_NOTIFY, 2, u32::from(queue), &mem, time::boot_time_ns());
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
                chain(&mem, &mut queues[rx], r, &segs);
            }
            7 => {
                /* A frame to send: the header, then the frame, in pieces. */
                let mut segs = vec![(gpa(r), if r.u8() < 230 { 10 } else { r.value32() }, false)];
                for _ in 0..r.below(4) {
                    let len = if r.bool() { r.below(1600) as u32 } else { r.value32() };
                    segs.push((gpa(r), len, false));
                }
                chain(&mem, &mut queues[tx], r, &segs);
            }
            8 | 9 => {
                let now = time::boot_time_ns();
                nic.poll(&mem, now);
                rx_promise_kept(&nic, now);
            }
            13 => {
                /* Time going by: a held receive interrupt's due, or not yet. */
                time::advance(r.pick(&[500u64, 20_000, 100_000, 2_000_000]));
            }
            10 => {
                let n = r.pick(&[0usize, 13, 14, 60, 64, 1500, 1514, 1515, 4000]);
                let frame: Vec<u8> = (0..n).map(|_| r.u8()).collect();
                state.lock().unwrap().rx.push_back((frame, crate::offload::Rx::Done));
            }
            11 => {
                nic.set_msix_enabled(r.bool());
                nic.line();
                nic.broken();
                nic.queue_pairs();
            }
            14 => {
                /* What the driver takes: everything offered, mostly --
                 * multiqueue and its control queue among it. */
                let features = if r.u8() < 200 { u32::MAX } else { r.value32() };
                nic.io_write(GUEST_FEATURES, 4, features, &mem, time::boot_time_ns());
            }
            16 => {
                /* A transmit ring of the largest sends of TCP, each to be cut
                 * by a segment size down to Linux's least: each cut and sent,
                 * none refused for its small segments -- refused under 256
                 * bytes once, a guest's connection to a peer with a small MSS
                 * stalled -- and a notify's pass held to its bound on the
                 * frames it makes and the bytes it copies, where a ring of
                 * them was once 65,536 frames and 16 MiB in one pass. */
                const F_CSUM: u32 = 1 << 0;
                const F_HOST_TSO4: u32 = 1 << 11;
                const F_MRG_RXBUF: u32 = 1 << 15;
                const TX: usize = 1;
                const TX_PFN: u64 = 4;
                const HEADER_AT: u64 = 0x1_0000;
                const FRAME_AT: u64 = 0x2_0000;
                nic.io_write(DEVICE_STATUS, 1, 0, &mem, time::boot_time_ns());
                nic.io_write(GUEST_FEATURES, 4, u32::MAX & !F_MRG_RXBUF, &mem, time::boot_time_ns());
                if nic.io_read(GUEST_FEATURES, 4) & (F_CSUM | F_HOST_TSO4) != F_CSUM | F_HOST_TSO4 {
                    continue;
                }
                nic.io_write(QUEUE_SELECT, 2, TX as u32, &mem, time::boot_time_ns());
                nic.io_write(QUEUE_PFN, 4, TX_PFN as u32, &mem, time::boot_time_ns());
                queues[TX] = DriverQueue::at(TX_PFN);
                let mss = r.pick(&[offload::MIN_SEGMENT, 64, 255, 1448]);
                let payload = r.pick(&[1usize, 1000, offload::MAX_SEND - 54]);
                let frame = tcp4_send(payload);
                let header = TxHeader { flags: offload::NEEDS_CSUM, gso_type: offload::GSO_TCPV4, hdr_len: 54,
                                        gso_size: mss as u16, csum_start: 34, csum_offset: 16 };
                mem.put(HEADER_AT, &header.bytes());
                mem.put(FRAME_AT, &frame);
                for _ in 0..QUEUE_SIZE / 2 {
                    chain_made_well(&mem, &mut queues[TX], &[(HEADER_AT, TxHeader::LEN as u32, false),
                                                             (FRAME_AT, frame.len() as u32, false)]);
                }
                let (sent, dropped, moved) = (state.lock().unwrap().sent, nic.stats.dropped, mem.moved());
                nic.io_write(QUEUE_NOTIFY, 2, TX as u32, &mem, time::boot_time_ns());
                let (frames, moved) = (state.lock().unwrap().sent - sent, mem.moved() - moved);
                let each = payload.div_ceil(mss).max(1) as u64;
                invariant!(nic.stats.dropped == dropped && frames >= each,
                           "sends of {} bytes cut at {}: {} frames sent, {} dropped", payload, mss, frames,
                           nic.stats.dropped - dropped);
                invariant!(frames <= net::TX_PASS_FRAMES as u64 + each && moved <= NET_PASS_MOST,
                           "one notify's pass: {} frames and {} bytes of the guest's memory, past its bound of {} \
                           frames and {} bytes and a send", frames, moved, net::TX_PASS_FRAMES, net::TX_PASS_BYTES);
            }
            15 => {
                /* A command on the control queue -- the last, where a driver
                 * that took multiqueue has it, or 2, where one that did not
                 * -- mostly how many pairs to use, then its notify. */
                let at = if r.bool() { count - 1 } else { 2.min(count - 1) };
                let (header, data, ack) = (gpa(r) & !1, gpa(r) & !1, gpa(r));
                let class_cmd = if r.u8() < 200 { [4u8, 0] } else { [r.u8(), r.u8()] };
                let _ = mem.write(header, &class_cmd);
                let n = if r.u8() < 200 { r.below(u64::from(pairs) + 2) as u16 } else { r.u16() };
                let _ = mem.write(data, &n.to_le_bytes());
                let segs = [(header, if r.u8() < 230 { 2 } else { r.value32() }, false),
                            (data, if r.u8() < 230 { 2 } else { r.value32() }, false),
                            (ack, if r.u8() < 230 { 1 } else { r.value32() }, true)];
                let n = 1 + r.below(3) as usize;
                chain(&mem, &mut queues[at], r, &segs[..n]);
                nic.io_write(QUEUE_NOTIFY, 2, at as u32, &mem, time::boot_time_ns());
            }
            _ => {
                /* Another CPU keeping a ring topped up while the device takes
                 * from it: a transmit ring through a notify, or a receive
                 * ring through a round of the devices with frames enough to
                 * fill it twice over. Either pass has to end itself. */
                if which % 2 == 1 {
                    let segs = [(gpa(r), 10, false), (gpa(r), 60, false)];
                    chain(&mem, &mut queues[tx], r, &segs);
                    racing(&mem, &mut queues[tx], || {
                        nic.io_write(QUEUE_NOTIFY, 2, tx as u32, &mem, time::boot_time_ns());
                    });
                } else {
                    {
                        let mut s = state.lock().unwrap();
                        while s.rx.len() < RACE_POSTS + 2 {
                            s.rx.push_back((vec![0x5A; 60], crate::offload::Rx::Done));
                        }
                    }
                    let segs = [(gpa(r), 1526, true)];
                    chain(&mem, &mut queues[rx], r, &segs);
                    racing(&mem, &mut queues[rx], || {
                        nic.poll(&mem, time::boot_time_ns());
                    });
                }
            }
        }
    }
    mem.held_to_map_budget(since, count);
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
    use crate::linux::{build, plan_below, set_entry, Firmware, Header};
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
    /* Whole pages, as guest memory is made of; `mem=` is in MiB -- below
     * the devices' pages, and past them up to 32 GiB. */
    let mem_bytes = match r.u8() % 8 {
        0 => r.value64() & !0xFFF,
        1 => 1 << 40,
        2 => (4064 + r.below(32768 - 4064 + 1)) << 20,
        _ => (64 + r.below(4096 - 64 + 1)) << 20,
    };
    let kernel_len = if r.u8() < 224 { r.below(32 << 20) } else { r.value64() };
    let initrd_len = match r.u8() % 8 {
        0 => 0,
        1 => r.value64(),
        _ => r.below(64 << 20),
    };
    /* A guest's RAM below 4 GiB to the devices' pages, or -- one holding a
     * host's function -- to the hole its BARs go in */
    let low_end = if r.u8() < 64 { crate::linux::HOLE_RAM_END } else { crate::linux::LOW_RAM_END };
    let Ok(l) = plan_below(&h, mem_bytes, low_end, kernel_len, initrd_len) else { return };
    invariant!(l.low_end == low_end, "a layout of RAM below {:#x}, planned for {:#x}", l.low_end, low_end);
    /* What the loader streams the kernel and the initrd by, and writes its
     * own tables around: inside the RAM below 4 GiB -- under the devices'
     * pages, whatever is past them -- apart, and clear of the tables. */
    let low = crate::linux::Ram::of_below(mem_bytes, low_end).low;
    let kernel_end = l.kernel_addr.checked_add(l.kernel_len);
    invariant!(kernel_end.is_some_and(|e| e <= low), "a kernel at {:#x} of {:#x} bytes in {:#x} of RAM below 4 GiB",
               l.kernel_addr, l.kernel_len, low);
    if l.initrd_len != 0 {
        invariant!(l.initrd_addr.checked_add(l.initrd_len).is_some_and(|e| e <= low),
                   "an initrd at {:#x} of {:#x} bytes in {:#x} of RAM below 4 GiB", l.initrd_addr, l.initrd_len, low);
        invariant!(kernel_end.is_some_and(|e| l.initrd_addr >= e), "the initrd at {:#x} inside the kernel, to {:#x}",
                   l.initrd_addr, kernel_end.unwrap_or(0));
    }
    invariant!(l.kernel_addr >= 1 << 20,
               "the kernel at {:#x}, over the tables the loader writes below 1 MiB", l.kernel_addr);
    if mem_bytes > crate::run::MAX_MEM_BYTES {
        return;
    }
    let Some(memory) = GuestMemory::with_ram_below(mem_bytes, low_end) else { return };
    let cmdline: Vec<u8> = (0..r.below(512)).map(|_| r.u8()).collect();
    let firmware = r.bool().then_some(Firmware { rsdp: crate::acpi::AREA, start: crate::acpi::AREA,
                                                 end: crate::acpi::AREA_END });
    let routes = [crate::acpi::Route { slot: 1, irq: 11 }, crate::acpi::Route { slot: 2, irq: 10 }];
    let cpus = 1 + r.below(16) as u32;
    if build(&memory, &h, &first, &l, &cmdline, cpus, firmware, &routes[..r.below(3) as usize], r.bool()).is_ok() {
        /* The zero page's memory map: RAM the guest has, every byte of it. */
        let mut n = [0u8; 1];
        memory.read(0x7000 + 0x1e8, &mut n).expect("the e820 count");
        e820_held(&memory, 0x7000 + 0x2d0, 20, usize::from(n[0]), mem_bytes, low_end);
        zero_page_held(&memory, &first, &l, usize::from(n[0]), firmware.map(|f| f.rsdp));
    }
    let mut vcpu = crate::vm::Backend::default();
    set_entry(&mut vcpu, &l);
}

/// The zero page at 0x7000 held to the boot protocol -- read by its offsets
/// here, not through `bzimage`, which wrote it for hv and writes hvboot's:
/// the header as the image has it, from 0x1f1 to the end its byte at 0x201
/// says, but for what a loader owns and the memory map's `entries` laid
/// over it; the loader's type undefined; loaded high; the command line at
/// the loader's 0x20000; no heap, and the extended loader's bytes clear;
/// the initrd where the plan put it; and the RSDP the firmware's, or none.
fn zero_page_held(memory: &GuestMemory, first: &[u8], l: &crate::linux::Layout, entries: usize, rsdp: Option<u64>) {
    let mut page = [0u8; 4096];
    memory.read(0x7000, &mut page).expect("the zero page");
    let word = |at: usize| u32::from_le_bytes(page[at..at + 4].try_into().unwrap());
    let owned = [0x210..0x212, 0x218..0x220, 0x224..0x22C, 0x2D0..0x2D0 + entries * 20];
    let end = (0x202 + usize::from(first[0x201])).min(first.len());
    for at in (0x1F1..end).filter(|at| !owned.iter().any(|r| r.contains(at))) {
        invariant!(page[at] == first[at], "the zero page's byte {:#x} is {:#x}, the image's {:#x}", at, page[at],
                   first[at]);
    }
    invariant!(page[0x210] == 0xFF, "type_of_loader {:#x}, not an undefined loader's", page[0x210]);
    invariant!(page[0x211] == first[0x211] | 1, "loadflags {:#x}, of the image's {:#x}: not loaded high",
               page[0x211], first[0x211]);
    invariant!(word(0x228) == 0x2_0000, "cmd_line_ptr {:#x}, where the line is at 0x20000", word(0x228));
    invariant!(word(0x224) == 0, "heap_end_ptr and the extended loader's bytes {:#x}", word(0x224));
    invariant!(u64::from(word(0x218)) == l.initrd_addr && u64::from(word(0x21C)) == l.initrd_len,
               "the initrd at {:#x}, {:#x} bytes: planned at {:#x}, {:#x}", word(0x218), word(0x21C), l.initrd_addr,
               l.initrd_len);
    let at = u64::from_le_bytes(page[0x70..0x78].try_into().unwrap());
    invariant!(at == rsdp.unwrap_or(0), "acpi_rsdp_addr {:#x}, where the firmware's is {:?}", at, rsdp);
}

/// A memory map of `n` entries at `map`, `stride` bytes apart, held to the
/// guest it describes: entries apart and in order, each RAM or reserved,
/// RAM only where the guest has memory, all of its RAM there, and the last
/// of it where `linux::Ram` ends it -- below 4 GiB at `low_end`.
fn e820_held(memory: &GuestMemory, map: u64, stride: usize, n: usize, mem_bytes: u64, low_end: u64) {
    let ram = crate::linux::Ram::of_below(mem_bytes, low_end);
    let mut ecam = false;
    invariant!((2..=crate::linux::E820_ENTRIES).contains(&n), "a memory map of {} entries", n);
    let mut end = 0u64;
    let mut ram_top = 0u64;
    let mut high = false;
    for i in 0..n {
        let mut ent = [0u8; 20];
        memory.read(map + (i * stride) as u64, &mut ent).expect("a memory map entry");
        let addr = u64::from_le_bytes(ent[0..8].try_into().unwrap());
        let size = u64::from_le_bytes(ent[8..16].try_into().unwrap());
        let kind = u32::from_le_bytes(ent[16..20].try_into().unwrap());
        invariant!(addr >= end && size != 0 && matches!(kind, 1 | 2),
                   "memory map entry {}: {:#x}+{:#x}, type {}, after {:#x}", i, addr, size, kind, end);
        end = addr + size;
        ecam |= kind == 2 && addr == crate::linux::ECAM_BASE && size == crate::linux::ECAM_BYTES;
        if kind == 1 {
            invariant!(memory.backs(addr, size), "RAM at {:#x}+{:#x} in the map, where the guest has none", addr, size);
            ram_top = end;
            high |= addr == crate::linux::HIGH_RAM_START && size == ram.high;
        }
    }
    invariant!(Some(ram_top) == ram.end(), "RAM to {:#x} in a guest of {:#x}", ram_top, mem_bytes);
    /* PCI Express's configuration space reserved where the guest has the
     * BARs' hole, its MCFG's window -- and only there */
    invariant!(ecam == (low_end == crate::linux::HOLE_RAM_END), "ECAM {} in the map of a guest whose RAM below 4 GiB \
                                                                  ends at {:#x}", if ecam { "reserved" } else { "not" },
               low_end);
    invariant!(high == (ram.high != 0), "a guest of {:#x} whose map says {} of its {:#x} from 4 GiB", mem_bytes,
               if high { "all" } else { "nothing" }, ram.high);
}

/* ---- what a guest's image decides: a PVH ELF ---- */

/// The file the PVH loader reads: its bytes, a read past the end short, as
/// a file's is -- and every read short, when the input says, as a file's
/// may be.
struct ElfFile<'a> {
    bytes: &'a [u8],
    short: bool,
}

impl crate::pvh::ReadAt for ElfFile<'_> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> crate::Result<usize> {
        let Some(rest) = usize::try_from(offset).ok().and_then(|o| self.bytes.get(o..)) else { return Ok(0) };
        let n = rest.len().min(buf.len()).min(if self.short { 7 } else { usize::MAX });
        buf[..n].copy_from_slice(&rest[..n]);
        Ok(n)
    }
}

/* The ELF the target makes: a header of either class, its program headers
 * right after it, and a body the segments and the note are cut from. */
const PVH_PT_LOAD: u32 = 1;
const PVH_PT_NOTE: u32 = 4;

fn put_at(f: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(dst) = f.get_mut(at..at + bytes.len()) {
        dst.copy_from_slice(bytes);
    }
}

/// An image that parses more often than random bytes would: a well-formed
/// ELF -- loadable segments, mostly above 1 MiB and apart, and the `Xen`
/// note naming an entry, mostly in one of them -- with the input's values
/// where they decide the layout, and a few of its bytes anywhere.
fn elf_image(r: &mut Input) -> Vec<u8> {
    let wide = r.u8() < 200;
    let (ehdr, phdr) = if wide { (64usize, 56usize) } else { (52usize, 32usize) };
    let phnum = r.below(7) as usize;
    let body_at = ehdr + phnum * phdr;
    let body = 0x200 + r.below(0x2000) as usize;
    let mut f = vec![0u8; body_at + body];

    put_at(&mut f, 0, &[0x7F, b'E', b'L', b'F', if wide { 2 } else { 1 }, 1, 1]);
    let machine: u16 = if r.u8() < 240 { if wide { 62 } else { 3 } } else { r.u16() };
    put_at(&mut f, 18, &machine.to_le_bytes());
    let phoff = if r.u8() < 240 { ehdr as u64 } else { r.value64() };
    let phentsize = if r.u8() < 240 { phdr as u16 } else { r.u16() };
    let count = if r.u8() < 240 { phnum as u16 } else { r.u16() };
    if wide {
        put_at(&mut f, 32, &phoff.to_le_bytes());
        put_at(&mut f, 54, &phentsize.to_le_bytes());
        put_at(&mut f, 56, &count.to_le_bytes());
    } else {
        put_at(&mut f, 28, &(phoff as u32).to_le_bytes());
        put_at(&mut f, 42, &phentsize.to_le_bytes());
        put_at(&mut f, 44, &count.to_le_bytes());
    }

    /* The note, somewhere in the body: "Xen", type 18, a 4- or 8-byte
     * entry -- mostly the first segment's first byte. */
    let note_at = body_at + (r.below((body - 24) as u64) as usize & !3);
    let first_paddr = 0x10_0000 + (r.below(64) << 20);
    let entry = if r.u8() < 200 { first_paddr + r.below(0x100) } else { r.value64() };
    let desc8 = r.u8() < 64;
    let descsz: u32 = if desc8 { 8 } else { 4 };
    put_at(&mut f, note_at, &4u32.to_le_bytes());
    put_at(&mut f, note_at + 4, &descsz.to_le_bytes());
    put_at(&mut f, note_at + 8, &(if r.u8() < 240 { 18u32 } else { r.u32() }).to_le_bytes());
    put_at(&mut f, note_at + 12, if r.u8() < 240 { b"Xen\0" } else { b"Nos\0" });
    if desc8 {
        put_at(&mut f, note_at + 16, &entry.to_le_bytes());
    } else {
        put_at(&mut f, note_at + 16, &(entry as u32).to_le_bytes());
    }
    let note_len = 16 + descsz as u64;

    let mut paddr = first_paddr;
    let file_end = (body_at + body) as u64;
    for i in 0..phnum {
        let at = ehdr + i * phdr;
        let note = i == 0 && r.u8() < 230;
        let (kind, offset, filesz, memsz) = if note {
            (PVH_PT_NOTE, note_at as u64, note_len, note_len)
        } else {
            let offset = if r.u8() < 230 { (body_at as u64) + r.below(body as u64) } else { r.value64() };
            let filesz = if r.u8() < 230 { r.below(file_end - offset.min(file_end)) } else { r.value64() };
            let memsz = if r.u8() < 230 { filesz.saturating_add(r.below(1 << 20)) } else { r.value64() };
            (if r.u8() < 240 { PVH_PT_LOAD } else { r.u32() }, offset, filesz, memsz)
        };
        let this = if note { 0 } else if r.u8() < 220 { paddr } else { r.value64() };
        if !note {
            paddr = paddr.saturating_add(memsz.max(1)).saturating_add(r.below(0x1000) * 0x1000);
        }
        put_at(&mut f, at, &kind.to_le_bytes());
        if wide {
            put_at(&mut f, at + 8, &offset.to_le_bytes());
            put_at(&mut f, at + 24, &this.to_le_bytes());
            put_at(&mut f, at + 32, &filesz.to_le_bytes());
            put_at(&mut f, at + 40, &memsz.to_le_bytes());
        } else {
            put_at(&mut f, at + 4, &(offset as u32).to_le_bytes());
            put_at(&mut f, at + 12, &(this as u32).to_le_bytes());
            put_at(&mut f, at + 16, &(filesz as u32).to_le_bytes());
            put_at(&mut f, at + 20, &(memsz as u32).to_le_bytes());
        }
    }
    for _ in 0..r.below(4) {
        let at = r.below(f.len() as u64) as usize;
        f[at] = r.u8();
    }
    f
}

/// How deep the `pvh` target's inputs got -- parsed, planned, built -- for
/// `HV_FUZZ_STATS`.
static PVH_PARSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PVH_PLANNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PVH_BUILT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A disk's requests given back before one handed over earlier.
static DISK_OUT_OF_ORDER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Discards and writes of zeros handed to the backend.
static RANGES_SERVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// A disk's passes ended at their bound on the data they moved.
static PASSES_CUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The 512e scenario's batches run, and the sectors of them held to the one
/// write alone over them (`blk_512e`).
static E_BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static E_ALONE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn report() {
    use std::sync::atomic::Ordering::Relaxed;
    println!("  pvh: {} images parsed, {} planned, {} built", PVH_PARSED.load(Relaxed), PVH_PLANNED.load(Relaxed),
             PVH_BUILT.load(Relaxed));
    println!("  disks: {} requests given back before one handed over earlier, {} discards and writes of zeros \
              handed over; {} chains made with buffers in an indirect table; {} passes cut short by the data they \
              moved", DISK_OUT_OF_ORDER.load(Relaxed), RANGES_SERVED.load(Relaxed), INDIRECT_MADE.load(Relaxed),
             PASSES_CUT.load(Relaxed));
    println!("  512e: {} batches on blocks larger than a sector, {} sectors held to the one write over them",
             E_BATCHES.load(Relaxed), E_ALONE.load(Relaxed));
}

fn pvh(r: &mut Input) {
    use crate::pvh::{build, plan_below, set_entry, Image, MAX_SEGMENTS, START_INFO};
    use std::sync::atomic::Ordering::Relaxed;
    let f = elf_image(r);
    let file_len = if r.u8() < 240 { f.len() as u64 } else { r.value64() };
    let mut file = ElfFile { bytes: &f, short: r.u8() < 32 };
    let Ok(image) = Image::parse(&mut file, file_len) else { return };
    PVH_PARSED.fetch_add(1, Relaxed);

    /* What the module streams by: each segment's bytes in the file, the
     * segments apart, and the entry an instruction the file holds. */
    let segs = image.segments();
    invariant!(!segs.is_empty() && segs.len() <= MAX_SEGMENTS, "{} segments", segs.len());
    for (i, s) in segs.iter().enumerate() {
        invariant!(s.filesz <= s.memsz, "a segment of {:#x} file bytes in {:#x}", s.filesz, s.memsz);
        invariant!(s.offset.checked_add(s.filesz).is_some_and(|e| e <= file_len),
                   "a segment's bytes at {:#x}, {:#x} of them, in a file of {:#x}", s.offset, s.filesz, file_len);
        invariant!(s.paddr.checked_add(s.memsz).is_some(), "a segment at {:#x} of {:#x} bytes", s.paddr, s.memsz);
        for t in &segs[i + 1..] {
            invariant!(s.paddr + s.memsz <= t.paddr || t.paddr + t.memsz <= s.paddr,
                       "segments over each other: {:#x}+{:#x} and {:#x}+{:#x}", s.paddr, s.memsz, t.paddr, t.memsz);
        }
    }
    let e = u64::from(image.entry);
    invariant!(segs.iter().any(|s| e >= s.paddr && e - s.paddr < s.filesz),
               "the entry {:#x} in no segment's file bytes", e);

    let mem_bytes = match r.u8() % 8 {
        0 => r.value64() & !0xFFF,
        1 => 1 << 40,
        2 => (4064 + r.below(32768 - 4064 + 1)) << 20,
        _ => (16 + r.below(4096 - 16 + 1)) << 20,
    };
    let low_end = if r.u8() < 64 { crate::linux::HOLE_RAM_END } else { crate::linux::LOW_RAM_END };
    let low = crate::linux::Ram::of_below(mem_bytes, low_end).low;
    let initrd_len = match r.u8() % 4 {
        0 => r.value64(),
        1 => 0,
        _ => r.below(64 << 20),
    };
    let Ok(l) = plan_below(&image, mem_bytes, low_end, initrd_len) else { return };
    PVH_PLANNED.fetch_add(1, Relaxed);
    /* All of it in the RAM below 4 GiB, which a 32-bit entry with paging
     * off reaches: under the devices' pages, whatever is past them. */
    for s in segs {
        invariant!(s.paddr >= 1 << 20 && s.paddr + s.memsz <= low,
                   "a segment at {:#x} of {:#x} bytes, in {:#x} of RAM below 4 GiB, over the tables below 1 MiB",
                   s.paddr, s.memsz, low);
    }
    let image_end = segs.iter().map(|s| s.paddr + s.memsz).max().unwrap_or(0);
    if l.initrd_len != 0 {
        invariant!(l.initrd_addr % 0x1000 == 0 && l.initrd_addr >= image_end
                   && l.initrd_addr.checked_add(l.initrd_len).is_some_and(|e| e <= low),
                   "an initrd at {:#x} of {:#x} bytes, the image to {:#x}, in {:#x} of RAM below 4 GiB", l.initrd_addr,
                   l.initrd_len, image_end, low);
    }
    if mem_bytes > crate::run::MAX_MEM_BYTES {
        return;
    }

    let Some(memory) = GuestMemory::with_ram_below(mem_bytes, low_end) else { return };
    let cmdline: Vec<u8> = (0..r.below(2100)).map(|_| r.u8()).collect();
    let firmware = r.bool().then_some(crate::linux::Firmware { rsdp: crate::acpi::AREA, start: crate::acpi::AREA,
                                                               end: crate::acpi::AREA_END });
    let routes = [crate::acpi::Route { slot: 1, irq: 11 }, crate::acpi::Route { slot: 2, irq: 10 }];
    let cpus = 1 + r.below(16) as u32;
    if build(&memory, &l, &cmdline, cpus, firmware, &routes[..r.below(3) as usize], r.bool()).is_ok() {
        PVH_BUILT.fetch_add(1, Relaxed);
        /* What the image is told: the start info's magic and its memory
         * map -- RAM up to the guest's last byte, below the 640 KiB hole and
         * from 1 MiB, entries apart and in order -- and an initrd as the
         * one module there is. */
        let mut si = [0u8; 56];
        memory.read(START_INFO, &mut si).expect("the start info");
        let at32 = |o: usize| u32::from_le_bytes(si[o..o + 4].try_into().unwrap());
        let at64 = |o: usize| u64::from_le_bytes(si[o..o + 8].try_into().unwrap());
        invariant!(at32(0) == 0x336E_C578 && at32(4) == 1, "a start info of magic {:#x}, version {}", at32(0), at32(4));
        invariant!(at32(12) == u32::from(l.initrd_len != 0), "{} modules, for an initrd of {}", at32(12), l.initrd_len);
        let (map, n) = (at64(40), at32(48) as usize);
        e820_held(&memory, map, 24, n, mem_bytes, low_end);
    }
    let mut vcpu = crate::vm::Backend::default();
    set_entry(&mut vcpu, &image);
    invariant!(vcpu.save().rip == e && vcpu.regs().rbx == START_INFO && vcpu.save().cr0 & (1 << 31) == 0,
               "the PVH entry at {:#x}, EBX {:#x}, CR0 {:#x}", vcpu.save().rip, vcpu.regs().rbx, vcpu.save().cr0);
}

/* ---- the ACPI tables, for any machine the host could describe ---- */

fn acpi(r: &mut Input) {
    use crate::acpi::{build, Machine, Route, MAX_BYTES};
    use crate::devices::pci::HostBridge;
    invariant!(crate::run::BARS64_ROUNDING == 8 << 20, "the 64-bit BAR window is not Cloud Hypervisor's 8 MiB granularity");
    let host_bridge = if r.bool() { HostBridge::Q35 } else { HostBridge::CloudHv };
    let cloudhv = host_bridge == HostBridge::CloudHv;
    let routes: Vec<Route> = (0..r.below(10)).map(|_| Route { slot: r.u8(), irq: r.u8() }).collect();
    let m = Machine {
        cpus: if r.bool() { r.below(17) as u32 } else { r.u32() },
        apic: r.bool(),
        x2apic: cloudhv && r.bool(),
        ioapic: r.bool().then(|| r.u8()),
        pci: &routes,
        mmio: (!cloudhv && r.bool()).then(|| (r.u32(), r.u32())),
        bars32: if cloudhv {
            Some((crate::linux::HOLE_RAM_END as u32, (crate::linux::CLOUDHV_MMIO32_END - crate::linux::HOLE_RAM_END) as u32))
        } else {
            r.bool().then(|| (r.u32(), r.u32()))
        },
        bars64: (!cloudhv && r.bool()).then(|| (r.u64(), r.u64())),
        ecam: (!cloudhv && r.bool()).then(|| (r.u32() & !0xF_FFFF, r.u8())),
        host_bridge,
    };
    let mut buf = vec![0u8; if r.u8() < 224 { MAX_BYTES } else { r.below(MAX_BYTES as u64) as usize }];
    if let Some(n) = build(&m, &mut buf) {
        invariant!(n <= buf.len(), "tables of {} bytes in {}", n, buf.len());
        /* ECAM, where the machine has it: one MCFG, its checksum whole, its
         * one allocation the window, of segment 0 from bus 0 -- and none
         * where it has not */
        let tables = &buf[..n];
        if m.apic && (1..=255).contains(&m.cpus) {
            let processors = tables.windows(8).filter(|w| *w == b"ACPI0007").count();
            invariant!(processors == m.cpus as usize, "{} processor objects for {} CPUs", processors, m.cpus);
            let madt = tables.windows(4).position(|w| w == b"APIC").expect("MADT");
            let len = u32::from_le_bytes(tables[madt + 4..madt + 8].try_into().unwrap()) as usize;
            invariant!(madt + len <= n && len >= 50, "invalid MADT length {}", len);
            let cpu_bytes = if m.x2apic { 16 } else { 8 };
            let cpu = &tables[madt + 44..madt + 44 + cpu_bytes];
            let expected: &[u8] = if m.x2apic {
                &[9, 16, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]
            } else {
                &[0, 8, 0, 0, 1, 0, 0, 0]
            };
            invariant!(cpu == expected, "MADT's first processor entry is {:x?}, not {:x?}", cpu, expected);
            let nmi_bytes = if m.x2apic { 12 } else { 6 };
            let nmi = &tables[madt + len - nmi_bytes..madt + len];
            let expected: &[u8] = if m.x2apic {
                &[10, 12, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 1, 0, 0, 0]
            } else {
                &[4, 6, 0xFF, 0, 0, 1]
            };
            invariant!(nmi == expected, "MADT's NMI entry is {:x?}, not {:x?}", nmi, expected);
        }
        let at: Vec<usize> = (0..n.saturating_sub(4)).step_by(16).filter(|&a| &tables[a..a + 4] == b"MCFG").collect();
        invariant!(at.len() == usize::from(m.ecam.is_some()), "{} MCFGs for ECAM {:x?}", at.len(), m.ecam);
        if let (Some(&a), Some((base, last))) = (at.first(), m.ecam) {
            let len = u32::from_le_bytes(tables[a + 4..a + 8].try_into().unwrap()) as usize;
            invariant!(len == 60 && a + len <= n, "an MCFG of {} bytes", len);
            let sum = tables[a..a + len].iter().fold(0u8, |s, b| s.wrapping_add(*b));
            invariant!(sum == 0, "the MCFG's checksum leaves {:#x}", sum);
            let entry = &tables[a + 44..a + 60];
            let got = (u64::from_le_bytes(entry[0..8].try_into().unwrap()), u16::from_le_bytes([entry[8], entry[9]]),
                       entry[10], entry[11]);
            invariant!(got == (u64::from(base), 0, 0, last), "the MCFG's allocation {:x?}, not {:#x} buses 0-{}",
                       got, base, last);
        }
        let spcr: Vec<usize> = (0..n.saturating_sub(4)).step_by(16).filter(|&a| &tables[a..a + 4] == b"SPCR").collect();
        invariant!(spcr.len() == usize::from(cloudhv), "{} SPCRs for CloudHv {}", spcr.len(), cloudhv);
        if let Some(&a) = spcr.first() {
            let len = u32::from_le_bytes(tables[a + 4..a + 8].try_into().unwrap()) as usize;
            invariant!(len == 80 && tables[a + 8] == 2 && a + len <= n, "an SPCR of {} bytes, revision {}", len, tables[a + 8]);
            let sum = tables[a..a + len].iter().fold(0u8, |s, b| s.wrapping_add(*b));
            invariant!(sum == 0, "the SPCR's checksum leaves {:#x}", sum);
            invariant!(tables[a + 36] == 0 && tables[a + 40] == 1 && tables[a + 41] == 8 && tables[a + 43] == 1
                       && u64::from_le_bytes(tables[a + 44..a + 52].try_into().unwrap()) == 0x3F8,
                       "the SPCR's UART interface or address is wrong");
            invariant!(tables[a + 52] == 3 && tables[a + 53] == 4
                       && u32::from_le_bytes(tables[a + 54..a + 58].try_into().unwrap()) == 4,
                       "the SPCR's interrupt is not COM1's IRQ/GSI 4");
            invariant!(tables[a + 58..a + 63] == [7, 0, 1, 0, 2], "the SPCR is not 115200 8N1, VT-UTF8");
            invariant!(tables[a + 64..a + 68] == [0xFF, 0xFF, 0xFF, 0xFF], "the SPCR names a PCI UART");
        }
        /* The host bridge's bus numbers, its `_CRS`'s first descriptor: 0
         * to ECAM's last where there is ECAM, which has to cover them, to
         * 255 where there is none */
        let bus_range = (0..n.saturating_sub(16)).find(|&a| tables[a..a + 4] == [0x88, 0x0D, 0x00, 0x02]);
        if let Some(a) = bus_range {
            let range = (u16::from_le_bytes([tables[a + 8], tables[a + 9]]),
                         u16::from_le_bytes([tables[a + 10], tables[a + 11]]));
            let want = (0, m.ecam.map_or(0xFF, |(_, last)| u16::from(last)));
            invariant!(range == want, "the host bridge's buses {:?}, not {:?}", range, want);
        }
        let fadt = (0..n.saturating_sub(4)).step_by(16).find(|&a| &tables[a..a + 4] == b"FACP")
            .expect("every machine has an FADT");
        let flags = u32::from_le_bytes(tables[fadt + 112..fadt + 116].try_into().unwrap());
        const HW_REDUCED: u32 = 1 << 20;
        invariant!((flags & HW_REDUCED != 0) == cloudhv, "hardware-reduced flags {:#x} for CloudHv {}", flags, cloudhv);
        if cloudhv {
            invariant!((0..n.saturating_sub(4)).step_by(16).all(|a| &tables[a..a + 4] != b"FACS"),
                       "a hardware-reduced machine has a FACS");
            invariant!(tables[fadt + 36..fadt + 40] == [0; 4] && tables[fadt + 56..fadt + 60] == [0; 4]
                       && tables[fadt + 64..fadt + 68] == [0; 4] && tables[fadt + 88..fadt + 90] == [0; 2],
                       "a hardware-reduced FADT has a FACS or legacy PM1 blocks");
            invariant!(u16::from_le_bytes(tables[fadt + 248..fadt + 250].try_into().unwrap()) == 0x600
                       && u16::from_le_bytes(tables[fadt + 260..fadt + 262].try_into().unwrap()) == 0x601,
                       "the sleep registers are not CloudHv's");
            let io: Vec<(u16, u16, u16)> = (0..=n.saturating_sub(16)).filter_map(|a| {
                if tables[a] == 0x88 && tables[a + 3] == 1 {
                    Some((u16::from_le_bytes([tables[a + 8], tables[a + 9]]),
                          u16::from_le_bytes([tables[a + 10], tables[a + 11]]),
                          u16::from_le_bytes([tables[a + 14], tables[a + 15]])))
                } else {
                    None
                }
            }).collect();
            invariant!(io == [(0xC000, 0xFFFF, 0x4000)], "CloudHv's I/O aperture {:x?}", io);
            let aperture = n >= 26 && (0..=n - 26).any(|a| {
                tables[a] == 0x87 && tables[a + 3] == 0 &&
                    u32::from_le_bytes(tables[a + 10..a + 14].try_into().unwrap()) == 0xC000_0000 &&
                    u32::from_le_bytes(tables[a + 14..a + 18].try_into().unwrap()) == 0xFBFF_FFFF &&
                    u32::from_le_bytes(tables[a + 22..a + 26].try_into().unwrap()) == 0x3C00_0000
            });
            invariant!(aperture, "CloudHv's MMIO32 aperture is missing");
            let com1 = tables.windows(4).any(|w| w == b"COM1") && tables.windows(11).any(|w| {
                w == [0x47, 1, 0xF8, 3, 0xF8, 3, 0, 8, 0x22, 0x10, 0]
            });
            invariant!(com1, "CloudHv's PNP0501 COM1 resources are missing");
        }
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
    /* A host network's server, or a cluster network's, which gives the
     * network's MTU too (option 26). */
    let net = Network { server_ip: 0x0A00_6401, server_mac: [2, 0, 0, 0, 0x64, 1], mask: 0xFFFF_FF00,
                        dns: if r.bool() { 0x0808_0808 } else { 0 },
                        mtu: if r.bool() { 0 } else { r.pick(&[576u16, 1400, 1440]) } };
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
    /* The MTU in every answer that is no NAK, where the network has one;
     * nowhere else. Read here as RFC 2132 has the options, sharing nothing
     * with the server's writing of them. */
    let msg = d.payload;
    let mut kind = None;
    let mut mtu = None;
    let mut at = DHCP_OPTIONS;
    while at < msg.len() && msg[at] != 255 {
        if msg[at] == 0 {
            at += 1;
            continue;
        }
        let Some(&len) = msg.get(at + 1) else { break };
        let Some(data) = msg.get(at + 2..at + 2 + usize::from(len)) else { break };
        match msg[at] {
            53 => kind = data.first().copied(),
            26 => mtu = Some(data.to_vec()),
            _ => {}
        }
        at += 2 + usize::from(len);
    }
    const NAK: u8 = 6;
    let want = (net.mtu != 0 && kind != Some(NAK)).then(|| net.mtu.to_be_bytes().to_vec());
    invariant!(mtu == want, "an answer of type {:?} gives MTU {:?}, the network's being {}", kind, mtu, net.mtu);
}


/// Hyper-V's basic and time MSRs against a model of the TLFS, including
/// independent hypercall and reference TSC overlays.
fn hyperv_target(r: &mut Input) {
    use crate::hyperv::{Hyperv, Vp, MSR_APIC_FREQUENCY, MSR_GUEST_OS_ID, MSR_HYPERCALL, MSR_REFERENCE_TSC,
                        MSR_STIMER0_CONFIG, MSR_STIMER3_COUNT, MSR_TIME_REF_COUNT, MSR_TSC_FREQUENCY, MSR_VP_INDEX};
    use crate::lapic::Lapic;
    use crate::memory::OverlaySlot;
    const ENABLE: u64 = 1;
    const LOCKED: u64 = 2;
    const RESERVED: u64 = 0xFFC;
    const GPFN: u64 = !0xFFF;
    const EPOCH: u64 = 2_000_000_000;
    const TSC_HZ: u64 = 2_500_000_000;
    const REF_HZ: u128 = 10_000_000;
    const PERIODIC: u64 = 1 << 1;
    const AUTO_ENABLE: u64 = 1 << 3;
    const VECTOR: u64 = 0xFF << 4;
    const DIRECT: u64 = 1 << 12;

    let mem = GuestMemory::with_size(MEM);
    let hv = Hyperv::new(crate::Vendor::Svm, EPOCH, TSC_HZ).expect("Hyper-V");
    mem.prepare_overlay(OverlaySlot::Hypercall, hv.code()).expect("the hypercall page");
    mem.prepare_overlay(OverlaySlot::ReferenceTsc, hv.reference_page()).expect("the reference page");
    let time = |tsc: u64| {
        let ticks = u128::from(tsc - EPOCH) * REF_HZ / u128::from(TSC_HZ);
        ticks.min(u128::from(u64::MAX)) as u64
    };
    invariant!(hv.read(MSR_TIME_REF_COUNT, 0, EPOCH) == Some(0), "reference time not zero at creation");
    let page = hv.reference_page();
    let sequence = u32::from_le_bytes(page[0..4].try_into().unwrap());
    let scale = u64::from_le_bytes(page[8..16].try_into().unwrap());
    let offset = i64::from_le_bytes(page[16..24].try_into().unwrap());
    invariant!(sequence == 1, "reference TSC sequence is not usable");
    invariant!(scale == u64::try_from((REF_HZ << 64) / u128::from(TSC_HZ)).unwrap(), "wrong TSC scale");
    let epoch_offset = (u128::from(EPOCH) * u128::from(scale)) >> 64;
    invariant!(offset == -i64::try_from(epoch_offset).unwrap(), "wrong TSC epoch offset");

    let (mut guest_id, mut hypercall, mut reference_tsc) = (0u64, 0u64, 0u64);
    let mut tsc = EPOCH;
    let mut crash = [0u64; 5];
    let mut vp = Vp::default();
    let mut configs = [0u64; 4];
    let mut counts = [0u64; 4];
    let mut scontrol = 0u64;
    let mut pages = [0u64; 3];
    let mut sints = [1u64 << 16; 16];
    let mut vp_assist = 0u64;
    let call_model = |input: u64| -> Result<crate::hyperv::Call, u64> {
        use crate::hyperv::Call;
        if input & (0xF000_F000_0000_0000 | 0xF800_0000) != 0 {
            return Err(3);
        }
        let code = input & 0xFFFF;
        let fast = input & 1 << 16 != 0;
        let variable = input >> 17 & 0x3FF;
        let reps = (input >> 32 & 0xFFF) as u16;
        let start = (input >> 48 & 0xFFF) as u16;
        match (code, variable, reps, start) {
            (2, 0, 0, 0) => Ok(Call::FlushSpace { fast }),
            (3, 0, n, at) if n != 0 && at < n => Ok(Call::FlushList { fast, reps, start }),
            (0xB, 0, 0, 0) => Ok(Call::SendIpi { fast }),
            (2 | 3 | 0xB, ..) => Err(3),
            _ => Err(2),
        }
    };
    while let Some(op) = r.op(24) {
        match op {
            0 => {
                let msr = r.pick(&[MSR_GUEST_OS_ID, MSR_HYPERCALL, MSR_VP_INDEX, MSR_TIME_REF_COUNT, MSR_REFERENCE_TSC,
                                   MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY]);
                let vp = r.u32();
                let want = match msr {
                    MSR_GUEST_OS_ID => guest_id,
                    MSR_HYPERCALL => hypercall,
                    MSR_VP_INDEX => u64::from(vp),
                    MSR_TIME_REF_COUNT => time(tsc),
                    MSR_REFERENCE_TSC => reference_tsc,
                    MSR_TSC_FREQUENCY => TSC_HZ,
                    _ => 1_000_000_000,
                };
                invariant!(hv.read(msr, vp, tsc) == Some(want), "MSR {:#x} read {:?}, wanted {:#x}",
                           msr, hv.read(msr, vp, tsc), want);
            }
            1 => {
                let value = r.value64();
                let ok = hv.write(MSR_GUEST_OS_ID, value, &mem);
                invariant!(ok == Some(true), "Guest OS ID refused: {:?}", ok);
                if value == 0 {
                    hypercall &= !ENABLE;
                }
                guest_id = value;
            }
            2 => {
                let value = if r.bool() {
                    r.pick(&[0, 0x1000, 0x100000, 0xFEE0_0000, memory::MAX_GPA]) | r.pick(&[0, 1, 2, 3])
                } else {
                    r.value64()
                };
                let old = hypercall;
                let mut next = (value & !RESERVED) | (old & RESERVED);
                let locked_bad = old & LOCKED != 0 && (next & LOCKED == 0 || next & GPFN != old & GPFN);
                if old & LOCKED != 0 && !locked_bad {
                    next |= LOCKED;
                }
                if next & ENABLE != 0 && guest_id == 0 {
                    next &= !ENABLE;
                }
                let gpa = next & GPFN;
                let collision = next & ENABLE != 0 && reference_tsc & ENABLE != 0 && gpa == reference_tsc & GPFN;
                let map_bad = gpa == memory::XAPIC_PAGE
                    || gpa.checked_add(PAGE).is_none_or(|end| end > memory::MAX_GPA) || collision;
                let ok = hv.write(MSR_HYPERCALL, value, &mem);
                invariant!(ok == Some(!locked_bad && !map_bad), "hypercall MSR write {:#x} -> {:?}, old {:#x}", value, ok, old);
                if ok == Some(true) {
                    hypercall = next;
                }
            }
            3 => {
                let vp = r.u32();
                invariant!(hv.read(MSR_VP_INDEX, vp, tsc) == Some(u64::from(vp)), "VP_INDEX changed");
            }
            4 => {
                invariant!(hv.write(MSR_VP_INDEX, r.value64(), &mem) == Some(false), "VP_INDEX writable");
            }
            5 => {
                let input = r.value64();
                let want = (guest_id != 0 && hypercall & ENABLE != 0).then_some(call_model(input));
                invariant!(hv.call(input) == want, "hypercall control {:#x} decoded as {:?}, wanted {:?}", input, hv.call(input), want);
                if guest_id != 0 && hypercall & ENABLE != 0 {
                    for input in [2, 0xB | 1 << 16, 3 | 1 << 32, 2 | 1 << 60, 3, 0xDEAD] {
                        invariant!(hv.call(input) == Some(call_model(input)), "hypercall control {:#x} decoded incorrectly", input);
                    }
                }
            }
            6 => {
                let msr = r.u32() | 0x8000_0000;
                invariant!(hv.read(msr, 0, tsc).is_none() && hv.write(msr, r.value64(), &mem).is_none(),
                           "an unknown MSR was claimed");
            }
            7 => {
                let n = r.below(5) as usize;
                let value = r.value64();
                let msr = crate::hyperv::MSR_CRASH_P0 + n as u32;
                invariant!(hv.write(msr, value, &mem) == Some(true), "crash parameter refused");
                crash[n] = value;
                invariant!(hv.read(msr, 0, tsc) == Some(value), "crash parameter not read back");
            }
            8 => {
                let value = r.value64();
                invariant!(hv.write(crate::hyperv::MSR_CRASH_CTL, value, &mem) == Some(true), "crash control refused");
                let crashed = value & (1 << 63) != 0;
                invariant!(hv.crashed() == crashed.then_some(crash), "CrashNotify gave the wrong parameters");
                invariant!(hv.read(crate::hyperv::MSR_CRASH_CTL, 0, tsc) == Some(1 << 63),
                           "CrashNotify not reported as available");
            }
            9 => {
                tsc = tsc.saturating_add(r.time_step());
                let got = hv.read(MSR_TIME_REF_COUNT, 0, tsc);
                invariant!(got == Some(time(tsc)), "reference time {:?}, wanted {:#x} at TSC {:#x}", got, time(tsc), tsc);
                let scaled = (u128::from(tsc) * u128::from(scale)) >> 64;
                let from_page = i128::from(offset) + i128::try_from(scaled).unwrap();
                invariant!((from_page - i128::from(time(tsc))).unsigned_abs() <= 1,
                           "reference page time {:#x}, MSR {:#x}", from_page, time(tsc));
            }
            10 => {
                let msr = r.pick(&[MSR_TIME_REF_COUNT, MSR_TSC_FREQUENCY, MSR_APIC_FREQUENCY]);
                invariant!(hv.write(msr, r.value64(), &mem) == Some(false), "read-only time/frequency MSR {:#x} writable", msr);
            }
            11 => {
                let value = if r.bool() {
                    r.pick(&[0, 0x1000, 0x100000, 0xFEE0_0000, memory::MAX_GPA]) | r.pick(&[0, 1, 3, RESERVED])
                } else {
                    r.value64()
                };
                let old = reference_tsc;
                let next = (value & !RESERVED) | (old & RESERVED);
                let gpa = next & GPFN;
                let collision = next & ENABLE != 0 && hypercall & ENABLE != 0 && gpa == hypercall & GPFN;
                let map_bad = gpa == memory::XAPIC_PAGE
                    || gpa.checked_add(PAGE).is_none_or(|end| end > memory::MAX_GPA) || collision;
                let ok = hv.write(MSR_REFERENCE_TSC, value, &mem);
                invariant!(ok == Some(!map_bad), "reference TSC MSR write {:#x} -> {:?}, old {:#x}", value, ok, old);
                if ok == Some(true) {
                    reference_tsc = next;
                }
                invariant!(hv.read(MSR_REFERENCE_TSC, 0, tsc) == Some(reference_tsc), "reference TSC not read back");
            }
            12 => {
                let value = (hypercall & GPFN) | ENABLE;
                let next = (value & !RESERVED) | (reference_tsc & RESERVED);
                let collision = hypercall & ENABLE != 0;
                let ok = hv.write(MSR_REFERENCE_TSC, value, &mem);
                invariant!(ok == Some(!collision), "reference page overlaid the hypercall page: {:?}", ok);
                if ok == Some(true) {
                    reference_tsc = next;
                }
            }
            13 => {
                let msr = MSR_STIMER0_CONFIG + r.below(8) as u32;
                let n = ((msr - MSR_STIMER0_CONFIG) / 2) as usize;
                let want = if msr & 1 == 0 { configs[n] } else { counts[n] };
                invariant!(vp.read(msr) == Some(want), "stimer MSR {:#x} read {:?}, wanted {:#x}", msr, vp.read(msr), want);
                invariant!(vp.read(MSR_STIMER3_COUNT + 1).is_none(), "an MSR past the stimers was claimed");
            }
            14 => {
                let n = r.below(4) as usize;
                let vector = if r.bool() { r.below(16) } else { 16 + r.below(240) };
                let value = match r.below(4) {
                    0 => r.value64(),
                    1 => ENABLE | DIRECT | (vector << 4) | r.pick(&[0, PERIODIC, AUTO_ENABLE, PERIODIC | AUTO_ENABLE]),
                    2 => ENABLE | (vector << 4),
                    _ => 0,
                };
                let valid = value & ENABLE == 0 || value & DIRECT == 0 || (value & VECTOR) >> 4 >= 16;
                let ok = vp.write(MSR_STIMER0_CONFIG + 2 * n as u32, value, time(tsc), &mem);
                invariant!(ok == Some(valid), "stimer config {:#x} -> {:?}, valid {}", value, ok, valid);
                if ok == Some(true) {
                    let mut next = value;
                    if next & ENABLE != 0 && next & DIRECT == 0 && next & (0xF << 16) == 0 {
                        next &= !ENABLE;
                    }
                    configs[n] = next;
                }
            }
            15 => {
                let n = r.below(4) as usize;
                let value = r.value64();
                let mut config = configs[n];
                if value == 0 {
                    config &= !ENABLE;
                } else if config & AUTO_ENABLE != 0 {
                    config |= ENABLE;
                }
                if config & ENABLE != 0 && config & DIRECT == 0 && config & (0xF << 16) == 0 {
                    config &= !ENABLE;
                }
                let valid = config & ENABLE == 0 || config & DIRECT == 0 || (config & VECTOR) >> 4 >= 16;
                let ok = vp.write(MSR_STIMER0_CONFIG + 2 * n as u32 + 1, value, time(tsc), &mem);
                invariant!(ok == Some(valid), "stimer count {:#x} -> {:?}, valid {}", value, ok, valid);
                if ok == Some(true) {
                    configs[n] = config;
                    counts[n] = value;
                }
            }
            16 => {
                let now = time(tsc);
                let mut one = Vp::default();
                let mut apic = Lapic::new(0, true, false);
                let config = ENABLE | AUTO_ENABLE | DIRECT | (0x30 << 4);
                invariant!(one.write(MSR_STIMER0_CONFIG, config, now, &mem) == Some(true), "direct one-shot config refused");
                invariant!(one.write(MSR_STIMER0_CONFIG + 1, now + 10, now, &mem) == Some(true), "one-shot count refused");
                invariant!(one.read(MSR_STIMER0_CONFIG + 1) == Some(now + 10), "one-shot count changed");
                invariant!(one.poll(now, &mut apic, &mem) == Some(now + 10) && !apic.requested(0x30),
                           "one-shot expired before its count");
                invariant!(one.poll(now + 11, &mut apic, &mem).is_none() && apic.requested(0x30),
                           "one-shot did not request its APIC vector");
                invariant!(one.read(MSR_STIMER0_CONFIG).is_some_and(|c| c & ENABLE == 0),
                           "one-shot stayed enabled after firing");
                invariant!(one.write(MSR_STIMER0_CONFIG + 1, 0, time(tsc), &mem) == Some(true)
                           && one.read(MSR_STIMER0_CONFIG).is_some_and(|c| c & ENABLE == 0),
                           "a zero count enabled a timer");

                let now = time(tsc);
                let mut periodic = Vp::default();
                let mut apic = Lapic::new(0, true, false);
                let config = ENABLE | PERIODIC | DIRECT | (0x31 << 4);
                invariant!(periodic.write(MSR_STIMER0_CONFIG, config, now, &mem) == Some(true), "periodic config refused");
                invariant!(periodic.write(MSR_STIMER0_CONFIG + 1, 100, now, &mem) == Some(true), "periodic count refused");
                invariant!(periodic.poll(now, &mut apic, &mem) == Some(now + 100), "wrong periodic first edge");
                let saved = periodic.save(now);
                let mut restored = Vp::default();
                invariant!(restored.restore(&saved, now + 500, &mem).is_ok(), "a live periodic stimer was not restored");
                let mut restored_apic = Lapic::new(0, true, false);
                invariant!(restored.poll(now + 599, &mut restored_apic, &mem) == Some(now + 600)
                           && !restored_apic.requested(0x31), "a restored periodic stimer fired early");
                invariant!(restored.poll(now + 601, &mut restored_apic, &mem) == Some(now + 700)
                           && restored_apic.requested(0x31), "a restored periodic stimer lost its phase");
                let mut invalid = saved.clone();
                invalid.scontrol = 2;
                invariant!(restored.restore(&invalid, now, &mem).is_err(), "an invalid saved SCONTROL was accepted");
                let mut zero = Vp::default();
                invariant!(zero.write(MSR_STIMER0_CONFIG, ENABLE | DIRECT | (0x30 << 4), now, &mem) == Some(true),
                           "an enabled zero-count stimer config refused");
                let zero_saved = zero.save(now);
                let mut zero_restored = Vp::default();
                invariant!(zero_restored.restore(&zero_saved, now, &mem).is_ok(),
                           "an enabled zero-count stimer config was not restored");
                invariant!(periodic.poll(now + 101, &mut apic, &mem) == Some(now + 200) && apic.requested(0x31),
                           "periodic edge or phase wrong");
                invariant!(periodic.poll(now + 101, &mut apic, &mem).is_none(), "periodic interrupt piled up");

                let mut message = Vp::default();
                let message_mem = GuestMemory::with_size(MEM);
                let mut apic = Lapic::new(0, true, false);
                let config = ENABLE | AUTO_ENABLE | (3 << 16);
                invariant!(message.write(crate::hyperv::MSR_SIMP, 0x1001, now, &message_mem) == Some(true),
                           "SIMP enable refused");
                invariant!(message.write(crate::hyperv::MSR_SINT0 + 3, 0x32, now, &message_mem) == Some(true),
                           "SINT3 config refused");
                invariant!(message.write(MSR_STIMER0_CONFIG, config, now, &message_mem) == Some(true)
                           && message.write(MSR_STIMER0_CONFIG + 1, now + 10, now, &message_mem) == Some(true),
                           "message-mode one-shot refused");
                invariant!(message.poll(now, &mut apic, &message_mem) == Some(now + 10) && !apic.requested(0x32),
                           "message-mode timer fired early");
                invariant!(message.poll(now + 11, &mut apic, &message_mem).is_none() && apic.requested(0x32),
                           "timer message did not request SINT3's vector");
                let at = 0x1000 + 3 * 256;
                let mut bytes = [0u8; 40];
                message_mem.read(at, &mut bytes).unwrap();
                invariant!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()) == 0x8000_0010 && bytes[4] == 24,
                           "wrong timer message header");
                invariant!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()) == 0
                           && u64::from_le_bytes(bytes[24..32].try_into().unwrap()) == now + 10
                           && u64::from_le_bytes(bytes[32..40].try_into().unwrap()) == now + 11,
                           "wrong timer message payload");

                let shared = hv.save();
                let mut restored_hv = Hyperv::new(crate::Vendor::Svm, EPOCH + 1000, TSC_HZ).expect("restored Hyper-V");
                invariant!(restored_hv.restore(&shared, TSC_HZ, &mem).is_ok(), "Hyper-V's shared state was not restored");
                invariant!(restored_hv.reference_time(tsc) == hv.reference_time(tsc)
                           && restored_hv.reference_page() == hv.reference_page(),
                           "Hyper-V's reference clock changed across restore");
                for msr in [MSR_GUEST_OS_ID, MSR_HYPERCALL, MSR_REFERENCE_TSC,
                            crate::hyperv::MSR_CRASH_P0, crate::hyperv::MSR_CRASH_CTL] {
                    invariant!(restored_hv.read(msr, 0, tsc) == hv.read(msr, 0, tsc),
                               "Hyper-V MSR {:#x} changed across restore", msr);
                }
            }
            17 => {
                use crate::hyperv::{MSR_EOM, MSR_SCONTROL, MSR_SIEFP, MSR_SIMP, MSR_SINT0, MSR_SINT15, MSR_SIRBP,
                                    MSR_SVERSION};
                let msr = if r.bool() {
                    r.pick(&[MSR_SCONTROL, MSR_SVERSION, MSR_SIEFP, MSR_SIMP, MSR_EOM, MSR_SIRBP])
                } else {
                    MSR_SINT0 + r.below(16) as u32
                };
                let want = match msr {
                    MSR_SCONTROL => scontrol,
                    MSR_SVERSION => 1,
                    MSR_SIEFP => pages[0],
                    MSR_SIMP => pages[1],
                    MSR_SIRBP => pages[2],
                    MSR_EOM => 0,
                    MSR_SINT0..=MSR_SINT15 => sints[(msr - MSR_SINT0) as usize],
                    _ => unreachable!(),
                };
                invariant!(vp.read(msr) == Some(want), "SynIC MSR {:#x} read {:?}, wanted {:#x}", msr, vp.read(msr), want);
            }
            18 => {
                let value = r.value64();
                let ok = vp.write(crate::hyperv::MSR_SCONTROL, value, time(tsc), &mem);
                invariant!(ok == Some(value & !1 == 0), "SCONTROL reserved bits accepted");
                if ok == Some(true) {
                    scontrol = value;
                }
            }
            19 => {
                invariant!(vp.write(crate::hyperv::MSR_SVERSION, r.value64(), time(tsc), &mem) == Some(false),
                           "SVERSION writable");
                invariant!(vp.read(crate::hyperv::MSR_SVERSION) == Some(1), "wrong SynIC version");
                invariant!(vp.write(crate::hyperv::MSR_EOM, r.value64(), time(tsc), &mem) == Some(true)
                           && vp.read(crate::hyperv::MSR_EOM) == Some(0), "EOM not answered");
            }
            20 => {
                use crate::hyperv::{MSR_SIEFP, MSR_SIMP, MSR_SIRBP};
                let slot = r.below(3) as usize;
                let msr = [MSR_SIEFP, MSR_SIMP, MSR_SIRBP][slot];
                let gpa = r.pick(&[0, 0x1000, MEM - PAGE, memory::MAX_GPA, hypercall & GPFN, reference_tsc & GPFN]);
                let value = gpa | r.pick(&[0, 1, RESERVED]) | if r.bool() { 1 } else { 0 };
                let next = (value & !RESERVED) | (pages[slot] & RESERVED);
                let enabled = next & 1 != 0;
                let address = next & GPFN;
                let valid = !enabled || mem.is_ram(address, PAGE as _) && !mem.overlay_contains(address)
                    && !(vp_assist & 1 != 0 && vp_assist & GPFN == address);
                if valid && enabled {
                    mem.write(address, &[0xAA; 8]).unwrap();
                }
                let ok = vp.write(msr, value, time(tsc), &mem);
                invariant!(ok == Some(valid), "SynIC page {:#x} -> {:?}, valid {}", value, ok, valid);
                if ok == Some(true) {
                    pages[slot] = next;
                    if enabled {
                        let mut bytes = [1u8; 8];
                        mem.read(address, &mut bytes).unwrap();
                        invariant!(bytes == [0; 8], "enabled SynIC page not zeroed");
                    }
                }
            }
            21 => {
                let n = r.below(16) as usize;
                let value = if r.bool() {
                    r.value64()
                } else {
                    r.below(256) | r.pick(&[0, 1 << 16, 1 << 17, 1 << 18])
                };
                let valid = value & !(0xFF | 0x1F << 16) == 0 && (value & 1 << 16 != 0 || value & 0xFF >= 16);
                let msr = crate::hyperv::MSR_SINT0 + n as u32;
                let ok = vp.write(msr, value, time(tsc), &mem);
                invariant!(ok == Some(valid), "SINT {:#x} -> {:?}, valid {}", value, ok, valid);
                if ok == Some(true) {
                    sints[n] = value;
                }
            }
            22 => {
                let gpa = r.pick(&[0, 0x1000, MEM - PAGE, memory::MAX_GPA, hypercall & GPFN, reference_tsc & GPFN]);
                let value = gpa | r.pick(&[0, 1, RESERVED]) | if r.bool() { 1 } else { 0 };
                let next = (value & !RESERVED) | (vp_assist & RESERVED);
                let enabled = next & 1 != 0;
                let address = next & GPFN;
                let synic = pages.iter().any(|page| page & 1 != 0 && page & GPFN == address);
                let valid = !enabled || !synic && mem.is_ram(address, PAGE as _) && !mem.overlay_contains(address);
                if valid && enabled {
                    mem.write(address, &[0xAA; 4]).unwrap();
                }
                let ok = vp.write(crate::hyperv::MSR_VP_ASSIST, value, time(tsc), &mem);
                invariant!(ok == Some(valid), "VP assist {:#x} -> {:?}, valid {}", value, ok, valid);
                if ok == Some(true) {
                    vp_assist = next;
                    if enabled {
                        let mut bytes = [1u8; 4];
                        mem.read(address, &mut bytes).unwrap();
                        invariant!(bytes == [0; 4], "VP assist EOI word not cleared");
                    }
                }
                invariant!(vp.read(crate::hyperv::MSR_VP_ASSIST) == Some(vp_assist), "VP assist MSR not read back");
                invariant!(vp.assist_page() == (vp_assist & 1 != 0).then_some(vp_assist & GPFN),
                           "VP assist page address differs from its MSR");
                let state_mem = GuestMemory::with_size(MEM);
                let mut state = Vp::default();
                invariant!(state.write(crate::hyperv::MSR_VP_ASSIST, 0x2001, time(tsc), &state_mem) == Some(true),
                           "VP assist enable for a state check refused");
                let saved = state.save(time(tsc));
                let mut restored = Vp::default();
                invariant!(restored.restore(&saved, time(tsc), &state_mem).is_ok(), "VP assist state was not restored");
                invariant!(restored.read(crate::hyperv::MSR_VP_ASSIST) == Some(0x2001), "VP assist MSR lost on restore");
            }
            23 => {
                let auto = scontrol & 1 != 0 && sints.iter().any(|sint| sint & (1 << 16 | 1 << 17) == 1 << 17);
                invariant!(vp.auto_eoi() == auto, "SynIC AutoEOI presence not reported");
                let vector = r.u8();
                let expected = scontrol & 1 != 0 && sints.iter().any(|sint| {
                    sint & (0xFF | 1 << 16 | 1 << 17) == u64::from(vector) | 1 << 17
                });
                invariant!(vp.auto_eoi_vector(vector) == expected, "AutoEOI for vector {:#x} not reported", vector);
                let mut one = Vp::default();
                invariant!(one.write(crate::hyperv::MSR_SCONTROL, 1, time(tsc), &mem) == Some(true)
                           && one.write(crate::hyperv::MSR_SINT0 + 2, 0x40 | 1 << 17, time(tsc), &mem) == Some(true),
                           "AutoEOI SINT setup refused");
                invariant!(one.auto_eoi() && one.auto_eoi_vector(0x40) && !one.auto_eoi_vector(0x41),
                           "AutoEOI SINT not reported by vector");
                invariant!(one.write(crate::hyperv::MSR_SINT0 + 2, 0x40 | 1 << 16 | 1 << 17, time(tsc), &mem) == Some(true)
                           && !one.auto_eoi() && !one.auto_eoi_vector(0x40), "a masked AutoEOI SINT still reported");
            }
            _ => {}
        }
        let hypercall_on = hypercall & ENABLE != 0;
        let reference_on = reference_tsc & ENABLE != 0;
        let (hypercall_gpa, reference_gpa) = (hypercall & GPFN, reference_tsc & GPFN);
        invariant!(mem.overlay_contains(hypercall_gpa) == (hypercall_on || reference_on && hypercall_gpa == reference_gpa),
                   "hypercall overlay at {:#x} does not match its MSR", hypercall_gpa);
        invariant!(mem.overlay_contains(reference_gpa) == (reference_on || hypercall_on && hypercall_gpa == reference_gpa),
                   "reference overlay at {:#x} does not match its MSR", reference_gpa);
    }
}
