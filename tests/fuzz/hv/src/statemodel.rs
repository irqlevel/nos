//! A VM's state as bytes (`vmstate`, plans/vm-migration.md's M0): what a
//! paused VM is moved and saved in, read on a host that did not write it.
//!
//! - **Hostile bytes**: whatever the input is, `decode` returns, and a state
//!   it takes is written back to bytes that read as the same state.
//! - **Round trips**: a state made of the input's fields, every one inside
//!   its bounds, is read back from its bytes the same -- and those bytes
//!   with one changed, cut short or added to, read or are refused, never a
//!   panic.
//! - **A CPU's registers**: what both backends write into their save area
//!   from a state (`state::restore_common`) read back out the same
//!   (`save_common`), every register a CPU could hold -- a field one leaves
//!   out is a guest that faults after its restore.
//! - **Bounds**: a state with one field one past its bound -- a CR8 of 16, a
//!   queue of no power of two, a flow steered to a pair the NIC has not, a
//!   local APIC in another CPU's place -- is refused, whichever field it is.

use vmstate::*;

use crate::targets::Input;

pub fn vmstate_target(r: &mut Input) {
    match r.op(5) {
        Some(3) => cpu_round_trip(r),
        Some(0) => {
            let bytes = r.rest();
            hostile(bytes);
        }
        Some(1) | Some(2) => {
            let s = state(r);
            let bytes = s.encode().expect("a state encodes");
            let back = VmState::decode(&bytes);
            invariant!(back.as_ref() == Ok(&s), "a state read back is not the one written: {:?}", back.err());
            as_version_1(&s, &bytes, r.bool());
            /* Its bytes changed, a byte at a time, cut short, grown. */
            while let Some(op) = r.op(3) {
                let mut b = bytes.clone();
                match op {
                    0 if !b.is_empty() => {
                        let at = r.below(b.len() as u64) as usize;
                        b[at] ^= r.u8() | 1;
                    }
                    1 => b.truncate(r.below(b.len() as u64 + 1) as usize),
                    _ => b.extend_from_slice(&[r.u8(); 3]),
                }
                hostile(&b);
            }
        }
        _ => {
            let mut s = state(r);
            let which = r.below(BOUNDS as u64) as usize;
            if !past_bound(&mut s, which, r) {
                return;
            }
            let bytes = s.encode().expect("a state encodes");
            let back = VmState::decode(&bytes);
            invariant!(back.is_err(), "bound {} broken and the state taken all the same", which);
        }
    }
}

/// Bytes anything may have written: read or refused, and a state read
/// writes back to bytes that read as it.
fn hostile(bytes: &[u8]) {
    if let Ok(s) = VmState::decode(bytes) {
        let again = s.encode().expect("a state read encodes");
        let back = VmState::decode(&again);
        invariant!(back.as_ref() == Ok(&s), "a state read, written and read again is another");
    }
}

