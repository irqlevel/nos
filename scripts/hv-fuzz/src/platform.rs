//! The whole machine: a Linux guest's platform as `hv boot` builds it --
//! its CPUs' APICs, the 8259, the PIT, the RTC, the serial port, ACPI's
//! fixed hardware, the IO-APIC, the PCI bus with virtio disks and NICs, the
//! MSI-X tables in guest memory -- loaded as a kernel would be, and run by
//! the real run loop (`LinuxGuest::run`) on each of its CPUs, each on a
//! thread of its own and one at a time, as the script passes the turn. Where
//! the loop would enter the guest, the CPU (`crate::vm`) asks the script
//! instead: each entry a guest's exit -- a port, an MSR, CPUID, a fault on a
//! device's page with the instruction that made it put where the guest's
//! paging finds it, HLT, a window opened, an event taken -- and in between,
//! what a guest does without exiting: its memory written, its flags changed.
//! Some steps are a driver's whole bring-up, several exits long: the 8259's
//! initialisation, the APIC turned on, an IO-APIC pin routed, MSI-X turned
//! on and its table filled, a virtio queue set up and given requests,
//! another CPU started by INIT and start-up IPIs. The script ends in a
//! shutdown, which ends the run.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::devices::blk;
use crate::linux::{self, Header};
use crate::machine::Machine;
use crate::memory::GuestMemory;
use crate::run::{Host, LinuxGuest};
use crate::smp::Doorbells;
use crate::svm::{Exit, Io, LongMode};
use crate::targets::{apic_value, blk_segments, chain, gpa_in, DiskState, Disk, DriverQueue, Input, NicState, Nic,
                     APIC_EOI, APIC_REGS};
use crate::vm::{self, Backend};
use crate::x86::svm::vmcb::{exit as code, npf};
use crate::{time, Vendor};

/* Where things are in the machine `run.rs` builds (its constants). */
const DISK_IO_BASE: u16 = 0xC000;
const NIC_IO_BASE: u16 = 0xC100;
const MSIX_PAGES: u64 = 0xFE00_0000;
const XAPIC_BASE: u64 = 0xFEE0_0000;
const IOAPIC_BASE: u64 = 0xFEC0_0000;
const COM1: u16 = 0x3F8;
/// Where the script's instructions go: RAM the loader's page tables map.
const CODE: u64 = 0x0100_0000;
const MIB: u64 = 1 << 20;
const RFLAGS_IF: u64 = 1 << 9;

/// The ports a guest's kernel uses, and some it probes.
const PORTS: [u16; 36] = [0x20, 0x21, 0xA0, 0xA1, 0x4D0, 0x4D1, 0x40, 0x41, 0x42, 0x43, 0x61, 0x70, 0x71, 0x3F8,
                          0x3F9, 0x3FA, 0x3FB, 0x3FC, 0x3FD, 0x3FE, 0x3FF, 0x600, 0x602, 0x604, 0x608, 0x60A, 0xCF8,
                          0xCFC, 0xCFD, 0xCFE, 0xCFF, 0x80, 0x64, 0x60, 0x2F8, 0x510];
/// MSRs a kernel reads and writes.
const MSRS: [u32; 29] = [0x10, 0x3A, 0x48, 0x49, 0x8B, 0x174, 0x175, 0x176, 0x179, 0x17A, 0x1A0, 0x277,
                         0x2FF, 0x200, 0x201, 0x250, 0x6E0, 0xC000_0080, 0xC000_0081, 0xC000_0082, 0xC000_0084,
                         0xC000_0100, 0xC000_0101, 0xC000_0102, 0xC000_0103, 0xC001_0015, 0xC001_1029,
                         0x4B56_4D00, 0x4000_0000];
const LEAVES: [u32; 20] = [0, 1, 2, 4, 6, 7, 0xA, 0xB, 0xD, 0xF, 0x10, 0x1F, 0x4000_0000, 0x4000_0001, 0x8000_0000,
                           0x8000_0001, 0x8000_0007, 0x8000_0008, 0x8000_001E, 0x8000_0021];
/// Instructions the emulator must refuse, which stop the guest: a load of
/// RSP, a string move, an ALU operation on memory, UD2.
const REFUSED: [&[u8]; 4] = [&[0x8B, 0x27], &[0xA5], &[0x01, 0x07], &[0x0F, 0x0B]];

/// What the host hands the run: the console's input and the power button,
/// which the script fills and presses.
struct HostState {
    input: Mutex<VecDeque<u8>>,
    button: AtomicBool,
    /// Rounds of the loops since the guest was last entered: a guest that
    /// sleeps for good is stopped by whoever runs it, here after a while.
    idle: std::sync::atomic::AtomicU32,
}

/// That many rounds with no entry, and the run is stopped.
const IDLE_ROUNDS: u32 = 1000;

struct FuzzHost(Arc<HostState>);

impl Host for FuzzHost {
    fn output(&self, _byte: u8) {}
    fn input(&self, _at_prompt: bool) -> Option<u8> {
        self.0.input.lock().unwrap().pop_front()
    }
    fn stop_requested(&self) -> bool {
        self.0.idle.fetch_add(1, Ordering::Relaxed) >= IDLE_ROUNDS
    }
    fn power_button(&self) -> bool {
        self.0.button.swap(false, Ordering::Relaxed)
    }
}

/// One step of a planned sequence: an exit with its registers, or memory
/// the guest writes on the way.
enum Step {
    Out { port: u16, size: u8, value: u32 },
    In { port: u16, size: u8 },
    Wrmsr { msr: u32, value: u64 },
    Mmio { gpa: u64, store: bool, value: u64 },
    Poke { gpa: u64, bytes: Vec<u8> },
}

