//! PCI bus 0, by configuration mechanism #1 -- a function's address at
//! 0xCF8, its register at 0xCFC -- which is all of hv's bus: the guest's
//! virtio disks are found on it.

use alloc::vec::Vec;

use crate::io::{inl, outl, outw};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;
const ENABLE: u32 = 1 << 31;
const SLOTS: u8 = 32;

const ID: u8 = 0x00;
const COMMAND: u8 = 0x04;
const BAR0: u8 = 0x10;
const COMMAND_IO: u16 = 1 << 0;
const COMMAND_BUS_MASTER: u16 = 1 << 2;
const BAR_IO: u32 = 1 << 0;
const BAR_IO_MASK: u32 = !0x3;

/// virtio's vendor, and its legacy block device's ID (0x1000 + type 2 - 1).
const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_LEGACY_BLOCK: u16 = 0x1001;

fn address(slot: u8, offset: u8) -> u32 {
    ENABLE | (u32::from(slot) << 11) | u32::from(offset & 0xFC)
}

fn read32(slot: u8, offset: u8) -> u32 {
    outl(CONFIG_ADDRESS, address(slot, offset));
    inl(CONFIG_DATA)
}

fn write16(slot: u8, offset: u8, v: u16) {
    outl(CONFIG_ADDRESS, address(slot, offset));
    outw(CONFIG_DATA + u16::from(offset & 2), v);
}

/// A legacy virtio block device: the slot it is in, and its registers' I/O
/// base.
pub struct VirtioDisk {
    pub slot: u8,
    pub io_base: u16,
}

/// The legacy virtio disks on bus 0, in slot order -- `vda` first, as the
/// guest's kernel will name them -- each with its I/O space and bus mastering
/// on.
pub fn virtio_disks() -> Vec<VirtioDisk> {
    let mut disks = Vec::new();
    for slot in 0..SLOTS {
        let id = read32(slot, ID);
        if id as u16 != VIRTIO_VENDOR || (id >> 16) as u16 != VIRTIO_LEGACY_BLOCK {
            continue;
        }
        let bar0 = read32(slot, BAR0);
        let io = bar0 & BAR_IO_MASK;
        if bar0 & BAR_IO == 0 || io == 0 || io > u32::from(u16::MAX) {
            say!("PCI slot {}: a virtio disk without an I/O BAR ({:#x}), passed over", slot, bar0);
            continue;
        }
        let command = read32(slot, COMMAND) as u16;
        write16(slot, COMMAND, command | COMMAND_IO | COMMAND_BUS_MASTER);
        disks.push(VirtioDisk { slot, io_base: io as u16 });
    }
    disks
}