/// `s`, written as version 1 wrote it -- its bytes, `bytes`, version 3's,
/// with the version 1 and the UART's last fields taken out, a "ESC [ 6" of
/// version 1's in place of a sequence of the one parameter 6 where `csi6`
/// -- read as `s` with what versions 2 and 3 added at their defaults: a state
/// of a host not yet updated, moved to one that is.
fn as_version_1(s: &VmState, bytes: &[u8], csi6: bool) {
    let u = &s.platform.uart;
    let mut want = s.clone();
    for c in &mut want.cpus {
        c.regs.mtrrs = Mtrrs::default();
    }
    {
        let w = &mut want.platform.uart;
        w.saved_col = None;
        if w.esc == ESC_CSI {
            w.csi = if csi6 { [6, 0] } else { [0; CSI_PARAMS] };
            w.csi_n = u8::from(csi6);
        }
    }
    /* The parts after the magic and the version, to the platform's. Version 1
     * has no MTRRs: take their 28 u64s out of each CPU's registers, leaving
     * the CPU state that follows them in place. */
    const MTRRS_LEN: usize = 28 * 8;
    const THROUGH_XSAVE_LEN: usize = 484;
    const AFTER_EVENT: usize = 10;
    let le32 = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    let mut cpu_parts = Vec::new();
    let mut cpu_index = 0;
    let mut at = 8 + 2;
    while bytes[at] != 3 {
        let len = le32(&bytes[at + 1..]);
        let body = at + 5;
        if bytes[at] == 2 {
            let regs = &s.cpus[cpu_index].regs;
            cpu_index += 1;
            let event_len = 1 + regs.event.as_ref().map_or(0, |e| 3 + 4 * usize::from(e.error.is_some()));
            let mtrr_at = THROUGH_XSAVE_LEN + regs.xsave.len() + event_len + AFTER_EVENT;
            cpu_parts.push(bytes[at]);
            cpu_parts.extend_from_slice(&((len - MTRRS_LEN) as u32).to_le_bytes());
            cpu_parts.extend_from_slice(&bytes[body..body + mtrr_at]);
            cpu_parts.extend_from_slice(&bytes[body + mtrr_at + MTRRS_LEN..body + len]);
        } else {
            cpu_parts.extend_from_slice(&bytes[at..body + len]);
        }
        at = body + len;
    }
    let len = le32(&bytes[at + 1..]);
    let uart_v1 = 6 + 8 + 1 + usize::from(u.loopback.is_some()) + 1 + usize::from(u.rx.is_some()) + 1 + 2 + 1
        + REPLY_MAX + 1 + 3;
    let tail = 2 * CSI_PARAMS + 1 + 1 + if u.saved_col.is_some() { 2 } else { 0 };
    let body = at + 5;
    let mut v1 = Vec::with_capacity(bytes.len());
    v1.extend_from_slice(&bytes[..8]);
    v1.extend_from_slice(&1u16.to_le_bytes());
    v1.extend_from_slice(&cpu_parts);
    v1.push(bytes[at]);
    v1.extend_from_slice(&((len - tail) as u32).to_le_bytes());
    v1.extend_from_slice(&bytes[body..body + uart_v1]);
    v1.extend_from_slice(&bytes[body + uart_v1 + tail..]);
    /* The UART's escape byte, version 1's 3 for a sequence of 6 */
    let platform_body = 8 + 2 + cpu_parts.len() + 5;
    let esc_at = platform_body + 6 + 8 + 1 + usize::from(u.loopback.is_some()) + 1 + usize::from(u.rx.is_some());
    invariant!(v1[esc_at] == u.esc, "the UART's escape not where version 1 has it");
    if u.esc == ESC_CSI && csi6 {
        v1[esc_at] = ESC_CSI6_V1;
    }
    let back = VmState::decode(&v1);
    invariant!(back.as_ref() == Ok(&want), "a state of version 1 not read as it was: {:?}", back.err());
}

/* ---- a state of the input's, every field inside its bounds ---- */

