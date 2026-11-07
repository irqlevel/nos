//! Raw DEFLATE (RFC 1951) -- what a compressed qcow2 cluster holds --
//! decoded as zlib's inflate decodes it. QEMU reads those clusters through
//! zlib, so a cluster this accepts must be one QEMU reads, to the byte, and
//! one QEMU refuses must be refused: every rule here is zlib's, checked in
//! zlib's order (its inflate.c and inftrees.c), and
//! scripts/diskimage-test.py holds the two to the same answer over streams
//! made and broken at random.
//!
//! A stream is decoded until `limit` bytes have come out of it -- a
//! cluster's size -- and handed to the caller a piece at a time, so that no
//! cluster has to fit in memory: what is kept is the last 32 KiB of output,
//! as far back as a match reaches, and a piece on its way out.
//!
//! When is a stream good? QEMU asks zlib for `limit` bytes and calls the
//! cluster good once zlib has filled them, whether or not the stream went
//! on, and bad when the stream or its input ends first. And zlib, its buffer
//! full, still reads on until a symbol would put out a byte -- the next
//! block's header and code tables, the codes of a match -- and fails on a
//! fault it finds there. So this reads on too, "dry": the stream is good if
//! that reaches a symbol with output, the end of its last block, or the end
//! of the input, with nothing wrong on the way.

use alloc::vec::Vec;
use core::fmt;

use crate::{buffer, CHUNK};

/// How far back a match reaches: the window.
const WINDOW: usize = 32 * 1024;
/// How much output is handed on at once.
const PIECE: usize = CHUNK;
/// The longest code there is, in bits.
const MAX_BITS: usize = 15;
/// A code of at most this many bits is found by one look in a table; a
/// longer one is walked to, a bit at a time.
const FAST_BITS: u32 = 9;
const FAST: usize = 1 << FAST_BITS;
/// The literal/length alphabet: 288 symbols in the fixed code, at most 286
/// in a dynamic one. The distance alphabet: 32, and at most 30. The
/// alphabet a dynamic block's code lengths are coded in: 19.
const LITLEN_SYMBOLS: usize = 288;
const DYNAMIC_LITLENS: usize = 286;
const DIST_SYMBOLS: usize = 32;
const DYNAMIC_DISTS: usize = 30;
const CODELEN_SYMBOLS: usize = 19;
/// The literal/length symbol that ends a block, and the first of a length.
const END_OF_BLOCK: u16 = 256;
const FIRST_LENGTH: u16 = 257;
/// The fewest literal/length codes, distance codes and code length codes a
/// dynamic block may say it has: what its header's fields count from.
const MIN_LITLENS: usize = 257;
const MIN_DISTS: usize = 1;
const MIN_CODELENS: usize = 4;
/// The code length code's symbols past the lengths 0 to 15: the previous
/// length again, 3 to 6 times; a zero, 3 to 10 times; and 11 to 138.
const REPEAT_PREVIOUS: u16 = 16;
const REPEAT_ZERO: u16 = 17;
/// The order a dynamic block lists its code length code's lengths in.
const CODELEN_ORDER: [usize; CODELEN_SYMBOLS] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
/// What each length symbol from 257 on stands for, and its extra bits.
const LENGTH_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
/// What each distance symbol stands for, and its extra bits.
const DIST_BASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
                              2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];
/// The fixed code's lengths: literals 0-143 and lengths 280-287 of 8 bits,
/// literals 144-255 of 9, 256-279 of 7; every distance of 5.
const FIXED_LITLEN: [(usize, u8); 4] = [(144, 8), (256, 9), (280, 7), (288, 8)];
const FIXED_DIST_BITS: u8 = 5;

/// What is wrong with a stream: zlib's own words where zlib has some.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The input ended before `limit` bytes came out.
    Truncated,
    /// The stream's last block ended before `limit` bytes came out.
    Short,
    BlockType,
    StoredLengths,
    TooManySymbols,
    CodeLengths,
    Repeat,
    NoEndOfBlock,
    LitLens,
    Dists,
    LitLenCode,
    DistCode,
    TooFarBack,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Fault::Truncated => "its data ends before the cluster is whole",
            Fault::Short => "its stream ends before the cluster is whole",
            Fault::BlockType => "invalid block type",
            Fault::StoredLengths => "invalid stored block lengths",
            Fault::TooManySymbols => "too many length or distance symbols",
            Fault::CodeLengths => "invalid code lengths set",
            Fault::Repeat => "invalid bit length repeat",
            Fault::NoEndOfBlock => "invalid code -- missing end-of-block",
            Fault::LitLens => "invalid literal/lengths set",
            Fault::Dists => "invalid distances set",
            Fault::LitLenCode => "invalid literal/length code",
            Fault::DistCode => "invalid distance code",
            Fault::TooFarBack => "invalid distance too far back",
        })
    }
}

