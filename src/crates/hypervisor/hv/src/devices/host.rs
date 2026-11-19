//! A function of the host's on the guest's bus (plans/gpu-passthrough.md):
//! a real device the guest's own driver drives, its DMA through an IOMMU
//! domain of the guest's RAM (the module's, `kcore::iommu`), its BARs mapped
//! into the guest's nested table straight -- but for the pages that trap.
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
//!   disable stays the host's (set: the device's interrupts are its MSI or
//!   MSI-X, the host's to take);
//! - there is no expansion ROM; the INTx pin is the device's, and the
//!   interrupt line the guest's to write: the pin is there because a driver
//!   may want a legacy IRQ to exist -- NVIDIA's refuses a function with
//!   neither one nor MSI-X (`nv_treat_missing_irq_as_error`) -- and the run
//!   loop wires it to a line nothing raises (`run::HOST_IRQ`): the device's
//!   INTx stays off;
//! - its interrupts are emulated, MSI-X's where the function has MSI-X with
//!   its table and PBA in BARs given, else MSI's: what the guest programs is
//!   what the host's handlers of the device's real ones send it
//!   (`Messages`), and the real capability is the host's (`kcore::msix`).
//!   The one of the two not given is not in the list the guest walks at
//!   all -- a guest that turned MSI on where MSI-X is what the host takes
//!   would wait for an interrupt that never comes. MSI-X's table is the
//!   guest's to the size it is told, `MAX_VECTORS` at most: each entry is a
//!   vector of the host's, and the host has so many;
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
//! Some pages of its BARs trap rather than map (`traps`), and each access of
//! one is answered here (`trapped`):
//!
//! - MSI-X's table and PBA: the guest's table and pending bits, emulated --
//!   mapped, the guest would program the real entries, and send any vector
//!   of the host's where interrupts are not remapped. A page they share
//!   with registers of the device's own has those passed through, an access
//!   at a time, as VFIO does;
//! - NVIDIA's GPUs mirror the configuration space in BAR0, at 0x88000 (and
//!   at 0x1800 on old ones): each access of that page is one of the
//!   configuration space above -- otherwise the guest would write the real
//!   BARs and MSI past every rule here. A write within the MSI capability's
//!   first two bytes -- its id and next pointer, read-only -- is also passed
//!   to the real mirror: a GeForce driver acknowledges its MSI so, and
//!   without it the GPU sends one interrupt and no more (QEMU's quirk,
//!   `vfio_nvidia_quirk_mirror_write`). Within them only: a write that
//!   reached the control word past them would be the guest's to the host's
//!   own MSI. Since Pascal the re-arm is a dword written at 0x704 instead
//!   (nouveau's `gp100_pci_msi_rearm`), past the 256 bytes the guest is
//!   given: that one dword goes to the real mirror alone, as VFIO lets a
//!   write there reach the device; the rest of the extended space -- its
//!   Resizable BAR, ACS and AER among it -- is nothing the guest reaches.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, Ordering};

/// The real function, as the module reaches it: its configuration space
/// (offsets under 256), and the pages of its BARs that trap, for what of
/// them is passed through.
pub trait Real: Send {
    fn read32(&self, offset: u16) -> u32;
    fn write32(&self, offset: u16, value: u32);
    fn write16(&self, offset: u16, value: u16);
    fn write8(&self, offset: u16, value: u8);
    /// The page `offset` bytes into BAR `bar` mapped for `page_read` and
    /// `page_write`: asked by `HostFunction::new` of each page it traps,
    /// before the function is given -- false where it cannot be.
    fn map_page(&mut self, bar: usize, offset: u64) -> bool;
    /// A read of `size` bytes (1, 2, 4 or 8, aligned to it) `offset` bytes
    /// into BAR `bar`, in a page `map_page` mapped; all ones in one it did
    /// not.
    fn page_read(&self, bar: usize, offset: u64, size: usize) -> u64;
    /// A write of `size` bytes of `value` likewise; dropped in a page not
    /// mapped.
    fn page_write(&self, bar: usize, offset: u64, size: usize, value: u64);
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

/// What the guest programmed of the device's interrupts -- MSI's one
/// message, or MSI-X's table an entry each -- for the host's handlers of the
/// real ones to send. Read there with interrupts off, so in atomics, never
/// under the platform's lock; written by the guest's CPUs under it.
pub struct Messages {
    msix: bool,
    /// MSI's control word's enable; MSI-X's enable and function mask.
    control: AtomicU16,
    entries: Box<[Entry]>,
}

/// One message: where it goes and what it says, whether it is masked, and
/// whether one came while it could not go.
struct Entry {
    address: AtomicU64,
    data: AtomicU32,
    /// MSI's mask bit; MSI-X's vector control, whose bit 0 is its mask.
    control: AtomicU32,
    pending: AtomicU32,
}

impl Messages {
    /// `count` messages -- MSI's one, or the MSI-X entries given -- each
    /// masked where it is MSI-X's, as a reset leaves a table. None where
    /// there is no memory for them.
    fn new(msix: bool, count: usize) -> Option<Messages> {
        let mut entries = Vec::new();
        entries.try_reserve_exact(count).ok()?;
        for _ in 0..count {
            entries.push(Entry {
                address: AtomicU64::new(0),
                data: AtomicU32::new(0),
                control: AtomicU32::new(if msix { MSIX_ENTRY_MASKED } else { 0 }),
                pending: AtomicU32::new(0),
            });
        }
        Some(Messages { msix, control: AtomicU16::new(0), entries: entries.into_boxed_slice() })
    }

