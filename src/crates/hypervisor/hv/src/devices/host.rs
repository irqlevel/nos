//! A function of the host's on the guest's bus (plans/gpu-passthrough.md):
//! a real device the guest's own driver drives, its DMA through an IOMMU
//! domain of the guest's RAM (the module's, `kcore::iommu`), its BARs mapped
//! into the guest's nested table straight.
//!
//! Its configuration space is the real one's, read through, with what has to
//! stay the host's laid over it:
//!
//! - the BARs are the guest's: what it writes is where they are in its
//!   physical address space, and sizing reads back the real sizes -- the
//!   real registers, which hold the host's addresses, are never written;
//! - the command register's bus-master bit is the guest's and goes to the
//!   device; its memory and I/O bits are the guest's alone: the device
//!   decodes its memory for as long as the guest holds it, and no I/O, so
//!   that no access of the guest's through its mapped BARs ever finds it
//!   deaf -- an Unsupported Request, which a server may escalate to a
//!   machine check (Linux's VFIO blocks it so, CVE-2020-12888); its INTx
//!   disable stays the host's (set: the device's interrupt is its MSI, the
//!   host's to take);
//! - there is no expansion ROM, no INTx pin, and the interrupt line is the
//!   guest's to write;
//! - the MSI capability is emulated: what the guest writes is what the
//!   host's handler of the device's real MSI sends it (`Msi`), and the real
//!   capability is the host's (`kcore::msix`);
//! - power management cannot take the device out of D0, the PCIe
//!   capability's control registers cannot start an FLR, change the payload
//!   sizes or the error reporting, or touch the link, nor the Advanced
//!   Features capability start an FLR: a reset is the host's to make and to
//!   undo; and phantom functions and no snoop are kept off -- the first
//!   would have the device's requests carry another function's id, which
//!   the IOMMU translates by another context (the audio function's beside
//!   a GPU, passed through: the whole of the host's memory), the second
//!   have its DMA read past what the CPUs' caches hold newer;
//! - a bridge is not given at all;
//! - every other register is the device's: written through, as QEMU's VFIO
//!   does -- a vendor's registers are the driver's to know.
//!
//! NVIDIA's GPUs mirror the configuration space in BAR0, at 0x88000 (and at
//! 0x1800 on old ones): that page is trapped rather than mapped, and each
//! access of it is one of the configuration space above -- otherwise the
//! guest would write the real BARs and MSI past every rule here. A write
//! within the MSI capability's first two bytes -- its id and next pointer,
//! read-only -- is also passed to the real mirror: a GeForce driver
//! acknowledges its MSI so, and without it the GPU sends one interrupt and
//! no more (QEMU's quirk, `vfio_nvidia_quirk_mirror_write`). Within them
//! only: a write that reached the control word past them would be the
//! guest's to the host's own MSI.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};

/// The real function, as the module reaches it: its configuration space
/// (offsets under 256) and its BAR0, for the mirror's MSI acknowledgement.
pub trait Real: Send {
    fn read32(&self, offset: u16) -> u32;
    fn write32(&self, offset: u16, value: u32);
    fn write16(&self, offset: u16, value: u16);
    fn write8(&self, offset: u16, value: u8);
    /// A write of `size` bytes of `value` at `offset` in BAR0.
    fn bar0_write(&self, offset: u32, size: u8, value: u32);
}

/// A BAR of the real function, as the module read it with the device's
/// decoding off: where the host has it, how big it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub host: u64,
    pub size: u64,
    /// A 64-bit memory BAR: it takes the next register too.
    pub wide: bool,
    pub prefetchable: bool,
}

/// What the guest wrote of the MSI capability, for the host's handler of
/// the device's real MSI to send it: read there with interrupts off, so in
/// atomics -- never under the platform's lock.
pub struct Msi {
    pub control: AtomicU16,
    pub address: AtomicU64,
    pub data: AtomicU32,
    pub mask: AtomicU32,
    /// One came while MSI was off or the vector masked: sent once it is on
    /// and unmasked, as the device would have held it pending.
    pub pending: AtomicU32,
}

