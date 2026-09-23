//! PCI configuration space, by the PC's mechanism #1 -- an address written
//! to port 0xCF8, the data at 0xCFC..0xCFF -- for bus 0 alone: a host bridge
//! at 00:00.0, which is what tells Linux the mechanism works, and a function
//! for each device after it.
//!
//! Every device here is reached through port I/O (its BAR 0 is an I/O
//! range), so nothing a guest does on the bus needs an instruction decoded:
//! an `in` or an `out` stops it with the port, the size and the value. A
//! function's configuration is 256 bytes of plain data, with the few fields
//! a guest may write -- the command register, BAR 0, the interrupt line --
//! written through their masks, and everything else read-only.

use alloc::vec::Vec;

use crate::{Error, Result};

pub const CONFIG_ADDRESS: u16 = 0xCF8;
pub const CONFIG_DATA: u16 = 0xCFC;
/// The last port of the pair: CONFIG_DATA is four bytes wide.
const CONFIG_END: u16 = 0xCFF;

/// CONFIG_ADDRESS: the enable bit, and where bus, device, function and the
/// register's dword are.
const ADDRESS_ENABLE: u32 = 1 << 31;
const ADDRESS_BUS_SHIFT: u32 = 16;
const ADDRESS_DEVICE_SHIFT: u32 = 11;
const ADDRESS_FUNCTION_SHIFT: u32 = 8;
const ADDRESS_REGISTER_MASK: u32 = 0xFC;
const ADDRESS_BUS_MASK: u32 = 0xFF;
const ADDRESS_DEVICE_MASK: u32 = 0x1F;
const ADDRESS_FUNCTION_MASK: u32 = 0x7;

/// Offsets in a type 0 header.
const VENDOR_ID: usize = 0x00;
const DEVICE_ID: usize = 0x02;
const COMMAND: usize = 0x04;
const REVISION: usize = 0x08;
const CLASS_PROG_IF: usize = 0x09;
const HEADER_TYPE: usize = 0x0E;
const BAR0: usize = 0x10;
const SUBSYSTEM_VENDOR: usize = 0x2C;
const SUBSYSTEM_ID: usize = 0x2E;
const INTERRUPT_LINE: usize = 0x3C;
const INTERRUPT_PIN: usize = 0x3D;

/// The command register's I/O space enable: a function answers at its I/O
/// BAR only with it set.
const COMMAND_IO: u16 = 1 << 0;
/// What of the command register a guest may set: I/O, memory, bus master.
const COMMAND_WRITABLE: u16 = 0x0007;
/// BAR 0's bit 0: an I/O range.
const BAR_IO: u32 = 1;
/// Interrupt pin A.
const PIN_INTA: u8 = 1;

/// The most functions on the bus, the host bridge's slot included.
pub const MAX_SLOTS: usize = 8;
/// The size of a function's configuration space.
const CONFIG_SIZE: usize = 256;

/// One function's configuration space.
pub struct Function {
    config: [u8; CONFIG_SIZE],
    /// BAR 0's size in ports, a power of two, or 0 for no BAR.
    bar_size: u32,
}

/// What a device function is, for its header.
pub struct Identity {
    pub vendor: u16,
    pub device: u16,
    pub revision: u8,
    /// Class, subclass, programming interface.
    pub class: u32,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
}

impl Function {
    fn blank(id: &Identity) -> Function {
        let mut f = Function { config: [0; CONFIG_SIZE], bar_size: 0 };
        f.put16(VENDOR_ID, id.vendor);
        f.put16(DEVICE_ID, id.device);
        f.config[REVISION] = id.revision;
        let class = id.class.to_le_bytes();
        f.config[CLASS_PROG_IF..CLASS_PROG_IF + 3].copy_from_slice(&class[..3]);
        f.config[HEADER_TYPE] = 0;
        f.put16(SUBSYSTEM_VENDOR, id.subsystem_vendor);
        f.put16(SUBSYSTEM_ID, id.subsystem);
        f
    }

    /// A device: its identity, an I/O BAR of `bar_size` ports (a power of
    /// two) placed at `io_base` as a BIOS would have, and INTA on `irq`.
    pub fn device(id: &Identity, io_base: u16, bar_size: u32, irq: u8) -> Function {
        let mut f = Function::blank(id);
        f.bar_size = bar_size;
        f.put32(BAR0, (u32::from(io_base) & !(bar_size - 1)) | BAR_IO);
        f.config[INTERRUPT_LINE] = irq;
        f.config[INTERRUPT_PIN] = PIN_INTA;
        f
    }

    fn get16(&self, at: usize) -> u16 {
        u16::from_le_bytes([self.config[at], self.config[at + 1]])
    }

