//! The journal of a filesystem mounted read-only: found through its inode,
//! checked, and -- when it holds transactions nobody replayed -- read
//! through, so that a read sees what a replay would have left and nothing
//! is written.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use extfmt::features::incompat;
use extfmt::inode::{Inode, S_IFREG};
use extfmt::jbd2::JournalSuper;
use extfmt::recovery;
use extfmt::sb::Super;
use extfmt::Map;
use kcore::trace;

use super::Ext4;

/// Where the newest copy of each home block is, in a journal nobody
/// replayed: its device block, and whether its first word was escaped.
pub type Overlay = BTreeMap<u64, (u64, bool)>;

/// The journal's blocks on the device: every block of its inode's size,
/// none a hole or unwritten, each inside the filesystem.
fn blocks(fs: &mut Ext4, sb: &Super) -> Result<Vec<u64>, String> {
    let ino = sb.journal_inum();
    let raw = fs.inode(ino).ok_or_else(|| format!("the journal's inode {} does not read", ino))?;
    let node = Inode::new(&raw).ok_or("a short journal inode")?;
    if node.file_type() != S_IFREG {
        return Err(format!("the journal's inode {} is not a regular file", ino));
    }
    let bs = fs.io.bs as u64;
    let n = node.size(true).div_ceil(bs);
    if n > u64::from(u32::MAX) || n < 2 {
        return Err(format!("a journal of {} blocks", n));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(n as usize).map_err(|_| String::from("no memory for the journal's map"))?;
    let mut l = 0u64;
    while l < n {
        match fs.map(ino, &raw, l) {
            Some(Map::Blocks { block, run }) => {
                for k in 0..run.min(n - l) {
                    out.push(block + k);
                }
                l += run;
            }
            Some(Map::Hole { .. }) => return Err(format!("the journal has a hole, or an unwritten run, at {}", l)),
            None => return Err(String::from("the journal's map does not read")),
        }
    }
    Ok(out)
}

/// The overlay of the journal of the filesystem `sb` describes: empty for
/// a journal with nothing to replay; why not for one that cannot be read
/// through -- a mount then refused, as a replay around it would show the
/// filesystem neither as it was nor as it was to be.
pub fn overlay(fs: &mut Ext4, sb: &Super) -> Result<Overlay, String> {
    let map = blocks(fs, sb)?;
    let bs = fs.io.bs;
    let mut buf = vec![0u8; bs];
    if !fs.io.read(map[0], &mut buf) {
        return Err(String::from("the journal's superblock does not read"));
    }
    let jsb = JournalSuper::parse(&buf)?;
    if jsb.maxlen() as usize > map.len() {
        return Err(format!("a journal of {} blocks in an inode of {}", jsb.maxlen(), map.len()));
    }
    let needs = sb.incompat() & incompat::RECOVER != 0;
    if jsb.start() != 0 && !needs {
        /* Linux throws such a log away (`jbd2_journal_wipe`): what is on
         * the disk is the filesystem. */
        trace!(0, "ext4: the journal holds a log the filesystem does not say needs replaying: read as it is");
        return Ok(Overlay::new());
    }
    if jsb.errno() != 0 {
        trace!(0, "ext4: the journal recorded error {}", jsb.errno() as i32);
    }
    if jsb.start() == 0 {
        return Ok(Overlay::new());
    }
    let plan = {
        let mut read = |jb: u32, out: &mut [u8]| match map.get(jb as usize) {
            Some(&dev) if fs.io.read(dev, out) => Ok(()),
            _ => Err(format!("journal block {} does not read", jb)),
        };
        recovery::plan(&jsb, bs, &mut read)?
    };
    if let Some(t) = plan.failed_commit {
        trace!(0, "ext4: journal transaction {} was corrupt: read up to it", t);
    }
    let first_journal = map.iter().copied().min().unwrap_or(0);
    let last_journal = map.iter().copied().max().unwrap_or(0);
    let mut out = Overlay::new();
    for w in &plan.writes {
        if w.home >= fs.io.blocks {
            return Err(format!("a journal copy for block {}, past the filesystem", w.home));
        }
        if w.home >= first_journal && w.home <= last_journal && map.contains(&w.home) {
            return Err(format!("a journal copy for block {}, the journal's own", w.home));
        }
        out.insert(w.home, (map[w.log as usize], w.escaped));
    }
    trace!(0, "ext4: {} transactions in the journal not replayed: read through them, {} blocks", plan.transactions,
        out.len());
    Ok(out)
}
