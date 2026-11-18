//! A host's PCI function given to a guest (plans/gpu-passthrough.md): found,
//! claimed against a second guest, its BARs read with its decoding off, reset
//! where it can be (FLR); moved to an IOMMU domain of the guest's RAM, so
//! that its DMA reaches the guest's memory and nothing else; its MSI the
//! host's, each one sent on to the guest as the guest programmed it
//! (`hv::host::Msi`). What the guest sees of it is `hv::host::HostFunction`'s.
//!
//! The order a guest lets go of one is the order it is safe in (`Drop`):
//! the route its interrupt takes to the guest first, then the interrupt and
//! the host's MSI, then the device's bus mastering and the device itself --
//! reset, so that it holds none of the guest's addresses -- then the domain,
//! which blocks it and lets go of the guest's pages, then the claim. The
//! device is left blocked, in no domain: never passed through again while it
//! may remember a guest.
//!
//! A device's functions are not isolated from each other -- no ACS between
//! them is assumed, which is why Linux makes a GPU and its audio function one
//! IOMMU group -- so the other functions of a device a guest holds one of are
//! blocked for as long as it does, each in a domain of nothing, and claimed:
//! unless the guest holds them too. A device one of whose other functions is
//! the host's own -- mastering, a driver's -- or another guest's is refused.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;

use hv::host::{Bar, HostFunction, Msi, Real};
use hv::run::LinuxGuest;
use kcore::dma::PhysMapping;
use kcore::io::MmioRegion;
use kcore::iommu::{self, Domain};
use kcore::msix::{MsixInterrupt, MsixTable};
use kcore::pci::{self, PciDevice};
use kcore::sync::IrqSpinLock;

/// The most functions held at once, by every guest together -- a route slot
/// each -- the other functions of their devices, blocked, among them.
const MAX_PASSED: usize = 16;

/// Where each held function's interrupt goes: its guest and what it wrote
/// of the MSI capability. Static, as an interrupt's target has to be; a
/// slot is filled once its guest is in an `Arc` (`Passed::arm`) and emptied
/// before the guest is let go of, by the task that holds it -- so the last
/// share of a guest is never dropped by an interrupt.
struct Route {
    guest: Arc<LinuxGuest>,
    msi: Arc<Msi>,
}

static ROUTES: [IrqSpinLock<Option<Route>>; MAX_PASSED] = [const { IrqSpinLock::new(None) }; MAX_PASSED];

/// Which slots are held, and by which requester id: one guest a function.
static CLAIMS: IrqSpinLock<[Option<u16>; MAX_PASSED]> = IrqSpinLock::new([None; MAX_PASSED]);

/// The device's MSI, in interrupt context: sent to its guest where the
/// guest has MSI on and the vector unmasked, else held pending for when it
/// has (`Msi::raise`).
fn on_msi(slot: &'static IrqSpinLock<Option<Route>>) {
    let route = slot.lock();
    if let Some(r) = route.as_ref() {
        if let Some((address, data)) = r.msi.raise() {
            r.guest.deliver_msi(address, data);
        }
    }
}