    /// How many there are: 1 for MSI.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Whether they are MSI-X's.
    pub fn msix(&self) -> bool {
        self.msix
    }

    /// Whether the function sends at all: MSI on, or MSI-X on and not masked
    /// whole.
    fn on(&self) -> bool {
        let c = self.control.load(Ordering::Acquire);
        if self.msix {
            c & MSIX_ENABLE != 0 && c & MSIX_FUNCTION_MASK == 0
        } else {
            c & MSI_ENABLE != 0
        }
    }

    /// Message `i` to send now: None while the function sends nothing, the
    /// message is masked, or there is no such message.
    pub fn message(&self, i: usize) -> Option<(u64, u32)> {
        let e = self.entries.get(i)?;
        if !self.on() || e.control.load(Ordering::Acquire) & 1 != 0 {
            return None;
        }
        Some((e.address.load(Ordering::Acquire), e.data.load(Ordering::Acquire)))
    }

    /// The device's interrupt `i`, as it comes: the message to send, or None
    /// and held pending.
    pub fn raise(&self, i: usize) -> Option<(u64, u32)> {
        let e = self.entries.get(i)?;
        let m = self.message(i);
        if m.is_none() {
            e.pending.store(1, Ordering::Release);
            /* Turned on or unmasked since the look: sent after all, by the
             * one who sees both */
            return self.take_pending(i);
        }
        m
    }

    /// Message `i` held pending, now that it can go: the message, the mark
    /// cleared.
    pub fn take_pending(&self, i: usize) -> Option<(u64, u32)> {
        let m = self.message(i)?;
        let e = self.entries.get(i)?;
        (e.pending.swap(0, Ordering::AcqRel) != 0).then_some(m)
    }

    /// Whether message `i` is held pending: MSI-X's PBA.
    pub fn pending(&self, i: usize) -> bool {
        self.entries.get(i).is_some_and(|e| e.pending.load(Ordering::Acquire) != 0)
    }

    /// Every message held pending that can go now, each handed to `send`:
    /// what a write of the guest's that turned the function on or unmasked
    /// a message lets out.
    pub fn send_pending(&self, mut send: impl FnMut(u64, u32)) {
        for i in 0..self.entries.len() {
            if let Some((address, data)) = self.take_pending(i) {
                send(address, data);
            }
        }
    }
}

/// Where MSI-X's table and PBA are: the BAR and the offset in it of each,
/// how many entries the device has, and how many the guest is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixLayout {
    pub table_bar: usize,
    pub table: u64,
    pub pba_bar: usize,
    pub pba: u64,
    pub entries: usize,
    pub given: usize,
}

impl MsixLayout {
    /// The table's bytes: the device's, every entry of it.
    pub fn table_bytes(&self) -> u64 {
        self.entries as u64 * MSIX_ENTRY_BYTES
    }

    /// The PBA's bytes: a bit an entry, in qwords.
    pub fn pba_bytes(&self) -> u64 {
        (self.entries as u64).div_ceil(64) * 8
    }
}

/// A page of one of the function's BARs that traps rather than maps: the
/// BAR, and the page's offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trap {
    pub bar: usize,
    pub offset: u64,
}

/// A BAR the guest moved: what the caller unmaps and maps again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    pub bar: usize,
    /// Where it was, 0 where it was nowhere yet.
    pub from: u64,
    pub to: u64,
}

/// The BARs one access moved -- an eight-byte write through the mirror may
/// move two -- each from where it was before the access to where it is
/// after.
#[derive(Clone, Copy, Debug, Default)]
pub struct Moves([Option<(u64, u64)>; 6]);

