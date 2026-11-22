use crate::wire::{In, Out};
use crate::{Error, Result};

const CAP: u32 = 0xFE;
const DEF_TYPE: u32 = 0x2FF;
const FIXED_FIRST: u32 = 0x250;
const FIXED_LAST: u32 = 0x26F;
const VARIABLE_FIRST: u32 = 0x200;
const VARIABLE_LAST: u32 = 0x20F;
const VARIABLE_PAIRS: usize = 8;
const FIXED_REGISTERS: usize = 11;
const CAP_VALUE: u64 = 0x508;
const ENABLE: u64 = 1 << 11;
const FIXED_ENABLE: u64 = 1 << 12;

/// The MTRRs of one vCPU, stored but never applied to the nested page table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mtrrs {
    pub def_type: u64,
    pub fixed: [u64; FIXED_REGISTERS],
    pub base: [u64; VARIABLE_PAIRS],
    pub mask: [u64; VARIABLE_PAIRS],
}

impl Mtrrs {
    pub fn is_msr(msr: u32) -> bool {
        matches!(msr, CAP | DEF_TYPE | 0x250 | 0x258 | 0x259 | 0x268..=0x26F | VARIABLE_FIRST..=VARIABLE_LAST)
    }

    pub fn read(&self, msr: u32) -> Option<u64> {
        if !Self::is_msr(msr) {
            return None;
        }
        Some(match msr {
            CAP => CAP_VALUE,
            DEF_TYPE => self.def_type,
            FIXED_FIRST..=FIXED_LAST => self.fixed[fixed_index(msr)?],
            _ => {
                let i = ((msr - VARIABLE_FIRST) / 2) as usize;
                if msr & 1 == 0 { self.base[i] } else { self.mask[i] }
            }
        })
    }

    pub fn write(&mut self, msr: u32, value: u64, phys_bits: u32) -> bool {
        if !Self::is_msr(msr) || msr == CAP {
            return false;
        }
        let valid = match msr {
            DEF_TYPE => valid_def(value),
            FIXED_FIRST..=FIXED_LAST => valid_fixed(value),
            _ if msr & 1 == 0 => valid_base(value, phys_bits),
            _ => valid_mask(value, phys_bits),
        };
        if !valid {
            return false;
        }
        match msr {
            DEF_TYPE => self.def_type = value,
            FIXED_FIRST..=FIXED_LAST => {
                if let Some(i) = fixed_index(msr) { self.fixed[i] = value } else { return false }
            }
            _ => {
                let i = ((msr - VARIABLE_FIRST) / 2) as usize;
                if msr & 1 == 0 { self.base[i] = value } else { self.mask[i] = value }
            }
        }
        true
    }

    pub fn valid(&self, phys_bits: u32) -> bool {
        valid_def(self.def_type)
            && self.fixed.iter().all(|&v| valid_fixed(v))
            && self.base.iter().all(|&v| valid_base(v, phys_bits))
            && self.mask.iter().all(|&v| valid_mask(v, phys_bits))
    }

    pub(crate) fn put(&self, o: &mut Out) {
        o.u64(self.def_type);
        for v in self.fixed {
            o.u64(v);
        }
        for v in self.base {
            o.u64(v);
        }
        for v in self.mask {
            o.u64(v);
        }
    }

    pub(crate) fn get(r: &mut In) -> Result<Mtrrs> {
        let def_type = r.u64("MTRR_DEF_TYPE")?;
        let mut fixed = [0; FIXED_REGISTERS];
        for v in &mut fixed {
            *v = r.u64("a fixed MTRR")?;
        }
        let mut base = [0; VARIABLE_PAIRS];
        for v in &mut base {
            *v = r.u64("a variable MTRR base")?;
        }
        let mut mask = [0; VARIABLE_PAIRS];
        for v in &mut mask {
            *v = r.u64("a variable MTRR mask")?;
        }
        let m = Mtrrs { def_type, fixed, base, mask };
        if !valid_def(m.def_type) {
            return Err(Error("MTRR_DEF_TYPE with a reserved bit or memory type"));
        }
        if !m.fixed.iter().all(|&v| valid_fixed(v)) {
            return Err(Error("a fixed MTRR with a reserved memory type"));
        }
        if !m.base.iter().all(|&v| valid_base(v, 52)) {
            return Err(Error("a variable MTRR base with a reserved bit or memory type"));
        }
        if !m.mask.iter().all(|&v| valid_mask(v, 52)) {
            return Err(Error("a variable MTRR mask with a reserved bit"));
        }
        Ok(m)
    }
}

fn fixed_index(msr: u32) -> Option<usize> {
    match msr {
        0x250 => Some(0),
        0x258 => Some(1),
        0x259 => Some(2),
        0x268..=0x26F => Some(3 + (msr - 0x268) as usize),
        _ => None,
    }
}

fn memory_type(v: u64) -> bool {
    matches!(v, 0 | 1 | 4 | 5 | 6 | 7)
}

fn address_mask(phys_bits: u32) -> u64 {
    (1u64 << phys_bits.clamp(12, 52)) - 1
}

fn valid_def(v: u64) -> bool {
    memory_type(v & 0xFF) && v & !(0xFF | ENABLE | FIXED_ENABLE) == 0
}

fn valid_fixed(v: u64) -> bool {
    v.to_le_bytes().iter().all(|&t| memory_type(u64::from(t)))
}

fn valid_base(v: u64, phys_bits: u32) -> bool {
    memory_type(v & 0xFF) && v & !((address_mask(phys_bits) & !0xFFF) | 0xFF) == 0
}

fn valid_mask(v: u64, phys_bits: u32) -> bool {
    v & !((address_mask(phys_bits) & !0xFFF) | ENABLE) == 0
}
