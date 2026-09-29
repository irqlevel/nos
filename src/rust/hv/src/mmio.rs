//! Performing a guest's MMIO access for it: the instruction that faulted on a
//! device's page fetched by the guest's own paging (`crate::walk`), decoded
//! (`crate::insn`), and carried out -- the value a store writes taken from
//! its register or immediate, the value a load reads put into its register,
//! and the instruction stepped past.
//!
//! In two steps, because the device is the caller's: [`begin`] decodes and
//! checks the instruction against the fault and says what is to be read or
//! written; the caller does it; [`finish`] puts a load's value in its
//! register and moves RIP on. Nothing is changed in the guest until
//! `finish`, so an access the caller refuses leaves the guest as it
//! faulted.
//!
//! What is checked, the instruction being the guest's own bytes: that the
//! fault was an instruction's at all, not an event's delivery; that it is
//! one decoded at all; that it goes the way the fault did -- a load for a
//! read, a store for a write -- which an instruction changed under the fault
//! by another of the guest's CPUs could not be trusted to; that the access
//! stays in the page it faulted on; and that it is none a VT-x exit would
//! lose (RSP, which that backend writes back only on its full sync). Each is
//! an [`Error`], for the caller to stop the guest over with the bytes shown.

use hvarch::x86::svm::vmcb::{self, Save};
use hvarch::x86::svm::GuestRegs;

use crate::insn::{self, Access, Extend, Insn, Mode, Reg};
use crate::memory::GuestMemory;
use crate::vm::Backend;
use crate::walk::{self, Paging};

/// Why an access was not performed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The fault came delivering an interrupt or an exception -- the stack
    /// it pushes to, the IDT it reads -- not from the instruction at RIP,
    /// which is the one the event came before.
    Delivery,
    /// The code could not be read: its linear address has no page, or no
    /// RAM, in the guest's paging -- or paging this does not walk.
    Fetch(walk::Miss),
    /// The CPU is in a mode this does not decode in: real mode, 16-bit code.
    Mode,
    /// The bytes are no MMIO access decoded here.
    Decode,
    /// The instruction goes the other way than the fault did.
    Direction,
    /// The access runs past the end of the page it faulted on.
    Crosses,
    /// It loads RSP, which is not written back on every exit.
    StackPointer,
}

impl Error {
    /// For a report.
    pub fn describe(&self) -> &'static str {
        match self {
            Error::Delivery => "the fault came delivering an interrupt or exception, not from an instruction",
            Error::Fetch(walk::Miss::NonCanonical) => "its RIP is not canonical",
            Error::Fetch(walk::Miss::NotMapped) => "its code is not mapped, or not RAM",
            Error::Fetch(walk::Miss::Unsupported) => "its paging is 32-bit, which is not walked",
            Error::Mode => "the CPU is in real mode or 16-bit code",
            Error::Decode => "the instruction is no MMIO access emulated here",
            Error::Direction => "the instruction goes the other way than its fault",
            Error::Crosses => "the access runs past the device's page",
            Error::StackPointer => "the instruction loads RSP",
        }
    }
}

/// An access decoded and checked: what to do with the device.
#[derive(Clone, Copy, Debug)]
pub struct Op {
    pub insn: Insn,
    /// The device's address the access starts at.
    pub gpa: u64,
    /// For a store, the value it writes, `size` bytes of it.
    pub value: u64,
}

impl Op {
    pub fn is_write(&self) -> bool {
        self.insn.access.is_write()
    }

    pub fn size(&self) -> u8 {
        self.insn.access.size()
    }
}

/// The instruction's bytes, for a report: as many as were fetched.
#[derive(Clone, Copy, Debug, Default)]
pub struct Bytes {
    pub b: [u8; insn::MAX_LEN],
    pub len: u8,
}

/// The mode the CPU decodes in: 64-bit code, or 32-bit protected; None for
/// anything else.
fn mode(save: &Save) -> Option<Mode> {
    const EFER_LMA: u64 = 1 << 10;
    const CR0_PE: u64 = 1 << 0;
    let long = save.efer & EFER_LMA != 0 && save.cs.attrib & vmcb::attrib::L != 0;
    if long {
        Some(Mode::Long)
    } else if save.cr0 & CR0_PE != 0 && save.cs.attrib & vmcb::attrib::DB != 0 {
        Some(Mode::Protected32)
    } else {
        None
    }
}