impl Msi {
    fn new() -> Msi {
        Msi {
            control: AtomicU16::new(0),
            address: AtomicU64::new(0),
            data: AtomicU32::new(0),
            mask: AtomicU32::new(0),
            pending: AtomicU32::new(0),
        }
    }

    /// The message to send now: None while MSI is off or the vector masked.
    pub fn message(&self) -> Option<(u64, u32)> {
        if self.control.load(Ordering::Acquire) & MSI_ENABLE == 0 || self.mask.load(Ordering::Acquire) & 1 != 0 {
            return None;
        }
        Some((self.address.load(Ordering::Acquire), self.data.load(Ordering::Acquire)))
    }

    /// The device's interrupt, as it comes: the message to send, or None and
    /// held pending.
    pub fn raise(&self) -> Option<(u64, u32)> {
        let m = self.message();
        if m.is_none() {
            self.pending.store(1, Ordering::Release);
            /* Turned on or unmasked since the look: sent after all, by the
             * one who sees both */
            return self.take_pending();
        }
        m
    }

    /// One held pending, now that it can go: its message, the mark cleared.
    pub fn take_pending(&self) -> Option<(u64, u32)> {
        let m = self.message()?;
        (self.pending.swap(0, Ordering::AcqRel) != 0).then_some(m)
    }
}

/// A BAR the guest moved: what the caller unmaps and maps again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    pub bar: usize,
    /// Where it was, 0 where it was nowhere yet.
    pub from: u64,
    pub to: u64,
}

/* The header's registers (PCI Local Bus 3.0, 6.2) */
const COMMAND: usize = 0x04;
const STATUS: usize = 0x06;
const HEADER_TYPE: usize = 0x0E;
const BAR0: usize = 0x10;
const BAR_END: usize = 0x28;
const ROM: usize = 0x30;
const CAPABILITIES: usize = 0x34;
/// The interrupt line's dword: the line the guest's, the pin -- the next
/// byte -- none, the last two the device's
const INTERRUPT_LINE: usize = 0x3C;
const CONFIG_SIZE: usize = 256;
/// The header's first byte past what it defines: capabilities from here.
const HEADER_END: usize = 0x40;

/// The command register's decoding and bus mastering: the guest's view of
/// all three, of which the device is told bus mastering alone.
const COMMAND_IO: u16 = 1 << 0;
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_MASTER: u16 = 1 << 2;
const COMMAND_GUEST: u16 = COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER;
/// INTx disable: the host's, set.
const COMMAND_INTX_DISABLE: u16 = 1 << 10;
/// A header type's multi-function bit: one function here; and its layout,
/// 0 an endpoint's -- a bridge's is not given.
const HEADER_MULTI: u8 = 0x80;
const HEADER_LAYOUT: u32 = 0x7F;
/// A memory BAR's low bits: its 64-bit type and prefetchable.
const BAR_64: u32 = 0x4;
const BAR_PREFETCH: u32 = 0x8;

/* Capabilities (PCI 3.0, 6.7; PCIe 5.0, 7.5.3) */
const CAP_PM: u8 = 0x01;
const CAP_MSI: u8 = 0x05;
const CAP_PCIE: u8 = 0x10;
const CAP_MSIX: u8 = 0x11;
const CAP_AF: u8 = 0x13;
/// The Advanced Features capability's control and status: its FLR.
const AF_CONTROL: usize = 4;
/// The longest walk of the list: 48 capabilities of 4 bytes past the header.
const CAP_WALK_MAX: usize = 48;

/// MSI's control word: enable, the messages enabled (6:4, kept at one), and
/// what the device can do (64-bit, per-vector masking) -- read-only.
const MSI_ENABLE: u16 = 1 << 0;
const MSI_GUEST_CONTROL: u16 = MSI_ENABLE;
const MSI_64: u16 = 1 << 7;
const MSI_MASKABLE: u16 = 1 << 8;

/// Power management's control and status register, and its power state.
const PM_CONTROL: usize = 4;
const PM_STATE_MASK: u32 = 0x3;

