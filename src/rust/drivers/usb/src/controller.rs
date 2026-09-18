//! The xHCI host controller: bring-up, the event ring, commands, control
//! transfers, enumeration, hubs and keyboard servicing.
//!
//! This is a polling driver. The USB task owns every controller and is the
//! only thing that calls `poll`; the shell reads what it published in
//! [`crate::shared`]. Nothing here runs from an interrupt handler, and every
//! wait is a sleep, so the whole file may block.

use alloc::boxed::Box;
use alloc::vec::Vec;

use kcore::barrier::{dma_rmb, dma_wmb};
use kcore::consts::PAGE_SIZE;
use kcore::dma::{DmaBuffer, PhysMapping};
use kcore::io::MmioRegion;
use kcore::pci::PciDevice;
use kcore::trace;

use crate::descriptors::{self as usb, speed_name};
use crate::device::{Attach, Device};
use crate::regs::*;
use crate::ring::{EventRing, Ring};
use crate::shared;

pub const MAX_CONTROLLERS: usize = 4;
pub const MAX_DEVICES: usize = 12;
pub const MAX_PORTS: usize = 64;

/// Route strings hold five nibbles; stopping at four keeps the recursion and
/// every downstream port number encodable.
const MAX_TIER: u8 = 4;

/// The per-subsystem trace level, as `kernel/trace.h` has it for UsbLL.
const USB_LL: u32 = 3;

/* Timeouts. Real controllers answer commands in microseconds; these bounds
 * exist only so a wedged or absent device cannot hang the USB task. */
const COMMAND_TIMEOUT_MS: u64 = 1000;
const TRANSFER_TIMEOUT_MS: u64 = 1000;
const RESET_TIMEOUT_MS: u64 = 1000;
const PORT_RESET_TIMEOUT_MS: u64 = 750;
const BIOS_HANDOFF_TIMEOUT_MS: u64 = 1000;

pub const POLL_PERIOD_MS: u64 = 4;
const PORT_RESCAN_PERIOD_MS: u64 = 1000;

fn sleep_ms(ms: u64) {
    kcore::task::sleep_ms(ms);
}

fn now_ms() -> u64 {
    kcore::time::boot_time_ns() / 1_000_000
}

/// PORTSC is a minefield of write-1-to-clear and write-to-act bits: build
/// every write from the live value with those bits forced off, then OR in
/// only the action wanted.
fn portsc_base(v: u32) -> u32 {
    v & !(PORTSC_CHANGE_MASK | PORTSC_ENABLED | PORTSC_RESET
        | PORTSC_WARM_RESET | PORTSC_LINK_WRITE_STROBE)
}

/// Encode bInterval into the xHCI endpoint-context Interval field, which is
/// log2 of the service interval in 125us microframes.
fn encode_interval(speed: u8, b_interval: u8) -> u8 {
    if speed == usb::SPEED_HIGH || speed == usb::SPEED_SUPER
        || speed == usb::SPEED_SUPER_PLUS
    {
        return b_interval.clamp(1, 16) - 1;
    }

    /* Low and full speed express bInterval in whole 1 ms frames */
    let micro = b_interval.max(1) as u32 * 8;
    let mut i = 3u8;
    while i < 10 && (1u32 << (i + 1)) <= micro {
        i += 1;
    }
    i
}

pub struct Controller {
    index: usize,
    ready: bool,
    pci: PciDevice,

    /* The BAR0 mapping must outlive the windows into it. */
    _mapping: PhysMapping,
    cap: MmioRegion,
    op: MmioRegion,
    rt: MmioRegion,
    db: MmioRegion,

    hci_version: u16,
    max_slots: u8,
    num_ports: u8,
    /// 32 or 64 bytes, from HCCPARAMS1.CSZ.
    context_size: u32,
    page_size_bytes: u32,
    /// In dwords; 0 when absent.
    ext_cap_offset: u16,

    dcbaa: Option<DmaBuffer>,
    scratchpad_array: Option<DmaBuffer>,
    scratchpads: Vec<DmaBuffer>,

    cmd_ring: Ring,
    evt_ring: EventRing,

    /* One command in flight at a time; the USB task is the only issuer. */
    pending_cmd_trb: u64,
    cmd_done: bool,
    cmd_completion: u32,
    cmd_slot_id: u8,

    port_change_pending: bool,
    last_scan_ms: u64,

    devices: [Device; MAX_DEVICES],
}

impl Controller {
    /* ---- register windows ---- */

    fn cap_read32(&self, off: usize) -> u32 {
        self.cap.read32(off)
    }

    fn op_read32(&self, off: usize) -> u32 {
        self.op.read32(off)
    }

    fn op_write32(&self, off: usize, val: u32) {
        self.op.write32(off, val);
    }

    /// Split into two 32-bit stores: several controllers (and QEMU's own
    /// device model) reject a single 64-bit access to these registers.
    fn op_write64(&self, off: usize, val: u64) {
        self.op.write32(off, val as u32);
        self.op.write32(off + 4, (val >> 32) as u32);
    }

    fn port_read32(&self, port: u8, off: usize) -> u32 {
        self.op.read32(OP_PORTSC_BASE + (port as usize - 1) * OP_PORT_REG_SIZE + off)
    }

    fn port_write32(&self, port: u8, off: usize, val: u32) {
        self.op.write32(OP_PORTSC_BASE + (port as usize - 1) * OP_PORT_REG_SIZE + off, val);
    }

    fn rt_read32(&self, off: usize) -> u32 {
        self.rt.read32(off)
    }

    fn rt_write32(&self, off: usize, val: u32) {
        self.rt.write32(off, val);
    }

    fn rt_write64(&self, off: usize, val: u64) {
        self.rt.write32(off, val as u32);
        self.rt.write32(off + 4, (val >> 32) as u32);
    }

    fn doorbell(&self, slot: u8, value: u32) {
        /* Every ring update must be visible before the doorbell rings */
        dma_wmb();
        self.db.write32(slot as usize * 4, value);
    }

    /// The byte offset of an endpoint context inside a context block. The
    /// slot context is at 0, so DCI `n` is at `n * context_size`.
    fn ep_ctx_offset(&self, dci: u8) -> usize {
        (dci as usize) * (self.context_size as usize)
    }

    /// Where the device's own contexts start inside the input context: the
    /// input control context takes the first block.
    fn input_body(&self) -> usize {
        self.context_size as usize
    }
}

/* ---- bring-up ---- */

impl Controller {
    /// Map BAR0 and read what the controller says about itself. None when it
    /// is not something this driver can drive.
    pub(crate) fn open(index: usize, pci: PciDevice) -> Option<Box<Self>> {
        /* Memory space and bus mastering. Firmware normally leaves both on,
         * but a controller handed over cold has neither. */
        let command = pci.read_config16(0x04) | (1 << 1) | (1 << 2);
        pci.write_config16(0x04, command);

        let (phys, size) = Self::bar0(&pci)?;
        let pages = size.div_ceil(PAGE_SIZE);
        let mapping = match PhysMapping::map(phys, pages) {
            Some(mapping) => mapping,
            None => {
                trace!(0, "Xhci: failed to map BAR0 phys 0x{:X} size 0x{:X}", phys, size);
                return None;
            }
        };
        trace!(0, "Xhci: BAR0 phys 0x{:X} size 0x{:X} va 0x{:X}",
            phys, size, mapping.as_mut_ptr() as usize);

        let base = mapping.as_mut_ptr();
        let cap = MmioRegion::new(base, size);

        let cap_dw0 = cap.read32(CAP_CAPLENGTH);
        let cap_length = (cap_dw0 & 0xFF) as u8;
        let hci_version = ((cap_dw0 >> 16) & 0xFFFF) as u16;

        if cap_length == 0 || cap_length == 0xFF {
            trace!(0, "Xhci: implausible CAPLENGTH {}", cap_length);
            return None;
        }

        let hcs1 = cap.read32(CAP_HCSPARAMS1);
        let max_slots = (hcs1 & 0xFF) as u8;
        let num_ports = ((hcs1 >> 24) & 0xFF) as u8;

        let hcc1 = cap.read32(CAP_HCCPARAMS1);
        let ac64 = hcc1 & 1 != 0;
        let context_size = if (hcc1 >> 2) & 1 != 0 { 64 } else { 32 };
        let ext_cap_offset = ((hcc1 >> 16) & 0xFFFF) as u16;

        let db_off = (cap.read32(CAP_DBOFF) & !0x3) as usize;
        let rts_off = (cap.read32(CAP_RTSOFF) & !0x1F) as usize;

        trace!(0, "Xhci: {:04x}:{:04x} hci {:x}.{:x} caplen {} slots {} ports {}",
            pci.vendor, pci.device, hci_version >> 8, hci_version & 0xFF,
            cap_length, max_slots, num_ports);

        if max_slots == 0 || num_ports == 0 {
            trace!(0, "Xhci: no slots or ports");
            return None;
        }

        if !ac64 {
            /* Every DMA structure this driver allocates could sit above 4 GB;
             * refusing beats silently truncating a pointer. */
            trace!(0, "Xhci: 32-bit-only controller not supported");
            return None;
        }

        let op = MmioRegion::new(unsafe { base.add(cap_length as usize) }, size);
        let rt = MmioRegion::new(unsafe { base.add(rts_off) }, size);
        let db = MmioRegion::new(unsafe { base.add(db_off) }, size);

        Some(Box::new(Self {
            index,
            ready: false,
            pci,
            _mapping: mapping,
            cap,
            op,
            rt,
            db,
            hci_version,
            max_slots,
            num_ports,
            context_size,
            page_size_bytes: PAGE_SIZE as u32,
            ext_cap_offset,
            dcbaa: None,
            scratchpad_array: None,
            scratchpads: Vec::new(),
            cmd_ring: Ring::new(),
            evt_ring: EventRing::new(),
            pending_cmd_trb: 0,
            cmd_done: false,
            cmd_completion: COMP_INVALID,
            cmd_slot_id: 0,
            port_change_pending: false,
            last_scan_ms: 0,
            devices: [const { Device::new() }; MAX_DEVICES],
        }))
    }

