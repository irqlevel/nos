//! The x86-64 half: the PCI IDE controllers, their channels and the disks on
//! them, driven through I/O ports.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicU16, AtomicUsize, Ordering};

use block::BlockDriver;
use kcore::consts::PAGE_SIZE;
use kcore::io::Port;
use kcore::klog;
use kcore::once::Once;
use kcore::pci::{self, PciDevice};
use kcore::sync::Mutex;
use kcore::time::Deadline;

use ataproto::cmd::{FLUSH_CACHE as CMD_FLUSH, FLUSH_CACHE_EXT as CMD_FLUSH_EXT, IDENTIFY as CMD_IDENTIFY,
    READ_SECTORS as CMD_READ, READ_SECTORS_EXT as CMD_READ_EXT, STANDBY_IMMEDIATE as CMD_STANDBY,
    WRITE_SECTORS as CMD_WRITE, WRITE_SECTORS_EXT as CMD_WRITE_EXT};
use ataproto::identify::{self, Flush, Identify, Rotation, WORDS};
use ataproto::MAX_UNFINISHED;

/* ---- PCI ---- */

const CLASS_STORAGE: u8 = 0x01;
const SUBCLASS_IDE: u8 = 0x01;
/// The programming interface's bits: a channel in native mode takes its
/// registers from its BARs, one in compatibility mode from the ISA
/// addresses (`LEGACY`).
const PROGIF_PRIMARY_NATIVE: u8 = 1 << 0;
const PROGIF_SECONDARY_NATIVE: u8 = 1 << 2;
const PCI_COMMAND: u16 = 0x04;
const PCI_COMMAND_IO: u16 = 1 << 0;
/// A BAR that decodes I/O space has bit 0 set; its address is the rest but
/// the low two bits.
const BAR_IO: u32 = 1;
const BAR_IO_ADDRESS: u32 = !0x3;

/// The compatibility-mode channels' command blocks and device control
/// registers, primary and secondary.
const LEGACY: [(u16, u16); 2] = [(0x1F0, 0x3F6), (0x170, 0x376)];
/// A native channel's control block is four ports, the register at 2.
const CONTROL_OFFSET: u32 = 2;
/// The highest port a command block may start at: it is eight ports.
const COMMAND_BLOCK_LAST: u32 = 0xFFFF - 7;

/* ---- the taskfile (ATA8-ACS; ATA/ATAPI-6 for the register block) ---- */

const REG_DATA: u16 = 0;
/// Read: the error register; written: the features register.
const REG_ERROR: u16 = 1;
const REG_FEATURES: u16 = 1;
const REG_COUNT: u16 = 2;
const REG_LBA0: u16 = 3;
const REG_LBA1: u16 = 4;
const REG_LBA2: u16 = 5;
const REG_DEVICE: u16 = 6;
/// Read: the status register, which also takes a pending interrupt back;
/// written: the command register.
const REG_STATUS: u16 = 7;
const REG_COMMAND: u16 = 7;

const ST_ERR: u8 = 1 << 0;
const ST_DRQ: u8 = 1 << 3;
const ST_DF: u8 = 1 << 5;
const ST_BSY: u8 = 1 << 7;
/// What a position with nothing at it reads as: a floating bus, all ones,
/// or the 0x7F an ICH gives for a SATA port with no link.
const ST_FLOATING: [u8; 2] = [0xFF, 0x7F];

/// The device control register: the device's interrupt off (nIEN) -- this
/// driver polls -- and the software reset of both devices on the channel.
const CTL_NIEN: u8 = 1 << 1;
const CTL_SRST: u8 = 1 << 2;

/// The device register: LBA addressing, with the two obsolete bits set as
/// every device expects them, and which of the two devices.
const DEV_BASE: u8 = 0xE0;
const DEV_SLAVE: u8 = 1 << 4;
/// A 28-bit address's top four bits go in the device register.
const DEV_LBA28_HIGH: u64 = 0x0F;