/// Why a stream was not decoded: a fault of its own, or the caller's input
/// or output failing.
#[derive(Debug)]
pub enum Stop<E> {
    Fault(Fault),
    Read(E),
    Write(E),
}

/// Where the compressed bytes come from.
pub trait Input {
    type Error;
    /// The next byte, or None at the end of the input -- and None again
    /// after that.
    fn byte(&mut self) -> Result<Option<u8>, Self::Error>;
}

/// The stream's bits, least significant first, as DEFLATE packs them.
struct Bits<'a, I: Input> {
    input: &'a mut I,
    hold: u64,
    count: u32,
    ended: bool,
}

impl<'a, I: Input> Bits<'a, I> {
    fn new(input: &'a mut I) -> Self {
        Bits { input, hold: 0, count: 0, ended: false }
    }

    /// At least `n` bits held, `n` at most 32 -- or all the input has left.
    fn fill(&mut self, n: u32) -> Result<(), Stop<I::Error>> {
        while self.count < n && !self.ended {
            match self.input.byte().map_err(Stop::Read)? {
                Some(b) => {
                    self.hold |= u64::from(b) << self.count;
                    self.count += 8;
                }
                None => self.ended = true,
            }
        }
        Ok(())
    }

    /// The next `n` bits, `n` at most 16, as a number: Truncated when the
    /// input has fewer.
    fn take(&mut self, n: u32) -> Result<u32, Stop<I::Error>> {
        self.fill(n)?;
        if self.count < n {
            return Err(Stop::Fault(Fault::Truncated));
        }
        let v = (self.hold & ((1u64 << n) - 1)) as u32;
        self.drop(n);
        Ok(v)
    }

    fn drop(&mut self, n: u32) {
        self.hold >>= n;
        self.count -= n;
    }

    /// On to the next byte's start, where a stored block's lengths are.
    fn align(&mut self) {
        self.drop(self.count % 8);
    }

    /// A whole byte, once aligned: from what is held, then from the input.
    fn byte(&mut self) -> Result<u8, Stop<I::Error>> {
        if self.count >= 8 {
            let b = self.hold as u8;
            self.drop(8);
            return Ok(b);
        }
        match self.input.byte().map_err(Stop::Read)? {
            Some(b) => Ok(b),
            None => Err(Stop::Fault(Fault::Truncated)),
        }
    }
}

/// Which alphabet a code is of: what zlib's inftrees.c lets each have.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    CodeLengths,
    LitLens,
    Dists,
}

/// A canonical Huffman code, as a list of code lengths makes one.
struct Code {
    kind: Kind,
    /// How many codes there are of each length, 1 to 15.
    count: [u16; MAX_BITS + 1],
    /// The symbols, by length and then by value: the order their codes run
    /// in.
    symbol: [u16; LITLEN_SYMBOLS],
    /// The longest code's length; 0 for a code with none.
    max: u32,
    /// By the next FAST_BITS bits of the stream: the symbol whose code of
    /// at most that many bits starts there, and that code's length --
    /// `symbol << 4 | length` -- or 0 where no such code does.
    fast: [u16; FAST],
}

impl Code {
    const fn new() -> Code {
        Code { kind: Kind::LitLens, count: [0; MAX_BITS + 1], symbol: [0; LITLEN_SYMBOLS], max: 0, fast: [0; FAST] }
    }

    /// The code `lengths` describe -- a length of 0 is a symbol with no
    /// code -- or false where zlib refuses it: lengths that ask for more
    /// codes than there are (over-subscribed), or fewer (incomplete) --
    /// which zlib lets a literal/length or distance code be only when it
    /// is a single code of one bit. A code with no lengths at all zlib
    /// takes, and fails on the first symbol read with it (`decode`).
    fn build(&mut self, lengths: &[u8], kind: Kind) -> bool {
        self.kind = kind;
        self.count = [0; MAX_BITS + 1];
        for &len in lengths {
            self.count[usize::from(len)] += 1;
        }
        self.count[0] = 0;
        self.max = (1..=MAX_BITS).rev().find(|&len| self.count[len] != 0).unwrap_or(0) as u32;
        self.fast = [0; FAST];
        if self.max == 0 {
            return true;
        }

        let mut left: i32 = 1;
        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= i32::from(self.count[len]);
            if left < 0 {
                return false;
            }
        }
        if left > 0 && (kind == Kind::CodeLengths || self.max != 1) {
            return false;
        }

