//! A guest CPU's local APIC, in either of its modes: x2APIC, reached
//! through MSRs, and xAPIC, a page of MMIO at 0xFEE00000.
//!
//! A guest of more than one CPU cannot do without a local APIC: it is what
//! one CPU interrupts another through (an IPI), what starts the others at
//! all (INIT and the start-up IPI), and each CPU's own timer. The x2APIC is
//! its registers as MSRs 0x800-0x8FF, and an MSR access is an exit whose
//! register, value and length the CPU hands over; so a guest is given an
//! x2APIC, turned on as firmware on a machine with more than 255 CPUs leaves
//! one, and a kernel that keeps it -- Linux does -- never touches the page.
//! One that turns x2APIC mode off (`nox2apic`), or a guest whose firmware
//! leaves it in xAPIC mode, reaches the same registers through the page:
//! every access a nested fault, the instruction decoded and performed by
//! `crate::mmio`, and the register answered here (`mmio_read`,
//! `mmio_write`). The two differ where the architecture has them differ --
//! the ID's place, a writable logical ID and destination format, the ICR in
//! two halves, what is reserved -- and are one register file otherwise.
//!
//! What is here is the register file and what each register does to the
//! others: the ID and the logical ID derived from it, the task and
//! processor priorities, in-service, request and trigger-mode registers,
//! EOI -- which hands a level-triggered interrupt's end back to the caller,
//! for the IO-APIC it came from (`Wrote::Eoi`) -- the
//! spurious-vector register that turns the APIC on and off, the error
//! register, the local vector table, the interrupt command register -- which
//! hands the IPI it sends back to the caller, who alone can reach the other
//! CPUs (`Ipi`) -- and the timer, counting down at a bus clock of 1 GHz off
//! the host's clock. Delivery into the guest is the run loop's: it asks for
//! the highest interrupt the priorities let through (`pending`), injects it,
//! and says so (`acknowledge`).
//!
//! Plain data and nothing else, as every device here is: stage 5's live
//! update has to be able to serialise it.

/// IA32_APIC_BASE.
pub const MSR_APIC_BASE: u32 = 0x1B;
/// The x2APIC's registers: MSR 0x800 plus the xAPIC offset over 16.
pub const MSR_X2APIC_FIRST: u32 = 0x800;
pub const MSR_X2APIC_LAST: u32 = 0x8FF;

/// Whether `msr` is one of this device's.
pub fn owns(msr: u32) -> bool {
    msr == MSR_APIC_BASE || (MSR_X2APIC_FIRST..=MSR_X2APIC_LAST).contains(&msr)
}

/* IA32_APIC_BASE: where the xAPIC page is, and the bits that say what the
 * APIC is -- the boot CPU's, on at all (EN), and in x2APIC mode (EXTD). */
/// The page a PC's firmware leaves the xAPIC at: where a guest that turned
/// x2APIC mode off would look for it, and what the base MSR says.
pub const DEFAULT_BASE: u64 = 0xFEE0_0000;
const BASE_BSP: u64 = 1 << 8;
const BASE_EXTD: u64 = 1 << 10;
const BASE_EN: u64 = 1 << 11;
/// The base address's bits, up to the 52 of physical address a CPU has at
/// most; everything else in the MSR is reserved.
const BASE_ADDRESS: u64 = 0x000F_FFFF_FFFF_F000;

/* Registers, by their x2APIC MSR's offset from 0x800. */
const REG_ID: u32 = 0x02;
const REG_VERSION: u32 = 0x03;
const REG_TPR: u32 = 0x08;
const REG_PPR: u32 = 0x0A;
const REG_EOI: u32 = 0x0B;
const REG_LDR: u32 = 0x0D;
const REG_SVR: u32 = 0x0F;
const REG_ISR: u32 = 0x10;
const REG_TMR: u32 = 0x18;
const REG_IRR: u32 = 0x20;
const REG_ESR: u32 = 0x28;
const REG_ICR: u32 = 0x30;
const REG_LVT_TIMER: u32 = 0x32;
const REG_LVT_ERROR: u32 = 0x37;
const REG_TIMER_INITIAL: u32 = 0x38;
const REG_TIMER_CURRENT: u32 = 0x39;
const REG_TIMER_DIVIDE: u32 = 0x3E;
const REG_SELF_IPI: u32 = 0x3F;
/// Eight 32-bit words make each of the 256-bit ISR, TMR and IRR.
const WORDS: u32 = 8;

/// The version register: an integrated APIC (0x14), with six entries in its
/// local vector table (the highest index, 5, in bits 23:16) -- the timer,
/// thermal, performance counter, LINT0, LINT1 and error ones; no CMCI, and
/// no suppression of EOI broadcasts: the EOI of a level-triggered interrupt
/// always goes on to the IO-APIC.
const VERSION: u32 = 0x14 | (((LVT_ENTRIES - 1) as u32) << 16);

/* The local vector table, in the order of its MSRs from 0x832. */
const LVT_ENTRIES: usize = 6;
const LVT_TIMER: usize = 0;
const LVT_LINT0: usize = 3;
const LVT_LINT1: usize = 4;
const LVT_ERROR: usize = 5;

/* An LVT entry's fields. */
const LVT_VECTOR: u32 = 0xFF;
const LVT_MASKED: u32 = 1 << 16;
const LVT_DELIVERY_SHIFT: u32 = 8;
const LVT_DELIVERY_MASK: u32 = 0x7 << LVT_DELIVERY_SHIFT;
const DELIVERY_NMI: u32 = 0x4 << LVT_DELIVERY_SHIFT;
const DELIVERY_EXTINT: u32 = 0x7 << LVT_DELIVERY_SHIFT;
/// The timer's mode, bits 18:17: one-shot (0) or periodic (1). The third,
/// TSC-deadline, is not offered (CPUID says so), and the bit is not
/// writable -- as on a CPU without it.
const TIMER_PERIODIC: u32 = 1 << 17;