    /// Where BAR0 is and how big, size-probing both halves -- probing only
    /// the low half computes a bogus size for a BAR placed above 4 GB.
    fn bar0(pci: &PciDevice) -> Option<(u64, usize)> {
        let bar_low = pci.read_config32(0x10);
        if bar_low & 1 != 0 {
            trace!(0, "Xhci: BAR0 is I/O space, not MMIO");
            return None;
        }

        let is64 = (bar_low & 0x6) == 0x4;
        let bar_high = if is64 { pci.read_config32(0x14) } else { 0 };
        let phys = ((bar_high as u64) << 32) | (bar_low & !0xF) as u64;
        if phys == 0 {
            trace!(0, "Xhci: BAR0 not assigned");
            return None;
        }

        pci.write_config32(0x10, 0xFFFF_FFFF);
        let mask_low = pci.read_config32(0x10);
        let mut mask_high = 0xFFFF_FFFFu32;
        if is64 {
            pci.write_config32(0x14, 0xFFFF_FFFF);
            mask_high = pci.read_config32(0x14);
            pci.write_config32(0x14, bar_high);
        }
        pci.write_config32(0x10, bar_low);

        let size_mask = ((mask_high as u64) << 32) | (mask_low & !0xF) as u64;
        let mut size = (!size_mask).wrapping_add(1) as usize;
        if size == 0 || size > 16 * 1024 * 1024 {
            size = 64 * 1024;
        }
        Some((phys, size))
    }

    /// Ask firmware for the controller and silence its SMIs.
    fn take_ownership_from_bios(&self) {
        if self.ext_cap_offset == 0 {
            return;
        }

        /* The extended capability list is dword-indexed from the capability
         * base, a zero Next terminating the walk. Bound the loop so a corrupt
         * list cannot spin forever. */
        const MAX_EXT_CAPS: usize = 64;
        let mut off = self.ext_cap_offset as usize * 4;

        for _ in 0..MAX_EXT_CAPS {
            if off == 0 {
                break;
            }
            let cap = self.cap_read32(off);
            let id = cap & 0xFF;
            let next = (cap >> 8) & 0xFF;

            if id == EXT_CAP_LEGACY_SUPPORT {
                if cap & LEGACY_BIOS_OWNED == 0 && cap & LEGACY_OS_OWNED != 0 {
                    trace!(0, "Xhci: already OS-owned");
                } else {
                    trace!(0, "Xhci: requesting ownership from firmware (usblegsup 0x{:X})", cap);
                    self.cap.write32(off, cap | LEGACY_OS_OWNED);

                    let deadline = now_ms() + BIOS_HANDOFF_TIMEOUT_MS;
                    loop {
                        let cur = self.cap_read32(off);
                        if cur & LEGACY_BIOS_OWNED == 0 {
                            break;
                        }
                        if now_ms() >= deadline {
                            /* Some firmware never releases the semaphore.
                             * Clear its bit by hand: the controller is about
                             * to be reset anyway, and leaving SMIs armed is
                             * worse. */
                            trace!(0, "Xhci: BIOS handoff timed out, forcing ownership");
                            let cur = self.cap_read32(off);
                            self.cap.write32(off, (cur & !LEGACY_BIOS_OWNED) | LEGACY_OS_OWNED);
                            break;
                        }
                        sleep_ms(10);
                    }
                }

                /* Silence every legacy SMI source and acknowledge stale
                 * status, otherwise firmware keeps trapping our register
                 * accesses. */
                let ctl = self.cap_read32(off + 4);
                self.cap.write32(off + 4, (ctl & LEGACY_CTL_KEEP_MASK) | LEGACY_CTL_ACK_SMI_MASK);
            }

            if next == 0 {
                break;
            }
            off += next as usize * 4;
        }
    }

    /// Intel 7/8-series and Cherry Trail PCHs boot with their USB2 ports
    /// wired to the companion EHCI controller and their SuperSpeed pins
    /// disabled; the ports only appear on the xHCI after this routing switch.
    /// Harmless on parts that do not implement the registers, but only
    /// applied to the models Intel documents it for.
    fn apply_intel_port_switch(&self) {
        const VENDOR_INTEL: u16 = 0x8086;
        const PANTHER_POINT: u16 = 0x1E31;
        const LYNX_POINT: u16 = 0x8C31;
        const LYNX_POINT_LP: u16 = 0x9C31;
        const CHERRYVIEW: u16 = 0x22B5;

        if self.pci.vendor != VENDOR_INTEL {
            return;
        }
        if !matches!(self.pci.device, PANTHER_POINT | LYNX_POINT | LYNX_POINT_LP | CHERRYVIEW) {
            return;
        }

        const REG_USB3_PRM: u16 = 0xDC; /* SuperSpeed-capable port mask */
        const REG_USB3_PSSEN: u16 = 0xD8; /* SuperSpeed enable */
        const REG_XUSB2_PRM: u16 = 0xD4; /* USB2 routable port mask */
        const REG_XUSB2_PR: u16 = 0xD0; /* USB2 port routing */

        let ss_mask = self.pci.read_config32(REG_USB3_PRM);
        self.pci.write_config32(REG_USB3_PSSEN, ss_mask);

        let hs_mask = self.pci.read_config32(REG_XUSB2_PRM);
        self.pci.write_config32(REG_XUSB2_PR, hs_mask);

        trace!(0, "Xhci: Intel port switch: usb3 0x{:X} usb2 0x{:X}", ss_mask, hs_mask);
    }

    fn reset_controller(&self) -> bool {
        /* The controller may still be publishing CNR after a warm reset */
        let mut deadline = now_ms() + RESET_TIMEOUT_MS;
        while self.op_read32(OP_USBSTS) & USBSTS_CONTROLLER_NOT_READY != 0 {
            if now_ms() >= deadline {
                trace!(0, "Xhci: controller not ready before reset");
                return false;
            }
            sleep_ms(1);
        }

        /* Stop it first: HCRST on a running controller is undefined */
        let cmd = self.op_read32(OP_USBCMD);
        if cmd & USBCMD_RUN != 0 {
            self.op_write32(OP_USBCMD, cmd & !USBCMD_RUN);

            deadline = now_ms() + RESET_TIMEOUT_MS;
            while self.op_read32(OP_USBSTS) & USBSTS_HALTED == 0 {
                if now_ms() >= deadline {
                    trace!(0, "Xhci: controller did not halt");
                    return false;
                }
                sleep_ms(1);
            }
        }

        self.op_write32(OP_USBCMD, self.op_read32(OP_USBCMD) | USBCMD_RESET);

        deadline = now_ms() + RESET_TIMEOUT_MS;
        loop {
            let c = self.op_read32(OP_USBCMD);
            let s = self.op_read32(OP_USBSTS);

            /* A controller that has fallen off the bus reads back all-ones */
            if c == 0xFFFF_FFFF || s == 0xFFFF_FFFF {
                trace!(0, "Xhci: controller vanished during reset");
                return false;
            }

            if c & USBCMD_RESET == 0 && s & USBSTS_CONTROLLER_NOT_READY == 0 {
                return true;
            }

            if now_ms() >= deadline {
                trace!(0, "Xhci: reset timed out (usbcmd 0x{:X} usbsts 0x{:X})", c, s);
                return false;
            }
            sleep_ms(1);
        }
    }

