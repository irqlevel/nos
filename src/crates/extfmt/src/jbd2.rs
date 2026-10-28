//! The journal's own format, JBD2's -- big-endian, unlike the filesystem's.
//! The journal is a ring of blocks after its superblock (journal block 0):
//! each transaction a run of descriptor blocks, each naming the home blocks
//! of the copies after it, revoke blocks naming home blocks whose older
//! copies are not to be replayed, and a commit block that makes it whole.
//! With CSUM_V3 every one of them carries a CRC32C seeded by the journal's
//! UUID, and each copy's checksum is in the tag that names it.

use alloc::string::String;
use alloc::vec::Vec;

use crate::crc::crc32c;

pub const MAGIC: u32 = 0xC03B_3998;
/// A block's header: magic, type, sequence.
pub const HEADER: usize = 12;

/* `h_blocktype`. */
pub const DESCRIPTOR_BLOCK: u32 = 1;
pub const COMMIT_BLOCK: u32 = 2;
pub const SUPERBLOCK_V1: u32 = 3;
pub const SUPERBLOCK_V2: u32 = 4;
pub const REVOKE_BLOCK: u32 = 5;

/* Features: compat, incompat. */
pub const COMPAT_CHECKSUM: u32 = 0x1;
pub const INCOMPAT_REVOKE: u32 = 0x1;
pub const INCOMPAT_64BIT: u32 = 0x2;
pub const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
pub const INCOMPAT_CSUM_V2: u32 = 0x8;
pub const INCOMPAT_CSUM_V3: u32 = 0x10;
pub const INCOMPAT_FAST_COMMIT: u32 = 0x20;
/// `s_checksum_type` for CRC32C.
pub const CHECKSUM_CRC32C: u8 = 4;
/// The blocks fast commits keep at the ring's end when the superblock does
/// not say (`JBD2_DEFAULT_FAST_COMMIT_BLOCKS`).
pub const DEFAULT_FAST_COMMIT_BLOCKS: u32 = 256;

/* A tag's flags. */
pub const FLAG_ESCAPE: u32 = 0x1;
pub const FLAG_SAME_UUID: u32 = 0x2;
pub const FLAG_DELETED: u32 = 0x4;
pub const FLAG_LAST_TAG: u32 = 0x8;
/// The UUID after a tag without SAME_UUID.
pub const TAG_UUID: usize = 16;
/// The checksum at the end of a descriptor or revoke block (v2, v3).
pub const BLOCK_TAIL: usize = 4;

/* The superblock, as journal_superblock_t has it. */
pub const SB_SIZE: usize = 1024;
pub const S_BLOCKSIZE: usize = 0x0C;
pub const S_MAXLEN: usize = 0x10;
pub const S_FIRST: usize = 0x14;
pub const S_SEQUENCE: usize = 0x18;
pub const S_START: usize = 0x1C;
pub const S_ERRNO: usize = 0x20;
pub const S_FEATURE_COMPAT: usize = 0x24;
pub const S_FEATURE_INCOMPAT: usize = 0x28;
pub const S_FEATURE_RO_COMPAT: usize = 0x2C;
pub const S_UUID: usize = 0x30;
pub const S_NR_USERS: usize = 0x40;
pub const S_MAX_TRANSACTION: usize = 0x48;
pub const S_CHECKSUM_TYPE: usize = 0x50;
pub const S_NUM_FC_BLKS: usize = 0x54;
pub const S_HEAD: usize = 0x58;
pub const S_CHECKSUM: usize = 0xFC;

/* A commit block, as commit_header has it. */
pub const H_CHKSUM_TYPE: usize = 12;
pub const H_CHKSUM_SIZE: usize = 13;
pub const H_CHKSUM: usize = 16;
pub const H_COMMIT_SEC: usize = 48;
pub const H_COMMIT_NSEC: usize = 56;
/* A revoke block: the header, then the bytes used, header included. */
pub const R_COUNT: usize = 12;
pub const REVOKE_HEADER: usize = 16;

#[inline]
pub fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[inline]
pub fn be64(b: &[u8], at: usize) -> u64 {
    (u64::from(be32(b, at)) << 32) | u64::from(be32(b, at + 4))
}

