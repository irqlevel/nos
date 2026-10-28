//! The journal replayed as the fuzzer reads JBD2 -- its own reading, from
//! the format, as its writer is its own: the log scanned from its start, a
//! transaction whole at its commit, revokes, the copies home. What `check`
//! does first to an image left needing recovery, as e2fsck does.

use std::collections::BTreeMap;

use crate::machine::disk::Media;

use super::jbd2::{be32, FLAG_ESCAPE, FLAG_LAST_TAG, FLAG_SAME_UUID, MAGIC};
use super::*;

/// What a replay did.
#[derive(Debug, Default)]
pub struct Replayed {
    pub transactions: u32,
    /// Home blocks written, in order.
    pub written: Vec<u64>,
    /// The journal's own blocks on the device: nobody else's.
    pub blocks: Vec<u64>,
}

/// An inode's bytes, through the descriptors of the superblock `s`.
pub fn inode_bytes(img: &Media, s: &[u8], n: u32) -> Option<Vec<u8>> {
    let bs = 1024u64 << rd32(s, sb::LOG_BLOCK_SIZE).min(2);
    let ipg = rd32(s, sb::INODES_PER_GROUP).max(1);
    let isz = rd16(s, sb::INODE_SIZE).max(128) as u64;
    let ds = if rd32(s, sb::FEATURE_INCOMPAT) & INCOMPAT_64BIT != 0 { rd16(s, sb::DESC_SIZE).max(32) as u64 } else { 32 };
    let group = ((n - 1) / ipg) as u64;
    let gdt = (rd32(s, sb::FIRST_DATA_BLOCK) as u64 + 1) * bs;
    let d = img.bytes(gdt + group * ds, ds as usize);
    let mut table = rd32(&d, gd::INODE_TABLE) as u64;
    if ds >= 64 {
        table |= (rd32(&d, gd::INODE_TABLE_HI) as u64) << 32;
    }
    let at = table.checked_mul(bs)?.checked_add(((n - 1) % ipg) as u64 * isz)?;
    Some(img.bytes(at, isz as usize))
}

/// The device blocks of the logical blocks `0..count` of the inode whose
/// `i_block` is `ib`: through its extent tree, each node checked for its
/// magic and depth -- None at the first that is wrong, or a hole.
pub fn map_all(img: &Media, ib: &[u8], extents: bool, bs: u64, count: u64, blocks: u64) -> Option<Vec<u64>> {
    let mut out = vec![0u64; count as usize];
    let mut seen = vec![false; count as usize];
    if !extents {
        /* A block map's direct and single indirect blocks: what the
         * judge's own images and mke2fs's journals need. */
        let ppb = bs / 4;
        for k in 0..count.min(12) {
            out[k as usize] = rd32(ib, 4 * k as usize) as u64;
        }
        if count > 12 {
            let ind = rd32(ib, 48) as u64;
            if ind == 0 || ind >= blocks {
                return None;
            }
            let b = img.bytes(ind * bs, bs as usize);
            for k in 12..count.min(12 + ppb) {
                out[k as usize] = rd32(&b, 4 * (k - 12) as usize) as u64;
            }
            if count > 12 + ppb {
                return None;
            }
        }
        return out.iter().all(|&b| b != 0 && b < blocks).then_some(out);
    }
    let mut stack: Vec<(Vec<u8>, Option<u16>)> = vec![(ib.to_vec(), None)];
    let mut nodes = 0;
    while let Some((node, want)) = stack.pop() {
        nodes += 1;
        if nodes > 100_000 || node.len() < 12 || rd16(&node, 0) != EXT_MAGIC {
            return None;
        }
        let (entries, max, depth) = (rd16(&node, 2) as usize, rd16(&node, 4) as usize, rd16(&node, 6));
        if entries > max || 12 + 12 * max > node.len() || depth > 5 || want.is_some_and(|w| w != depth) {
            return None;
        }
        for k in 0..entries {
            let e = &node[12 + 12 * k..24 + 12 * k];
            if depth == 0 {
                let first = rd32(e, 0) as u64;
                let raw = rd16(e, 4) as u64;
                let len = if raw > EXT_INIT_MAX_LEN as u64 { raw - EXT_INIT_MAX_LEN as u64 } else { raw };
                let start = ((rd16(e, 6) as u64) << 32) | rd32(e, 8) as u64;
                for j in 0..len {
                    let l = first + j;
                    if l < count {
                        out[l as usize] = start + j;
                        seen[l as usize] = true;
                    }
                }
            } else {
                let child = ((rd16(e, 8) as u64) << 32) | rd32(e, 4) as u64;
                if child >= blocks {
                    return None;
                }
                stack.push((img.bytes(child * bs, bs as usize), Some(depth - 1)));
            }
        }
    }
    (seen.iter().all(|&s| s) && out.iter().all(|&b| b < blocks)).then_some(out)
}