    fn setup_memory(&mut self) -> bool {
        /* The Device Context Base Address Array: index 0 holds the scratchpad
         * array pointer, 1..max_slots the per-slot device contexts. */
        let mut dcbaa = match DmaBuffer::new(1) {
            Some(dcbaa) => dcbaa,
            None => return false,
        };
        dcbaa.as_mut_slice().fill(0);

        let hcs2 = self.cap_read32(CAP_HCSPARAMS2);
        let mut count = (((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F);

        const MAX_SCRATCHPADS: u32 = 512;
        if count > MAX_SCRATCHPADS {
            trace!(0, "Xhci: {} scratchpad buffers requested, capping at {}",
                count, MAX_SCRATCHPADS);
            count = MAX_SCRATCHPADS;
        }

        if count > 0 {
            let mut array = match DmaBuffer::new(1) {
                Some(array) => array,
                None => return false,
            };
            array.as_mut_slice().fill(0);

            let pages_each = (self.page_size_bytes as usize / PAGE_SIZE).max(1);
            if self.scratchpads.try_reserve_exact(count as usize).is_err() {
                trace!(0, "Xhci: out of memory for the scratchpad list");
                return false;
            }

            for i in 0..count as usize {
                let mut page = match DmaBuffer::new(pages_each) {
                    Some(page) => page,
                    None => {
                        trace!(0, "Xhci: out of memory for scratchpad {}", i);
                        return false;
                    }
                };
                page.as_mut_slice().fill(0);
                let phys = page.phys();
                array.as_mut_slice()[i * 8..i * 8 + 8].copy_from_slice(&phys.to_le_bytes());
                self.scratchpads.push(page);
            }

            let array_phys = array.phys();
            dcbaa.as_mut_slice()[0..8].copy_from_slice(&array_phys.to_le_bytes());
            self.scratchpad_array = Some(array);
        }

        trace!(0, "Xhci: {} slots, {} ports, {} scratchpads, ctx {} bytes",
            self.max_slots, self.num_ports, count, self.context_size);

        let dcbaa_phys = dcbaa.phys();
        self.dcbaa = Some(dcbaa);

        self.op_write32(OP_CONFIG, (self.op_read32(OP_CONFIG) & !0xFF) | self.max_slots as u32);
        self.op_write64(OP_DCBAAP, dcbaa_phys);
        self.op_write32(OP_DNCTRL, 0);

        if !self.cmd_ring.init() {
            return false;
        }
        self.op_write64(OP_CRCR, self.cmd_ring.phys() | CRCR_RING_CYCLE_STATE);

        if !self.evt_ring.init() {
            return false;
        }

        /* ERSTBA must be written last: that store arms the event ring */
        self.rt_write32(RT_INTERRUPTER0 + IR_ERSTSZ, 1);
        self.rt_write64(RT_INTERRUPTER0 + IR_ERDP,
            self.evt_ring.dequeue_phys() | ERDP_EVENT_HANDLER_BUSY);
        self.rt_write64(RT_INTERRUPTER0 + IR_ERSTBA, self.evt_ring.erst_phys());

        /* A polling driver: leave the interrupter disabled but keep its
         * pending bit clear, so the event-ring state machine keeps
         * advancing. */
        self.rt_write32(RT_INTERRUPTER0 + IR_IMOD, 0);
        let iman = self.rt_read32(RT_INTERRUPTER0 + IR_IMAN);
        self.rt_write32(RT_INTERRUPTER0 + IR_IMAN,
            (iman & !IMAN_INTERRUPT_ENABLE) | IMAN_INTERRUPT_PENDING);

        true
    }

    fn start_controller(&self) -> bool {
        let cmd = (self.op_read32(OP_USBCMD) | USBCMD_RUN | USBCMD_HS_ERR_ENABLE)
            & !USBCMD_INT_ENABLE;
        self.op_write32(OP_USBCMD, cmd);

        let deadline = now_ms() + RESET_TIMEOUT_MS;
        while self.op_read32(OP_USBSTS) & USBSTS_HALTED != 0 {
            if now_ms() >= deadline {
                trace!(0, "Xhci: controller did not start");
                return false;
            }
            sleep_ms(1);
        }
        true
    }

    fn power_ports(&self) {
        /* The debounce interval a device attached before boot still needs
         * before PORTSC.CCS can be trusted (USB 2.0 TATTDB). */
        const PORT_DEBOUNCE_MS: u64 = 100;

        for port in 1..=self.num_ports.min(MAX_PORTS as u8 - 1) {
            let v = self.port_read32(port, 0);
            if v & PORTSC_POWER == 0 {
                self.port_write32(port, 0, portsc_base(v) | PORTSC_POWER);
            }
        }

        /* USB 2.0 requires 100 ms of bPwrOn2PwrGood after switching port
         * power, and the same wait doubles as the attach debounce when
         * firmware had already powered everything. */
        sleep_ms(PORT_DEBOUNCE_MS);
    }

    pub(crate) fn init(&mut self) -> bool {
        self.apply_intel_port_switch();
        self.take_ownership_from_bios();

        if !self.reset_controller() {
            return false;
        }

        /* PAGESIZE is a bitmap: bit n set means the controller supports a
         * 2^(n+12) byte page. Take the smallest one it offers. */
        let bitmap = self.op_read32(OP_PAGESIZE) & 0xFFFF;
        self.page_size_bytes = PAGE_SIZE as u32;
        for bit in 0..16 {
            if bitmap & (1 << bit) != 0 {
                self.page_size_bytes = 1u32 << (bit + 12);
                break;
            }
        }
        if (self.page_size_bytes as usize) < PAGE_SIZE {
            self.page_size_bytes = PAGE_SIZE as u32;
        }

        if !self.setup_memory() || !self.start_controller() {
            return false;
        }

        self.ready = true;

        shared::update(self.index, |s| {
            s.live = true;
            s.vendor = self.pci.vendor;
            s.device = self.pci.device;
            s.hci_version = self.hci_version;
            s.max_slots = self.max_slots;
            s.num_ports = self.num_ports;
            s.context_size = self.context_size;
        });

        self.power_ports();
        self.last_scan_ms = now_ms();
        self.scan_ports();

        true
    }
}

/* ---- events and commands ---- */

impl Controller {
    fn device_by_slot(&mut self, slot: u8) -> Option<usize> {
        if slot == 0 {
            return None;
        }
        (0..MAX_DEVICES).find(|&i| self.devices[i].in_use && self.devices[i].slot_id == slot)
    }

    fn alloc_device(&mut self) -> Option<usize> {
        (0..MAX_DEVICES).find(|&i| !self.devices[i].in_use)
    }

    fn handle_command_event(&mut self, param: u64, status: u32, control: u32) {
        let trb_phys = param & !0xF;
        let code = trb_completion_of(status);
        let slot = trb_slot_of(control);

        if self.pending_cmd_trb != 0 && trb_phys == self.pending_cmd_trb {
            self.cmd_completion = code;
            self.cmd_slot_id = slot;
            self.cmd_done = true;
            return;
        }

        trace!(USB_LL, "Xhci: stray command completion trb 0x{:X} code {}", trb_phys, code);
    }

    fn handle_transfer_event(&mut self, param: u64, status: u32, control: u32) {
        let trb_phys = param & !0xF;
        let code = trb_completion_of(status);
        let residual = trb_residual_of(status);
        let slot = trb_slot_of(control);
        let dci = trb_endpoint_of(control);

        let index = match self.device_by_slot(slot) {
            Some(index) => index,
            None => {
                trace!(USB_LL, "Xhci: transfer event for unknown slot {}", slot);
                return;
            }
        };

        let dev = &mut self.devices[index];
        let ep = if dci == dev.ep0.dci {
            &mut dev.ep0
        } else if dci == dev.intr_in.dci && dev.intr_in.dci != 0 {
            &mut dev.intr_in
        } else {
            trace!(USB_LL, "Xhci: transfer event for unknown dci {} slot {}", dci, slot);
            return;
        };

        /* Only the data-carrying TRB reports a meaningful residual. A short
         * packet ends the data TD early and raises its own event (ISP is
         * set); the status TD still runs and delivers the completion being
         * waited on, so record the length here and let the wait continue. */
        if trb_phys == ep.data_trb {
            ep.residual = residual;
        }

        if trb_phys == ep.pending_trb {
            ep.completion = code;
            ep.done = true;
            return;
        }

        if code != COMP_SUCCESS && code != COMP_SHORT_PACKET {
            /* An error on an earlier stage of the transfer: nothing further
             * will be executed, so fail the wait now instead of timing out. */
            ep.completion = code;
            ep.done = true;
        }
    }

    fn process_events(&mut self) {
        if !self.evt_ring.is_ready() {
            return;
        }

        /* Bounded, so a controller wedged into producing events forever
         * cannot starve the rest of the USB task. */
        const MAX_EVENTS_PER_PASS: usize = 256;

        let mut drained = 0;
        while drained < MAX_EVENTS_PER_PASS {
            let ev = match self.evt_ring.pop() {
                Some(ev) => ev,
                None => break,
            };
            drained += 1;

            match trb_type_of(ev.control) {
                TRB_TRANSFER_EVENT => self.handle_transfer_event(ev.param, ev.status, ev.control),
                TRB_COMMAND_COMPLETION => {
                    self.handle_command_event(ev.param, ev.status, ev.control)
                }
                TRB_PORT_STATUS_CHANGE => {
                    self.port_change_pending = true;
                    trace!(USB_LL, "Xhci: port status change, port {}", (ev.param >> 24) & 0xFF);
                }
                other => trace!(USB_LL, "Xhci: event type {} ignored", other),
            }
        }

        if drained > 0 {
            self.rt_write64(RT_INTERRUPTER0 + IR_ERDP,
                self.evt_ring.dequeue_phys() | ERDP_EVENT_HANDLER_BUSY);
        }

        let sts = self.op_read32(OP_USBSTS);
        if sts & USBSTS_EVENT_INT != 0 {
            self.op_write32(OP_USBSTS, USBSTS_EVENT_INT);
        }
        if sts & USBSTS_PORT_CHANGE != 0 {
            self.op_write32(OP_USBSTS, USBSTS_PORT_CHANGE);
            self.port_change_pending = true;
        }
        if sts & (USBSTS_HOST_SYSTEM_ERROR | USBSTS_HOST_CONTROLLER_ERROR) != 0 {
            trace!(0, "Xhci: host controller error, usbsts 0x{:X}", sts);
            self.op_write32(OP_USBSTS,
                sts & (USBSTS_HOST_SYSTEM_ERROR | USBSTS_HOST_CONTROLLER_ERROR));
            self.ready = false;
        }

        let iman = self.rt_read32(RT_INTERRUPTER0 + IR_IMAN);
        if iman & IMAN_INTERRUPT_PENDING != 0 {
            self.rt_write32(RT_INTERRUPTER0 + IR_IMAN,
                (iman & !IMAN_INTERRUPT_ENABLE) | IMAN_INTERRUPT_PENDING);
        }
    }

    fn wait_command(&mut self, trb_phys: u64) -> u32 {
        let deadline = now_ms() + COMMAND_TIMEOUT_MS;

        while !self.cmd_done {
            self.process_events();
            if self.cmd_done {
                break;
            }
            if now_ms() >= deadline {
                trace!(0, "Xhci: command trb 0x{:X} timed out", trb_phys);
                self.pending_cmd_trb = 0;
                return COMP_INVALID;
            }
            sleep_ms(1);
        }

        self.pending_cmd_trb = 0;
        self.cmd_done = false;
        self.cmd_completion
    }

    /// Run one command and wait for it: the completion code, and the slot id
    /// the controller answered with.
    fn run_command(&mut self, param: u64, status: u32, control: u32) -> (u32, u8) {
        if !self.cmd_ring.is_ready() {
            return (COMP_INVALID, 0);
        }

        self.cmd_done = false;
        self.cmd_completion = COMP_INVALID;
        self.cmd_slot_id = 0;

        let trb_phys = self.cmd_ring.push(param, status, control);
        if trb_phys == 0 {
            return (COMP_INVALID, 0);
        }
        self.pending_cmd_trb = trb_phys;

        self.doorbell(0, 0);

        let code = self.wait_command(trb_phys);
        (code, self.cmd_slot_id)
    }
}

/* ---- transfers ---- */

/// Which endpoint of a device a transfer is on. The two are named rather
/// than borrowed, because the wait between submitting and completing calls
/// back into the controller.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ep {
    Control,
    IntrIn,
}

impl Controller {
    fn ep_mut<'a>(dev: &'a mut Device, ep: Ep) -> &'a mut crate::device::Endpoint {
        match ep {
            Ep::Control => &mut dev.ep0,
            Ep::IntrIn => &mut dev.intr_in,
        }
    }

    fn wait_transfer(&mut self, index: usize, ep: Ep, trb_phys: u64, timeout_ms: u64) -> u32 {
        let deadline = now_ms() + timeout_ms;

        loop {
            if Self::ep_mut(&mut self.devices[index], ep).done {
                break;
            }
            self.process_events();
            if Self::ep_mut(&mut self.devices[index], ep).done {
                break;
            }
            if now_ms() >= deadline {
                let dci = Self::ep_mut(&mut self.devices[index], ep).dci;
                trace!(0, "Xhci: transfer trb 0x{:X} dci {} timed out", trb_phys, dci);
                let ep = Self::ep_mut(&mut self.devices[index], ep);
                ep.in_flight = false;
                ep.pending_trb = 0;
                return COMP_INVALID;
            }
            sleep_ms(1);
        }

        let ep = Self::ep_mut(&mut self.devices[index], ep);
        ep.done = false;
        ep.in_flight = false;
        ep.pending_trb = 0;
        ep.completion
    }

    /// One control transfer on a device's default pipe: the completion code,
    /// and how many bytes moved.
    fn control_transfer(&mut self, index: usize, request_type: u8, request: u8,
                        value: u16, wire_index: u16, length: u16) -> (u32, u32) {
        let dev = &mut self.devices[index];
        if !dev.ep0.ring.is_ready() || length as usize > PAGE_SIZE {
            return (COMP_INVALID, 0);
        }

        let dir_in = request_type & usb::DIR_IN != 0;
        let buf_phys = dev.buf_phys();
        let slot_id = dev.slot_id;
        let dci = dev.ep0.dci;

        let ep = &mut dev.ep0;
        ep.residual = 0;
        ep.completion = COMP_INVALID;
        ep.done = false;
        ep.data_trb = 0;

        let setup = (request_type as u64)
            | ((request as u64) << 8)
            | ((value as u64) << 16)
            | ((wire_index as u64) << 32)
            | ((length as u64) << 48);

        let trt = if length == 0 {
            TRT_NO_DATA
        } else if dir_in {
            TRT_IN_DATA
        } else {
            TRT_OUT_DATA
        };

        ep.ring.push(setup, 8, trb_type_field(TRB_SETUP_STAGE) | TRB_IDT | (trt << 16));

        if length > 0 {
            let mut control = trb_type_field(TRB_DATA_STAGE) | TRB_ISP;
            if dir_in {
                control |= TRB_DIR_IN;
            }
            ep.data_trb = ep.ring.push(buf_phys, length as u32, control);
        }

        /* The status stage runs opposite to the data stage (IN when there was
         * no data), and carries the IOC that raises the completion event. */
        let mut status_control = trb_type_field(TRB_STATUS_STAGE) | TRB_IOC;
        if length == 0 || !dir_in {
            status_control |= TRB_DIR_IN;
        }

        let status_trb = ep.ring.push(0, 0, status_control);
        if status_trb == 0 {
            return (COMP_INVALID, 0);
        }

        ep.pending_trb = status_trb;
        ep.in_flight = true;

        self.doorbell(slot_id, dci as u32);

        let code = self.wait_transfer(index, Ep::Control, status_trb, TRANSFER_TIMEOUT_MS);

        let residual = self.devices[index].ep0.residual;
        let done = (length as u32).saturating_sub(residual);
        (code, done)
    }
}

