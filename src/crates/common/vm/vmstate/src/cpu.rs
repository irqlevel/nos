//! A CPU's state, in a form of neither extension's: AMD-V's VMCB and VT-x's
//! VMCS are each read into it, and each written back from it, by the
//! backend that ran the CPU (`hv::svm`, `hv::vmx`).

use alloc::vec::Vec;

use crate::apic::{Lapic, Mail};
use crate::wire::{In, Out};
use crate::{Error, Result};

/// The most bytes of XSAVE's area a state carries: its legacy region, its
/// header and the components up to AVX's are 832; the room is for more.
pub const XSAVE_MAX: usize = 4096;

/// A segment register, hidden part and all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Segment {
    pub selector: u16,
    pub base: u64,
    pub limit: u32,
    /// The descriptor's attributes as AMD-V's VMCB packs them, which is the
    /// descriptor's own bits: its type, S, DPL and P (bits 40-47) in the low
    /// eight, AVL, L, D/B and G (52-55) in the next four. A segment VT-x
    /// calls unusable -- a null selector loaded -- is 0.
    pub attrib: u16,
}

/// GDTR or IDTR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub base: u64,
    pub limit: u16,
}

/// What a CPU is doing, as the run loop has it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activity {
    Running,
    /// Halted with interrupts on: until one it can take is pending.
    Halted,
    /// Halted with interrupts off: until an NMI or an INIT.
    Asleep,
    /// Reset, or never started: until a start-up IPI.
    WaitForSipi,
}

/// What an event on its way into the guest is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// An external interrupt.
    External,
    Nmi,
    /// A hardware exception.
    Exception,
    /// `int n`.
    SoftInt,
}

/// An event the CPU is to be given at its next entry: one whose delivery an
/// exit cut short, or one the host queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub vector: u8,
    pub error: Option<u32>,
}

/// What a CPU's extension keeps of it -- its registers, and what is on its
/// way into it: what each backend reads out of its VMCB or VMCS and writes
/// back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Regs {
    /// RAX to R15, in the order the encoding numbers them: RAX, RCX, RDX,
    /// RBX, RSP, RBP, RSI, RDI, R8-R15.
    pub gprs: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    /// CR0 and CR4 as the guest reads them: the host's own bits of them
    /// (MCE, VMX's fixed ones) are the host's to set again.
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    /// Its task priority, CR8: bits 3:0.
    pub cr8: u8,
    /// EFER as the guest has it: without AMD-V's SVME, which the host's.
    pub efer: u64,
    pub pat: u64,
    /// Its privilege level: SS's DPL, which AMD-V keeps apart.
    pub cpl: u8,
    /// ES, CS, SS, DS, FS, GS, LDTR, TR.
    pub segs: [Segment; 8],
    pub gdtr: Table,
    pub idtr: Table,
    pub star: u64,
    pub lstar: u64,
    pub cstar: u64,
    pub sfmask: u64,
    pub kernel_gs_base: u64,
    pub sysenter_cs: u64,
    pub sysenter_esp: u64,
    pub sysenter_eip: u64,
    /// DR0-DR3, DR6, DR7.
    pub dr: [u64; 4],
    pub dr6: u64,
    pub dr7: u64,
    pub tsc_aux: u64,
    pub spec_ctrl: u64,
    /// Fixed and variable MTRRs and DEF_TYPE, saved but not applied to the
    /// nested page table.
    pub mtrrs: crate::Mtrrs,
    /// XCR0 and PKRU, and the x87, SSE and AVX registers in XSAVE's
    /// standard form -- the legacy region, the header, AVX's component.
    pub xcr0: u64,
    pub pkru: u32,
    pub xsave: Vec<u8>,
    /// An event to give it at its next entry.
    pub event: Option<Event>,
    /// In an interrupt shadow: the instruction after STI or MOV SS.
    pub shadow: bool,
    /// In its NMI handler: none taken until its IRET.
    pub nmi_blocked: bool,
    /// Debug exceptions pending, as DR6 has their bits (VT-x keeps them in
    /// its VMCS; AMD-V delivers them at once, and has none).
    pub pending_debug: u64,
}

/// One CPU: its extension's part, and its run loop's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub regs: Regs,
    /// An NMI sent it and not yet given: latched, one at a time.
    pub nmi_pending: bool,
    /// An exception of its own the host took and owes it -- #DB, #AC -- with
    /// its error code.
    pub exception: Option<(u8, Option<u32>)>,
    pub activity: Activity,
    /// The last interrupt it took came from the 8259: when the 8259 and its
    /// APIC both have one, they take turns.
    pub last_from_pic: bool,
    /// KVM's PV EOI MSR as the guest wrote it.
    pub pv_eoi: u64,
    pub lapic: Lapic,
    pub mail: Mail,
}

impl Activity {
    fn code(self) -> u8 {
        match self {
            Activity::Running => 0,
            Activity::Halted => 1,
            Activity::Asleep => 2,
            Activity::WaitForSipi => 3,
        }
    }

