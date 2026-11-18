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
}