#[inline]
pub fn set_be32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn set_be64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// A block's header, if it has the magic: its type and sequence.
pub fn header(block: &[u8]) -> Option<(u32, u32)> {
    (block.len() >= HEADER && be32(block, 0) == MAGIC).then(|| (be32(block, 4), be32(block, 8)))
}

/// The journal's superblock, as read: a copy of its first 1024 bytes,
/// checked enough that the ring it describes lies inside the journal.
#[derive(Clone)]
pub struct JournalSuper {
    raw: [u8; SB_SIZE],
}

impl JournalSuper {
    /// The superblock in `block`, journal block 0 -- the magic, a version,
    /// a ring of at least two blocks after it and a start inside the ring
    /// -- or why not. `block` is at least 1024 bytes.
    pub fn parse(block: &[u8]) -> Result<JournalSuper, String> {
        if block.len() < SB_SIZE {
            return Err(alloc::format!("{} bytes of a journal superblock of {}", block.len(), SB_SIZE));
        }
        let mut raw = [0u8; SB_SIZE];
        raw.copy_from_slice(&block[..SB_SIZE]);
        let sb = JournalSuper { raw };
        match header(&sb.raw) {
            Some((SUPERBLOCK_V1 | SUPERBLOCK_V2, _)) => {}
            Some((t, _)) => return Err(alloc::format!("a journal superblock of type {}", t)),
            None => return Err(String::from("no journal superblock")),
        }
        let (first, last) = (sb.first(), sb.last());
        if first == 0 || first >= last {
            return Err(alloc::format!("a journal ring of blocks {}..{}", first, last));
        }
        if sb.start() != 0 && (sb.start() < first || sb.start() >= last) {
            return Err(alloc::format!("a journal starting at {}, outside its ring {}..{}", sb.start(), first, last));
        }
        Ok(sb)
    }

    pub fn raw(&self) -> &[u8; SB_SIZE] {
        &self.raw
    }
    pub fn version(&self) -> u32 {
        be32(&self.raw, 4)
    }
    pub fn blocksize(&self) -> u32 {
        be32(&self.raw, S_BLOCKSIZE)
    }
    /// Blocks in the journal, its superblock included.
    pub fn maxlen(&self) -> u32 {
        be32(&self.raw, S_MAXLEN)
    }
    /// The ring's first block.
    pub fn first(&self) -> u32 {
        be32(&self.raw, S_FIRST)
    }
    /// The sequence of the first transaction the log holds.
    pub fn sequence(&self) -> u32 {
        be32(&self.raw, S_SEQUENCE)
    }
    /// Where the log starts: 0 for an empty journal, nothing to replay.
    pub fn start(&self) -> u32 {
        be32(&self.raw, S_START)
    }
    pub fn errno(&self) -> u32 {
        be32(&self.raw, S_ERRNO)
    }
    /// The feature words: only a v2 superblock has them.
    pub fn compat(&self) -> u32 {
        if self.version() == SUPERBLOCK_V2 { be32(&self.raw, S_FEATURE_COMPAT) } else { 0 }
    }
    pub fn incompat(&self) -> u32 {
        if self.version() == SUPERBLOCK_V2 { be32(&self.raw, S_FEATURE_INCOMPAT) } else { 0 }
    }
    pub fn ro_compat(&self) -> u32 {
        if self.version() == SUPERBLOCK_V2 { be32(&self.raw, S_FEATURE_RO_COMPAT) } else { 0 }
    }
    pub fn uuid(&self) -> [u8; 16] {
        let mut u = [0u8; 16];
        u.copy_from_slice(&self.raw[S_UUID..S_UUID + 16]);
        u
    }
    pub fn nr_users(&self) -> u32 {
        be32(&self.raw, S_NR_USERS)
    }
    pub fn checksum_type(&self) -> u8 {
        self.raw[S_CHECKSUM_TYPE]
    }
    pub fn num_fc_blks(&self) -> u32 {
        be32(&self.raw, S_NUM_FC_BLKS)
    }
    pub fn checksum(&self) -> u32 {
        be32(&self.raw, S_CHECKSUM)
    }
    pub fn is_64bit(&self) -> bool {
        self.incompat() & INCOMPAT_64BIT != 0
    }
    pub fn csum_v3(&self) -> bool {
        self.incompat() & INCOMPAT_CSUM_V3 != 0
    }
    /// Whether its blocks carry v2's or v3's checksums.
    pub fn csum_v2or3(&self) -> bool {
        self.incompat() & (INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3) != 0
    }