    fn from_code(code: u8) -> Result<Activity> {
        Ok(match code {
            0 => Activity::Running,
            1 => Activity::Halted,
            2 => Activity::Asleep,
            3 => Activity::WaitForSipi,
            _ => return Err(Error("an activity no CPU has")),
        })
    }
}

impl EventKind {
    fn code(self) -> u8 {
        match self {
            EventKind::External => 0,
            EventKind::Nmi => 2,
            EventKind::Exception => 3,
            EventKind::SoftInt => 4,
        }
    }

    fn from_code(code: u8) -> Result<EventKind> {
        Ok(match code {
            0 => EventKind::External,
            2 => EventKind::Nmi,
            3 => EventKind::Exception,
            4 => EventKind::SoftInt,
            _ => return Err(Error("an event of a kind there is none of")),
        })
    }
}

/// The exceptions that push an error code: #DF, #TS, #NP, #SS, #GP, #PF,
/// #AC, #CP, #VC, #SX.
pub const ERROR_CODE_VECTORS: u32 = (1 << 8) | (1 << 10) | (1 << 11) | (1 << 12) | (1 << 13) | (1 << 14)
    | (1 << 17) | (1 << 21) | (1 << 29) | (1 << 30);

/// The bits of a segment's attributes there are: the low twelve.
const ATTRIB_BITS: u16 = 0x0FFF;
/// The bits of DR6 and DR7 there are: the low 32; above them a CPU refuses.
const DR_BITS: u64 = 0xFFFF_FFFF;

impl Event {
    fn put(&self, o: &mut Out) {
        o.u8(self.kind.code());
        o.u8(self.vector);
        o.opt(self.error, |o, e| o.u32(e));
    }

    fn get(r: &mut In) -> Result<Event> {
        let kind = EventKind::from_code(r.u8("an event's kind")?)?;
        let vector = r.u8("an event's vector")?;
        let error = r.opt("an event's error code", |r| r.u32("an event's error code"))?;
        match kind {
            EventKind::Nmi if vector != 2 => return Err(Error("an NMI not at vector 2")),
            EventKind::Exception if vector >= 32 => return Err(Error("an exception past vector 31")),
            EventKind::External if vector < 16 => return Err(Error("an external interrupt at a reserved vector")),
            _ => {}
        }
        let wants = kind == EventKind::Exception && ERROR_CODE_VECTORS & (1 << (vector % 32)) != 0;
        if error.is_some() != wants {
            return Err(Error("an event's error code where it has none, or none where it has one"));
        }
        Ok(Event { kind, vector, error })
    }
}

impl Segment {
    fn put(&self, o: &mut Out) {
        o.u16(self.selector);
        o.u64(self.base);
        o.u32(self.limit);
        o.u16(self.attrib);
    }

    fn get(r: &mut In) -> Result<Segment> {
        let s = Segment { selector: r.u16("a selector")?, base: r.u64("a segment's base")?,
                          limit: r.u32("a segment's limit")?, attrib: r.u16("a segment's attributes")? };
        if s.attrib & !ATTRIB_BITS != 0 {
            return Err(Error("a segment's attributes no descriptor has"));
        }
        Ok(s)
    }
}

impl Table {
    fn put(&self, o: &mut Out) {
        o.u64(self.base);
        o.u16(self.limit);
    }

    fn get(r: &mut In) -> Result<Table> {
        Ok(Table { base: r.u64("a table's base")?, limit: r.u16("a table's limit")? })
    }
}

impl Regs {
    /// Every register out of reset as zero, the XSAVE area empty: what a
    /// backend reads its own into.
    pub fn zeroed() -> Regs {
        Regs {
            gprs: [0; 16], rip: 0, rflags: 0, cr0: 0, cr2: 0, cr3: 0, cr4: 0, cr8: 0, efer: 0, pat: 0, cpl: 0,
            segs: [Segment::default(); 8], gdtr: Table::default(), idtr: Table::default(), star: 0, lstar: 0, cstar: 0,
            sfmask: 0, kernel_gs_base: 0, sysenter_cs: 0, sysenter_esp: 0, sysenter_eip: 0, dr: [0; 4], dr6: 0, dr7: 0,
            tsc_aux: 0, spec_ctrl: 0, mtrrs: crate::Mtrrs::default(), xcr0: 0, pkru: 0, xsave: Vec::new(), event: None,
            shadow: false, nmi_blocked: false, pending_debug: 0,
        }
    }

    fn put(&self, o: &mut Out) {
        for g in self.gprs {
            o.u64(g);
        }
        o.u64(self.rip);
        o.u64(self.rflags);
        o.u64(self.cr0);
        o.u64(self.cr2);
        o.u64(self.cr3);
        o.u64(self.cr4);
        o.u8(self.cr8);
        o.u64(self.efer);
        o.u64(self.pat);
        o.u8(self.cpl);
        for s in &self.segs {
            s.put(o);
        }
        self.gdtr.put(o);
        self.idtr.put(o);
        for v in [self.star, self.lstar, self.cstar, self.sfmask, self.kernel_gs_base, self.sysenter_cs,
                  self.sysenter_esp, self.sysenter_eip] {
            o.u64(v);
        }
        for d in self.dr {
            o.u64(d);
        }
        o.u64(self.dr6);
        o.u64(self.dr7);
        o.u64(self.tsc_aux);
        o.u64(self.spec_ctrl);
        o.u64(self.xcr0);
        o.u32(self.pkru);
        o.u16(self.xsave.len() as u16);
        o.fixed(&self.xsave);
        o.opt(self.event.as_ref(), |o, e| e.put(o));
        o.bool(self.shadow);
        o.bool(self.nmi_blocked);
        o.u64(self.pending_debug);
        self.mtrrs.put(o);
    }

