//! A CPU's local APIC, and its mailbox: what the other CPUs and the devices
//! had sent it and it had not taken.

use crate::wire::{In, Out};
use crate::{Error, Result};

/// The words of a 256-bit vector register: IRR, ISR, TMR.
pub const WORDS: usize = 8;
/// The most local vector table entries.
pub const LVT_MAX: usize = 8;

/// A local APIC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lapic {
    /// Its ID: the CPU's place in the guest.
    pub id: u32,
    /// IA32_APIC_BASE: its page, EN, EXTD, BSP.
    pub base: u64,
    pub tpr: u8,
    pub svr: u32,
    pub isr: [u32; WORDS],
    pub irr: [u32; WORDS],
    pub tmr: [u32; WORDS],
    pub esr: u32,
    pub esr_pending: u32,
    pub icr: u64,
    /// The local vector table: its first `lvt_len` entries.
    pub lvt: [u32; LVT_MAX],
    pub lvt_len: u8,
    /// The timer: its count and divide configuration as written; the ticks
    /// it had counted by the time it last (re)started counting, and how long
    /// before the pause that was; how long after the pause it was next due,
    /// 0 for at once, or None for no edge to come.
    pub timer_initial: u32,
    pub timer_divide: u32,
    pub timer_counted: u64,
    pub timer_loaded_ago_ns: u64,
    pub timer_due_in_ns: Option<u64>,
    /// IA32_TSC_DEADLINE as written, in the guest's TSC: 0 for none.
    pub tsc_deadline: u64,
    /// xAPIC mode's logical destination and destination format.
    pub ldr: u32,
    pub dfr: u32,
}

/// What waited in a CPU's mailbox.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mail {
    /// Fixed interrupts, a bit a vector, edge-triggered and level.
    pub edge: [u64; 4],
    pub level: [u64; 4],
    pub nmi: bool,
    pub init: bool,
    /// A start-up IPI's vector.
    pub sipi: Option<u8>,
}

/// IA32_APIC_BASE's bits there are: BSP (8), EXTD (10), EN (11) and the
/// page's address up to 52 bits.
const BASE_BITS: u64 = 0x000F_FFFF_FFFF_F000 | (1 << 8) | (1 << 10) | (1 << 11);

impl Lapic {
    pub(crate) fn put(&self, o: &mut Out) {
        o.u32(self.id);
        o.u64(self.base);
        o.u8(self.tpr);
        o.u32(self.svr);
        for w in self.isr.iter().chain(&self.irr).chain(&self.tmr) {
            o.u32(*w);
        }
        o.u32(self.esr);
        o.u32(self.esr_pending);
        o.u64(self.icr);
        o.u8(self.lvt_len);
        for v in &self.lvt {
            o.u32(*v);
        }
        o.u32(self.timer_initial);
        o.u32(self.timer_divide);
        o.u64(self.timer_counted);
        o.u64(self.timer_loaded_ago_ns);
        o.opt(self.timer_due_in_ns, |o, v| o.u64(v));
        o.u64(self.tsc_deadline);
        o.u32(self.ldr);
        o.u32(self.dfr);
    }

    pub(crate) fn get(r: &mut In) -> Result<Lapic> {
        let id = r.u32("an APIC's ID")?;
        let base = r.u64("an APIC's base")?;
        if base & !BASE_BITS != 0 {
            return Err(Error("an APIC base with bits no APIC has"));
        }
        let tpr = r.u8("an APIC's TPR")?;
        let svr = r.u32("an APIC's SVR")?;
        let mut words = [[0u32; WORDS]; 3];
        for reg in &mut words {
            for w in reg.iter_mut() {
                *w = r.u32("an APIC's vector register")?;
            }
        }
        let [isr, irr, tmr] = words;
        let esr = r.u32("an APIC's ESR")?;
        let esr_pending = r.u32("an APIC's ESR")?;
        let icr = r.u64("an APIC's ICR")?;
        let lvt_len = r.u8("an APIC's LVT")?;
        if usize::from(lvt_len) > LVT_MAX {
            return Err(Error("more LVT entries than an APIC has"));
        }
        let mut lvt = [0u32; LVT_MAX];
        for v in &mut lvt {
            *v = r.u32("an LVT entry")?;
        }
        let timer_initial = r.u32("the timer's count")?;
        let timer_divide = r.u32("the timer's divide")?;
        let timer_counted = r.u64("the timer's ticks")?;
        let timer_loaded_ago_ns = r.u64("the timer's start")?;
        let timer_due_in_ns = r.opt("the timer's edge", |r| r.u64("the timer's edge"))?;
        let tsc_deadline = r.u64("the TSC deadline")?;
        let ldr = r.u32("an APIC's LDR")?;
        let dfr = r.u32("an APIC's DFR")?;
        Ok(Lapic { id, base, tpr, svr, isr, irr, tmr, esr, esr_pending, icr, lvt, lvt_len, timer_initial, timer_divide,
                   timer_counted, timer_loaded_ago_ns, timer_due_in_ns, tsc_deadline, ldr, dfr })
    }
}

impl Mail {
    pub(crate) fn put(&self, o: &mut Out) {
        for w in self.edge.iter().chain(&self.level) {
            o.u64(*w);
        }
        o.bool(self.nmi);
        o.bool(self.init);
        o.opt(self.sipi, |o, v| o.u8(v));
    }

    pub(crate) fn get(r: &mut In) -> Result<Mail> {
        let mut words = [[0u64; 4]; 2];
        for reg in &mut words {
            for w in reg.iter_mut() {
                *w = r.u64("a mailbox's vectors")?;
            }
        }
        let [edge, level] = words;
        let nmi = r.bool("a mailbox's NMI")?;
        let init = r.bool("a mailbox's INIT")?;
        let sipi = r.opt("a mailbox's start-up IPI", |r| r.u8("a start-up vector"))?;
        Ok(Mail { edge, level, nmi, init, sipi })
    }
}