/* ---- enumeration ---- */

impl Controller {
    fn reset_port(&self, port: u8) -> bool {
        let mut v = self.port_read32(port, 0);

        /* USB3 ports train their link automatically and come up enabled */
        if v & PORTSC_ENABLED != 0 {
            return true;
        }

        /* Acknowledge stale change bits so the reset-done edge is
         * unambiguous */
        self.port_write32(port, 0, portsc_base(v) | (v & PORTSC_CHANGE_MASK));

        v = self.port_read32(port, 0);
        self.port_write32(port, 0, portsc_base(v) | PORTSC_RESET);

        let mut deadline = now_ms() + PORT_RESET_TIMEOUT_MS;
        loop {
            let cur = self.port_read32(port, 0);
            if cur == 0xFFFF_FFFF {
                return false;
            }
            if cur & PORTSC_RESET_CHANGE != 0 {
                self.port_write32(port, 0, portsc_base(cur) | PORTSC_RESET_CHANGE);
                break;
            }
            if now_ms() >= deadline {
                trace!(0, "Xhci: port {} reset timed out (portsc 0x{:X})", port, cur);
                return false;
            }
            sleep_ms(2);
        }

        /* USB 2.0 TRSTRCY: the device is unresponsive for 10 ms after reset */
        sleep_ms(10);

        let after = self.port_read32(port, 0);
        if after & PORTSC_ENABLED != 0 {
            return true;
        }

        /* A SuperSpeed link that failed to train needs a warm reset; the bit
         * is reserved on USB2 ports, where this write is a no-op. */
        trace!(0, "Xhci: port {} not enabled after hot reset (portsc 0x{:X}), \
trying warm reset", port, after);

        self.port_write32(port, 0, portsc_base(after) | PORTSC_WARM_RESET);

        deadline = now_ms() + PORT_RESET_TIMEOUT_MS;
        loop {
            let cur = self.port_read32(port, 0);
            if cur == 0xFFFF_FFFF {
                return false;
            }
            if cur & PORTSC_ENABLED != 0 {
                self.port_write32(port, 0, portsc_base(cur) | (cur & PORTSC_CHANGE_MASK));
                sleep_ms(10);
                return true;
            }
            if now_ms() >= deadline {
                trace!(0, "Xhci: port {} warm reset failed (portsc 0x{:X})", port, cur);
                return false;
            }
            sleep_ms(2);
        }
    }