/// What LBA1 and LBA2 hold after a reset when the device is a packet
/// (ATAPI) one -- a CD drive, QEMU's `-cdrom` among them.
const SIG_ATAPI: (u8, u8) = (0x14, 0xEB);
/// The register test's pattern: a device keeps what is written to its count
/// and LBA registers, an empty position does not.
const TEST_COUNT: u8 = 0x55;
const TEST_LBA0: u8 = 0xAA;

/// The most sectors one command moves: the channel is held for no longer
/// than that takes, and it is also what a 28-bit command's count register
/// holds at most (written as 0).
const MAX_SECTORS_PER_COMMAND: u64 = 256;
const LBA28_LIMIT: u64 = 1 << 28;

/* ---- how long things may take ---- */

/// A device to take a selection, or show it is not busy.
const SELECT_MS: u64 = 5_000;
/// A command that moves data: an HDD that has to spin up first, or
/// retries a sector it cannot read, takes seconds, and ATA gives a reset 31.
const COMMAND_MS: u64 = 31_000;
/// A flush, which writes back what the cache holds: on an HDD, at random
/// places, seek after seek.
const FLUSH_MS: u64 = 60_000;
const RESET_MS: u64 = 31_000;
/// An HDD spinning down takes seconds.
const STANDBY_MS: u64 = 31_000;
/// SRST held this long (at least 5 us), and BSY not believed for this long
/// after (2 ms).
const SRST_HOLD_US: u64 = 10;
const AFTER_RESET_US: u64 = 2_000;

/* ---- what is taken ---- */

/// Controllers taken, each with two channels and four positions -- named
/// `hda` to `hdx` by position, so that a disk's name says where it is
/// connected whatever else is.
const MAX_CONTROLLERS: usize = 6;
const POSITIONS_PER_CONTROLLER: usize = 4;
const POSITIONS_PER_CHANNEL: usize = 2;
const MAX_DISKS: usize = MAX_CONTROLLERS * POSITIONS_PER_CONTROLLER;
/// Channels taken at all: the command blocks claimed, so that a
/// compatibility-mode channel two functions both point at is one channel,
/// not two locks over the same registers.
const MAX_CHANNELS: usize = MAX_CONTROLLERS * 2;

static CONTROLLERS: AtomicUsize = AtomicUsize::new(0);
static CLAIMED: [AtomicU16; MAX_CHANNELS] = [const { AtomicU16::new(0) }; MAX_CHANNELS];
static CLAIMED_COUNT: AtomicUsize = AtomicUsize::new(0);
static DISKS: [Once<&'static AtaDisk>; MAX_DISKS] = [const { Once::new() }; MAX_DISKS];
static DISK_COUNT: AtomicUsize = AtomicUsize::new(0);

/* ---- waiting ---- */

/// Between two polls: the CPU to whatever else can run here once the boot
/// has a scheduler to give it to (`block::interrupts_started`), a pause of
/// the spin before.
fn relax() {
    if block::interrupts_started() {
        kcore::task::yield_to_runnable();
    } else {
        core::hint::spin_loop();
    }
}

/// Why a command did not go through.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// It took longer than it may: the status register as it was last read.
    Timeout { status: u8 },
    /// The device said it failed: ERR or DF in its status, and its error
    /// register.
    Device { status: u8, error: u8 },
}

/* ---- a channel's registers ---- */

/// A channel's registers: its command block and its device control
/// register, which reads back as the alternate status. Reached through the
/// channel's lock alone (`Channel::regs`), so that a command -- device
/// selected, taskfile written, data moved, status read -- is one holder's
/// from its start to its end, whichever of the two devices it is for.
struct Regs {
    cmd: u16,
    ctl: u16,
}

impl Regs {
    /// `cmd` is a command block's first port, at most `COMMAND_BLOCK_LAST`
    /// (`native_ports`, `LEGACY`): every register's port is inside it.
    fn read(&self, reg: u16) -> u8 {
        Port::<u8>::new(self.cmd + reg).read()
    }

    fn write(&self, reg: u16, value: u8) {
        Port::<u8>::new(self.cmd + reg).write(value);
    }

    fn data_in(&self) -> u16 {
        Port::<u16>::new(self.cmd + REG_DATA).read()
    }

    fn data_out(&self, value: u16) {
        Port::<u16>::new(self.cmd + REG_DATA).write(value);
    }

