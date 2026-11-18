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
//! port then gets pages of its own: its command list, the area the HBA puts
//! the FISes it receives in, and one command table, whose PRDT has room for
//! 504 pieces of memory scattered anywhere. A disk is one command at a time,
//! slot 0, polled, under its port's lock: READ and WRITE DMA EXT straight to
//! and from the caller's buffer through the PRDT, its pages found by their
//! physical addresses, up to 256 KiB a command; a batch's pieces
//! (`read_pieces`, `write_pieces`) that follow one another on the disk one
//! command, wherever they are in the batch's buffer -- an ext4 batch of a
//! file's contiguous blocks is one read; FLUSH CACHE EXT; at power-off
//! STANDBY IMMEDIATE; IDENTIFY DEVICE read by `ataproto`, as the IDE driver
//! reads it.
//!
//! A command that fails is said with the port's registers, and the port is
//! brought back as AHCI 1.3.1 6.2.2.2 and 10.4.2 say: stopped -- and where
//! its command engine will not stop, reset (COMRESET) -- its errors cleared,
//! the device's busy cleared by CLO or a COMRESET, started again. A link lost
//! under a command (PxIS.PRCS or PCS) ends it at once rather than at its
//! timeout. A port that does not come back, or `ataproto::MAX_UNFINISHED`
//! commands running that did not finish, and the disk is failed: said once,
//! and every request of it refused at once from then on, rather than each
//! waiting out a timeout of its own.
//!
//! Disks are named by where they are connected, `ada` the first HBA's port
//! 0, so that a name is a port whatever else is plugged in.
//!
//! Left for later: NCQ and the 32 slots, the HBA's MSI, the asynchronous path,
//! TRIM, port multipliers, hot plug, an Intel HBA in RAID mode (class 0104),
//! whose interface is AHCI too.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use ataproto::cmd;
use ataproto::identify::{self, Flush, Identify, Rotation, WORDS};
use ataproto::MAX_UNFINISHED;
use block::{BlockDriver, Piece};
use kcore::consts::PAGE_SIZE;
use kcore::dma::{self, DmaBuffer, PhysMapping};
use kcore::io::MmioRegion;
use kcore::klog;
use kcore::once::Once;
use kcore::pci::{self, PciDevice};
use kcore::sync::Mutex;
use kcore::time::Deadline;

/* ---- PCI ---- */

const CLASS_STORAGE: u8 = 0x01;
const SUBCLASS_SATA: u8 = 0x06;
const PROGIF_AHCI: u8 = 0x01;
/// The ABAR: the HBA's registers.
const ABAR: u8 = 5;
const BAR_IO: u32 = 1;
const BAR_ADDRESS: u32 = !0xF;
const PCI_COMMAND: u16 = 0x04;
const PCI_COMMAND_MEMORY: u16 = 1 << 1;

/* ---- the generic host control registers (AHCI 1.3.1, 3.1) ---- */

const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0C;
const HBA_VS: usize = 0x10;
const HBA_CAP2: usize = 0x24;
const HBA_BOHC: usize = 0x28;

/// CAP: ports less one, staggered spin-up, command list override, 64-bit.
const CAP_NP_MASK: u32 = 0x1F;
const CAP_SSS: u32 = 1 << 27;
const CAP_SCLO: u32 = 1 << 24;
const CAP_S64A: u32 = 1 << 31;
const CAP2_BOH: u32 = 1 << 0;
const BOHC_BOS: u32 = 1 << 0;
const BOHC_OOS: u32 = 1 << 1;
const BOHC_BB: u32 = 1 << 4;
const GHC_HR: u32 = 1 << 0;
const GHC_IE: u32 = 1 << 1;
const GHC_AE: u32 = 1 << 31;
/// Before 1.2 there is no CAP2 and no BOHC.
const VS_1_2: u32 = 0x0001_0200;

/* ---- a port's registers (3.3), 0x80 of them a port from 0x100 ---- */

const PORTS_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
/// CAP.NP's most: 32 ports.
const MAX_PORTS: usize = 32;
const PX_CLB: usize = 0x00;
const PX_CLBU: usize = 0x04;
const PX_FB: usize = 0x08;
const PX_FBU: usize = 0x0C;
const PX_IS: usize = 0x10;
const PX_IE: usize = 0x14;
const PX_CMD: usize = 0x18;
const PX_TFD: usize = 0x20;
const PX_SIG: usize = 0x24;
const PX_SSTS: usize = 0x28;
const PX_SCTL: usize = 0x2C;
const PX_SERR: usize = 0x30;
const PX_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_CLO: u32 = 1 << 3;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const CMD_CPD: u32 = 1 << 20;
const CMD_ALPE: u32 = 1 << 26;

const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_DF: u32 = 1 << 5;
const TFD_BSY: u32 = 1 << 7;

/// PxIS bits that end a command as failed: a task file error, a host bus
/// fatal or data error, an interface fatal error, an overflow.
const IS_ERRORS: u32 = (1 << 30) | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 24);
/// PxIS bits that say the link changed under a command: PhyRdy changed
/// (PRCS, PxSERR.DIAG.N) or the device announced itself again (PCS,
/// PxSERR.DIAG.X -- a COMINIT). The command is lost either way. Both are
/// PxSERR's, cleared there (`clear_errors`), and with the link kept out of
/// its power states nothing else sets them.
const IS_LINK: u32 = (1 << 22) | (1 << 6);