/// Replay `img`'s journal in place, if its superblock says it needs it:
/// what it did, or why it could not -- a journal the fuzzer cannot read
/// is a corrupt filesystem.
pub fn replay(img: &mut Media) -> Result<Replayed, String> {
    let s = img.bytes(sb::OFFSET, 1024);
    let mut done = Replayed::default();
    if rd32(&s, sb::FEATURE_COMPAT) & COMPAT_HAS_JOURNAL == 0 {
        return Ok(done);
    }
    let bs = 1024u64 << rd32(&s, sb::LOG_BLOCK_SIZE).min(2);
    let blocks = rd32(&s, sb::BLOCKS_COUNT_LO) as u64
        | if rd32(&s, sb::FEATURE_INCOMPAT) & INCOMPAT_64BIT != 0 { (rd32(&s, sb::BLOCKS_COUNT_HI) as u64) << 32 } else { 0 };
    let ji = rd32(&s, sb::JOURNAL_INUM);
    if ji == 0 || ji > rd32(&s, sb::INODES_COUNT) {
        return Err(format!("a journal inode {}", ji));
    }
    let i = inode_bytes(img, &s, ji).ok_or("the journal's inode outside the image")?;
    let len = rd32(&i, ino::SIZE) as u64 | (rd32(&i, ino::SIZE_HIGH) as u64) << 32;
    let count = len / bs;
    if !(1024..=1 << 24).contains(&count) {
        return Err(format!("a journal of {} blocks", count));
    }
    let extents = rd32(&i, ino::FLAGS) & ino::EXTENTS_FL != 0;
    let map = map_all(img, &i[ino::BLOCK..ino::BLOCK + 60], extents, bs, count, blocks)
        .ok_or("the journal's blocks do not map")?;
    done.blocks = map.clone();
    let read = |img: &Media, jb: u32| img.bytes(map[jb as usize] * bs, bs as usize);
    let j = read(img, 0);
    if be32(&j, 0) != MAGIC || !(3..=4).contains(&be32(&j, 4)) || be32(&j, 0x0C) as u64 != bs {
        return Err("no journal superblock".into());
    }
    let (maxlen, first, mut seq, start) = (be32(&j, 0x10), be32(&j, 0x14), be32(&j, 0x18), be32(&j, 0x1C));
    let v2 = be32(&j, 4) == 4;
    let incompat = if v2 { be32(&j, 0x28) } else { 0 };
    let csum = incompat & jbd2::INCOMPAT_CSUM_V3 != 0;
    let bit64 = incompat & jbd2::INCOMPAT_64BIT != 0;
    if incompat & !(jbd2::INCOMPAT_REVOKE | jbd2::INCOMPAT_64BIT | jbd2::INCOMPAT_CSUM_V3) != 0
        || (v2 && be32(&j, 0x24) != 0)
    {
        return Err(format!("journal features {:#x} the fuzzer does not read", incompat));
    }
    if maxlen as u64 > count || first == 0 || first >= maxlen {
        return Err(format!("a journal ring of {}..{} in {} blocks", first, maxlen, count));
    }
    let seed = crc32c(!0, &j[0x30..0x40]);
    if csum {
        let mut z = j[..1024].to_vec();
        z[0xFC..0x100].fill(0);
        if crc32c(!0, &z) != be32(&j, 0xFC) {
            return Err("the journal's superblock fails its checksum".into());
        }
    }
    let needs = rd32(&s, sb::FEATURE_INCOMPAT) & INCOMPAT_RECOVER != 0;
    if start == 0 || !needs {
        return Ok(done);
    }
    if start < first || start >= maxlen {
        return Err(format!("a log starting at {} outside its ring", start));
    }
    let tag_bytes = if csum { 16 } else if bit64 { 12 } else { 8 };
    let tail_ok = |b: &[u8]| {
        let mut z = b.to_vec();
        let at = z.len() - 4;
        z[at..].fill(0);
        crc32c(seed, &z) == be32(b, at)
    };
    /* The scan: (sequence, copies (home, journal block, flags, checksum),
     * revokes), each transaction whole at its commit. */
    struct Txn {
        seq: u32,
        copies: Vec<(u64, u32, u32, u32)>,
        revokes: Vec<u64>,
        bad: bool,
    }
    let ring = maxlen - first;
    let next = |b: u32| if b + 1 >= maxlen { first } else { b + 1 };
    let (mut b, mut scanned, mut last_time) = (start, 0u32, 0u64);
    let mut whole: Vec<Txn> = Vec::new();
    let mut cur = Txn { seq, copies: Vec::new(), revokes: Vec::new(), bad: false };
    'scan: while scanned < ring {
        let blk = read(img, b);
        scanned += 1;
        if be32(&blk, 0) != MAGIC || be32(&blk, 8) != seq {
            break;
        }
        match be32(&blk, 4) {
            jbd2::DESCRIPTOR => {
                if csum && !tail_ok(&blk) {
                    cur.bad = true;
                }
                let end = bs as usize - if csum { 4 } else { 0 };
                let mut at = 12;
                while at + tag_bytes <= end {
                    let lo = be32(&blk, at) as u64;
                    let (flags, hi, sum) = if csum {
                        (be32(&blk, at + 4), be32(&blk, at + 8) as u64, be32(&blk, at + 12))
                    } else {
                        (u16::from_be_bytes([blk[at + 6], blk[at + 7]]) as u32,
                         if bit64 { be32(&blk, at + 8) as u64 } else { 0 }, 0)
                    };
                    b = next(b);
                    if scanned >= ring {
                        break 'scan;
                    }
                    scanned += 1;
                    cur.copies.push((lo | hi << 32, b, flags, sum));
                    at += tag_bytes + if flags & FLAG_SAME_UUID == 0 { 16 } else { 0 };
                    if flags & FLAG_LAST_TAG != 0 {
                        break;
                    }
                }
            }
            jbd2::REVOKE => {
                if csum && !tail_ok(&blk) {
                    cur.bad = true;
                } else {
                    let used = be32(&blk, 12) as usize;
                    let width = if bit64 { 8 } else { 4 };
                    if !(16..=bs as usize).contains(&used) {
                        cur.bad = true;
                    } else {
                        let mut at = 16;
                        while at + width <= used {
                            cur.revokes.push(if bit64 { rd64_be(&blk, at) } else { be32(&blk, at) as u64 });
                            at += width;
                        }
                    }
                }
            }
            jbd2::COMMIT => {
                let time = rd64_be(&blk, 48);
                if csum {
                    let mut z = blk.clone();
                    z[12..14].fill(0);
                    z[16..20].fill(0);
                    if crc32c(seed, &z) != be32(&blk, 16) {
                        break;
                    }
                }
                if cur.bad {
                    if time < last_time {
                        break;
                    }
                    return Err(format!("journal transaction {} damaged under a sound commit", seq));
                }
                last_time = time;
                seq = seq.wrapping_add(1);
                whole.push(std::mem::replace(&mut cur, Txn { seq, copies: Vec::new(), revokes: Vec::new(), bad: false }));
            }
            _ => break,
        }
        b = next(b);
    }
    let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
    for t in &whole {
        for &r in &t.revokes {
            revoked.insert(r, t.seq);
        }
    }
    for t in &whole {
        for &(home, jb, flags, sum) in &t.copies {
            if revoked.get(&home).is_some_and(|&r| (t.seq.wrapping_sub(r) as i32) <= 0) {
                continue;
            }
            let mut data = read(img, jb);
            if csum && crc32c(crc32c(seed, &t.seq.to_be_bytes()), &data) != sum {
                return Err(format!("journal transaction {}: a copy fails its checksum", t.seq));
            }
            if flags & FLAG_ESCAPE != 0 {
                data[..4].copy_from_slice(&MAGIC.to_be_bytes());
            }
            if home >= blocks {
                return Err(format!("a journal copy for block {}, past the filesystem", home));
            }
            img.write(home * bs, &data);
            done.written.push(home);
        }
    }
    done.transactions = whole.len() as u32;
    /* The journal empty after it, at the next sequence; the filesystem no
     * longer needing recovery -- its superblock as the replay left it. */
    let mut j = read(img, 0);
    jbd2::wbe32(&mut j, 0x18, seq.wrapping_add(1));
    jbd2::wbe32(&mut j, 0x1C, 0);
    if csum {
        j[0xFC..0x100].fill(0);
        let c = crc32c(!0, &j[..1024]);
        jbd2::wbe32(&mut j, 0xFC, c);
    }
    img.write(map[0] * bs, &j);
    let mut s = img.bytes(sb::OFFSET, 1024);
    let features = rd32(&s, sb::FEATURE_INCOMPAT) & !INCOMPAT_RECOVER;
    wr32(&mut s, sb::FEATURE_INCOMPAT, features);
    let c = crc32c(!0, &s[..sb::CHECKSUM]);
    wr32(&mut s, sb::CHECKSUM, c);
    img.write(sb::OFFSET, &s);
    Ok(done)
}

fn rd64_be(b: &[u8], at: usize) -> u64 {
    (be32(b, at) as u64) << 32 | be32(b, at + 4) as u64
}