        let mut next = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            next[len + 1] = next[len] + self.count[len];
        }
        for (symbol, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let at = &mut next[usize::from(len)];
                self.symbol[usize::from(*at)] = symbol as u16;
                *at += 1;
            }
        }

        /* The codes of at most FAST_BITS bits, as the stream has them: a
         * code's bits come most significant first, so its entries are at
         * its bits reversed, under every value of the bits after it. */
        let mut code: u32 = 0;
        let mut index = 0;
        for len in 1..=FAST_BITS.min(self.max) {
            for _ in 0..self.count[len as usize] {
                let entry = (self.symbol[index] << 4) | len as u16;
                index += 1;
                let mut at = (code.reverse_bits() >> (32 - len)) as usize;
                while at < FAST {
                    self.fast[at] = entry;
                    at += 1 << len;
                }
                code += 1;
            }
            code <<= 1;
        }
        true
    }

    /// The next symbol: None for a run of bits no code has -- an incomplete
    /// code's, or any at all of a code with none -- which zlib calls a
    /// fault once it holds a bit of it; and Truncated when the input ends
    /// inside a code. A code length code with no codes reads every length
    /// as 0, a bit each, as zlib's does.
    fn decode<I: Input>(&self, bits: &mut Bits<I>) -> Result<Option<u16>, Stop<I::Error>> {
        bits.fill(MAX_BITS as u32)?;
        if self.max == 0 {
            if bits.count == 0 {
                return Err(Stop::Fault(Fault::Truncated));
            }
            bits.drop(1);
            return Ok(if self.kind == Kind::CodeLengths { Some(0) } else { None });
        }

        let entry = self.fast[(bits.hold as usize) & (FAST - 1)];
        if entry != 0 {
            let len = u32::from(entry & 0xF);
            if len > bits.count {
                return Err(Stop::Fault(Fault::Truncated));
            }
            bits.drop(len);
            return Ok(Some(entry >> 4));
        }

        /* A longer code, or none: walked to a bit at a time, the codes of
         * each length the next run of numbers (`first` the first of them,
         * `index` where their symbols start). */
        let mut code: u32 = 0;
        let mut first: u32 = 0;
        let mut index: u32 = 0;
        for len in 1..=self.max {
            if len > bits.count {
                return Err(Stop::Fault(Fault::Truncated));
            }
            code |= ((bits.hold >> (len - 1)) & 1) as u32;
            let count = u32::from(self.count[len as usize]);
            if code < first + count {
                bits.drop(len);
                return Ok(Some(self.symbol[(index + code - first) as usize]));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        /* Only an incomplete code -- one code of one bit -- has bits no
         * code starts with; zlib's table marks the other bit so. */
        bits.drop(1);
        Ok(None)
    }
}

/// Where a block left the stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// At its end: on to the next block, unless it was the last.
    Ended,
    /// At a symbol with output, after the last byte there was room for: as
    /// far as zlib reads.
    Full,
}

/// Which code tables are built: the fixed ones are kept between blocks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tables {
    None,
    Fixed,
    Dynamic,
}

/// What a stream is decoded with: the window, the output on its way, and the
/// code tables, taken once and used for every stream.
pub struct Inflater {
    window: Vec<u8>,
    /// Where the next byte goes in the window.
    at: usize,
    out: Vec<u8>,
    produced: u64,
    limit: u64,
    tables: Tables,
    lengths: [u8; DYNAMIC_LITLENS + DYNAMIC_DISTS],
    litlen: Code,
    dist: Code,
    codelen: Code,
}

impl Inflater {
    /// One, or None out of memory.
    pub fn new() -> Option<Inflater> {
        let window = buffer(WINDOW)?;
        let mut out = Vec::new();
        out.try_reserve_exact(PIECE).ok()?;
        Some(Inflater {
            window,
            at: 0,
            out,
            produced: 0,
            limit: 0,
            tables: Tables::None,
            lengths: [0; DYNAMIC_LITLENS + DYNAMIC_DISTS],
            litlen: Code::new(),
            dist: Code::new(),
            codelen: Code::new(),
        })
    }