    fn put16(&mut self, at: usize, v: u16) {
        self.config[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }

    fn get32(&self, at: usize) -> u32 {
        u32::from_le_bytes([self.config[at], self.config[at + 1], self.config[at + 2], self.config[at + 3]])
    }

    fn put32(&mut self, at: usize, v: u32) {
        self.config[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// Where its I/O BAR is, when I/O decoding is on.
    fn io_range(&self) -> Option<(u16, u32)> {
        if self.bar_size == 0 || self.get16(COMMAND) & COMMAND_IO == 0 {
            return None;
        }
        let base = self.get32(BAR0) & !(BAR_IO | 0x2);
        u16::try_from(base).ok().map(|b| (b, self.bar_size))
    }

    /// `size` bytes (1, 2 or 4) of configuration from `at`, which the caller
    /// keeps within the space and aligned to `size`.
    fn read(&self, at: usize, size: usize) -> u32 {
        let mut v = 0u32;
        for i in 0..size {
            v |= u32::from(self.config[at + i]) << (8 * i);
        }
        v
    }

    /// A write of `size` bytes at `at`, byte by byte through what each byte
    /// of the header lets a guest change.
    fn write(&mut self, at: usize, size: usize, value: u32) {
        for i in 0..size {
            let byte = (value >> (8 * i)) as u8;
            let off = at + i;
            match off {
                o if (COMMAND..COMMAND + 2).contains(&o) => {
                    let mask = COMMAND_WRITABLE.to_le_bytes()[o - COMMAND];
                    self.config[o] = (self.config[o] & !mask) | (byte & mask);
                }
                o if (BAR0..BAR0 + 4).contains(&o) && self.bar_size != 0 => {
                    /* The low bits of a BAR are its size: what is written
                     * there reads back as zero, which is how a guest sizes
                     * it -- all ones written, the mask read back -- and bit 0
                     * is always the I/O indicator. */
                    let mask = (!(self.bar_size - 1) & !0x3).to_le_bytes()[o - BAR0];
                    let fixed = BAR_IO.to_le_bytes()[o - BAR0];
                    self.config[o] = (byte & mask) | fixed;
                }
                INTERRUPT_LINE => self.config[off] = byte,
                _ => {}
            }
        }
    }
}

/// Bus 0.
pub struct PciBus {
    address: u32,
    slots: Vec<Function>,
}

impl PciBus {
    /// The bus with its host bridge, an Intel 440FX as far as its header
    /// says -- a class of host bridge, and a vendor Linux's sanity check of
    /// the mechanism knows.
    pub fn new() -> Result<PciBus> {
        let mut slots = Vec::new();
        slots.try_reserve_exact(MAX_SLOTS).map_err(|_| Error::NoMemory)?;
        slots.push(Function::blank(&Identity {
            vendor: 0x8086,
            device: 0x1237,
            revision: 2,
            class: 0x06_00_00,
            subsystem_vendor: 0,
            subsystem: 0,
        }));
        Ok(PciBus { address: 0, slots })
    }

    /// Put a function in the next slot, and say which.
    pub fn add(&mut self, f: Function) -> Result<u8> {
        if self.slots.len() >= MAX_SLOTS {
            return Err(Error::NoMemory);
        }
        self.slots.push(f);
        Ok((self.slots.len() - 1) as u8)
    }

    pub fn owns(port: u16) -> bool {
        (CONFIG_ADDRESS..=CONFIG_END).contains(&port)
    }

    /// The function CONFIG_ADDRESS names, and the byte in its space `port`
    /// is at -- or None, for an access with the enable bit clear, another
    /// bus, a function past 0 or a slot with nothing in it, which read as
    /// all ones.
    fn target(&self, port: u16) -> Option<(usize, usize)> {
        let a = self.address;
        if a & ADDRESS_ENABLE == 0 || (a >> ADDRESS_BUS_SHIFT) & ADDRESS_BUS_MASK != 0 {
            return None;
        }
        if (a >> ADDRESS_FUNCTION_SHIFT) & ADDRESS_FUNCTION_MASK != 0 {
            return None;
        }
        let slot = ((a >> ADDRESS_DEVICE_SHIFT) & ADDRESS_DEVICE_MASK) as usize;
        if slot >= self.slots.len() {
            return None;
        }
        let at = (a & ADDRESS_REGISTER_MASK) as usize + usize::from(port - CONFIG_DATA);
        Some((slot, at))
    }

    /// A read of `size` bytes (1, 2 or 4) at `port`.
    pub fn read(&self, port: u16, size: u8) -> u32 {
        let size = usize::from(size);
        if port < CONFIG_DATA {
            /* CONFIG_ADDRESS answers a dword access at its port and nothing
             * else: a byte at 0xCF9 is the chipset's reset control, and one
             * at 0xCFB mechanism #2's probe -- neither emulated. */
            return if port == CONFIG_ADDRESS && size == 4 { self.address } else { size_mask(size) };
        }
        match self.target(port) {
            /* Within the dword, and aligned: anything else floats. */
            Some((slot, at)) if at % size == 0 && at + size <= CONFIG_SIZE => self.slots[slot].read(at, size),
            _ => size_mask(size),
        }
    }

    /// A write of `size` bytes (1, 2 or 4) at `port`.
    pub fn write(&mut self, port: u16, size: u8, value: u32) {
        let size = usize::from(size);
        if port < CONFIG_DATA {
            if port == CONFIG_ADDRESS && size == 4 {
                self.address = value;
            }
            return;
        }
        if let Some((slot, at)) = self.target(port) {
            if at % size == 0 && at + size <= CONFIG_SIZE {
                self.slots[slot].write(at, size, value);
            }
        }
    }

    /// The slot whose I/O BAR `port` falls in, and the offset in it.
    pub fn io_target(&self, port: u16) -> Option<(u8, u16)> {
        for (slot, f) in self.slots.iter().enumerate().skip(1) {
            if let Some((base, size)) = f.io_range() {
                let offset = u32::from(port).wrapping_sub(u32::from(base));
                if u32::from(port) >= u32::from(base) && offset < size {
                    return Some((slot as u8, offset as u16));
                }
            }
        }
        None
    }
}

/// All ones in `size` bytes.
fn size_mask(size: usize) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => 0xFFFF_FFFF,
    }
}
