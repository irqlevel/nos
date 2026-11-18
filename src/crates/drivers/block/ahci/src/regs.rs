//! What the driver uses of AHCI 1.3.1 and ATA8-ACS: the HBA's and a port's
//! registers, the FIS a command goes as, a port's memory, and how long each
//! thing may take.

use kcore::consts::PAGE_SIZE;

/* ---- PCI ---- */

pub const CLASS_STORAGE: u8 = 0x01;
pub const SUBCLASS_SATA: u8 = 0x06;
pub const PROGIF_AHCI: u8 = 0x01;
/// The ABAR: the HBA's registers.
pub const ABAR: u8 = 5;
pub const BAR_IO: u32 = 1;
pub const BAR_ADDRESS: u32 = !0xF;
pub const PCI_COMMAND: u16 = 0x04;
pub const PCI_COMMAND_MEMORY: u16 = 1 << 1;

/* ---- the generic host control registers (3.1) ---- */

pub const HBA_CAP: usize = 0x00;
pub const HBA_GHC: usize = 0x04;
/// Which ports have an interrupt pending: a port's bit is cleared after its
/// PxIS is (10.7.2).
pub const HBA_IS: usize = 0x08;
pub const HBA_PI: usize = 0x0C;
pub const HBA_VS: usize = 0x10;
pub const HBA_CAP2: usize = 0x24;
pub const HBA_BOHC: usize = 0x28;

/// CAP: ports less one, command slots less one, staggered spin-up, command
/// list override, NCQ, 64-bit.
pub const CAP_NP_MASK: u32 = 0x1F;
pub const CAP_NCS_SHIFT: u32 = 8;
pub const CAP_NCS_MASK: u32 = 0x1F;
pub const CAP_SSS: u32 = 1 << 27;
pub const CAP_SCLO: u32 = 1 << 24;
pub const CAP_SNCQ: u32 = 1 << 30;
pub const CAP_S64A: u32 = 1 << 31;
pub const CAP2_BOH: u32 = 1 << 0;
pub const BOHC_BOS: u32 = 1 << 0;
pub const BOHC_OOS: u32 = 1 << 1;
pub const BOHC_BB: u32 = 1 << 4;
pub const GHC_HR: u32 = 1 << 0;
pub const GHC_IE: u32 = 1 << 1;
pub const GHC_AE: u32 = 1 << 31;
/// Before 1.2 there is no CAP2 and no BOHC.
pub const VS_1_2: u32 = 0x0001_0200;

/* ---- a port's registers (3.3), 0x80 of them a port from 0x100 ---- */

pub const PORTS_BASE: usize = 0x100;
pub const PORT_STRIDE: usize = 0x80;
/// CAP.NP's most: 32 ports.
pub const MAX_PORTS: usize = 32;
pub const PX_CLB: usize = 0x00;
pub const PX_CLBU: usize = 0x04;
pub const PX_FB: usize = 0x08;
pub const PX_FBU: usize = 0x0C;
pub const PX_IS: usize = 0x10;
pub const PX_IE: usize = 0x14;
pub const PX_CMD: usize = 0x18;
pub const PX_TFD: usize = 0x20;
pub const PX_SIG: usize = 0x24;
pub const PX_SSTS: usize = 0x28;
pub const PX_SCTL: usize = 0x2C;
pub const PX_SERR: usize = 0x30;
/// NCQ's tags the device has not finished: set by the host before PxCI,
/// cleared by the device's set device bits FIS.
pub const PX_SACT: usize = 0x34;
pub const PX_CI: usize = 0x38;

pub const CMD_ST: u32 = 1 << 0;
pub const CMD_SUD: u32 = 1 << 1;
pub const CMD_POD: u32 = 1 << 2;
pub const CMD_CLO: u32 = 1 << 3;
pub const CMD_FRE: u32 = 1 << 4;
pub const CMD_FR: u32 = 1 << 14;
pub const CMD_CR: u32 = 1 << 15;
pub const CMD_CPD: u32 = 1 << 20;
pub const CMD_ALPE: u32 = 1 << 26;