/// The PCIe capability's device control register: what of it is the
/// host's, kept as the host has it -- error reporting (3:0), the payload
/// size (7:5), the read request size (14:12) -- and what is kept off:
/// phantom functions (9), no snoop (11), and FLR (15), the host's to start.
/// And the link control register, the host's whole.
const PCIE_DEVICE_CONTROL: usize = 0x08;
const PCIE_DEVCTL_HOST: u16 = 0xF | (0x7 << 5) | (0x7 << 12);
const PCIE_DEVCTL_OFF: u16 = (1 << 9) | (1 << 11) | (1 << 15);
const PCIE_LINK_CONTROL: usize = 0x10;

/// NVIDIA, and where its GPUs mirror their configuration space in BAR0.
pub const VENDOR_NVIDIA: u16 = 0x10DE;
pub const NVIDIA_MIRROR: u32 = 0x88000;
pub const NVIDIA_MIRROR_OLD: u32 = 0x1800;
/// The mirror's span trapped: a page, of which the configuration space is
/// the first 256 bytes.
pub const MIRROR_BYTES: u32 = 0x1000;

/// One function of the host's, as the guest sees it.
pub struct HostFunction {
    real: Box<dyn Real>,
    vendor: u16,
    /// The real BARs, by register: a 64-bit one at its first register.
    bars: [Option<Bar>; 6],
    /// The guest's BAR registers.
    guest_bars: [u32; 6],
    command: u16,
    line: u8,
    msi_cap: usize,
    pm_cap: usize,
    pcie_cap: usize,
    af_cap: usize,
    /// The capability MSI-X has, hidden: a function with MSI-X has it
    /// emulated before it is given (not yet: such a function is refused).
    msi: Arc<Msi>,
}

impl HostFunction {
    /// The function `real`, its BARs `bars` as the module read them: None
    /// where it is a bridge, has MSI-X -- whose table would be the guest's to
    /// program unless trapped, which this does not do yet -- no MSI to give
    /// the guest, or a capability list this cannot be sure of (above). Given, the device decodes its memory from here on,
    /// no I/O, and masters nothing until the guest has it do so; phantom
    /// functions and no snoop off.
    pub fn new(real: Box<dyn Real>, bars: [Option<Bar>; 6]) -> Option<HostFunction> {
        if (real.read32(HEADER_TYPE as u16 & !3) >> 16) & HEADER_LAYOUT != 0 {
            return None;
        }
        let id = real.read32(0);
        let mut f = HostFunction {
            real,
            vendor: (id & 0xFFFF) as u16,
            bars,
            guest_bars: [0; 6],
            command: 0,
            line: 0,
            msi_cap: 0,
            pm_cap: 0,
            pcie_cap: 0,
            af_cap: 0,
            msi: Arc::new(Msi::new()),
        };
        /* The capability list, whole: every register the host keeps is in a
         * capability it found, and a guest writes at any offset it likes --
         * so a list that loops, runs on past the walk or points into the
         * header, or a second capability of a kind the host keeps registers
         * of (a second PCIe capability's FLR, a second MSI's address), is a
         * function not given */
        let mut at = (f.real.read32(CAPABILITIES as u16) & 0xFC) as usize;
        let mut walked = 0;
        while at != 0 {
            walked += 1;
            if walked > CAP_WALK_MAX || at < HEADER_END || at > CONFIG_SIZE - 4 {
                return None;
            }
            let head = f.real.read32(at as u16);
            let found = match (head & 0xFF) as u8 {
                CAP_MSI => &mut f.msi_cap,
                CAP_PM => &mut f.pm_cap,
                CAP_PCIE => &mut f.pcie_cap,
                CAP_AF => &mut f.af_cap,
                CAP_MSIX => return None,
                _ => {
                    at = ((head >> 8) & 0xFC) as usize;
                    continue;
                }
            };
            if *found != 0 {
                return None;
            }
            *found = at;
            at = ((head >> 8) & 0xFC) as usize;
        }
        if f.msi_cap == 0 {
            return None;
        }
        /* Each BAR the guest's at nowhere until it, or the caller placing
         * it, writes one: the type bits as the real one's */
        for i in 0..6 {
            if let Some(b) = f.bars[i] {
                f.guest_bars[i] = bar_type(&b);
            }
        }
        f.real_command();
        if f.pcie_cap != 0 {
            let at = (f.pcie_cap + PCIE_DEVICE_CONTROL) as u16;
            let real = (f.real.read32(at) & 0xFFFF) as u16;
            f.real.write16(at, real & !PCIE_DEVCTL_OFF);
        }
        Some(f)
    }

