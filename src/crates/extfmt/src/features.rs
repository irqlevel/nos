//! The feature flags a superblock names, in its three words -- compat
//! (a reader that does not know one may ignore it), incompat (it must not
//! mount the filesystem at all) and ro_compat (it may mount it read-only)
//! -- and what a driver of the kind nos's ext4 is may do with a filesystem
//! that has them: one that writes only through a journal, maps every file
//! it writes by extents and keeps metadata_csum's checksums.

use crate::sb::Super;

pub mod compat {
    pub const DIR_PREALLOC: u32 = 0x1;
    pub const IMAGIC_INODES: u32 = 0x2;
    pub const HAS_JOURNAL: u32 = 0x4;
    pub const EXT_ATTR: u32 = 0x8;
    pub const RESIZE_INODE: u32 = 0x10;
    pub const DIR_INDEX: u32 = 0x20;
    pub const LAZY_BG: u32 = 0x40;
    pub const EXCLUDE_INODE: u32 = 0x80;
    pub const EXCLUDE_BITMAP: u32 = 0x100;
    pub const SPARSE_SUPER2: u32 = 0x200;
    pub const FAST_COMMIT: u32 = 0x400;
    pub const STABLE_INODES: u32 = 0x800;
    pub const ORPHAN_FILE: u32 = 0x1000;
}

pub mod incompat {
    pub const COMPRESSION: u32 = 0x1;
    pub const FILETYPE: u32 = 0x2;
    /// Not a feature but a state: the journal holds transactions that have
    /// not reached their places yet.
    pub const RECOVER: u32 = 0x4;
    pub const JOURNAL_DEV: u32 = 0x8;
    pub const META_BG: u32 = 0x10;
    pub const EXTENTS: u32 = 0x40;
    pub const BIT64: u32 = 0x80;
    pub const MMP: u32 = 0x100;
    pub const FLEX_BG: u32 = 0x200;
    pub const EA_INODE: u32 = 0x400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x10000;
    pub const CASEFOLD: u32 = 0x20000;
}

pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x1;
    pub const LARGE_FILE: u32 = 0x2;
    pub const BTREE_DIR: u32 = 0x4;
    pub const HUGE_FILE: u32 = 0x8;
    pub const GDT_CSUM: u32 = 0x10;
    pub const DIR_NLINK: u32 = 0x20;
    pub const EXTRA_ISIZE: u32 = 0x40;
    pub const HAS_SNAPSHOT: u32 = 0x80;
    pub const QUOTA: u32 = 0x100;
    pub const BIGALLOC: u32 = 0x200;
    pub const METADATA_CSUM: u32 = 0x400;
    pub const REPLICA: u32 = 0x800;
    pub const READONLY: u32 = 0x1000;
    pub const PROJECT: u32 = 0x2000;
    pub const SHARED_BLOCKS: u32 = 0x4000;
    pub const VERITY: u32 = 0x8000;
    pub const ORPHAN_PRESENT: u32 = 0x10000;
}

/// The compat features such a driver keeps as they are while it writes:
/// a hint (DIR_PREALLOC), what it maintains (HAS_JOURNAL, EXT_ATTR's
/// blocks, DIR_INDEX by dropping a directory's index when it changes one),
/// what it never touches (RESIZE_INODE's reserved blocks), a journal area
/// it never writes (FAST_COMMIT) and a promise it keeps (STABLE_INODES).
pub const COMPAT_WRITABLE: u32 = compat::DIR_PREALLOC | compat::HAS_JOURNAL | compat::EXT_ATTR
    | compat::RESIZE_INODE | compat::DIR_INDEX | compat::FAST_COMMIT | compat::STABLE_INODES;

/// The incompat features it reads -- and writes.
pub const INCOMPAT_READ: u32 = incompat::FILETYPE | incompat::RECOVER | incompat::EXTENTS | incompat::BIT64
    | incompat::FLEX_BG | incompat::CSUM_SEED;

/// The ro_compat features it keeps while it writes.
pub const RO_COMPAT_WRITABLE: u32 = ro_compat::SPARSE_SUPER | ro_compat::LARGE_FILE | ro_compat::HUGE_FILE
    | ro_compat::DIR_NLINK | ro_compat::EXTRA_ISIZE | ro_compat::METADATA_CSUM;