/// Decode the instruction the guest faulted at on `gpa`, a `write` or a
/// read, and check it against the fault: the access to make, or why not.
/// `bytes` gets what was fetched, whatever the outcome.
pub fn begin(v: &Backend, memory: &GuestMemory, gpa: u64, write: bool, bytes: &mut Bytes) -> Result<Op, Error> {
    /* An event whose delivery the fault cut short is queued again by the
     * time the exit is handled (`requeue_event`): the access was the
     * delivery's, and the instruction at RIP -- the one the event came
     * before -- has nothing to do with it. */
    if v.event_queued() {
        return Err(Error::Delivery);
    }
    let save = v.save();
    let mode = mode(save).ok_or(Error::Mode)?;
    let paging = Paging { cr0: save.cr0, cr3: save.cr3, cr4: save.cr4, efer: save.efer };
    /* 64-bit code ignores CS's base; 32-bit code adds it, and wraps at 4 GiB. */
    let la = match mode {
        Mode::Long => save.rip,
        Mode::Protected32 => save.cs.base.wrapping_add(save.rip) & 0xFFFF_FFFF,
    };
    let read_u64 = |gpa: u64| {
        let mut q = [0u8; 8];
        memory.read(gpa, &mut q).ok().map(|_| u64::from_le_bytes(q))
    };
    let read_bytes = |gpa: u64, buf: &mut [u8]| memory.read(gpa, buf).is_ok();
    let n = walk::fetch(&paging, la, &mut bytes.b, read_u64, read_bytes).map_err(Error::Fetch)?;
    bytes.len = n as u8;

    let insn = insn::decode(&bytes.b[..n], mode).ok_or(Error::Decode)?;
    if insn.access.is_write() != write {
        return Err(Error::Direction);
    }
    let size = u64::from(insn.access.size());
    if (gpa & (walk::PAGE_SIZE - 1)) + size > walk::PAGE_SIZE {
        return Err(Error::Crosses);
    }
    let regs = v.regs();
    let value = match insn.access {
        Access::Store { reg, size } => read_reg(save, regs, reg, size),
        Access::StoreImm { value, size } => value & mask(size),
        Access::Load { reg, .. } => {
            if reg.num == RSP && !reg.high_byte {
                return Err(Error::StackPointer);
            }
            0
        }
    };
    Ok(Op { insn, gpa, value })
}

/// Complete `op`: a load's value -- what the device answered, `read` --
/// into its register, and the instruction stepped past, and out of an
/// interrupt shadow it was in, as the CPU would have left it.
pub fn finish(v: &mut Backend, op: &Op, read: u64) {
    if let Access::Load { reg, size, dest_size, extend } = op.insn.access {
        let (save, regs) = v.save_and_regs_mut();
        let value = read & mask(size);
        let value = match extend {
            Extend::Sign => sign_extend(value, size) & mask(dest_size),
            Extend::Zero | Extend::None => value,
        };
        write_reg(save, regs, reg, dest_size, value);
    }
    v.skip_emulated(u64::from(op.insn.len));
}

/// RSP's number in the encoding.
const RSP: u8 = 4;

/// All ones in `size` bytes.
fn mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}

/// `value`, `size` bytes of it, sign-extended to 64 bits.
fn sign_extend(value: u64, size: u8) -> u64 {
    let bits = u32::from(size) * 8;
    if bits >= 64 {
        return value;
    }
    let shift = 64 - bits;
    (((value << shift) as i64) >> shift) as u64
}

/// The whole of register `num`, as the guest has it.
fn gpr(save: &Save, regs: &GuestRegs, num: u8) -> u64 {
    match num {
        0 => save.rax,
        1 => regs.rcx,
        2 => regs.rdx,
        3 => regs.rbx,
        4 => save.rsp,
        5 => regs.rbp,
        6 => regs.rsi,
        7 => regs.rdi,
        8 => regs.r8,
        9 => regs.r9,
        10 => regs.r10,
        11 => regs.r11,
        12 => regs.r12,
        13 => regs.r13,
        14 => regs.r14,
        _ => regs.r15,
    }
}

fn gpr_mut<'a>(save: &'a mut Save, regs: &'a mut GuestRegs, num: u8) -> &'a mut u64 {
    match num {
        0 => &mut save.rax,
        1 => &mut regs.rcx,
        2 => &mut regs.rdx,
        3 => &mut regs.rbx,
        4 => &mut save.rsp,
        5 => &mut regs.rbp,
        6 => &mut regs.rsi,
        7 => &mut regs.rdi,
        8 => &mut regs.r8,
        9 => &mut regs.r9,
        10 => &mut regs.r10,
        11 => &mut regs.r11,
        12 => &mut regs.r12,
        13 => &mut regs.r13,
        14 => &mut regs.r14,
        _ => &mut regs.r15,
    }
}

/// `size` bytes of `reg` -- bits 15:8 of its register for AH to BH.
fn read_reg(save: &Save, regs: &GuestRegs, reg: Reg, size: u8) -> u64 {
    let full = gpr(save, regs, reg.num);
    if reg.high_byte {
        (full >> 8) & 0xFF
    } else {
        full & mask(size)
    }
}

/// Write `value` into `reg`, `size` bytes wide, as a load does: a 32-bit
/// destination clears the register's top half, a 16- or 8-bit one leaves
/// the rest as it was, and AH to BH are bits 15:8.
fn write_reg(save: &mut Save, regs: &mut GuestRegs, reg: Reg, size: u8, value: u64) {
    let r = gpr_mut(save, regs, reg.num);
    *r = if reg.high_byte {
        (*r & !0xFF00) | ((value & 0xFF) << 8)
    } else {
        match size {
            4 => value & 0xFFFF_FFFF,
            8 => value,
            s => (*r & !mask(s)) | (value & mask(s)),
        }
    };
}
