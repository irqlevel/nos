//! The journal: replayed by extfmt's planner, for the gate to hold to
//! e2fsck's replay of the same image, and the planner fuzzed over journals
//! damaged at random.

use extfmt::features;
use extfmt::group::Desc;
use extfmt::inode::{Inode, EXTENTS_FL};
use extfmt::jbd2::{self, JournalSuper};
use extfmt::recovery::{self, Plan};
use extfmt::sb::Super;
use extfmt::{blockmap, extent, Map};

use crate::{get, Rng, ITERATION};

/// A filesystem's journal: where each of its blocks is on the device.
pub struct Journal {
    pub bs: usize,
    pub map: Vec<u64>,
}

impl Journal {
    /// Journal block `jb`'s bytes in the image.
    pub fn block<'a>(&self, img: &'a [u8], jb: u32) -> Option<&'a [u8]> {
        let dev = *self.map.get(jb as usize)?;
        get(img, dev.checked_mul(self.bs as u64)?, self.bs)
    }
}

/// Where inode `ino`'s bytes are in the image.
pub fn inode_offset(img: &[u8], sb: &Super, ino: u32) -> Result<u64, String> {
    if ino == 0 || ino > sb.inodes_count() {
        return Err(format!("inode {} of {}", ino, sb.inodes_count()));
    }
    let bs = sb.block_size();
    let (ipg, ds) = (u64::from(sb.inodes_per_group()), sb.desc_size());
    let (group, index) = (u64::from(ino - 1) / ipg, u64::from(ino - 1) % ipg);
    let at = (u64::from(sb.first_data_block()) + 1) * bs + group * ds;
    let desc = get(img, at, ds as usize).and_then(Desc::new)
        .ok_or_else(|| format!("group {}'s descriptor outside the image", group))?;
    desc.inode_table().checked_mul(bs).and_then(|t| t.checked_add(index * sb.inode_size()))
        .ok_or_else(|| format!("group {}'s inode table at {}", group, desc.inode_table()))
}

/// The filesystem's superblock and journal: every block of the journal
/// inode's size mapped, none a hole -- a journal is written whole.
pub fn journal(img: &[u8]) -> Result<(Super, Journal), String> {
    let sb = Super::parse(get(img, extfmt::sb::OFFSET, extfmt::sb::SIZE).ok_or("no superblock")?)?;
    if sb.compat() & features::compat::HAS_JOURNAL == 0 || sb.journal_inum() == 0 {
        return Err(String::from("no journal in the filesystem"));
    }
    let bs = sb.block_size() as usize;
    let at = inode_offset(img, &sb, sb.journal_inum())?;
    let raw = get(img, at, sb.inode_size() as usize).ok_or("the journal's inode outside the image")?;
    let node = Inode::new(raw).ok_or("a short inode")?;
    let blocks = node.size(true).div_ceil(bs as u64);
    if blocks > u64::from(u32::MAX) {
        return Err(format!("a journal of {} blocks", blocks));
    }
    let fs_blocks = sb.blocks_count();
    let mut read = |b: u64, buf: &mut [u8]| {
        if b >= fs_blocks {
            return Err(format!("block {} outside the filesystem", b));
        }
        match get(img, b * bs as u64, bs) {
            Some(src) => {
                buf.copy_from_slice(src);
                Ok(())
            }
            None => Err(format!("block {} outside the image", b)),
        }
    };
    let mut map = Vec::new();
    let mut l = 0u64;
    while l < blocks {
        let m = if node.flags() & EXTENTS_FL != 0 {
            extent::map(node.i_block(), l, fs_blocks, bs, sb.journal_inum(), &mut read)?
        } else {
            blockmap::map(node.i_block(), l, bs, sb.journal_inum(), &mut read)?
        };
        match m {
            Map::Blocks { block, run } => {
                for k in 0..run.min(blocks - l) {
                    map.push(block + k);
                }
                l += run;
            }
            Map::Hole { .. } => return Err(format!("a hole in the journal at its block {}", l)),
        }
    }
    Ok((sb, Journal { bs, map }))
}

/// The journal's superblock and the plan of replaying it.
pub fn plan_of(img: &[u8]) -> Result<(Super, Journal, JournalSuper, Plan), String> {
    let (sb, j) = journal(img)?;
    let jsb = JournalSuper::parse(j.block(img, 0).ok_or("the journal's superblock outside the image")?)?;
    if jsb.maxlen() as usize > j.map.len() {
        return Err(format!("a journal of {} blocks in an inode of {}", jsb.maxlen(), j.map.len()));
    }
    let mut read = |jb: u32, buf: &mut [u8]| match j.block(img, jb) {
        Some(b) => {
            buf.copy_from_slice(b);
            Ok(())
        }
        None => Err(format!("journal block {} outside the image", jb)),
    };
    let plan = recovery::plan(&jsb, j.bs, &mut read)?;
    Ok((sb, j, jsb, plan))
}