/* Configuration space (PCI 3.0, 6.2; PCIe 5.0, 7.5.3) */
const COMMAND: u16 = 0x04;
const COMMAND_IO: u16 = 1 << 0;
const COMMAND_MEMORY: u16 = 1 << 1;
const COMMAND_MASTER: u16 = 1 << 2;
const BAR0: u16 = 0x10;
const BARS: u8 = 6;
const BAR_IO: u32 = 1;
const BAR_TYPE_MASK: u32 = 0x6;
const BAR_TYPE_64: u32 = 0x4;
const CAP_PCIE: u8 = 0x10;
const PCIE_DEVCAP: u16 = 0x04;
const PCIE_DEVCAP_FLR: u32 = 1 << 28;
const PCIE_DEVCTL: u16 = 0x08;
const PCIE_DEVCTL_FLR: u16 = 1 << 15;
const PCIE_DEVSTA: u16 = 0x0A;
const PCIE_DEVSTA_TRPND: u16 = 1 << 5;
const PCIE_LNKCTL: u16 = 0x10;
const PCIE_DEVCTL2: u16 = 0x28;
/// The capability's version: 2 and later have DevCtl2 (PCIe 5.0, 7.5.3.2).
const PCIE_FLAGS: u16 = 0x02;
const PCIE_FLAGS_VERSION: u16 = 0xF;
const VENDOR_NONE: u16 = 0xFFFF;
/// The header type's register, and its bit for a device of more than one
/// function; how many functions a device has at most.
const HEADER_TYPE: u16 = 0x0E;
const HEADER_MULTI: u8 = 0x80;
const FUNCTIONS: u16 = 8;
/// What a reset leaves to be written back: the header's dwords from the
/// command register to the interrupt line's.
const SAVED: core::ops::Range<u16> = 0x04..0x40;
/// How long an FLR takes at most (PCIe 5.0, 6.6.2: 100 ms), and how long a
/// device has to answer after it before it is taken for gone.
const FLR_MS: u64 = 100;
const ANSWER_MS: u64 = 1000;
const ANSWER_STEP_MS: u64 = 10;

/// The real function, as `hv::host::HostFunction` reaches it.
struct Device {
    dev: PciDevice,
    /// The page of BAR0 NVIDIA's mirror is in, mapped, for the MSI
    /// acknowledgement passed through it: its offset in BAR0, and the page.
    mirror: Option<(u32, MmioRegion, PhysMapping)>,
}

impl Real for Device {
    fn read32(&self, offset: u16) -> u32 {
        self.dev.read_config32(offset)
    }
    fn write32(&self, offset: u16, value: u32) {
        self.dev.write_config32(offset, value)
    }
    fn write16(&self, offset: u16, value: u16) {
        self.dev.write_config16(offset, value)
    }
    fn write8(&self, offset: u16, value: u8) {
        self.dev.write_config8(offset, value)
    }
    fn bar0_write(&self, offset: u32, size: u8, value: u32) {
        let Some((base, region, _)) = &self.mirror else { return };
        let Some(at) = offset.checked_sub(*base).map(|a| a as usize) else { return };
        match size {
            1 => region.write8(at, value as u8),
            2 => region.write16(at, value as u16),
            _ => region.write32(at, value),
        }
    }
}

/// A function held, by its slot in `CLAIMS` -- and `ROUTES` -- let go of
/// when dropped: no path out of a guest's start forgets one.
struct Claim(usize);

impl Claim {
    fn take(sid: u16) -> Option<Claim> {
        let mut claims = CLAIMS.lock();
        if claims.iter().any(|c| *c == Some(sid)) {
            return None;
        }
        let slot = claims.iter().position(|c| c.is_none())?;
        claims[slot] = Some(sid);
        Some(Claim(slot))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(c) = CLAIMS.lock().get_mut(self.0) {
            *c = None;
        }
    }
}

/// Another function of the device, blocked while the guest holds its
/// sibling: its domain maps nothing, and it is left blocked when that goes.
struct Sibling {
    _domain: Domain,
    _claim: Claim,
}

/// A host's function a guest holds. Its fields go after `drop`'s body in
/// this order: the device's siblings left blocked, then its claim.
pub struct Passed {
    sid: u16,
    dev: PciDevice,
    function: Option<HostFunction>,
    domain: Option<Domain>,
    table: Option<MsixTable>,
    interrupt: Option<MsixInterrupt>,
    msi: Arc<Msi>,
    _siblings: alloc::vec::Vec<Sibling>,
    claim: Claim,
}