fn state(r: &mut Input) -> VmState {
    let cpus = 1 + r.below(4) as u32;
    let ioapic = cpus > 1 && r.bool();
    let mut ranges = Vec::new();
    let mut at = r.below(16) * 0x1000;
    for _ in 0..1 + r.below(3) {
        let size = (1 + r.below(64)) * 0x1000;
        ranges.push(Range { base: at, size });
        at += size + r.below(4) * 0x1000;
    }
    let vm = Vm {
        vendor: if r.bool() { Vendor::Svm } else { Vendor::Vmx },
        uuid: core::array::from_fn(|_| r.u8()),
        ranges,
        x2apic: r.bool(),
        ioapic,
        apicv: match r.below(3) {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        },
        offloads: r.bool(),
        rx_moderation_ns: r.below(1_000_000_001),
        shape: Shape { cpus, smt: r.bool(), llc: r.bool() },
        model: core::array::from_fn(|_| r.u8()),
        tsc_hz: 1 + r.below(u64::from(u32::MAX)),
        tsc: r.u64(),
        started: r.u64(),
    };
    let cpus = (0..cpus).map(|i| cpu(r, i)).collect();
    let devices: Vec<Device> = (0..r.below(4)).map(|_| device(r)).collect();
    let platform = Platform {
        uart: uart(r),
        pic: Pic { master: pic_chip(r), slave: pic_chip(r), elcr: r.u16() },
        pit: Pit { ch: core::array::from_fn(|_| pit_channel(r)), port61: r.u8(), ch0_edges_given: r.u64(),
                   ch0_armed: r.bool() },
        rtc: Rtc { index: r.u8(), ram: core::array::from_fn(|_| r.u8()) },
        pm: Pm { status: r.u16(), enable: r.u16(), control: r.u16(), elapsed_ns: r.u64(), carries_seen: r.u64(),
                 scis: r.u64(), presses: r.u32(), used: r.bool(), other_sleep: r.bool().then(|| r.u8()) },
        pvpanic: PvPanic { crash_loaded: r.u32() },
        ioapic: ioapic.then(|| IoApic { id: r.u32(), select: r.u32(), rte: core::array::from_fn(|_| r.u64()),
                                        lines: r.u32(), remote_irr: r.u32() }),
        pci_address: r.u32(),
        functions: (0..devices.len() + 1).map(|_| Function {
            command: r.u16(), bar0: r.u32(), bar1: r.u32(), interrupt_line: r.u8(), msix_control: r.u16(),
        }).collect(),
        msix_pending: devices.iter().map(|_| r.u32()).collect(),
        devices,
    };
    VmState { vm, cpus, platform }
}

fn segment(r: &mut Input) -> Segment {
    Segment { selector: r.u16(), base: r.u64(), limit: r.u32(), attrib: r.u16() & 0x0FFF }
}

fn event(r: &mut Input) -> Event {
    match r.below(4) {
        0 => Event { kind: EventKind::External, vector: 16 + r.below(240) as u8, error: None },
        1 => Event { kind: EventKind::Nmi, vector: 2, error: None },
        2 => {
            let vector = r.below(32) as u8;
            let error = (ERROR_CODE_VECTORS & (1 << vector) != 0).then(|| r.u32());
            Event { kind: EventKind::Exception, vector, error }
        }
        _ => Event { kind: EventKind::SoftInt, vector: r.u8(), error: None },
    }
}

fn fixed_mtrr(r: &mut Input) -> u64 {
    (0..8).map(|i| r.pick(&[0u64, 1, 4, 5, 6, 7]) << (8 * i)).sum()
}

fn valid_mtrrs(r: &mut Input) -> Mtrrs {
    let types: [u64; 6] = [0, 1, 4, 5, 6, 7];
    Mtrrs {
        def_type: r.pick(&[0, 0x806, 0x1000, 0x1806, 1, 4, 6, 0x800]),
        fixed: core::array::from_fn(|_| fixed_mtrr(r)),
        base: core::array::from_fn(|_| (r.u64() & ((1u64 << 52) - 1) & !0xFFF) | r.pick(&types)),
        mask: core::array::from_fn(|_| (r.u64() & ((1u64 << 52) - 1) & !0xFFF) | (u64::from(r.bool()) << 11)),
    }
}