const SSTS_DET_MASK: u32 = 0xF;
/// A device there and the PHY talking to it.
const SSTS_DET_PRESENT: u32 = 3;
const SSTS_SPD_SHIFT: u32 = 4;
const SSTS_SPD_MASK: u32 = 0xF;
const SCTL_DET_MASK: u32 = 0xF;
const SCTL_DET_COMRESET: u32 = 1;
/// SControl's IPM: neither partial nor slumber -- the link is kept awake,
/// since nothing here would wake it on time.
const SCTL_IPM_MASK: u32 = 0xF << 8;
const SCTL_IPM_NONE: u32 = 3 << 8;

const SIG_ATA: u32 = 0x0000_0101;
const SIG_ATAPI: u32 = 0xEB14_0101;

/* ---- a port's pages ---- */

/// What a port has of memory: one physically contiguous run of pages.
const PORT_PAGES: usize = 3;
const PORT_BYTES: usize = PORT_PAGES * PAGE_SIZE;
/// The command list: 32 headers of 32 bytes, 1 KiB aligned, slot 0 used.
const CL_OFFSET: usize = 0;
/// The received FIS area, 256 bytes, 256-aligned.
const FIS_OFFSET: usize = 1024;
/// IDENTIFY DEVICE's 512 bytes.
const IDENTIFY_OFFSET: usize = 1536;
const IDENTIFY_BYTES: usize = 512;
/// Slot 0's command table, 128-aligned: the command FIS at its start, the
/// PRDT from 0x80 to the end of the port's pages.
const TABLE_OFFSET: usize = PAGE_SIZE;
const CFIS_BYTES: usize = 64;
const PRDT_OFFSET: usize = TABLE_OFFSET + 0x80;
const PRD_BYTES: usize = 16;
const MAX_PRDS: usize = (PORT_BYTES - PRDT_OFFSET) / PRD_BYTES;
/// One PRD entry moves at most 4 MiB; its count is the bytes less one.
const PRD_MAX_BYTES: usize = 4 << 20;
const PRD_DBC_MASK: u32 = (1 << 22) - 1;
const HEADER_BYTES: usize = 32;

/// A command header's first word: the command FIS's length in dwords (an H2D
/// register FIS is five), the direction, and the PRDT's length.
const HDR_CFL_H2D: u32 = 5;
const HDR_WRITE: u32 = 1 << 6;
const HDR_PRDTL_SHIFT: u32 = 16;

/// The register FIS a command goes as.
const FIS_TYPE_H2D: u8 = 0x27;
const FIS_H2D_COMMAND: u8 = 1 << 7;
const FIS_DEVICE_LBA: u8 = 1 << 6;
const FIS_H2D_BYTES: usize = 20;

/// The most one command moves: what ext4's batch is, 64 blocks of 4 KiB,
/// and as fast a size as measured. The Supermicro's SSD read one command at
/// a time under this driver at 313 MiB/s in commands of 128 KiB, 315 in
/// commands of 256 and 293 in commands of 512 (blkload, its Ubuntu disk,
/// twice each to a tenth): bigger is not faster here, and past 256 slower.
/// A page a PRD entry, for a buffer that starts in the middle of a page, is
/// 65 entries; MAX_PRDS has room besides for a batch of pieces shorter than
/// a page, each an entry of its own.
const MAX_BYTES_PER_COMMAND: usize = 256 << 10;
const _: () = assert!(MAX_BYTES_PER_COMMAND / PAGE_SIZE + 1 <= MAX_PRDS, "a command's pages fit its PRDT");
const LBA28_LIMIT: u64 = 1 << 28;
/// A 28-bit command's 0 in the count register is 256, a 48-bit one's 65536.
const LBA28_COUNT_MAX: u64 = 256;
const LBA48_COUNT_MAX: u64 = 65536;

/* ---- how long things may take ---- */

const HANDOFF_MS: u64 = 25;
const HANDOFF_BUSY_MS: u64 = 2_000;
const HBA_RESET_MS: u64 = 1_000;
const PORT_STOP_MS: u64 = 500;
const LINK_MS: u64 = 1_000;
/// COMRESET held at least 1 ms (AHCI 10.4.2).
const COMRESET_HOLD_US: u64 = 1_000;
/// A device to come out of its reset: an HDD spins up first, and ATA gives a
/// reset 31 s.
const DEVICE_READY_MS: u64 = 31_000;
const IDENTIFY_MS: u64 = 5_000;
/// A command that moves data: an HDD may spin up or retry a sector.
const COMMAND_MS: u64 = 31_000;
const FLUSH_MS: u64 = 60_000;
/// An HDD spinning down takes seconds.
const STANDBY_MS: u64 = 31_000;

/// Disks named `ada`..`adz`, by their port's place across the HBAs.
const MAX_DISKS: usize = 26;
const MAX_HBAS: usize = 4;