/// What the guest's code on one CPU knows: what it has planned to do, and
/// its local APIC's mode.
struct CpuPlan {
    plan: VecDeque<Step>,
    /// xAPIC mode, as the guest believes it is in.
    xapic: bool,
    /// The mode a write of the base MSR just asked for: the guest's, once the
    /// write did not fault.
    asked_xapic: Option<bool>,
    /// In 32-bit protected mode, paging off, rather than long mode.
    protected32: bool,
}

/// The guest: a script, and what its drivers know of the machine.
struct Script {
    data: Vec<u8>,
    at: usize,
    host: Arc<HostState>,
    disks: Vec<Arc<Mutex<DiskState>>>,
    nics: Vec<Arc<Mutex<NicState>>>,
    /// Each device's queues, disks first: a disk's one, a NIC's two.
    queues: Vec<[DriverQueue; 2]>,
    /// Whether it has local APICs at all: a guest of one CPU has none.
    apic: bool,
    ioapic: bool,
    cpus: Vec<CpuPlan>,
    /// The CPU whose turn it is.
    me: usize,
    /// The page table the boot CPU was loaded with, which a CPU started
    /// later switches to on its way to long mode.
    cr3: u64,
    /// Where the next instruction goes.
    code: u64,
}

impl Script {
    fn plan(&mut self) -> &mut VecDeque<Step> {
        &mut self.cpus[self.me].plan
    }

    fn xapic(&self) -> bool {
        self.cpus[self.me].xapic
    }

    /// Put an MMIO instruction at the guest's RIP -- `store` or a load of
    /// `size` bytes, mostly as asked, to or from any register -- and the
    /// value a store writes in the register it names.
    fn mmio_insn(&mut self, v: &mut Backend, mem: &GuestMemory, r: &mut Input, store: bool, size: u8, value: u64) {
        let long = !self.cpus[self.me].protected32;
        let size = if long { size } else { size.min(4) };
        /* The form: a register's move, an immediate's, or a load that
         * extends -- zero or sign. */
        #[derive(PartialEq)]
        enum Form { Reg, Imm, Zx, Sx }
        let form = match (store, size, r.u8() % 4) {
            (true, 1..=4, 0) => Form::Imm,
            (false, 1 | 2, 1) => Form::Zx,
            (false, 1 | 2, 2) => Form::Sx,
            _ => Form::Reg,
        };
        /* A register: any of sixteen in long mode, eight in 32-bit code. A
         * byte's 4 to 7 with no REX are AH to BH; a load into RSP stops the
         * guest, so none is asked for but AH's. */
        let mut reg = r.below(if long { 16 } else { 8 }) as u8;
        let high_byte = size == 1 && form == Form::Reg && (4..8).contains(&reg);
        if !store && reg == 4 && !high_byte {
            reg = 0;
        }
        let mut insn = Vec::new();
        if size == 2 && form != Form::Zx && form != Form::Sx {
            insn.push(0x66);
        }
        let rex = if reg >= 8 { 0x44 } else { 0 } | if size == 8 { 0x48 } else { 0 };
        if rex != 0 {
            insn.push(rex);
        }
        let modrm = ((reg & 7) << 3) | 0x07;
        match form {
            Form::Imm => {
                insn.push(if size == 1 { 0xC6 } else { 0xC7 });
                insn.push(0x07);
                insn.extend_from_slice(&value.to_le_bytes()[..usize::from(size.min(4))]);
            }
            Form::Zx | Form::Sx => {
                let base = if form == Form::Zx { 0xB6 } else { 0xBE };
                insn.extend_from_slice(&[0x0F, base + u8::from(size == 2), modrm]);
            }
            Form::Reg => {
                let opcode = match (store, size) {
                    (true, 1) => 0x88,
                    (true, _) => 0x89,
                    (false, 1) => 0x8A,
                    (false, _) => 0x8B,
                };
                insn.extend_from_slice(&[opcode, modrm]);
            }
        }
        if r.rare() {
            /* What the guest has there: an instruction to refuse, or anything. */
            insn = if r.bool() { r.pick(&REFUSED).to_vec() } else { (0..1 + r.below(15)).map(|_| r.u8()).collect() };
        }
        self.code = CODE + (self.code + 16 - CODE) % (2 * MIB);
        let rip = if !r.rare() { self.code } else { r.value64() };
        let _ = mem.write(rip, &insn);
        let (sv, regs) = v.save_and_regs_mut();
        sv.rip = rip;
        /* The value a store takes, where it takes it from: AH to BH are bits
         * 15:8 of RAX to RBX. */
        let (num, value) = if high_byte { (reg - 4, value << 8) } else { (reg, value) };
        match num {
            0 => sv.rax = value,
            1 => regs.rcx = value,
            2 => regs.rdx = value,
            3 => regs.rbx = value,
            4 => {}
            5 => regs.rbp = value,
            6 => regs.rsi = value,
            7 => regs.rdi = value,
            8 => regs.r8 = value,
            9 => regs.r9 = value,
            10 => regs.r10 = value,
            11 => regs.r11 = value,
            12 => regs.r12 = value,
            13 => regs.r13 = value,
            14 => regs.r14 = value,
            _ => regs.r15 = value,
        }
    }