    /// The first `limit` bytes of the stream `input` holds, handed to
    /// `emit` a piece at a time and in order: Ok once they all are and the
    /// stream is good as zlib has it (above), or why not. Some of the
    /// output may have been handed on before a fault is found.
    pub fn inflate<I: Input>(&mut self, input: &mut I, limit: u64,
                             emit: &mut dyn FnMut(&[u8]) -> Result<(), I::Error>) -> Result<(), Stop<I::Error>> {
        self.at = 0;
        self.out.clear();
        self.produced = 0;
        self.limit = limit;
        let mut bits = Bits::new(input);
        match self.blocks(&mut bits, emit) {
            Ok(()) => {}
            /* Past the last byte there was room for, the input may end
             * anywhere: zlib stops there, its buffer full. */
            Err(Stop::Fault(Fault::Truncated)) if self.produced == self.limit => {}
            Err(e) => return Err(e),
        }
        if self.produced != self.limit {
            return Err(Stop::Fault(Fault::Short));
        }
        if !self.out.is_empty() {
            emit(&self.out).map_err(Stop::Write)?;
            self.out.clear();
        }
        Ok(())
    }

    /// Block after block, to the end of the last, or as far as zlib reads
    /// once the output is whole.
    fn blocks<I: Input>(&mut self, bits: &mut Bits<I>,
                        emit: &mut dyn FnMut(&[u8]) -> Result<(), I::Error>) -> Result<(), Stop<I::Error>> {
        loop {
            let header = bits.take(3)?;
            let last = header & 1 != 0;
            let flow = match header >> 1 {
                0 => self.stored(bits, emit)?,
                1 => {
                    self.fixed();
                    self.codes(bits, emit)?
                }
                2 => {
                    self.dynamic(bits)?;
                    self.codes(bits, emit)?
                }
                _ => return Err(Stop::Fault(Fault::BlockType)),
            };
            if flow == Flow::Full || last {
                return Ok(());
            }
        }
    }

    /// A stored block: its length, and that many bytes as they are.
    fn stored<I: Input>(&mut self, bits: &mut Bits<I>,
                        emit: &mut dyn FnMut(&[u8]) -> Result<(), I::Error>) -> Result<Flow, Stop<I::Error>> {
        bits.align();
        let len = bits.take(16)?;
        let nlen = bits.take(16)?;
        if len != !nlen & 0xFFFF {
            return Err(Stop::Fault(Fault::StoredLengths));
        }
        for _ in 0..len {
            if self.produced == self.limit {
                return Ok(Flow::Full);
            }
            let b = bits.byte()?;
            self.put(b, emit)?;
        }
        Ok(Flow::Ended)
    }

    /// The fixed code's tables, unless the last block's were them.
    fn fixed(&mut self) {
        if self.tables == Tables::Fixed {
            return;
        }
        let mut lengths = [0u8; LITLEN_SYMBOLS];
        let mut from = 0;
        for (to, len) in FIXED_LITLEN {
            lengths[from..to].fill(len);
            from = to;
        }
        /* Both complete codes: neither can be refused. */
        self.litlen.build(&lengths, Kind::LitLens);
        self.dist.build(&[FIXED_DIST_BITS; DIST_SYMBOLS], Kind::Dists);
        self.tables = Tables::Fixed;
    }