    /// The status, without taking a pending interrupt back.
    fn alt_status(&self) -> u8 {
        Port::<u8>::new(self.ctl).read()
    }

    fn control(&self, value: u8) {
        Port::<u8>::new(self.ctl).write(value);
    }

    /// The 400 ns a device has after a selection or a command before its
    /// status means anything: four reads of the alternate status.
    fn settle(&self) {
        for _ in 0..4 {
            let _ = self.alt_status();
        }
    }

    fn delay_us(&self, us: u64) {
        let mut deadline = Deadline::after_us(us);
        while !deadline.passed() {
            let _ = self.alt_status();
        }
    }

    /// Until the selected device is not busy: its status then.
    fn wait_idle(&self, ms: u64) -> Result<u8, Fault> {
        let mut deadline = Deadline::after_ms(ms);
        loop {
            let status = self.alt_status();
            if status & ST_BSY == 0 {
                return Ok(status);
            }
            if deadline.passed() {
                return Err(Fault::Timeout { status });
            }
            relax();
        }
    }

    /// Until the device has a block of data for the host, or wants one --
    /// DRQ with BSY clear -- or says it failed.
    fn wait_data(&self, ms: u64) -> Result<(), Fault> {
        let mut deadline = Deadline::after_ms(ms);
        loop {
            let status = self.alt_status();
            if status & ST_BSY == 0 {
                if status & (ST_ERR | ST_DF) != 0 {
                    return Err(Fault::Device { status, error: self.read(REG_ERROR) });
                }
                if status & ST_DRQ != 0 {
                    return Ok(());
                }
            }
            if deadline.passed() {
                return Err(Fault::Timeout { status });
            }
            relax();
        }
    }

    /// A command's end: the device not busy, and its status -- read from
    /// the status register itself, which takes the interrupt it would have
    /// raised back -- with no error, no fault and nothing more to move.
    fn finish(&self, ms: u64) -> Result<(), Fault> {
        self.settle();
        self.wait_idle(ms)?;
        let status = self.read(REG_STATUS);
        if status & (ST_ERR | ST_DF | ST_DRQ) != 0 {
            return Err(Fault::Device { status, error: self.read(REG_ERROR) });
        }
        Ok(())
    }

    fn select(&self, slave: bool) -> Result<(), Fault> {
        self.write(REG_DEVICE, DEV_BASE | if slave { DEV_SLAVE } else { 0 });
        self.settle();
        let mut deadline = Deadline::after_ms(SELECT_MS);
        loop {
            let status = self.alt_status();
            if status & (ST_BSY | ST_DRQ) == 0 {
                return Ok(());
            }
            if deadline.passed() {
                return Err(Fault::Timeout { status });
            }
            relax();
        }
    }

    /// Whether something answers at a position: not a floating bus, and it
    /// keeps what is written to its registers.
    fn present(&self, slave: bool) -> bool {
        self.write(REG_DEVICE, DEV_BASE | if slave { DEV_SLAVE } else { 0 });
        self.settle();
        if ST_FLOATING.contains(&self.alt_status()) {
            return false;
        }
        self.write(REG_COUNT, TEST_COUNT);
        self.write(REG_LBA0, TEST_LBA0);
        self.read(REG_COUNT) == TEST_COUNT && self.read(REG_LBA0) == TEST_LBA0
    }

    /// Both devices on the channel reset -- SRST is the channel's -- and each
    /// of `waited` waited for: a position with nothing at it, or with a
    /// device given up on, would hold BSY up for the whole of a reset's time.
    /// Which position did not come back, and how, where one did not.
    fn reset(&self, waited: [bool; POSITIONS_PER_CHANNEL]) -> Result<(), (usize, Fault)> {
        self.control(CTL_NIEN | CTL_SRST);
        self.delay_us(SRST_HOLD_US);
        self.control(CTL_NIEN);
        self.delay_us(AFTER_RESET_US);
        for (position, (slave, &there)) in [false, true].iter().zip(waited.iter()).enumerate() {
            if there {
                self.write(REG_DEVICE, DEV_BASE | if *slave { DEV_SLAVE } else { 0 });
                self.settle();
                self.wait_idle(RESET_MS).map_err(|fault| (position, fault))?;
            }
        }
        Ok(())
    }

