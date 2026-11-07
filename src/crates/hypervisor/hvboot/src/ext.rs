//! ext2, ext3 and ext4, read-only: what GRUB reads a Linux disk's kernel
//! from. Enough of the format to find a file by its path and read it --
//! extents and block maps, 64-bit descriptors, flex_bg, htree directories
//! (read as the linear ones they also are), inline data, fast and slow
//! symlinks, holes -- and a refusal, saying which, of any incompatible
//! feature it does not read. The journal is not replayed, as GRUB does not
//! replay it: a filesystem left needing recovery is read as it is on disk.
//!
//! Everything on the disk is the guest's and is checked before it is used:
//! a block outside the filesystem, a descriptor or an inode outside its
//! table, an extent tree deeper than the format allows or whose node lies
//! about its entries, a directory entry running past its block -- each an
//! error, never an index out of range. The format itself -- superblock,
//! descriptors, inodes, extent trees, block maps, directory entries -- is
//! extfmt's, which the kernel's ext4 driver is to read it through as well; what
//! is here is GRUB's way of using it: which features are read, a path
//! walked, a file read whole.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use extfmt::features::incompat;
use extfmt::group::Desc;
use extfmt::inode::{EXTENTS_FL, INLINE_DATA_FL, I_BLOCK_BYTES, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use extfmt::sb::Super;
use extfmt::Map;

/// A filesystem's bytes: `buf.len()` of them at `offset`, both whole
/// sectors.
pub trait Source {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String>;
}

/* Incompatible features: those read, and the rest refused. What GRUB reads
 * past -- a journal not replayed, MMP's block, ea_inode's attributes, the
 * seed of checksums it does not check -- is read past here too. */
const INCOMPAT_READ: u32 = incompat::FILETYPE | incompat::RECOVER | incompat::EXTENTS | incompat::BIT64
    | incompat::MMP | incompat::FLEX_BG | incompat::EA_INODE | incompat::CSUM_SEED | incompat::LARGEDIR
    | incompat::INLINE_DATA;

const ROOT_INO: u32 = 2;

/// Block sizes read: 1, 2 and 4 KiB -- what mkfs makes on x86.
const MAX_LOG_BLOCK_SIZE: u32 = 2;
const MAX_SYMLINKS: u32 = 8;
const MAX_LINK_BYTES: u64 = 4096;
/// The most blocks of a directory searched.
const MAX_DIR_BLOCKS: u64 = 1 << 16;

/// An inode, as read.
#[derive(Clone, Debug)]
pub struct Inode {
    pub ino: u32,
    mode: u16,
    flags: u32,
    pub size: u64,
    block: [u8; I_BLOCK_BYTES],
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
    fn is_link(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }
}

pub struct Fs<S> {
    src: S,
    block_size: u64,
    blocks: u64,
    inodes: u32,
    inodes_per_group: u32,
    groups: u64,
    inode_size: u64,
    desc_size: u64,
    /// Where the group descriptors begin, in bytes.
    descriptors: u64,
    incompat: u32,
    uuid: [u8; 16],
    label: [u8; 16],
}

impl<S: Source> Fs<S> {
    /// The filesystem on `src`, if there is one this reads.
    pub fn mount(mut src: S) -> Result<Fs<S>, String> {
        let mut raw = [0u8; extfmt::sb::SIZE];
        src.read(extfmt::sb::OFFSET, &mut raw)?;
        /* The magic and a geometry that holds together are the format's to
         * check; which block sizes and features are read is GRUB's. */
        let sb = Super::parse(&raw)?;
        let log = sb.log_block_size();
        if log > MAX_LOG_BLOCK_SIZE {
            return Err(alloc::format!("blocks of {} KiB, more than this reads", 1u64 << log.min(20)));
        }
        let incompat = sb.incompat();
        if incompat & !INCOMPAT_READ != 0 {
            return Err(alloc::format!("incompatible features {:#x} this does not read", incompat & !INCOMPAT_READ));
        }
        let block_size = sb.block_size();
        let descriptors = (u64::from(sb.first_data_block()) + 1) * block_size;
        Ok(Fs { src, block_size, blocks: sb.blocks_count(), inodes: sb.inodes_count(),
                inodes_per_group: sb.inodes_per_group(), groups: sb.group_count(), inode_size: sb.inode_size(),
                desc_size: sb.desc_size(), descriptors, incompat, uuid: sb.uuid(), label: sb.volume_name() })
    }

    /// Its UUID as GRUB's `search --fs-uuid` writes one: 8-4-4-4-12 lowercase
    /// hex.
    pub fn uuid(&self) -> String {
        let mut s = String::new();
        for (i, b) in self.uuid.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                s.push('-');
            }
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{:02x}", b));
        }
        s
    }

    /// Its label, up to the first NUL.
    pub fn label(&self) -> String {
        let end = self.label.iter().position(|&b| b == 0).unwrap_or(self.label.len());
        String::from_utf8_lossy(&self.label[..end]).into_owned()
    }

    /// Left needing its journal replayed: read as it is on disk regardless.
    pub fn needs_recovery(&self) -> bool {
        self.incompat & incompat::RECOVER != 0
    }

    /// Read whole block `block` into `buf`, a block long.
    fn read_block(&mut self, block: u64, buf: &mut [u8]) -> Result<(), String> {
        if block >= self.blocks {
            return Err(alloc::format!("block {} of a filesystem of {}", block, self.blocks));
        }
        self.src.read(block * self.block_size, buf)
    }

    /// Inode `ino`.
    pub fn inode(&mut self, ino: u32) -> Result<Inode, String> {
        if ino == 0 || ino > self.inodes {
            return Err(alloc::format!("inode {} of {}", ino, self.inodes));
        }
        let group = u64::from((ino - 1) / self.inodes_per_group);
        let index = u64::from((ino - 1) % self.inodes_per_group);
        if group >= self.groups {
            return Err(alloc::format!("inode {} in group {} of {}", ino, group, self.groups));
        }
        let bs = self.block_size;
        let mut buf = vec![0u8; bs as usize];

        let at = self.descriptors + group * self.desc_size;
        self.read_block(at / bs, &mut buf)?;
        let d = (at % bs) as usize;
        /* The descriptor size divides the block (a power of two no bigger
         * than it), so a descriptor never straddles two. */
        let table = Desc::new(&buf[d..d + self.desc_size as usize])
            .ok_or_else(|| alloc::format!("group {}'s descriptor of {} bytes", group, self.desc_size))?
            .inode_table();
        let at = table
            .checked_mul(bs)
            .and_then(|t| t.checked_add(index * self.inode_size))
            .ok_or_else(|| alloc::format!("group {}'s inode table at block {}", group, table))?;
        self.read_block(at / bs, &mut buf)?;
        let i = (at % bs) as usize;
        let raw = extfmt::inode::Inode::new(&buf[i..i + self.inode_size as usize])
            .ok_or_else(|| alloc::format!("inode {} of {} bytes", ino, self.inode_size))?;
        let mut block = [0u8; I_BLOCK_BYTES];
        block.copy_from_slice(raw.i_block());
        /* The size's high word is a regular file's alone, as GRUB reads it. */
        Ok(Inode { ino, mode: raw.mode(), flags: raw.flags(), size: raw.size(false), block })
    }

    /// Where the file's block `lblock` is.
    fn map(&mut self, inode: &Inode, lblock: u64) -> Result<Map, String> {
        let (blocks, bs) = (self.blocks, self.block_size as usize);
        let mut read = |block: u64, buf: &mut [u8]| self.read_block(block, buf);
        if inode.flags & EXTENTS_FL != 0 {
            extfmt::extent::map(&inode.block, lblock, blocks, bs, inode.ino, &mut read)
        } else {
            extfmt::blockmap::map(&inode.block, lblock, bs, inode.ino, &mut read)
        }
    }

    /// Read the file's bytes `[offset, offset + dest.len())` into `dest`.
    pub fn read(&mut self, inode: &Inode, offset: u64, dest: &mut [u8]) -> Result<(), String> {
        if offset.checked_add(dest.len() as u64).is_none_or(|end| end > inode.size) {
            return Err(alloc::format!("inode {}: {} bytes at {} of {}", inode.ino, dest.len(), offset, inode.size));
        }
        if inode.flags & INLINE_DATA_FL != 0 {
            if inode.size > I_BLOCK_BYTES as u64 {
                return Err(alloc::format!("inode {}: inline data past its inode, which this does not read",
                                          inode.ino));
            }
            let at = offset as usize;
            dest.copy_from_slice(&inode.block[at..at + dest.len()]);
            return Ok(());
        }
        let bs = self.block_size;
        let mut bounce: Vec<u8> = Vec::new();
        let mut done = 0usize;
        while done < dest.len() {
            let pos = offset + done as u64;
            let (lblock, within) = (pos / bs, (pos % bs) as usize);
            let left = dest.len() - done;
            match self.map(inode, lblock)? {
                Map::Hole { run } => {
                    let n = (run.saturating_mul(bs) - within as u64).min(left as u64) as usize;
                    dest[done..done + n].fill(0);
                    done += n;
                }
                Map::Blocks { block, run } => {
                    if block.checked_add(run).is_none_or(|end| end > self.blocks) {
                        return Err(alloc::format!("inode {}: blocks {}..+{} past the filesystem's {}", inode.ino,
                                                  block, run, self.blocks));
                    }
                    if within == 0 && left as u64 >= bs {
                        /* Whole blocks straight into place, as many as the
                         * run and the file have. */
                        let n = (run.min(left as u64 / bs) * bs) as usize;
                        self.src.read(block * bs, &mut dest[done..done + n])?;
                        done += n;
                    } else {
                        if bounce.is_empty() {
                            bounce = vec![0u8; bs as usize];
                        }
                        self.read_block(block, &mut bounce)?;
                        let n = (bs as usize - within).min(left);
                        dest[done..done + n].copy_from_slice(&bounce[within..within + n]);
                        done += n;
                    }
                }
            }
        }
        Ok(())
    }

    /// The whole of a file of at most `max` bytes.
    pub fn read_all(&mut self, inode: &Inode, max: u64) -> Result<Vec<u8>, String> {
        if inode.size > max {
            return Err(alloc::format!("inode {}: {} bytes, more than {}", inode.ino, inode.size, max));
        }
        let mut v = vec![0u8; inode.size as usize];
        self.read(inode, 0, &mut v)?;
        Ok(v)
    }

    /// The inode named `name` in directory `dir`.
    fn find(&mut self, dir: &Inode, name: &[u8]) -> Result<Option<u32>, String> {
        if dir.flags & INLINE_DATA_FL != 0 {
            /* An inline directory: its parent's inode first, then entries. */
            if name == b".." {
                return Ok(Some(extfmt::le32(&dir.block, 0)));
            }
            let end = (dir.size as usize).min(I_BLOCK_BYTES);
            return extfmt::dir::find_entry(&dir.block[4.min(end)..end], name, dir.ino);
        }
        let bs = self.block_size;
        let blocks = dir.size.div_ceil(bs).min(MAX_DIR_BLOCKS);
        let mut buf = vec![0u8; bs as usize];
        let mut lblock = 0;
        while lblock < blocks {
            match self.map(dir, lblock)? {
                Map::Hole { run } => lblock = lblock.saturating_add(run),
                Map::Blocks { block, run } => {
                    for k in 0..run.min(blocks - lblock) {
                        self.read_block(block + k, &mut buf)?;
                        if let Some(ino) = extfmt::dir::find_entry(&buf, name, dir.ino)? {
                            return Ok(Some(ino));
                        }
                    }
                    lblock += run;
                }
            }
        }
        Ok(None)
    }

    /// A symlink's target.
    fn link_target(&mut self, link: &Inode) -> Result<Vec<u8>, String> {
        if link.size > MAX_LINK_BYTES {
            return Err(alloc::format!("inode {}: a symlink of {} bytes", link.ino, link.size));
        }
        if link.flags & (EXTENTS_FL | INLINE_DATA_FL) == 0 && link.size < I_BLOCK_BYTES as u64 {
            return Ok(link.block[..link.size as usize].to_vec());
        }
        self.read_all(link, MAX_LINK_BYTES)
    }

    /// The inode at `path`, from the root, symlinks followed.
    pub fn lookup(&mut self, path: &str) -> Result<Inode, String> {
        let root = self.inode(ROOT_INO)?;
        let mut dir = root.clone();
        /* The components still to walk, the next last. */
        let mut todo: Vec<Vec<u8>> = path.split('/').rev().map(|c| c.as_bytes().to_vec()).collect();
        let mut links = 0u32;
        let mut current = dir.clone();
        while let Some(name) = todo.pop() {
            if name.is_empty() || name == b"." {
                continue;
            }
            if !current.is_dir() {
                return Err(alloc::format!("{}: a component is not a directory", path));
            }
            dir = current.clone();
            let Some(ino) = self.find(&dir, &name)? else {
                return Err(alloc::format!("{}: no such file", path));
            };
            let node = self.inode(ino)?;
            if node.is_link() {
                links += 1;
                if links > MAX_SYMLINKS {
                    return Err(alloc::format!("{}: more than {} symlinks", path, MAX_SYMLINKS));
                }
                let target = self.link_target(&node)?;
                for c in target.split(|&b| b == b'/').rev() {
                    todo.push(c.to_vec());
                }
                current = if target.first() == Some(&b'/') { root.clone() } else { dir.clone() };
                continue;
            }
            current = node;
        }
        Ok(current)
    }
}