    /// The real command register as it is while the guest holds the
    /// device: memory decoded, no I/O, bus mastering as the guest has it,
    /// INTx off; the rest -- SERR, parity -- the host's.
    fn real_command(&self) {
        let real = (self.real.read32(COMMAND as u16) & 0xFFFF) as u16;
        let next = (real & !COMMAND_GUEST) | COMMAND_MEMORY | (self.command & COMMAND_MASTER) | COMMAND_INTX_DISABLE;
        self.real.write16(COMMAND as u16, next);
    }

    /// Where the host's handler of the device's MSI finds what to send.
    pub fn msi(&self) -> Arc<Msi> {
        self.msi.clone()
    }

    /// Whether this is an NVIDIA function, whose BAR0 mirrors its
    /// configuration space and is trapped there.
    pub fn nvidia(&self) -> bool {
        self.vendor == VENDOR_NVIDIA
    }

    /// The function's BARs: the real one of each register, None for none.
    pub fn bars(&self) -> &[Option<Bar>; 6] {
        &self.bars
    }

    /// BAR `bar`'s guest address, as the guest or the caller last put it.
    pub fn guest_address(&self, bar: usize) -> u64 {
        let Some(b) = self.bars.get(bar).copied().flatten() else { return 0 };
        let low = u64::from(self.guest_bars[bar] & !0xF);
        if b.wide && bar + 1 < 6 {
            low | (u64::from(self.guest_bars[bar + 1]) << 32)
        } else {
            low
        }
    }

    /// BAR `bar` put at `gpa` before the guest starts -- what firmware
    /// would have done: its size aligned, the type bits kept.
    pub fn place(&mut self, bar: usize, gpa: u64) -> bool {
        let Some(b) = self.bars.get(bar).copied().flatten() else { return false };
        if b.size == 0 || gpa % b.size != 0 || (!b.wide && gpa > u64::from(u32::MAX)) {
            return false;
        }
        self.guest_bars[bar] = (gpa as u32 & !0xF) | bar_type(&b);
        if b.wide && bar + 1 < 6 {
            self.guest_bars[bar + 1] = (gpa >> 32) as u32;
        }
        true
    }

    /// A read of `size` bytes (1, 2 or 4, aligned to it) at `at`; all ones
    /// for any other.
    pub fn config_read(&self, at: usize, size: usize) -> u32 {
        if !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > CONFIG_SIZE {
            return size_mask(size);
        }
        let dword = at & !3;
        let shift = 8 * (at - dword) as u32;
        (self.read_dword(dword) >> shift) & size_mask(size)
    }