    /// IDENTIFY DEVICE of a position found present, after a reset: its words,
    /// or None for a packet device or nothing that answers it.
    fn identify(&self, slave: bool) -> Option<[u16; WORDS]> {
        self.select(slave).ok()?;
        if (self.read(REG_LBA1), self.read(REG_LBA2)) == SIG_ATAPI {
            return None;
        }
        self.write(REG_COMMAND, CMD_IDENTIFY);
        self.settle();
        if self.alt_status() == 0 {
            return None;
        }
        self.wait_data(SELECT_MS).ok()?;
        let mut words = [0u16; WORDS];
        for word in words.iter_mut() {
            *word = self.data_in();
        }
        self.finish(SELECT_MS).ok()?;
        Some(words)
    }
}

/* ---- a channel and the disks on it ---- */

/// Where a channel is, for what the log says of it.
#[derive(Clone, Copy)]
struct Where {
    bus: u16,
    slot: u16,
    func: u16,
    channel: u8,
}

/// What a channel's lock owns: its registers, and how each of its two
/// devices is doing -- a reset is the channel's, so whether one of them has
/// been given up on is what a reset for the other one asks.
struct ChannelState {
    regs: Regs,
    /// Per position: commands in a row that did not finish.
    unfinished: [u32; POSITIONS_PER_CHANNEL],
    /// Per position: given up on, every request refused.
    failed: [bool; POSITIONS_PER_CHANNEL],
}

struct Channel {
    state: Mutex<ChannelState>,
    at: Where,
    /// The positions that answered at bring-up: what a reset waits for --
    /// one with nothing at it would hold BSY up for the whole of a reset's
    /// time.
    present: [bool; POSITIONS_PER_CHANNEL],
}

impl Channel {
    /// The device at `position` failed for good: said once, and refused from
    /// here on -- by its disk, and by the channel's resets, which wait for it
    /// no more.
    fn give_up(&self, state: &mut ChannelState, position: usize, why: &str) {
        if state.failed[position] {
            return;
        }
        state.failed[position] = true;
        let at = self.at;
        klog!(0, "ata: {:02x}:{:02x}.{} channel {} {}: {}: the disk is failed, and every request of it refused from \
                  here on", at.bus, at.slot, at.func, at.channel, if position == 0 { "master" } else { "slave" }, why);
    }
}

struct AtaDisk {
    channel: &'static Channel,
    slave: bool,
    id: Identify,
    identity: Option<String>,
    name: [u8; 3],
}