impl Moves {
    fn add(&mut self, m: Option<Moved>) {
        let Some(m) = m else { return };
        if let Some(e) = self.0.get_mut(m.bar) {
            *e = Some((e.map_or(m.from, |(from, _)| from), m.to));
        }
    }

    /// Each BAR moved, but one moved back to where it was.
    pub fn iter(&self) -> impl Iterator<Item = Moved> + '_ {
        self.0.iter().enumerate().filter_map(|(bar, m)| {
            m.filter(|&(from, to)| from != to).map(|(from, to)| Moved { bar, from, to })
        })
    }
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
/// A memory BAR's low bits: its 64-bit type and prefetchable; an I/O BAR's
/// first bit; and the largest a 32-bit memory BAR is.
const BAR_64: u32 = 0x4;
const BAR_PREFETCH: u32 = 0x8;
const BAR_IO: u32 = 0x1;
const BAR32_MAX: u64 = 1 << 31;

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
/// A capability's next pointer: the second byte of its first dword, and the
/// bits of it that are an offset.
const CAP_NEXT_SHIFT: u32 = 8;
const CAP_POINTER_MASK: u32 = 0xFC;

/// MSI's control word: enable, the messages enabled (6:4, kept at one), and
/// what the device can do (64-bit, per-vector masking) -- read-only.
const MSI_ENABLE: u16 = 1 << 0;
const MSI_GUEST_CONTROL: u16 = MSI_ENABLE;
const MSI_64: u16 = 1 << 7;
const MSI_MASKABLE: u16 = 1 << 8;

/// MSI-X (PCI 3.0, 6.8.2): its control word's table size (10:0), function
/// mask and enable -- the last two the guest's; the table's and the PBA's
/// dwords, a BAR's number (2:0) and an offset each; the capability's
/// bytes; a table entry's bytes, and its vector control's dword and mask.
const MSIX_SIZE_MASK: u16 = 0x7FF;
const MSIX_FUNCTION_MASK: u16 = 1 << 14;
const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_GUEST_CONTROL: u16 = MSIX_ENABLE | MSIX_FUNCTION_MASK;
const MSIX_TABLE: usize = 4;
const MSIX_PBA: usize = 8;
const MSIX_BIR_MASK: u32 = 0x7;
const MSIX_LEN: usize = 12;
const MSIX_ENTRY_BYTES: u64 = 16;
const MSIX_ENTRY_CONTROL: u64 = 12;
const MSIX_ENTRY_MASKED: u32 = 1;
/// A message address's low two bits: the dword's alignment, written as 0.
const MSIX_ADDRESS_LOW: u32 = 0x3;

/// The most MSI-X entries a function is given: the table the guest is told
/// of is no longer, whatever the device's -- each entry it uses is a vector
/// of the host's, and a slot of the kernel's for its handler.
pub const MAX_VECTORS: usize = 16;

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
/// NVIDIA's MSI re-arm since Pascal, in the mirror: a dword at 0x704.
pub const NVIDIA_MSI_REARM: u32 = 0x704;

/// A page of a BAR.
const PAGE: u64 = 0x1000;

/// What a byte of a trapped page is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    /// NVIDIA's mirror, whose first byte is at the offset given.
    Mirror(u32),
    /// MSI-X's table and PBA, so many bytes into each.
    Table(u64),
    Pba(u64),
    /// A register of the device's own.
    Device,
}

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
    msix_cap: usize,
    pm_cap: usize,
    pcie_cap: usize,
    af_cap: usize,
    /// MSI-X's table and PBA, where MSI-X is what the guest is given.
    msix: Option<MsixLayout>,
    /// The capability the guest is not shown -- of MSI and MSI-X, where the
    /// function has both, the one not given -- its length, and the pointer
    /// that names it in the real list: 0x34's or the capability's before it.
    /// 0 where none is hidden.
    hidden: usize,
    hidden_len: usize,
    hidden_from: usize,
    /// Each capability's place in the list, a bit a dword: its id and next
    /// pointer, read-only, are no write's of the guest's -- a device that
    /// took one would have the guest change the list the host walked.
    caps: u64,
    messages: Arc<Messages>,
    /// The pages of its BARs that trap rather than map.
    traps: Vec<Trap>,
}