/// The ro_compat features whose filesystem it does not even read: bigalloc
/// counts its bitmaps in clusters, and verity's files are not to be read
/// without their Merkle trees checked.
pub const RO_COMPAT_REFUSED: u32 = ro_compat::BIGALLOC | ro_compat::VERITY;

/// What may be done with a filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Read and written.
    Writable,
    /// Read only, for the reason given.
    ReadOnly(&'static str),
    /// Not mounted at all, for the reason given.
    Refused(&'static str),
}

/// What a driver that writes through a journal, by extents, with
/// metadata_csum's checksums, may do with `sb`'s filesystem. The first
/// reason found is the one given: a refusal before a reason to read only.
pub fn classify(sb: &Super) -> Support {
    let (c, i, r) = (sb.compat(), sb.incompat(), sb.ro_compat());
    if sb.rev_level() == 0 {
        return Support::Refused("revision 0, which has no features");
    }
    if i & incompat::JOURNAL_DEV != 0 {
        return Support::Refused("an external journal's device, not a filesystem");
    }
    if i & !INCOMPAT_READ != 0 {
        return Support::Refused(incompat_reason(i & !INCOMPAT_READ));
    }
    if i & incompat::FILETYPE == 0 {
        return Support::Refused("directory entries that do not say what they name (no filetype)");
    }
    if r & RO_COMPAT_REFUSED != 0 {
        return Support::Refused(if r & ro_compat::BIGALLOC != 0 { "bigalloc" } else { "verity" });
    }
    if r & ro_compat::METADATA_CSUM != 0 && sb.checksum_type() != crate::sb::CHECKSUM_CRC32C {
        return Support::Refused("metadata checksums of a kind other than CRC32C");
    }
    if c & compat::HAS_JOURNAL != 0 && sb.journal_inum() == 0 {
        return Support::Refused("a journal on another device");
    }
    if c & compat::HAS_JOURNAL == 0 {
        return Support::ReadOnly("no journal, and this writes only through one");
    }
    if i & incompat::EXTENTS == 0 {
        return Support::ReadOnly("no extents (ext3): files mapped by block maps are not written");
    }
    if r & ro_compat::METADATA_CSUM == 0 {
        return Support::ReadOnly("no metadata_csum: this writes only with checksums");
    }
    if r & ro_compat::GDT_CSUM != 0 {
        return Support::ReadOnly("both gdt_csum and metadata_csum (e2fsck should drop one)");
    }
    if c & compat::ORPHAN_FILE != 0 || r & ro_compat::ORPHAN_PRESENT != 0 {
        return Support::ReadOnly("orphan_file, which this does not keep");
    }
    if c & !COMPAT_WRITABLE != 0 {
        return Support::ReadOnly("compat features this does not keep while writing");
    }
    if r & !RO_COMPAT_WRITABLE != 0 {
        return Support::ReadOnly(ro_compat_reason(r & !RO_COMPAT_WRITABLE));
    }
    Support::Writable
}

fn incompat_reason(bits: u32) -> &'static str {
    use incompat::*;
    match bits {
        b if b & COMPRESSION != 0 => "compression",
        b if b & META_BG != 0 => "meta_bg",
        b if b & MMP != 0 => "mmp",
        b if b & EA_INODE != 0 => "ea_inode",
        b if b & DIRDATA != 0 => "dirdata",
        b if b & LARGEDIR != 0 => "largedir",
        b if b & INLINE_DATA != 0 => "inline_data",
        b if b & ENCRYPT != 0 => "encryption",
        b if b & CASEFOLD != 0 => "casefold",
        _ => "incompatible features this does not know",
    }
}

fn ro_compat_reason(bits: u32) -> &'static str {
    use ro_compat::*;
    match bits {
        b if b & QUOTA != 0 => "quota",
        b if b & PROJECT != 0 => "project quotas",
        b if b & READONLY != 0 => "marked read-only",
        b if b & SHARED_BLOCKS != 0 => "shared blocks",
        b if b & HAS_SNAPSHOT != 0 => "snapshots",
        b if b & BTREE_DIR != 0 => "btree directories",
        b if b & REPLICA != 0 => "replicas",
        _ => "ro_compat features this does not keep",
    }
}
