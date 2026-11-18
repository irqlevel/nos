//! The ATA command set as the tree's two ATA disk drivers speak it --
//! `drivers/block/ata`, through a PCI IDE controller's taskfile, and
//! `drivers/block/ahci`, through an AHCI port's FISes: what IDENTIFY DEVICE
//! says ([`identify`]), and the commands' numbers ([`cmd`]). Written once,
//! here, so that the two cannot read a disk differently.

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
    pub const WRITE_SECTORS: u8 = 0x30;
    pub const WRITE_SECTORS_EXT: u8 = 0x34;
    pub const WRITE_DMA_EXT: u8 = 0x35;
    pub const READ_DMA: u8 = 0xC8;
    pub const WRITE_DMA: u8 = 0xCA;
    pub const FLUSH_CACHE: u8 = 0xE7;
    pub const FLUSH_CACHE_EXT: u8 = 0xEA;
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
