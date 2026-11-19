//! A PCIe root port of the guest's, one above each host's device it holds
//! (`devices::host`, plans/gpu-passthrough.md): the device's functions on
//! the bus below it at their own numbers -- a GPU's audio function at 00.1
//! beside it, where Linux's `hda_intel` looks for its GPU, a NIC's ports in
//! one slot, as its driver's link aggregation wants them -- and a link
//! above an endpoint, as on the machine.
//!
//! Nothing here reaches the device. The port is registers: the bus numbers
//! the guest gives it, by which the run loop routes configuration accesses
//! (`run::host_config`), and windows, a command register and a bridge
//! control that are the guest's to write and change nothing -- the device's
//! BARs are mapped where its own registers put them (`run::move_bar`), so a
//! window narrowed or a decode turned off leaves no access the device would
//! see, and a secondary bus reset resets nothing: a reset is the host's to
//! make (`host`'s module comment). Its link is the device's as it is now,
//! read from the device's own PCIe capability at each read (`Link`), and
//! offers no ASPM, no bandwidth notification and no slot: the guest's PCIe
//! code has no link to manage -- ASPM and the common clock would write the
//! device's link control, which is the host's. No interrupt of its own --
//! no MSI, no pin, no PME -- and no extended capability: Linux's port
//! driver binds and finds no service to start.

/// Red Hat's root port, QEMU's: Linux binds its port driver to it and
/// knows no quirk of it -- a port of a chipset's would have the guest apply
/// that chipset's.
pub const VENDOR: u16 = 0x1B36;
pub const DEVICE: u16 = 0x000C;
/// A PCI-to-PCI bridge, decoding positively.
const CLASS_BRIDGE: u32 = 0x06_04_00;
/// A type 1 header, of one function.
const HEADER_BRIDGE: u32 = 0x01;

/// The size of its configuration space, and of a dword's offsets in it;
/// with its extended space, as ECAM reaches it, 4 KiB.
const CONFIG_SIZE: usize = 256;
const DWORDS: usize = CONFIG_SIZE / 4;
const EXTENDED_SIZE: usize = 4096;

/* A type 1 header, by its dwords. */
const ID: usize = 0x00;
const COMMAND: usize = 0x04;
const CLASS: usize = 0x08;
const HEADER: usize = 0x0C;
const BUSES: usize = 0x18;
const MEMORY: usize = 0x20;
const PREFETCH: usize = 0x24;
const PREFETCH_BASE_UPPER: usize = 0x28;
const PREFETCH_LIMIT_UPPER: usize = 0x2C;
const CAPABILITIES: usize = 0x34;
/// The interrupt line and pin, and the bridge control above them.
const INTERRUPT: usize = 0x3C;

/// The command register's bits a guest may set: I/O, memory, bus master,
/// parity error response, SERR, INTx disable.
const COMMAND_GUEST: u32 = 0x0547;
/// Memory and bus mastering, as firmware leaves a port it enumerated.
const COMMAND_FIRMWARE: u32 = 0x0006;
/// The status register's "capabilities list".
const STATUS_CAP_LIST: u32 = 1 << 4;
/// The bus numbers' bytes, primary, secondary and subordinate; the
/// secondary latency timer above them is 0.
const BUSES_GUEST: u32 = 0x00FF_FFFF;
const SECONDARY_SHIFT: u32 = 8;
const SUBORDINATE_SHIFT: u32 = 16;
/// A window's base and limit registers: address bits 31:20 in bits 15:4,
/// in each half of the dword; a prefetchable one's low nibbles say 64-bit.
const WINDOW_BITS: u32 = 0xFFF0_FFF0;
const PREFETCH_64: u32 = 0x0001_0001;
/// A window as one closed: its base above its limit.
const WINDOW_CLOSED: u32 = 0x0000_FFF0;
const WINDOW_SHIFT: u32 = 16;
pub const WINDOW_ALIGN: u64 = 1 << 20;
/// The interrupt line, a byte; the bridge control's parity, SERR, ISA,
/// VGA, VGA 16-bit, master abort and secondary bus reset bits.
const INTERRUPT_GUEST: u32 = 0x007F_00FF;

