//! A host's PCI function given to a guest (plans/gpu-passthrough.md): found,
//! claimed against a second guest, its BARs read with its decoding off, reset
//! where it can be (FLR); moved to an IOMMU domain of the guest's RAM, so
//! that its DMA reaches the guest's memory and nothing else; its interrupts
//! the host's -- MSI-X's entries given, a vector of the host's each, or its
//! MSI -- each one sent on to the guest as the guest programmed it
//! (`hv::host::Messages`). What the guest sees of it is
//! `hv::host::HostFunction`'s.
//!
//! The order a guest lets go of one is the order it is safe in (`Drop`):
//! the route its interrupts take to the guest first, then the interrupts and
//! the host's table, then the device's bus mastering and the device itself
//! -- reset, so that it holds none of the guest's addresses -- then the
//! domain, which blocks it and lets go of the guest's pages, then the claim.
//! The device is left blocked, in no domain: never passed through again
//! while it may remember a guest.
//!
//! A device's functions are not isolated from each other -- no ACS between
//! them is assumed, which is why Linux makes a GPU and its audio function one
//! IOMMU group -- so the other functions of a device a guest holds one of are
//! blocked for as long as it does, each in a domain of nothing, and claimed:
//! unless the guest holds them too. A function a driver of the host's has
//! taken is refused, and so is a device one of whose other functions is the
//! host's own -- a driver's, or mastering -- or another guest's: `guestpci=`
//! keeps the host's drivers off a function at boot.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use hv::host::{Bar, HostFunction, Messages, Real, MAX_VECTORS};
use hv::run::{HostAddress, LinuxGuest};
use kcore::dma::PhysMapping;
use kcore::io::MmioRegion;
use kcore::iommu::{self, Domain};
use kcore::msix::{MsixInterrupt, MsixTable};
use kcore::pci::{self, PciDevice, ScreenHold};
use kcore::sync::IrqSpinLock;

use crate::groups::{self, Kind};

/// The most functions held at once, by every guest together -- a route slot
/// each -- the other functions of their devices, blocked, among them.
const MAX_PASSED: usize = 16;

/// Where each held function's interrupts go: its guest and what it wrote
/// of its MSI or MSI-X. Static, as an interrupt's target has to be; a slot
/// is filled once its guest is in an `Arc` (`Passed::arm`) and emptied
/// before the guest is let go of, by the task that holds it -- so the last
/// share of a guest is never dropped by an interrupt.
struct Route {
    guest: Arc<LinuxGuest>,
    messages: Arc<Messages>,
}

static ROUTES: [IrqSpinLock<Option<Route>>; MAX_PASSED] = [const { IrqSpinLock::new(None) }; MAX_PASSED];

/// Which slots are held, and by which requester id: one guest a function.
static CLAIMS: IrqSpinLock<[Option<u16>; MAX_PASSED]> = IrqSpinLock::new([None; MAX_PASSED]);

/// What the handler of one of the device's vectors is given: its function's
/// slot, and which of its messages the vector is.
#[derive(Clone, Copy)]
struct Vector {
    slot: u8,
    index: u8,
}

const fn vectors() -> [[Vector; MAX_VECTORS]; MAX_PASSED] {
    let mut all = [[Vector { slot: 0, index: 0 }; MAX_VECTORS]; MAX_PASSED];
    let mut slot = 0;
    while slot < MAX_PASSED {
        let mut index = 0;
        while index < MAX_VECTORS {
            all[slot][index] = Vector { slot: slot as u8, index: index as u8 };
            index += 1;
        }
        slot += 1;
    }
    all
}

static VECTORS: [[Vector; MAX_VECTORS]; MAX_PASSED] = vectors();