    /// A dynamic block's header: its codes' lengths, themselves coded, and
    /// the tables made of them -- each step as zlib takes it, its checks in
    /// zlib's order.
    fn dynamic<I: Input>(&mut self, bits: &mut Bits<I>) -> Result<(), Stop<I::Error>> {
        self.tables = Tables::Dynamic;
        let header = bits.take(14)?;
        let nlen = (header & 0x1F) as usize + MIN_LITLENS;
        let ndist = ((header >> 5) & 0x1F) as usize + MIN_DISTS;
        let ncode = ((header >> 10) & 0xF) as usize + MIN_CODELENS;
        if nlen > DYNAMIC_LITLENS || ndist > DYNAMIC_DISTS {
            return Err(Stop::Fault(Fault::TooManySymbols));
        }

        let mut codelens = [0u8; CODELEN_SYMBOLS];
        for &symbol in &CODELEN_ORDER[..ncode] {
            codelens[symbol] = bits.take(3)? as u8;
        }
        if !self.codelen.build(&codelens, Kind::CodeLengths) {
            return Err(Stop::Fault(Fault::CodeLengths));
        }

        let total = nlen + ndist;
        let mut have = 0;
        while have < total {
            /* A complete code, or one with none, which reads as 0s: never None. */
            let symbol = self.codelen.decode(bits)?.ok_or(Stop::Fault(Fault::CodeLengths))?;
            if symbol < REPEAT_PREVIOUS {
                self.lengths[have] = symbol as u8;
                have += 1;
                continue;
            }
            let (len, copy) = if symbol == REPEAT_PREVIOUS {
                let copy = 3 + bits.take(2)? as usize;
                if have == 0 {
                    return Err(Stop::Fault(Fault::Repeat));
                }
                (self.lengths[have - 1], copy)
            } else if symbol == REPEAT_ZERO {
                (0, 3 + bits.take(3)? as usize)
            } else {
                (0, 11 + bits.take(7)? as usize)
            };
            if have + copy > total {
                return Err(Stop::Fault(Fault::Repeat));
            }
            self.lengths[have..have + copy].fill(len);
            have += copy;
        }

        if self.lengths[usize::from(END_OF_BLOCK)] == 0 {
            return Err(Stop::Fault(Fault::NoEndOfBlock));
        }
        if !self.litlen.build(&self.lengths[..nlen], Kind::LitLens) {
            return Err(Stop::Fault(Fault::LitLens));
        }
        if !self.dist.build(&self.lengths[nlen..total], Kind::Dists) {
            return Err(Stop::Fault(Fault::Dists));
        }
        Ok(())
    }

    /// A block's codes, to its end: literals, and matches -- a length and a
    /// distance back into what came out before.
    fn codes<I: Input>(&mut self, bits: &mut Bits<I>,
                       emit: &mut dyn FnMut(&[u8]) -> Result<(), I::Error>) -> Result<Flow, Stop<I::Error>> {
        loop {
            /* Dry: the output is whole, and what zlib reads on to is read. */
            let full = self.produced == self.limit;
            let symbol = self.litlen.decode(bits)?.ok_or(Stop::Fault(Fault::LitLenCode))?;
            if symbol < END_OF_BLOCK {
                if full {
                    return Ok(Flow::Full);
                }
                self.put(symbol as u8, emit)?;
                continue;
            }
            if symbol == END_OF_BLOCK {
                return Ok(Flow::Ended);
            }
            let Some(&base) = LENGTH_BASE.get(usize::from(symbol - FIRST_LENGTH)) else {
                return Err(Stop::Fault(Fault::LitLenCode));
            };
            let i = usize::from(symbol - FIRST_LENGTH);
            let len = u32::from(base) + bits.take(u32::from(LENGTH_EXTRA[i]))?;

            let d = usize::from(self.dist.decode(bits)?.ok_or(Stop::Fault(Fault::DistCode))?);
            let Some(&base) = DIST_BASE.get(d) else {
                return Err(Stop::Fault(Fault::DistCode));
            };
            let dist = usize::from(base) + bits.take(u32::from(DIST_EXTRA[d]))? as usize;
            if full {
                return Ok(Flow::Full);
            }
            /* Never past the first byte: there is no dictionary before it.
             * And never past the window, which is as far as a distance
             * reaches. */
            if dist as u64 > self.produced {
                return Err(Stop::Fault(Fault::TooFarBack));
            }
            for _ in 0..len {
                if self.produced == self.limit {
                    return Ok(Flow::Full);
                }
                let b = self.window[(self.at + WINDOW - dist) & (WINDOW - 1)];
                self.put(b, emit)?;
            }
        }
    }

    /// One byte out: into the window, and onto the piece on its way, which
    /// goes once it is full.
    #[inline]
    fn put<E>(&mut self, b: u8, emit: &mut dyn FnMut(&[u8]) -> Result<(), E>) -> Result<(), Stop<E>> {
        self.window[self.at] = b;
        self.at = (self.at + 1) & (WINDOW - 1);
        /* Into the room taken at `new`: the piece goes at PIECE bytes. */
        self.out.push(b);
        self.produced += 1;
        if self.out.len() == PIECE {
            emit(&self.out).map_err(Stop::Write)?;
            self.out.clear();
        }
        Ok(())
    }
}