impl Passed {
    /// The function at `bus`:`slot`.`func` taken for a guest that holds
    /// `all`: claimed, its device's other functions blocked but those in
    /// `all` (above), its BARs read, reset, its MSI the host's. Refused,
    /// said, where DMA is not translated, the function is not there or is
    /// another guest's, a function beside it is the host's or another
    /// guest's, it has no MSI (or has MSI-X, not yet given), or no vector is
    /// to be had.
    pub fn open(bus: u16, slot: u16, func: u16, all: &[(u16, u16, u16)]) -> Result<Passed, String> {
        let name = alloc::format!("{:02x}:{:02x}.{:x}", bus, slot, func);
        if !iommu::on() {
            return Err(alloc::format!("{}: DMA is not translated -- boot with iommu=on", name));
        }
        let dev = pci::get_device_by_bdf(bus, slot, func).ok_or_else(|| alloc::format!("{}: no such function", name))?;
        let sid = iommu::sid(bus, slot, func);
        let claim = Claim::take(sid).ok_or_else(|| alloc::format!("{}: another guest's, or too many held", name))?;
        let siblings = block_siblings(bus, slot, func, all).map_err(|e| alloc::format!("{}: {}", name, e))?;

        /* Quiet while it is read: decoding and bus mastering off */
        let command = dev.read_config16(COMMAND);
        dev.write_config16(COMMAND, command & !(COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER));
        let bars = read_bars(&dev, &name);
        if !reset(&dev) {
            return Err(alloc::format!("{}: did not answer after its reset", name));
        }

        let mirror = match bars[0] {
            Some(b0) if dev.vendor == hv::host::VENDOR_NVIDIA
                && u64::from(hv::host::NVIDIA_MIRROR + hv::host::MIRROR_BYTES) <= b0.size =>
            {
                let at = b0.host + u64::from(hv::host::NVIDIA_MIRROR);
                PhysMapping::map(at, 1).map(|m| {
                    let region = MmioRegion::new(m.as_mut_ptr(), m.len());
                    (hv::host::NVIDIA_MIRROR, region, m)
                })
            }
            _ => None,
        };
        let real = Box::new(Device { dev, mirror });
        let Some(function) = HostFunction::new(real, bars) else {
            return Err(alloc::format!("{}: a bridge, no MSI to give, MSI-X -- not given yet -- or a capability \
                                       list not whole", name));
        };
        let msi = function.msi();

        let table = MsixTable::new_msi(&dev);
        let interrupt = table.as_ref().and_then(|t| MsixInterrupt::register_for(t, 0, &ROUTES[claim.0], on_msi));
        if interrupt.is_none() {
            return Err(alloc::format!("{}: its MSI could not be taken by the host", name));
        }
        /* Said every time: until interrupt remapping is done, a DMA write
         * of the device's to the interrupt range is an interrupt of the
         * host's, of any vector -- the guest is trusted with that */
        kcore::klog!(0, "hv: {} given to a guest -- its DMA reaches the guest's RAM alone, but its interrupts are not \
                         remapped yet: a guest that has it write to 0xFEE00000 raises any interrupt of the host's \
                         (plans/gpu-passthrough.md)", name);
        Ok(Passed { sid, dev, function: Some(function), domain: None, table, interrupt, msi, _siblings: siblings, claim })
    }

    /// The function's DMA moved to a domain of `guest`'s RAM -- every region
    /// of it at its own guest address -- before the guest's devices are
    /// placed: refused, said, where the IOMMU will not.
    pub fn translate(&mut self, guest: &LinuxGuest) -> Result<(), String> {
        let mut domain = Domain::new(self.sid).map_err(|e| alloc::format!("no IOMMU domain: {:?}", e))?;
        for (base, run) in guest.memory().ram() {
            domain.map(base, run).map_err(|e| alloc::format!("the guest's RAM in its domain: {:?}", e))?;
        }
        domain.attach(self.sid).map_err(|e| alloc::format!("the function into its domain: {:?}", e))?;
        self.domain = Some(domain);
        Ok(())
    }