static NEXT_POSITION: AtomicUsize = AtomicUsize::new(0);
static HBAS: AtomicUsize = AtomicUsize::new(0);
static DISKS: [Once<&'static AhciDisk>; MAX_DISKS] = [const { Once::new() }; MAX_DISKS];
static DISK_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Between two polls: the CPU to whatever else can run here once the boot
/// has a scheduler to give it to.
fn relax() {
    if block::interrupts_started() {
        kcore::task::yield_to_runnable();
    } else {
        core::hint::spin_loop();
    }
}

fn wait(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let mut deadline = Deadline::after_ms(ms);
    loop {
        if done() {
            return true;
        }
        if deadline.passed() {
            return done();
        }
        relax();
    }
}

/// Why a command did not go through.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// The HBA still had it after its time: PxIS, PxTFD, PxCI then.
    Timeout { is: u32, tfd: u32, ci: u32 },
    /// The HBA or the device said it failed: PxIS and PxTFD.
    Device { is: u32, tfd: u32 },
    /// Done, but fewer bytes moved than were asked for.
    Short { moved: u32, asked: usize },
    /// The link went from under it (`IS_LINK`): PxIS and PxSSTS then.
    Link { is: u32, ssts: u32 },
    /// A buffer the HBA cannot take: no physical address, an odd one, past
    /// 32 bits on an HBA without 64-bit addressing, more pieces than a PRDT.
    Buffer,
}

/// PxTFD: the device's status in its low byte, its error register above.
const TFD_STATUS_MASK: u32 = 0xFF;
const TFD_ERROR_SHIFT: u32 = 8;

impl Fault {
    /// What the log says of it.
    fn describe(self) -> String {
        match self {
            Fault::Timeout { is, tfd, ci } => alloc::format!(
                "timed out, is {:#x} ci {:#x}, status {:#x} error {:#x}", is, ci, tfd & TFD_STATUS_MASK,
                (tfd >> TFD_ERROR_SHIFT) & TFD_STATUS_MASK),
            Fault::Device { is, tfd } => alloc::format!(
                "failed, is {:#x}, status {:#x} error {:#x}", is, tfd & TFD_STATUS_MASK,
                (tfd >> TFD_ERROR_SHIFT) & TFD_STATUS_MASK),
            Fault::Short { moved, asked } => alloc::format!("moved {} of the {} bytes asked", moved, asked),
            Fault::Link { is, ssts } => alloc::format!("lost its link, is {:#x} ssts {:#x}", is, ssts),
            Fault::Buffer => String::from("a buffer the HBA cannot take"),
        }
    }
}

/* ---- a port ---- */

/// A port's registers and its pages, and how its disk is doing. Reached
/// through its disk's lock alone, so that a command -- its FIS, PRDT and
/// header written, issued, waited for -- is one holder's from start to end.
struct Port {
    regs: MmioRegion,
    mem: DmaBuffer,
    /// The HBA's CAP: 64-bit addressing, CLO.
    cap: u32,
    /// Commands in a row that did not finish: timed out, or lost the link.
    unfinished: u32,
    /// Given up on (`AhciDisk::give_up`): the port stopped, every request
    /// refused.
    failed: bool,
}

impl Port {
    fn read(&self, reg: usize) -> u32 {
        self.regs.read32(reg)
    }

    fn write(&self, reg: usize, value: u32) {
        self.regs.write32(reg, value);
    }

