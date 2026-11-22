//! Microsoft Hv#1's partition-wide and per-virtual-processor state.

use alloc::vec::Vec;

use crate::wire::{list, In, Out};
use crate::{Error, Result, CPUS_MAX};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HypervShared {
    pub guest_os_id: u64,
    pub hypercall: u64,
    pub reference_tsc: u64,
    pub crash: [u64; 5],
    pub crashed: bool,
    /// The reference-time epoch and rate, in the guest TSC's ticks and Hz.
    pub epoch: u64,
    pub tsc_hz: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HypervTimer {
    pub config: u64,
    pub count: u64,
    /// Reference-time ticks until expiration at the pause, not an absolute
    /// guest or host timestamp.
    pub due_in: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HypervVp {
    pub scontrol: u64,
    pub pages: [u64; 3],
    pub sints: [u64; 16],
    pub timers: [HypervTimer; 4],
    pub vp_assist: u64,
}

impl HypervVp {
    /// Move deadlines from when a CPU parked to when the whole VM was captured.
    pub fn elapse(&mut self, ticks: u64) {
        for timer in &mut self.timers {
            timer.due_in = timer.due_in.map(|due| due.saturating_sub(ticks));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HypervState {
    pub shared: HypervShared,
    pub vps: Vec<HypervVp>,
}

impl HypervShared {
    fn put(&self, o: &mut Out) {
        o.u64(self.guest_os_id);
        o.u64(self.hypercall);
        o.u64(self.reference_tsc);
        for value in self.crash {
            o.u64(value);
        }
        o.bool(self.crashed);
        o.u64(self.epoch);
        o.u64(self.tsc_hz);
    }

    fn get(r: &mut In) -> Result<HypervShared> {
        let guest_os_id = r.u64("Hyper-V's guest OS ID")?;
        let hypercall = r.u64("Hyper-V's hypercall MSR")?;
        let reference_tsc = r.u64("Hyper-V's reference TSC MSR")?;
        let mut crash = [0u64; 5];
        for value in &mut crash {
            *value = r.u64("a Hyper-V crash parameter")?;
        }
        let crashed = r.bool("Hyper-V's crash notification")?;
        let epoch = r.u64("Hyper-V's reference-time epoch")?;
        let tsc_hz = r.u64("Hyper-V's TSC rate")?;
        if tsc_hz == 0 {
            return Err(Error("Hyper-V's reference clock has no rate"));
        }
        Ok(HypervShared { guest_os_id, hypercall, reference_tsc, crash, crashed, epoch, tsc_hz })
    }
}

impl HypervVp {
    fn put(&self, o: &mut Out) {
        o.u64(self.scontrol);
        for value in self.pages {
            o.u64(value);
        }
        for value in self.sints {
            o.u64(value);
        }
        for timer in self.timers {
            o.u64(timer.config);
            o.u64(timer.count);
            o.opt(timer.due_in, |o, due| o.u64(due));
        }
        o.u64(self.vp_assist);
    }

    fn get(r: &mut In) -> Result<HypervVp> {
        let scontrol = r.u64("Hyper-V's SCONTROL")?;
        let mut pages = [0u64; 3];
        for value in &mut pages {
            *value = r.u64("a Hyper-V SynIC page")?;
        }
        let mut sints = [0u64; 16];
        for value in &mut sints {
            *value = r.u64("a Hyper-V SINT")?;
        }
        let mut timers = [HypervTimer::default(); 4];
        for timer in &mut timers {
            timer.config = r.u64("a Hyper-V stimer's config")?;
            timer.count = r.u64("a Hyper-V stimer's count")?;
            timer.due_in = r.opt("a Hyper-V stimer's deadline", |r| r.u64("a Hyper-V stimer's deadline"))?;
        }
        let vp_assist = if r.version >= 5 { r.u64("Hyper-V's VP assist MSR")? } else { 0 };
        Ok(HypervVp { scontrol, pages, sints, timers, vp_assist })
    }
}

impl HypervState {
    pub(crate) fn put(&self, o: &mut Out) {
        self.shared.put(o);
        o.u8(self.vps.len() as u8);
        for vp in &self.vps {
            vp.put(o);
        }
    }

    pub(crate) fn get(r: &mut In) -> Result<HypervState> {
        let shared = HypervShared::get(r)?;
        let n = r.u8("Hyper-V's VP count")? as usize;
        let vps = list(r, n, CPUS_MAX, "too many Hyper-V VPs", HypervVp::get)?;
        Ok(HypervState { shared, vps })
    }
}