fn cpu(r: &mut Input, index: u32) -> Cpu {
    let mut regs = Regs::zeroed();
    regs.gprs = core::array::from_fn(|_| r.u64());
    regs.rip = r.u64();
    regs.rflags = r.u64();
    regs.cr0 = r.u64();
    regs.cr2 = r.u64();
    regs.cr3 = r.u64();
    regs.cr4 = r.u64();
    regs.cr8 = r.below(16) as u8;
    regs.efer = r.u64();
    regs.pat = r.u64();
    regs.cpl = r.below(4) as u8;
    regs.segs = core::array::from_fn(|_| segment(r));
    regs.gdtr = Table { base: r.u64(), limit: r.u16() };
    regs.idtr = Table { base: r.u64(), limit: r.u16() };
    regs.star = r.u64();
    regs.lstar = r.u64();
    regs.kernel_gs_base = r.u64();
    regs.sysenter_eip = r.u64();
    regs.dr = core::array::from_fn(|_| r.u64());
    regs.dr6 = u64::from(r.u32());
    regs.dr7 = u64::from(r.u32());
    regs.tsc_aux = r.u64();
    regs.spec_ctrl = r.u64();
    regs.mtrrs = valid_mtrrs(r);
    regs.xcr0 = r.u64();
    regs.pkru = r.u32();
    regs.xsave = (0..r.below(64) * 13).map(|_| r.u8()).collect();
    regs.event = r.bool().then(|| event(r));
    regs.shadow = r.bool();
    regs.nmi_blocked = r.bool();
    regs.pending_debug = r.u64();
    let exception = r.bool().then(|| {
        let v = r.below(32) as u8;
        (v, (ERROR_CODE_VECTORS & (1 << v) != 0).then(|| r.u32()))
    });
    let lvt_len = r.below(LVT_MAX as u64 + 1) as u8;
    let lapic = Lapic {
        id: index,
        base: r.u64() & (0x000F_FFFF_FFFF_F000 | (1 << 8) | (1 << 10) | (1 << 11)),
        tpr: r.u8(),
        svr: r.u32(),
        isr: core::array::from_fn(|_| r.u32()),
        irr: core::array::from_fn(|_| r.u32()),
        tmr: core::array::from_fn(|_| r.u32()),
        esr: r.u32(),
        esr_pending: r.u32(),
        icr: r.u64(),
        lvt: core::array::from_fn(|_| r.u32()),
        lvt_len,
        timer_initial: r.u32(),
        timer_divide: r.u32(),
        timer_counted: r.u64(),
        timer_loaded_ago_ns: r.u64(),
        timer_due_in_ns: r.bool().then(|| r.u64()),
        tsc_deadline: r.u64(),
        ldr: r.u32(),
        dfr: r.u32(),
    };
    let mail = Mail {
        edge: core::array::from_fn(|_| r.u64()),
        level: core::array::from_fn(|_| r.u64()),
        nmi: r.bool(),
        init: r.bool(),
        sipi: r.bool().then(|| r.u8()),
    };
    Cpu {
        regs,
        nmi_pending: r.bool(),
        exception,
        activity: match r.below(4) {
            0 => Activity::Running,
            1 => Activity::Halted,
            2 => Activity::Asleep,
            _ => Activity::WaitForSipi,
        },
        last_from_pic: r.bool(),
        pv_eoi: r.u64(),
        lapic,
        mail,
    }
}

fn uart(r: &mut Input) -> Uart {
    let reply_len = r.below(REPLY_MAX as u64 + 1) as u8;
    /* Version 1's "ESC [ 6" (3) is no state version 2 writes */
    let esc = r.pick(&[ESC_TEXT, ESC_START, ESC_CSI, ESC_CSI_OTHER]);
    let csi_n = if esc == ESC_CSI { r.below(CSI_PARAMS as u64 + 1) as u8 } else { 0 };
    let csi = core::array::from_fn(|i| if i < usize::from(csi_n) { r.u16() } else { 0 });
    Uart {
        dll: r.u8(), dlm: r.u8(), ier: r.u8(), lcr: r.u8(), mcr: r.u8(), scr: r.u8(), written: r.u64(),
        loopback: r.bool().then(|| r.u8()), rx: r.bool().then(|| r.u8()), esc, col: r.u16(), csi, csi_n,
        saved_col: r.bool().then(|| r.u16()),
        reply: core::array::from_fn(|_| r.u8()), reply_len, reply_pos: r.below(u64::from(reply_len) + 1) as u8,
        prompt_seen: r.bool(), thri: r.bool(), pulse: r.bool(),
    }
}