/// What of each entry a write sets, by entry -- KVM's masks, less the
/// TSC-deadline mode: the vector, the delivery mode where there is one, the
/// LINT pins' polarity and trigger mode, the mask, the timer's mode. The
/// delivery-status and remote-IRR bits are the APIC's to report.
const LVT_WRITABLE: [u32; LVT_ENTRIES] = [
    LVT_VECTOR | LVT_MASKED | TIMER_PERIODIC,
    LVT_VECTOR | LVT_DELIVERY_MASK | LVT_MASKED,
    LVT_VECTOR | LVT_DELIVERY_MASK | LVT_MASKED,
    LVT_VECTOR | LVT_DELIVERY_MASK | (1 << 13) | (1 << 15) | LVT_MASKED,
    LVT_VECTOR | LVT_DELIVERY_MASK | (1 << 13) | (1 << 15) | LVT_MASKED,
    LVT_VECTOR | LVT_MASKED,
];

/* The spurious-interrupt vector register: its vector in bits 7:0, and the
 * APIC's software enable. Bits 9 and up -- focus checking, EOI-broadcast
 * suppression -- are nothing here. */
const SVR_ENABLE: u32 = 1 << 8;
/// What a write keeps: KVM's mask.
const SVR_WRITABLE: u32 = 0x3FF;
/// Out of reset: vector 0xFF, software-disabled.
const SVR_RESET: u32 = 0xFF;

/* The error status register's bits this APIC can set. */
const ESR_SEND_ILLEGAL_VECTOR: u32 = 1 << 5;
const ESR_RECEIVE_ILLEGAL_VECTOR: u32 = 1 << 6;

/* The interrupt command register, x2APIC's 64-bit form. */
const ICR_VECTOR: u64 = 0xFF;
const ICR_DELIVERY_SHIFT: u64 = 8;
const ICR_DELIVERY_MASK: u64 = 0x7;
const ICR_LOGICAL: u64 = 1 << 11;
const ICR_ASSERT: u64 = 1 << 14;
const ICR_LEVEL: u64 = 1 << 15;
const ICR_SHORTHAND_SHIFT: u64 = 18;
const ICR_SHORTHAND_MASK: u64 = 0x3;
const ICR_DEST_SHIFT: u64 = 32;
/// The delivery-status bit, which x2APIC mode has no use for: ignored on a
/// write, as KVM ignores it.
const ICR_BUSY: u64 = 1 << 12;
/// Reserved in x2APIC mode, and a #GP when set: bits 31:20, 17:16 and 13.
const ICR_RESERVED: u64 = 0xFFF0_0000 | (0x3 << 16) | (1 << 13);

/// The broadcast destination, physical or logical: an x2APIC's.
const BROADCAST: u32 = u32::MAX;
/// An xAPIC's, and a message's with an 8-bit destination.
const XAPIC_BROADCAST: u32 = 0xFF;

/* xAPIC mode: the page's registers are at their x2APIC MSR's offset times
 * 16, each 32 bits at the start of its 16 bytes. */
/// The page's size.
pub const XAPIC_PAGE_SIZE: u64 = 4096;
const XAPIC_STRIDE_SHIFT: u32 = 4;
const XAPIC_REG_BYTES: u32 = 4;
/// Registers xAPIC mode has and x2APIC mode has not.
const REG_APR: u32 = 0x09;
const REG_RRD: u32 = 0x0C;
const REG_DFR: u32 = 0x0E;
const REG_ICR_HIGH: u32 = 0x31;
/// The xAPIC ID's place: bits 31:24.
const XAPIC_ID_SHIFT: u32 = 24;
/// The logical ID's bits in the LDR, and the destination model's in the
/// DFR: flat (all ones) or cluster (0); the rest of the DFR reads ones.
const LDR_MASK: u32 = 0xFF00_0000;
const DFR_MODEL_MASK: u32 = 0xF000_0000;
const DFR_RESET: u32 = u32::MAX;
const DFR_FLAT: u32 = 0xF000_0000;
/// xAPIC mode's ICR: the destination in bits 63:56 -- ICR_HIGH's 31:24.
const XAPIC_ICR_DEST_SHIFT: u64 = 56;

/// The timer's divide configuration register: bits 0, 1 and 3.
const DIVIDE_WRITABLE: u32 = 0b1011;

/// The bus clock the timer counts at, in nanoseconds a tick: 1 GHz, as
/// KVM's. A guest measures it against its PIT or its TSC before it uses the
/// timer, so any rate would do; this one makes a count nanoseconds.
const BUS_NS_PER_TICK: u64 = 1;
/// The shortest period the timer is let repeat at: a guest programming a
/// period of a few ticks would otherwise be an interrupt storm the host
/// pays for. KVM's floor is 200 us; a periodic tick is milliseconds.
const MIN_PERIOD_NS: u64 = 100_000;
/// The most periods a periodic timer that fell behind is owed: a second's
/// worth, as for the PIT (`Pit::ch0_fire`). A guest the host did not run for
/// longer takes its time up from its clocksource, not from a flood.
const MOST_OWED_NS: u64 = kcore::consts::NS_PER_SEC;