    /// The end of the ring: the journal's last block past it, less the
    /// fast commits' blocks at its end.
    pub fn last(&self) -> u32 {
        let fc = if self.incompat() & INCOMPAT_FAST_COMMIT != 0 {
            if self.num_fc_blks() == 0 { DEFAULT_FAST_COMMIT_BLOCKS } else { self.num_fc_blks() }
        } else {
            0
        };
        self.maxlen().saturating_sub(fc)
    }

    /// The bytes a descriptor's tag takes (`journal_tag_bytes`): v3's 16;
    /// else 8, with the high word of the block for 64bit, and v2's 16 bits
    /// of checksum.
    pub fn tag_bytes(&self) -> usize {
        if self.csum_v3() {
            return 16;
        }
        let mut n = 12;
        if self.incompat() & INCOMPAT_CSUM_V2 != 0 {
            n += 2;
        }
        if self.is_64bit() { n } else { n - 4 }
    }

    /// What every checksum of the journal starts from: its UUID's CRC32C.
    pub fn csum_seed(&self) -> u32 {
        crc32c(!0, &self.raw[S_UUID..S_UUID + 16])
    }

    /// The superblock's checksum, as it should be: over all of it, the
    /// checksum field taken as zero.
    pub fn compute_checksum(&self) -> u32 {
        let mut c = crc32c(!0, &self.raw[..S_CHECKSUM]);
        c = crc32c(c, &[0, 0, 0, 0]);
        crc32c(c, &self.raw[S_CHECKSUM + 4..])
    }

    /// Whether the checksum it carries is right -- true for a journal with
    /// no checksums.
    pub fn checksum_ok(&self) -> bool {
        !self.csum_v2or3() || self.checksum() == self.compute_checksum()
    }

    pub fn set_start(&mut self, v: u32) {
        set_be32(&mut self.raw, S_START, v);
    }
    pub fn set_sequence(&mut self, v: u32) {
        set_be32(&mut self.raw, S_SEQUENCE, v);
    }
    pub fn set_incompat(&mut self, v: u32) {
        set_be32(&mut self.raw, S_FEATURE_INCOMPAT, v);
    }
    pub fn set_checksum_type(&mut self, v: u8) {
        self.raw[S_CHECKSUM_TYPE] = v;
    }
    pub fn set_checksum(&mut self) {
        if self.csum_v2or3() {
            let c = self.compute_checksum();
            set_be32(&mut self.raw, S_CHECKSUM, c);
        }
    }
}

/// A descriptor's tag: the home block of the copy it names, and how it
/// was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    pub block: u64,
    pub flags: u32,
    /// The copy's checksum: v3's 32 bits, v2's 16, else 0.
    pub checksum: u32,
}

impl Tag {
    /// The copy's first word was the magic, written as zero.
    pub fn escaped(&self) -> bool {
        self.flags & FLAG_ESCAPE != 0
    }
}

/// The tags of descriptor block `block`, in order, up to the one flagged
/// last or to the end of the room tags have -- a block's length less its
/// tail, with v2's and v3's checksums. A tag that would run past it ends
/// them: the blocks after the descriptor are those its tags count.
pub fn tags(block: &[u8], sb: &JournalSuper) -> Vec<Tag> {
    let tag_bytes = sb.tag_bytes();
    let end = block.len().saturating_sub(if sb.csum_v2or3() { BLOCK_TAIL } else { 0 });
    let mut out = Vec::new();
    let mut at = HEADER;
    while at + tag_bytes <= end {
        let lo = u64::from(be32(block, at));
        let (flags, high, checksum) = if sb.csum_v3() {
            (be32(block, at + 4), be32(block, at + 8), be32(block, at + 12))
        } else {
            let flags = u32::from(u16::from_be_bytes([block[at + 6], block[at + 7]]));
            let checksum = u32::from(u16::from_be_bytes([block[at + 4], block[at + 5]]));
            let high = if sb.is_64bit() { be32(block, at + 8) } else { 0 };
            (flags, high, checksum)
        };
        let blockno = if sb.is_64bit() { lo | (u64::from(high) << 32) } else { lo };
        out.push(Tag { block: blockno, flags, checksum });
        at += tag_bytes;
        if flags & FLAG_SAME_UUID == 0 {
            at += TAG_UUID;
        }
        if flags & FLAG_LAST_TAG != 0 {
            break;
        }
    }
    out
}