    fn address_device(&mut self, index: usize, block_set_address: bool) -> bool {
        let body = self.input_body();
        let ep0_at = body + self.ep_ctx_offset(1);
        let dev = &mut self.devices[index];

        dev.clear_input_ctx();

        /* Input Control Context: add the slot context (A0) and EP0 (A1) */
        dev.write_ctx32(0, 0); /* drop flags */
        dev.write_ctx32(4, (1 << 0) | (1 << 1));

        let mut slot0 = (1u32 << 27)                        /* Context Entries = 1 */
            | (((dev.speed as u32) & 0xF) << 20)            /* Speed */
            | (dev.route & 0xFFFFF);                        /* Route String */
        if dev.mtt {
            slot0 |= 1 << 25;
        }
        dev.write_ctx32(body, slot0);
        dev.write_ctx32(body + 4, ((dev.root_port as u32) & 0xFF) << 16);
        /* TT Hub Slot ID and TT Port Number route a low- or full-speed
         * device's split transactions through the transaction translator of
         * the nearest high-speed hub; both stay zero on a root port. */
        dev.write_ctx32(body + 8, ((dev.tt_port as u32) << 8) | dev.tt_slot as u32);
        dev.write_ctx32(body + 12, 0);

        let deq = dev.ep0.ring.phys() | (dev.ep0.ring.cycle() as u64 & 1);
        dev.write_ctx32(ep0_at, 0);
        dev.write_ctx32(ep0_at + 4,
            (EP_TYPE_CONTROL << 3)                          /* EP Type = Control */
                | (3 << 1)                                  /* CErr = 3 */
                | ((dev.ep0.max_packet as u32) << 16));
        dev.write_ctx32(ep0_at + 8, deq as u32);
        dev.write_ctx32(ep0_at + 12, (deq >> 32) as u32);
        dev.write_ctx32(ep0_at + 16, 8);                    /* Average TRB Length */

        let mut control = trb_type_field(TRB_ADDRESS_DEVICE) | trb_slot_field(dev.slot_id);
        if block_set_address {
            control |= 1 << 9; /* BSR */
        }
        let (input_phys, slot_id) = (dev.input_ctx_phys(), dev.slot_id);

        dma_wmb();

        let (code, _) = self.run_command(input_phys, 0, control);
        if code != COMP_SUCCESS {
            trace!(0, "Xhci: address device slot {} failed, code {}", slot_id, code);
            return false;
        }
        true
    }

    /// Walk a configuration descriptor for a boot keyboard's interrupt-IN
    /// endpoint. True when one was found and recorded.
    fn parse_configuration(&mut self, index: usize, length: u32) -> bool {
        let speed = self.devices[index].speed;
        let buf = self.devices[index].buf();
        let length = (length as usize).min(buf.len());

        let mut in_boot_keyboard = false;
        let mut keyboard_interface = 0u8;
        let mut found: Option<(u8, u8, u16, u8, u8)> = None;

        let mut off = 0;
        while off + 2 <= length {
            let len = usb::desc_len(&buf[off..]) as usize;
            let kind = usb::desc_type(&buf[off..]);

            if len < 2 || off + len > length {
                break;
            }
            let desc = &buf[off..off + len];

            if kind == usb::DESC_INTERFACE && len >= 9 {
                in_boot_keyboard = usb::interface::class(desc) == usb::CLASS_HID
                    && usb::interface::subclass(desc) == usb::SUBCLASS_BOOT
                    && usb::interface::protocol(desc) == usb::PROTOCOL_KEYBOARD;

                if in_boot_keyboard {
                    keyboard_interface = usb::interface::number(desc);
                }

                trace!(USB_LL, "Xhci: interface {} class {}/{}/{}",
                    usb::interface::number(desc), usb::interface::class(desc),
                    usb::interface::subclass(desc), usb::interface::protocol(desc));
            } else if kind == usb::DESC_ENDPOINT && len >= 7 {
                let attrs = usb::endpoint::attributes(desc);
                let address = usb::endpoint::address(desc);
                let is_intr_in = attrs & usb::endpoint::XFER_MASK
                        == usb::endpoint::XFER_INTERRUPT
                    && address & usb::endpoint::DIR_IN != 0;

                if in_boot_keyboard && is_intr_in && found.is_none() {
                    let num = address & usb::endpoint::NUM_MASK;
                    found = Some((
                        address,
                        num * 2 + 1,
                        usb::endpoint::max_packet(desc) & 0x7FF,
                        encode_interval(speed, usb::endpoint::interval(desc)),
                        keyboard_interface,
                    ));
                }
            }

            off += len;
        }

        let (address, dci, max_packet, interval, interface) = match found {
            Some(found) => found,
            None => return false,
        };

        let dev = &mut self.devices[index];
        if dev.intr_in.dci != 0 {
            return dev.is_keyboard;
        }

        dev.intr_in.address = address;
        dev.intr_in.dci = dci;
        dev.intr_in.max_packet = max_packet;
        dev.intr_in.interval = interval;
        dev.interface_num = interface;
        dev.is_keyboard = true;

        trace!(0, "Xhci: boot keyboard on interface {} ep 0x{:X} mps {} interval {} dci {}",
            interface, address, max_packet, interval, dci);
        true
    }

    fn configure_keyboard(&mut self, index: usize) -> bool {
        /* Configure Endpoint adds the interrupt-IN pipe. The slot context has
         * to be resubmitted with Context Entries raised to cover the new DCI,
         * seeded from the controller's own output context. */
        let body = self.input_body();
        let ctx_size = self.context_size as usize;
        let dci = self.devices[index].intr_in.dci;
        let ep_at = body + self.ep_ctx_offset(dci);

        let dev = &mut self.devices[index];
        dev.clear_input_ctx();
        dev.write_ctx32(0, 0);
        dev.write_ctx32(4, (1 << 0) | (1 << dci));

        /* The slot context is written by the controller; order the copy after
         * the command completion that published it. */
        dma_rmb();

        let mut slot_copy = [0u8; 64];
        slot_copy[..ctx_size].copy_from_slice(&dev.dev_ctx()[..ctx_size]);
        if let Some(dma) = dev.dma.as_mut() {
            dma.input_ctx.as_mut_slice()[body..body + ctx_size]
                .copy_from_slice(&slot_copy[..ctx_size]);
        }

        let slot0 = Device::read_ctx32(&slot_copy, 0);
        dev.write_ctx32(body, (slot0 & !(0x1F << 27)) | ((dci as u32) << 27));
        /* Slot State and Device Address are controller-owned; the input copy
         * must present them as zero. */
        dev.write_ctx32(body + 12, 0);

        let max_esit = dev.intr_in.max_packet as u32;
        let deq = dev.intr_in.ring.phys() | (dev.intr_in.ring.cycle() as u64 & 1);

        dev.write_ctx32(ep_at, ((dev.intr_in.interval as u32) << 16) | ((max_esit >> 16) << 24));
        dev.write_ctx32(ep_at + 4,
            (EP_TYPE_INTERRUPT_IN << 3) | (3 << 1) | ((dev.intr_in.max_packet as u32) << 16));
        dev.write_ctx32(ep_at + 8, deq as u32);
        dev.write_ctx32(ep_at + 12, (deq >> 32) as u32);
        dev.write_ctx32(ep_at + 16, dev.intr_in.max_packet as u32 | ((max_esit & 0xFFFF) << 16));

        let (input_phys, slot_id) = (dev.input_ctx_phys(), dev.slot_id);

        dma_wmb();

        let (code, _) = self.run_command(input_phys, 0,
            trb_type_field(TRB_CONFIGURE_ENDPOINT) | trb_slot_field(slot_id));
        if code != COMP_SUCCESS {
            trace!(0, "Xhci: configure endpoint slot {} failed, code {}", slot_id, code);
            return false;
        }
        true
    }

    fn enumerate_port(&mut self, port: u8) -> bool {
        let portsc = self.port_read32(port, 0);
        if portsc & PORTSC_CONNECTED == 0 {
            return false;
        }

        if !self.reset_port(port) {
            return false;
        }

        let portsc = self.port_read32(port, 0);
        let speed = ((portsc >> PORTSC_SPEED_SHIFT) & PORTSC_SPEED_MASK) as u8;

        trace!(0, "Xhci: port {} connected, {} speed", port, speed_name(speed));

        let at = port as usize;
        shared::update(self.index, |s| s.ports[at].speed = speed);

        self.enumerate_device(Attach {
            root_port: port,
            speed,
            ..Attach::default()
        })
        .is_some()
    }
}