    fn get(r: &mut In) -> Result<Regs> {
        let mut gprs = [0u64; 16];
        for g in &mut gprs {
            *g = r.u64("a register")?;
        }
        let rip = r.u64("RIP")?;
        let rflags = r.u64("RFLAGS")?;
        let cr0 = r.u64("CR0")?;
        let cr2 = r.u64("CR2")?;
        let cr3 = r.u64("CR3")?;
        let cr4 = r.u64("CR4")?;
        let cr8 = r.u8("CR8")?;
        if cr8 > 0xF {
            return Err(Error("a CR8 past its four bits"));
        }
        let efer = r.u64("EFER")?;
        let pat = r.u64("PAT")?;
        let cpl = r.u8("CPL")?;
        if cpl > 3 {
            return Err(Error("a privilege level past 3"));
        }
        let mut segs = [Segment::default(); 8];
        for s in &mut segs {
            *s = Segment::get(r)?;
        }
        let gdtr = Table::get(r)?;
        let idtr = Table::get(r)?;
        let mut msrs = [0u64; 8];
        for m in &mut msrs {
            *m = r.u64("a system MSR")?;
        }
        let [star, lstar, cstar, sfmask, kernel_gs_base, sysenter_cs, sysenter_esp, sysenter_eip] = msrs;
        let mut dr = [0u64; 4];
        for d in &mut dr {
            *d = r.u64("a breakpoint's address")?;
        }
        let dr6 = r.u64("DR6")?;
        let dr7 = r.u64("DR7")?;
        if dr6 & !DR_BITS != 0 || dr7 & !DR_BITS != 0 {
            return Err(Error("DR6 or DR7 past bit 31"));
        }
        let tsc_aux = r.u64("TSC_AUX")?;
        let spec_ctrl = r.u64("SPEC_CTRL")?;
        let xcr0 = r.u64("XCR0")?;
        let pkru = r.u32("PKRU")?;
        let n = usize::from(r.u16("the XSAVE area's length")?);
        if n > XSAVE_MAX {
            return Err(Error("an XSAVE area longer than a state carries"));
        }
        let mut xsave = Vec::new();
        xsave.try_reserve_exact(n).map_err(|_| Error("no memory for the XSAVE area"))?;
        xsave.extend_from_slice(r.bytes(n, "the XSAVE area")?);
        let event = r.opt("an event", Event::get)?;
        let shadow = r.bool("the interrupt shadow")?;
        let nmi_blocked = r.bool("NMIs blocked")?;
        let pending_debug = r.u64("debug exceptions pending")?;
        let mtrrs = if r.version >= 3 { crate::Mtrrs::get(r)? } else { crate::Mtrrs::default() };
        Ok(Regs {
            gprs, rip, rflags, cr0, cr2, cr3, cr4, cr8, efer, pat, cpl, segs, gdtr, idtr, star, lstar, cstar, sfmask,
            kernel_gs_base, sysenter_cs, sysenter_esp, sysenter_eip, dr, dr6, dr7, tsc_aux, spec_ctrl, mtrrs, xcr0,
            pkru, xsave, event, shadow, nmi_blocked, pending_debug,
        })
    }
}

impl Cpu {
    pub(crate) fn put(&self, o: &mut Out) {
        self.regs.put(o);
        o.bool(self.nmi_pending);
        o.opt(self.exception, |o, (v, e)| {
            o.u8(v);
            o.opt(e, |o, e| o.u32(e));
        });
        o.u8(self.activity.code());
        o.bool(self.last_from_pic);
        o.u64(self.pv_eoi);
        self.lapic.put(o);
        self.mail.put(o);
    }

    pub(crate) fn get(r: &mut In) -> Result<Cpu> {
        let regs = Regs::get(r)?;
        let nmi_pending = r.bool("an NMI pending")?;
        let exception = r.opt("an exception owed", |r| {
            let vector = r.u8("an exception's vector")?;
            let error = r.opt("an exception's error code", |r| r.u32("an exception's error code"))?;
            if vector >= 32 || error.is_some() != (ERROR_CODE_VECTORS & (1 << vector) != 0) {
                return Err(Error("an exception owed that no CPU raises so"));
            }
            Ok((vector, error))
        })?;
        let activity = Activity::from_code(r.u8("activity")?)?;
        let last_from_pic = r.bool("last from the 8259")?;
        let pv_eoi = r.u64("PV EOI")?;
        let lapic = Lapic::get(r)?;
        let mail = Mail::get(r)?;
        Ok(Cpu { regs, nmi_pending, exception, activity, last_from_pic, pv_eoi, lapic, mail })
    }
}