    /// Commands and received FISes off: the HBA does nothing with this
    /// port's memory from here on.
    fn stop(&self) -> bool {
        let command = self.read(PX_CMD);
        if command & CMD_ST != 0 {
            self.write(PX_CMD, command & !CMD_ST);
        }
        if !wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CR == 0) {
            return false;
        }
        let command = self.read(PX_CMD);
        if command & CMD_FRE != 0 {
            self.write(PX_CMD, command & !CMD_FRE);
        }
        wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_FR == 0)
    }

    fn clear_errors(&self) {
        self.write(PX_SERR, u32::MAX);
        self.write(PX_IS, u32::MAX);
    }

    fn linked(&self) -> bool {
        self.read(PX_SSTS) & SSTS_DET_MASK == SSTS_DET_PRESENT
    }

    fn comreset(&self) -> bool {
        let control = self.read(PX_SCTL);
        self.write(PX_SCTL, (control & !SCTL_DET_MASK) | SCTL_DET_COMRESET);
        kcore::time::delay_us(COMRESET_HOLD_US);
        self.write(PX_SCTL, control & !SCTL_DET_MASK);
        let linked = wait(LINK_MS, || self.linked());
        self.write(PX_SERR, u32::MAX);
        linked
    }

    fn ready(&self) -> bool {
        self.read(PX_TFD) & (TFD_BSY | TFD_DRQ) == 0
    }

    /// Commands on, once the device is not busy: FRE is on already.
    fn start(&self) -> bool {
        if !wait(DEVICE_READY_MS, || self.ready()) {
            return false;
        }
        let command = self.read(PX_CMD);
        self.write(PX_CMD, command | CMD_FRE | CMD_ST);
        true
    }

    /// Back from a failed command (AHCI 1.3.1, 6.2.2.2 and 10.4.2): stopped,
    /// errors cleared, and a COMRESET -- with ST off -- where the command
    /// engine would not stop, the link is gone, or the device stays busy and
    /// CLO cannot clear that; started again. False where none of it brings
    /// the port back.
    fn recover(&self) -> bool {
        let command = self.read(PX_CMD);
        self.write(PX_CMD, command & !CMD_ST);
        let stopped = wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CR == 0);
        self.clear_errors();
        let reset = !stopped || !self.linked() || !(self.ready() || self.override_busy());
        if reset && (!self.comreset() || !wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CR == 0)) {
            return false;
        }
        self.start()
    }

    /// CLO, where the HBA has it: the device's busy and DRQ taken out of
    /// PxTFD, so that a command can be issued to it again. Whether the
    /// device then reads as ready.
    fn override_busy(&self) -> bool {
        if self.cap & CAP_SCLO == 0 {
            return false;
        }
        let command = self.read(PX_CMD);
        self.write(PX_CMD, command | CMD_CLO);
        wait(PORT_STOP_MS, || self.read(PX_CMD) & CMD_CLO == 0) && self.ready()
    }

    /// One PRD entry: `len` bytes at `phys`.
    fn put_prd(&mut self, index: usize, phys: u64, len: usize) -> Result<(), Fault> {
        if index >= MAX_PRDS || len == 0 || len % 2 != 0 || len > PRD_MAX_BYTES || phys & 1 != 0
            || (phys >> 32 != 0 && self.cap & CAP_S64A == 0)
        {
            return Err(Fault::Buffer);
        }
        let entry = self.mem.bytes_mut(PRDT_OFFSET + index * PRD_BYTES, PRD_BYTES).ok_or(Fault::Buffer)?;
        entry[0..4].copy_from_slice(&(phys as u32).to_le_bytes());
        entry[4..8].copy_from_slice(&((phys >> 32) as u32).to_le_bytes());
        entry[8..12].copy_from_slice(&0u32.to_le_bytes());
        entry[12..16].copy_from_slice(&(((len - 1) as u32) & PRD_DBC_MASK).to_le_bytes());
        Ok(())
    }

    /// The PRDT over `len` bytes at the kernel's `addr`, its pages found by
    /// their physical addresses and runs of them that are contiguous made one
    /// entry: how many entries.
    fn prdt_for(&mut self, addr: usize, len: usize) -> Result<usize, Fault> {
        let mut entries = 0usize;
        let mut run: Option<(u64, usize)> = None;
        let mut done = 0usize;
        while done < len {
            let at = addr.checked_add(done).ok_or(Fault::Buffer)?;
            let piece = (PAGE_SIZE - at % PAGE_SIZE).min(len - done);
            let phys = dma::virt_to_phys(at as *const u8);
            if phys == 0 {
                return Err(Fault::Buffer);
            }
            run = match run {
                Some((start, bytes)) if start.checked_add(bytes as u64) == Some(phys)
                    && bytes + piece <= PRD_MAX_BYTES => Some((start, bytes + piece)),
                Some((start, bytes)) => {
                    self.put_prd(entries, start, bytes)?;
                    entries += 1;
                    Some((phys, piece))
                }
                None => Some((phys, piece)),
            };
            done += piece;
        }
        if let Some((start, bytes)) = run {
            self.put_prd(entries, start, bytes)?;
            entries += 1;
        }
        Ok(entries)
    }

    /// The PRDT of the one command that takes `pieces` from the first on,
    /// each `len` bytes at `phys + at` of a batch's buffer -- for as long as
    /// they follow one another on the disk and the command has room: at most
    /// `limit` bytes and MAX_PRDS entries, pieces next to each other in
    /// memory one entry. How many pieces it takes, their bytes, and the
    /// entries.
    fn prdt_for_pieces(&mut self, phys: u64, pieces: &[Piece], sector_size: usize, limit: usize)
        -> Result<(usize, usize, usize), Fault>
    {
        let mut taken = 0usize;
        let mut bytes = 0usize;
        let mut entries = 0usize;
        let mut run: Option<(u64, usize)> = None;
        let mut next_sector = pieces.first().map_or(0, |p| p.sector);
        for p in pieces {
            if p.sector != next_sector || p.len == 0 || p.len % sector_size != 0
                || bytes.checked_add(p.len).is_none_or(|total| total > limit)
            {
                break;
            }
            let at = phys.checked_add(p.at as u64).ok_or(Fault::Buffer)?;
            run = match run {
                Some((start, len)) if start.checked_add(len as u64) == Some(at) && len + p.len <= PRD_MAX_BYTES => {
                    Some((start, len + p.len))
                }
                Some((start, len)) => {
                    /* Room for the run closed here and the one this piece
                     * opens, or the command ends before it */
                    if entries + 2 > MAX_PRDS {
                        break;
                    }
                    self.put_prd(entries, start, len)?;
                    entries += 1;
                    Some((at, p.len))
                }
                None => Some((at, p.len)),
            };
            bytes += p.len;
            next_sector = p.sector.checked_add((p.len / sector_size) as u64).ok_or(Fault::Buffer)?;
            taken += 1;
        }
        if let Some((start, len)) = run {
            self.put_prd(entries, start, len)?;
            entries += 1;
        }
        if taken == 0 {
            return Err(Fault::Buffer);
        }
        Ok((taken, bytes, entries))
    }

    /// Slot 0's command -- `fis`, over `prds` PRD entries already written --
    /// issued and waited for: the bytes the HBA says it moved.
    fn execute(&mut self, fis: &[u8; FIS_H2D_BYTES], prds: usize, write: bool, ms: u64) -> Result<u32, Fault> {
        {
            let table = self.mem.bytes_mut(TABLE_OFFSET, CFIS_BYTES).ok_or(Fault::Buffer)?;
            table.fill(0);
            table[..FIS_H2D_BYTES].copy_from_slice(fis);
        }
        let ctba = self.mem.phys() + TABLE_OFFSET as u64;
        {
            let header = self.mem.bytes_mut(CL_OFFSET, HEADER_BYTES).ok_or(Fault::Buffer)?;
            header.fill(0);
            let flags = HDR_CFL_H2D | if write { HDR_WRITE } else { 0 } | ((prds as u32) << HDR_PRDTL_SHIFT);
            header[0..4].copy_from_slice(&flags.to_le_bytes());
            header[8..12].copy_from_slice(&(ctba as u32).to_le_bytes());
            header[12..16].copy_from_slice(&((ctba >> 32) as u32).to_le_bytes());
        }

        self.write(PX_IS, u32::MAX);
        /* The header, the table and the PRDT in memory before the HBA is told
         * to fetch them */
        kcore::barrier::dma_wmb();
        self.write(PX_CI, 1);

        let mut deadline = Deadline::after_ms(ms);
        loop {
            let is = self.read(PX_IS);
            if is & IS_ERRORS != 0 {
                return Err(Fault::Device { is, tfd: self.read(PX_TFD) });
            }
            if self.read(PX_CI) & 1 == 0 {
                break;
            }
            /* Not done, and the link changed under it: lost, and said now
             * rather than at its timeout */
            if is & IS_LINK != 0 {
                return Err(Fault::Link { is, ssts: self.read(PX_SSTS) });
            }
            if deadline.passed() {
                return Err(Fault::Timeout { is, tfd: self.read(PX_TFD), ci: self.read(PX_CI) });
            }
            relax();
        }
        /* What the HBA wrote -- the data, the byte count -- read after it said
         * it was done */
        kcore::barrier::dma_rmb();
        let tfd = self.read(PX_TFD);
        if tfd & (TFD_ERR | TFD_DF) != 0 {
            return Err(Fault::Device { is: self.read(PX_IS), tfd });
        }
        let moved = self.mem.bytes(CL_OFFSET + 4, 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
        Ok(moved)
    }

    /// IDENTIFY DEVICE, into the port's own page.
    fn identify(&mut self) -> Result<[u16; WORDS], Fault> {
        let mut fis = [0u8; FIS_H2D_BYTES];
        fis[0] = FIS_TYPE_H2D;
        fis[1] = FIS_H2D_COMMAND;
        fis[2] = cmd::IDENTIFY;
        let phys = self.mem.phys() + IDENTIFY_OFFSET as u64;
        self.put_prd(0, phys, IDENTIFY_BYTES)?;
        let moved = self.execute(&fis, 1, false, IDENTIFY_MS)?;
        if moved as usize != IDENTIFY_BYTES {
            return Err(Fault::Short { moved, asked: IDENTIFY_BYTES });
        }
        let bytes = self.mem.bytes(IDENTIFY_OFFSET, IDENTIFY_BYTES).ok_or(Fault::Buffer)?;
        let mut words = [0u16; WORDS];
        for (word, pair) in words.iter_mut().zip(bytes.chunks_exact(2)) {
            *word = u16::from_le_bytes([pair[0], pair[1]]);
        }
        Ok(words)
    }
}