impl AtaDisk {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("hd?")
    }

    /// Whether `count` sectors from `lba` are inside the disk, and on a disk
    /// with no 48-bit commands inside what 28 bits reach. The table has
    /// checked what it hands down already; this is the driver's own check
    /// on the address it is about to give the device.
    fn within(&self, lba: u64, count: u64) -> bool {
        let limit = if self.id.lba48 { self.id.sectors } else { self.id.sectors.min(LBA28_LIMIT) };
        count != 0 && lba.checked_add(count).is_some_and(|end| end <= limit)
    }

    /// The taskfile for `command` over `count` sectors from `lba`, and the
    /// command: the device selected first, the address and count after --
    /// an ICH keeps a set of shadow registers to each device and takes the
    /// writes into the one the device register names.
    fn issue(&self, regs: &Regs, command: u8, lba: u64, count: u64) -> Result<(), Fault> {
        regs.select(self.slave)?;
        let device = DEV_BASE | if self.slave { DEV_SLAVE } else { 0 };
        /* At most MAX_SECTORS_PER_COMMAND: a 16-bit count, never the 0 that
         * would mean 65536 */
        let count = count as u16;
        if self.id.lba48 {
            /* The high half first: each register is a two-deep FIFO */
            regs.write(REG_FEATURES, 0);
            regs.write(REG_COUNT, (count >> 8) as u8);
            regs.write(REG_LBA0, (lba >> 24) as u8);
            regs.write(REG_LBA1, (lba >> 32) as u8);
            regs.write(REG_LBA2, (lba >> 40) as u8);
            regs.write(REG_FEATURES, 0);
            regs.write(REG_COUNT, count as u8);
            regs.write(REG_LBA0, lba as u8);
            regs.write(REG_LBA1, (lba >> 8) as u8);
            regs.write(REG_LBA2, (lba >> 16) as u8);
            regs.write(REG_DEVICE, device);
        } else {
            /* 256 is written as 0, which is what it means here */
            regs.write(REG_FEATURES, 0);
            regs.write(REG_COUNT, count as u8);
            regs.write(REG_LBA0, lba as u8);
            regs.write(REG_LBA1, (lba >> 8) as u8);
            regs.write(REG_LBA2, (lba >> 16) as u8);
            regs.write(REG_DEVICE, device | ((lba >> 24) & DEV_LBA28_HIGH) as u8);
        }
        regs.write(REG_COMMAND, command);
        regs.settle();
        Ok(())
    }

    fn pio_in(&self, regs: &Regs, lba: u64, count: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.issue(regs, if self.id.lba48 { CMD_READ_EXT } else { CMD_READ }, lba, count)?;
        for sector in buf.chunks_exact_mut(self.id.sector_size as usize) {
            regs.wait_data(COMMAND_MS)?;
            for pair in sector.chunks_exact_mut(2) {
                pair.copy_from_slice(&regs.data_in().to_le_bytes());
            }
        }
        regs.finish(COMMAND_MS)
    }

    fn pio_out(&self, regs: &Regs, lba: u64, count: u64, data: &[u8]) -> Result<(), Fault> {
        self.issue(regs, if self.id.lba48 { CMD_WRITE_EXT } else { CMD_WRITE }, lba, count)?;
        for sector in data.chunks_exact(self.id.sector_size as usize) {
            regs.wait_data(COMMAND_MS)?;
            for pair in sector.chunks_exact(2) {
                regs.data_out(u16::from_le_bytes([pair[0], pair[1]]));
            }
        }
        regs.finish(COMMAND_MS)
    }

    /// Its place on the channel: 0 the master, 1 the slave.
    fn position(&self) -> usize {
        usize::from(self.slave)
    }

    /// What a command came to, taken in: done, the device's run of commands
    /// that did not finish ends; failed, it is said with all there is to say
    /// of it and the channel reset, so that the next command starts from a
    /// device that is not halfway through this one -- and a device that does
    /// not come back from the reset, or that has not finished MAX_UNFINISHED
    /// commands running, is given up on. Whether the command went through.
    fn outcome(&self, state: &mut ChannelState, what: &str, lba: u64, count: u64, result: Result<(), Fault>)
        -> bool
    {
        let me = self.position();
        let fault = match result {
            Ok(()) => {
                state.unfinished[me] = 0;
                return true;
            }
            Err(fault) => fault,
        };
        let at = self.channel.at;
        match fault {
            Fault::Timeout { status } => {
                klog!(0, "ata: {} ({:02x}:{:02x}.{} channel {}): {} of {} sectors at {} timed out, status {:#04x}",
                    self.name(), at.bus, at.slot, at.func, at.channel, what, count, lba, status);
                state.unfinished[me] += 1;
            }
            Fault::Device { status, error } => {
                klog!(0, "ata: {} ({:02x}:{:02x}.{} channel {}): {} of {} sectors at {} failed, status {:#04x} \
                          error {:#04x}", self.name(), at.bus, at.slot, at.func, at.channel, what, count, lba, status,
                    error);
                /* The device answered: it is there */
                state.unfinished[me] = 0;
            }
        }
        let present = self.channel.present;
        let waited = [present[0] && !state.failed[0], present[1] && !state.failed[1]];
        match state.regs.reset(waited) {
            Ok(()) => {
                if state.unfinished[me] >= MAX_UNFINISHED {
                    self.channel.give_up(state, me, "commands in a row did not finish");
                }
            }
            Err((position, fault)) => {
                let why = match fault {
                    Fault::Timeout { status } => alloc::format!("busy after a reset, status {:#04x}", status),
                    Fault::Device { status, error } => {
                        alloc::format!("came back from a reset failed, status {:#04x} error {:#04x}", status, error)
                    }
                };
                self.channel.give_up(state, position, &why);
            }
        }
        false
    }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> bool {
        let size = self.id.sector_size as usize;
        if buf.is_empty() || buf.len() % size != 0 || !self.within(lba, (buf.len() / size) as u64) {
            klog!(0, "ata: {}: a read of {} bytes at {} is none this disk takes", self.name(), buf.len(), lba);
            return false;
        }
        let mut state = self.channel.state.lock();
        if state.failed[self.position()] {
            return false;
        }
        let mut at = lba;
        for chunk in buf.chunks_mut(MAX_SECTORS_PER_COMMAND as usize * size) {
            let count = (chunk.len() / size) as u64;
            let result = self.pio_in(&state.regs, at, count, chunk);
            if !self.outcome(&mut state, "read", at, count, result) {
                return false;
            }
            at += count;
        }
        true
    }

    fn write_sectors(&self, lba: u64, data: &[u8]) -> bool {
        let size = self.id.sector_size as usize;
        if data.is_empty() || data.len() % size != 0 || !self.within(lba, (data.len() / size) as u64) {
            klog!(0, "ata: {}: a write of {} bytes at {} is none this disk takes", self.name(), data.len(), lba);
            return false;
        }
        let mut state = self.channel.state.lock();
        if state.failed[self.position()] {
            return false;
        }
        let mut at = lba;
        for chunk in data.chunks(MAX_SECTORS_PER_COMMAND as usize * size) {
            let count = (chunk.len() / size) as u64;
            let result = self.pio_out(&state.regs, at, count, chunk);
            if !self.outcome(&mut state, "write", at, count, result) {
                return false;
            }
            at += count;
        }
        true
    }

    /// A command that moves nothing -- a flush, a standby -- issued and
    /// taken in as every command is.
    fn non_data(&self, command: u8, what: &str, ms: u64) -> bool {
        let mut state = self.channel.state.lock();
        if state.failed[self.position()] {
            return false;
        }
        let regs = &state.regs;
        let result = regs.select(self.slave).and_then(|()| {
            regs.write(REG_COMMAND, command);
            regs.finish(ms)
        });
        self.outcome(&mut state, what, 0, 0, result)
    }

    /// The write cache pushed out, where the disk has a command for it.
    fn flush_cache(&self) -> bool {
        match self.id.flush {
            Flush::None => true,
            Flush::Flush => self.non_data(CMD_FLUSH, "flush", FLUSH_MS),
            Flush::FlushExt => self.non_data(CMD_FLUSH_EXT, "flush", FLUSH_MS),
        }
    }

    /// The disk told the power is about to go: STANDBY IMMEDIATE.
    fn standby(&self) -> bool {
        self.non_data(CMD_STANDBY, "standby", STANDBY_MS)
    }

    fn is_failed(&self) -> bool {
        self.channel.state.lock().failed[self.position()]
    }
}

