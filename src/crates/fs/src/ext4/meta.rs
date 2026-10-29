//! What a mounted ext4 knows of itself: its geometry, its superblock and
//! group descriptors as last committed, and -- for a mount that writes --
//! the system zone: every block its own metadata and its journal take,
//! which no file is ever given, whatever a bitmap says.

use alloc::vec::Vec;

use extfmt::features::{compat, ro_compat};
use extfmt::group::Desc;
use extfmt::sb::Super;

pub(super) struct Meta {
    /// The superblock as last committed, or as read.
    pub sb: Super,
    /// The descriptors, as last committed: whole blocks of them.
    pub gdt: Vec<u8>,
    pub bs: usize,
    pub blocks: u64,
    pub groups: u64,
    pub desc_size: usize,
    pub inode_size: usize,
    pub ipg: u32,
    pub bpg: u32,
    pub first_data: u64,
    pub first_ino: u32,
    pub seed: u32,
    pub csum: bool,
    /// Whether descriptors' uninit flags mean anything: with metadata_csum
    /// or gdt_csum.
    pub uninit: bool,
    /// Runs of blocks, sorted and apart: what no file may own.
    pub system: Vec<(u64, u64)>,
    /// The free blocks the descriptors count, as last committed: what the
    /// reserve a punch may dip into is kept out of (`txn::RESERVE`).
    pub free_blocks: u64,
}

impl Meta {
    /// The geometry of the filesystem `sb` describes, which the caller has
    /// held to the device (`Ext4::check_super`); no descriptors yet.
    pub fn new(sb: &Super) -> Meta {
        Meta {
            sb: sb.clone(),
            gdt: Vec::new(),
            bs: sb.block_size() as usize,
            blocks: sb.blocks_count(),
            groups: sb.group_count(),
            desc_size: sb.desc_size() as usize,
            inode_size: sb.inode_size() as usize,
            ipg: sb.inodes_per_group(),
            bpg: sb.blocks_per_group(),
            first_data: u64::from(sb.first_data_block()),
            first_ino: sb.first_ino(),
            seed: sb.csum_seed(),
            csum: sb.metadata_csum(),
            uninit: sb.metadata_csum() || sb.ro_compat() & ro_compat::GDT_CSUM != 0,
            system: Vec::new(),
            free_blocks: 0,
        }
    }

    /// The free blocks every descriptor counts, summed: once a mount has
    /// them, and then kept by each commit.
    pub fn count_free_blocks(&mut self) {
        self.free_blocks = (0..self.groups).filter_map(|g| self.desc(g)).map(|d| u64::from(d.free_blocks_count())).sum();
    }

    pub fn largedir(&self) -> bool {
        self.sb.largedir()
    }

    pub fn huge_file(&self) -> bool {
        self.sb.huge_file()
    }

    pub fn dir_nlink(&self) -> bool {
        self.sb.ro_compat() & ro_compat::DIR_NLINK != 0
    }

    /// The block the superblock is in, and where in it.
    pub fn sb_block(&self) -> u64 {
        extfmt::sb::OFFSET / self.bs as u64
    }

    pub fn sb_at(&self) -> usize {
        (extfmt::sb::OFFSET % self.bs as u64) as usize
    }

    /// The descriptors' first block, and how many there are.
    pub fn gdt_first(&self) -> u64 {
        self.first_data + 1
    }

    pub fn gdt_len(&self) -> u64 {
        (self.groups as usize * self.desc_size).div_ceil(self.bs) as u64
    }

    /// The block group `g`'s descriptor is in, and where in it.
    pub fn desc_at(&self, g: u64) -> (u64, usize) {
        let byte = g as usize * self.desc_size;
        (self.gdt_first() + (byte / self.bs) as u64, byte % self.bs)
    }

    /// Group `g`'s descriptor as last committed.
    pub fn desc(&self, g: u64) -> Option<Desc<'_>> {
        if g >= self.groups {
            return None;
        }
        let at = (g as usize).checked_mul(self.desc_size)?;
        Desc::new(self.gdt.get(at..at + self.desc_size)?)
    }

    pub fn table_blocks(&self) -> u64 {
        u64::from(self.ipg) * self.inode_size as u64 / self.bs as u64
    }

    pub fn group_start(&self, g: u64) -> u64 {
        self.first_data + g * u64::from(self.bpg)
    }

    /// How many blocks group `g` has: all its bitmap covers but in the last.
    pub fn group_len(&self, g: u64) -> u64 {
        u64::from(self.bpg).min(self.blocks.saturating_sub(self.group_start(g)))
    }

    pub fn group_of(&self, b: u64) -> u64 {
        (b.saturating_sub(self.first_data) / u64::from(self.bpg)).min(self.groups.saturating_sub(1))
    }

    /// Whether group `g` keeps a copy of the superblock and descriptors:
    /// every one, or with sparse_super 0, 1 and the powers of 3, 5 and 7.
    pub fn has_super(&self, g: u64) -> bool {
        if g <= 1 || self.sb.ro_compat() & ro_compat::SPARSE_SUPER == 0 {
            return true;
        }
        [3u64, 5, 7].iter().any(|&p| {
            let mut n = p;
            while n < g {
                n *= p;
            }
            n == g
        })
    }

    /// The blocks at group `g`'s start its copy of the superblock and the
    /// descriptors take, the descriptor blocks reserved for growing among
    /// them.
    pub fn base_meta(&self, g: u64) -> u64 {
        if !self.has_super(g) {
            return 0;
        }
        let reserved = if self.sb.compat() & compat::RESIZE_INODE != 0 {
            u64::from(self.sb.reserved_gdt_blocks())
        } else {
            0
        };
        1 + self.gdt_len() + reserved
    }

    /// The system zone, worked out: each group's copies of the superblock
    /// and descriptors, its bitmaps and inode table, and the journal's
    /// blocks. False with no memory for it.
    pub fn build_system_zone(&mut self, journal: &[u64]) -> bool {
        let mut runs: Vec<(u64, u64)> = Vec::new();
        let groups = self.groups as usize;
        if runs.try_reserve(groups.saturating_mul(4).saturating_add(journal.len())).is_err() {
            return false;
        }
        let table = self.table_blocks();
        for g in 0..self.groups {
            let base = self.base_meta(g);
            if base > 0 {
                runs.push((self.group_start(g), self.group_start(g) + base));
            }
            if let Some(d) = self.desc(g) {
                runs.push((d.block_bitmap(), d.block_bitmap() + 1));
                runs.push((d.inode_bitmap(), d.inode_bitmap() + 1));
                runs.push((d.inode_table(), d.inode_table() + table));
            }
        }
        runs.extend(journal.iter().map(|&b| (b, b + 1)));
        runs.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        if merged.try_reserve(runs.len()).is_err() {
            return false;
        }
        for (s, e) in runs {
            match merged.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        self.system = merged;
        true
    }

    pub fn in_system(&self, b: u64) -> bool {
        let at = self.system.partition_point(|&(s, _)| s <= b);
        at > 0 && b < self.system[at - 1].1
    }

    /// Whether any of blocks `start..end` is the system zone's.
    pub fn overlaps_system(&self, start: u64, end: u64) -> bool {
        let at = self.system.partition_point(|&(_, e)| e <= start);
        self.system.get(at).is_some_and(|&(s, _)| s < end)
    }
}