impl Controller {
    /// Address a freshly reset device, read enough of its descriptors to
    /// classify it, and keep it only if it is something this driver drives: a
    /// boot-protocol keyboard, or a hub whose downstream ports may hold one.
    /// Anything else has its slot released again immediately, so a machine
    /// full of USB devices does not exhaust the device records.
    ///
    /// Answers with the index of the device kept, or None if nothing was.
    fn enumerate_device(&mut self, info: Attach) -> Option<usize> {
        let index = match self.alloc_device() {
            Some(index) => index,
            None => {
                trace!(0, "Xhci: no free device record for root port {} route 0x{:X}",
                    info.root_port, info.route);
                return None;
            }
        };

        {
            let dev = &mut self.devices[index];
            dev.reset();
            dev.in_use = true;
            dev.root_port = info.root_port;
            dev.speed = info.speed;
            dev.route = info.route;
            dev.tier = info.tier;
            dev.tt_slot = info.tt_slot;
            dev.tt_port = info.tt_port;
            dev.mtt = info.mtt;
            dev.parent_slot = info.parent_slot;
            dev.parent_port = info.parent_port;
            dev.ep0.dci = 1;
            dev.ep0.max_packet = usb::default_max_packet0(info.speed);
        }

        let (code, slot_id) = self.run_command(0, 0, trb_type_field(TRB_ENABLE_SLOT));
        if code != COMP_SUCCESS || slot_id == 0 || slot_id > self.max_slots {
            trace!(0, "Xhci: enable slot failed for root port {}, code {}", info.root_port, code);
            self.devices[index].reset();
            return None;
        }
        self.devices[index].slot_id = slot_id;

        if !self.devices[index].alloc_dma() {
            trace!(0, "Xhci: out of memory for slot {}", slot_id);
            self.release_device(index);
            return None;
        }

        let dev_ctx_phys = self.devices[index].dev_ctx_phys();
        self.dcbaa_set(slot_id, dev_ctx_phys);
        dma_wmb();

        if !self.address_device(index, false) {
            self.release_device(index);
            return None;
        }

        /* Low and full speed devices only reveal their real EP0 packet size
         * in the first eight bytes of the device descriptor. */
        let (code, got) = self.control_transfer(index,
            usb::DIR_IN | usb::TYPE_STANDARD | usb::RECIP_DEVICE,
            usb::REQ_GET_DESCRIPTOR, (usb::DESC_DEVICE as u16) << 8, 0, 8);
        if code != COMP_SUCCESS || got < 8 {
            trace!(0, "Xhci: short device descriptor on slot {}, code {} got {}",
                slot_id, code, got);
            self.release_device(index);
            return None;
        }

        let raw_mps0 = usb::device::max_packet0(self.devices[index].buf());
        let real_mps = if info.speed == usb::SPEED_SUPER || info.speed == usb::SPEED_SUPER_PLUS {
            1u16 << raw_mps0
        } else {
            raw_mps0 as u16
        };

        if real_mps != 0 && real_mps != self.devices[index].ep0.max_packet {
            trace!(USB_LL, "Xhci: slot {} ep0 mps {} -> {}",
                slot_id, self.devices[index].ep0.max_packet, real_mps);
            self.devices[index].ep0.max_packet = real_mps;
            self.evaluate_ep0(index, real_mps);
        }

        let (code, got) = self.control_transfer(index,
            usb::DIR_IN | usb::TYPE_STANDARD | usb::RECIP_DEVICE,
            usb::REQ_GET_DESCRIPTOR, (usb::DESC_DEVICE as u16) << 8, 0,
            usb::DEVICE_DESC_LEN as u16);
        if code != COMP_SUCCESS || (got as usize) < usb::DEVICE_DESC_LEN {
            trace!(0, "Xhci: device descriptor read failed on slot {}, code {}", slot_id, code);
            self.release_device(index);
            return None;
        }

        {
            let buf = self.devices[index].buf();
            let (vendor, product, class) = (usb::device::vendor_id(buf),
                usb::device::product_id(buf), usb::device::device_class(buf));
            let dev = &mut self.devices[index];
            dev.vendor_id = vendor;
            dev.product_id = product;
            dev.device_class = class;
        }

        let dev_class = self.devices[index].device_class;
        trace!(0, "Xhci: slot {} device {:04x}:{:04x} class {} route 0x{:X} tier {}",
            slot_id, self.devices[index].vendor_id, self.devices[index].product_id,
            dev_class, info.route, info.tier);

        if info.tier == 0 && (info.root_port as usize) < MAX_PORTS {
            let (at, vendor, product) = (info.root_port as usize,
                self.devices[index].vendor_id, self.devices[index].product_id);
            shared::update(self.index, |s| {
                s.ports[at].vendor_id = vendor;
                s.ports[at].product_id = product;
                s.ports[at].device_class = dev_class;
            });
        }

        /* The configuration descriptor: the header first for its total
         * length, then the whole thing so the interface and endpoint
         * descriptors can be walked. */
        let (code, got) = self.control_transfer(index,
            usb::DIR_IN | usb::TYPE_STANDARD | usb::RECIP_DEVICE,
            usb::REQ_GET_DESCRIPTOR, (usb::DESC_CONFIGURATION as u16) << 8, 0,
            usb::CONFIG_DESC_LEN as u16);
        if code != COMP_SUCCESS || (got as usize) < usb::CONFIG_DESC_LEN {
            trace!(0, "Xhci: config descriptor header failed, code {}", code);
            self.release_device(index);
            return None;
        }

        let (mut total_length, config_value) = {
            let buf = self.devices[index].buf();
            (usb::config::total_length(buf), usb::config::configuration_value(buf))
        };
        if total_length as usize > PAGE_SIZE {
            total_length = PAGE_SIZE as u16;
        }

        let (code, got) = self.control_transfer(index,
            usb::DIR_IN | usb::TYPE_STANDARD | usb::RECIP_DEVICE,
            usb::REQ_GET_DESCRIPTOR, (usb::DESC_CONFIGURATION as u16) << 8, 0, total_length);
        if code != COMP_SUCCESS || (got as usize) < usb::CONFIG_DESC_LEN {
            trace!(0, "Xhci: config descriptor read failed, code {}", code);
            self.release_device(index);
            return None;
        }

        let is_keyboard = self.parse_configuration(index, got);
        let is_hub = dev_class == usb::CLASS_HUB && info.tier < MAX_TIER;

        if !is_keyboard && !is_hub {
            trace!(0, "Xhci: slot {} is neither keyboard nor hub, releasing", slot_id);
            self.release_device(index);
            return None;
        }

        let (code, _) = self.control_transfer(index,
            usb::DIR_OUT | usb::TYPE_STANDARD | usb::RECIP_DEVICE,
            usb::REQ_SET_CONFIGURATION, config_value as u16, 0, 0);
        if code != COMP_SUCCESS {
            trace!(0, "Xhci: set configuration {} failed, code {}", config_value, code);
            self.release_device(index);
            return None;
        }

        if is_keyboard {
            return self.finish_keyboard(index, info);
        }

        if !self.setup_hub(index) {
            self.release_device(index);
            return None;
        }

        self.scan_hub_ports(index);

        /* An empty hub is kept anyway: its slot is what makes a keyboard
         * plugged into it later reachable, and scan_ports walks known hubs
         * every rescan. */

        if info.tier == 0 && (info.root_port as usize) < MAX_PORTS {
            let at = info.root_port as usize;
            shared::update(self.index, |s| s.ports[at].slot_id = slot_id);
        }

        Some(index)
    }

    /// Evaluate Context updates EP0's packet size without disturbing the
    /// address the device has already accepted.
    fn evaluate_ep0(&mut self, index: usize, mps: u16) {
        let body = self.input_body();
        let ep0_at = body + self.ep_ctx_offset(1);

        let dev = &mut self.devices[index];
        dev.clear_input_ctx();
        dev.write_ctx32(0, 0);
        dev.write_ctx32(4, 1 << 1);
        dev.write_ctx32(ep0_at + 4,
            (EP_TYPE_CONTROL << 3) | (3 << 1) | ((mps as u32) << 16));

        let (input_phys, slot_id) = (dev.input_ctx_phys(), dev.slot_id);

        dma_wmb();

        let (code, _) = self.run_command(input_phys, 0,
            trb_type_field(TRB_EVALUATE_CONTEXT) | trb_slot_field(slot_id));
        if code != COMP_SUCCESS {
            trace!(0, "Xhci: evaluate context slot {} failed, code {}", slot_id, code);
        }
    }

    fn finish_keyboard(&mut self, index: usize, info: Attach) -> Option<usize> {
        if !self.configure_keyboard(index) {
            self.release_device(index);
            return None;
        }

        let (slot_id, interface) =
            (self.devices[index].slot_id, self.devices[index].interface_num as u16);

        /* Boot protocol gives the fixed 8-byte report this driver decodes; a
         * keyboard that only speaks report protocol would need a descriptor
         * parser. Both of these are optional requests -- a device is allowed
         * to STALL them -- so failures are logged, not fatal. */
        let (code, _) = self.control_transfer(index,
            usb::DIR_OUT | usb::TYPE_CLASS | usb::RECIP_INTERFACE,
            usb::REQ_SET_PROTOCOL, usb::HID_PROTOCOL_BOOT, interface, 0);
        if code != COMP_SUCCESS {
            trace!(0, "Xhci: set protocol(boot) failed, code {}", code);
        }

        let (code, _) = self.control_transfer(index,
            usb::DIR_OUT | usb::TYPE_CLASS | usb::RECIP_INTERFACE,
            usb::REQ_SET_IDLE, 0, interface, 0);
        if code != COMP_SUCCESS {
            trace!(USB_LL, "Xhci: set idle failed, code {}", code);
        }

        if info.tier == 0 && (info.root_port as usize) < MAX_PORTS {
            let at = info.root_port as usize;
            shared::update(self.index, |s| {
                s.ports[at].slot_id = slot_id;
                s.ports[at].keyboard = true;
            });
        }

        self.publish_keyboard(index);

        if !self.submit_report(index) {
            self.release_device(index);
            return None;
        }

        trace!(0, "Xhci: keyboard ready on root port {} slot {} route 0x{:X}",
            info.root_port, slot_id, info.route);
        Some(index)
    }

    fn dcbaa_set(&mut self, slot: u8, phys: u64) {
        if let Some(dcbaa) = self.dcbaa.as_mut() {
            let at = slot as usize * 8;
            if at + 8 <= dcbaa.as_slice().len() {
                dcbaa.as_mut_slice()[at..at + 8].copy_from_slice(&phys.to_le_bytes());
            }
        }
    }