fn pic_chip(r: &mut Input) -> PicChip {
    PicChip { irr: r.u8(), isr: r.u8(), imr: r.u8(), vector_base: r.u8(), init_step: r.below(5) as u8,
              expect_icw4: r.bool(), read_isr: r.bool() }
}

fn pit_channel(r: &mut Input) -> PitChannel {
    PitChannel {
        reload: r.u16(), access: 1 + r.below(3) as u8, mode: r.below(8) as u8, write_high: r.bool(),
        read_high: r.bool(), low: r.u8(), latched: r.bool().then(|| r.u16()), loaded: r.bool(), counted: r.u64(),
        counting_for_ns: r.bool().then(|| r.u64()),
    }
}

fn transport(r: &mut Input, queues: usize) -> Transport {
    let device_features = r.u32();
    let msix = r.bool().then(|| {
        let entries = 1 + r.below(2048) as u16;
        let vector = |r: &mut Input| if r.bool() { u16::MAX } else { r.below(u64::from(entries)) as u16 };
        Msix { entries, enabled: r.bool(), config: vector(r), queues: (0..queues).map(|_| vector(r)).collect() }
    });
    Transport {
        device_features,
        driver_features: r.u32() & device_features,
        status: r.u8(),
        isr: r.u8(),
        select: r.u16(),
        queues: (0..queues).map(|_| Queue {
            size: 1 << r.below(16), pfn: r.u32(), next_avail: r.u16(), next_used: r.u16(), seen: r.u16(),
            signalled_used: r.bool().then(|| r.u16()),
        }).collect(),
        msix,
    }
}

fn device(r: &mut Input) -> Device {
    if r.bool() {
        Device::Disk(Blk { transport: transport(r, 1), broken: r.bool().then(|| r.u8()) })
    } else {
        let pairs = 1 + r.below(8) as u16;
        let queues = 2 * usize::from(pairs) + usize::from(pairs > 1);
        Device::Nic(Net {
            transport: transport(r, queues),
            mac: core::array::from_fn(|_| r.u8()),
            pairs,
            active: 1 + r.below(u64::from(pairs)) as u16,
            offloads: r.bool(),
            mtu: r.u16(),
            flows: core::array::from_fn(|_| r.below(u64::from(pairs) + 1) as u8),
            told: Takes { partial: r.bool(), whole: r.bool() },
            broken: r.bool().then(|| r.u8()),
        })
    }
}

/* ---- one field past its bound ---- */

const BOUNDS: usize = 30;