impl HostFunction {
    /// The function `real`, its BARs `bars` as the module read them: None
    /// where it is a bridge, has neither MSI-X with its table and PBA in
    /// BARs given nor MSI to give the guest, has a capability list this
    /// cannot be sure of (above), or a page that has to trap cannot be
    /// mapped for it. Given, the device decodes its memory from here on, no
    /// I/O, and masters nothing until the guest has it do so; phantom
    /// functions and no snoop off.
    pub fn new(mut real: Box<dyn Real>, bars: [Option<Bar>; 6]) -> Option<HostFunction> {
        if (real.read32(HEADER_TYPE as u16 & !3) >> 16) & HEADER_LAYOUT != 0 {
            return None;
        }
        let id = real.read32(0);
        let vendor = (id & 0xFFFF) as u16;
        /* The capability list, whole: every register the host keeps is in a
         * capability it found, and a guest writes at any offset it likes --
         * so a list that loops, runs on past the walk or points into the
         * header, or a second capability of a kind the host keeps registers
         * of (a second PCIe capability's FLR, a second MSI's address), is a
         * function not given. Each one's place in the list is kept too:
         * the pointer that names it, for a capability hidden. */
        let (mut msi_cap, mut msix_cap, mut pm_cap, mut pcie_cap, mut af_cap) = (0, 0, 0, 0, 0);
        let (mut msi_from, mut msix_from) = (0, 0);
        let mut from = CAPABILITIES;
        let mut caps = 0u64;
        let mut at = (real.read32(CAPABILITIES as u16) & CAP_POINTER_MASK) as usize;
        let mut walked = 0;
        while at != 0 {
            walked += 1;
            if walked > CAP_WALK_MAX || at < HEADER_END || at > CONFIG_SIZE - 4 {
                return None;
            }
            caps |= 1 << (at / 4);
            let head = real.read32(at as u16);
            let found = match (head & 0xFF) as u8 {
                CAP_MSI => {
                    msi_from = from;
                    &mut msi_cap
                }
                CAP_MSIX => {
                    msix_from = from;
                    &mut msix_cap
                }
                CAP_PM => &mut pm_cap,
                CAP_PCIE => &mut pcie_cap,
                CAP_AF => &mut af_cap,
                _ => {
                    from = at;
                    at = ((head >> CAP_NEXT_SHIFT) & CAP_POINTER_MASK) as usize;
                    continue;
                }
            };
            if *found != 0 {
                return None;
            }
            *found = at;
            from = at;
            at = ((head >> CAP_NEXT_SHIFT) & CAP_POINTER_MASK) as usize;
        }
        /* MSI-X where its table and PBA are in BARs given, MSI else; the
         * other one hidden */
        let msix = if msix_cap != 0 { msix_layout(&*real, msix_cap, &bars) } else { None };
        let (hidden, hidden_len, hidden_from) = match (msix, msi_cap, msix_cap) {
            (Some(_), 0, _) => (0, 0, 0),
            (Some(_), m, _) => (m, msi_len_at(&*real, m), msi_from),
            (None, 0, _) => return None,
            (None, _, 0) => (0, 0, 0),
            (None, _, x) => (x, MSIX_LEN, msix_from),
        };
        let messages = Arc::new(Messages::new(msix.is_some(), msix.map_or(1, |l| l.given))?);

        /* The pages that trap: NVIDIA's mirror at 0x88000, a page of its
         * own -- the old one at 0x1800 (NV4x) shares its page with registers
         * of the GPU's, and nothing since uses it -- and every page MSI-X's
         * table or PBA has a byte in, each mapped for what of it is passed
         * through */
        let mut traps: Vec<Trap> = Vec::new();
        let mut trap = |bar: usize, offset: u64| -> Option<()> {
            let t = Trap { bar, offset: offset & !(PAGE - 1) };
            if !traps.contains(&t) {
                traps.try_reserve(1).ok()?;
                traps.push(t);
            }
            Some(())
        };
        if vendor == VENDOR_NVIDIA && bars[0].is_some_and(|b0| u64::from(NVIDIA_MIRROR + MIRROR_BYTES) <= b0.size) {
            trap(0, u64::from(NVIDIA_MIRROR))?;
        }
        if let Some(l) = msix {
            for (bar, start, len) in [(l.table_bar, l.table, l.table_bytes()), (l.pba_bar, l.pba, l.pba_bytes())] {
                let mut page = start & !(PAGE - 1);
                while page < start + len {
                    trap(bar, page)?;
                    page += PAGE;
                }
            }
        }
        for t in &traps {
            if !real.map_page(t.bar, t.offset) {
                return None;
            }
        }

        let mut f = HostFunction {
            real,
            vendor,
            bars,
            guest_bars: [0; 6],
            command: 0,
            line: 0,
            msi_cap,
            msix_cap,
            pm_cap,
            pcie_cap,
            af_cap,
            msix,
            hidden,
            hidden_len,
            hidden_from,
            caps,
            messages,
            traps,
        };
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

    /// Where the host's handlers of the device's interrupts find what to
    /// send.
    pub fn messages(&self) -> Arc<Messages> {
        self.messages.clone()
    }

    /// MSI-X's table and PBA, where the guest is given MSI-X: the host takes
    /// the device's MSI-X then, an entry of its own for each given; else its
    /// MSI.
    pub fn msix_layout(&self) -> Option<MsixLayout> {
        self.msix
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

    /// The pages of its BARs that trap rather than map.
    pub fn traps(&self) -> &[Trap] {
        &self.traps
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
            /* The capability not shown: nothing of it is the guest's */
            d if self.hidden != 0 && (self.hidden..self.hidden + self.hidden_len).contains(&d) => None,
            d if self.msix.is_some() && (self.msix_cap..self.msix_cap + MSIX_LEN).contains(&d) => {
                self.write_msix(d - self.msix_cap, mask, value);
                None
            }
            d if self.msix.is_none() && self.msi_cap != 0 && (self.msi_cap..self.msi_cap + msi_len(self)).contains(&d) => {
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
                 * word or a dword as the guest wrote it -- but a capability's
                 * id and next pointer, the first two bytes of its first
                 * dword, which a dword's write leaves out of a word's */
                let header = if self.caps & (1 << (d / 4)) != 0 { 0xFFFF } else { 0 };
                match size {
                    _ if mask & header == mask => {}
                    1 => self.real.write8(at as u16, (value >> shift) as u8),
                    2 => self.real.write16(at as u16, (value >> shift) as u16),
                    _ if header != 0 => self.real.write16((d + 2) as u16, (value >> 16) as u16),
                    _ => self.real.write32(d as u16, value),
                }
                None
            }
        }
    }

    /// An access of a trapped page of BAR `bar`, `offset` bytes into the
    /// BAR, of `size` bytes -- 1, 2, 4 or 8, within the page -- the value a
    /// write writes: what a read reads, and the BARs a write through the
    /// mirror moved. One of NVIDIA's mirror is the configuration space's
    /// (`mirror`), one of MSI-X's table or PBA the guest's own, any other a
    /// register of the device's, passed through; one not aligned to its
    /// size is made a byte at a time -- the mapping's volatile access cannot
    /// be misaligned. An access of a page that does not trap reads all ones
    /// and writes nothing.
    pub fn trapped(&mut self, bar: usize, offset: u64, size: usize, write: bool, value: u64) -> (u64, Moves) {
        let mut moves = Moves::default();
        let page = offset & !(PAGE - 1);
        if !matches!(size, 1 | 2 | 4 | 8) || offset - page + size as u64 > PAGE
            || !self.traps.contains(&Trap { bar, offset: page })
        {
            return (size_mask64(size), moves);
        }
        if offset % size as u64 != 0 {
            let mut read = 0u64;
            for k in 0..size as u64 {
                let (r, m) = self.trapped_aligned(bar, offset + k, 1, write, value >> (8 * k));
                read |= (r & 0xFF) << (8 * k);
                moves.add(m);
            }
            return (read, moves);
        }
        /* Eight bytes as two dwords where each is a register of four */
        if size == 8 && self.region(bar, offset) != Region::Device {
            let (low, m) = self.trapped_aligned(bar, offset, 4, write, value & 0xFFFF_FFFF);
            moves.add(m);
            let (high, m) = self.trapped_aligned(bar, offset + 4, 4, write, value >> 32);
            moves.add(m);
            return ((low & 0xFFFF_FFFF) | (high << 32), moves);
        }
        let (read, m) = self.trapped_aligned(bar, offset, size, write, value);
        moves.add(m);
        (read, moves)
    }

    /// What the byte `offset` into BAR `bar` is, in a trapped page.
    fn region(&self, bar: usize, offset: u64) -> Region {
        let mirror = u64::from(NVIDIA_MIRROR);
        if bar == 0 && self.nvidia() && offset >= mirror && offset - mirror < u64::from(MIRROR_BYTES)
            && self.traps.contains(&Trap { bar: 0, offset: mirror })
        {
            return Region::Mirror(NVIDIA_MIRROR);
        }
        if let Some(l) = self.msix {
            if bar == l.table_bar && offset >= l.table && offset - l.table < l.table_bytes() {
                return Region::Table(offset - l.table);
            }
            if bar == l.pba_bar && offset >= l.pba && offset - l.pba < l.pba_bytes() {
                return Region::Pba(offset - l.pba);
            }
        }
        Region::Device
    }

    /// `trapped`'s access, aligned to its size, eight bytes only for the
    /// device's own registers.
    fn trapped_aligned(&mut self, bar: usize, offset: u64, size: usize, write: bool, value: u64) -> (u64, Option<Moved>) {
        match self.region(bar, offset) {
            Region::Mirror(base) => {
                let (read, moved) = self.mirror(base, (offset - u64::from(base)) as u32, size, write, value as u32);
                (u64::from(read), moved)
            }
            Region::Table(at) => {
                if write {
                    self.table_write(at, size, value as u32);
                    (0, None)
                } else {
                    (u64::from(self.table_read(at, size)), None)
                }
            }
            /* Read-only: the guest's pending bits */
            Region::Pba(at) => (if write { 0 } else { self.pba_read(at, size) }, None),
            Region::Device => {
                if write {
                    self.real.page_write(bar, offset, size, value);
                    (0, None)
                } else {
                    (self.real.page_read(bar, offset, size), None)
                }
            }
        }
    }

    /// An access of the trapped mirror page, `offset` bytes into it, of 1, 2
    /// or 4 bytes: a configuration space access of the same bytes, one within
    /// the MSI capability's first two also written to the real mirror (see
    /// the module's comment). What a read reads: all ones for an access of
    /// another size, zeroes past the configuration space -- the extended
    /// space's, which the guest is not given -- where writes are dropped.
    pub fn mirror(&mut self, mirror: u32, offset: u32, size: usize, write: bool, value: u32) -> (u32, Option<Moved>) {
        let at = offset as usize;
        if !matches!(size, 1 | 2 | 4) {
            return (size_mask(size), None);
        }
        if write && size == 4 && offset == NVIDIA_MSI_REARM && self.nvidia() {
            self.real.page_write(0, u64::from(mirror + offset), 4, u64::from(value));
            return (0, None);
        }
        if at + size > CONFIG_SIZE {
            return (0, None);
        }
        if !write {
            return (self.config_read(at, size), None);
        }
        if self.msix.is_none() && self.msi_cap != 0 && at >= self.msi_cap && at + size <= self.msi_cap + 2 {
            self.real.page_write(0, u64::from(mirror + offset), size, u64::from(value));
        }
        (0, self.config_write(at, size, value))
    }

    /// The dword at `dword`, the guest's view of it.
    fn read_dword(&self, dword: usize) -> u32 {
        let v = match dword {
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
                (real & 0xFFFF_FF00) | u32::from(self.line)
            }
            d if self.hidden != 0 && (self.hidden..self.hidden + self.hidden_len).contains(&d) => 0,
            d if self.msix.is_some() && (self.msix_cap..self.msix_cap + MSIX_LEN).contains(&d) => {
                self.read_msix(d - self.msix_cap)
            }
            d if self.msix.is_none() && self.msi_cap != 0 && (self.msi_cap..self.msi_cap + msi_len(self)).contains(&d) => {
                self.read_msi(d - self.msi_cap)
            }
            d => self.real.read32(d as u16),
        };
        self.unlinked(dword, v)
    }