/* Its PCIe capability, the only one: version 2, a root port, no slot. */
const PCIE: usize = 0x40;
const CAP_PCIE: u32 = 0x10;
const PCIE_FLAGS: u32 = 0x0042;
const DEVCAP: usize = PCIE + 0x04;
const DEVCTL: usize = PCIE + 0x08;
const LNKCAP: usize = PCIE + 0x0C;
const LNKCTL: usize = PCIE + 0x10;
const ROOTCTL: usize = PCIE + 0x1C;
const DEVCTL2: usize = PCIE + 0x28;
const LNKCAP2: usize = PCIE + 0x2C;
const LNKCTL2: usize = PCIE + 0x30;
/// Device capabilities: role-based error reporting.
const DEVCAP_ROLE_BASED: u32 = 1 << 15;
/// Device control's payload size field, the device's as it is set: the
/// guest's Linux finds port and device alike and sets neither; and the
/// error reporting and relaxed ordering bits the guest may write.
const PAYLOAD_SHIFT: u32 = 5;
const PAYLOAD_MASK: u16 = 0x7;
const DEVCTL_GUEST: u32 = 0x001F;
/// Link capabilities: the device's speed and width, data link layer active
/// reporting, and the port's number at the top.
const LINK_SPEED_WIDTH: u32 = 0x3FF;
const LNKCAP_DLL_ACTIVE: u32 = 1 << 20;
const LNKCAP_PORT_SHIFT: u32 = 24;
/// Link control's bits a guest may write -- ASPM control, the read
/// completion boundary, the common clock, extended synch, clock PM, the
/// autonomous width disable -- none of which reaches the link; link disable
/// and retrain read 0. Link status: the device's speed, width and slot
/// clock, the link active, never training.
const LNKCTL_GUEST: u32 = 0x03CB;
const LNKSTA_SLOT_CLOCK: u16 = 1 << 12;
const LNKSTA_DLL_ACTIVE: u16 = 1 << 13;
const LNKSTA_SHIFT: u32 = 16;
/// Root control's SERR and PME enables, which the guest may write; root
/// capabilities none -- no CRS visibility.
const ROOTCTL_GUEST: u32 = 0x000F;
/// Device control 2's completion timeout and its disable.
const DEVCTL2_GUEST: u32 = 0x001F;
/// Link capabilities 2's speeds, a bit each from bit 1; link control 2's
/// target speed, which the guest may write -- the device's most until it
/// does.
const SPEEDS_MASK: u32 = 0xFE;
const SPEED_MASK: u32 = 0xF;
const LNKCTL2_GUEST: u32 = 0x000F;

/// What the device below says of its link, as the port shows it: its own
/// PCIe capability's registers, read as the guest reads the port.
#[derive(Clone, Copy, Debug, Default)]
pub struct Link {
    /// Link capabilities: the device's most speed and width.
    pub cap: u32,
    /// Link status: the speed and width trained, and the slot clock.
    pub status: u16,
    /// Link capabilities 2: the speeds the device supports, a bit each from
    /// bit 1; 0 for a device of PCIe 2 or older, which has none.
    pub cap2: u32,
    /// The payload size the device is set to: device control's field.
    pub payload: u16,
}

impl Link {
    /// The speeds supported, a bit each, none past the most: the device's
    /// own list, or for one with none, every speed up to its most.
    fn speeds(&self) -> u32 {
        let most = self.cap & SPEED_MASK;
        let upto = ((1u32 << (most + 1)) - 2) & SPEEDS_MASK;
        match self.cap2 & SPEEDS_MASK {
            0 => upto,
            listed => listed & upto,
        }
    }
}

/// A root port: its configuration space, a dword each, as the guest has
/// written it; the dwords whose fields are the device's are made at each
/// read (`read`).
pub struct RootPort {
    config: [u32; DWORDS],
}

/// What of the dword at `dword` a guest may write.
const fn writable(dword: usize) -> u32 {
    match dword {
        COMMAND => COMMAND_GUEST,
        HEADER => 0xFF,
        BUSES => BUSES_GUEST,
        MEMORY | PREFETCH => WINDOW_BITS,
        PREFETCH_BASE_UPPER | PREFETCH_LIMIT_UPPER => u32::MAX,
        INTERRUPT => INTERRUPT_GUEST,
        DEVCTL => DEVCTL_GUEST,
        LNKCTL => LNKCTL_GUEST,
        ROOTCTL => ROOTCTL_GUEST,
        DEVCTL2 => DEVCTL2_GUEST,
        LNKCTL2 => LNKCTL2_GUEST,
        _ => 0,
    }
}

impl RootPort {
    /// Port `number`, its bus below numbered `secondary` as firmware would --
    /// on bus 0 above it, nothing past it -- its windows closed until they
    /// are set (`set_windows`), decoding and mastering on.
    pub fn new(number: u8, secondary: u8) -> RootPort {
        let mut config = [0u32; DWORDS];
        config[ID / 4] = u32::from(VENDOR) | (u32::from(DEVICE) << 16);
        config[COMMAND / 4] = COMMAND_FIRMWARE | (STATUS_CAP_LIST << 16);
        config[CLASS / 4] = CLASS_BRIDGE << 8;
        config[HEADER / 4] = HEADER_BRIDGE << 16;
        config[BUSES / 4] = (u32::from(secondary) << SECONDARY_SHIFT) | (u32::from(secondary) << SUBORDINATE_SHIFT);
        config[MEMORY / 4] = WINDOW_CLOSED;
        config[PREFETCH / 4] = WINDOW_CLOSED | PREFETCH_64;
        config[CAPABILITIES / 4] = PCIE as u32;
        config[PCIE / 4] = CAP_PCIE | (PCIE_FLAGS << 16);
        config[LNKCAP / 4] = u32::from(number) << LNKCAP_PORT_SHIFT;
        RootPort { config }
    }

