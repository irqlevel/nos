//! A file's data, as the calls write it. A write is cut into transactions
//! of at most `CHUNK_BLOCKS` blocks, and each one's data is on the device
//! before its commit -- whose flush comes first -- as Linux's data=ordered
//! has it, so that no block a commit maps into a file can show what it held
//! before: a block mapped and written is written over in place, one in a
//! hole is allocated and written whole -- zeros around what goes into it --
//! and one in an unwritten extent is written whole and made written. The
//! blocks go to the device in batches of as many as the batch holds, each
//! one call (`Io::write_batch`), the last before the commit. A write
//! that fails part way leaves the transactions before it committed, and the
//! file the size they gave it. Where only a block of its data did not
//! write, the filesystem stays writable: that block's transaction is
//! undone, so nothing maps what failed, and one file's bad sector -- a
//! guest disk's -- is not every file's.
//!
//! A truncate gives back everything mapped past the new end -- unwritten
//! extents beyond the old end among them -- and writes zeros over what of
//! the new last block lies past it, so that a file reads as zeros wherever
//! it grows over; a punch gives back the blocks of a range inside the file,
//! the size as it was, and may take the free blocks kept for it
//! (`txn::RESERVE`). Each gives back from the end of its range, as much as
//! a transaction holds: a truncate that one cannot finish has its size and
//! the file on the orphan list in the first, for the ones after to finish
//! (`orphans.rs`); a punch goes on in more, each a range punched whole.

use alloc::vec;
use alloc::vec::Vec;

use extfmt::extent::Leaf;
use extfmt::inode::{Inode, EXTENTS_FL};
use extfmt::Map;
use kcore::trace;

use super::inodes as inode;
use super::txn::Txn;
use super::Ext4;
use crate::vnode::NodeId;

/// The most blocks one transaction of a write writes: its metadata -- the
/// bitmaps, the descriptors, the inode, the tree's leaves -- well inside
/// what a transaction holds.
const CHUNK_BLOCKS: u64 = 256;

/// The size at which a filesystem must say large_file.
const LARGE_FILE_SIZE: u64 = 1 << 31;