    /// The exit for a planned step, with what the guest did for it.
    fn step(&mut self, v: &mut Backend, mem: &GuestMemory, r: &mut Input, s: Step) -> Option<Exit> {
        match s {
            Step::Out { port, size, value } => {
                v.save_mut().rax = u64::from(value);
                Some(io_exit(v, port, size, false))
            }
            Step::In { port, size } => Some(io_exit(v, port, size, true)),
            Step::Wrmsr { msr, value } => {
                if msr == 0x1B {
                    self.cpus[self.me].asked_xapic = Some(value & (1 << 10) == 0);
                }
                v.regs_mut().rcx = u64::from(msr);
                v.regs_mut().rdx = value >> 32;
                v.save_mut().rax = value & 0xFFFF_FFFF;
                Some(Exit::Msr { write: true })
            }
            Step::Mmio { gpa, store, value } => {
                self.mmio_insn(v, mem, r, store, 4, value);
                Some(Exit::NestedFault { gpa, error: npf::FINAL | if store { npf::WRITE } else { 0 } })
            }
            Step::Poke { gpa, bytes } => {
                let _ = mem.write(gpa, &bytes);
                None
            }
        }
    }

    /// A register of the local APIC written, the way the guest's mode has it.
    fn apic_write(&mut self, reg: u32, value: u64) {
        let step = if self.xapic() {
            Step::Mmio { gpa: XAPIC_BASE + (u64::from(reg) << 4), store: true, value: value & 0xFFFF_FFFF }
        } else {
            Step::Wrmsr { msr: 0x800 + reg, value }
        };
        self.plan().push_back(step);
    }

    fn pci_write(&mut self, slot: u32, reg: u32, size: u8, value: u32) {
        self.plan().push_back(Step::Out { port: 0xCF8, size: 4, value: 0x8000_0000 | (slot << 11) | (reg & 0xFC) });
        self.plan().push_back(Step::Out { port: 0xCFC + (reg & 3) as u16, size, value });
    }

    /// MSI-X on for device `dev`, its table filled first: a message to an
    /// APIC, mostly a fixed interrupt, sometimes masked.
    fn plan_msix(&mut self, dev: u32, r: &mut Input) {
        let slot = dev + 1;
        let page = MSIX_PAGES + u64::from(slot) * 4096;
        for entry in 0..3u64 {
            let dest = u64::from(r.below(4) as u8);
            let mut e = [0u8; 16];
            e[..4].copy_from_slice(&((XAPIC_BASE | dest << 12 | r.below(2) << 2) as u32).to_le_bytes());
            let data = 0x20 + r.below(0xD0) as u32 | if r.u8() < 16 { (r.below(8) as u32) << 8 } else { 0 };
            e[8..12].copy_from_slice(&data.to_le_bytes());
            e[12] = u8::from(r.u8() < 32);
            self.plan().push_back(Step::Poke { gpa: page + 16 * entry, bytes: e.to_vec() });
        }
        let control = 0x8000 | if r.u8() < 32 { 0x4000 } else { 0 };
        self.pci_write(slot, 0x40, 4, control << 16 | 0x11);
    }

    /// Start CPU `to`, as a kernel brings its CPUs up: INIT asserted and
    /// deasserted, then two start-up IPIs -- by the interrupt command
    /// register, its destination above it in x2APIC mode and in its high
    /// half through the page.
    fn plan_start(&mut self, to: u32, r: &mut Input) {
        let vector = u64::from(r.pick(&[0x9Au8, 0x10, 0x9A, 0]));
        for low in [0x4500u64, 0x8500, 0x4600 | vector, 0x4600 | vector] {
            if self.xapic() {
                self.apic_write(0x31, u64::from(to) << 24);
                self.apic_write(0x30, low);
            } else {
                self.apic_write(0x30, (u64::from(to) << 32) | low);
            }
        }
    }