pub const TFD_ERR: u32 = 1 << 0;
pub const TFD_DRQ: u32 = 1 << 3;
pub const TFD_DF: u32 = 1 << 5;
pub const TFD_BSY: u32 = 1 << 7;
/// PxTFD: the device's status in its low byte, its error register above.
pub const TFD_STATUS_MASK: u32 = 0xFF;
pub const TFD_ERROR_SHIFT: u32 = 8;

/// PxIS: a command done -- a D2H register FIS, a PIO setup FIS (a PIO
/// command's last), a DMA setup FIS, NCQ's set device bits FIS.
pub const IS_DHRS: u32 = 1 << 0;
pub const IS_PSS: u32 = 1 << 1;
pub const IS_DSS: u32 = 1 << 2;
pub const IS_SDBS: u32 = 1 << 3;
/// A task file error: the device said a command failed.
pub const IS_TFES: u32 = 1 << 30;
/// What ends a command as failed: a task file error, a host bus fatal or
/// data error, an interface fatal error, an overflow.
pub const IS_ERRORS: u32 = IS_TFES | (1 << 29) | (1 << 28) | (1 << 27) | (1 << 24);
/// The link changed under the commands: PhyRdy changed (PRCS, PxSERR.DIAG.N)
/// or the device announced itself again (PCS, PxSERR.DIAG.X -- a COMINIT).
/// What it had is lost either way. Both are PxSERR's, cleared there, and with
/// the link kept out of its power states nothing else sets them.
pub const IS_LINK: u32 = (1 << 22) | (1 << 6);
/// What a port interrupts for: every command's end, every error, every
/// change of the link.
pub const IE_ALL: u32 = IS_DHRS | IS_PSS | IS_DSS | IS_SDBS | IS_ERRORS | IS_LINK;

pub const SSTS_DET_MASK: u32 = 0xF;
/// A device there and the PHY talking to it.
pub const SSTS_DET_PRESENT: u32 = 3;
pub const SSTS_SPD_SHIFT: u32 = 4;
pub const SSTS_SPD_MASK: u32 = 0xF;
pub const SCTL_DET_MASK: u32 = 0xF;
pub const SCTL_DET_COMRESET: u32 = 1;
/// SControl's IPM: neither partial nor slumber -- the link is kept awake,
/// since nothing here would wake it on time.
pub const SCTL_IPM_MASK: u32 = 0xF << 8;
pub const SCTL_IPM_NONE: u32 = 3 << 8;

pub const SIG_ATA: u32 = 0x0000_0101;
pub const SIG_ATAPI: u32 = 0xEB14_0101;

/* ---- the FIS a command goes as: a register FIS, host to device ---- */

pub const FIS_TYPE_H2D: u8 = 0x27;
pub const FIS_H2D_COMMAND: u8 = 1 << 7;
pub const FIS_DEVICE_LBA: u8 = 1 << 6;
/// An NCQ write's FUA: durable when it ends.
pub const FIS_DEVICE_FUA: u8 = 1 << 7;
pub const FIS_H2D_BYTES: usize = 20;
/// NCQ: the tag in the count register's bits 3-7.
pub const FIS_TAG_SHIFT: u32 = 3;

/* ---- a port's memory ---- */

/// The command list: 32 headers of 32 bytes, 1 KiB aligned.
pub const CL_OFFSET: usize = 0;
pub const HEADER_BYTES: usize = 32;
/// The received FIS area, 256 bytes, 256-aligned.
pub const FIS_OFFSET: usize = 1024;
/// A sector the driver's own commands read into: IDENTIFY DEVICE's words,
/// the NCQ Command Error log.
pub const SCRATCH_OFFSET: usize = 2048;
pub const SCRATCH_BYTES: usize = 512;
/// A command header's first word: the command FIS's length in dwords (an H2D
/// register FIS is five), the direction, and the PRDT's length.
pub const HDR_CFL_H2D: u32 = 5;
pub const HDR_WRITE: u32 = 1 << 6;
pub const HDR_PRDTL_SHIFT: u32 = 16;