impl BlockDriver for AtaDisk {
    fn capacity(&self) -> u64 {
        self.id.sectors
    }

    fn sector_size(&self) -> u64 {
        u64::from(self.id.sector_size)
    }

    fn read(&'static self, sector: u64, buf: &mut [u8]) -> bool {
        self.read_sectors(sector, buf)
    }

    /* FUA as a flush after the write: the disk's FUA commands are DMA ones,
     * and a flush is what makes the write durable either way. */
    fn write(&'static self, sector: u64, data: &[u8], fua: bool) -> bool {
        self.write_sectors(sector, data) && (!fua || self.flush_cache())
    }

    fn flush(&'static self) -> bool {
        self.flush_cache()
    }

    fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }
}

/* ---- bring-up ---- */

/// A native channel's command block and control register, from its BARs:
/// I/O BARs both, inside the port space. None for anything else.
fn native_ports(dev: &PciDevice, channel: u8) -> Option<(u16, u16)> {
    let cmd_bar = dev.get_bar(channel * 2);
    let ctl_bar = dev.get_bar(channel * 2 + 1);
    if cmd_bar & BAR_IO == 0 || ctl_bar & BAR_IO == 0 {
        return None;
    }
    let cmd = cmd_bar & BAR_IO_ADDRESS;
    let ctl = (ctl_bar & BAR_IO_ADDRESS).checked_add(CONTROL_OFFSET)?;
    if cmd == 0 || cmd > COMMAND_BLOCK_LAST || ctl > u32::from(u16::MAX) {
        return None;
    }
    Some((cmd as u16, ctl as u16))
}