/// How an IPI is delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// A vector, into the request register of every CPU it names.
    Fixed,
    /// A vector, to one of the CPUs it names: the first of them here.
    LowestPriority,
    Smi,
    Nmi,
    /// INIT, asserted: the CPUs named go back to waiting for a start-up IPI.
    Init,
    /// INIT de-asserted: what the MP protocol's second INIT is, which
    /// resets nothing on any APIC since the Pentium 4 -- accepted and
    /// dropped.
    InitDeassert,
    /// A start-up IPI: a CPU waiting for one starts in real mode at the
    /// vector's page.
    Startup,
}

/// Whom an IPI is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    /// The CPU whose APIC ID is this; all ones for every CPU -- 32 of them
    /// from an x2APIC, 8 from an xAPIC or a message.
    Physical(u32),
    /// The CPUs whose logical ID matches, each by its own mode: an x2APIC's
    /// -- a cluster (bits 31:16) and a bit in it -- the cluster the same and a
    /// bit in common; an xAPIC's eight bits by its destination model.
    Logical(u32),
    SelfOnly,
    All,
    AllButSelf,
}

/// A local APIC's mode, as IA32_APIC_BASE has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Off: EN clear.
    Off,
    Xapic,
    X2apic,
}

/// What a message needs to know of an APIC to tell whether it is for it,
/// beside its ID: its mode, and in xAPIC mode its logical ID and whether its
/// model is flat. What another CPU matches its IPIs against, from the copy
/// each CPU publishes ([`Addressing::pack`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addressing {
    pub mode: Mode,
    /// The xAPIC logical ID: LDR bits 31:24.
    pub ldr: u8,
    pub flat: bool,
}

impl Addressing {
    const MODE_MASK: u64 = 0x3;
    const LDR_SHIFT: u32 = 8;
    const FLAT: u64 = 1 << 16;

    /// As a word, for a CPU to publish where the others read it.
    pub fn pack(self) -> u64 {
        let mode = match self.mode {
            Mode::Off => 0,
            Mode::Xapic => 1,
            Mode::X2apic => 2,
        };
        mode | (u64::from(self.ldr) << Self::LDR_SHIFT) | if self.flat { Self::FLAT } else { 0 }
    }

    pub fn unpack(w: u64) -> Addressing {
        let mode = match w & Self::MODE_MASK {
            0 => Mode::Off,
            1 => Mode::Xapic,
            _ => Mode::X2apic,
        };
        Addressing { mode, ldr: (w >> Self::LDR_SHIFT) as u8, flat: w & Self::FLAT != 0 }
    }
}

/// An IPI a write of the ICR sends: for the caller to deliver, since only
/// it can reach the other CPUs.
#[derive(Clone, Copy, Debug)]
pub struct Ipi {
    pub delivery: Delivery,
    pub vector: u8,
    pub destination: Destination,
}

impl Ipi {
    /// Whether the CPU with APIC ID `id`, addressed as `target`, is among
    /// those this is for, the sender's being `from`.
    pub fn reaches(&self, id: u32, target: Addressing, from: u32) -> bool {
        match self.destination {
            Destination::All => true,
            Destination::SelfOnly => id == from,
            Destination::AllButSelf => id != from,
            Destination::Physical(BROADCAST) | Destination::Logical(BROADCAST) => true,
            Destination::Physical(dest) => {
                dest == id || (target.mode == Mode::Xapic && dest == XAPIC_BROADCAST)
            }
            Destination::Logical(dest) if target.mode == Mode::Xapic => {
                let d = dest as u8;
                if dest > XAPIC_BROADCAST {
                    false
                } else if dest == XAPIC_BROADCAST {
                    true
                } else if target.flat {
                    d & target.ldr != 0
                } else {
                    /* Cluster: the top four bits the cluster, the low four
                     * a bit each for its members. */
                    d >> 4 == target.ldr >> 4 && d & target.ldr & 0xF != 0
                }
            }
            Destination::Logical(dest) => {
                let ldr = logical_id(id);
                ldr >> 16 == dest >> 16 && ldr & dest & 0xFFFF != 0
            }
        }
    }
}

/* A message-signalled interrupt, as a device writes it: to an address in
 * 0xFEExxxxx that says whom -- an 8-bit destination in bits 19:12, logical
 * or physical by bit 2 -- and data that says what: the vector, and the
 * delivery mode in bits 10:8. What x86 Linux writes without interrupt
 * remapping, its destination the x2APIC ID or, in cluster mode, the first
 * cluster's bits: its eight CPUs are what an 8-bit field can name. */
const MSI_ADDRESS_BASE: u64 = 0xFEE;
const MSI_ADDRESS_BASE_SHIFT: u32 = 20;
const MSI_DEST_SHIFT: u32 = 12;
const MSI_DEST_MASK: u64 = 0xFF;
const MSI_DEST_LOGICAL: u64 = 1 << 2;
const MSI_VECTOR_MASK: u32 = 0xFF;
const MSI_DELIVERY_SHIFT: u32 = 8;
const MSI_DELIVERY_MASK: u32 = 0x7;
const MSI_DELIVERY_FIXED: u32 = 0;
const MSI_DELIVERY_LOWEST: u32 = 1;

/// The interrupt a device's MSI of `address` and `data` names, as the APIC
/// bus would carry it -- or None for a write that is no interrupt message,
/// outside 0xFEExxxxx, or one of a delivery mode no device here sends:
/// fixed and lowest priority are what a device's are.
pub fn msi(address: u64, data: u32) -> Option<Ipi> {
    if address >> MSI_ADDRESS_BASE_SHIFT != MSI_ADDRESS_BASE {
        return None;
    }
    let delivery = match (data >> MSI_DELIVERY_SHIFT) & MSI_DELIVERY_MASK {
        MSI_DELIVERY_FIXED => Delivery::Fixed,
        MSI_DELIVERY_LOWEST => Delivery::LowestPriority,
        _ => return None,
    };
    let dest = ((address >> MSI_DEST_SHIFT) & MSI_DEST_MASK) as u8;
    Some(message(dest, address & MSI_DEST_LOGICAL != 0, delivery, (data & MSI_VECTOR_MASK) as u8))
}

