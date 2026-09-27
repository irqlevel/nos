//! A guest CPU's local APIC -- as an x2APIC, reached only through MSRs.
//!
//! A guest of more than one CPU cannot do without a local APIC: it is what
//! one CPU interrupts another through (an IPI), what starts the others at
//! all (INIT and the start-up IPI), and each CPU's own timer. It has two
//! ways in. The xAPIC is a page of MMIO at 0xFEE00000, and emulating a page
//! of MMIO means decoding the instruction that touched it -- `mov`, and
//! every form a compiler picks for it -- which is the instruction emulator
//! this hypervisor exists without. The x2APIC is the same registers as MSRs
//! 0x800-0x8FF, and an MSR access is an exit whose register, value and
//! length the CPU hands over already. So a guest is given an x2APIC, turned
//! on as firmware on a machine with more than 255 CPUs leaves one, and never
//! the page: its every register is an intercepted `rdmsr` or `wrmsr` answered
//! here, and a guest that turns x2APIC mode off to use the page is stopped
//! and told why (`Wrote::Xapic`) -- Linux never does, unless it is told to
//! use an IO-APIC this machine does not have (`noapic`, which the loader
//! passes).
//!
//! What is here is the register file and what each register does to the
//! others: the ID and the logical ID derived from it, the task and
//! processor priorities, in-service and request registers, EOI, the
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
/// no suppression of EOI broadcasts.
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