/// One of the device's interrupts, in interrupt context: sent to its guest
/// where the guest has that message on and unmasked, else held pending for
/// when it has (`Messages::raise`).
fn on_vector(v: &'static Vector) {
    let Some(slot) = ROUTES.get(usize::from(v.slot)) else { return };
    let route = slot.lock();
    if let Some(r) = route.as_ref() {
        if let Some((address, data)) = r.messages.raise(usize::from(v.index)) {
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
/// How long a reset takes at most before a function is asked anything (PCIe
/// 5.0, 6.6.2: 100 ms), how long it has to answer after that before it is
/// taken for gone, and how long a bus reset is held (Linux's 2 ms).
const RESET_MS: u64 = 100;
const ANSWER_MS: u64 = 1000;
const ANSWER_STEP_MS: u64 = 10;
const BUS_RESET_MS: u64 = 2;
/// A bridge's header (PCI-to-PCI Bridge 1.2, 3.2): its layout's number, its
/// secondary bus, and its bridge control's Secondary Bus Reset.
const HEADER_LAYOUT: u8 = 0x7F;
const HEADER_BRIDGE: u8 = 0x01;
const SECONDARY_BUS: u16 = 0x19;
const BRIDGE_CONTROL: u16 = 0x3E;
const BRIDGE_BUS_RESET: u16 = 1 << 6;
/// The devices a bus has at most.
const SLOTS: u16 = 32;
/// The expansion ROM's BAR (PCI 3.0, 6.2.5.2): its register, enable bit and
/// address bits.
const ROM_REGISTER: u16 = 0x30;
const ROM_ENABLE: u32 = 1;
const ROM_ADDRESS: u32 = 0xFFFF_F800;
/// Extended capabilities (PCIe 5.0, 7.8.6 and 7.7.7): the Resizable BAR --
/// its first control register, which says how many BARs it has (7:5), and
/// the bytes from one BAR's registers to the next -- and ACS's control.
const EXT_REBAR: u16 = 0x0015;
const REBAR_CONTROL: u16 = 0x08;
const REBAR_COUNT_SHIFT: u32 = 5;
const REBAR_COUNT_MASK: u32 = 0x7;
const REBAR_ENTRY_BYTES: u16 = 8;
const REBARS_MAX: usize = 6;
const EXT_ACS: u16 = 0x000D;
const ACS_CAPABILITY: u16 = 0x04;
const ACS_CONTROL: u16 = 0x06;
/// The PCIe capability's device or port type (its flags' bits 7:4), by what
/// isolation makes of it (`groups::Kind`): endpoints, a legacy one, an
/// integrated one, a switch's upstream port; root and downstream ports.
const PCIE_TYPE_SHIFT: u16 = 4;
const PCIE_TYPE_MASK: u16 = 0xF;
const PCIE_ENDPOINTS: [u16; 4] = [0x0, 0x1, 0x9, 0x5];
const PCIE_PORTS: [u16; 2] = [0x4, 0x6];
const SUBORDINATE_BUS: u16 = 0x1A;
/// A VGA controller, and the legacy window one decodes where the firmware
/// routed it to the boot GPU -- the text buffer at 0xB8000 among it, which
/// the kernel's console writes when GRUB handed it no framebuffer (Hertz).
const CLASS_DISPLAY: u8 = 0x03;
const SUBCLASS_VGA: u8 = 0x00;
const VGA_WINDOW: u64 = 0xA0000;
const VGA_WINDOW_BYTES: u64 = 0x20000;

/// The real function, as `hv::host::HostFunction` reaches it: its
/// configuration space, and the pages of its BARs that trap -- NVIDIA's
/// mirror, MSI-X's table and PBA -- mapped for what of them is passed
/// through.
struct Device {
    dev: PciDevice,
    bars: [Option<Bar>; 6],
    /// Each page mapped: its BAR, its offset in it, the mapping.
    pages: Vec<(usize, u64, MmioRegion, PhysMapping)>,
}

impl Device {
    /// The mapping of the page `offset` bytes into BAR `bar`, and where in
    /// it an access of `size` bytes there is -- None where the page is not
    /// mapped, or the access is not aligned to its size or runs past it.
    fn page(&self, bar: usize, offset: u64, size: usize) -> Option<(&MmioRegion, usize)> {
        let page_size = kcore::consts::PAGE_SIZE as u64;
        let base = offset & !(page_size - 1);
        let at = (offset - base) as usize;
        if !matches!(size, 1 | 2 | 4 | 8) || at % size != 0 || at + size > page_size as usize {
            return None;
        }
        let (_, _, region, _) = self.pages.iter().find(|p| p.0 == bar && p.1 == base)?;
        Some((region, at))
    }
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
    fn map_page(&mut self, bar: usize, offset: u64) -> bool {
        let page_size = kcore::consts::PAGE_SIZE as u64;
        let Some(b) = self.bars.get(bar).copied().flatten() else { return false };
        if offset % page_size != 0 || offset >= b.size || self.pages.try_reserve(1).is_err() {
            return false;
        }
        let Some(m) = PhysMapping::map(b.host + offset, 1) else { return false };
        let region = MmioRegion::new(m.as_mut_ptr(), m.len());
        self.pages.push((bar, offset, region, m));
        true
    }
    fn page_read(&self, bar: usize, offset: u64, size: usize) -> u64 {
        let Some((region, at)) = self.page(bar, offset, size) else { return u64::MAX };
        match size {
            1 => u64::from(region.read8(at)),
            2 => u64::from(region.read16(at)),
            4 => u64::from(region.read32(at)),
            _ => region.read64(at),
        }
    }
    fn page_write(&self, bar: usize, offset: u64, size: usize, value: u64) {
        let Some((region, at)) = self.page(bar, offset, size) else { return };
        match size {
            1 => region.write8(at, value as u8),
            2 => region.write16(at, value as u16),
            4 => region.write32(at, value as u32),
            _ => region.write64(at, value),
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

/// Another function of its IOMMU group, blocked while the guest holds one
/// (`block_group`): its domain maps nothing, and it is left blocked when that
/// goes.
struct Sibling {
    _domain: Domain,
    _claim: Claim,
}

/// A host's function a guest holds. Its fields go after `drop`'s body in
/// this order: its IOMMU group's other functions left blocked, then its claim.
pub struct Passed {
    sid: u16,
    dev: PciDevice,
    function: Option<HostFunction>,
    domain: Option<Domain>,
    /// The host's side of the device's interrupts: its MSI-X table -- or MSI,
    /// a table of one -- the mapping of its BAR the table is reached through
    /// (MSI-X's), and a vector of the host's for each message given.
    table: Option<MsixTable>,
    _table_map: Option<PhysMapping>,
    interrupts: Vec<MsixInterrupt>,
    messages: Arc<Messages>,
    /// How it is reset as the guest lets go of it, as it was as it was given.
    reset: ResetPlan,
    _blocked: Vec<Sibling>,
    claim: Claim,
}

impl Passed {
    /// The function at `bus`:`slot`.`func` taken for a guest that holds
    /// `all`: claimed, its device's other functions blocked but those in
    /// `all` (above), its BARs read, reset, its interrupts the host's.
    /// Refused, said, where DMA is not translated, the function is not there,
    /// is another guest's or a driver of the host's has it, a function beside
    /// it is the host's or another guest's, it has neither MSI-X nor MSI to
    /// give, or its vectors are not to be had.
    pub fn open(bus: u16, slot: u16, func: u16, all: &[(u16, u16, u16)]) -> Result<Passed, String> {
        let name = alloc::format!("{:02x}:{:02x}.{:x}", bus, slot, func);
        if !iommu::on() {
            return Err(alloc::format!("{}: DMA is not translated -- boot with iommu=on", name));
        }
        let dev = pci::get_device_by_bdf(bus, slot, func).ok_or_else(|| alloc::format!("{}: no such function", name))?;
        if dev.driven() {
            return Err(alloc::format!("{}: a driver of the host's has it -- boot with guestpci={} to keep the \
                                       host's drivers off it", name, name));
        }
        let sid = iommu::sid(bus, slot, func);
        if let Some((base, limit)) = iommu::reserved(sid) {
            return Err(alloc::format!("{}: the firmware reserved memory for its DMA ({:#x}-{:#x}, a DMAR RMRR): it \
                                       keeps reaching the host's memory there, and is no guest's", name, base, limit));
        }
        let claim = Claim::take(sid).ok_or_else(|| alloc::format!("{}: another guest's, or too many held", name))?;
        let blocked = block_group(bus, slot, func, all).map_err(|e| alloc::format!("{}: {}", name, e))?;

        /* Quiet while it is read: decoding and bus mastering off -- and the
         * screen held meanwhile: the boot GPU's memory may be the console's,
         * the firmware's framebuffer in one of its BARs or, for a VGA
         * controller, the text buffer in its legacy window, and neither
         * answers while it is off. Where it is, it is let go of for good: the
         * reset ends that picture anyway, and the memory is the guest's */
        let hold = ScreenHold::new();
        let command = dev.read_config16(COMMAND);
        dev.write_config16(COMMAND, command & !(COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER));
        let bars = read_bars(&dev, &name);
        let screen = (0..bars.len())
            .find(|&i| bars[i].is_some_and(|b| pci::release_screen(b.host, b.size)))
            .map(|i| alloc::format!("BAR{} holds the screen's framebuffer", i))
            .or_else(|| {
                (dev.class == CLASS_DISPLAY && dev.subclass == SUBCLASS_VGA
                    && pci::release_screen(VGA_WINDOW, VGA_WINDOW_BYTES))
                    .then(|| String::from("a VGA controller, whose legacy window holds the screen's text"))
            });
        drop(hold);
        if let Some(why) = screen {
            kcore::klog!(0, "hv: {}: {} -- let go of: nothing is drawn there from now on", name, why);
        }
        let reset = ResetPlan::of(bus, slot, func, all);
        if !reset.make(&dev) {
            return Err(alloc::format!("{}: did not answer after its reset", name));
        }

        let rom = read_rom(&dev, &name);
        let real = Box::new(Device { dev, bars, pages: Vec::new() });
        let Some(mut function) = HostFunction::new(real, bars) else {
            return Err(alloc::format!("{}: a bridge, neither MSI-X with its table in a BAR given nor MSI, a \
                                       capability list not whole, or a page that traps not mapped", name));
        };
        if let Some(image) = rom {
            let kib = image.len() / 1024;
            if function.set_rom(image) {
                kcore::klog!(0, "hv: {}: its ROM of {} KiB given, a copy", name, kib);
            }
        }
        let messages = function.messages();
        let (table, table_map, interrupts) = match function.msix_layout() {
            Some(l) => {
                /* The real table, the host's to program: through a mapping of
                 * its BAR from its start to the table's end, so that the
                 * kernel does not size the BAR anew while it decodes */
                let page = kcore::consts::PAGE_SIZE as u64;
                let b = bars[l.table_bar].ok_or_else(|| alloc::format!("{}: MSI-X's BAR not given", name))?;
                let pages = (l.table + l.table_bytes()).div_ceil(page) as usize;
                let map = PhysMapping::map(b.host, pages)
                    .ok_or_else(|| alloc::format!("{}: MSI-X's table could not be mapped", name))?;
                let mut mapped = [0u64; 6];
                mapped[l.table_bar] = map.as_mut_ptr() as u64;
                let table = MsixTable::new_with_bars(&dev, &mapped)
                    .ok_or_else(|| alloc::format!("{}: its MSI-X could not be taken by the host", name))?;
                let interrupts = register(&table, &claim, messages.count(), &name)?;
                (table, Some(map), interrupts)
            }
            None => {
                let table = MsixTable::new_msi(&dev)
                    .ok_or_else(|| alloc::format!("{}: its MSI could not be taken by the host", name))?;
                let interrupts = register(&table, &claim, 1, &name)?;
                (table, None, interrupts)
            }
        };
        /* Said every time. Remapped, a DMA write of the device's to the
         * interrupt range raises its own vectors alone; not, any vector of
         * the host's -- the guest is trusted with that (unsafe_interrupts) */
        let kind = if messages.msix() {
            alloc::format!("MSI-X, {} of its vectors", messages.count())
        } else {
            String::from("MSI")
        };
        if iommu::remapping() {
            kcore::klog!(0, "hv: {} given to a guest -- its DMA reaches the guest's RAM alone, its interrupts \
                             ({}) remapped: what it sends raises its own vectors and no other", name, kind);
        } else {
            kcore::klog!(0, "hv: {} given to a guest -- its DMA reaches the guest's RAM alone, but its interrupts \
                             ({}) are not remapped: a guest that has it write to 0xFEE00000 raises any interrupt \
                             of the host's (plans/gpu-passthrough.md)", name, kind);
        }
        Ok(Passed {
            sid,
            dev,
            function: Some(function),
            domain: None,
            table: Some(table),
            _table_map: table_map,
            interrupts,
            messages,
            reset,
            _blocked: blocked,
            claim,
        })
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

    /// Its bus, device and function on the host: which functions are one
    /// device, behind one port in the guest.
    pub fn address(&self) -> HostAddress {
        HostAddress { bus: (self.sid >> 8) as u8, device: ((self.sid >> 3) & 0x1F) as u8, function: (self.sid & 0x7) as u8 }
    }

    /// Its interrupts sent on to `guest` from now on.
    pub fn arm(&self, guest: &Arc<LinuxGuest>) {
        *ROUTES[self.claim.0].lock() = Some(Route { guest: guest.clone(), messages: self.messages.clone() });
    }
}

/// A vector of the host's for each of the device's first `count` messages,
/// its handler `on_vector` with the function's slot and the message's index.
fn register(table: &MsixTable, claim: &Claim, count: usize, name: &str) -> Result<Vec<MsixInterrupt>, String> {
    let mut interrupts = Vec::new();
    interrupts.try_reserve_exact(count).map_err(|_| String::from("out of memory"))?;
    for (index, target) in VECTORS[claim.0].iter().enumerate().take(count) {
        let interrupt = MsixInterrupt::register_for(table, index as u16, target, on_vector).ok_or_else(|| {
            alloc::format!("{}: no vector of the host's for its message {} -- the kernel's interrupt slots are \
                            taken", name, index)
        })?;
        interrupts.push(interrupt);
    }
    Ok(interrupts)
}

impl Drop for Passed {
    fn drop(&mut self) {
        /* The route first, taken out under its lock -- an interrupt in flight
         * is done with it -- and dropped here, in a task */
        let route = ROUTES[self.claim.0].lock().take();
        drop(route);
        self.interrupts.clear();
        self.table = None;
        let command = self.dev.read_config16(COMMAND);
        self.dev.write_config16(COMMAND, command & !(COMMAND_IO | COMMAND_MEMORY | COMMAND_MASTER));
        let _ = self.reset.make(&self.dev);
        /* Blocked, out of the domain, the guest's pages let go of */
        self.domain = None;
    }
}

/// What isolation makes of the host's bus (`groups`): every function the
/// PCI scan found -- its kind, by its PCIe capability's type, whether its
/// device has others, its ACS, the buses below a bridge -- and the index of
/// `bus`:`slot`.`func` among them.
fn topology(bus: u16, slot: u16, func: u16) -> Result<(Vec<groups::Function>, usize), String> {
    let mut all = Vec::new();
    all.try_reserve_exact(pci::device_count()).map_err(|_| String::from("out of memory"))?;
    let mut me = None;
    for d in (0..pci::device_count()).filter_map(pci::get_any) {
        let (Ok(b), Ok(s), Ok(f)) = (u8::try_from(d.bus), u8::try_from(d.slot), u8::try_from(d.func)) else { continue };
        let kind = match d.find_capability(CAP_PCIE) {
            Some(at) => {
                let t = (d.read_config16(u16::from(at) + PCIE_FLAGS) >> PCIE_TYPE_SHIFT) & PCIE_TYPE_MASK;
                if PCIE_PORTS.contains(&t) {
                    Kind::Port
                } else if PCIE_ENDPOINTS.contains(&t) {
                    Kind::Endpoint
                } else {
                    Kind::Conventional
                }
            }
            None => Kind::Conventional,
        };
        let multi = pci::get_device_by_bdf(d.bus, d.slot, 0)
            .is_some_and(|f0| f0.read_config8(HEADER_TYPE) & HEADER_MULTI != 0);
        let acs = d.find_extended_capability(EXT_ACS)
            .map(|at| (d.read_config16(at + ACS_CAPABILITY), d.read_config16(at + ACS_CONTROL)));
        let below = (d.read_config8(HEADER_TYPE) & HEADER_LAYOUT == HEADER_BRIDGE)
            .then(|| (d.read_config8(SECONDARY_BUS), d.read_config8(SUBORDINATE_BUS)));
        if (d.bus, d.slot, d.func) == (bus, slot, func) {
            me = Some(all.len());
        }
        all.push(groups::Function { bus: b, slot: s, func: f, kind, multi, acs, below });
    }
    let me = me.ok_or_else(|| String::from("not in the PCI scan"))?;
    Ok((all, me))
}

/// ACS on wherever the host's functions have it -- Source Validation, P2P
/// Request and Completion Redirect, Upstream Forwarding, each where the
/// function has the feature -- as Linux turns it on where it has an IOMMU
/// (`pci_std_enable_acs`): a port with it sends what comes up from below
/// on up, where the IOMMU sees it, rather than to a peer. Said where it
/// changed anything.
fn enable_acs() {
    for d in (0..pci::device_count()).filter_map(pci::get_any) {
        let Some(at) = d.find_extended_capability(EXT_ACS) else { continue };
        let cap = d.read_config16(at + ACS_CAPABILITY);
        let ctrl = d.read_config16(at + ACS_CONTROL);
        let want = ctrl | (cap & groups::ACS_NEEDED);
        if want != ctrl {
            d.write_config16(at + ACS_CONTROL, want);
            kcore::klog!(0, "hv: {:02x}:{:02x}.{:x}: ACS on, control {:#x} (was {:#x})", d.bus, d.slot, d.func,
                         d.read_config16(at + ACS_CONTROL), ctrl);
        }
    }
}

/// The functions of `bus`:`slot`.`func`'s IOMMU group (`groups`) that the
/// guest is not given -- held apart from it by nothing: what one sends may
/// reach the other peer to peer -- blocked while the guest holds it:
/// claimed, each in a domain of none, so that its DMA reaches nothing; by
/// the function of the group the guest is given first, in the host's order.
/// The group's bridges are left be: they forward, and master nothing of
/// their own. Refused where one of them is the host's -- a driver's, or
/// mastering -- another guest's, or one the firmware reserved memory for;
/// and for a function whose requests reach its IOMMU by a conventional
/// bridge's id, or on a bus this cannot be sure of.
fn block_group(bus: u16, slot: u16, func: u16, given: &[(u16, u16, u16)]) -> Result<Vec<Sibling>, String> {
    enable_acs();
    let (all, me) = topology(bus, slot, func)?;
    let at = |i: usize| (u16::from(all[i].bus), u16::from(all[i].slot), u16::from(all[i].func));
    let bdf = |i: usize| alloc::format!("{:02x}:{:02x}.{:x}", all[i].bus, all[i].slot, all[i].func);
    if let Some(b) = groups::behind_conventional(&all, me) {
        return Err(alloc::format!("below {}, a conventional bridge: its requests reach the IOMMU by the bridge's id, \
                                   and no domain of its own is to be had", bdf(b)));
    }
    let group = groups::groups(&all).ok_or_else(|| {
        String::from("its IOMMU group not found: no memory, or a bus with two bridges above it")
    })?;
    let members: Vec<usize> = (0..all.len()).filter(|&i| i != me && group[i] == group[me]).collect();
    if !members.is_empty() {
        let mut list = String::new();
        for &i in &members {
            list.push(' ');
            list.push_str(&bdf(i));
            if all[i].below.is_some() {
                list.push_str(" (a bridge)");
            }
        }
        let why = groups::open_port(&all, me).map_or_else(|| String::from("its device's functions, with no ACS"),
                                                          |p| alloc::format!("{} has no ACS", bdf(p)));
        kcore::klog!(0, "hv: {}: IOMMU group with{} -- {}", bdf(me), list, why);
    }
    let mut blocked = Vec::new();
    if members.iter().any(|&i| given.contains(&at(i)) && at(i) < at(me)) {
        return Ok(blocked);
    }
    for i in members.into_iter().filter(|&i| all[i].below.is_none() && !given.contains(&at(i))) {
        let (b, s, f) = at(i);
        let name = alloc::format!("{}, of its IOMMU group,", bdf(i));
        let Some(dev) = pci::get_device_by_bdf(b, s, f) else { continue };
        if dev.driven() || dev.read_config16(COMMAND) & COMMAND_MASTER != 0 {
            return Err(alloc::format!("{} is the host's: its DMA could reach the guest's device, and the guest's \
                                       it, peer to peer", name));
        }
        let sid = iommu::sid(b, s, f);
        if let Some((base, limit)) = iommu::reserved(sid) {
            return Err(alloc::format!("{} has memory the firmware reserved for its DMA ({:#x}-{:#x}, a DMAR \
                                       RMRR): it cannot be blocked", name, base, limit));
        }
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

/// How the guest's functions of a device are reset, as the guest is given
/// them and as it lets go: each by its own FLR, where every one of them has
/// one; else the whole device by a secondary bus reset of the bridge above
/// it, made once -- by the first of them the guest names, before any is set
/// up, as a bus reset of a function set up already would undo that -- where
/// one can be made (`bus_resettable`); else each by its FLR, and one with
/// none given as it is, said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ResetPlan {
    Flr,
    /// This function makes the bus reset.
    Bus,
    /// Another of the device's functions makes it.
    ByFirst,
}

impl ResetPlan {
    /// `bus`:`slot`.`func`'s, of a guest that holds `all`.
    fn of(bus: u16, slot: u16, func: u16, all: &[(u16, u16, u16)]) -> ResetPlan {
        let device = |&&(b, s, _): &&(u16, u16, u16)| b == bus && s == slot;
        let without_flr = all.iter().filter(device).any(|&(b, s, f)| {
            !pci::get_device_by_bdf(b, s, f).is_some_and(|d| has_flr(&d))
        });
        if !without_flr {
            return ResetPlan::Flr;
        }
        if let Err(why) = bus_resettable(bus, slot) {
            if !pci::get_device_by_bdf(bus, slot, func).is_some_and(|d| has_flr(&d)) {
                kcore::klog!(0, "hv: {:02x}:{:02x}.{:x}: no FLR, and no bus reset to be had -- {}: given as it is", bus,
                             slot, func, why);
            }
            return ResetPlan::Flr;
        }
        let me = all.iter().position(|&f| f == (bus, slot, func)).unwrap_or(0);
        if all[..me].iter().any(|f| device(&f)) { ResetPlan::ByFirst } else { ResetPlan::Bus }
    }

    /// Made of `dev`: false, said, where the device does not answer after.
    fn make(self, dev: &PciDevice) -> bool {
        match self {
            ResetPlan::Flr => flr(dev),
            ResetPlan::Bus => bus_reset(dev),
            ResetPlan::ByFirst => true,
        }
    }
}

/// Whether the function has an FLR of its own.
fn has_flr(dev: &PciDevice) -> bool {
    dev.find_capability(CAP_PCIE)
        .is_some_and(|cap| dev.read_config32(u16::from(cap) + PCIE_DEVCAP) & PCIE_DEVCAP_FLR != 0)
}

/// What a reset loses of a function that the host had set, read before it
/// and written back after, as Linux's `pci_save_state` keeps it: the
/// header's dwords from the command register to the interrupt line's, the
/// PCIe capability's controls, each Resizable BAR's size -- a BAR whose size
/// a reset changed under its register is mapped for the guest as one size
/// and decodes another -- and ACS's controls.
struct State {
    header: [u32; ((SAVED.end - SAVED.start) / 4) as usize],
    /// The PCIe capability, its device and link controls, and DevCtl2 where
    /// the capability's version has one.
    pcie: Option<(u16, u16, u16, Option<u16>)>,
    rebar: [Option<(u16, u32)>; REBARS_MAX],
    acs: Option<(u16, u16)>,
}

impl State {
    fn save(dev: &PciDevice) -> State {
        let mut header = [0u32; ((SAVED.end - SAVED.start) / 4) as usize];
        for (k, reg) in SAVED.step_by(4).enumerate() {
            header[k] = dev.read_config32(reg);
        }
        let pcie = dev.find_capability(CAP_PCIE).map(|cap| {
            let cap = u16::from(cap);
            let second = (dev.read_config16(cap + PCIE_FLAGS) & PCIE_FLAGS_VERSION) >= 2;
            (cap, dev.read_config16(cap + PCIE_DEVCTL), dev.read_config16(cap + PCIE_LNKCTL),
             second.then(|| dev.read_config16(cap + PCIE_DEVCTL2)))
        });
        let mut rebar = [None; REBARS_MAX];
        if let Some(cap) = dev.find_extended_capability(EXT_REBAR) {
            let count = ((dev.read_config32(cap + REBAR_CONTROL) >> REBAR_COUNT_SHIFT) & REBAR_COUNT_MASK) as usize;
            for (k, slot) in rebar.iter_mut().enumerate().take(count) {
                let at = cap + REBAR_CONTROL + REBAR_ENTRY_BYTES * k as u16;
                *slot = Some((at, dev.read_config32(at)));
            }
        }
        let acs = dev.find_extended_capability(EXT_ACS).map(|cap| (cap + ACS_CONTROL, dev.read_config16(cap + ACS_CONTROL)));
        State { header, pcie, rebar, acs }
    }

    /// Written back: the capability's controls and the BARs' sizes first --
    /// a BAR register's writable bits are its size's -- then the header, its
    /// command register last.
    fn restore(&self, dev: &PciDevice) {
        if let Some((cap, control, link, control2)) = self.pcie {
            dev.write_config16(cap + PCIE_DEVCTL, control & !PCIE_DEVCTL_FLR);
            dev.write_config16(cap + PCIE_LNKCTL, link);
            if let Some(c2) = control2 {
                dev.write_config16(cap + PCIE_DEVCTL2, c2);
            }
        }
        for &(at, control) in self.rebar.iter().flatten() {
            dev.write_config32(at, control);
        }
        if let Some((at, control)) = self.acs {
            dev.write_config16(at, control);
        }
        for (k, reg) in SAVED.step_by(4).enumerate().skip(1) {
            dev.write_config32(reg, self.header[k]);
        }
        dev.write_config32(SAVED.start, self.header[0]);
    }
}

/// Whether `dev` answers -- asked for its vendor id until it does, a second
/// at most past the time its reset has (PCIe 5.0, 6.6.2: 100 ms). Said where
/// it does not.
fn answers(dev: &PciDevice, how: &str) -> bool {
    kcore::task::sleep_ms(RESET_MS);
    let mut waited = 0;
    while dev.read_config16(0) == VENDOR_NONE && waited < ANSWER_MS {
        kcore::task::sleep_ms(ANSWER_STEP_MS);
        waited += ANSWER_STEP_MS;
    }
    if dev.read_config16(0) == VENDOR_NONE {
        kcore::klog!(0, "hv: {:02x}:{:02x}.{:x} did not answer after its {}", dev.bus, dev.slot, dev.func, how);
        return false;
    }
    true
}

/// A Function Level Reset, as Linux makes it (`pcie_flr`,
/// `pci_save_state`): its transactions given a second to finish, its state
/// saved before and written back after (`State`), the device asked until it
/// answers. False, said, and nothing more done, for a device that does not
/// answer; true and nothing done for a function without FLR.
fn flr(dev: &PciDevice) -> bool {
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
    let state = State::save(dev);
    let control = dev.read_config16(cap + PCIE_DEVCTL);
    dev.write_config16(cap + PCIE_DEVCTL, control | PCIE_DEVCTL_FLR);
    if !answers(dev, "FLR") {
        return false;
    }
    state.restore(dev);
    true
}

/// The function's expansion ROM, read once (PCI 3.0, 6.3): its BAR sized,
/// turned on at the address the firmware gave it -- the device's memory
/// decoded for that alone, as nothing else reaches it meanwhile -- its image
/// copied a dword at a time, the BAR off again. None where it has no ROM BAR
/// or the firmware gave it no address; said, where the image is none -- no
/// 0x55AA -- or its size is no ROM's the guest is given.
fn read_rom(dev: &PciDevice, name: &str) -> Option<Box<[u8]>> {
    let register = dev.read_config32(ROM_REGISTER);
    dev.write_config32(ROM_REGISTER, ROM_ADDRESS);
    let mask = dev.read_config32(ROM_REGISTER) & ROM_ADDRESS;
    dev.write_config32(ROM_REGISTER, register & !ROM_ENABLE);
    let address = u64::from(register & ROM_ADDRESS);
    if mask == 0 || address == 0 {
        return None;
    }
    let size = u64::from(!mask) + 1;
    if size > hv::host::ROM_MAX as u64 || address % size != 0 {
        kcore::klog!(0, "hv: {}: its ROM of {:#x} bytes at {:#x} is no ROM a guest is given", name, size, address);
        return None;
    }
    /* A page at least, as the guest is mapped it: one smaller padded with
     * what no ROM holds there, all ones */
    let given = (size as usize).max(hv::host::ROM_MIN);
    let mut image = Vec::new();
    image.try_reserve_exact(given).ok()?;
    let page = kcore::consts::PAGE_SIZE as u64;
    let map = PhysMapping::map(address, size.div_ceil(page) as usize)?;
    let region = MmioRegion::new(map.as_mut_ptr(), map.len());
    let command = dev.read_config16(COMMAND);
    dev.write_config16(COMMAND, command | COMMAND_MEMORY);
    dev.write_config32(ROM_REGISTER, (register & ROM_ADDRESS) | ROM_ENABLE);
    for at in (0..size as usize).step_by(4) {
        image.extend_from_slice(&region.read32(at).to_le_bytes());
    }
    dev.write_config32(ROM_REGISTER, register & !ROM_ENABLE);
    dev.write_config16(COMMAND, command);
    image.resize(given, 0xFF);
    if image.get(..2) != Some(&[0x55, 0xAA][..]) {
        kcore::klog!(0, "hv: {}: its ROM BAR at {:#x} holds no ROM image (no 0x55AA): none given", name, address);
        return None;
    }
    Some(image.into_boxed_slice())
}

/// The bridge whose secondary bus is `bus`.
fn bridge_of(bus: u16) -> Option<PciDevice> {
    (0..pci::device_count()).filter_map(pci::get_device).find(|d| {
        d.read_config8(HEADER_TYPE) & HEADER_LAYOUT == HEADER_BRIDGE && u16::from(d.read_config8(SECONDARY_BUS)) == bus
    })
}

/// The bridge a secondary bus reset of the device at `bus`:`slot` goes
/// through -- where the bus is the device's alone, as it is below a PCIe
/// port: the reset reaches every function on the bus, and the guest holds or
/// has blocked only the device's. Why not, where there is no such bridge or
/// another device is beside it.
fn bus_resettable(bus: u16, slot: u16) -> Result<PciDevice, String> {
    let bridge = bridge_of(bus).ok_or_else(|| String::from("no bridge above it"))?;
    for s in (0..SLOTS).filter(|&s| s != slot) {
        if let Some(f) = (0..FUNCTIONS).find_map(|func| pci::get_device_by_bdf(bus, s, func)) {
            return Err(alloc::format!("{:02x}:{:02x}.{:x} is on its bus, which the reset would reach", f.bus, f.slot,
                                      f.func));
        }
    }
    Ok(bridge)
}

/// The device `dev` is a function of reset whole by the bridge above it --
/// a secondary bus reset (PCI-to-PCI Bridge 1.2, 3.2.5.18), as Linux's
/// `pci_reset_secondary_bus` makes one: every function of it saved before
/// and written back after, where `bus_resettable` says it can be. False,
/// said, for a device that does not answer after.
fn bus_reset(dev: &PciDevice) -> bool {
    let name = alloc::format!("{:02x}:{:02x}.{:x}", dev.bus, dev.slot, dev.func);
    let bridge = match bus_resettable(dev.bus, dev.slot) {
        Ok(b) => b,
        Err(why) => {
            kcore::klog!(0, "hv: {}: no bus reset to be had -- {}: given as it is", name, why);
            return true;
        }
    };
    let mut functions = Vec::new();
    for func in 0..FUNCTIONS {
        let Some(f) = pci::get_device_by_bdf(dev.bus, dev.slot, func) else { continue };
        if functions.try_reserve(1).is_err() {
            kcore::klog!(0, "hv: {}: out of memory for its functions' state: not reset", name);
            return true;
        }
        functions.push((f, State::save(&f)));
    }
    let control = bridge.read_config16(BRIDGE_CONTROL);
    bridge.write_config16(BRIDGE_CONTROL, control | BRIDGE_BUS_RESET);
    kcore::task::sleep_ms(BUS_RESET_MS);
    bridge.write_config16(BRIDGE_CONTROL, control & !BRIDGE_BUS_RESET);
    if !answers(dev, "bus reset") {
        return false;
    }
    for (f, state) in &functions {
        state.restore(f);
    }
    kcore::klog!(0, "hv: {} reset by its bridge {:02x}:{:02x}.{:x}'s secondary bus reset, {} function(s) of it written \
                     back", name, bridge.bus, bridge.slot, bridge.func, functions.len());
    true
}