    /// What `usb` should say about this keyboard from now on.
    fn publish_keyboard(&self, index: usize) {
        let dev = &self.devices[index];
        let (live, slot_id, ep_address, reports, errors) =
            (dev.in_use && dev.is_keyboard, dev.slot_id, dev.intr_in.address,
             dev.reports, dev.errors);
        shared::update(self.index, |s| {
            s.keyboards[index] = crate::shared::KeyboardRecord {
                live, slot_id, ep_address, reports, errors,
            };
        });
    }
}

/* ---- hubs ---- */

impl Controller {
    fn hub_port_status(&mut self, hub: usize, port: u8) -> Option<(u16, u16)> {
        let (code, got) = self.control_transfer(hub,
            usb::DIR_IN | usb::TYPE_CLASS | usb::RECIP_OTHER,
            0x00 /* GET_STATUS */, 0, port as u16, 4);
        if code != COMP_SUCCESS || got < 4 {
            trace!(USB_LL, "Xhci: hub slot {} port {} status failed, code {}",
                self.devices[hub].slot_id, port, code);
            return None;
        }

        /* The hub wrote this through DMA; order the reads after the
         * completion. */
        dma_rmb();

        let buf = self.devices[hub].buf();
        Some((u16::from_le_bytes([buf[0], buf[1]]), u16::from_le_bytes([buf[2], buf[3]])))
    }

    fn hub_set_port_feature(&mut self, hub: usize, port: u8, feature: u16) -> bool {
        let (code, _) = self.control_transfer(hub,
            usb::DIR_OUT | usb::TYPE_CLASS | usb::RECIP_OTHER,
            usb::REQ_SET_FEATURE, feature, port as u16, 0);
        code == COMP_SUCCESS
    }

    fn hub_clear_port_feature(&mut self, hub: usize, port: u8, feature: u16) -> bool {
        let (code, _) = self.control_transfer(hub,
            usb::DIR_OUT | usb::TYPE_CLASS | usb::RECIP_OTHER,
            usb::REQ_CLEAR_FEATURE, feature, port as u16, 0);
        code == COMP_SUCCESS
    }

    fn hub_reset_port(&mut self, hub: usize, port: u8) -> Option<u8> {
        if !self.hub_set_port_feature(hub, port, usb::HUB_FEATURE_PORT_RESET) {
            trace!(0, "Xhci: hub slot {} port {} reset request failed",
                self.devices[hub].slot_id, port);
            return None;
        }

        let deadline = now_ms() + PORT_RESET_TIMEOUT_MS;
        loop {
            let (status, change) = self.hub_port_status(hub, port)?;

            if change & usb::HUB_PORT_CHANGE_RESET != 0
                || (status & usb::HUB_PORT_STATUS_RESET == 0
                    && status & usb::HUB_PORT_STATUS_ENABLE != 0)
            {
                break;
            }

            if now_ms() >= deadline {
                trace!(0, "Xhci: hub slot {} port {} reset timed out (status 0x{:X})",
                    self.devices[hub].slot_id, port, status);
                return None;
            }
            sleep_ms(10);
        }

        self.hub_clear_port_feature(hub, port, usb::HUB_FEATURE_C_PORT_RESET);

        /* USB 2.0 TRSTRCY */
        sleep_ms(10);

        let (status, _) = self.hub_port_status(hub, port)?;
        if status & usb::HUB_PORT_STATUS_ENABLE == 0 {
            trace!(0, "Xhci: hub slot {} port {} not enabled after reset (0x{:X})",
                self.devices[hub].slot_id, port, status);
            return None;
        }

        let hub_speed = self.devices[hub].speed;
        Some(if hub_speed == usb::SPEED_SUPER || hub_speed == usb::SPEED_SUPER_PLUS {
            usb::SPEED_SUPER
        } else if status & usb::HUB_PORT_STATUS_LOW_SPEED != 0 {
            usb::SPEED_LOW
        } else if status & usb::HUB_PORT_STATUS_HIGH_SPEED != 0 {
            usb::SPEED_HIGH
        } else {
            usb::SPEED_FULL
        })
    }

    /// Read the hub class descriptor, tell the controller this slot is a hub
    /// (the Hub, Number of Ports and TTT fields are only evaluated by a
    /// Configure Endpoint command), then switch power on to every downstream
    /// port.
    fn setup_hub(&mut self, hub: usize) -> bool {
        let speed = self.devices[hub].speed;
        let desc_type = if speed == usb::SPEED_SUPER || speed == usb::SPEED_SUPER_PLUS {
            usb::DESC_HUB_SUPER_SPEED
        } else {
            usb::DESC_HUB
        };

        let (code, got) = self.control_transfer(hub,
            usb::DIR_IN | usb::TYPE_CLASS | usb::RECIP_DEVICE,
            usb::REQ_GET_DESCRIPTOR, (desc_type as u16) << 8, 0, 9);
        if code != COMP_SUCCESS || (got as usize) < usb::HUB_DESC_MIN_LENGTH {
            trace!(0, "Xhci: hub slot {} descriptor read failed, code {} got {}",
                self.devices[hub].slot_id, code, got);
            return false;
        }

        dma_rmb();

        let (mut num_ports, characteristics, power_on) = {
            let buf = self.devices[hub].buf();
            (buf[usb::HUB_DESC_NUM_PORTS],
             u16::from_le_bytes([buf[usb::HUB_DESC_CHARACTERISTICS],
                                 buf[usb::HUB_DESC_CHARACTERISTICS + 1]]),
             buf[usb::HUB_DESC_POWER_ON_DELAY])
        };

        /* The route string gives each tier one nibble, so port numbers above
         * 15 are unreachable however many the hub reports. */
        if num_ports > 15 {
            num_ports = 15;
        }
        if num_ports == 0 {
            trace!(0, "Xhci: hub slot {} reports no ports", self.devices[hub].slot_id);
            return false;
        }

        {
            let dev = &mut self.devices[hub];
            dev.is_hub = true;
            dev.hub_ports = num_ports;
            dev.hub_ttt = ((characteristics >> 5) & 0x3) as u8;
            dev.hub_power_on_delay_ms = power_on as u16 * 2;
        }

        trace!(0, "Xhci: hub slot {}: {} ports, ttt {}, power-on {} ms",
            self.devices[hub].slot_id, num_ports, self.devices[hub].hub_ttt,
            self.devices[hub].hub_power_on_delay_ms);

        let body = self.input_body();
        let ctx_size = self.context_size as usize;
        let dev = &mut self.devices[hub];

        dev.clear_input_ctx();
        dev.write_ctx32(0, 0);
        dev.write_ctx32(4, 1 << 0); /* the slot context only */

        /* Seed from the controller's own output context so nothing else
         * changes. */
        dma_rmb();

        let mut slot_copy = [0u8; 64];
        slot_copy[..ctx_size].copy_from_slice(&dev.dev_ctx()[..ctx_size]);
        if let Some(dma) = dev.dma.as_mut() {
            dma.input_ctx.as_mut_slice()[body..body + ctx_size]
                .copy_from_slice(&slot_copy[..ctx_size]);
        }

        let slot0 = Device::read_ctx32(&slot_copy, 0);
        let slot1 = Device::read_ctx32(&slot_copy, 4);
        let slot2 = Device::read_ctx32(&slot_copy, 8);
        dev.write_ctx32(body, slot0 | (1 << 26)); /* Hub */
        dev.write_ctx32(body + 4, (slot1 & 0x00FF_FFFF) | ((num_ports as u32) << 24));
        dev.write_ctx32(body + 8, (slot2 & !(3 << 16)) | ((dev.hub_ttt as u32) << 16));
        /* Slot State and Device Address are controller-owned */
        dev.write_ctx32(body + 12, 0);

        let (input_phys, slot_id) = (dev.input_ctx_phys(), dev.slot_id);

        dma_wmb();

        let (code, _) = self.run_command(input_phys, 0,
            trb_type_field(TRB_CONFIGURE_ENDPOINT) | trb_slot_field(slot_id));
        if code != COMP_SUCCESS {
            trace!(0, "Xhci: marking slot {} as a hub failed, code {}", slot_id, code);
            return false;
        }

        let mut any_powered = false;
        for p in 1..=num_ports {
            if self.hub_set_port_feature(hub, p, usb::HUB_FEATURE_PORT_POWER) {
                any_powered = true;
            }
        }

        if !any_powered {
            trace!(0, "Xhci: hub slot {} refused port power", slot_id);
            return false;
        }

        /* bPwrOn2PwrGood plus the attach debounce before status means
         * anything. */
        sleep_ms(self.devices[hub].hub_power_on_delay_ms as u64 + 100);
        true
    }