    /// The capability list as the guest walks it: the pointer that names
    /// the hidden capability -- 0x34's, or the next pointer of the one
    /// before it -- made to name the one after it. `v` is the dword `dword`
    /// reads otherwise.
    fn unlinked(&self, dword: usize, v: u32) -> u32 {
        if self.hidden == 0 || dword != self.hidden_from {
            return v;
        }
        let next = (self.real.read32(self.hidden as u16) >> CAP_NEXT_SHIFT) & CAP_POINTER_MASK;
        let shift = if dword == CAPABILITIES { 0 } else { CAP_NEXT_SHIFT };
        (v & !(0xFF << shift)) | (next << shift)
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

    /// MSI's one message, as the guest has it.
    fn msi_entry(&self) -> Option<&Entry> {
        self.messages.entries.first()
    }

    fn read_msi(&self, off: usize) -> u32 {
        let real = self.real.read32(self.msi_cap as u16);
        let caps = ((real >> 16) as u16) & (MSI_64 | MSI_MASKABLE);
        let control = (self.messages.control.load(Ordering::Acquire) & MSI_GUEST_CONTROL) | caps;
        let wide = caps & MSI_64 != 0;
        let Some(e) = self.msi_entry() else { return 0 };
        let address = e.address.load(Ordering::Acquire);
        match off {
            0x0 => (real & 0xFFFF) | (u32::from(control) << 16),
            0x4 => address as u32,
            0x8 if wide => (address >> 32) as u32,
            0x8 | 0xC if off == data_offset(wide) => e.data.load(Ordering::Acquire) & 0xFFFF,
            o if caps & MSI_MASKABLE != 0 && o == data_offset(wide) + 4 => e.control.load(Ordering::Acquire),
            _ => 0,
        }
    }

    fn write_msi(&mut self, off: usize, mask: u32, value: u32) {
        let real = self.real.read32(self.msi_cap as u16);
        let caps = ((real >> 16) as u16) & (MSI_64 | MSI_MASKABLE);
        let wide = caps & MSI_64 != 0;
        let merge = |old: u32| (old & !mask) | (value & mask);
        if off == 0 {
            let old = u32::from(self.messages.control.load(Ordering::Acquire)) << 16;
            let control = ((merge(old) >> 16) as u16) & MSI_GUEST_CONTROL;
            self.messages.control.store(control, Ordering::Release);
            return;
        }
        let Some(e) = self.msi_entry() else { return };
        let address = e.address.load(Ordering::Acquire);
        match off {
            0x4 => {
                let low = u64::from(merge(address as u32) & !0x3);
                e.address.store((address & !0xFFFF_FFFF) | low, Ordering::Release);
            }
            0x8 if wide => {
                let high = u64::from(merge((address >> 32) as u32));
                e.address.store((address & 0xFFFF_FFFF) | (high << 32), Ordering::Release);
            }
            o if o == data_offset(wide) => {
                let data = merge(e.data.load(Ordering::Acquire)) & 0xFFFF;
                e.data.store(data, Ordering::Release);
            }
            o if caps & MSI_MASKABLE != 0 && o == data_offset(wide) + 4 => {
                let m = merge(e.control.load(Ordering::Acquire)) & 1;
                e.control.store(m, Ordering::Release);
            }
            _ => {}
        }
    }

    /// MSI-X's capability as the guest sees it: its control word the
    /// guest's enable and function mask over the table's size it is given,
    /// the table's and the PBA's dwords the device's.
    fn read_msix(&self, off: usize) -> u32 {
        let real = self.real.read32((self.msix_cap + off) as u16);
        if off != 0 {
            return real;
        }
        let given = self.messages.count().clamp(1, usize::from(MSIX_SIZE_MASK) + 1) as u16;
        let control = (self.messages.control.load(Ordering::Acquire) & MSIX_GUEST_CONTROL) | (given - 1);
        (real & 0xFFFF) | (u32::from(control) << 16)
    }

    fn write_msix(&mut self, off: usize, mask: u32, value: u32) {
        if off != 0 || mask & 0xFFFF_0000 == 0 {
            return;
        }
        let old = u32::from(self.messages.control.load(Ordering::Acquire)) << 16;
        let control = ((((old & !mask) | (value & mask)) >> 16) as u16) & MSIX_GUEST_CONTROL;
        self.messages.control.store(control, Ordering::Release);
    }

    /// The guest's MSI-X table, `at` bytes into it, `size` bytes (1, 2 or
    /// 4, aligned to it): an entry's address, data and vector control --
    /// its mask alone -- as the guest wrote them; nothing past the entries
    /// it is given.
    fn table_read(&self, at: u64, size: usize) -> u32 {
        let dword = at & !3;
        let shift = 8 * (at - dword) as u32;
        let Some(e) = self.messages.entries.get((dword / MSIX_ENTRY_BYTES) as usize) else { return 0 };
        let v = match dword % MSIX_ENTRY_BYTES {
            0 => e.address.load(Ordering::Acquire) as u32,
            4 => (e.address.load(Ordering::Acquire) >> 32) as u32,
            8 => e.data.load(Ordering::Acquire),
            _ => e.control.load(Ordering::Acquire) & MSIX_ENTRY_MASKED,
        };
        (v >> shift) & size_mask(size)
    }

    fn table_write(&mut self, at: u64, size: usize, value: u32) {
        let dword = at & !3;
        let shift = 8 * (at - dword) as u32;
        let mask = size_mask(size) << shift;
        let value = (value << shift) & mask;
        let merge = |old: u32| (old & !mask) | value;
        let Some(e) = self.messages.entries.get((dword / MSIX_ENTRY_BYTES) as usize) else { return };
        match dword % MSIX_ENTRY_BYTES {
            0 => {
                let a = e.address.load(Ordering::Acquire);
                let low = u64::from(merge(a as u32) & !MSIX_ADDRESS_LOW);
                e.address.store((a & !0xFFFF_FFFF) | low, Ordering::Release);
            }
            4 => {
                let a = e.address.load(Ordering::Acquire);
                let high = u64::from(merge((a >> 32) as u32));
                e.address.store((a & 0xFFFF_FFFF) | (high << 32), Ordering::Release);
            }
            8 => e.data.store(merge(e.data.load(Ordering::Acquire)), Ordering::Release),
            _ => {
                let control = merge(e.control.load(Ordering::Acquire)) & MSIX_ENTRY_MASKED;
                e.control.store(control, Ordering::Release);
            }
        }
    }

    /// The guest's PBA, `at` bytes into it, `size` bytes: a bit a message
    /// held pending, none past the entries given.
    fn pba_read(&self, at: u64, size: usize) -> u64 {
        let first = (at * 8) as usize;
        (0..size * 8).filter(|&bit| self.messages.pending(first + bit)).fold(0, |v, bit| v | (1 << bit))
    }
}

/// Where MSI-X's table and PBA are, by the capability at `cap`: None where
/// either is not wholly inside a memory BAR given -- an I/O BAR's, one not
/// decoded, or past its end.
fn msix_layout(real: &dyn Real, cap: usize, bars: &[Option<Bar>; 6]) -> Option<MsixLayout> {
    let control = (real.read32(cap as u16) >> 16) as u16;
    let entries = usize::from(control & MSIX_SIZE_MASK) + 1;
    let table = real.read32((cap + MSIX_TABLE) as u16);
    let pba = real.read32((cap + MSIX_PBA) as u16);
    let l = MsixLayout {
        table_bar: (table & MSIX_BIR_MASK) as usize,
        table: u64::from(table & !MSIX_BIR_MASK),
        pba_bar: (pba & MSIX_BIR_MASK) as usize,
        pba: u64::from(pba & !MSIX_BIR_MASK),
        entries,
        given: entries.min(MAX_VECTORS),
    };
    let inside = |bar: usize, start: u64, len: u64| {
        bars.get(bar).copied().flatten().is_some_and(|b| start.checked_add(len).is_some_and(|end| end <= b.size))
    };
    (inside(l.table_bar, l.table, l.table_bytes()) && inside(l.pba_bar, l.pba, l.pba_bytes())).then_some(l)
}

/// A memory BAR as the module reads it with the device's decoding off:
/// `low` its register as it was, `mask_low` what the register read back
/// after all ones were written to it, `mask_high` the same of the next
/// register for a 64-bit BAR, and `host` the address the registers said.
/// None for an I/O BAR, and for one not implemented, not assigned, not
/// aligned to its size, or of a size no BAR is -- under a page, a 32-bit one
/// over 2 GiB, or a mask whose ones do not run down from the top. A 64-bit
/// BAR of 4 GiB or more keeps no address bit in its low register at all:
/// its size is in its high one alone (an RTX A2000's BAR1 is 8 GiB).
pub fn decode_bar(low: u32, mask_low: u32, mask_high: Option<u32>, host: u64) -> Option<Bar> {
    if low & BAR_IO != 0 {
        return None;
    }
    let mask = match mask_high {
        Some(high) => (u64::from(high) << 32) | u64::from(mask_low & !0xF),
        None => 0xFFFF_FFFF_0000_0000 | u64::from(mask_low & !0xF),
    };
    let size = (!mask).wrapping_add(1);
    let largest = if mask_high.is_some() { u64::MAX } else { BAR32_MAX };
    let page = kcore::consts::PAGE_SIZE as u64;
    if !size.is_power_of_two() || size < page || size > largest || host == 0 || host % size != 0 {
        return None;
    }
    Some(Bar { host, size, wide: mask_high.is_some(), prefetchable: low & BAR_PREFETCH != 0 })
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

/// The bytes of the MSI capability at `cap`, as long as the real one's.
fn msi_len_at(real: &dyn Real, cap: usize) -> usize {
    let control = (real.read32(cap as u16) >> 16) as u16;
    let base = if control & MSI_64 != 0 { 0x10 } else { 0xC };
    if control & MSI_MASKABLE != 0 { base + 8 } else { base }
}

/// The bytes of the function's MSI capability.
fn msi_len(f: &HostFunction) -> usize {
    msi_len_at(&*f.real, f.msi_cap)
}

fn size_mask(size: usize) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        _ => u32::MAX,
    }
}

fn size_mask64(size: usize) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}