    /// What the guest is given of it, once: `hv::run::LinuxGuest::add_host_function`'s.
    pub fn function(&mut self) -> Option<HostFunction> {
        self.function.take()
    }

    /// Its interrupt sent on to `guest` from now on.
    pub fn arm(&self, guest: &Arc<LinuxGuest>) {
        *ROUTES[self.claim.0].lock() = Some(Route { guest: guest.clone(), msi: self.msi.clone() });
    }
}

impl Drop for Passed {
    fn drop(&mut self) {
        /* The route first, taken out under its lock -- an interrupt in flight
         * is done with it -- and dropped here, in a task */
        let route = ROUTES[self.claim.0].lock().take();
        drop(route);
        self.interrupt = None;
        self.table = None;
        let command = self.dev.read_config16(COMMAND);
        self.dev.write_config16(COMMAND, command & !(COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER));
        let _ = reset(&self.dev);
        /* Blocked, out of the domain, the guest's pages let go of */
        self.domain = None;
    }
}

/// The device at `bus`:`slot`'s functions but `func` and the others the
/// guest holds (`all`), each blocked and claimed -- once a device, by the
/// lowest function of it the guest holds. Refused where one is mastering,
/// a driver of the host's at work on it, or claimed already, another
/// guest's: its DMA would not be kept from the one the guest is given.
fn block_siblings(bus: u16, slot: u16, func: u16, all: &[(u16, u16, u16)]) -> Result<alloc::vec::Vec<Sibling>, String> {
    let mut blocked = alloc::vec::Vec::new();
    let held = |f: u16| all.contains(&(bus, slot, f));
    if (0..func).any(|f| held(f)) {
        return Ok(blocked);
    }
    let multi = pci::get_device_by_bdf(bus, slot, 0).is_some_and(|d| d.read_config8(HEADER_TYPE) & HEADER_MULTI != 0);
    if !multi {
        return Ok(blocked);
    }
    for f in (0..FUNCTIONS).filter(|&f| f != func && !held(f)) {
        let Some(dev) = pci::get_device_by_bdf(bus, slot, f) else { continue };
        let name = alloc::format!("{:02x}:{:02x}.{:x}, beside it,", bus, slot, f);
        if dev.read_config16(COMMAND) & COMMAND_MASTER != 0 {
            return Err(alloc::format!("{} is the host's: its DMA would not be kept from the guest's", name));
        }
        let sid = iommu::sid(bus, slot, f);
        let claim = Claim::take(sid).ok_or_else(|| alloc::format!("{} is another guest's, or too many are held", name))?;
        blocked.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        let mut domain = Domain::new(sid).map_err(|e| alloc::format!("{} has no IOMMU domain: {:?}", name, e))?;
        domain.attach(sid).map_err(|e| alloc::format!("{} could not be blocked: {:?}", name, e))?;
        blocked.push(Sibling { _domain: domain, _claim: claim });
    }
    Ok(blocked)
}

