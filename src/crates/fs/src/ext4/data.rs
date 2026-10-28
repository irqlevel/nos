//! A file's data, as the calls write it. A write is cut into transactions
//! of at most `CHUNK_BLOCKS` blocks, and each one's data is on the device
//! before its commit -- whose flush comes first -- as Linux's data=ordered
//! has it, so that no block a commit maps into a file can show what it held
//! before: a block mapped and written is written over in place, one in a
//! hole is allocated and written whole -- zeros around what goes into it --
//! and one in an unwritten extent is written and then made written. A write
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
//! the size as it was. Each is one transaction: a file whose blocks it
//! could not give back in one is the next step's (S7, the orphan list).

use alloc::vec;

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
    fn file_inode(&mut self, ino: u32) -> Option<alloc::vec::Vec<u8>> {
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
    /// unwritten extent's block, made written after. Every block's data is
    /// on the device -- in its cache -- before this returns.
    fn write_data(&mut self, ino: u32, raw: &mut [u8], data: &[u8], at: u64) -> bool {
        let bs = self.m.bs;
        let mut goal = self.m.group_start(u64::from((ino - 1) / self.m.ipg));
        let mut blk = vec![0u8; bs];
        let mut done = 0usize;
        while done < data.len() {
            let pos = at + done as u64;
            let (l, within) = (pos / bs as u64, (pos % bs as u64) as usize);
            let n = (bs - within).min(data.len() - done);
            let Ok(l) = u32::try_from(l) else { return false };
            let piece = &data[done..done + n];
            let Some(found) = self.ext_lookup(ino, raw, l) else { return false };
            match found {
                Some(e) if !e.unwritten => {
                    let b = e.start + u64::from(l - e.first);
                    if n < bs && !self.io.read(b, &mut blk) {
                        return false;
                    }
                    blk[within..within + n].copy_from_slice(piece);
                    if !self.io.write(b, &blk, false) {
                        return false;
                    }
                    goal = b + 1;
                }
                Some(e) => {
                    /* Unwritten: it reads as zeros, whatever its block holds. */
                    let b = e.start + u64::from(l - e.first);
                    blk.fill(0);
                    blk[within..within + n].copy_from_slice(piece);
                    if !self.io.write(b, &blk, false) || self.ext_mark_written(ino, raw, l, 1).is_none() {
                        return false;
                    }
                    goal = b + 1;
                }
                None => {
                    let Some(b) = self.alloc_block(goal) else { return false };
                    blk.fill(0);
                    blk[within..within + n].copy_from_slice(piece);
                    if !self.io.write(b, &blk, false)
                        || !self.ext_insert(ino, raw, Leaf { first: l, len: 1, start: b, unwritten: false })
                        || !inode::add_blocks(self.m, raw, 1) {
                        return false;
                    }
                    goal = b + 1;
                }
            }
            done += n;
        }
        true
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
    /// mapping given back and the new last block's tail made zeros.
    fn truncate_inode(&mut self, ino: u32, size: u64) -> bool {
        let Some(mut raw) = self.file_inode(ino) else { return false };
        let old = Inode::new(&raw).map_or(0, |i| i.size(true));
        let bs = self.m.bs as u64;
        if size < old {
            if self.ext_unmap(ino, &mut raw, size.div_ceil(bs), 1 << 32).is_none() {
                return false;
            }
            if size % bs != 0 {
                let l = (size / bs) as u32;
                match self.ext_lookup(ino, &raw, l) {
                    None => return false,
                    Some(Some(e)) if !e.unwritten => {
                        let b = e.start + u64::from(l - e.first);
                        let mut blk = vec![0u8; self.m.bs];
                        if !self.io.read(b, &mut blk) {
                            return false;
                        }
                        blk[(size % bs) as usize..].fill(0);
                        if !self.io.write(b, &blk, false) {
                            return false;
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        if size >= LARGE_FILE_SIZE && !self.large_file() {
            return false;
        }
        inode::set_size(&mut raw, size);
        inode::touch(&mut raw, self.now, true);
        self.put_inode(ino, &mut raw)
    }

    /// File blocks `first..to` of `ino` given back: how many.
    fn punch_inode(&mut self, ino: u32, first: u64, to: u64) -> Option<u64> {
        let mut raw = self.file_inode(ino)?;
        let freed = self.ext_unmap(ino, &mut raw, first, to)?;
        inode::touch(&mut raw, self.now, true);
        self.put_inode(ino, &mut raw).then_some(freed)
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
            Some(mut t) => t.truncate_inode(ino, size as u64) && t.commit(),
            None => false,
        };
        self.sized(file, ino);
        done
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
        let to = first.saturating_add(count).min(size.div_ceil(self.io.bs as u64));
        if first >= to {
            return Some(0);
        }
        let mut t = self.begin()?;
        let freed = t.punch_inode(ino, first, to)?;
        t.commit().then_some(freed)
    }
}