/// Replay `img`'s journal in place, by extfmt's plan: every copy to its
/// home block, the journal marked empty at the next sequence, and the
/// filesystem -- its superblock as the replay left it -- no longer
/// needing recovery.
pub fn replay(img: &mut [u8]) -> Result<Plan, String> {
    let (sb, j, jsb, plan) = plan_of(img)?;
    for w in plan.writes.values() {
        if w.home >= sb.blocks_count() {
            return Err(format!("a copy for block {}, past the filesystem's {}", w.home, sb.blocks_count()));
        }
        let mut data = j.block(img, w.log).ok_or("a copy outside the image")?.to_vec();
        if w.escaped {
            data[..4].copy_from_slice(&jbd2::MAGIC.to_be_bytes());
        }
        let at = w.home as usize * j.bs;
        img[at..at + j.bs].copy_from_slice(&data);
    }
    let mut empty = jsb.clone();
    empty.set_start(0);
    empty.set_sequence(plan.next_sequence);
    empty.set_checksum();
    let at = j.map[0] as usize * j.bs;
    img[at..at + jbd2::SB_SIZE].copy_from_slice(empty.raw());
    let mut sb = Super::parse(&img[1024..2048])?;
    sb.set_incompat(sb.incompat() & !features::incompat::RECOVER);
    sb.set_checksum();
    img[1024..2048].copy_from_slice(sb.raw());
    Ok(plan)
}

/// The journal's ring turned so that its log starts `before` blocks short
/// of the ring's end and runs on round it: every block of the ring moved,
/// stale ones too, the superblock's start with them. The blocks say
/// nothing of where they are, so the journal is the same one -- for the
/// gate to have e2fsck replay a log that wraps.
pub fn rotate(img: &mut [u8], before: u32) -> Result<u32, String> {
    let (_, j, mut jsb, _) = plan_of(img)?;
    let (first, last, start) = (jsb.first(), jsb.last(), jsb.start());
    if start == 0 {
        return Err(String::from("an empty journal has no log to turn"));
    }
    let ring = last - first;
    let target = last - before.clamp(1, ring);
    let shift = (target + ring - start) % ring;
    let mut blocks = Vec::with_capacity(ring as usize);
    for b in first..last {
        blocks.push(j.block(img, b).ok_or("a ring block outside the image")?.to_vec());
    }
    for (i, data) in blocks.into_iter().enumerate() {
        let to = first + (i as u32 + shift) % ring;
        let at = j.map[to as usize] as usize * j.bs;
        img[at..at + j.bs].copy_from_slice(&data);
    }
    jsb = JournalSuper::parse(j.block(img, 0).ok_or("no journal superblock")?)?;
    let mut turned = jsb.clone();
    turned.set_start(first + (start - first + shift) % ring);
    turned.set_checksum();
    let at = j.map[0] as usize * j.bs;
    img[at..at + jbd2::SB_SIZE].copy_from_slice(turned.raw());
    Ok(turned.start())
}

/// Every block of the ring that has a journal block's header, in the ring's
/// order from the log's start: its journal block, type and sequence, and
/// where it is in the image -- for the gate to damage one of a kind.
pub fn log(img: &[u8]) -> Result<(Vec<u64>, Vec<(u32, u32, u32, u64)>), String> {
    let (_, j, jsb, _) = plan_of(img)?;
    let offsets = j.map.iter().map(|&b| b * j.bs as u64).collect();
    let (first, last) = (jsb.first(), jsb.last());
    let start = if jsb.start() == 0 { first } else { jsb.start() };
    let mut out = Vec::new();
    for i in 0..last - first {
        let b = first + (start - first + i) % (last - first);
        if let Some((kind, seq)) = j.block(img, b).and_then(jbd2::header) {
            out.push((b, kind, seq, j.map[b as usize] * j.bs as u64));
        }
    }
    Ok((offsets, out))
}

/// The planner over the journal of `img` damaged at random: up to eight
/// bytes of its superblock and of the log the clean plan read changed at a
/// time, the planner held to never panicking, always ending, and planning
/// only copies inside the ring.
pub fn fuzz(img: &mut [u8], seed: u64, iterations: u64) -> i32 {
    let (j, jsb) = match plan_of(img) {
        Ok((_, j, jsb, _)) => (j, jsb),
        Err(e) => {
            eprintln!("not a journal to fuzz: {}", e);
            return 2;
        }
    };
    /* Where the log is: from its start for as many blocks as there were
     * transactions' worth, or the whole ring of an empty journal. */
    let ring: Vec<u64> = (jsb.first()..jsb.last()).filter_map(|b| j.map.get(b as usize).copied()).collect();
    let targets: Vec<u64> = core::iter::once(j.map[0]).chain(ring.iter().copied()).collect();
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let (mut planned, mut refused) = (0u64, 0u64);
    for it in 0..iterations {
        ITERATION.store(it, std::sync::atomic::Ordering::Relaxed);
        let mut undo = Vec::new();
        for _ in 0..1 + rng.below(8) {
            /* Mostly the journal's superblock and the log's first blocks,
             * where its descriptors, revokes and commits are. */
            let block = if rng.below(4) == 0 {
                targets[rng.below(targets.len() as u64) as usize]
            } else {
                targets[rng.below(targets.len().min(24) as u64) as usize]
            };
            let at = block as usize * j.bs + rng.below(j.bs as u64) as usize;
            let Some(&old) = img.get(at) else { continue };
            undo.push((at, old));
            img[at] = match rng.below(3) {
                0 => 0,
                1 => 0xFF,
                _ => rng.next() as u8,
            };
        }
        match plan_of(img) {
            Ok((_, _, jsb, plan)) => {
                planned += 1;
                if let Some(w) = plan.writes.values().find(|w| w.log < jsb.first() || w.log >= jsb.last()) {
                    eprintln!("FINDING: seed {} iteration {}: a copy planned from journal block {}, outside the \
                               ring {}..{}", seed, it, w.log, jsb.first(), jsb.last());
                    return 1;
                }
            }
            Err(_) => refused += 1,
        }
        for (at, old) in undo.into_iter().rev() {
            img[at] = old;
        }
    }
    println!("fuzz-journal: {} damaged journals: {} planned, {} refused, no panic", iterations, planned, refused);
    0
}