/// Whether the command block at `cmd` is this driver's now -- false when a
/// channel taken already has it.
fn claim(cmd: u16) -> bool {
    let count = CLAIMED_COUNT.load(Ordering::Acquire);
    if CLAIMED.iter().take(count).any(|c| c.load(Ordering::Relaxed) == cmd) || count >= MAX_CHANNELS {
        return false;
    }
    CLAIMED[count].store(cmd, Ordering::Relaxed);
    CLAIMED_COUNT.store(count + 1, Ordering::Release);
    true
}

fn take_controller(dev: &PciDevice, controller: usize) {
    let command = dev.read_config16(PCI_COMMAND);
    if command & PCI_COMMAND_IO == 0 {
        klog!(0, "ata: {:02x}:{:02x}.{} had I/O decoding off: turned on", dev.bus, dev.slot, dev.func);
        dev.write_config16(PCI_COMMAND, command | PCI_COMMAND_IO);
    }

    for channel in 0..2u8 {
        let native_bit = if channel == 0 { PROGIF_PRIMARY_NATIVE } else { PROGIF_SECONDARY_NATIVE };
        let ports = if dev.prog_if & native_bit != 0 {
            native_ports(dev, channel)
        } else {
            Some(LEGACY[channel as usize])
        };
        let Some((cmd, ctl)) = ports else {
            klog!(0, "ata: {:02x}:{:02x}.{} channel {}: no I/O ports to reach it by",
                dev.bus, dev.slot, dev.func, channel);
            continue;
        };
        if !claim(cmd) {
            klog!(0, "ata: {:02x}:{:02x}.{} channel {}: ports {:#x} are another channel's already",
                dev.bus, dev.slot, dev.func, channel, cmd);
            continue;
        }
        let at = Where { bus: dev.bus, slot: dev.slot, func: dev.func, channel };
        take_channel(Regs { cmd, ctl }, at, controller * POSITIONS_PER_CONTROLLER
            + channel as usize * POSITIONS_PER_CHANNEL);
    }
}

fn take_channel(regs: Regs, at: Where, first_position: usize) {
    let present = [regs.present(false), regs.present(true)];
    if !present[0] && !present[1] {
        klog!(1, "ata: {:02x}:{:02x}.{} channel {} ({:#x}): nothing on it", at.bus, at.slot, at.func, at.channel,
            regs.cmd);
        return;
    }

    /* The device's interrupt off before anything is asked of it: this driver
     * polls, and the line is nobody's (see the crate's comment) */
    regs.control(CTL_NIEN);
    if let Err((position, fault)) = regs.reset(present) {
        klog!(0, "ata: {:02x}:{:02x}.{} channel {}: the {} did not come back from the reset: {:?}",
            at.bus, at.slot, at.func, at.channel, if position == 0 { "master" } else { "slave" }, fault);
        return;
    }

    let mut found: [Option<(bool, Identify)>; POSITIONS_PER_CHANNEL] = [None, None];
    for (i, slave) in [false, true].into_iter().enumerate() {
        if !present[i] {
            continue;
        }
        let Some(words) = regs.identify(slave) else {
            klog!(0, "ata: {:02x}:{:02x}.{} channel {} {}: no ATA disk answers IDENTIFY (a CD drive?)",
                at.bus, at.slot, at.func, at.channel, if slave { "slave" } else { "master" });
            continue;
        };
        match Identify::parse(&words, PAGE_SIZE) {
            Ok(id) => found[i] = Some((slave, id)),
            Err(why) => klog!(0, "ata: {:02x}:{:02x}.{} channel {} {}: not taken: {:?}",
                at.bus, at.slot, at.func, at.channel, if slave { "slave" } else { "master" }, why),
        }
    }
    if found.iter().all(Option::is_none) {
        return;
    }

    let state = ChannelState { regs, unfinished: [0; POSITIONS_PER_CHANNEL], failed: [false; POSITIONS_PER_CHANNEL] };
    let Some(state) = Mutex::new(state) else {
        klog!(0, "ata: no memory for a channel's lock");
        return;
    };
    /* For good: its disks point at it, and the block table never gives a
     * disk back. */
    let channel: &'static Channel = Box::leak(Box::new(Channel { state, at, present }));

    for (i, entry) in found.into_iter().enumerate() {
        if let Some((slave, id)) = entry {
            register(channel, slave, id, first_position + i);
        }
    }
}