/// The register FIS of a read, a write or a flush: LBA48 where the disk has
/// it, a 28-bit command's address and count otherwise. `count` is at most
/// 65536 for the one and 256 for the other, which the callers keep to.
fn command_fis(command: u8, lba: u64, count: u64, lba48: bool) -> [u8; FIS_H2D_BYTES] {
    let mut fis = [0u8; FIS_H2D_BYTES];
    fis[0] = FIS_TYPE_H2D;
    fis[1] = FIS_H2D_COMMAND;
    fis[2] = command;
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    if lba48 {
        fis[7] = FIS_DEVICE_LBA;
        fis[8] = (lba >> 24) as u8;
        fis[9] = (lba >> 32) as u8;
        fis[10] = (lba >> 40) as u8;
        fis[13] = (count >> 8) as u8;
    } else {
        fis[7] = FIS_DEVICE_LBA | ((lba >> 24) & 0x0F) as u8;
    }
    /* 65536 and 256 are 0 here, which is what they mean */
    fis[12] = count as u8;
    fis
}

/* ---- a disk ---- */

struct AhciDisk {
    port: Mutex<Port>,
    /// For the log: the HBA's address and the port's number on it.
    at: (u16, u16, u16, u32),
    id: Identify,
    identity: Option<String>,
    name: [u8; 3],
}

