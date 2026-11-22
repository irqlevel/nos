//! Microsoft's Hv#1 interface for a firmware guest. KVM remains the default
//! elsewhere. Its hypercall and reference TSC pages are read-only overlays;
//! its basic, time and SynIC MSRs, virtual APIC and flush/IPI hypercalls follow
//! the TLFS.

use kcore::sync::Mutex;

use crate::lapic::Lapic;
use crate::memory::{GuestMemory, OverlaySlot};
use crate::{Error, Result, Vendor};

pub const MSR_GUEST_OS_ID: u32 = 0x4000_0000;
pub const MSR_HYPERCALL: u32 = 0x4000_0001;
pub const MSR_VP_INDEX: u32 = 0x4000_0002;
pub const MSR_TIME_REF_COUNT: u32 = 0x4000_0020;
pub const MSR_REFERENCE_TSC: u32 = 0x4000_0021;
pub const MSR_TSC_FREQUENCY: u32 = 0x4000_0022;
pub const MSR_APIC_FREQUENCY: u32 = 0x4000_0023;
pub const MSR_SCONTROL: u32 = 0x4000_0080;
pub const MSR_SVERSION: u32 = 0x4000_0081;
pub const MSR_SIEFP: u32 = 0x4000_0082;
pub const MSR_SIMP: u32 = 0x4000_0083;
pub const MSR_EOM: u32 = 0x4000_0084;
pub const MSR_SIRBP: u32 = 0x4000_0085;
pub const MSR_SINT0: u32 = 0x4000_0090;
pub const MSR_SINT15: u32 = 0x4000_009F;
pub const MSR_STIMER0_CONFIG: u32 = 0x4000_00B0;
pub const MSR_STIMER3_COUNT: u32 = 0x4000_00B7;
pub const MSR_CRASH_P0: u32 = 0x4000_0100;
pub const MSR_CRASH_CTL: u32 = 0x4000_0105;
pub const MSR_APIC_EOI: u32 = 0x4000_0070;
pub const MSR_APIC_ICR: u32 = 0x4000_0071;
pub const MSR_APIC_TPR: u32 = 0x4000_0072;
pub const MSR_VP_ASSIST: u32 = 0x4000_0073;
const CRASH_NOTIFY: u64 = 1 << 63;

const HYPERCALL_ENABLE: u64 = 1;
const HYPERCALL_LOCKED: u64 = 1 << 1;
const HYPERCALL_RESERVED: u64 = 0xFFC;
const HYPERCALL_GPFN: u64 = !0xFFF;
const REFERENCE_TSC_ENABLE: u64 = 1;
const REFERENCE_TSC_RESERVED: u64 = 0xFFC;
const REFERENCE_TSC_GPFN: u64 = !0xFFF;
const REFERENCE_HZ: u128 = 10_000_000;
const APIC_HZ: u64 = 1_000_000_000;
const HV_STATUS_INVALID_HYPERCALL_CODE: u64 = 2;
pub const HV_STATUS_INVALID_HYPERCALL_INPUT: u64 = 3;
pub const HV_STATUS_INVALID_ALIGNMENT: u64 = 4;
pub const HV_STATUS_TIME_OUT: u64 = 0x78;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    FlushSpace { fast: bool },
    FlushList { fast: bool, reps: u16, start: u16 },
    SendIpi { fast: bool },
}

impl Call {
    pub fn decode(input: u64) -> core::result::Result<Call, u64> {
        const RESERVED: u64 = 0xF000_F000_0000_0000 | 0xF800_0000;
        if input & RESERVED != 0 {
            return Err(HV_STATUS_INVALID_HYPERCALL_INPUT);
        }
        let code = input as u16;
        let fast = input & 1 << 16 != 0;
        let variable = (input >> 17) & 0x3FF;
        let reps = ((input >> 32) & 0xFFF) as u16;
        let start = ((input >> 48) & 0xFFF) as u16;
        match code {
            0x0002 if variable == 0 && reps == 0 && start == 0 => Ok(Call::FlushSpace { fast }),
            0x0003 if variable == 0 && reps != 0 && start < reps => Ok(Call::FlushList { fast, reps, start }),
            0x000B if variable == 0 && reps == 0 && start == 0 => Ok(Call::SendIpi { fast }),
            0x0002 | 0x0003 | 0x000B => Err(HV_STATUS_INVALID_HYPERCALL_INPUT),
            _ => Err(HV_STATUS_INVALID_HYPERCALL_CODE),
        }
    }
}

