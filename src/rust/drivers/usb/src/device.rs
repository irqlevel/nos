//! What the driver keeps per device and per endpoint.

use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;

use crate::descriptors::SPEED_INVALID;
use crate::hid::BootKeyboard;
use crate::regs::COMP_INVALID;
use crate::ring::Ring;

pub struct Endpoint {
    pub ring: Ring,
    /// Device Context Index: 1 for EP0, `2 * n + dir` otherwise.
    pub dci: u8,
    /// bEndpointAddress; 0 for the default control pipe.
    pub address: u8,
    pub max_packet: u16,
    /// The xHCI-encoded interval.
    pub interval: u8,
    pub in_flight: bool,
    pub done: bool,
    pub completion: u32,
    pub residual: u32,
    /// The TRB whose completion ends the wait.
    pub pending_trb: u64,
    /// The TRB whose residual is the byte count.
    pub data_trb: u64,
}

impl Endpoint {
    pub const fn new() -> Self {
        Self {
            ring: Ring::new(),
            dci: 0,
            address: 0,
            max_packet: 0,
            interval: 0,
            in_flight: false,
            done: false,
            completion: COMP_INVALID,
            residual: 0,
            pending_trb: 0,
            data_trb: 0,
        }
    }

    pub fn clear(&mut self) {
        self.dci = 0;
        self.address = 0;
        self.max_packet = 0;
        self.interval = 0;
        self.in_flight = false;
        self.done = false;
        self.completion = COMP_INVALID;
        self.residual = 0;
        self.pending_trb = 0;
        self.data_trb = 0;
    }
}

/// Everything the slot context needs in order to say where a device sits in
/// the topology. A root-port device has route 0 and tier 0; a device behind a
/// hub inherits the root port and adds its own nibble to the route string.
#[derive(Clone, Copy, Default)]
pub struct Attach {
    /// The 1-based root hub port carrying this branch.
    pub root_port: u8,
    pub speed: u8,
    /// The xHCI route string, four bits per hub tier.
    pub route: u32,
    /// 0 when the device is directly on a root port.
    pub tier: u8,
    /// The slot of the nearest high-speed hub, 0 if there is none.
    pub tt_slot: u8,
    /// That hub's downstream port number.
    pub tt_port: u8,
    pub mtt: bool,
    /// The hub slot this device hangs off, 0 for a root port.
    pub parent_slot: u8,
    pub parent_port: u8,
}

/// The DMA a device needs. One page each: the input context (33 * 64 bytes
/// worst case), the device context (32 * 64), the control payload staging
/// buffer and the interrupt-IN landing zone. Page granularity satisfies the
/// 64-byte alignment and no-page-crossing rules for free.
pub struct DeviceDma {
    pub input_ctx: DmaBuffer,
    pub dev_ctx: DmaBuffer,
    pub buf: DmaBuffer,
    pub report: DmaBuffer,
}

impl DeviceDma {
    fn new() -> Option<Self> {
        let mut dma = Self {
            input_ctx: DmaBuffer::new(1)?,
            dev_ctx: DmaBuffer::new(1)?,
            buf: DmaBuffer::new(1)?,
            report: DmaBuffer::new(1)?,
        };
        dma.input_ctx.as_mut_slice().fill(0);
        dma.dev_ctx.as_mut_slice().fill(0);
        dma.buf.as_mut_slice().fill(0);
        dma.report.as_mut_slice().fill(0);
        Some(dma)
    }
}

pub struct Device {
    pub in_use: bool,
    pub slot_id: u8,
    /// The 1-based root hub port number.
    pub root_port: u8,
    pub speed: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub interface_num: u8,
    pub is_keyboard: bool,
    /// Interrupt-IN reports accepted, for the `usb` dump.
    pub reports: u32,
    /// Transfers that came back with a failure code.
    pub errors: u32,

    /* topology */
    pub route: u32,
    pub tier: u8,
    pub tt_slot: u8,
    pub tt_port: u8,
    pub mtt: bool,
    pub parent_slot: u8,
    pub parent_port: u8,

    /* hub state, meaningful when is_hub */
    pub is_hub: bool,
    pub hub_ports: u8,
    pub hub_ttt: u8,
    pub hub_power_on_delay_ms: u16,
    /// Downstream ports already looked at.
    pub hub_enumerated_mask: u32,

    pub ep0: Endpoint,
    pub intr_in: Endpoint,