impl AhciDisk {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("ad?")
    }

    /// Whether `count` sectors from `lba` are inside the disk -- and inside
    /// what 28 bits reach on one without 48-bit commands. The table checked
    /// what it hands down; this is the driver's own check on the address it
    /// is about to give the device.
    fn within(&self, lba: u64, count: u64) -> bool {
        let limit = if self.id.lba48 { self.id.sectors } else { self.id.sectors.min(LBA28_LIMIT) };
        count != 0 && lba.checked_add(count).is_some_and(|end| end <= limit)
    }

    /// The most bytes one command moves on this disk: MAX_BYTES_PER_COMMAND,
    /// or what its count register reaches, whole sectors.
    fn per_command(&self) -> usize {
        let size = self.id.sector_size as usize;
        let count = if self.id.lba48 { LBA48_COUNT_MAX } else { LBA28_COUNT_MAX } as usize;
        (MAX_BYTES_PER_COMMAND / size).min(count).max(1) * size
    }

    fn data_command(&self, write: bool) -> u8 {
        match (write, self.id.lba48) {
            (false, true) => cmd::READ_DMA_EXT,
            (true, true) => cmd::WRITE_DMA_EXT,
            (false, false) => cmd::READ_DMA,
            (true, false) => cmd::WRITE_DMA,
        }
    }

    /// What a command came to, taken in: done, the port's run of commands
    /// that did not finish ends; failed, it is said with the port's
    /// registers and the port brought back -- and where it will not come
    /// back, or MAX_UNFINISHED commands running have not finished, the disk
    /// is given up on. Whether the command went through.
    fn outcome(&self, port: &mut Port, what: &str, lba: u64, count: u64, result: Result<(), Fault>) -> bool {
        let fault = match result {
            Ok(()) => {
                port.unfinished = 0;
                return true;
            }
            Err(fault) => fault,
        };
        let (bus, slot, func, index) = self.at;
        klog!(0, "ahci: {} ({:02x}:{:02x}.{} port {}): {} of {} sectors at {}: {}, serr {:#x} ssts {:#x}",
            self.name(), bus, slot, func, index, what, count, lba, fault.describe(), port.read(PX_SERR),
            port.read(PX_SSTS));
        match fault {
            /* Nothing was handed to the HBA: nothing to bring back */
            Fault::Buffer => return false,
            Fault::Timeout { .. } | Fault::Link { .. } => port.unfinished += 1,
            /* The device answered: it is there */
            Fault::Device { .. } | Fault::Short { .. } => port.unfinished = 0,
        }
        if !port.recover() {
            self.give_up(port, "the port did not come back");
        } else if port.unfinished >= MAX_UNFINISHED {
            self.give_up(port, "commands in a row did not finish");
        }
        false
    }

    /// The disk failed for good: its port stopped, so that the HBA does
    /// nothing more in its memory, and every request of it refused from here
    /// on at once -- said once, here, rather than as a timeout a request.
    fn give_up(&self, port: &mut Port, why: &str) {
        port.failed = true;
        let _ = port.stop();
        let (bus, slot, func, index) = self.at;
        klog!(0, "ahci: {} ({:02x}:{:02x}.{} port {}): {} (tfd {:#x} cmd {:#x} ssts {:#x}): the disk is failed, \
                  and every request of it refused from here on",
            self.name(), bus, slot, func, index, why, port.read(PX_TFD), port.read(PX_CMD), port.read(PX_SSTS));
    }

    /// Whole sectors to or from `addr`, `len` bytes, from `lba` on: in
    /// commands of at most `per_command`, the buffer the device's.
    fn transfer(&self, lba: u64, addr: usize, len: usize, write: bool) -> bool {
        let size = self.id.sector_size as usize;
        let what = if write { "write" } else { "read" };
        if len == 0 || len % size != 0 || !self.within(lba, (len / size) as u64) {
            klog!(0, "ahci: {}: a {} of {} bytes at {} is none this disk takes", self.name(), what, len, lba);
            return false;
        }
        let per_command = self.per_command();
        let command = self.data_command(write);

        let mut port = self.port.lock();
        if port.failed {
            return false;
        }
        let mut done = 0usize;
        while done < len {
            let bytes = per_command.min(len - done);
            let count = (bytes / size) as u64;
            let at = lba + (done / size) as u64;
            let result = port.prdt_for(addr + done, bytes).and_then(|prds| {
                let fis = command_fis(command, at, count, self.id.lba48);
                port.execute(&fis, prds, write, COMMAND_MS)
            }).and_then(|moved| {
                if moved as usize == bytes { Ok(()) } else { Err(Fault::Short { moved, asked: bytes }) }
            });
            if !self.outcome(&mut port, what, at, count, result) {
                return false;
            }
            done += bytes;
        }
        true
    }

    /// A batch (`read_pieces`, `write_pieces`): its pieces, each `len`
    /// bytes at `phys + at` of the batch's buffer, a command for every run
    /// of them that follow one another on the disk -- a run of a file's
    /// contiguous blocks one command, wherever its pieces are in the buffer.
    /// The table has checked every piece against the buffer and the device;
    /// the command's range is checked again here, as `transfer`'s is.
    fn batch(&self, write: bool, phys: u64, base: u64, pieces: &[Piece]) -> bool {
        let size = self.id.sector_size as usize;
        let what = if write { "batch write" } else { "batch read" };
        let per_command = self.per_command();
        let command = self.data_command(write);

        let mut port = self.port.lock();
        if port.failed {
            return false;
        }
        let mut next = 0usize;
        while let Some(first) = pieces.get(next) {
            let Some(lba) = base.checked_add(first.sector) else { return false };
            let (taken, result, count) = match port.prdt_for_pieces(phys, &pieces[next..], size, per_command) {
                Ok((taken, bytes, prds)) => {
                    let count = (bytes / size) as u64;
                    let result = if self.within(lba, count) {
                        let fis = command_fis(command, lba, count, self.id.lba48);
                        port.execute(&fis, prds, write, COMMAND_MS).and_then(|moved| {
                            if moved as usize == bytes { Ok(()) } else { Err(Fault::Short { moved, asked: bytes }) }
                        })
                    } else {
                        Err(Fault::Buffer)
                    };
                    (taken, result, count)
                }
                Err(fault) => (0, Err(fault), (first.len / size) as u64),
            };
            if !self.outcome(&mut port, what, lba, count, result) {
                return false;
            }
            next += taken;
        }
        true
    }

    /// A command that moves nothing -- a flush, a standby -- issued and
    /// taken in as every command is.
    fn non_data(&self, command: u8, what: &str, ms: u64) -> bool {
        let mut port = self.port.lock();
        if port.failed {
            return false;
        }
        let fis = command_fis(command, 0, 0, false);
        let result = port.execute(&fis, 0, false, ms).map(|_| ());
        self.outcome(&mut port, what, 0, 0, result)
    }

    /// The write cache pushed out, where the disk has a command for it.
    fn flush_cache(&self) -> bool {
        match self.id.flush {
            Flush::None => true,
            Flush::Flush => self.non_data(cmd::FLUSH_CACHE, "flush", FLUSH_MS),
            Flush::FlushExt => self.non_data(cmd::FLUSH_CACHE_EXT, "flush", FLUSH_MS),
        }
    }

    /// The disk told the power is about to go: STANDBY IMMEDIATE.
    fn standby(&self) -> bool {
        self.non_data(cmd::STANDBY_IMMEDIATE, "standby", STANDBY_MS)
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

    fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
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
    /* AHCI mode on again, no interrupt -- this driver polls -- and what
     * firmware set in CAP and PI back where the reset took it: both are
     * write-once on Intel's, set by the BIOS, and some reset them */
    regs.write32(HBA_GHC, GHC_AE);
    if regs.read32(HBA_CAP) != cap {
        regs.write32(HBA_CAP, cap);
    }
    if regs.read32(HBA_PI) != pi {
        regs.write32(HBA_PI, pi);
    }
    let ghc = regs.read32(HBA_GHC);
    if ghc & GHC_IE != 0 {
        regs.write32(HBA_GHC, ghc & !GHC_IE);
    }
    Some((cap, pi))
}

