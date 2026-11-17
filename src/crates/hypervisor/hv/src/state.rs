//! A guest CPU's registers in a VM's state (`vmstate::Regs`), and back: the
//! part both backends keep alike -- VT-x keeps its guest state in the shape
//! of AMD-V's save area, synced to the VMCS around an entry -- so the
//! general registers, the control registers, the segments, the system
//! MSRs are read from one shape and written into it once here; each
//! backend adds what is its own (`svm::Vcpu::save_regs`,
//! `vmx::Vcpu::save_regs`).
//!
//! A restore is refused unless every register holds what a CPU could hold:
//! a state is another host's word, and a CPU handed a register no CPU has
//! is a guest that does not enter, at best -- found here, before the
//! handover, rather than after it.

use hvarch::x86::svm::vmcb::{attrib, Save, Segment};
use hvarch::x86::svm::GuestRegs;

use crate::svm::{CR0_CD, CR0_NW, CR0_PE, CR0_PG, CR4_PAE, EFER_LMA, EFER_LME, EFER_SVME};

/// EFER's bits a guest may have: SCE, LME, LMA, NXE, and on AMD's side
/// LMSLE, FFXSR and TCE -- not SVME, the host's (`svm::EFER_DEFINED`).
const EFER_GUEST: u64 = 0xFD01 & !EFER_SVME;
/// RFLAGS: bit 1 is always set; 3, 5, 15 and 22-63 never are.
const RFLAGS_ONE: u64 = 1 << 1;
const RFLAGS_RESERVED: u64 = (1 << 3) | (1 << 5) | (1 << 15) | !((1u64 << 22) - 1);
/// The memory types a PAT entry may name: UC, WC, WT, WP, WB, UC-.
fn pat_valid(pat: u64) -> bool {
    (0..8).all(|i| matches!((pat >> (i * 8)) & 0xFF, 0 | 1 | 4 | 5 | 6 | 7))
}

/// The save area's segments, in the state's order: ES, CS, SS, DS, FS, GS,
/// LDTR, TR.
fn segments(s: &Save) -> [&Segment; 8] {
    [&s.es, &s.cs, &s.ss, &s.ds, &s.fs, &s.gs, &s.ldtr, &s.tr]
}

fn segments_mut(s: &mut Save) -> [&mut Segment; 8] {
    [&mut s.es, &mut s.cs, &mut s.ss, &mut s.ds, &mut s.fs, &mut s.gs, &mut s.ldtr, &mut s.tr]
}

/// The part of `r` the save area and the stub's registers hold: the general
/// registers, RIP and RFLAGS, CR0, CR2, CR3 -- CR4 and CR8 are each
/// backend's -- EFER less SVME, the PAT, the CPL, the segments, GDTR and
/// IDTR, the system MSRs, DR6 and DR7.
pub(crate) fn save_common(s: &Save, g: &GuestRegs, r: &mut vmstate::Regs) {
    r.gprs = [s.rax, g.rcx, g.rdx, g.rbx, s.rsp, g.rbp, g.rsi, g.rdi, g.r8, g.r9, g.r10, g.r11, g.r12, g.r13,
              g.r14, g.r15];
    r.rip = s.rip;
    r.rflags = s.rflags;
    r.cr0 = s.cr0;
    r.cr2 = s.cr2;
    r.cr3 = s.cr3;
    r.efer = s.efer & !EFER_SVME;
    r.pat = s.g_pat;
    r.cpl = s.cpl;
    for (out, seg) in r.segs.iter_mut().zip(segments(s)) {
        *out = vmstate::Segment { selector: seg.selector, base: seg.base, limit: seg.limit, attrib: seg.attrib };
    }
    r.gdtr = vmstate::Table { base: s.gdtr.base, limit: s.gdtr.limit as u16 };
    r.idtr = vmstate::Table { base: s.idtr.base, limit: s.idtr.limit as u16 };
    r.star = s.star;
    r.lstar = s.lstar;
    r.cstar = s.cstar;
    r.sfmask = s.sfmask;
    r.kernel_gs_base = s.kernel_gs_base;
    r.sysenter_cs = s.sysenter_cs;
    r.sysenter_esp = s.sysenter_esp;
    r.sysenter_eip = s.sysenter_eip;
    r.dr6 = s.dr6;
    r.dr7 = s.dr7;
}