    /// A driver's bring-up, several exits long.
    fn plan_something(&mut self, r: &mut Input) {
        match r.u8() % 12 {
            11 if self.cpus.len() > 1 => {
                let to = 1 + r.below(self.cpus.len() as u64 - 1) as u32;
                self.plan_start(to, r);
            }
            0 => {
                /* The 8259s, as Linux initialises them: vectors 0x30 and 0x38,
                 * cascaded on IRQ 2, then masks. */
                for (port, value) in [(0x20, 0x11), (0x21, 0x30), (0x21, 0x04), (0x21, 0x01), (0xA0, 0x11),
                                      (0xA1, 0x38), (0xA1, 0x02), (0xA1, 0x01)] {
                    self.plan().push_back(Step::Out { port, size: 1, value });
                }
                self.plan().push_back(Step::Out { port: 0x21, size: 1, value: u32::from(r.u8()) });
                self.plan().push_back(Step::Out { port: 0xA1, size: 1, value: u32::from(r.u8()) });
            }
            1 => {
                /* The PIT's channel 0 at some rate. */
                let count = r.u16();
                self.plan().push_back(Step::Out { port: 0x43, size: 1, value: r.pick(&[0x34, 0x30, 0x36, 0x32]) });
                self.plan().push_back(Step::Out { port: 0x40, size: 1, value: u32::from(count & 0xFF) });
                self.plan().push_back(Step::Out { port: 0x40, size: 1, value: u32::from(count >> 8) });
            }
            2 => {
                /* The APIC on, its timer and LINT0/1, the task priority. */
                self.apic_write(0x0F, 0x1FF);
                for reg in [0x32u32, 0x35, 0x36, 0x3E, 0x38, 0x08] {
                    let value = apic_value(r, reg);
                    self.apic_write(reg, value);
                }
            }
                3 if self.apic => {
                /* The base MSR, to the mode the guest wants: x2APIC or not --
                 * from x2APIC back to xAPIC through disabled, as the
                 * architecture has it, a direct switch being a #GP. */
                let x2 = r.bool();
                const EN: u64 = 1 << 11;
                const EXTD: u64 = 1 << 10;
                const BSP: u64 = 1 << 8;
                if !x2 && !self.xapic() {
                    self.plan().push_back(Step::Wrmsr { msr: 0x1B, value: XAPIC_BASE | BSP });
                }
                let value = XAPIC_BASE | EN | if x2 { EXTD } else { 0 } | BSP;
                self.plan().push_back(Step::Wrmsr { msr: 0x1B, value });
                /* What comes after in the plan was made for the new mode. */
                self.cpus[self.me].xapic = !x2;
            }
            4 if self.ioapic => {
                /* An IO-APIC pin routed: its entry's high word, then its low. */
                /* The pins a device is on, mostly: the PIT's, the serial
                 * port's, the SCI's, the NICs' and the disks'. */
                let pin = if r.u8() < 200 { r.pick(&[2u32, 4, 9, 10, 11]) } else { r.below(24) as u32 };
                let low = u64::from(0x20 + r.below(0xD0) as u32) | (r.below(2) << 11) | (r.below(2) << 13)
                    | (r.below(2) << 15) | if r.u8() < 32 { 1 << 16 } else { 0 };
                for (select, value) in [(0x11 + 2 * pin, u64::from(r.u8()) << 24), (0x10 + 2 * pin, low)] {
                    self.plan().push_back(Step::Mmio { gpa: IOAPIC_BASE, store: true, value: u64::from(select) });
                    self.plan().push_back(Step::Mmio { gpa: IOAPIC_BASE + 0x10, store: true, value });
                }
            }
            5 => {
                let dev = r.below(self.queues.len().max(1) as u64) as u32;
                self.plan_msix(dev, r);
            }
            6 | 7 if !self.queues.is_empty() => {
                /* A virtio device's queue set up: I/O on, reset, the queue's
                 * page, the driver ready -- after MSI-X, as a driver that
                 * uses it turns it on first. */
                let dev = r.below(self.queues.len() as u64) as usize;
                if r.bool() {
                    self.plan_msix(dev as u32, r);
                }
                let base = self.io_base(dev);
                let queue = if self.is_nic(dev) { r.below(2) as usize } else { 0 };
                let pfn = 0x100 + r.below(0x700);
                self.pci_write(dev as u32 + 1, 0x04, 2, 0x7);
                self.plan().push_back(Step::Out { port: base + 0x12, size: 1, value: 0 });
                self.plan().push_back(Step::Out { port: base + 0x12, size: 1, value: 3 });
                self.plan().push_back(Step::Out { port: base + 0x04, size: 4, value: r.u32() });
                self.plan().push_back(Step::Out { port: base + 0x0E, size: 2, value: queue as u32 });
                self.plan().push_back(Step::Out { port: base + 0x08, size: 4, value: pfn as u32 });
                if r.u8() < 160 {
                    /* With MSI-X on: the configuration's vector, then the
                     * queue's. */
                    self.plan().push_back(Step::Out { port: base + 0x14, size: 2, value: 0 });
                    self.plan().push_back(Step::Out { port: base + 0x16, size: 2, value: 1 + queue as u32 });
                }
                self.plan().push_back(Step::Out { port: base + 0x12, size: 1, value: 7 });
                self.queues[dev][queue] = DriverQueue::at(pfn);
            }
            9 => {
                /* The serial port's interrupts on, OUT2 up, and a byte sent. */
                self.plan().push_back(Step::Out { port: COM1 + 3, size: 1, value: 0x03 });
                self.plan().push_back(Step::Out { port: COM1 + 1, size: 1, value: u32::from(r.u8() & 0x0F) });
                self.plan().push_back(Step::Out { port: COM1 + 4, size: 1, value: 0x0B });
                self.plan().push_back(Step::Out { port: COM1, size: 1, value: u32::from(r.u8()) });
                self.plan().push_back(Step::In { port: COM1 + 2, size: 1 });
                if r.bool() {
                    /* A shell's line editor asking where its cursor is, and
                     * reading the answer. */
                    for b in *b"\x1b[6n" {
                        self.plan().push_back(Step::Out { port: COM1, size: 1, value: u32::from(b) });
                    }
                    for _ in 0..8 {
                        self.plan().push_back(Step::In { port: COM1, size: 1 });
                        self.plan().push_back(Step::In { port: COM1 + 5, size: 1 });
                    }
                }
            }
            10 => {
                /* ACPI on: SCI_EN, the power button's event enabled -- and
                 * pressed. */
                self.plan().push_back(Step::Out { port: 0x604, size: 2, value: 1 });
                self.plan().push_back(Step::Out { port: 0x602, size: 2, value: 0x100 | u32::from(r.u8() & 1) });
                self.host.button.store(r.bool(), Ordering::Relaxed);
            }
            _ if !self.queues.is_empty() => {
                /* Requests: a disk's, a NIC's buffers or frames, and the
                 * notify that hands them over. */
                let dev = r.below(self.queues.len() as u64) as usize;
                let base = self.io_base(dev);
                let queue = if self.is_nic(dev) { r.below(2) as usize } else { 0 };
                let mem_view = PlannedMemory::default();
                for _ in 0..1 + r.below(6) {
                    let segs = if self.is_nic(dev) {
                        if queue == 0 {
                            vec![(gpa_in(r, 64 * MIB), 1526, true)]
                        } else {
                            let len = 60 + r.below(1500) as u32;
                            vec![(gpa_in(r, 64 * MIB), 10, false), (gpa_in(r, 64 * MIB), len, false)]
                        }
                    } else if r.u8() < 192 {
                        self.valid_request(&mem_view, r, dev)
                    } else {
                        blk_segments(&mem_view, r, 64 * MIB)
                    };
                    chain(&mem_view, &mut self.queues[dev][queue], r, &segs);
                }
                for (gpa, bytes) in mem_view.take() {
                    self.plan().push_back(Step::Poke { gpa, bytes });
                }
                self.plan().push_back(Step::Out { port: base + 0x10, size: 2, value: queue as u32 });
            }
            _ => {}
        }
    }