fn register(channel: &'static Channel, slave: bool, id: Identify, position: usize) {
    let index = DISK_COUNT.load(Ordering::Relaxed);
    if index >= MAX_DISKS || position >= MAX_DISKS {
        klog!(0, "ata: no room for another disk");
        return;
    }
    let identity = identify::identity_word(&id.model, &id.serial);
    let disk: &'static AtaDisk = Box::leak(Box::new(AtaDisk {
        channel,
        slave,
        id,
        identity,
        name: [b'h', b'd', b'a' + position as u8],
    }));

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
    let at = channel.at;

    match block::register_driver(disk.name(), disk) {
        Some(_) => {
            let _ = DISKS[index].set(disk);
            DISK_COUNT.store(index + 1, Ordering::Release);
            klog!(0, "ata: {} at {:02x}:{:02x}.{} channel {} {}: {} ({}), {} sectors of {} bytes (physical {}), {}, \
                      write cache {}, {}{}, PIO polled",
                disk.name(), at.bus, at.slot, at.func, at.channel, if slave { "slave" } else { "master" },
                model, firmware, disk.id.sectors, disk.id.sector_size, disk.id.physical_sector_size, spins,
                match disk.id.write_cache { Some(true) => "on", Some(false) => "off", None => "none" },
                if disk.id.lba48 { "LBA48" } else { "LBA28" },
                if disk.id.trim { ", TRIM" } else { "" });
        }
        None => klog!(0, "ata: the device table would not take {}", disk.name()),
    }
}

/// The PCI IDE controllers, their channels and the disks on them.
pub fn init() {
    for i in 0..pci::device_count() {
        let Some(dev) = pci::get_device(i) else { continue };
        if dev.class != CLASS_STORAGE || dev.subclass != SUBCLASS_IDE {
            continue;
        }
        let controller = CONTROLLERS.load(Ordering::Relaxed);
        if controller >= MAX_CONTROLLERS {
            klog!(0, "ata: {:02x}:{:02x}.{}: past the {} controllers taken", dev.bus, dev.slot, dev.func,
                MAX_CONTROLLERS);
            break;
        }
        CONTROLLERS.store(controller + 1, Ordering::Relaxed);
        klog!(0, "ata: IDE controller {:04x}:{:04x} at {:02x}:{:02x}.{}, programming interface {:#04x}",
            dev.vendor, dev.device, dev.bus, dev.slot, dev.func, dev.prog_if);
        take_controller(&dev, controller);
    }
}

/// On the way down: every disk's write cache pushed out, so that what the
/// filesystems wrote before they were unmounted is on the medium when the
/// power goes -- and with `power_off` the disk put in standby, as the power
/// is about to go: not for a reboot, which leaves an HDD spinning to come
/// back up the sooner. A disk given up on is skipped.
pub fn shutdown(power_off: bool) {
    let count = DISK_COUNT.load(Ordering::Acquire).min(MAX_DISKS);
    for slot in DISKS[..count].iter() {
        let Some(disk) = slot.get() else { continue };
        if disk.is_failed() {
            klog!(0, "ata: {}: failed, so neither flushed nor put in standby", disk.name());
            continue;
        }
        if !disk.flush_cache() {
            klog!(0, "ata: {}: the write cache was not flushed on the way down", disk.name());
        }
        if power_off {
            if disk.standby() {
                klog!(0, "ata: {}: in standby for the power-off", disk.name());
            } else {
                klog!(0, "ata: {}: did not go to standby for the power-off", disk.name());
            }
        }
    }
}