    /// A write of `size` bytes (1, 2 or 4, aligned to it) of `value` at
    /// `at`: what the guest moved of its BARs, for the caller to map anew.
    pub fn config_write(&mut self, at: usize, size: usize, value: u32) -> Option<Moved> {
        if !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > CONFIG_SIZE {
            return None;
        }
        let dword = at & !3;
        let shift = 8 * (at - dword) as u32;
        let mask = size_mask(size) << shift;
        let value = (value << shift) & mask;

        match dword {
            0x00 | 0x08 | 0x28 | 0x2C | 0x34 | 0x38 => None,
            0x04 => {
                if mask & 0xFFFF != 0 {
                    let wanted = self.command & !(mask as u16) | (value as u16);
                    self.command = wanted & COMMAND_GUEST;
                    self.real_command();
                }
                if mask & 0xFFFF_0000 != 0 {
                    /* The status register's write-one-to-clear bits: the
                     * device's, cleared as asked */
                    self.real.write16(STATUS as u16, (value >> 16) as u16);
                }
                None
            }
            0x0C => None,
            d if (BAR0..BAR_END).contains(&d) => self.write_bar((d - BAR0) / 4, mask, value),
            ROM => None,
            INTERRUPT_LINE => {
                if mask & 0xFF != 0 {
                    self.line = value as u8;
                }
                None
            }
            d if self.msi_cap != 0 && (self.msi_cap..self.msi_cap + msi_len(self)).contains(&d) => {
                self.write_msi(d - self.msi_cap, mask, value);
                None
            }
            d if self.pm_cap != 0 && d == self.pm_cap + PM_CONTROL => {
                /* Not out of D0: what is written of the power state is
                 * dropped, the rest -- PME's -- written */
                let real = self.real.read32(d as u16);
                let keep = mask & !PM_STATE_MASK;
                self.real.write32(d as u16, (real & !keep) | (value & keep));
                None
            }
            d if self.pcie_cap != 0 && d == self.pcie_cap + PCIE_DEVICE_CONTROL => {
                if mask & 0xFFFF != 0 {
                    let real = (self.real.read32(d as u16) & 0xFFFF) as u16;
                    let writable = mask as u16 & !(PCIE_DEVCTL_HOST | PCIE_DEVCTL_OFF);
                    let next = ((real & !writable) | (value as u16 & writable)) & !PCIE_DEVCTL_OFF;
                    self.real.write16(d as u16, next);
                }
                if mask & 0xFFFF_0000 != 0 {
                    /* Device status: write-one-to-clear */
                    self.real.write16((d + 2) as u16, (value >> 16) as u16);
                }
                None
            }
            d if self.pcie_cap != 0 && d == self.pcie_cap + PCIE_LINK_CONTROL => None,
            /* Advanced Features' control: its one bit is FLR's */
            d if self.af_cap != 0 && d == self.af_cap + AF_CONTROL => None,
            d => {
                /* A register of the device's own: written through, a byte, a
                 * word or a dword as the guest wrote it */
                match size {
                    1 => self.real.write8(at as u16, (value >> shift) as u8),
                    2 => self.real.write16(at as u16, (value >> shift) as u16),
                    _ => self.real.write32(d as u16, value),
                }
                None
            }
        }
    }

    /// An access of the trapped mirror page, `offset` bytes into it, of 1, 2
    /// or 4 bytes -- the caller splits a wider one: a configuration space
    /// access of the same bytes, one within the MSI capability's first two
    /// also written to the real mirror (see the module's comment). What a
    /// read reads: all ones for an access of another size, zeroes past the
    /// configuration space -- the extended space's, which the guest is not
    /// given -- where writes are dropped.
    pub fn mirror(&mut self, mirror: u32, offset: u32, size: usize, write: bool, value: u32) -> (u32, Option<Moved>) {
        let at = offset as usize;
        if !matches!(size, 1 | 2 | 4) {
            return (size_mask(size), None);
        }
        if at + size > CONFIG_SIZE {
            return (0, None);
        }
        if !write {
            return (self.config_read(at, size), None);
        }
        if self.msi_cap != 0 && at >= self.msi_cap && at + size <= self.msi_cap + 2 {
            self.real.bar0_write(mirror + offset, size as u8, value);
        }
        (0, self.config_write(at, size, value))
    }

    /// The dword at `dword`, the guest's view of it.
    fn read_dword(&self, dword: usize) -> u32 {
        match dword {
            0x04 => {
                let real = self.real.read32(COMMAND as u16);
                (real & 0xFFFF_0000) | u32::from(self.command)
            }
            0x0C => {
                let real = self.real.read32(dword as u16);
                real & !(u32::from(HEADER_MULTI) << (8 * (HEADER_TYPE - 0x0C)))
            }
            d if (BAR0..BAR_END).contains(&d) => self.guest_bars[(d - BAR0) / 4],
            ROM => 0,
            INTERRUPT_LINE => {
                let real = self.real.read32(dword as u16);
                (real & 0xFFFF_0000) | u32::from(self.line)
            }
            d if self.msi_cap != 0 && (self.msi_cap..self.msi_cap + msi_len(self)).contains(&d) => {
                self.read_msi(d - self.msi_cap)
            }
            d => self.real.read32(d as u16),
        }
    }