/// Field `which` of `s` put one past its bound: false where `s` has none of
/// that field (no device, no event) to put there.
fn past_bound(s: &mut VmState, which: usize, r: &mut Input) -> bool {
    let cpu = r.below(s.cpus.len() as u64) as usize;
    let c = &mut s.cpus[cpu];
    match which {
        0 => c.lapic.id += 1 + r.below(4) as u32,
        1 => c.regs.cr8 = 16 + r.below(240) as u8,
        2 => c.regs.cpl = 4 + r.below(252) as u8,
        3 => c.regs.segs[r.below(8) as usize].attrib |= 1 << (12 + r.below(4)),
        4 => c.regs.dr6 |= 1 << (32 + r.below(32)),
        5 => c.regs.xsave = vec![0; XSAVE_MAX + 1 + r.below(64) as usize],
        6 => c.regs.event = Some(Event { kind: EventKind::Nmi, vector: 3 + r.below(253) as u8, error: None }),
        7 => c.regs.event = Some(Event { kind: EventKind::Exception, vector: 32 + r.below(224) as u8, error: None }),
        8 => c.regs.event = Some(Event { kind: EventKind::Exception, vector: 13, error: None }),
        9 => c.exception = Some((32 + r.below(224) as u8, None)),
        10 => c.lapic.base |= 1 << r.pick(&[0u32, 1, 9, 52, 63]),
        11 => c.lapic.lvt_len = LVT_MAX as u8 + 1 + r.below(8) as u8,
        12 => s.platform.uart.esc = 5 + r.below(251) as u8,
        13 => s.platform.uart.reply_len = REPLY_MAX as u8 + 1,
        14 => {
            let u = &mut s.platform.uart;
            u.reply_len = r.below(REPLY_MAX as u64) as u8;
            u.reply_pos = u.reply_len + 1;
        }
        15 => s.platform.pic.slave.init_step = 5 + r.below(251) as u8,
        16 => s.platform.pit.ch[r.below(3) as usize].access = 0,
        17 => s.platform.pit.ch[r.below(3) as usize].mode = 8 + r.below(248) as u8,
        18 => {
            if s.platform.ioapic.is_some() {
                s.vm.ioapic = false;
            } else if s.vm.shape.cpus > 1 {
                s.vm.ioapic = true;
            } else {
                return false;
            }
        }
        19 => s.platform.functions.push(Function::default()),
        20 => {
            let Some(q) = queue(s) else { return false };
            q.size = 3 + 2 * r.below(1000) as u16;
        }
        21 => {
            let Some(t) = transport_of(s) else { return false };
            if t.device_features == u32::MAX {
                return false;
            }
            t.driver_features |= !t.device_features & (1 << (!t.device_features).trailing_zeros());
        }
        22 => {
            let Some(t) = transport_of(s) else { return false };
            let Some(m) = t.msix.as_mut() else { return false };
            m.config = m.entries + r.below(u64::from(u16::MAX - 1 - m.entries)) as u16;
        }
        23 => {
            let Some(n) = nic(s) else { return false };
            n.active = n.pairs + 1;
        }
        24 => {
            let Some(n) = nic(s) else { return false };
            n.flows[r.below(FLOWS as u64) as usize] = (n.pairs + 1) as u8;
        }
        25 => match r.below(3) {
            0 => s.vm.ranges[0].base |= 1 + r.below(0xFFF),
            1 => s.vm.ranges[0].size = 0,
            _ => {
                let first = s.vm.ranges[0];
                s.vm.ranges.push(Range { base: first.base, size: first.size });
            }
        },
        27 => {
            /* Version 2 has no "ESC [ 6" of version 1's */
            let u = &mut s.platform.uart;
            (u.esc, u.csi, u.csi_n) = (ESC_CSI6_V1, [0; CSI_PARAMS], 0);
        }
        28 => {
            let u = &mut s.platform.uart;
            u.esc = ESC_CSI;
            u.csi_n = CSI_PARAMS as u8 + 1 + r.below(8) as u8;
        }
        29 => {
            /* Parameters where no sequence is followed, or past those begun */
            let u = &mut s.platform.uart;
            if r.bool() {
                u.esc = r.pick(&[ESC_TEXT, ESC_START, ESC_CSI_OTHER]);
                u.csi_n = 1;
            } else {
                u.esc = ESC_CSI;
                u.csi_n = 0;
                u.csi[r.below(CSI_PARAMS as u64) as usize] = 1 + r.below(u64::from(u16::MAX)) as u16;
            }
        }
        _ => match r.below(3) {
            0 => s.vm.rx_moderation_ns = 1_000_000_001 + r.below(1 << 40),
            1 => s.vm.tsc_hz = 0,
            _ => {
                if s.vm.shape.cpus != 1 {
                    return false;
                }
                s.vm.ioapic = true;
                s.platform.ioapic = Some(IoApic { id: 0, select: 0, rte: [0; IOAPIC_PINS], lines: 0, remote_irr: 0 });
            }
        },
    }
    true
}

fn transport_of(s: &mut VmState) -> Option<&mut Transport> {
    s.platform.devices.first_mut().map(|d| match d {
        Device::Disk(b) => &mut b.transport,
        Device::Nic(n) => &mut n.transport,
    })
}