/// A port made ready to find its device: stopped, its page given it, its
/// FIS receive on, spun up where the HBA staggers that, its link kept from
/// power states. Every port is this first, so that all the links come up
/// together and are waited for once (`finish_port`). False where the port
/// cannot be taken.
fn prepare_port(port: &mut Port, name: (u16, u16, u16, u32)) -> bool {
    let (bus, slot, func, index) = name;
    if !port.stop() {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: would not stop", bus, slot, func, index);
        return false;
    }

    let clb = port.mem.phys() + CL_OFFSET as u64;
    let fb = port.mem.phys() + FIS_OFFSET as u64;
    /* The command list, the FIS area and the command table are all in it */
    let last = port.mem.phys() + (PORT_BYTES - 1) as u64;
    if last >> 32 != 0 && port.cap & CAP_S64A == 0 {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: its pages are past 4 GiB and the HBA addresses 32 bits", bus,
            slot, func, index);
        return false;
    }
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
/// one wait every port of the HBA shares -- and its device, as far as an
/// ATA disk's IDENTIFY: its words, or None, with a line where there was
/// something to say.
fn finish_port(port: &mut Port, name: (u16, u16, u16, u32), links: &mut Deadline) -> Option<[u16; WORDS]> {
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
            return None;
        }
    }
    if !wait(DEVICE_READY_MS, || port.ready()) {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: the device stays busy (tfd {:#x})", bus, slot, func, index,
            port.read(PX_TFD));
        let _ = port.stop();
        return None;
    }
    let signature = port.read(PX_SIG);
    if signature != SIG_ATA {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: {} (signature {:#x}) passed over", bus, slot, func, index,
            if signature == SIG_ATAPI { "a packet device" } else { "not an ATA disk" }, signature);
        let _ = port.stop();
        return None;
    }

    port.clear_errors();
    if !port.start() {
        klog!(0, "ahci: {:02x}:{:02x}.{} port {}: would not start", bus, slot, func, index);
        let _ = port.stop();
        return None;
    }
    match port.identify() {
        Ok(words) => Some(words),
        Err(fault) => {
            klog!(0, "ahci: {:02x}:{:02x}.{} port {}: IDENTIFY {}", bus, slot, func, index, fault.describe());
            let _ = port.stop();
            None
        }
    }
}

fn register(port: Port, id: Identify, at: (u16, u16, u16, u32), position: usize) {
    let index = DISK_COUNT.load(Ordering::Relaxed);
    if index >= MAX_DISKS || position >= MAX_DISKS {
        klog!(0, "ahci: no room for another disk");
        let _ = port.stop();
        return;
    }
    let speed = (port.read(PX_SSTS) >> SSTS_SPD_SHIFT) & SSTS_SPD_MASK;
    let Some(port) = Mutex::new(port) else {
        klog!(0, "ahci: no memory for a port's lock");
        return;
    };
    let identity = identify::identity_word(&id.model, &id.serial);
    let disk: &'static AhciDisk = Box::leak(Box::new(AhciDisk {
        port,
        at,
        id,
        identity,
        name: [b'a', b'd', b'a' + position as u8],
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
    let link = match speed {
        1 => "1.5 Gbps",
        2 => "3 Gbps",
        3 => "6 Gbps",
        _ => "speed not said",
    };
    let (bus, slot, func, port_index) = at;

    match block::register_driver(disk.name(), disk) {
        Some(_) => {
            let _ = DISKS[index].set(disk);
            DISK_COUNT.store(index + 1, Ordering::Release);
            klog!(0, "ahci: {} at {:02x}:{:02x}.{} port {}: {} ({}), {} sectors of {} bytes (physical {}), {}, \
                      write cache {}, {}{}, link {}, polled",
                disk.name(), bus, slot, func, port_index, model, firmware, disk.id.sectors, disk.id.sector_size,
                disk.id.physical_sector_size, spins,
                match disk.id.write_cache { Some(true) => "on", Some(false) => "off", None => "none" },
                if disk.id.lba48 { "LBA48" } else { "LBA28" },
                if disk.id.trim { ", TRIM" } else { "" }, link);
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
    klog!(0, "ahci: HBA {:04x}:{:04x} at {:02x}:{:02x}.{}, version {:#x}, {} ports (implemented {:#x}), {}, {}",
        dev.vendor, dev.device, dev.bus, dev.slot, dev.func, regs.read32(HBA_VS), ports, pi,
        if cap & CAP_S64A != 0 { "64-bit" } else { "32-bit" },
        if cap & CAP_SSS != 0 { "staggered spin-up" } else { "no staggered spin-up" });

    let base = NEXT_POSITION.fetch_add(ports, Ordering::Relaxed);
    let mut prepared: Vec<(usize, Port)> = Vec::new();
    for index in 0..ports.min(MAX_PORTS) {
        if pi & (1 << index) == 0 {
            continue;
        }
        let Some(window) = regs.window(PORTS_BASE + index * PORT_STRIDE) else { continue };
        let Some(mem) = DmaBuffer::new(PORT_PAGES) else {
            klog!(0, "ahci: no memory for a port's pages");
            continue;
        };
        let mut port = Port { regs: window, mem, cap, unfinished: 0, failed: false };
        if prepare_port(&mut port, (dev.bus, dev.slot, dev.func, index as u32)) && prepared.try_reserve(1).is_ok() {
            prepared.push((index, port));
        }
    }

    /* One wait for every port's link: a port with nothing on it waits
     * out what is left of it, not a second of its own */
    let mut links = Deadline::after_ms(LINK_MS);
    for (index, mut port) in prepared {
        let name = (dev.bus, dev.slot, dev.func, index as u32);
        let Some(words) = finish_port(&mut port, name, &mut links) else { continue };
        match Identify::parse(&words, PAGE_SIZE) {
            Ok(id) => register(port, id, name, base + index),
            Err(why) => {
                klog!(0, "ahci: {:02x}:{:02x}.{} port {}: not taken: {:?}", dev.bus, dev.slot, dev.func, index, why);
                let _ = port.stop();
            }
        }
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
        if HBAS.fetch_add(1, Ordering::Relaxed) >= MAX_HBAS {
            klog!(0, "ahci: {:02x}:{:02x}.{}: past the {} HBAs taken", dev.bus, dev.slot, dev.func, MAX_HBAS);
            break;
        }
        take(&dev);
    }
}

/// On the way down: every disk's cache flushed -- and with `power_off` the
/// disk put in standby, as the power is about to go: not for a reboot,
/// which leaves an HDD spinning to come back up the sooner -- and every port
/// stopped, so that the HBA does nothing more with memory the kernel is done
/// with. A disk given up on is skipped: its port is stopped already.
pub fn shutdown(power_off: bool) {
    let count = DISK_COUNT.load(Ordering::Acquire).min(MAX_DISKS);
    for slot in DISKS[..count].iter() {
        let Some(disk) = slot.get() else { continue };
        if disk.port.lock().failed {
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
        let port = disk.port.lock();
        if !port.failed && !port.stop() {
            klog!(0, "ahci: {}: the port did not stop", disk.name());
        }
    }
}
