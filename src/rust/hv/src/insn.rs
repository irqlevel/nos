//! The x86 instructions a guest reaches MMIO with, decoded -- as far as it
//! takes to perform one for the guest: which way the data goes, how much of
//! it, from or into which register or from which immediate, and how long the
//! instruction is, to step past it.
//!
//! A nested page fault on a device's page says where the guest reached and
//! whether it wrote, and nothing else: not the register, not the size, not
//! the length. Those are in the instruction, which the caller fetches from
//! the guest's memory by its own paging (`crate::walk`) and hands here as
//! bytes. What is decoded is what Linux's MMIO accessors are: `readl` and
//! `writel` and their kind are `mov`s in inline assembly -- a register
//! loaded from memory or stored to it (0x8A/0x8B, 0x88/0x89), an immediate
//! stored (0xC6/0xC7) -- and `movzx`/`movsx` for the narrow reads
//! (0x0F 0xB6/0xB7/0xBE/0xBF). That is the set Linux itself decodes when it
//! emulates MMIO for a confidential guest (`insn_decode_mmio`), where it
//! must handle every access its own drivers make. Their memory operand may
//! take any addressing form the compiler picks -- a fixmap address is an
//! absolute displacement through a SIB byte -- so every form is taken apart
//! to be stepped over; the address itself is not worked out, the fault
//! having said it already.
//!
//! Anything else -- another opcode, a string move, an access through a
//! register rather than memory, 16-bit addressing, a lock prefix -- is not an
//! MMIO access this decodes, and is `None`: the caller stops the guest and
//! shows the bytes, rather than guess. Pure code over a slice, and nothing
//! of the guest's is trusted: every length is checked against what is there.

/// The most bytes an x86 instruction has.
pub const MAX_LEN: usize = 15;

/* Prefixes. */
const OPERAND_SIZE: u8 = 0x66;
const ADDRESS_SIZE: u8 = 0x67;
const LOCK: u8 = 0xF0;
const REPNE: u8 = 0xF2;
const REP: u8 = 0xF3;
const SEGMENT_OVERRIDES: [u8; 6] = [0x26, 0x2E, 0x36, 0x3E, 0x64, 0x65];
/// REX, in 64-bit mode: 0x40-0x4F, the low four bits W, R, X and B.
const REX_BASE: u8 = 0x40;
const REX_MASK: u8 = 0xF0;
const REX_W: u8 = 1 << 3;
const REX_R: u8 = 1 << 2;

/* Opcodes. */
const MOV_STORE_8: u8 = 0x88;
const MOV_STORE: u8 = 0x89;
const MOV_LOAD_8: u8 = 0x8A;
const MOV_LOAD: u8 = 0x8B;
const MOV_IMM_8: u8 = 0xC6;
const MOV_IMM: u8 = 0xC7;
const TWO_BYTE: u8 = 0x0F;
const MOVZX_8: u8 = 0xB6;
const MOVZX_16: u8 = 0xB7;
const MOVSX_8: u8 = 0xBE;
const MOVSX_16: u8 = 0xBF;

/* ModRM and SIB fields. */
const MOD_SHIFT: u8 = 6;
const REG_SHIFT: u8 = 3;
const FIELD_MASK: u8 = 0x7;
const MOD_REGISTER: u8 = 3;
const MOD_DISP8: u8 = 1;
const MOD_DISP32: u8 = 2;
/// rm 100: a SIB byte follows.
const RM_SIB: u8 = 4;
/// rm 101 with mod 00: a 32-bit displacement alone (RIP-relative in 64-bit
/// mode); and a SIB base of 101 with mod 00, the same with no base.
const RM_DISP32: u8 = 5;

/// The mode the CPU decodes in, which says what REX and the operand- and
/// address-size prefixes mean.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 64-bit mode: REX prefixes, 32-bit operands by default.
    Long,
    /// 32-bit protected (or compatibility) mode: no REX.
    Protected32,
}

