//! The ATA command set as the tree's two ATA disk drivers speak it --
//! `drivers/block/ata`, through a PCI IDE controller's taskfile, and
//! `drivers/block/ahci`, through an AHCI port's FISes: what IDENTIFY DEVICE
//! says ([`identify`]), the commands' numbers ([`cmd`]), and the NCQ Command
//! Error log a disk names a failed queued command in ([`ncq_error`]).
//! Written once, here, so that the two cannot read a disk differently.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod identify;

/// The commands the drivers issue (ATA8-ACS).
pub mod cmd {
    pub const IDENTIFY: u8 = 0xEC;
    pub const READ_SECTORS: u8 = 0x20;
    pub const READ_SECTORS_EXT: u8 = 0x24;
    pub const READ_DMA_EXT: u8 = 0x25;
    pub const READ_LOG_EXT: u8 = 0x2F;
    pub const WRITE_SECTORS: u8 = 0x30;
    pub const WRITE_SECTORS_EXT: u8 = 0x34;
    pub const WRITE_DMA_EXT: u8 = 0x35;
    /// WRITE DMA EXT past the cache: durable when it ends.
    pub const WRITE_DMA_FUA_EXT: u8 = 0x3D;
    /// NCQ's reads and writes: the count in the features registers, the
    /// tag in the count register's bits 3-7.
    pub const READ_FPDMA_QUEUED: u8 = 0x60;
    pub const WRITE_FPDMA_QUEUED: u8 = 0x61;
    pub const READ_DMA: u8 = 0xC8;
    pub const WRITE_DMA: u8 = 0xCA;
    pub const FLUSH_CACHE: u8 = 0xE7;
    pub const FLUSH_CACHE_EXT: u8 = 0xEA;
    /// A command that moves nothing and changes nothing: what a driver asks
    /// a disk to see that the command's end interrupts it.
    pub const CHECK_POWER_MODE: u8 = 0xE5;
    /// The power is about to go: an HDD parks its heads and spins down, an
    /// SSD puts its tables away -- what Linux sends a disk at power-off, so
    /// that the cut is no emergency to it.
    pub const STANDBY_IMMEDIATE: u8 = 0xE0;
}

/// How many commands in a row may not finish -- time out, or lose the link
/// under them -- before a driver gives the disk up: every one of them is a
/// command's whole timeout, and a disk that answers nothing would otherwise
/// make every request of it wait that long, for as long as anybody asks.
pub const MAX_UNFINISHED: u32 = 3;

/// The NCQ Command Error log (ATA8-ACS, A.13): READ LOG EXT of this page,
/// one sector, is what a disk that failed a queued command answers with the
/// command's tag -- and what takes the disk out of the state a failed queued
/// command leaves it in, aborting every other queued command it had.
pub const LOG_NCQ_ERROR: u8 = 0x10;
pub const LOG_PAGE_BYTES: usize = 512;

/// Byte 0 of the log: the tag, and whether the error was a command's that
/// was not queued after all (NQ), when no tag is the one.
const NCQ_LOG_TAG_MASK: u8 = 0x1F;
const NCQ_LOG_NQ: u8 = 1 << 7;

/// What the NCQ Command Error log says of the command that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcqError {
    /// The failed command's tag -- meaningless when `non_queued`.
    pub tag: u8,
    /// The error was a command's that was not a queued one.
    pub non_queued: bool,
    pub status: u8,
    pub error: u8,
    /// The address the command was at.
    pub lba: u64,
}

/// The log page held to its own checksum -- its 512 bytes sum to 0 -- and
/// read; None for one that is short, or does not sum, which names no command
/// that can be believed.
pub fn ncq_error(page: &[u8]) -> Option<NcqError> {
    let page = page.get(..LOG_PAGE_BYTES)?;
    if page.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) != 0 {
        return None;
    }
    let lba = [page[4], page[5], page[6], page[8], page[9], page[10]]
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, &b)| acc | u64::from(b) << (8 * i));
    Some(NcqError {
        tag: page[0] & NCQ_LOG_TAG_MASK,
        non_queued: page[0] & NCQ_LOG_NQ != 0,
        status: page[2],
        error: page[3],
        lba,
    })
}