/// Its memory BARs, as the host has them -- with the device's decoding off,
/// which the caller saw to: each register's, a 64-bit one at its first. Each
/// given said, and each memory BAR that is not, with what its registers read:
/// a BAR that went missing in silence was an RTX A2000's 8 GiB BAR1, whose low
/// register has no address bit to read back.
fn read_bars(dev: &PciDevice, name: &str) -> [Option<Bar>; 6] {
    let mut bars = [None; 6];
    let mut i = 0u8;
    while i < BARS {
        let reg = BAR0 + u16::from(i) * 4;
        let low = dev.read_config32(reg);
        let wide = low & BAR_IO == 0 && low & BAR_TYPE_MASK == BAR_TYPE_64 && i + 1 < BARS;
        if low & BAR_IO != 0 {
            i += 1;
            continue;
        }
        let high = if wide { dev.read_config32(reg + 4) } else { 0 };
        dev.write_config32(reg, u32::MAX);
        let mask_low = dev.read_config32(reg);
        dev.write_config32(reg, low);
        let mask_high = wide.then(|| {
            dev.write_config32(reg + 4, u32::MAX);
            let mask = dev.read_config32(reg + 4);
            dev.write_config32(reg + 4, high);
            mask
        });
        let host = dev.bar_address(i);
        bars[usize::from(i)] = hv::host::decode_bar(low, mask_low, mask_high, host);
        match bars[usize::from(i)] {
            Some(b) => kcore::klog!(0, "hv: {}: BAR{} of {:#x} bytes at {:#x}{}{}", name, i, b.size, b.host,
                                    if b.wide { ", 64-bit" } else { "" }, if b.prefetchable { ", prefetchable" } else { "" }),
            None if mask_low != 0 || mask_high.is_some_and(|m| m != 0) => {
                kcore::klog!(0, "hv: {}: BAR{} not given -- {:#x}, all ones read back as {:#x} {:?}, at {:#x}: not \
                                 assigned, not aligned, or a size no BAR is", name, i, low, mask_low, mask_high, host);
            }
            None => {}
        }
        i += if wide { 2 } else { 1 };
    }
    bars
}

/// A Function Level Reset where the function has one, as Linux makes it
/// (`pcie_flr`, `pci_save_state`): its transactions given a second to
/// finish, its header and the PCIe capability's controls saved before and
/// written back after, the device asked for its vendor id until it answers
/// -- a second past the 100 ms it has. False, said, and nothing more done,
/// for a device that does not answer. Nothing for a function without FLR
/// -- its bridge's secondary bus reset is not done yet.
fn reset(dev: &PciDevice) -> bool {
    let Some(cap) = dev.find_capability(CAP_PCIE) else { return true };
    let cap = u16::from(cap);
    if dev.read_config32(cap + PCIE_DEVCAP) & PCIE_DEVCAP_FLR == 0 {
        return true;
    }
    let mut waited = 0;
    while dev.read_config16(cap + PCIE_DEVSTA) & PCIE_DEVSTA_TRPND != 0 && waited < ANSWER_MS {
        kcore::task::sleep_ms(ANSWER_STEP_MS);
        waited += ANSWER_STEP_MS;
    }
    let mut saved = [0u32; ((SAVED.end - SAVED.start) / 4) as usize];
    for (k, reg) in SAVED.step_by(4).enumerate() {
        saved[k] = dev.read_config32(reg);
    }
    let control = dev.read_config16(cap + PCIE_DEVCTL);
    let link = dev.read_config16(cap + PCIE_LNKCTL);
    let second = (dev.read_config16(cap + PCIE_FLAGS) & PCIE_FLAGS_VERSION) >= 2;
    let control2 = if second { dev.read_config16(cap + PCIE_DEVCTL2) } else { 0 };

    dev.write_config16(cap + PCIE_DEVCTL, control | PCIE_DEVCTL_FLR);
    kcore::task::sleep_ms(FLR_MS);
    let mut waited = 0;
    while dev.read_config16(0) == VENDOR_NONE && waited < ANSWER_MS {
        kcore::task::sleep_ms(ANSWER_STEP_MS);
        waited += ANSWER_STEP_MS;
    }
    if dev.read_config16(0) == VENDOR_NONE {
        kcore::klog!(0, "hv: {:02x}:{:02x}.{:x} did not answer after its reset", dev.bus, dev.slot, dev.func);
        return false;
    }
    /* The capability's controls, then the BARs and the rest of the header,
     * the command register last */
    dev.write_config16(cap + PCIE_DEVCTL, control & !PCIE_DEVCTL_FLR);
    dev.write_config16(cap + PCIE_LNKCTL, link);
    if second {
        dev.write_config16(cap + PCIE_DEVCTL2, control2);
    }
    for (k, reg) in SAVED.step_by(4).enumerate().skip(1) {
        dev.write_config32(reg, saved[k]);
    }
    dev.write_config32(SAVED.start, saved[0]);
    true
}