/// A message on the APIC bus from a device -- an MSI, or an IO-APIC's
/// redirection entry: an 8-bit destination, `logical` or physical, all
/// ones for every CPU whatever mode the APICs are in; a delivery mode; a
/// vector.
pub fn message(dest: u8, logical: bool, delivery: Delivery, vector: u8) -> Ipi {
    let dest = match u32::from(dest) {
        XAPIC_BROADCAST => BROADCAST,
        d => d,
    };
    let destination = if logical { Destination::Logical(dest) } else { Destination::Physical(dest) };
    Ipi { delivery, vector, destination }
}

/// The logical ID an x2APIC has, from its ID: the cluster -- the ID over
/// 16 -- in bits 31:16, and in 15:0 a bit for its place in the cluster.
pub fn logical_id(id: u32) -> u32 {
    ((id >> 4) << 16) | (1 << (id & 0xF))
}

/// What a write did that the caller has to act on.
#[derive(Clone, Copy, Debug)]
pub enum Wrote {
    /// Nothing beyond the register.
    Done,
    /// The task priority changed: CR8 is the same register, and the
    /// caller's CPU keeps it.
    Tpr(u8),
    /// An IPI to send.
    Ipi(Ipi),
    /// A level-triggered interrupt of this vector ended: its EOI goes on to
    /// the IO-APIC, whose line may be up still (`devices::ioapic`).
    Eoi(u8),
}

/// Why a write or read of an APIC register is a #GP, when it is one: an
/// address that is no register, a read of a write-only one or a write of a
/// read-only one, a reserved bit set, or a move between modes the
/// architecture forbids.
#[derive(Clone, Copy, Debug)]
pub struct Refused;

/// The timer.
#[derive(Clone, Copy)]
struct Timer {
    /// The count last written (TMICT); 0 stops the timer.
    initial: u32,
    /// The divide configuration, as written.
    divide: u32,
    /// When the count was loaded, host nanoseconds.
    loaded_ns: u64,
    /// When it next reaches zero -- the next interrupt -- or None when it
    /// has none to come: stopped, or a one-shot count that has run out.
    deadline: Option<u64>,
}

impl Timer {
    /// Nanoseconds a tick of the count, by the divide configuration: 2 to
    /// 128 in powers of two, and 0b1011 is 1.
    fn ns_per_tick(&self) -> u64 {
        let code = (self.divide & 0b11) | ((self.divide & 0b1000) >> 1);
        let divisor = if code == 0b111 { 1 } else { 2u64 << code };
        divisor * BUS_NS_PER_TICK
    }

    /// The whole count in nanoseconds: a period.
    fn span_ns(&self) -> u64 {
        u64::from(self.initial).saturating_mul(self.ns_per_tick())
    }
}

/// One CPU's local APIC.
#[derive(Clone)]
pub struct Lapic {
    id: u32,
    base: u64,
    tpr: u8,
    svr: u32,
    isr: [u32; WORDS as usize],
    irr: [u32; WORDS as usize],
    tmr: [u32; WORDS as usize],
    /// What the error status register reads: the errors latched by the last
    /// write of it.
    esr: u32,
    /// Errors since that write, which the next write latches.
    esr_pending: u32,
    icr: u64,
    lvt: [u32; LVT_ENTRIES],
    timer: Timer,
    /// xAPIC mode's logical ID (bits 31:24) and destination format, as
    /// written; x2APIC mode's logical ID is its ID's, and it has no format.
    ldr: u32,
    dfr: u32,
}

impl Lapic {
    /// The APIC of the CPU whose APIC ID is `id`, as firmware leaves it: in
    /// x2APIC mode, or -- `x2apic` false -- in xAPIC mode, as most PCs'
    /// firmware leaves it. The boot CPU's is on, in the virtual-wire mode a
    /// PC's firmware leaves it in -- LINT0 taking the 8259's interrupts,
    /// LINT1 NMIs -- which is how a guest that uses no APIC at all
    /// (`nolapic`) still takes its interrupts from the 8259; the others'
    /// are off, with every entry masked, until the guest brings them up.
    pub fn new(id: u32, bsp: bool, x2apic: bool) -> Lapic {
        let mut apic = Lapic {
            id,
            base: DEFAULT_BASE | BASE_EN | if x2apic { BASE_EXTD } else { 0 } | if bsp { BASE_BSP } else { 0 },
            tpr: 0,
            svr: SVR_RESET,
            isr: [0; WORDS as usize],
            irr: [0; WORDS as usize],
            tmr: [0; WORDS as usize],
            esr: 0,
            esr_pending: 0,
            icr: 0,
            lvt: [LVT_MASKED; LVT_ENTRIES],
            timer: Timer { initial: 0, divide: 0, loaded_ns: 0, deadline: None },
            ldr: 0,
            dfr: DFR_RESET,
        };
        if bsp {
            apic.svr = SVR_RESET | SVR_ENABLE;
            apic.lvt[LVT_LINT0] = DELIVERY_EXTINT;
            apic.lvt[LVT_LINT1] = DELIVERY_NMI;
        }
        apic
    }

