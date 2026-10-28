//! The journal as the fuzzer writes one -- its own JBD2, from the format:
//! the superblock, and transactions of descriptor blocks naming the home
//! of each copy after them, revoke blocks and commit blocks, with v3's
//! checksums or none. What a driver must replay, or read through without
//! replaying on a read-only mount.

use crate::machine::disk::Media;

use super::crc32c;

pub const MAGIC: u32 = 0xC03B_3998;
pub const DESCRIPTOR: u32 = 1;
pub const COMMIT: u32 = 2;
pub const SUPERBLOCK_V2: u32 = 4;
pub const REVOKE: u32 = 5;
pub const INCOMPAT_REVOKE: u32 = 0x1;
pub const INCOMPAT_64BIT: u32 = 0x2;
pub const INCOMPAT_CSUM_V3: u32 = 0x10;
pub const FLAG_ESCAPE: u32 = 0x1;
pub const FLAG_SAME_UUID: u32 = 0x2;
pub const FLAG_LAST_TAG: u32 = 0x8;

pub fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn wbe32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

pub fn wbe64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// A journal being written: where its blocks are on the device, how it
/// writes them, and where the log has got to.
pub struct Journal {
    pub bs: usize,
    /// Each journal block's device block.
    pub map: Vec<u64>,
    pub csum: bool,
    pub bit64: bool,
    pub uuid: [u8; 16],
    /// The block the next one goes to, and the next transaction's sequence.
    pub pos: u32,
    pub seq: u32,
}

impl Journal {
    pub fn new(bs: usize, map: Vec<u64>, csum: bool, bit64: bool, uuid: [u8; 16], seq: u32) -> Journal {
        Journal { bs, map, csum, bit64, uuid, pos: 1, seq }
    }

    fn seed(&self) -> u32 {
        crc32c(!0, &self.uuid)
    }

    fn incompat(&self) -> u32 {
        let mut f = INCOMPAT_REVOKE;
        if self.bit64 {
            f |= INCOMPAT_64BIT;
        }
        if self.csum {
            f |= INCOMPAT_CSUM_V3;
        }
        f
    }

    fn tag_bytes(&self) -> usize {
        if self.csum { 16 } else if self.bit64 { 12 } else { 8 }
    }

    /// The journal's superblock: a log of `maxlen` blocks from block 1,
    /// starting at `start` (0 for empty) with sequence `seq`.
    pub fn superblock(&self, start: u32, seq: u32) -> Vec<u8> {
        let mut b = vec![0u8; self.bs];
        wbe32(&mut b, 0, MAGIC);
        wbe32(&mut b, 4, SUPERBLOCK_V2);
        wbe32(&mut b, 0x0C, self.bs as u32);
        wbe32(&mut b, 0x10, self.map.len() as u32);
        wbe32(&mut b, 0x14, 1);
        wbe32(&mut b, 0x18, seq);
        wbe32(&mut b, 0x1C, start);
        wbe32(&mut b, 0x28, self.incompat());
        b[0x30..0x40].copy_from_slice(&self.uuid);
        wbe32(&mut b, 0x40, 1);
        b[0x100..0x110].copy_from_slice(&self.uuid);
        if self.csum {
            b[0x50] = 4;
            let c = crc32c(!0, &b[..1024]);
            wbe32(&mut b, 0xFC, c);
        }
        b
    }

    fn put(&mut self, media: &mut Media, data: &[u8]) {
        let dev = self.map[self.pos as usize];
        media.write(dev * self.bs as u64, data);
        self.pos += 1;
    }

    fn header(&self, kind: u32, seq: u32) -> Vec<u8> {
        let mut b = vec![0u8; self.bs];
        wbe32(&mut b, 0, MAGIC);
        wbe32(&mut b, 4, kind);
        wbe32(&mut b, 8, seq);
        b
    }

    fn tail(&self, b: &mut [u8]) {
        if self.csum {
            let at = self.bs - 4;
            b[at..].fill(0);
            let c = crc32c(self.seed(), b);
            wbe32(b, at, c);
        }
    }

    /// One transaction, of sequence `self.seq`: its copies after the
    /// descriptors naming them -- a copy whose first word is the magic
    /// written escaped, as zero -- its revokes, and a commit committed at
    /// `time` unless `commit` is false. The blocks go where the log is.
    pub fn transaction(&mut self, media: &mut Media, copies: &[(u64, Vec<u8>)], revokes: &[u64], commit: bool,
                       time: u64) {
        let seq = self.seq;
        let tb = self.tag_bytes();
        let room = self.bs - if self.csum { 4 } else { 0 };
        let mut k = 0;
        while k < copies.len() {
            /* As many tags as the block holds: the first followed by the
             * UUID, the rest the same's. */
            let mut d = self.header(DESCRIPTOR, seq);
            let mut at = 12;
            let start = k;
            let mut ends: Vec<usize> = Vec::new();
            while k < copies.len() {
                let need = tb + if k == start { 16 } else { 0 };
                if at + need > room {
                    break;
                }
                ends.push(at);
                at += need;
                k += 1;
            }
            let mut escaped: Vec<Vec<u8>> = Vec::new();
            for (n, (home, data)) in copies[start..k].iter().enumerate() {
                let tag = ends[n];
                let mut copy = data.clone();
                let mut flags = if n == 0 { 0 } else { FLAG_SAME_UUID };
                if be32(&copy, 0) == MAGIC {
                    copy[..4].fill(0);
                    flags |= FLAG_ESCAPE;
                }
                if start + n + 1 == k {
                    flags |= FLAG_LAST_TAG;
                }
                wbe32(&mut d, tag, *home as u32);
                if self.csum {
                    wbe32(&mut d, tag + 4, flags);
                    wbe32(&mut d, tag + 8, (*home >> 32) as u32);
                    let c = crc32c(crc32c(self.seed(), &seq.to_be_bytes()), &copy);
                    wbe32(&mut d, tag + 12, c);
                } else {
                    d[tag + 6..tag + 8].copy_from_slice(&(flags as u16).to_be_bytes());
                    if self.bit64 {
                        wbe32(&mut d, tag + 8, (*home >> 32) as u32);
                    }
                }
                if n == 0 {
                    d[tag + tb..tag + tb + 16].copy_from_slice(&self.uuid);
                }
                escaped.push(copy);
            }
            self.tail(&mut d);
            self.put(media, &d);
            for copy in escaped {
                self.put(media, &copy);
            }
        }
        if !revokes.is_empty() {
            let width = if self.bit64 { 8 } else { 4 };
            let mut r = self.header(REVOKE, seq);
            let mut at = 16;
            for &b in revokes {
                if self.bit64 {
                    wbe64(&mut r, at, b);
                } else {
                    wbe32(&mut r, at, b as u32);
                }
                at += width;
            }
            wbe32(&mut r, 12, at as u32);
            self.tail(&mut r);
            self.put(media, &r);
        }
        if commit {
            let mut c = self.header(COMMIT, seq);
            wbe64(&mut c, 48, time);
            if self.csum {
                let sum = crc32c(self.seed(), &c);
                wbe32(&mut c, 16, sum);
            }
            self.put(media, &c);
        }
        self.seq = self.seq.wrapping_add(1);
    }
}