    /// A request a driver would make of disk `dev`: a read, a write, a
    /// flush or its ID, inside the disk, in pieces of sectors.
    fn valid_request(&self, mem: &dyn crate::targets::Memory, r: &mut Input, dev: usize) -> Vec<(u64, u32, bool)> {
        let sectors = self.disks[dev].lock().unwrap().size() / blk::SECTOR;
        let kind = r.pick(&[0u32, 0, 1, 1, 4, 8]);
        let pieces = 1 + r.below(4);
        let each = 1 + r.below(8);
        let sector = r.below(sectors.saturating_sub(pieces * each).max(1));
        let header = 0x80_0000 + (r.below(0x10_0000) & !0xF);
        let mut h = [0u8; 16];
        h[..4].copy_from_slice(&kind.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        mem.put(header, &h);
        let mut segs = vec![(header, 16, false)];
        let data = if kind == 4 { 0 } else if kind == 8 { 1 } else { pieces };
        for i in 0..data {
            let len = if kind == 8 { 20 } else { (each * blk::SECTOR) as u32 };
            segs.push((0x100_0000 + 0x10_0000 * (i + 1) + r.below(0x8_0000), len, kind != 1));
        }
        segs.push((0x90_0000 + r.below(0x1000), 1, true));
        segs
    }

    fn is_nic(&self, dev: usize) -> bool {
        dev >= self.disks.len()
    }

    fn io_base(&self, dev: usize) -> u16 {
        if self.is_nic(dev) {
            NIC_IO_BASE + 0x40 * (dev - self.disks.len()) as u16
        } else {
            DISK_IO_BASE + 0x40 * dev as u16
        }
    }
}

/// Guest memory a plan writes, kept until the plan's steps write it.
#[derive(Default)]
pub struct PlannedMemory(Mutex<Vec<(u64, Vec<u8>)>>);

impl PlannedMemory {
    fn take(&self) -> Vec<(u64, Vec<u8>)> {
        core::mem::take(&mut self.0.lock().unwrap())
    }
}

impl crate::targets::Memory for PlannedMemory {
    fn put(&self, gpa: u64, bytes: &[u8]) {
        self.0.lock().unwrap().push((gpa, bytes.to_vec()));
    }
}

fn io_exit(v: &mut Backend, port: u16, size: u8, input: bool) -> Exit {
    let rip = v.save().rip;
    Exit::Io(Io { port, size, input, string: false, rep: false, next_rip: rip.wrapping_add(2) })
}

impl vm::Guest for Script {
    fn next(&mut self, cpu: usize, v: &mut Backend, mem: &GuestMemory) -> Option<vm::Next> {
        self.host.idle.store(0, Ordering::Relaxed);
        self.me = cpu;
        if v.save().cr0 & 1 == 0 {
            /* Started by INIT and a start-up IPI, in real mode: its
             * trampoline takes it to long mode on the boot CPU's tables. */
            v.long_mode(&LongMode { entry: CODE, stack: 0x7_0000 + 0x1000 * cpu as u64, cr3: self.cr3,
                                    code_selector: 0x10, data_selector: 0x18, ..LongMode::default() });
        }
        let data = core::mem::take(&mut self.data);
        let mut r = Input::resume(&data, self.at);
        let next = self.run(v, mem, &mut r);
        self.at = r.position();
        self.data = data;
        next
    }
}

impl Script {
    fn run(&mut self, v: &mut Backend, mem: &GuestMemory, r: &mut Input) -> Option<vm::Next> {
        /* The last write of the base MSR: in force, unless it faulted. */
        if let Some(asked) = self.cpus[self.me].asked_xapic.take() {
            if v.event == Some(vm::Event::Exception(13)) {
                self.cpus[self.me].xapic = !asked;
            }
        }
        loop {
            if let Some(s) = self.plan().pop_front() {
                v.take_event();
                match self.step(v, mem, r, s) {
                    Some(exit) => return Some(vm::Next::Exit(exit)),
                    None => continue,
                }
            }
            let op = r.op(34)?;
            let exit = match op {
                0..=5 => {
                    /* A port: one of the platform's, a device's, or any. */
                    let port = match r.u8() % 8 {
                        0..=4 => r.pick(&PORTS),
                        5 => DISK_IO_BASE + r.below(0x140) as u16,
                        6 => COM1 + r.below(8) as u16,
                        _ => r.u16(),
                    };
                    let size = if r.u8() < 160 { 1 } else { r.pick(&[1u8, 2, 4]) };
                    let input = r.bool();
                    v.save_mut().rax = if r.bool() { u64::from(r.u8()) } else { r.value64() };
                    let rip = v.save().rip;
                    let (string, rep) = (r.u8() < 4, r.u8() < 4);
                    Exit::Io(Io { port, size, input, string, rep, next_rip: rip.wrapping_add(1 + r.below(3)) })
                }
                6 | 7 => {
                    /* An MSR: the APIC's, a kernel's, or any. */
                    let msr = match r.u8() % 4 {
                        0 | 1 if self.apic => 0x800 + r.pick(&APIC_REGS),
                        _ if r.rare() => r.u32(),
                        _ => r.pick(&MSRS),
                    };
                    let value = if (0x800..0x900).contains(&msr) { apic_value(r, msr - 0x800) } else { r.value64() };
                    v.regs_mut().rcx = u64::from(msr) | if r.u8() < 8 { r.value64() << 32 } else { 0 };
                    v.regs_mut().rdx = value >> 32;
                    v.save_mut().rax = value & 0xFFFF_FFFF;
                    Exit::Msr { write: r.bool() }
                }
                8 => {
                    v.save_mut().rax = u64::from(r.pick(&LEAVES)) | if r.u8() < 16 { u64::from(r.u32()) } else { 0 };
                    v.regs_mut().rcx = r.below(4);
                    Exit::Cpuid
                }
                9 if r.u8() >= 64 => continue,
                9 => {
                    /* `sti; hlt` mostly -- or `cli; hlt`, asleep for good --
                     * often with a timer armed to wake it. */
                    if r.bool() {
                        match r.u8() % 3 {
                            0 => {
                                self.plan().push_back(Step::Out { port: 0x43, size: 1, value: 0x34 });
                                self.plan().push_back(Step::Out { port: 0x40, size: 1, value: 0x9B });
                                self.plan().push_back(Step::Out { port: 0x40, size: 1, value: 0x2E });
                                self.plan().push_back(Step::Out { port: 0x21, size: 1, value: 0xFA });
                            }
                            _ if self.apic => {
                                self.apic_write(0x0F, 0x1FF);
                                self.apic_write(0x32, 0x20000 | 0xEC);
                                self.apic_write(0x3E, 0x3);
                                self.apic_write(0x38, 100_000);
                            }
                            _ => {}
                        }
                        continue;
                    }
                    let s = v.save_mut();
                    s.rflags = if !r.rare() { s.rflags | RFLAGS_IF } else { s.rflags & !RFLAGS_IF };
                    Exit::Hlt
                }
                10..=13 => {
                    /* A device's page: the local APIC's, the IO-APIC's, the
                     * window's, or anywhere. */
                    /* A write to a page with no device behind it stops the
                     * guest; so does one outside the window. */
                    let apic = XAPIC_BASE + (u64::from(r.pick(&APIC_REGS)) << 4) + if r.u8() < 16 { r.below(16) } else { 0 };
                    let ioapic = IOAPIC_BASE + r.pick(&[0x00, 0x10, 0x40, 0x10, 0x14, 0x20]);
                    let (gpa, store) = match r.u8() % 64 {
                        /* The pages the guest has, as it believes; now and
                         * then one it has not, which stops it. */
                        0..=35 if self.xapic() || r.rare() => (apic, r.bool()),
                        36..=59 if self.ioapic || r.rare() => (ioapic, r.bool()),
                        0..=59 if self.xapic() => (apic, r.bool()),
                        0..=59 if self.ioapic => (ioapic, r.bool()),
                        62 if r.rare() => (r.value64(), r.bool()),
                        _ => (0xC000_0000 + (r.below(0x4000_0000) & !3), r.rare()),
                    };
                    let size = if r.u8() < 200 { 4 } else { r.pick(&[1u8, 2, 8]) };
                    let value = if gpa & !0xFFF == XAPIC_BASE {
                        apic_value(r, ((gpa >> 4) & 0xFF) as u32)
                    } else {
                        r.value64()
                    };
                    self.mmio_insn(v, mem, r, store, size, value);
                    let mut error = npf::FINAL | if store { npf::WRITE } else { 0 };
                    if r.rare() {
                        error = r.pick(&[npf::PRESENT, npf::FETCH, npf::TABLE_WALK, npf::USER, 0]) | if r.bool() { error } else { 0 };
                    }
                    if r.rare() && v.event_queued() {
                        /* The fault came on the way in of an event: it is
                         * queued again, and was not taken. */
                        return Some(vm::Next::Exit(Exit::NestedFault { gpa, error }));
                    }
                    Exit::NestedFault { gpa, error }
                }
                14 => {
                    /* The guest took what it was given, and ends it: an EOI
                     * of its APIC and of the 8259s. */
                    if r.bool() {
                        self.apic_write(APIC_EOI, 0);
                    } else {
                        self.plan().push_back(Step::Out { port: 0x20, size: 1, value: 0x20 });
                        self.plan().push_back(Step::Out { port: 0xA0, size: 1, value: 0x20 });
                    }
                    continue;
                }
                15 | 16 => {
                    self.plan_something(r);
                    continue;
                }
                17 => {
                    /* Interrupts on or off, a shadow: what STI, CLI, MOV SS do. */
                    let s = v.save_mut();
                    s.rflags = if r.bool() { s.rflags | RFLAGS_IF } else { s.rflags & !RFLAGS_IF };
                    v.shadow = r.u8() < 64;
                    continue;
                }
                18 => {
                    /* Its interrupt window, opened. */
                    v.save_mut().rflags |= RFLAGS_IF;
                    v.shadow = false;
                    Exit::IrqWindow
                }
                19 => {
                    /* The IRET that ends its NMI handler. */
                    v.nmi_masked = false;
                    Exit::NmiWindow
                }
                20 => {
                    /* Memory the guest writes: an MSI-X table, a ring, anything. */
                    let at = if r.bool() { MSIX_PAGES + r.below(8 * 4096) } else { gpa_in(r, 64 * MIB) };
                    let bytes: Vec<u8> = (0..1 + r.below(32)).map(|_| r.u8()).collect();
                    let _ = mem.write(at, &bytes);
                    continue;
                }
                21 => {
                    for _ in 0..1 + r.below(8) {
                        self.host.input.lock().unwrap().push_back(r.u8());
                    }
                    continue;
                }
                22 => {
                    self.host.button.store(true, Ordering::Relaxed);
                    continue;
                }
                23 => {
                    /* The guest ran a while: mostly microseconds, now and then
                     * a host that did not run it for seconds. */
                    time::advance(r.pick(&[0, 1_000, 50_000, 1_000_000, 4_000_000, 10_000_000, 100_000_000,
                                           2_000_000_000]));
                    continue;
                }
                24 => {
                    /* The disks answer; the NICs receive. */
                    for d in &self.disks {
                        let mut s = d.lock().unwrap();
                        s.ready += 1 + r.below(8) as usize;
                        s.fail = r.u8() < 16;
                    }
                    for n in &self.nics {
                        let len = r.pick(&[60usize, 64, 590, 1514, 1515, 20]);
                        n.lock().unwrap().rx.push_back((0..len).map(|i| i as u8).collect());
                    }
                    Exit::Host
                }
                25 => {
                    /* Not entered: kicked on the way in, what was queued
                     * still queued. */
                    return Some(vm::Next::Exit(Exit::Kicked));
                }
                31 if r.u8() < 8 => {
                    /* 32-bit protected mode, paging off -- or back to long
                     * mode, as its entry left it. */
                    let c = &mut self.cpus[self.me];
                    c.protected32 = !c.protected32;
                    if c.protected32 {
                        use crate::x86::svm::vmcb::{attrib, Segment};
                        let flat = Segment { selector: 0x10, attrib: attrib::P | attrib::S | attrib::CODE
                                             | attrib::WRITE_OR_READ | attrib::DB | attrib::G, limit: 0xFFFF_FFFF,
                                             base: 0 };
                        let s = v.save_mut();
                        s.cs = flat;
                        s.efer = 1 << 12;
                        s.cr0 = 0x11;
                    } else {
                        v.long_mode(&LongMode { entry: CODE, stack: 0x7_0000 + 0x1000 * self.me as u64, cr3: self.cr3,
                                                code_selector: 0x10, data_selector: 0x18, ..LongMode::default() });
                    }
                    continue;
                }
                32 | 33 if self.cpus.len() > 1 => {
                    /* Another CPU runs a while, this one in its guest. */
                    return Some(vm::Next::Switch(r.below(self.cpus.len() as u64) as usize));
                }
                26 => Exit::Pause,
                27 => {
                    let write = r.bool();
                    let gpr = r.below(16) as u8;
                    v.regs_mut().rcx = r.value64();
                    v.regs_mut().r9 = r.value64();
                    Exit::Cr8 { write, gpr }
                }
                28 => Exit::Other(if r.rare() { code::INVD } else {
                    r.pick(&[code::MONITOR, code::MWAIT, code::RDTSCP, code::XSETBV, code::WBINVD, code::RDPRU])
                }),
                29 => Exit::Hypercall,
                30 => Exit::Host,
                _ if r.rare() => match r.u8() % 4 {
                    /* The ends, rare: a run that ends early reaches little. */
                    0 => Exit::Shutdown,
                    1 => Exit::Exception { vector: r.u8() % 32, error: None },
                    2 => Exit::Invalid,
                    _ => Exit::MachineCheck,
                },
                /* VT-x's CR0 exits: what a kernel writes, and now and then
                 * anything. */
                _ if r.u8() < 32 => Exit::Cr0Write { value: if r.rare() { r.value64() } else { v.save().cr0 ^ (1 << 16) } },
                _ => Exit::Host,
            };
            v.take_event();
            return Some(vm::Next::Exit(exit));
        }
    }
}

/// A bzImage's header, enough of one for the loader: a 64-bit kernel of the
/// boot protocol's 2.15, loaded at 16 MiB.
fn header() -> (Header, Vec<u8>) {
    let mut first = vec![0u8; 0x1000];
    first[0x1F1] = 27;
    first[0x1FE..0x200].copy_from_slice(&0xAA55u16.to_le_bytes());
    first[0x201] = 0x6A;
    first[0x202..0x206].copy_from_slice(b"HdrS");
    first[0x206..0x208].copy_from_slice(&0x20Fu16.to_le_bytes());
    first[0x211] = 1;
    first[0x22C..0x230].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    first[0x230..0x234].copy_from_slice(&0x20_0000u32.to_le_bytes());
    first[0x234] = 1;
    first[0x236..0x238].copy_from_slice(&0x7Fu16.to_le_bytes());
    first[0x238..0x23C].copy_from_slice(&2047u32.to_le_bytes());
    first[0x258..0x260].copy_from_slice(&0x100_0000u64.to_le_bytes());
    first[0x260..0x264].copy_from_slice(&(24u32 << 20).to_le_bytes());
    let h = Header::parse(&first).expect("the header parses");
    (h, first)
}

/// How runs ended, and their exits: `HV_FUZZ_STATS=1` prints them, for
/// telling whether the script reaches deep or ends its runs early.
static ENDS: Mutex<std::collections::BTreeMap<String, (u64, u64)>> = Mutex::new(std::collections::BTreeMap::new());

pub fn report() {
    for (kind, (n, exits)) in ENDS.lock().unwrap().iter() {
        println!("  platform: {:>8} runs ended {:<12} {:>6} exits each", n, kind, exits / n.max(&1));
    }
}

/// Run each of the guest's CPUs on a thread of its own, the first first and
/// then as the script passes the turn: their exits, all told. A panic on
/// one has the others leave their runs, and is the run's.
fn run_threads(guest: &LinuxGuest, vcpus: &mut [crate::run::GuestCpu], machine: &Machine, deadline: u64,
               host: &FuzzHost) -> u64 {
    vm::begin_threads(vcpus.len());
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = vcpus.iter_mut().enumerate().map(|(i, gc)| {
            scope.spawn(move || {
                vm::set_vcpu(i);
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    vm::wait_turn(i);
                    guest.run(gc, machine, deadline, host).exits
                }));
                if ran.is_err() {
                    vm::abort();
                }
                vm::finished(i);
                ran
            })
        }).collect();
        handles.into_iter().map(|h| h.join()).collect::<Vec<_>>()
    });
    vm::end_threads();
    let mut exits = 0;
    for ran in results {
        match ran {
            Ok(Ok(n)) => exits += n,
            Ok(Err(panic)) | Err(panic) => std::panic::resume_unwind(panic),
        }
    }
    exits
}