/// The broadcast destination, physical or logical.
const BROADCAST: u32 = u32::MAX;

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
    /// The CPU whose x2APIC ID is this; `BROADCAST` for every CPU.
    Physical(u32),
    /// The CPUs whose logical ID -- a cluster (bits 31:16) and a bit in it
    /// (15:0) -- matches: the cluster the same and a bit in common.
    Logical(u32),
    SelfOnly,
    All,
    AllButSelf,
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
    /// Whether the CPU with x2APIC ID `id` is among those this is for, the
    /// sender's being `from`.
    pub fn reaches(&self, id: u32, from: u32) -> bool {
        match self.destination {
            Destination::Physical(BROADCAST) | Destination::Logical(BROADCAST) | Destination::All => true,
            Destination::Physical(dest) => dest == id,
            Destination::Logical(dest) => {
                let ldr = logical_id(id);
                ldr >> 16 == dest >> 16 && ldr & dest & 0xFFFF != 0
            }
            Destination::SelfOnly => id == from,
            Destination::AllButSelf => id != from,
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
    let dest = ((address >> MSI_DEST_SHIFT) & MSI_DEST_MASK) as u32;
    let destination = if address & MSI_DEST_LOGICAL != 0 {
        Destination::Logical(dest)
    } else {
        Destination::Physical(dest)
    };
    Some(Ipi { delivery, vector: (data & MSI_VECTOR_MASK) as u8, destination })
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
    /// The guest took the APIC out of x2APIC mode into xAPIC mode, the page
    /// of MMIO this hypervisor does not emulate: the guest is to be stopped,
    /// and told why.
    Xapic,
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
}

impl Lapic {
    /// The APIC of the CPU whose x2APIC ID is `id`, as firmware leaves it:
    /// in x2APIC mode. The boot CPU's is on, in the virtual-wire mode a PC's
    /// firmware leaves it in -- LINT0 taking the 8259's interrupts, LINT1
    /// NMIs -- which is how a guest that uses no APIC at all (`nolapic`)
    /// still takes its interrupts from the 8259; the others' are off, with
    /// every entry masked, until the guest brings them up.
    pub fn new(id: u32, bsp: bool) -> Lapic {
        let mut apic = Lapic {
            id,
            base: DEFAULT_BASE | BASE_EN | BASE_EXTD | if bsp { BASE_BSP } else { 0 },
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
        *self = Lapic::new(id, false);
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

    /// A fixed interrupt of `vector`, into the request register: from an
    /// IPI, the timer, or a self-IPI. Dropped by an APIC software-disabled,
    /// and an illegal vector -- below 16, the exceptions' -- is an error.
    /// False when it was not taken.
    pub fn accept(&mut self, vector: u8) -> bool {
        if !self.software_enabled() {
            return false;
        }
        if vector < 16 {
            self.error(ESR_RECEIVE_ILLEGAL_VECTOR);
            return false;
        }
        set_bit(&mut self.irr, vector);
        true
    }

    /// Several fixed interrupts at once, a bit a vector: what other CPUs
    /// posted while this one ran.
    pub fn accept_all(&mut self, words: &[u64; 4]) {
        for (w, &bits) in words.iter().enumerate() {
            let mut left = bits;
            while left != 0 {
                let bit = left.trailing_zeros();
                left &= left - 1;
                self.accept((w as u32 * 64 + bit) as u8);
            }
        }
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

    /// The highest interrupt in service is done.
    fn eoi(&mut self) {
        if let Some(vector) = highest(&self.isr) {
            clear_bit(&mut self.isr, vector);
            /* A level-triggered interrupt's EOI goes on to its IO-APIC,
             * and there is none: every interrupt here is an edge. */
            clear_bit(&mut self.tmr, vector);
        }
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
    /// out, or masked.
    pub fn next_timer_ns(&self) -> Option<u64> {
        if self.lvt[LVT_TIMER] & LVT_MASKED != 0 {
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
        /* Every other register is x2APIC mode's: with the APIC off, an MSR
         * that is not there. */
        if !self.x2apic() {
            return Err(Refused);
        }
        let reg = msr - MSR_X2APIC_FIRST;
        let value = match reg {
            REG_ID => self.id,
            REG_VERSION => VERSION,
            REG_TPR => u32::from(self.tpr),
            REG_PPR => u32::from(self.ppr()),
            REG_LDR => logical_id(self.id),
            REG_SVR => self.svr,
            r if (REG_ISR..REG_ISR + WORDS).contains(&r) => self.isr[(r - REG_ISR) as usize],
            r if (REG_TMR..REG_TMR + WORDS).contains(&r) => self.tmr[(r - REG_TMR) as usize],
            r if (REG_IRR..REG_IRR + WORDS).contains(&r) => self.irr[(r - REG_IRR) as usize],
            REG_ESR => self.esr,
            REG_ICR => return Ok(self.icr),
            r if (REG_LVT_TIMER..=REG_LVT_ERROR).contains(&r) => self.lvt[(r - REG_LVT_TIMER) as usize],
            REG_TIMER_INITIAL => self.timer.initial,
            REG_TIMER_CURRENT => self.current_count(now),
            REG_TIMER_DIVIDE => self.timer.divide,
            /* EOI and SELF IPI are write-only; the rest of the range is no
             * register at all. */
            _ => return Err(Refused),
        };
        Ok(u64::from(value))
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
            return self.write_icr(value).map(|ipi| ipi.map_or(Wrote::Done, Wrote::Ipi));
        }
        /* Bits 63:32 are reserved in every register but the ICR. */
        if value >> 32 != 0 {
            return Err(Refused);
        }
        let v = value as u32;
        match reg {
            REG_TPR => {
                if v & !0xFF != 0 {
                    return Err(Refused);
                }
                self.tpr = v as u8;
                return Ok(Wrote::Tpr(self.tpr));
            }
            REG_EOI => {
                /* x2APIC mode wants 0 written, and faults anything else. */
                if v != 0 {
                    return Err(Refused);
                }
                self.eoi();
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
                if v != 0 {
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
            REG_SELF_IPI => {
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
    /// bit is the platform's, and a write keeps it as it was. Into xAPIC
    /// mode is allowed and not emulated: the caller stops the guest
    /// (`Wrote::Xapic`).
    fn write_base(&mut self, value: u64) -> Result<Wrote, Refused> {
        if value & !(BASE_ADDRESS | BASE_BSP | BASE_EXTD | BASE_EN) != 0 {
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
        Ok(if new == BASE_EN { Wrote::Xapic } else { Wrote::Done })
    }

    /// A write of the interrupt command register: the IPI it sends, or a
    /// #GP for a reserved bit set. An IPI with a vector no fixed interrupt
    /// may have, or a delivery mode no IPI has, is an error on this APIC,
    /// and sends nothing.
    fn write_icr(&mut self, value: u64) -> Result<Option<Ipi>, Refused> {
        if value & ICR_RESERVED != 0 {
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
        let dest = (value >> ICR_DEST_SHIFT) as u32;
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