    /// Its windows as firmware sets them for the BARs below it: `low`, the
    /// memory window, below 4 GiB, and `high`, the prefetchable one -- each
    /// a start and an end, or closed where None or empty. The registers say
    /// whole MiBs, so each window is its span widened to them.
    pub fn set_windows(&mut self, low: Option<(u64, u64)>, high: Option<(u64, u64)>) {
        let span = |(start, end): (u64, u64)| -> Option<(u64, u64)> {
            let last = end.checked_sub(1)?;
            (start <= last).then_some((start, last))
        };
        let field = |a: u64| ((a >> WINDOW_SHIFT) as u32) & 0xFFF0;
        self.config[MEMORY / 4] = match low.and_then(span).filter(|&(_, last)| last >> 32 == 0) {
            Some((start, last)) => field(start) | (field(last) << 16),
            None => WINDOW_CLOSED,
        };
        let (base, limit, upper) = match high.and_then(span) {
            Some((start, last)) => (field(start), field(last), ((start >> 32) as u32, (last >> 32) as u32)),
            None => (WINDOW_CLOSED, 0, (0, 0)),
        };
        self.config[PREFETCH / 4] = base | (limit << 16) | PREFETCH_64;
        self.config[PREFETCH_BASE_UPPER / 4] = upper.0;
        self.config[PREFETCH_LIMIT_UPPER / 4] = upper.1;
    }

    /// The bus below it as the guest has it numbered now.
    pub fn secondary(&self) -> u8 {
        (self.config[BUSES / 4] >> SECONDARY_SHIFT) as u8
    }

    /// Whether `bus` is the one right below it: its function's, which the
    /// run loop answers for. A bus past it within its subordinate is one
    /// nothing is on, and bus 0 is never one below a port.
    pub fn below(&self, bus: u8) -> bool {
        bus != 0 && bus == self.secondary()
    }

    /// A read of `size` bytes (1, 2 or 4, aligned to it) at `at`, with
    /// `link` what the device below says of its link now: past the 256
    /// bytes, its extended space, of no capability -- zeroes; all ones for
    /// any other access.
    pub fn read(&self, at: usize, size: usize, link: &Link) -> u32 {
        if !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > EXTENDED_SIZE {
            return size_mask(size);
        }
        if at >= CONFIG_SIZE {
            return 0;
        }
        let dword = at & !3;
        let shift = 8 * (at - dword) as u32;
        (self.dword(dword, link) >> shift) & size_mask(size)
    }

    /// A write of `size` bytes (1, 2 or 4, aligned to it) of `value` at `at`:
    /// what of it the guest may write, kept; nothing else.
    pub fn write(&mut self, at: usize, size: usize, value: u32) {
        if !matches!(size, 1 | 2 | 4) || at % size != 0 || at + size > CONFIG_SIZE {
            return;
        }
        let dword = at & !3;
        let shift = 8 * (at - dword) as u32;
        let mask = (size_mask(size) << shift) & writable(dword);
        let old = self.config[dword / 4];
        self.config[dword / 4] = (old & !mask) | ((value << shift) & mask);
    }

    /// The dword at `dword` as the guest reads it.
    fn dword(&self, dword: usize, link: &Link) -> u32 {
        let kept = self.config[dword / 4];
        match dword {
            DEVCAP => u32::from(link.payload & PAYLOAD_MASK) | DEVCAP_ROLE_BASED,
            DEVCTL => (kept & DEVCTL_GUEST) | (u32::from(link.payload & PAYLOAD_MASK) << PAYLOAD_SHIFT),
            LNKCAP => (kept & !(LINK_SPEED_WIDTH | LNKCAP_DLL_ACTIVE)) | (link.cap & LINK_SPEED_WIDTH) | LNKCAP_DLL_ACTIVE,
            LNKCTL => {
                let status = (link.status & (LINK_SPEED_WIDTH as u16 | LNKSTA_SLOT_CLOCK)) | LNKSTA_DLL_ACTIVE;
                (kept & LNKCTL_GUEST) | (u32::from(status) << LNKSTA_SHIFT)
            }
            LNKCAP2 => link.speeds(),
            LNKCTL2 => match kept & LNKCTL2_GUEST {
                0 => link.cap & SPEED_MASK,
                target => target,
            },
            _ => kept,
        }
    }
}

fn size_mask(size: usize) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => u32::MAX,
    }
}