/// A general-purpose register, as an instruction names it for the width of
/// its operand: by number, 0-15 in the encoding's order (RAX, RCX, RDX, RBX,
/// RSP, RBP, RSI, RDI, R8-R15) -- or, for a byte operand without a REX
/// prefix, one of AH, CH, DH and BH, bits 15:8 of RAX to RBX.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reg {
    pub num: u8,
    pub high_byte: bool,
}

/// What a loaded value becomes in its register, wider than the access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Extend {
    /// As wide as the access (`mov`).
    None,
    /// Zero-extended (`movzx`).
    Zero,
    /// Sign-extended (`movsx`).
    Sign,
}

/// What the instruction does with the memory it faulted on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// Reads `size` bytes into `reg`, `dest_size` bytes wide, extended as
    /// `extend` says.
    Load { reg: Reg, size: u8, dest_size: u8, extend: Extend },
    /// Writes `size` bytes of `reg`.
    Store { reg: Reg, size: u8 },
    /// Writes `size` bytes of `value`, the immediate as the instruction
    /// extends it to its operand.
    StoreImm { value: u64, size: u8 },
}

impl Access {
    /// Whether it writes the device.
    pub fn is_write(&self) -> bool {
        !matches!(self, Access::Load { .. })
    }

    /// How many bytes of the device it reaches.
    pub fn size(&self) -> u8 {
        match *self {
            Access::Load { size, .. } | Access::Store { size, .. } | Access::StoreImm { size, .. } => size,
        }
    }
}

/// A decoded instruction: its access, and its length.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Insn {
    pub access: Access,
    pub len: u8,
}

/// A cursor over the instruction's bytes: every read checked against the
/// slice, which the caller filled with what the guest's memory had.
struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}

impl Bytes<'_> {
    fn next(&mut self) -> Option<u8> {
        let v = *self.b.get(self.at)?;
        self.at += 1;
        Some(v)
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn skip(&mut self, n: usize) -> Option<()> {
        let end = self.at.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        self.at = end;
        Some(())
    }

    fn le(&mut self, n: usize) -> Option<u64> {
        let mut v = 0u64;
        for i in 0..n {
            v |= u64::from(self.next()?) << (8 * i);
        }
        Some(v)
    }
}