    fn scan_hub_ports(&mut self, hub: usize) {
        for p in 1..=self.devices[hub].hub_ports {
            let (status, change) = match self.hub_port_status(hub, p) {
                Some(answer) => answer,
                None => continue,
            };

            if change & usb::HUB_PORT_CHANGE_CONNECTION != 0 {
                self.hub_clear_port_feature(hub, p, usb::HUB_FEATURE_C_PORT_CONNECTION);
            }

            let connected = status & usb::HUB_PORT_STATUS_CONNECTION != 0;
            let known = self.devices[hub].hub_enumerated_mask & (1 << p) != 0;

            if !connected {
                if known {
                    self.devices[hub].hub_enumerated_mask &= !(1 << p);

                    let hub_slot = self.devices[hub].slot_id;
                    for i in 0..MAX_DEVICES {
                        if self.devices[i].in_use
                            && self.devices[i].parent_slot == hub_slot
                            && self.devices[i].parent_port == p
                        {
                            trace!(0, "Xhci: hub slot {} port {} disconnected", hub_slot, p);
                            self.release_device(i);
                        }
                    }
                }
                continue;
            }

            if known {
                continue;
            }

            /* Attempted, successfully or not: only an unplug re-arms this
             * port. */
            self.devices[hub].hub_enumerated_mask |= 1 << p;

            let child_speed = match self.hub_reset_port(hub, p) {
                Some(speed) => speed,
                None => continue,
            };

            let h = &self.devices[hub];
            let mut info = Attach {
                root_port: h.root_port,
                speed: child_speed,
                route: h.route | ((p as u32 & 0xF) << (4 * h.tier)),
                tier: h.tier + 1,
                parent_slot: h.slot_id,
                parent_port: p,
                ..Attach::default()
            };

            /* Split transactions terminate at the nearest high-speed hub;
             * deeper low- and full-speed hubs inherit that hub's
             * translator. */
            if h.speed == usb::SPEED_HIGH
                && (child_speed == usb::SPEED_LOW || child_speed == usb::SPEED_FULL)
            {
                info.tt_slot = h.slot_id;
                info.tt_port = p;
                info.mtt = h.mtt;
            } else {
                info.tt_slot = h.tt_slot;
                info.tt_port = h.tt_port;
                info.mtt = false;
            }

            trace!(0, "Xhci: hub slot {} port {}: {} speed device",
                h.slot_id, p, speed_name(child_speed));

            self.enumerate_device(info);
        }
    }

    fn release_device(&mut self, index: usize) {
        /* Children first: a device behind this hub is unreachable once the
         * hub's slot is gone, and its own slot would leak. */
        let slot = self.devices[index].slot_id;
        if slot != 0 {
            for i in 0..MAX_DEVICES {
                if i != index && self.devices[i].in_use && self.devices[i].parent_slot == slot {
                    self.release_device(i);
                }
            }

            self.run_command(0, 0, trb_type_field(TRB_DISABLE_SLOT) | trb_slot_field(slot));
            if slot <= self.max_slots {
                self.dcbaa_set(slot, 0);
            }
        }

        let port = self.devices[index].root_port;
        let tier = self.devices[index].tier;

        self.devices[index].reset();
        self.publish_keyboard(index);

        /* Only a root-port device owns the port record */
        if tier != 0 {
            return;
        }
        if port != 0 && (port as usize) < MAX_PORTS {
            let at = port as usize;
            shared::update(self.index, |s| {
                s.ports[at].enumerated = false;
                s.ports[at].slot_id = 0;
                s.ports[at].keyboard = false;
            });
        }
    }

    fn scan_ports(&mut self) {
        self.port_change_pending = false;

        /* Hubs do not raise root-port events for their own downstream ports,
         * so walk the hubs already known on every scan. */
        for i in 0..MAX_DEVICES {
            if self.devices[i].in_use && self.devices[i].is_hub {
                self.scan_hub_ports(i);
            }
        }

        for port in 1..=self.num_ports.min(MAX_PORTS as u8 - 1) {
            let v = self.port_read32(port, 0);
            if v == 0xFFFF_FFFF {
                continue;
            }

            if v & PORTSC_CHANGE_MASK != 0 {
                self.port_write32(port, 0, portsc_base(v) | (v & PORTSC_CHANGE_MASK));
            }

            let connected = v & PORTSC_CONNECTED != 0;
            let at = port as usize;
            let mut was_enumerated = false;
            shared::update(self.index, |s| {
                s.ports[at].connected = connected;
                was_enumerated = s.ports[at].enumerated;
            });

            if connected && !was_enumerated {
                self.enumerate_port(port);

                /* Attempted, successfully or not: only an unplug re-arms it */
                shared::update(self.index, |s| s.ports[at].enumerated = true);
            } else if !connected && was_enumerated {
                trace!(0, "Xhci: port {} disconnected", port);

                for i in 0..MAX_DEVICES {
                    if self.devices[i].in_use && self.devices[i].root_port == port {
                        self.release_device(i);
                    }
                }

                shared::update(self.index, |s| {
                    s.ports[at].enumerated = false;
                    s.ports[at].keyboard = false;
                    s.ports[at].slot_id = 0;
                });
            }
        }
    }
}

/* ---- keyboard servicing ---- */

/// The completion code a halted endpoint answers with.
const COMP_STALL_ERROR: u32 = 6;

impl Controller {
    fn submit_report(&mut self, index: usize) -> bool {
        let dev = &mut self.devices[index];
        if !dev.intr_in.ring.is_ready() || dev.intr_in.dci == 0 {
            return false;
        }

        let mut len = dev.intr_in.max_packet as usize;
        if len == 0 || len > 64 {
            len = crate::hid::REPORT_SIZE;
        }

        dev.report_buf()[..len].fill(0);

        let (report_phys, slot_id, dci) = (dev.report_phys(), dev.slot_id, dev.intr_in.dci);
        let ep = &mut dev.intr_in;
        ep.residual = 0;
        ep.completion = COMP_INVALID;
        ep.done = false;

        let trb = ep.ring.push(report_phys, len as u32,
            trb_type_field(TRB_NORMAL) | TRB_IOC | TRB_ISP);
        if trb == 0 {
            return false;
        }

        ep.data_trb = trb;
        ep.pending_trb = trb;
        ep.in_flight = true;

        self.doorbell(slot_id, dci as u32);
        true
    }

    /// An endpoint that came back halted: reset it, point the controller at
    /// where the producer actually is, and clear the device's own halt.
    fn recover_endpoint(&mut self, index: usize) {
        let dev = &self.devices[index];
        let (slot_id, dci, address) = (dev.slot_id, dev.intr_in.dci, dev.intr_in.address);

        trace!(0, "Xhci: keyboard slot {} endpoint stalled, resetting", slot_id);

        self.run_command(0, 0, trb_type_field(TRB_RESET_ENDPOINT)
            | ((dci as u32) << 16) | trb_slot_field(slot_id));

        /* Point the controller at the producer's current position, not at the
         * head: the ring behind the enqueue pointer holds consumed TRBs the
         * controller must not replay. */
        let ring = &self.devices[index].intr_in.ring;
        let deq = ring.enqueue_phys() | (ring.cycle() as u64 & 1);
        self.run_command(deq, 0, trb_type_field(TRB_SET_TR_DEQUEUE)
            | ((dci as u32) << 16) | trb_slot_field(slot_id));

        /* FEATURE_ENDPOINT_HALT is selector 0, on the endpoint itself. */
        const RECIP_ENDPOINT: u8 = 0x02;
        self.control_transfer(index, usb::DIR_OUT | usb::TYPE_STANDARD | RECIP_ENDPOINT,
            usb::REQ_CLEAR_FEATURE, 0, address as u16, 0);

        self.devices[index].kbd.reset();
    }

    fn pump_keyboards(&mut self) {
        let now = kcore::time::boot_time_ns();

        for index in 0..MAX_DEVICES {
            if !self.devices[index].in_use || !self.devices[index].is_keyboard {
                continue;
            }

            let (in_flight, done) =
                (self.devices[index].intr_in.in_flight, self.devices[index].intr_in.done);

            if in_flight && done {
                let dev = &mut self.devices[index];
                let code = dev.intr_in.completion;
                let len = (dev.intr_in.max_packet as u32)
                    .saturating_sub(dev.intr_in.residual) as usize;

                dev.intr_in.in_flight = false;
                dev.intr_in.done = false;
                dev.intr_in.pending_trb = 0;

                if code == COMP_SUCCESS || code == COMP_SHORT_PACKET {
                    dev.reports += 1;
                    let mut report = [0u8; crate::hid::REPORT_SIZE];
                    let take = len.min(report.len());
                    report[..take].copy_from_slice(&dev.report_buf()[..take]);
                    if take >= crate::hid::REPORT_SIZE {
                        dev.kbd.on_report(&report);
                    }
                    self.publish_keyboard(index);
                } else if code == COMP_STALL_ERROR {
                    dev.errors += 1;
                    self.recover_endpoint(index);
                    self.publish_keyboard(index);
                } else {
                    dev.errors += 1;
                    let slot = dev.slot_id;
                    trace!(USB_LL, "Xhci: keyboard slot {} transfer code {}", slot, code);
                    self.publish_keyboard(index);
                }
            }

            if !self.devices[index].intr_in.in_flight && !self.submit_report(index) {
                trace!(0, "Xhci: cannot resubmit report for slot {}",
                    self.devices[index].slot_id);
                continue;
            }

            self.devices[index].kbd.on_tick(now);
        }
    }

    pub fn poll(&mut self) {
        if !self.ready {
            return;
        }

        self.process_events();

        /* Hot-plug normally arrives as a Port Status Change event; the
         * periodic rescan is a backstop for controllers that drop one. */
        let now = now_ms();
        if self.port_change_pending || now - self.last_scan_ms >= PORT_RESCAN_PERIOD_MS {
            self.last_scan_ms = now;
            self.scan_ports();
        }

        self.pump_keyboards();
    }
}