/// Whether `r` holds what a CPU could: RFLAGS with its fixed bits as they
/// are, CR0 with no bit above 31 and NW only with CD, paging only in
/// protected mode, CR3 no bit above 51, CR4 only what the guest's CPUID lets
/// it set (`policy::cr4_allowed`), EFER only a guest's bits, long mode
/// active exactly where it is enabled with paging on -- and with PAE --
/// 64-bit code with no D, a PAT of memory types, a privilege level that is
/// SS's.
pub(crate) fn check(r: &vmstate::Regs) -> vmstate::Result<()> {
    let bad = |what| Err(vmstate::Error(what));
    if r.rflags & RFLAGS_ONE == 0 || r.rflags & RFLAGS_RESERVED != 0 {
        return bad("RFLAGS with a reserved bit as no CPU has it");
    }
    if r.cr0 >> 32 != 0 || (r.cr0 & CR0_NW != 0 && r.cr0 & CR0_CD == 0)
        || (r.cr0 & CR0_PG != 0 && r.cr0 & CR0_PE == 0)
    {
        return bad("a CR0 no CPU holds");
    }
    if r.cr3 >> 52 != 0 {
        return bad("a CR3 past 52 bits");
    }
    if r.cr4 & !crate::policy::cr4_allowed() != 0 {
        return bad("a CR4 with a bit the guest's CPUID does not let it set");
    }
    if r.efer & !EFER_GUEST != 0 {
        return bad("an EFER with a bit a guest has not");
    }
    let (lme, pg, lma) = (r.efer & EFER_LME != 0, r.cr0 & CR0_PG != 0, r.efer & EFER_LMA != 0);
    if lma != (lme && pg) || (lma && r.cr4 & CR4_PAE == 0) {
        return bad("long mode active other than where it is enabled with paging and PAE on");
    }
    let cs = r.segs[1].attrib;
    if lma && cs & attrib::L != 0 && cs & attrib::DB != 0 {
        return bad("64-bit code with D set");
    }
    if !pat_valid(r.pat) {
        return bad("a PAT entry that is no memory type");
    }
    let ss_dpl = ((r.segs[2].attrib >> attrib::DPL_SHIFT) & 0x3) as u8;
    if r.segs[2].attrib & attrib::P != 0 && r.cr0 & CR0_PE != 0 && r.cpl != ss_dpl {
        return bad("a privilege level that is not SS's");
    }
    if r.dr6 >> 32 != 0 || r.dr7 >> 32 != 0 {
        return bad("DR6 or DR7 past bit 31");
    }
    Ok(())
}

/// The part of `r` [`save_common`] reads, written into the save area and the
/// stub's registers -- checked first ([`check`]). Every field of `r` named
/// here, the backends' own as `_`: a field added to the state and not
/// written back is a build that fails, not a guest that faults after its
/// restore -- as EFER, left out once, made one in long mode run on in
/// legacy mode until its first interrupt.
#[deny(unused_variables)]
pub(crate) fn restore_common(r: &vmstate::Regs, s: &mut Save, g: &mut GuestRegs) -> vmstate::Result<()> {
    check(r)?;
    let vmstate::Regs {
        gprs, rip, rflags, cr0, cr2, cr3, cr4: _, cr8: _, efer, pat, cpl, segs, gdtr, idtr, star, lstar, cstar, sfmask,
        kernel_gs_base, sysenter_cs, sysenter_esp, sysenter_eip, dr: _, dr6, dr7, tsc_aux: _, spec_ctrl: _, xcr0: _,
        pkru: _, xsave: _, event: _, shadow: _, nmi_blocked: _, pending_debug: _,
    } = r;
    let [rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15] = *gprs;
    s.rax = rax;
    s.rsp = rsp;
    *g = GuestRegs { rbx, rcx, rdx, rsi, rdi, rbp, r8, r9, r10, r11, r12, r13, r14, r15 };
    s.rip = *rip;
    s.rflags = *rflags;
    s.cr0 = *cr0;
    s.cr2 = *cr2;
    s.cr3 = *cr3;
    s.efer = *efer;
    s.g_pat = *pat;
    s.cpl = *cpl;
    for (seg, from) in segments_mut(s).into_iter().zip(segs) {
        *seg = Segment { selector: from.selector, attrib: from.attrib, limit: from.limit, base: from.base };
    }
    s.gdtr = Segment { base: gdtr.base, limit: u32::from(gdtr.limit), ..Segment::default() };
    s.idtr = Segment { base: idtr.base, limit: u32::from(idtr.limit), ..Segment::default() };
    s.star = *star;
    s.lstar = *lstar;
    s.cstar = *cstar;
    s.sfmask = *sfmask;
    s.kernel_gs_base = *kernel_gs_base;
    s.sysenter_cs = *sysenter_cs;
    s.sysenter_esp = *sysenter_esp;
    s.sysenter_eip = *sysenter_eip;
    s.dr6 = *dr6;
    s.dr7 = *dr7;
    Ok(())
}

/// An event in VMCB's `event_inj` format, or VMENTRY_INTR_INFO's -- the two
/// share their low twelve bits: the vector, the type, and whether an error
/// code goes with it.
pub(crate) fn event_bits(e: &vmstate::Event) -> (u32, Option<u32>) {
    const VALID: u32 = 1 << 31;
    const ERROR: u32 = 1 << 11;
    let kind = match e.kind {
        vmstate::EventKind::External => 0,
        vmstate::EventKind::Nmi => 2,
        vmstate::EventKind::Exception => 3,
        vmstate::EventKind::SoftInt => 4,
    };
    let bits = VALID | (kind << 8) | u32::from(e.vector) | if e.error.is_some() { ERROR } else { 0 };
    (bits, e.error)
}

/// The event `bits` -- `event_inj`'s or VMENTRY_INTR_INFO's low 32 -- and
/// `error`, in the state's form; None for one not valid, or of a type the
/// state has none of -- a software exception, which neither backend ever
/// queues.
pub(crate) fn event_of(bits: u32, error: u32) -> Option<vmstate::Event> {
    const VALID: u32 = 1 << 31;
    const ERROR: u32 = 1 << 11;
    if bits & VALID == 0 {
        return None;
    }
    let kind = match (bits >> 8) & 0x7 {
        0 => vmstate::EventKind::External,
        2 => vmstate::EventKind::Nmi,
        3 => vmstate::EventKind::Exception,
        4 => vmstate::EventKind::SoftInt,
        _ => return None,
    };
    Some(vmstate::Event { kind, vector: bits as u8, error: (bits & ERROR != 0).then_some(error) })
}