/// Decode the instruction at the start of `bytes` -- at most `MAX_LEN` of
/// them, and fewer only where the guest's memory ends -- in `mode`: the
/// access it makes, or None for one this does not decode.
pub fn decode(bytes: &[u8], mode: Mode) -> Option<Insn> {
    let bytes = &bytes[..bytes.len().min(MAX_LEN)];
    let mut c = Bytes { b: bytes, at: 0 };

    /* Legacy prefixes, in any order; then, in 64-bit mode, a REX -- which
     * counts only as the last byte before the opcode. */
    let mut operand16 = false;
    let mut address_size = false;
    let mut rex = 0u8;
    loop {
        let b = c.peek()?;
        match b {
            OPERAND_SIZE => operand16 = true,
            ADDRESS_SIZE => address_size = true,
            /* A lock on a `mov` is #UD; a repeat is no MMIO access this
             * decodes -- a string move, or a `mov` a compiler would not
             * give one. */
            LOCK | REP | REPNE => return None,
            b if SEGMENT_OVERRIDES.contains(&b) => {}
            b if mode == Mode::Long && b & REX_MASK == REX_BASE => {
                c.next()?;
                rex = b;
                /* Anything but the opcode after it makes it void, and a
                 * prefix after a REX is no encoding a compiler emits. */
                if matches!(c.peek()?, OPERAND_SIZE | ADDRESS_SIZE | LOCK | REP | REPNE)
                    || SEGMENT_OVERRIDES.contains(&c.peek()?)
                    || c.peek()? & REX_MASK == REX_BASE
                {
                    return None;
                }
                break;
            }
            _ => break,
        }
        c.next()?;
    }
    /* In 32-bit mode, 0x67 makes the addressing 16-bit, whose ModRM is
     * another table: not an access Linux makes. In 64-bit mode it makes the
     * address 32 bits, the same forms. */
    if address_size && mode == Mode::Protected32 {
        return None;
    }

    let wide = if rex & REX_W != 0 { 8 } else if operand16 { 2 } else { 4 };
    let opcode = c.next()?;
    let (kind, size) = match opcode {
        MOV_STORE_8 => (Kind::Store, 1),
        MOV_STORE => (Kind::Store, wide),
        MOV_LOAD_8 => (Kind::Load(Extend::None, 1), 1),
        MOV_LOAD => (Kind::Load(Extend::None, wide), wide),
        MOV_IMM_8 => (Kind::Imm, 1),
        MOV_IMM => (Kind::Imm, wide),
        TWO_BYTE => match c.next()? {
            MOVZX_8 => (Kind::Load(Extend::Zero, wide), 1),
            MOVZX_16 => (Kind::Load(Extend::Zero, wide), 2),
            MOVSX_8 => (Kind::Load(Extend::Sign, wide), 1),
            MOVSX_16 => (Kind::Load(Extend::Sign, wide), 2),
            _ => return None,
        },
        _ => return None,
    };

    let modrm = c.next()?;
    let md = modrm >> MOD_SHIFT;
    let reg_field = (modrm >> REG_SHIFT) & FIELD_MASK;
    let rm = modrm & FIELD_MASK;
    if md == MOD_REGISTER {
        /* A register operand: no memory, so no MMIO. */
        return None;
    }
    /* The rest of the memory operand: a SIB byte, and a displacement. */
    let mut disp = match md {
        MOD_DISP8 => 1,
        MOD_DISP32 => 4,
        _ => 0,
    };
    if rm == RM_SIB {
        let sib = c.next()?;
        if md == 0 && sib & FIELD_MASK == RM_DISP32 {
            disp = 4;
        }
    } else if md == 0 && rm == RM_DISP32 {
        disp = 4;
    }
    c.skip(disp)?;

    let access = match kind {
        Kind::Store => Access::Store { reg: reg(reg_field, rex, size), size },
        Kind::Load(extend, dest_size) => {
            /* A `movzx`/`movsx` into a register no wider than its source is
             * no encoding a compiler emits: 0x66 0x0F 0xB7 would be a word
             * into a word. */
            if extend != Extend::None && dest_size <= size {
                return None;
            }
            Access::Load { reg: reg(reg_field, rex, dest_size), size, dest_size, extend }
        }
        Kind::Imm => {
            /* C6/C7's reg field is the /0 that makes them `mov`. */
            if reg_field != 0 {
                return None;
            }
            let value = match size {
                1 => c.le(1)?,
                2 => c.le(2)?,
                4 => c.le(4)?,
                /* A 64-bit store's immediate is 32 bits, sign-extended. */
                _ => c.le(4)? as u32 as i32 as i64 as u64,
            };
            Access::StoreImm { value, size }
        }
    };
    Some(Insn { access, len: c.at as u8 })
}

/// What the opcode said the access is, before the operand is decoded.
#[derive(Clone, Copy)]
enum Kind {
    Store,
    /// A load, and how its value is extended into a register this wide.
    Load(Extend, u8),
    Imm,
}

/// The register ModRM's reg field names -- with REX.R, the high eight --
/// for an operand of `size` bytes: a byte operand without a REX prefix
/// names AH to BH by 4 to 7.
fn reg(field: u8, rex: u8, size: u8) -> Reg {
    let num = field | if rex & REX_R != 0 { 8 } else { 0 };
    let high_byte = size == 1 && rex == 0 && (4..8).contains(&field);
    Reg { num: if high_byte { field - 4 } else { num }, high_byte }
}