const STIMER_ENABLE: u64 = 1 << 0;
const STIMER_PERIODIC: u64 = 1 << 1;
const STIMER_AUTO_ENABLE: u64 = 1 << 3;
const STIMER_VECTOR: u64 = 0xFF << 4;
const STIMER_DIRECT: u64 = 1 << 12;
const STIMER_SINT: u64 = 0xF << 16;

const PAGE_ENABLED: u64 = 1;
const PAGE_PRESERVED: u64 = 0xFFC;
const SINT_VECTOR: u64 = 0xFF;
const SINT_MASKED: u64 = 1 << 16;
const SINT_AUTO_EOI: u64 = 1 << 17;
const SINT_WRITABLE: u64 = SINT_VECTOR | (0x1F << 16);

#[derive(Clone, Copy, Default)]
struct Stimer {
    config: u64,
    count: u64,
    expires: Option<u64>,
}

/// One virtual processor's basic SynIC registers and four synthetic timers,
/// in direct mode or by a timer-expired message through a SINT.
pub struct Vp {
    scontrol: u64,
    pages: [u64; 3],
    sints: [u64; 16],
    timers: [Stimer; 4],
    vp_assist: u64,
}

impl Default for Vp {
    fn default() -> Self {
        Self { scontrol: 0, pages: [0; 3], sints: [SINT_MASKED; 16], timers: [Stimer::default(); 4], vp_assist: 0 }
    }
}

impl Vp {
    fn valid(config: u64) -> bool {
        config & STIMER_ENABLE == 0 || config & STIMER_DIRECT == 0 || (config & STIMER_VECTOR) >> 4 >= 16
    }

    fn normalize(config: &mut u64) {
        if *config & STIMER_ENABLE != 0 && *config & STIMER_DIRECT == 0 && *config & STIMER_SINT == 0 {
            *config &= !STIMER_ENABLE;
        }
    }

    pub fn read(&self, msr: u32) -> Option<u64> {
        match msr {
            MSR_SCONTROL => return Some(self.scontrol),
            MSR_SVERSION => return Some(1),
            MSR_SIEFP => return Some(self.pages[0]),
            MSR_SIMP => return Some(self.pages[1]),
            MSR_EOM => return Some(0),
            MSR_SIRBP => return Some(self.pages[2]),
            MSR_VP_ASSIST => return Some(self.vp_assist),
            MSR_SINT0..=MSR_SINT15 => return Some(self.sints[(msr - MSR_SINT0) as usize]),
            _ => {}
        }
        if !(MSR_STIMER0_CONFIG..=MSR_STIMER3_COUNT).contains(&msr) {
            return None;
        }
        let timer = &self.timers[((msr - MSR_STIMER0_CONFIG) / 2) as usize];
        Some(if msr & 1 == 0 { timer.config } else { timer.count })
    }

    fn write_page(&mut self, slot: usize, value: u64, memory: &GuestMemory) -> bool {
        let next = (value & !PAGE_PRESERVED) | (self.pages[slot] & PAGE_PRESERVED);
        if next & PAGE_ENABLED != 0 {
            let gpa = next & !0xFFF;
            if !memory.is_ram(gpa, kcore::consts::PAGE_SIZE as _) || memory.overlay_contains(gpa)
                || self.vp_assist & PAGE_ENABLED != 0 && self.vp_assist & !0xFFF == gpa
            {
                return false;
            }
            for off in (0..kcore::consts::PAGE_SIZE as u64).step_by(512) {
                if memory.write(gpa + off, &[0u8; 512]).is_err() {
                    return false;
                }
            }
        }
        self.pages[slot] = next;
        true
    }