/// The checksum a descriptor or revoke block's tail should carry: from
/// the seed over the block, the tail taken as zero.
pub fn block_tail_csum(seed: u32, block: &[u8]) -> Option<u32> {
    let at = block.len().checked_sub(BLOCK_TAIL)?;
    Some(crc32c(crc32c(seed, &block[..at]), &[0, 0, 0, 0]))
}

/// Whether a descriptor or revoke block carries the checksum it should.
pub fn block_tail_ok(seed: u32, block: &[u8]) -> bool {
    let Some(at) = block.len().checked_sub(BLOCK_TAIL) else { return false };
    block_tail_csum(seed, block) == Some(be32(block, at))
}

/// The checksum a commit block should carry in its first checksum word:
/// from the seed over the block, the checksum's type, size and first word
/// taken as zero.
pub fn commit_csum(seed: u32, block: &[u8]) -> Option<u32> {
    if block.len() < H_COMMIT_NSEC + 4 {
        return None;
    }
    let mut c = crc32c(seed, &block[..H_CHKSUM_TYPE]);
    c = crc32c(c, &[0, 0]);
    c = crc32c(c, &block[H_CHKSUM_TYPE + 2..H_CHKSUM]);
    c = crc32c(c, &[0, 0, 0, 0]);
    Some(crc32c(c, &block[H_CHKSUM + 4..]))
}

pub fn commit_ok(seed: u32, block: &[u8]) -> bool {
    commit_csum(seed, block).is_some_and(|c| c == be32(block, H_CHKSUM))
}

/// When a commit block says its transaction was committed, in seconds.
pub fn commit_time(block: &[u8]) -> u64 {
    if block.len() >= H_COMMIT_SEC + 8 { be64(block, H_COMMIT_SEC) } else { 0 }
}

/// A copy's checksum as its tag should carry it: from the seed over the
/// transaction's sequence, then the copy as it is in the journal --
/// escaped, if it was.
pub fn copy_csum(seed: u32, sequence: u32, data: &[u8]) -> u32 {
    crc32c(crc32c(seed, &sequence.to_be_bytes()), data)
}

/// Whether a tag's checksum is the copy's: all 32 bits with v3, the low 16
/// with v2, and nothing to check without either.
pub fn copy_ok(sb: &JournalSuper, seed: u32, sequence: u32, tag: &Tag, data: &[u8]) -> bool {
    let c = copy_csum(seed, sequence, data);
    if sb.csum_v3() {
        c == tag.checksum
    } else if sb.incompat() & INCOMPAT_CSUM_V2 != 0 {
        c & 0xFFFF == tag.checksum
    } else {
        true
    }
}

/// The home blocks revoke block `block` names: its records, 8 bytes each
/// with 64bit and 4 without, up to the count its header gives -- held to
/// the block, less its tail with checksums.
pub fn revoke_records(block: &[u8], sb: &JournalSuper) -> Result<Vec<u64>, String> {
    if block.len() < REVOKE_HEADER {
        return Err(String::from("a revoke block too short for its header"));
    }
    let count = be32(block, R_COUNT) as usize;
    let room = block.len().saturating_sub(if sb.csum_v2or3() { BLOCK_TAIL } else { 0 });
    if count < REVOKE_HEADER || count > room {
        return Err(alloc::format!("a revoke block of {} bytes used in {}", count, room));
    }
    let width = if sb.is_64bit() { 8 } else { 4 };
    let mut out = Vec::new();
    let mut at = REVOKE_HEADER;
    while at + width <= count {
        out.push(if width == 8 { be64(block, at) } else { u64::from(be32(block, at)) });
        at += width;
    }
    Ok(out)
}