pub fn platform(r: &mut Input) {
    let cpus = 1 + r.below(4) as u32;
    let x2apic = r.bool();
    let ioapic = cpus > 1 && r.bool();
    let mem_bytes = r.pick(&[64 * MIB, 128 * MIB, 256 * MIB, 3072 * MIB, 4064 * MIB, 4096 * MIB]);
    let machine = Machine::new(if r.bool() { Vendor::Svm } else { Vendor::Vmx });
    let Some(doorbells) = Doorbells::new(cpus as usize) else { return };
    let made = LinuxGuest::new(&machine, mem_bytes, cpus, Arc::new(doorbells), x2apic, ioapic);
    if mem_bytes > crate::run::MAX_MEM_BYTES {
        invariant!(made.is_err(), "a guest of {} MiB made, its RAM over its devices' pages", mem_bytes / MIB);
        return;
    }
    let Ok((mut guest, mut vcpus)) = made else { return };

    let mut disks = Vec::new();
    for i in 0..r.below(3) {
        let size = blk::SECTOR * (1 + r.below(1 << 16));
        let state = Arc::new(Mutex::new(DiskState::new(size, r.u8() < 32)));
        if guest.add_disk(Box::new(Disk(state.clone())), &format!("fuzz{}", i)).is_ok() {
            disks.push(state);
        }
    }
    let mut nics = Vec::new();
    for i in 0..r.below(2) {
        let state = Arc::new(Mutex::new(NicState::default()));
        if guest.add_nic(Box::new(Nic(state.clone())), [2, 0, 0, 0, 0x64, i as u8]).is_ok() {
            nics.push(state);
        }
    }

    let (h, first) = header();
    let Ok(layout) = linux::plan(&h, mem_bytes, 8 * MIB, if r.bool() { 4 * MIB } else { 0 }) else { return };
    let cmdline: Vec<u8> = b"console=ttyS0 ".iter().copied().chain((0..r.below(64)).map(|_| b'a' + r.u8() % 26)).collect();
    let Some(bsp) = vcpus.first_mut() else { return };
    invariant!(guest.load(bsp, &h, &first, layout, &cmdline).is_ok(), "a plain kernel's load failed");

    let host = Arc::new(HostState { input: Mutex::new(VecDeque::new()), button: AtomicBool::new(false),
                                    idle: std::sync::atomic::AtomicU32::new(0) });
    let devices = disks.len() + nics.len();
    let n = cpus as usize;
    let first_xapic = cpus > 1 && (!x2apic || ioapic);
    let script = Script {
        data: r.rest().to_vec(),
        at: 0,
        host: host.clone(),
        disks,
        nics,
        queues: vec![[DriverQueue::default(); 2]; devices],
        apic: cpus > 1,
        ioapic,
        cpus: (0..n).map(|_| CpuPlan { plan: VecDeque::new(), xapic: first_xapic, asked_xapic: None,
                                       protected32: false }).collect(),
        me: 0,
        cr3: vcpus[0].backend_mut().save().cr3,
        code: CODE,
    };
    vm::set_guest(Some(Box::new(script)));
    let deadline = time::boot_time_ns() + 3600 * crate::consts::NS_PER_SEC;
    let fuzz_host = FuzzHost(host.clone());
    let exits = if n == 1 {
        vm::set_vcpu(0);
        guest.run(&mut vcpus[0], &machine, deadline, &fuzz_host).exits
    } else {
        run_threads(&guest, &mut vcpus, &machine, deadline, &fuzz_host)
    };
    vm::set_guest(None);
    let stopped = guest.take_stopped();
    invariant!(stopped.is_some(), "the run ended with no stop recorded");
    if let Some(s) = &stopped {
        let what = format!("{:?}", s.stop);
        let mut kind = what.split([' ', '(', '{']).next().unwrap_or("").to_string();
        match &s.stop {
            crate::run::Stop::MmioInsn { error, .. } => kind = format!("{}/{:?}", kind, error),
            crate::run::Stop::Mmio { gpa, .. } => kind = format!("{}/{:x}", kind, gpa >> 20),
            _ => {}
        }
        let mut ends = ENDS.lock().unwrap();
        let e = ends.entry(format!("{} cpus {}", kind, n)).or_insert((0, 0));
        e.0 += 1;
        e.1 += exits;
    }
    /* What the module reports of a guest, from what it was left in. */
    let _ = (guest.disk_stats(), guest.nic_stats(), guest.acpi_stats(), guest.msr_faults(), guest.irq_debug(),
             guest.cpu_states(), guest.ioapic_stats(), guest.uart_ier(), guest.absent_pages(), guest.cpus_started(),
             guest.hot_ports(r.below(20) as usize));
}