    fn write_vp_assist(&mut self, value: u64, memory: &GuestMemory) -> bool {
        let next = (value & !PAGE_PRESERVED) | (self.vp_assist & PAGE_PRESERVED);
        if next & PAGE_ENABLED != 0 {
            let gpa = next & !0xFFF;
            let synic = self.pages.iter().any(|page| page & PAGE_ENABLED != 0 && page & !0xFFF == gpa);
            if synic || !memory.is_ram(gpa, kcore::consts::PAGE_SIZE as u64) || memory.overlay_contains(gpa)
                || memory.write(gpa, &0u32.to_le_bytes()).is_err()
            {
                return false;
            }
        }
        self.vp_assist = next;
        true
    }

    /// The enabled VP assist page's EOI-assist word.
    pub fn assist_page(&self) -> Option<u64> {
        (self.vp_assist & PAGE_ENABLED != 0).then_some(self.vp_assist & !0xFFF)
    }

    pub fn auto_eoi(&self) -> bool {
        self.scontrol & PAGE_ENABLED != 0
            && self.sints.iter().any(|sint| sint & (SINT_MASKED | SINT_AUTO_EOI) == SINT_AUTO_EOI)
    }

    pub fn auto_eoi_vector(&self, vector: u8) -> bool {
        self.scontrol & PAGE_ENABLED != 0
            && self.sints.iter().any(|sint| {
                sint & (SINT_VECTOR | SINT_MASKED | SINT_AUTO_EOI) == u64::from(vector) | SINT_AUTO_EOI
            })
    }

    /// Write a SynIC register or a timer at reference time `now`:
    /// Some(false) is a #GP, None an MSR this interface does not own.
    pub fn write(&mut self, msr: u32, value: u64, now: u64, memory: &GuestMemory) -> Option<bool> {
        match msr {
            MSR_VP_ASSIST => return Some(self.write_vp_assist(value, memory)),
            MSR_SCONTROL => {
                if value & !1 != 0 {
                    return Some(false);
                }
                self.scontrol = value;
                return Some(true);
            }
            MSR_SVERSION => return Some(false),
            MSR_SIEFP => return Some(self.write_page(0, value, memory)),
            MSR_SIMP => return Some(self.write_page(1, value, memory)),
            MSR_SIRBP => return Some(self.write_page(2, value, memory)),
            MSR_EOM => return Some(true),
            MSR_SINT0..=MSR_SINT15 => {
                if value & !SINT_WRITABLE != 0 || value & SINT_MASKED == 0 && value & SINT_VECTOR < 16 {
                    return Some(false);
                }
                self.sints[(msr - MSR_SINT0) as usize] = value;
                return Some(true);
            }
            _ => {}
        }
        if !(MSR_STIMER0_CONFIG..=MSR_STIMER3_COUNT).contains(&msr) {
            return None;
        }
        let timer = &mut self.timers[((msr - MSR_STIMER0_CONFIG) / 2) as usize];
        if msr & 1 == 0 {
            if !Self::valid(value) {
                return Some(false);
            }
            let mut config = value;
            Self::normalize(&mut config);
            timer.config = config;
            timer.expires = None;
            return Some(true);
        }
        let mut config = timer.config;
        if value == 0 {
            config &= !STIMER_ENABLE;
        } else if config & STIMER_AUTO_ENABLE != 0 {
            config |= STIMER_ENABLE;
        }
        Self::normalize(&mut config);
        if !Self::valid(config) {
            return Some(false);
        }
        timer.count = value;
        timer.config = config;
        timer.expires = (value != 0 && config & STIMER_ENABLE != 0).then_some(if config & STIMER_PERIODIC != 0 {
            now.saturating_add(value)
        } else {
            value
        });
        Some(true)
    }

    pub fn save(&self, now: u64) -> vmstate::HypervVp {
        vmstate::HypervVp {
            scontrol: self.scontrol,
            pages: self.pages,
            sints: self.sints,
            vp_assist: self.vp_assist,
            timers: core::array::from_fn(|i| {
                let timer = self.timers[i];
                vmstate::HypervTimer {
                    config: timer.config,
                    count: timer.count,
                    due_in: timer.expires.map(|expires| expires.saturating_sub(now)),
                }
            }),
        }
    }