fn queue(s: &mut VmState) -> Option<&mut Queue> {
    transport_of(s)?.queues.first_mut()
}

fn nic(s: &mut VmState) -> Option<&mut Net> {
    s.platform.devices.iter_mut().find_map(|d| match d {
        Device::Nic(n) => Some(n),
        _ => None,
    })
}

/* ---- a CPU's registers through the save area and back ---- */

/// Registers every rule `state::check` holds a CPU to keeps: its fixed
/// RFLAGS bits, a CR0 a CPU takes, long mode only enabled with paging and
/// PAE, CR4 of the guest's allowed bits, a PAT of memory types, SS's DPL its
/// privilege level.
fn valid_regs(r: &mut Input) -> Regs {
    const RFLAGS_KEEP: u64 = (1 << 22) - 1 & !((1 << 3) | (1 << 5) | (1 << 15));
    const CR0_PE: u64 = 1;
    const CR0_NW: u64 = 1 << 29;
    const CR0_CD: u64 = 1 << 30;
    const CR0_PG: u64 = 1 << 31;
    const CR4_PAE: u64 = 1 << 5;
    const EFER_SCE: u64 = 1;
    const EFER_LME: u64 = 1 << 8;
    const EFER_LMA: u64 = 1 << 10;
    const EFER_NXE: u64 = 1 << 11;
    let mut regs = cpu(r, 0).regs;
    regs.rflags = (r.u64() & RFLAGS_KEEP) | 2;
    let mut cr0 = u64::from(r.u32());
    if cr0 & CR0_NW != 0 {
        cr0 |= CR0_CD;
    }
    if cr0 & CR0_PG != 0 {
        cr0 |= CR0_PE;
    }
    regs.cr0 = cr0;
    regs.cr3 = r.u64() & ((1 << 52) - 1);
    regs.cr4 = r.u64() & crate::policy::cr4_allowed();
    let long = cr0 & CR0_PG != 0 && r.bool();
    regs.efer = (r.u64() & (EFER_SCE | EFER_NXE)) | if long { EFER_LME | EFER_LMA } else { 0 };
    if long {
        regs.cr4 |= CR4_PAE;
    }
    if long && regs.segs[1].attrib & (1 << 9) != 0 {
        regs.segs[1].attrib &= !(1 << 10);
    }
    regs.pat = (0..8).map(|i| (r.pick(&[0u64, 1, 4, 5, 6, 7])) << (8 * i)).sum();
    if regs.segs[2].attrib & (1 << 7) != 0 && cr0 & CR0_PE != 0 {
        regs.cpl = ((regs.segs[2].attrib >> 5) & 3) as u8;
    }
    regs
}

/// Registers written into a save area by what both backends write them with
/// (`state::restore_common`), and read back by what they read them with
/// (`save_common`): the same, but what is each backend's own.
fn cpu_round_trip(r: &mut Input) {
    let regs = valid_regs(r);
    let mut save = crate::pod::zeroed::<crate::x86::svm::vmcb::Vmcb>().save;
    let mut g = crate::x86::svm::GuestRegs::default();
    if let Err(e) = crate::state::restore_common(&regs, &mut save, &mut g) {
        invariant!(false, "registers a CPU holds refused: {}", e);
    }
    let mut back = Regs::zeroed();
    crate::state::save_common(&save, &g, &mut back);
    let mut want = regs.clone();
    /* Each backend's own, which neither of the two touches. */
    (want.cr4, want.cr8, want.dr, want.tsc_aux, want.spec_ctrl, want.mtrrs, want.xcr0, want.pkru) =
        (0, 0, [0; 4], 0, 0, Mtrrs::default(), 0, 0);
    want.xsave.clear();
    (want.event, want.shadow, want.nmi_blocked, want.pending_debug) = (None, false, false, 0);
    invariant!(back == want, "registers written into a save area and read back are others:\n{:?}\n{:?}", want, back);
}