    fn write_bar(&mut self, bar: usize, mask: u32, value: u32) -> Option<Moved> {
        /* The register a 64-bit BAR's high half is in belongs to the BAR
         * before it */
        let (first, high) = match (self.bars.get(bar).copied().flatten(), bar.checked_sub(1)) {
            (Some(_), _) => (bar, false),
            (None, Some(prev)) if self.bars[prev].is_some_and(|b| b.wide) => (prev, true),
            _ => return None,
        };
        let b = self.bars[first]?;
        let before = self.guest_address(first);
        let reg = if high { first + 1 } else { first };
        let merged = (self.guest_bars[reg] & !mask) | (value & mask);
        if high {
            /* The high half of the size's mask: all ones where the size is
             * under 4 GiB */
            let size_high = !((b.size - 1) >> 32) as u32;
            self.guest_bars[reg] = merged & size_high;
        } else {
            let size_low = !((b.size - 1) as u32) & !0xF;
            self.guest_bars[reg] = (merged & size_low) | bar_type(&b);
        }
        let after = self.guest_address(first);
        (after != before).then_some(Moved { bar: first, from: before, to: after })
    }

    fn read_msi(&self, off: usize) -> u32 {
        let real = self.real.read32(self.msi_cap as u16);
        let caps = ((real >> 16) as u16) & (MSI_64 | MSI_MASKABLE);
        let control = (self.msi.control.load(Ordering::Acquire) & MSI_GUEST_CONTROL) | caps;
        let wide = caps & MSI_64 != 0;
        let address = self.msi.address.load(Ordering::Acquire);
        match off {
            0x0 => (real & 0xFFFF) | (u32::from(control) << 16),
            0x4 => address as u32,
            0x8 if wide => (address >> 32) as u32,
            0x8 | 0xC if off == data_offset(wide) => self.msi.data.load(Ordering::Acquire) & 0xFFFF,
            o if caps & MSI_MASKABLE != 0 && o == data_offset(wide) + 4 => self.msi.mask.load(Ordering::Acquire),
            _ => 0,
        }
    }

    fn write_msi(&mut self, off: usize, mask: u32, value: u32) {
        let real = self.real.read32(self.msi_cap as u16);
        let caps = ((real >> 16) as u16) & (MSI_64 | MSI_MASKABLE);
        let wide = caps & MSI_64 != 0;
        let address = self.msi.address.load(Ordering::Acquire);
        let merge = |old: u32| (old & !mask) | (value & mask);
        match off {
            0x0 => {
                let old = u32::from(self.msi.control.load(Ordering::Acquire)) << 16;
                let control = ((merge(old) >> 16) as u16) & MSI_GUEST_CONTROL;
                self.msi.control.store(control, Ordering::Release);
            }
            0x4 => {
                let low = u64::from(merge(address as u32) & !0x3);
                self.msi.address.store((address & !0xFFFF_FFFF) | low, Ordering::Release);
            }
            0x8 if wide => {
                let high = u64::from(merge((address >> 32) as u32));
                self.msi.address.store((address & 0xFFFF_FFFF) | (high << 32), Ordering::Release);
            }
            o if o == data_offset(wide) => {
                let data = merge(self.msi.data.load(Ordering::Acquire)) & 0xFFFF;
                self.msi.data.store(data, Ordering::Release);
            }
            o if caps & MSI_MASKABLE != 0 && o == data_offset(wide) + 4 => {
                let m = merge(self.msi.mask.load(Ordering::Acquire)) & 1;
                self.msi.mask.store(m, Ordering::Release);
            }
            _ => {}
        }
    }

}

/// A BAR's type bits as its register has them: memory always (the module
/// gives no I/O BAR), 64-bit or not, prefetchable or not.
fn bar_type(b: &Bar) -> u32 {
    (if b.wide { BAR_64 } else { 0 }) | (if b.prefetchable { BAR_PREFETCH } else { 0 })
}

/// Where the MSI capability's data register is: after a 64-bit address, or
/// a 32-bit one.
fn data_offset(wide: bool) -> usize {
    if wide { 0xC } else { 0x8 }
}

/// The bytes of the MSI capability, as long as the real one's.
fn msi_len(f: &HostFunction) -> usize {
    let real = f.real.read32(f.msi_cap as u16);
    let caps = ((real >> 16) as u16) & (MSI_64 | MSI_MASKABLE);
    let base = if caps & MSI_64 != 0 { 0x10 } else { 0xC };
    if caps & MSI_MASKABLE != 0 { base + 8 } else { base }
}

fn size_mask(size: usize) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => u32::MAX,
    }
}