    pub fn restore(&mut self, s: &vmstate::HypervVp, now: u64, memory: &GuestMemory) -> vmstate::Result<()> {
        let bad = |what| Err(vmstate::Error(what));
        if s.scontrol & !1 != 0 {
            return bad("Hyper-V's SCONTROL has reserved bits");
        }
        for page in s.pages {
            if page & PAGE_ENABLED != 0 {
                let gpa = page & !0xFFF;
                if !memory.is_ram(gpa, kcore::consts::PAGE_SIZE as u64) || memory.overlay_contains(gpa) {
                    return bad("a Hyper-V SynIC page outside RAM or under an overlay");
                }
            }
        }
        if s.vp_assist & PAGE_ENABLED != 0 {
            let gpa = s.vp_assist & !0xFFF;
            let synic = s.pages.iter().any(|page| page & PAGE_ENABLED != 0 && page & !0xFFF == gpa);
            if synic || !memory.is_ram(gpa, kcore::consts::PAGE_SIZE as u64) || memory.overlay_contains(gpa) {
                return bad("a Hyper-V VP assist page outside RAM, under an overlay or shared with SynIC");
            }
        }
        if s.sints.iter().any(|&value| {
            value & !SINT_WRITABLE != 0 || value & SINT_MASKED == 0 && value & SINT_VECTOR < 16
        }) {
            return bad("a Hyper-V SINT with reserved bits or a reserved vector");
        }
        for timer in &s.timers {
            let mut config = timer.config;
            Self::normalize(&mut config);
            if !Self::valid(timer.config) || config != timer.config
                || timer.count == 0 && timer.due_in.is_some()
                || timer.config & STIMER_ENABLE == 0 && timer.due_in.is_some()
            {
                return bad("a Hyper-V stimer state the interface cannot have");
            }
        }
        self.scontrol = s.scontrol;
        self.pages = s.pages;
        self.sints = s.sints;
        self.vp_assist = s.vp_assist;
        self.timers = core::array::from_fn(|i| {
            let timer = s.timers[i];
            Stimer {
                config: timer.config,
                count: timer.count,
                expires: timer.due_in.map(|due| now.saturating_add(due)),
            }
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver_message(simp: u64, sint_index: u64, sint: u64, index: usize, expires: u64, now: u64, periodic: bool,
                       memory: &GuestMemory, lapic: &mut Lapic) -> bool {
        if simp & PAGE_ENABLED == 0 {
            return false;
        }
        let at = (simp & !0xFFF) + sint_index * 256;
        let mut kind = [0u8; 4];
        if memory.read(at, &mut kind).is_err() {
            return false;
        }
        if u32::from_le_bytes(kind) != 0 {
            if periodic {
                return true;
            }
            let mut flags = [0u8];
            if memory.read(at + 5, &mut flags).is_err() {
                return false;
            }
            flags[0] |= 1;
            let _ = memory.write(at + 5, &flags);
            return false;
        }
        let mut message = [0u8; 40];
        message[0..4].copy_from_slice(&0x8000_0010u32.to_le_bytes());
        message[4] = 24;
        message[16..20].copy_from_slice(&(index as u32).to_le_bytes());
        message[24..32].copy_from_slice(&expires.to_le_bytes());
        message[32..40].copy_from_slice(&now.to_le_bytes());
        if memory.write(at, &message).is_err() || sint & SINT_MASKED != 0 {
            return false;
        }
        lapic.accept((sint & SINT_VECTOR) as u8)
    }

    /// Request every timer due at `now`; its next expiration, or None where
    /// none is armed, its direct vector is waiting, or its message cannot run.
    pub fn poll(&mut self, now: u64, lapic: &mut Lapic, memory: &GuestMemory) -> Option<u64> {
        let mut next = None;
        for (index, timer) in self.timers.iter_mut().enumerate() {
            if timer.config & STIMER_ENABLE == 0 || timer.count == 0 {
                continue;
            }
            let direct = timer.config & STIMER_DIRECT != 0;
            let vector = ((timer.config & STIMER_VECTOR) >> 4) as u8;
            if direct && lapic.requested(vector) {
                continue;
            }
            let expires = *timer.expires.get_or_insert_with(|| {
                if timer.config & STIMER_PERIODIC != 0 { now.saturating_add(timer.count) } else { timer.count }
            });
            if now >= expires {
                let periodic = timer.config & STIMER_PERIODIC != 0;
                let delivered = if direct {
                    lapic.accept(vector)
                } else {
                    let sint_index = (timer.config & STIMER_SINT) >> 16;
                    let sint = self.sints[sint_index as usize];
                    Self::deliver_message(self.pages[1], sint_index, sint, index, expires, now, periodic, memory, lapic)
                };
                if !delivered {
                    continue;
                }
                if periodic {
                    let behind = now - expires;
                    timer.expires = Some(now.saturating_add(timer.count - behind % timer.count));
                } else {
                    timer.config &= !STIMER_ENABLE;
                    timer.expires = None;
                    continue;
                }
            }
            if let Some(expires) = timer.expires {
                next = Some(next.map_or(expires, |n: u64| n.min(expires)));
            }
        }
        next
    }
}

#[derive(Default)]
struct Shared {
    guest_os_id: u64,
    hypercall: u64,
    reference_tsc: u64,
    crash: [u64; 5],
    crashed: bool,
}

pub struct Hyperv {
    shared: Mutex<Shared>,
    /// The hypercall page's vendor-specific opcode and near return.
    code: [u8; 4],
    /// The partition's reference-time epoch, in its virtual TSC's ticks.
    epoch: u64,
    tsc_hz: u64,
    /// TscSequence, TscScale and TscOffset, as the guest's page has them.
    reference: [u8; 24],
}

impl Hyperv {
    fn make_reference(epoch: u64, tsc_hz: u64) -> Result<[u8; 24]> {
        if tsc_hz == 0 {
            return Err(Error::NotImplemented);
        }
        let scale = u64::try_from((REFERENCE_HZ << 64) / u128::from(tsc_hz)).map_err(|_| Error::NotImplemented)?;
        let offset = (u128::from(epoch) * u128::from(scale)) >> 64;
        let offset = i64::try_from(offset).ok().and_then(|v| v.checked_neg()).ok_or(Error::NotImplemented)?;
        if scale == 0 {
            return Err(Error::NotImplemented);
        }
        let mut reference = [0u8; 24];
        reference[0..4].copy_from_slice(&1u32.to_le_bytes());
        reference[8..16].copy_from_slice(&scale.to_le_bytes());
        reference[16..24].copy_from_slice(&offset.to_le_bytes());
        Ok(reference)
    }

    #[allow(unreachable_patterns)]
    pub fn new(vendor: Vendor, epoch: u64, tsc_hz: u64) -> Result<Hyperv> {
        let code = match vendor {
            Vendor::Svm => [0x0F, 0x01, 0xD9, 0xC3],
            Vendor::Vmx => [0x0F, 0x01, 0xC1, 0xC3],
            _ => return Err(Error::NotImplemented),
        };
        if tsc_hz == 0 {
            return Err(Error::NotImplemented);
        }
        let reference = Self::make_reference(epoch, tsc_hz)?;
        Ok(Hyperv {
            shared: Mutex::new(Shared::default()).ok_or(Error::NoMemory)?,
            code,
            epoch,
            tsc_hz,
            reference,
        })
    }

    pub fn code(&self) -> &[u8] {
        &self.code
    }

    pub fn reference_page(&self) -> &[u8] {
        &self.reference
    }

    pub fn save(&self) -> vmstate::HypervShared {
        let s = self.shared.lock();
        vmstate::HypervShared {
            guest_os_id: s.guest_os_id,
            hypercall: s.hypercall,
            reference_tsc: s.reference_tsc,
            crash: s.crash,
            crashed: s.crashed,
            epoch: self.epoch,
            tsc_hz: self.tsc_hz,
        }
    }

    pub fn restore(&mut self, s: &vmstate::HypervShared, vm_tsc_hz: u64, memory: &GuestMemory) -> vmstate::Result<()> {
        let bad = |what| Err(vmstate::Error(what));
        if s.tsc_hz == 0 || s.tsc_hz != vm_tsc_hz {
            return bad("Hyper-V's TSC rate differs from the VM's");
        }
        if s.hypercall & HYPERCALL_ENABLE != 0 && s.guest_os_id == 0 {
            return bad("Hyper-V's hypercall page is enabled with no guest OS");
        }
        let valid_gpa = |value: u64| {
            let gpa = value & !0xFFF;
            gpa != crate::memory::XAPIC_PAGE
                && gpa.checked_add(kcore::consts::PAGE_SIZE as u64).is_some_and(|end| end <= crate::memory::MAX_GPA)
        };
        if !valid_gpa(s.hypercall) || !valid_gpa(s.reference_tsc) {
            return bad("a Hyper-V overlay at an invalid guest address");
        }
        if s.hypercall & HYPERCALL_ENABLE != 0 && s.reference_tsc & REFERENCE_TSC_ENABLE != 0
            && s.hypercall & HYPERCALL_GPFN == s.reference_tsc & REFERENCE_TSC_GPFN
        {
            return bad("Hyper-V's overlays collide");
        }
        let reference = Self::make_reference(s.epoch, s.tsc_hz)
            .map_err(|_| vmstate::Error("Hyper-V's reference clock cannot be restored"))?;
        memory.prepare_overlay(OverlaySlot::Hypercall, &self.code)
            .map_err(|_| vmstate::Error("Hyper-V's hypercall page cannot be restored"))?;
        memory.prepare_overlay(OverlaySlot::ReferenceTsc, &reference)
            .map_err(|_| vmstate::Error("Hyper-V's reference page cannot be restored"))?;
        let map = |slot, enabled, value| {
            let result = if enabled {
                memory.map_overlay(slot, value & !0xFFF)
            } else {
                memory.unmap_overlay(slot)
            };
            result.map_err(|_| vmstate::Error("Hyper-V's overlay mapping cannot be restored"))
        };
        map(OverlaySlot::Hypercall, s.hypercall & HYPERCALL_ENABLE != 0, s.hypercall)?;
        map(OverlaySlot::ReferenceTsc, s.reference_tsc & REFERENCE_TSC_ENABLE != 0, s.reference_tsc)?;
        self.epoch = s.epoch;
        self.tsc_hz = s.tsc_hz;
        self.reference = reference;
        *self.shared.lock() = Shared {
            guest_os_id: s.guest_os_id,
            hypercall: s.hypercall,
            reference_tsc: s.reference_tsc,
            crash: s.crash,
            crashed: s.crashed,
        };
        Ok(())
    }

    /// Reference time in 100 ns units, zero at the partition's creation.
    pub fn reference_time(&self, tsc: u64) -> u64 {
        let ticks = u128::from(tsc.wrapping_sub(self.epoch)) * REFERENCE_HZ / u128::from(self.tsc_hz);
        ticks.min(u128::from(u64::MAX)) as u64
    }

    /// Read one of the interface's MSRs at virtual TSC `tsc`, or None for
    /// one it does not own.
    pub fn read(&self, msr: u32, vp: u32, tsc: u64) -> Option<u64> {
        let s = self.shared.lock();
        match msr {
            MSR_GUEST_OS_ID => Some(s.guest_os_id),
            MSR_HYPERCALL => Some(s.hypercall),
            MSR_VP_INDEX => Some(u64::from(vp)),
            MSR_TIME_REF_COUNT => Some(self.reference_time(tsc)),
            MSR_REFERENCE_TSC => Some(s.reference_tsc),
            MSR_TSC_FREQUENCY => Some(self.tsc_hz),
            MSR_APIC_FREQUENCY => Some(APIC_HZ),
            MSR_CRASH_CTL => Some(CRASH_NOTIFY),
            MSR_CRASH_P0..MSR_CRASH_CTL => Some(s.crash[(msr - MSR_CRASH_P0) as usize]),
            _ => None,
        }
    }

    /// Write one of them: Some(false) is a #GP, None not this interface's.
    pub fn write(&self, msr: u32, value: u64, memory: &GuestMemory) -> Option<bool> {
        if matches!(msr, MSR_VP_INDEX | MSR_TIME_REF_COUNT | MSR_TSC_FREQUENCY | MSR_APIC_FREQUENCY) {
            return Some(false);
        }
        if msr != MSR_GUEST_OS_ID && msr != MSR_HYPERCALL && msr != MSR_REFERENCE_TSC
            && !(MSR_CRASH_P0..=MSR_CRASH_CTL).contains(&msr)
        {
            return None;
        }
        let mut s = self.shared.lock();
        if msr == MSR_CRASH_CTL {
            s.crashed = value & CRASH_NOTIFY != 0;
            return Some(true);
        }
        if (MSR_CRASH_P0..MSR_CRASH_CTL).contains(&msr) {
            s.crash[(msr - MSR_CRASH_P0) as usize] = value;
            return Some(true);
        }
        if msr == MSR_GUEST_OS_ID {
            if value == 0 && s.hypercall & HYPERCALL_ENABLE != 0 && memory.unmap_overlay(OverlaySlot::Hypercall).is_err() {
                return Some(false);
            }
            s.guest_os_id = value;
            if value == 0 {
                s.hypercall &= !HYPERCALL_ENABLE;
            }
            return Some(true);
        }

        if msr == MSR_REFERENCE_TSC {
            let old = s.reference_tsc;
            let next = (value & !REFERENCE_TSC_RESERVED) | (old & REFERENCE_TSC_RESERVED);
            let gpa = next & REFERENCE_TSC_GPFN;
            if gpa == crate::memory::XAPIC_PAGE
                || gpa.checked_add(kcore::consts::PAGE_SIZE as u64).is_none_or(|end| end > crate::memory::MAX_GPA)
            {
                return Some(false);
            }
            let mapped = if next & REFERENCE_TSC_ENABLE != 0 {
                memory.map_overlay(OverlaySlot::ReferenceTsc, gpa)
            } else {
                memory.unmap_overlay(OverlaySlot::ReferenceTsc)
            };
            if mapped.is_err() {
                return Some(false);
            }
            s.reference_tsc = next;
            return Some(true);
        }

        let old = s.hypercall;
        let mut next = (value & !HYPERCALL_RESERVED) | (old & HYPERCALL_RESERVED);
        if old & HYPERCALL_LOCKED != 0 {
            if next & HYPERCALL_LOCKED == 0 || next & HYPERCALL_GPFN != old & HYPERCALL_GPFN {
                return Some(false);
            }
            next |= HYPERCALL_LOCKED;
        }
        if next & HYPERCALL_ENABLE != 0 && s.guest_os_id == 0 {
            next &= !HYPERCALL_ENABLE;
        }
        let gpa = next & HYPERCALL_GPFN;
        if gpa == crate::memory::XAPIC_PAGE
            || gpa.checked_add(kcore::consts::PAGE_SIZE as u64).is_none_or(|end| end > crate::memory::MAX_GPA)
        {
            return Some(false);
        }
        let mapped = if next & HYPERCALL_ENABLE != 0 {
            memory.map_overlay(OverlaySlot::Hypercall, gpa)
        } else {
            memory.unmap_overlay(OverlaySlot::Hypercall)
        };
        if mapped.is_err() {
            return Some(false);
        }
        s.hypercall = next;
        Some(true)
    }

    /// Decode a call when the interface is enabled; None leaves the opcode #UD.
    pub fn call(&self, input: u64) -> Option<core::result::Result<Call, u64>> {
        let s = self.shared.lock();
        (s.guest_os_id != 0 && s.hypercall & HYPERCALL_ENABLE != 0).then_some(Call::decode(input))
    }

    /// The five crash parameters once CrashNotify arrives.
    pub fn crashed(&self) -> Option<[u64; 5]> {
        let s = self.shared.lock();
        s.crashed.then_some(s.crash)
    }
}