impl Txn<'_> {
    /// Regular file `ino`'s inode, to change: refused for one mapped by
    /// blocks, which this does not write.
    fn file_inode(&mut self, ino: u32) -> Option<Vec<u8>> {
        let raw = self.inode(ino)?;
        let node = Inode::new(&raw)?;
        if !node.is_file() {
            trace!(0, "ext4: inode {} is not a regular file", ino);
            return None;
        }
        if node.flags() & EXTENTS_FL == 0 {
            trace!(0, "ext4: inode {} is mapped by blocks, which this does not write", ino);
            return None;
        }
        Some(raw)
    }

    /// `data` written into file `ino` (`raw`) at byte `at`, block by block:
    /// over what is there, into a block allocated for a hole, or into an
    /// unwritten extent's block, made written. Each block is made whole in a
    /// page of the batch -- what one written over in part holds read in
    /// first, zeros around what goes into one that showed none -- and the
    /// batch is written when it is full, and at the end: every block's data
    /// is on the device -- in its cache -- before this returns, and so
    /// before the commit that maps it.
    fn write_data(&mut self, ino: u32, raw: &mut [u8], data: &[u8], at: u64) -> bool {
        let bs = self.m.bs;
        let mut goal = self.m.group_start(u64::from((ino - 1) / self.m.ipg));
        let mut batch: Vec<(usize, u64)> = Vec::with_capacity(self.io.batch_blocks());
        let mut done = 0usize;
        while done < data.len() {
            let pos = at + done as u64;
            let (l, within) = (pos / bs as u64, (pos % bs as u64) as usize);
            let n = (bs - within).min(data.len() - done);
            let Ok(l) = u32::try_from(l) else { return false };
            let k = batch.len();
            let Some(found) = self.ext_lookup(ino, raw, l) else { return false };
            let b = match found {
                Some(e) if !e.unwritten => {
                    let b = e.start + u64::from(l - e.first);
                    if n < bs && !self.io.read_slot(b, k) {
                        return false;
                    }
                    b
                }
                Some(e) => {
                    /* Unwritten: it reads as zeros, whatever its block holds. */
                    if self.ext_mark_written(ino, raw, l, 1).is_none() {
                        return false;
                    }
                    self.io.slot_mut(k).fill(0);
                    e.start + u64::from(l - e.first)
                }
                None => {
                    let Some(b) = self.alloc_block(goal) else { return false };
                    if !self.ext_insert(ino, raw, Leaf { first: l, len: 1, start: b, unwritten: false })
                        || !inode::add_blocks(self.m, raw, 1) {
                        return false;
                    }
                    self.io.slot_mut(k).fill(0);
                    b
                }
            };
            self.io.slot_mut(k)[within..within + n].copy_from_slice(&data[done..done + n]);
            batch.push((k, b));
            if batch.len() == self.io.batch_blocks() {
                if !self.io.write_batch(&batch) {
                    return false;
                }
                batch.clear();
            }
            goal = b + 1;
            done += n;
        }
        self.io.write_batch(&batch)
    }

    /// One transaction of a write: `data` into file `ino` at `at`, the size
    /// grown to its end, the times.
    fn write_chunk(&mut self, ino: u32, data: &[u8], at: u64) -> bool {
        let Some(mut raw) = self.file_inode(ino) else { return false };
        let size = Inode::new(&raw).map_or(0, |i| i.size(true));
        if !self.write_data(ino, &mut raw, data, at) {
            return false;
        }
        let end = at + data.len() as u64;
        if end > size {
            if end >= LARGE_FILE_SIZE && !self.large_file() {
                return false;
            }
            inode::set_size(&mut raw, end);
        }
        inode::touch(&mut raw, self.now, true);
        self.put_inode(ino, &mut raw)
    }

    /// File `ino` cut, or grown, to `size` bytes: past a smaller one every
    /// mapping given back -- or as much as this transaction holds, the file
    /// then an orphan for the transactions after -- and the new last
    /// block's tail made zeros. Whether it is an orphan now, or None.
    fn truncate_inode(&mut self, ino: u32, size: u64) -> Option<bool> {
        let mut raw = self.file_inode(ino)?;
        let old = Inode::new(&raw).map_or(0, |i| i.size(true));
        let bs = self.m.bs as u64;
        let mut orphaned = false;
        let mut tail: Option<u64> = None;
        if size < old {
            let keep = size.div_ceil(bs);
            let pieces = self.free_pieces();
            let (_, end) = self.ext_unmap_tail(ino, &mut raw, keep, 1 << 32, pieces)?;
            orphaned = end > keep;
            if size % bs != 0 {
                let l = (size / bs) as u32;
                tail = self.ext_lookup(ino, &raw, l)?.filter(|e| !e.unwritten).map(|e| e.start + u64::from(l - e.first));
            }
        }
        if size >= LARGE_FILE_SIZE && !self.large_file() {
            return None;
        }
        inode::set_size(&mut raw, size);
        inode::touch(&mut raw, self.now, true);
        if orphaned && !self.orphan_add(ino, &mut raw) {
            return None;
        }
        if !self.put_inode(ino, &mut raw) {
            return None;
        }
        /* The new last block's tail made zeros last, in place: before the
         * commit, as data=ordered has it, and after everything else of the
         * transaction is done, so that only the commit can fail after it. */
        if let Some(b) = tail {
            let mut blk = vec![0u8; self.m.bs];
            if !self.io.read(b, &mut blk) {
                return None;
            }
            blk[(size % bs) as usize..].fill(0);
            if !self.io.write(b, &blk, false) {
                return None;
            }
        }
        Some(orphaned)
    }

    /// File blocks `first..to` of `ino` given back from the end, as many as
    /// this transaction holds: how many, and where what is left of the
    /// range ends -- `first` when nothing is.
    fn punch_inode(&mut self, ino: u32, first: u64, to: u64) -> Option<(u64, u64)> {
        let mut raw = self.file_inode(ino)?;
        let pieces = self.free_pieces();
        let (freed, end) = self.ext_unmap_tail(ino, &mut raw, first, to, pieces)?;
        inode::touch(&mut raw, self.now, true);
        self.put_inode(ino, &mut raw).then_some((freed, end))
    }
}

impl Ext4 {
    /// The biggest a file is here: what extents number, 2^32 - 1 blocks.
    pub fn max_size(&self) -> u64 {
        ((1u64 << 32) - 1) * self.io.bs as u64
    }

    /// The vnodes of inode `ino` -- `file`, and its other names where it has
    /// more than one -- given the size the inode has.
    fn sized(&mut self, file: NodeId, ino: u32) {
        let Some(raw) = self.inode(ino) else { return };
        let Some(node) = Inode::new(&raw) else { return };
        let size = usize::try_from(node.size(true)).unwrap_or(usize::MAX);
        if node.links_count() <= 1 {
            if let Some(n) = self.tree.get_mut(file) {
                n.size = size;
            }
            return;
        }
        let Some(root) = self.root else { return };
        let mut stack = vec![root];
        while let Some(at) = stack.pop() {
            let Some(n) = self.tree.get_mut(at) else { continue };
            if n.is_file() && n.ino == ino as usize {
                n.size = size;
            }
            if n.is_dir() {
                stack.extend(self.tree.children(at));
            }
        }
    }