/// The command slots a port has at most: the bits of PxCI.
pub const MAX_SLOTS: usize = 32;
/// Each slot's command table, 128-aligned: the command FIS at its start, its
/// PRDT from 0x80. 72 entries make it 1280 bytes, ten lines.
pub const TABLE_PRDT: usize = 0x80;
pub const CFIS_BYTES: usize = 64;
pub const PRD_BYTES: usize = 16;
pub const PRDS_PER_SLOT: usize = 72;
pub const TABLE_BYTES: usize = TABLE_PRDT + PRDS_PER_SLOT * PRD_BYTES;
const _: () = assert!(TABLE_BYTES % 128 == 0, "a command table is 128-aligned");
/// One PRD entry moves at most 4 MiB; its count is the bytes less one.
pub const PRD_MAX_BYTES: usize = 4 << 20;
pub const PRD_DBC_MASK: u32 = (1 << 22) - 1;

/// The most one command moves: what ext4's batch is, 64 blocks of 4 KiB,
/// and as fast a size as measured. The Supermicro's SSD read one command at
/// a time under this driver at 313 MiB/s in commands of 128 KiB, 315 in
/// commands of 256 and 293 in commands of 512 (blkload, its Ubuntu disk,
/// twice each to a tenth): bigger is not faster here, and past 256 slower.
/// A page a PRD entry, for a buffer that starts in the middle of a page, is
/// 65 entries; a slot's table has room besides for a batch of pieces
/// shorter than a page, each an entry of its own.
pub const MAX_BYTES_PER_COMMAND: usize = 256 << 10;
const _: () = assert!(MAX_BYTES_PER_COMMAND / PAGE_SIZE + 1 <= PRDS_PER_SLOT, "a command's pages fit its PRDT");
/// What an asynchronous request moves at most: a command's most from
/// anywhere in a page -- the block layer counts it from a page's start.
pub const ASYNC_MAX: usize = MAX_BYTES_PER_COMMAND + PAGE_SIZE;
pub const LBA28_LIMIT: u64 = 1 << 28;
/// A 28-bit command's 0 in the count register is 256, a 48-bit one's 65536.
pub const LBA28_COUNT_MAX: u64 = 256;
pub const LBA48_COUNT_MAX: u64 = 65536;

/* ---- how long things may take ---- */

pub const HANDOFF_MS: u64 = 25;
pub const HANDOFF_BUSY_MS: u64 = 2_000;
pub const HBA_RESET_MS: u64 = 1_000;
pub const PORT_STOP_MS: u64 = 500;
pub const LINK_MS: u64 = 1_000;
/// COMRESET held at least 1 ms (AHCI 10.4.2).
pub const COMRESET_HOLD_US: u64 = 1_000;
/// A device to come out of its reset: an HDD spins up first, and ATA gives a
/// reset 31 s.
pub const DEVICE_READY_MS: u64 = 31_000;
pub const IDENTIFY_MS: u64 = 5_000;
/// A command that moves data: an HDD may spin up or retry a sector.
pub const COMMAND_MS: u64 = 31_000;
pub const FLUSH_MS: u64 = 60_000;
/// An HDD spinning down takes seconds.
pub const STANDBY_MS: u64 = 31_000;
/// READ LOG EXT of the NCQ Command Error log.
pub const LOG_MS: u64 = 5_000;
/// How often the error-handling task looks, when nothing wakes it sooner:
/// for a command out past its time, and for a completion whose interrupt
/// never came.
pub const EH_TICK_MS: u64 = 100;
/// How long `start` waits for the interrupt of a command it has seen done.
pub const INTERRUPT_CHECK_MS: u64 = 1_000;
/// How long the error-handling task gives an interrupt it finds pending in
/// IS: an MSI on its way is taken in microseconds; one not taken by then
/// was lost, or the HBA holds IS latched and raises no other.
pub const MSI_GRACE_US: u64 = 1_000;
/// How many times a command goes to the disk when it was lost with others --
/// aborted by another's failure, or out when the port was reset.
pub const MAX_TRIES: u8 = 2;