    /// What INIT leaves: every register as out of reset but the ID and the
    /// base MSR -- the mode among them -- which INIT does not touch.
    pub fn init(&mut self) {
        let (id, base) = (self.id, self.base);
        *self = Lapic::new(id, false, true);
        self.base = base;
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// On at all: IA32_APIC_BASE.EN. Off, the CPU takes the 8259's
    /// interrupts on its INTR pin directly, and nothing else.
    pub fn enabled(&self) -> bool {
        self.base & BASE_EN != 0
    }

    /// In xAPIC mode: on, and not extended -- its registers the page's.
    pub fn xapic(&self) -> bool {
        self.base & (BASE_EN | BASE_EXTD) == BASE_EN
    }

    /// What another CPU's message is matched against: see [`Addressing`].
    pub fn addressing(&self) -> Addressing {
        let mode = if !self.enabled() {
            Mode::Off
        } else if self.x2apic() {
            Mode::X2apic
        } else {
            Mode::Xapic
        };
        Addressing {
            mode,
            ldr: (self.ldr >> XAPIC_ID_SHIFT) as u8,
            flat: self.dfr & DFR_MODEL_MASK == DFR_FLAT,
        }
    }

    /// Software-enabled: SVR bit 8. Off, it takes no fixed interrupt, and
    /// its local vector table is masked.
    pub fn software_enabled(&self) -> bool {
        self.enabled() && self.svr & SVR_ENABLE != 0
    }

    /// Whether the 8259's interrupts reach this CPU: through LINT0 set to
    /// ExtINT and unmasked, or -- the APIC off altogether -- on the pin.
    pub fn accepts_extint(&self) -> bool {
        if !self.enabled() {
            return true;
        }
        let lint0 = self.lvt[LVT_LINT0];
        lint0 & LVT_MASKED == 0 && lint0 & LVT_DELIVERY_MASK == DELIVERY_EXTINT
    }

    /// The task priority, bits 7:4 of which are CR8.
    pub fn tpr(&self) -> u8 {
        self.tpr
    }

    /// The guest's CR8 as the CPU has it, which it may have written without
    /// an exit: CR8 is TPR bits 7:4, so a TPR whose top half differs takes
    /// CR8's, and its low half with it goes to 0, as the silicon's does.
    pub fn sync_cr8(&mut self, cr8: u8) {
        if self.tpr >> 4 != cr8 & 0xF {
            self.tpr = (cr8 & 0xF) << 4;
        }
    }

    /// A fixed interrupt of `vector`, edge-triggered, into the request
    /// register: from an IPI, the timer, a self-IPI, an MSI, an IO-APIC's
    /// edge. Dropped by an APIC software-disabled, and an illegal vector --
    /// below 16, the exceptions' -- is an error. False when it was not
    /// taken.
    pub fn accept(&mut self, vector: u8) -> bool {
        self.accept_as(vector, false)
    }

    /// A fixed interrupt of `vector` from an IO-APIC's level-triggered pin:
    /// taken as `accept` takes one, and marked in the trigger-mode register,
    /// so that its EOI goes back to the IO-APIC (`Wrote::Eoi`).
    pub fn accept_level(&mut self, vector: u8) -> bool {
        self.accept_as(vector, true)
    }

    fn accept_as(&mut self, vector: u8, level: bool) -> bool {
        if !self.software_enabled() {
            return false;
        }
        if vector < 16 {
            self.error(ESR_RECEIVE_ILLEGAL_VECTOR);
            return false;
        }
        set_bit(&mut self.irr, vector);
        if level {
            set_bit(&mut self.tmr, vector);
        } else {
            clear_bit(&mut self.tmr, vector);
        }
        true
    }

    /// Several fixed interrupts at once, a bit a vector, and which of them
    /// are level-triggered: what other CPUs posted while this one ran.
    pub fn accept_all(&mut self, words: &[u64; 4], level: &[u64; 4]) {
        for (w, (&bits, &levels)) in words.iter().zip(level.iter()).enumerate() {
            let mut left = bits;
            while left != 0 {
                let bit = left.trailing_zeros();
                left &= left - 1;
                self.accept_as((w as u32 * 64 + bit) as u8, levels & (1 << bit) != 0);
            }
        }
    }

    /// Whether `vector` is requested and not yet taken: an edge sent for it
    /// now would be the same request again, and lost.
    pub fn requested(&self, vector: u8) -> bool {
        test_bit(&self.irr, vector)
    }

    /// The processor priority: the task priority, or the class of the
    /// highest interrupt in service when that is higher.
    fn ppr(&self) -> u8 {
        let isrv = highest(&self.isr).unwrap_or(0);
        if self.tpr >> 4 >= isrv >> 4 { self.tpr } else { isrv & 0xF0 }
    }

    /// The vector of the highest-priority interrupt requested that the
    /// processor priority lets through: the one to inject when the CPU can
    /// take an interrupt.
    pub fn pending(&self) -> Option<u8> {
        if !self.software_enabled() {
            return None;
        }
        let vector = highest(&self.irr)?;
        (vector >> 4 > self.ppr() >> 4).then_some(vector)
    }

    /// `vector` was injected: out of the request register and into service,
    /// until the guest's EOI.
    pub fn acknowledge(&mut self, vector: u8) {
        clear_bit(&mut self.irr, vector);
        set_bit(&mut self.isr, vector);
    }

    /// The highest interrupt in service is done: its vector, when it was
    /// level-triggered -- an IO-APIC's, whose EOI goes on to it.
    fn eoi(&mut self) -> Option<u8> {
        let vector = highest(&self.isr)?;
        clear_bit(&mut self.isr, vector);
        let level = test_bit(&self.tmr, vector);
        clear_bit(&mut self.tmr, vector);
        level.then_some(vector)
    }

    /// An error: into what the next write of ESR latches, and -- the error
    /// entry unmasked -- its interrupt.
    fn error(&mut self, bits: u32) {
        let new = bits & !self.esr_pending;
        self.esr_pending |= bits;
        let lvt = self.lvt[LVT_ERROR];
        if new != 0 && lvt & LVT_MASKED == 0 && self.software_enabled() && lvt as u8 >= 16 {
            set_bit(&mut self.irr, lvt as u8);
        }
    }

    /// Whether the timer has run out by `now`: its interrupt into the
    /// request register, unless its entry is masked, and in periodic mode
    /// the next period begun. A periodic timer that fell behind is owed its
    /// periods, handed over one at a time and each only once the last is no
    /// longer waiting in the request register -- a second's worth at most.
    /// True when an interrupt was requested.
    pub fn timer(&mut self, now: u64) -> bool {
        let Some(deadline) = self.timer.deadline else { return false };
        if now < deadline {
            return false;
        }
        let lvt = self.lvt[LVT_TIMER];
        let vector = lvt as u8;
        let periodic = lvt & TIMER_PERIODIC != 0;
        if periodic && test_bit(&self.irr, vector) {
            /* Still waiting to be taken: the next period is owed, and
             * delivered once it has been. */
            return false;
        }
        self.timer.deadline = if periodic {
            let period = self.timer.span_ns().max(MIN_PERIOD_NS);
            let behind = now.saturating_sub(deadline);
            let next = if behind > MOST_OWED_NS {
                now + period
            } else {
                deadline.saturating_add(period)
            };
            Some(next)
        } else {
            None
        };
        if lvt & LVT_MASKED != 0 {
            return false;
        }
        self.accept(vector)
    }

    /// When the timer next runs out, host nanoseconds: what a halted CPU
    /// sleeps until at most. None with no interrupt to come -- stopped, run
    /// out, or masked -- and none for a periodic timer whose last period
    /// still waits in the request register: `timer` hands over the next only
    /// once that one is taken, which a CPU halted with it requested does not
    /// do until something else wakes it -- the priorities holding it back,
    /// say. Its deadline is in the past by then, and a halted CPU that slept
    /// until it would not sleep at all: the loop spun, a host CPU's whole
    /// time, for as long as the guest stayed so (hv-fuzz found it).
    pub fn next_timer_ns(&self) -> Option<u64> {
        let lvt = self.lvt[LVT_TIMER];
        if lvt & LVT_MASKED != 0 || (lvt & TIMER_PERIODIC != 0 && test_bit(&self.irr, lvt as u8)) {
            return None;
        }
        self.timer.deadline
    }

    /// The count as it reads now: counting down from the last one written,
    /// 0 once a one-shot has run out, and round again for a periodic one.
    fn current_count(&self, now: u64) -> u32 {
        let t = &self.timer;
        if t.initial == 0 {
            return 0;
        }
        let ticks = now.saturating_sub(t.loaded_ns) / t.ns_per_tick();
        let initial = u64::from(t.initial);
        if self.lvt[LVT_TIMER] & TIMER_PERIODIC != 0 {
            (initial - ticks % initial) as u32
        } else if ticks >= initial {
            0
        } else {
            (initial - ticks) as u32
        }
    }

    /// A read of `msr`, at `now`: its value, or [`Refused`] -- a #GP.
    pub fn rdmsr(&self, msr: u32, now: u64) -> Result<u64, Refused> {
        if msr == MSR_APIC_BASE {
            return Ok(self.base);
        }
        /* Every other register is x2APIC mode's: in xAPIC mode, or with the
         * APIC off, an MSR that is not there. */
        if !self.x2apic() {
            return Err(Refused);
        }
        let reg = msr - MSR_X2APIC_FIRST;
        if reg == REG_ICR {
            return Ok(self.icr);
        }
        self.read_register(reg, now, false).map(u64::from).ok_or(Refused)
    }

    /// The 32-bit register `reg` -- by its x2APIC MSR's offset -- as the mode
    /// reads it, `xapic` or not, at `now`; None for no register, or one
    /// write-only. The ICR's two halves are xAPIC mode's; x2APIC mode reads
    /// it whole (`rdmsr`).
    fn read_register(&self, reg: u32, now: u64, xapic: bool) -> Option<u32> {
        let value = match reg {
            REG_ID if xapic => self.id << XAPIC_ID_SHIFT,
            REG_ID => self.id,
            REG_VERSION => VERSION,
            REG_TPR => u32::from(self.tpr),
            /* xAPIC mode's arbitration priority and remote read, which no
             * guest of an integrated APIC asks for: nothing. */
            REG_APR | REG_RRD if xapic => 0,
            REG_PPR => u32::from(self.ppr()),
            REG_LDR if xapic => self.ldr,
            REG_LDR => logical_id(self.id),
            REG_DFR if xapic => self.dfr,
            REG_SVR => self.svr,
            r if (REG_ISR..REG_ISR + WORDS).contains(&r) => self.isr[(r - REG_ISR) as usize],
            r if (REG_TMR..REG_TMR + WORDS).contains(&r) => self.tmr[(r - REG_TMR) as usize],
            r if (REG_IRR..REG_IRR + WORDS).contains(&r) => self.irr[(r - REG_IRR) as usize],
            REG_ESR => self.esr,
            /* The delivery-status bit reads idle: an IPI is sent as it is
             * written. */
            REG_ICR if xapic => self.icr as u32,
            REG_ICR_HIGH if xapic => (self.icr >> 32) as u32,
            r if (REG_LVT_TIMER..=REG_LVT_ERROR).contains(&r) => self.lvt[(r - REG_LVT_TIMER) as usize],
            REG_TIMER_INITIAL => self.timer.initial,
            REG_TIMER_CURRENT => self.current_count(now),
            REG_TIMER_DIVIDE => self.timer.divide,
            /* EOI and SELF IPI are write-only; the rest of the range is no
             * register at all. */
            _ => return None,
        };
        Some(value)
    }

    /// A read of `size` bytes at `offset` in the xAPIC page, at `now`: the
    /// bytes of the register there, and nothing -- zero -- past its 32 bits
    /// or where there is no register. The caller keeps the size's bytes.
    pub fn mmio_read(&self, offset: u32, size: u8, now: u64) -> u32 {
        let within = offset & ((1 << XAPIC_STRIDE_SHIFT) - 1);
        if within + u32::from(size) > XAPIC_REG_BYTES {
            return 0;
        }
        let value = self.read_register(offset >> XAPIC_STRIDE_SHIFT, now, true).unwrap_or(0);
        value >> (8 * within)
    }

    /// A write of `size` bytes of `value` at `offset` in the xAPIC page, at
    /// `now`: what it did that the caller has to act on. A register is
    /// written whole or not at all -- an access of another size, or not at a
    /// register's start, is dropped, as is a write of no writable register:
    /// xAPIC mode has no #GP to answer one with.
    pub fn mmio_write(&mut self, offset: u32, size: u8, value: u32, now: u64) -> Wrote {
        if offset & ((1 << XAPIC_STRIDE_SHIFT) - 1) != 0 || u32::from(size) != XAPIC_REG_BYTES {
            return Wrote::Done;
        }
        self.write_register(offset >> XAPIC_STRIDE_SHIFT, value, now, true).unwrap_or(Wrote::Done)
    }

    /// In x2APIC mode: on, and extended.
    fn x2apic(&self) -> bool {
        self.base & (BASE_EN | BASE_EXTD) == BASE_EN | BASE_EXTD
    }

    /// A write of `value` to `msr`, at `now`: what it did that the caller
    /// has to act on, or [`Refused`] -- a #GP, and nothing written.
    pub fn wrmsr(&mut self, msr: u32, value: u64, now: u64) -> Result<Wrote, Refused> {
        if msr == MSR_APIC_BASE {
            return self.write_base(value);
        }
        if !self.x2apic() {
            return Err(Refused);
        }
        let reg = msr - MSR_X2APIC_FIRST;
        if reg == REG_ICR {
            return self.write_icr(value, false).map(|ipi| ipi.map_or(Wrote::Done, Wrote::Ipi));
        }
        /* Bits 63:32 are reserved in every register but the ICR. */
        if value >> 32 != 0 {
            return Err(Refused);
        }
        self.write_register(reg, value as u32, now, false)
    }

    /// A write of `v` to the 32-bit register `reg` -- by its x2APIC MSR's
    /// offset -- in the mode it is in, `xapic` or not: what it did, or
    /// [`Refused`] for what x2APIC mode faults -- a read-only register, a
    /// reserved bit, a value it wants to be 0 -- and xAPIC mode drops.
    fn write_register(&mut self, reg: u32, v: u32, now: u64, xapic: bool) -> Result<Wrote, Refused> {
        match reg {
            REG_TPR => {
                if v & !0xFF != 0 && !xapic {
                    return Err(Refused);
                }
                self.tpr = v as u8;
                return Ok(Wrote::Tpr(self.tpr));
            }
            REG_EOI => {
                /* x2APIC mode wants 0 written, and faults anything else;
                 * xAPIC mode takes any value. */
                if v != 0 && !xapic {
                    return Err(Refused);
                }
                if let Some(vector) = self.eoi() {
                    return Ok(Wrote::Eoi(vector));
                }
            }
            REG_LDR if xapic => self.ldr = v & LDR_MASK,
            REG_DFR if xapic => self.dfr = (v & DFR_MODEL_MASK) | !DFR_MODEL_MASK,
            REG_ICR if xapic => {
                /* The low half sends, with the high half's destination. */
                let icr = (self.icr & !u64::from(u32::MAX)) | u64::from(v);
                return self.write_icr(icr, true).map(|ipi| ipi.map_or(Wrote::Done, Wrote::Ipi));
            }
            REG_ICR_HIGH if xapic => {
                self.icr = (self.icr & u64::from(u32::MAX)) | (u64::from(v) << 32);
            }
            REG_SVR => {
                self.svr = v & SVR_WRITABLE;
                if self.svr & SVR_ENABLE == 0 {
                    /* Software-disabled: every entry of the local vector
                     * table masked, and kept so until the APIC is back on. */
                    for entry in self.lvt.iter_mut() {
                        *entry |= LVT_MASKED;
                    }
                }
            }
            REG_ESR => {
                /* A write latches the errors since the last one; x2APIC
                 * mode wants the write itself to be 0. */
                if v != 0 && !xapic {
                    return Err(Refused);
                }
                self.esr = self.esr_pending;
                self.esr_pending = 0;
            }
            r if (REG_LVT_TIMER..=REG_LVT_ERROR).contains(&r) => {
                let i = (r - REG_LVT_TIMER) as usize;
                let mut entry = v & LVT_WRITABLE[i];
                if !self.software_enabled() {
                    entry |= LVT_MASKED;
                }
                self.lvt[i] = entry;
            }
            REG_TIMER_INITIAL => {
                self.timer.initial = v;
                self.timer.loaded_ns = now;
                self.timer.deadline = (v != 0).then(|| {
                    let span = if self.lvt[LVT_TIMER] & TIMER_PERIODIC != 0 {
                        self.timer.span_ns().max(MIN_PERIOD_NS)
                    } else {
                        self.timer.span_ns()
                    };
                    now.saturating_add(span)
                });
            }
            REG_TIMER_DIVIDE => {
                self.timer.divide = v & DIVIDE_WRITABLE;
            }
            REG_SELF_IPI if !xapic => {
                /* The vector alone, in bits 7:0. */
                if v & !0xFF != 0 {
                    return Err(Refused);
                }
                return Ok(Wrote::Ipi(Ipi {
                    delivery: Delivery::Fixed,
                    vector: v as u8,
                    destination: Destination::SelfOnly,
                }));
            }
            /* Read-only -- the ID, the version, the priorities computed,
             * the logical ID, the ISR, TMR and IRR, the current count --
             * or no register at all. */
            _ => return Err(Refused),
        }
        Ok(Wrote::Done)
    }

    /// A write of IA32_APIC_BASE. Its mode bits move between off (neither),
    /// xAPIC (EN) and x2APIC (EN and EXTD), and the architecture forbids
    /// two moves -- x2APIC straight to xAPIC, and off straight to x2APIC --
    /// and EXTD without EN: each a #GP, as are the reserved bits. The BSP
    /// bit is the platform's, and a write keeps it as it was. And the page
    /// stays where firmware put it: a guest moving it would have the page it
    /// left be RAM to one of its CPUs and a device to another, which no
    /// guest does -- a #GP.
    fn write_base(&mut self, value: u64) -> Result<Wrote, Refused> {
        if value & !(BASE_ADDRESS | BASE_BSP | BASE_EXTD | BASE_EN) != 0 {
            return Err(Refused);
        }
        if value & BASE_ADDRESS != DEFAULT_BASE {
            return Err(Refused);
        }
        let value = (value & !BASE_BSP) | (self.base & BASE_BSP);
        let old = self.base & (BASE_EN | BASE_EXTD);
        let new = value & (BASE_EN | BASE_EXTD);
        let x2 = BASE_EN | BASE_EXTD;
        match (old, new) {
            (_, BASE_EXTD) => return Err(Refused),
            (o, n) if o == x2 && n == BASE_EN => return Err(Refused),
            (0, n) if n == x2 => return Err(Refused),
            _ => {}
        }
        self.base = value;
        if new == 0 && old != 0 {
            /* Off: the APIC as out of reset, but for its ID and this MSR,
             * which is what turning one off and on again gives. */
            self.init();
            self.base = value;
        }
        Ok(Wrote::Done)
    }

    /// A write of the interrupt command register: the IPI it sends, or a
    /// #GP for a reserved bit set -- in x2APIC mode; xAPIC mode ignores them.
    /// An IPI with a vector no fixed interrupt may have, or a delivery mode
    /// no IPI has, is an error on this APIC, and sends nothing.
    fn write_icr(&mut self, value: u64, xapic: bool) -> Result<Option<Ipi>, Refused> {
        if value & ICR_RESERVED != 0 && !xapic {
            return Err(Refused);
        }
        let value = value & !ICR_BUSY;
        self.icr = value;
        let vector = (value & ICR_VECTOR) as u8;
        let delivery = match (value >> ICR_DELIVERY_SHIFT) & ICR_DELIVERY_MASK {
            0 => Delivery::Fixed,
            1 => Delivery::LowestPriority,
            2 => Delivery::Smi,
            4 => Delivery::Nmi,
            5 if value & ICR_ASSERT == 0 && value & ICR_LEVEL != 0 => Delivery::InitDeassert,
            5 => Delivery::Init,
            6 => Delivery::Startup,
            /* 3 and 7 are reserved, and ExtINT is not a mode an IPI has:
             * nothing is sent, and the send is an error. */
            _ => {
                self.error(ESR_SEND_ILLEGAL_VECTOR);
                return Ok(None);
            }
        };
        if matches!(delivery, Delivery::Fixed | Delivery::LowestPriority) && vector < 16 {
            self.error(ESR_SEND_ILLEGAL_VECTOR);
            return Ok(None);
        }
        let dest = if xapic {
            (value >> XAPIC_ICR_DEST_SHIFT) as u32
        } else {
            (value >> ICR_DEST_SHIFT) as u32
        };
        let destination = match (value >> ICR_SHORTHAND_SHIFT) & ICR_SHORTHAND_MASK {
            1 => Destination::SelfOnly,
            2 => Destination::All,
            3 => Destination::AllButSelf,
            _ if value & ICR_LOGICAL != 0 => Destination::Logical(dest),
            _ => Destination::Physical(dest),
        };
        Ok(Some(Ipi { delivery, vector, destination }))
    }

    /// The registers a report is read by: the in-service and requested
    /// vectors at their highest, the priorities, and whether it is on.
    pub fn state(&self) -> (Option<u8>, Option<u8>, u8, u8, bool) {
        (highest(&self.isr), highest(&self.irr), self.tpr, self.ppr(), self.software_enabled())
    }
}

fn set_bit(words: &mut [u32; WORDS as usize], vector: u8) {
    words[usize::from(vector / 32)] |= 1 << (vector % 32);
}

fn clear_bit(words: &mut [u32; WORDS as usize], vector: u8) {
    words[usize::from(vector / 32)] &= !(1 << (vector % 32));
}

fn test_bit(words: &[u32; WORDS as usize], vector: u8) -> bool {
    words[usize::from(vector / 32)] & (1 << (vector % 32)) != 0
}

/// The highest vector set among 256 bits.
fn highest(words: &[u32; WORDS as usize]) -> Option<u8> {
    words.iter().enumerate().rev()
        .find(|(_, &w)| w != 0)
        .map(|(i, &w)| (i as u32 * 32 + 31 - w.leading_zeros()) as u8)
}