    /// The boot-protocol decoder, used when `is_keyboard`.
    pub kbd: BootKeyboard,

    pub dma: Option<DeviceDma>,
}

impl Device {
    pub const fn new() -> Self {
        Self {
            in_use: false,
            slot_id: 0,
            root_port: 0,
            speed: SPEED_INVALID,
            vendor_id: 0,
            product_id: 0,
            device_class: 0,
            interface_num: 0,
            is_keyboard: false,
            reports: 0,
            errors: 0,
            route: 0,
            tier: 0,
            tt_slot: 0,
            tt_port: 0,
            mtt: false,
            parent_slot: 0,
            parent_port: 0,
            is_hub: false,
            hub_ports: 0,
            hub_ttt: 0,
            hub_power_on_delay_ms: 0,
            hub_enumerated_mask: 0,
            ep0: Endpoint::new(),
            intr_in: Endpoint::new(),
            kbd: BootKeyboard::new(),
            dma: None,
        }
    }

    pub fn alloc_dma(&mut self) -> bool {
        self.dma = match DeviceDma::new() {
            Some(dma) => Some(dma),
            None => return false,
        };
        self.ep0.ring.init() && self.intr_in.ring.init()
    }

    pub fn free_dma(&mut self) {
        self.ep0.ring.deinit();
        self.intr_in.ring.deinit();
        self.dma = None;
    }

    pub fn reset(&mut self) {
        self.free_dma();

        self.in_use = false;
        self.slot_id = 0;
        self.root_port = 0;
        self.speed = SPEED_INVALID;
        self.vendor_id = 0;
        self.product_id = 0;
        self.device_class = 0;
        self.interface_num = 0;
        self.is_keyboard = false;
        self.reports = 0;
        self.errors = 0;
        self.route = 0;
        self.tier = 0;
        self.tt_slot = 0;
        self.tt_port = 0;
        self.mtt = false;
        self.parent_slot = 0;
        self.parent_port = 0;
        self.is_hub = false;
        self.hub_ports = 0;
        self.hub_ttt = 0;
        self.hub_power_on_delay_ms = 0;
        self.hub_enumerated_mask = 0;
        self.ep0.clear();
        self.intr_in.clear();
        self.kbd.reset();
    }

    /* The DMA windows, as the code that fills them wants them. */

    pub fn input_ctx_phys(&self) -> u64 {
        self.dma.as_ref().map_or(0, |dma| dma.input_ctx.phys())
    }

    pub fn dev_ctx_phys(&self) -> u64 {
        self.dma.as_ref().map_or(0, |dma| dma.dev_ctx.phys())
    }

    pub fn buf_phys(&self) -> u64 {
        self.dma.as_ref().map_or(0, |dma| dma.buf.phys())
    }

    pub fn report_phys(&self) -> u64 {
        self.dma.as_ref().map_or(0, |dma| dma.report.phys())
    }

    pub fn input_ctx(&mut self) -> &mut [u8] {
        match self.dma.as_mut() {
            Some(dma) => dma.input_ctx.as_mut_slice(),
            None => &mut [],
        }
    }

    pub fn dev_ctx(&self) -> &[u8] {
        match self.dma.as_ref() {
            Some(dma) => dma.dev_ctx.as_slice(),
            None => &[],
        }
    }

    pub fn buf(&self) -> &[u8] {
        match self.dma.as_ref() {
            Some(dma) => dma.buf.as_slice(),
            None => &[],
        }
    }

    pub fn report_buf(&mut self) -> &mut [u8] {
        match self.dma.as_mut() {
            Some(dma) => dma.report.as_mut_slice(),
            None => &mut [],
        }
    }

    /// A dword of the input context, at a byte offset.
    pub fn write_ctx32(&mut self, at: usize, value: u32) {
        let ctx = self.input_ctx();
        if at + 4 <= ctx.len() {
            ctx[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    pub fn read_ctx32(ctx: &[u8], at: usize) -> u32 {
        if at + 4 <= ctx.len() {
            u32::from_le_bytes(ctx[at..at + 4].try_into().unwrap())
        } else {
            0
        }
    }

    pub fn clear_input_ctx(&mut self) {
        if let Some(dma) = self.dma.as_mut() {
            dma.input_ctx.as_mut_slice()[..PAGE_SIZE].fill(0);
        }
    }
}