    pub fn write(&mut self, file: NodeId, data: &[u8], offset: usize) -> bool {
        if !self.writable() {
            return false;
        }
        let Some(ino) = self.file_ino(file) else {
            trace!(0, "ext4: a write to something that is not a file");
            return false;
        };
        if data.is_empty() {
            return true;
        }
        let end = offset as u64 + data.len() as u64;
        if end > self.max_size() {
            trace!(0, "ext4: a write of {} bytes at {} is past what a file holds here", data.len(), offset);
            return false;
        }
        let bs = self.io.bs as u64;
        let mut done = 0usize;
        let mut ok = true;
        while done < data.len() {
            let pos = offset as u64 + done as u64;
            let room = (CHUNK_BLOCKS * bs - pos % bs) as usize;
            let n = (data.len() - done).min(room);
            let size = self.inode(ino).and_then(|raw| Inode::new(&raw).map(|i| i.size(true))).unwrap_or(0);
            let committed = match self.begin() {
                Some(mut t) => t.write_chunk(ino, &data[done..done + n], pos) && t.commit(),
                None => false,
            };
            if !committed {
                self.zero_past_end(ino, size, pos, n as u64);
                self.flush_failed();
                ok = false;
                break;
            }
            done += n;
        }
        self.sized(file, ino);
        ok
    }

    /// After a write of `n` bytes at `pos` that never committed, zeros again
    /// over what of file `ino`'s last block lies past its end, `size`: the
    /// write was into the block in place, and the size that would have
    /// covered it never came -- what it left there would show the moment
    /// the file grew over it. Should that fail, the journal is aborted: the
    /// filesystem no longer keeps a file reading as zeros where it grows.
    fn zero_past_end(&mut self, ino: u32, size: u64, pos: u64, n: u64) {
        let bs = self.io.bs as u64;
        let tail = size % bs;
        if tail == 0 || pos >= size.div_ceil(bs) * bs || pos + n <= size {
            return;
        }
        let Some(raw) = self.inode(ino) else { return };
        let Some(Map::Blocks { block, .. }) = self.map(ino, &raw, size / bs) else { return };
        let mut blk = vec![0u8; self.io.bs];
        let zeroed = self.io.read(block, &mut blk) && {
            blk[tail as usize..].fill(0);
            self.io.write(block, &blk, false)
        };
        if !zeroed {
            if let Some(j) = self.jnl.as_mut() {
                j.abort("what a failed write left past a file's end cannot be made zeros again");
            }
        }
    }

    /// After a write that never committed, what it wrote in place -- over
    /// blocks the file had, and the zeros put back past its end -- forced to
    /// the medium, as everything a call leaves is when it returns: no commit
    /// came to flush it, and a power cut would otherwise take the file back
    /// from what it reads as. A flush the device fails aborts the journal.
    fn flush_failed(&mut self) {
        if self.jnl.as_ref().is_none_or(|j| j.aborted) || self.io.flush() {
            return;
        }
        if let Some(j) = self.jnl.as_mut() {
            j.abort("the device does not flush what a failed write left");
        }
    }

    pub fn truncate(&mut self, file: NodeId, size: usize) -> bool {
        if !self.writable() {
            return false;
        }
        let Some(ino) = self.file_ino(file) else {
            trace!(0, "ext4: a truncate of something that is not a file");
            return false;
        };
        if size as u64 > self.max_size() {
            trace!(0, "ext4: a size of {} is past what a file holds here", size);
            return false;
        }
        let done = match self.begin() {
            Some(mut t) => match t.truncate_inode(ino, size as u64) {
                Some(orphaned) => t.commit().then_some(orphaned),
                None => None,
            },
            None => None,
        };
        /* Cut with the first transaction, whatever comes of the rest: what
         * a later one fails to give back, the next mount does. */
        if done == Some(true) {
            self.finish_orphan(ino);
        }
        self.sized(file, ino);
        done.is_some()
    }

    /// The blocks of `count` blocks of `file` from block `first` -- of those
    /// inside it -- given back: holes from here on, the size as it was. How
    /// many, or None.
    pub fn punch(&mut self, file: NodeId, first: u64, count: u64) -> Option<u64> {
        if !self.writable() {
            return None;
        }
        let Some(ino) = self.file_ino(file) else {
            trace!(0, "ext4: a punch of something that is not a file");
            return None;
        };
        let size = self.tree.get(file)?.size as u64;
        let mut end = first.saturating_add(count).min(size.div_ceil(self.io.bs as u64));
        let mut given = 0u64;
        while end > first {
            let mut t = self.begin()?;
            t.may_reserve = true;
            let (freed, left) = t.punch_inode(ino, first, end)?;
            if !t.commit() {
                return None;
            }
            if left >= end {
                trace!(0, "ext4: a punch of inode {} gave nothing back of {}..{}", ino, first, end);
                return None;
            }
            given += freed;
            end = left;
        }
        Some(given)
    }
}
