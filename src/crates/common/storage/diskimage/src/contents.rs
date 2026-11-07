//! What a disk or a partition holds, from its first bytes and its last: a
//! filesystem, a partition table, a member of a RAID or LVM, an encrypted
//! volume -- so that a device is not given to a guest over what is on it
//! unless whoever gives it says so (hv's `force`).
//!
//! It reads what each kind keeps at a fixed place: its magic, and where
//! there is one its label. A device with none of them but bytes that are not
//! zeros in its first [`HEAD`] holds something all the same, and says so;
//! one whose first [`HEAD`] are all zeros holds nothing a later look would
//! find. Nothing here trusts a length the bytes give: a label is cut where
//! the kind's field ends.

use alloc::string::String;

/// How much of a device's start [`contents`] looks at: past btrfs's
/// superblock at 64 KiB, and swap's last place for its signature.
pub const HEAD: usize = 72 * 1024;
/// And of its end: where an md array's 0.90 and 1.0 superblocks are.
pub const TAIL: usize = 128 * 1024;

/// What is on a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// What it is, in a few words: "an ext4 filesystem".
    pub what: &'static str,
    /// Its label, where the kind has one and it is set.
    pub label: Option<String>,
}

impl core::fmt::Display for Found {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.label {
            Some(label) => write!(f, "{} labelled \"{}\"", self.what, label),
            None => f.write_str(self.what),
        }
    }
}

/* ext2/3/4: the superblock at 1 KiB, its magic, features and label. */
const EXT_SB: usize = 1024;
const EXT_MAGIC_AT: usize = EXT_SB + 56;
const EXT_COMPAT_AT: usize = EXT_SB + 92;
const EXT_INCOMPAT_AT: usize = EXT_SB + 96;
const EXT_RO_COMPAT_AT: usize = EXT_SB + 100;
const EXT_LABEL_AT: usize = EXT_SB + 120;
const EXT_COMPAT_HAS_JOURNAL: u32 = 0x4;
const EXT_INCOMPAT_JOURNAL_DEV: u32 = 0x8;
/* What only ext4 has: extents, 64-bit, flex_bg, a journal checksum's
 * metadata_csum, huge files, dir_nlink, extra_isize. */
const EXT4_INCOMPAT: u32 = 0x40 | 0x80 | 0x200 | 0x400 | 0x10000;
const EXT4_RO_COMPAT: u32 = 0x8 | 0x20 | 0x40 | 0x400;

/* btrfs: its superblock at 64 KiB, the magic 64 bytes in, the label at 299. */
const BTRFS_SB: usize = 65536;

/* swap: the signature in the last 10 bytes of its first page, whatever the
 * page size of the machine that made it; a version 2 label at 1 KiB + 28. */
const SWAP_PAGES: [usize; 3] = [4096, 16384, 65536];
const SWAP_LABEL_AT: usize = 1024 + 28;

/* md RAID: the superblock's magic, and where each version keeps it. */
const MD_MAGIC: u32 = 0xa92b_4efc;
const MD_V12_AT: usize = 4096;

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn at(b: &[u8], offset: usize, magic: &[u8]) -> bool {
    b.get(offset..offset + magic.len()) == Some(magic)
}

/// A label from a field of `len` bytes at `offset`: up to its first NUL,
/// spaces trimmed, None when that leaves nothing -- or bytes that are not
/// text, shown as `?`.
fn label(b: &[u8], offset: usize, len: usize) -> Option<String> {
    let field = b.get(offset..offset + len)?;
    let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
    let field = &field[..end];
    let first = field.iter().position(|&c| c != b' ')?;
    let last = field.iter().rposition(|&c| c != b' ')?;
    let mut text = String::new();
    /* Nothing grows past this: a byte a character, every one of them ASCII. */
    text.try_reserve_exact(last + 1 - first).ok()?;
    text.extend(field[first..=last].iter().map(|&c| if (0x20..0x7f).contains(&c) { char::from(c) } else { '?' }));
    Some(text)
}

fn found(what: &'static str, label: Option<String>) -> Option<Found> {
    Some(Found { what, label })
}

/// What `head` -- a device's first bytes, [`HEAD`] of them or the whole of
/// a smaller device -- and `tail` -- its last, [`TAIL`] of them or as many
/// as there are -- of a device `size` bytes long say it holds; None for a
/// device whose start is all zeros and has nothing at its end.
pub fn contents(head: &[u8], tail: &[u8], size: u64) -> Option<Found> {
    let h = head;
    if at(h, 0, b"LUKS\xba\xbe") {
        return found("a LUKS volume", None);
    }
    if at(h, 0, b"XFSB") {
        return found("an XFS filesystem", label(h, 108, 12));
    }
    if at(h, 0, b"hsqs") {
        return found("a squashfs", None);
    }
    if at(h, 0, b"NOSLOG1\0") {
        return found("nos's disk log area", None);
    }
    if le32(h, 0) == Some(0x4E41_4E4F) {
        return found("a nanofs filesystem", None);
    }
    if le16(h, EXT_MAGIC_AT) == Some(0xEF53) {
        let compat = le32(h, EXT_COMPAT_AT).unwrap_or(0);
        let incompat = le32(h, EXT_INCOMPAT_AT).unwrap_or(0);
        let ro_compat = le32(h, EXT_RO_COMPAT_AT).unwrap_or(0);
        let what = if incompat & EXT_INCOMPAT_JOURNAL_DEV != 0 {
            "an ext3/4 journal device"
        } else if incompat & EXT4_INCOMPAT != 0 || ro_compat & EXT4_RO_COMPAT != 0 {
            "an ext4 filesystem"
        } else if compat & EXT_COMPAT_HAS_JOURNAL != 0 {
            "an ext3 filesystem"
        } else {
            "an ext2 filesystem"
        };
        return found(what, label(h, EXT_LABEL_AT, 16));
    }
    if le32(h, EXT_SB) == Some(0xF2F5_2010) {
        return found("an F2FS filesystem", None);
    }
    if at(h, BTRFS_SB + 64, b"_BHRfS_M") {
        return found("a btrfs filesystem", label(h, BTRFS_SB + 299, 256));
    }
    for page in SWAP_PAGES {
        if at(h, page - 10, b"SWAPSPACE2") {
            return found("swap", label(h, SWAP_LABEL_AT, 16));
        }
        if at(h, page - 10, b"SWAP-SPACE") {
            return found("swap", None);
        }
    }
    for sector in 0..4 {
        if at(h, sector * 512, b"LABELONE") && at(h, sector * 512 + 24, b"LVM2 001") {
            return found("an LVM physical volume", None);
        }
    }
    if le32(h, 0) == Some(MD_MAGIC) || le32(h, MD_V12_AT) == Some(MD_MAGIC) || md_at_end(tail, size) {
        return found("a member of an md RAID array", None);
    }
    if at(h, 0x8001, b"CD001") {
        return found("an ISO 9660 filesystem", label(h, 0x8028, 32));
    }
    if at(h, 3, b"NTFS    ") {
        return found("an NTFS filesystem", None);
    }
    if at(h, 3, b"EXFAT   ") {
        return found("an exFAT filesystem", None);
    }
    let boot_signature = at(h, 510, &[0x55, 0xAA]);
    if boot_signature && at(h, 82, b"FAT32") {
        return found("a FAT filesystem", label(h, 71, 11));
    }
    if boot_signature && at(h, 54, b"FAT") {
        return found("a FAT filesystem", label(h, 43, 11));
    }
    if at(h, 512, b"EFI PART") || at(h, 4096, b"EFI PART") {
        return found("a GPT partition table", None);
    }
    if boot_signature && (0..4).any(|i| h.get(446 + i * 16 + 4).is_some_and(|&t| t != 0)) {
        return found("an MBR partition table", None);
    }
    if h.iter().any(|&b| b != 0) {
        return found("data nos does not recognize", None);
    }
    None
}

/// Whether an md superblock of version 0.90 or 1.0 is where it would be at
/// the end of a device `size` bytes long whose last bytes `tail` are.
fn md_at_end(tail: &[u8], size: u64) -> bool {
    let start = size.saturating_sub(tail.len() as u64);
    let magic_at = |offset: u64| -> bool {
        if offset < start {
            return false;
        }
        let Ok(i) = usize::try_from(offset - start) else { return false };
        /* 0.90 keeps it in the order of the machine that made the array. */
        le32(tail, i) == Some(MD_MAGIC) || tail.get(i..i + 4) == Some(&MD_MAGIC.to_be_bytes())
    };
    /* 0.90: 64 KiB from the end, on a 64 KiB boundary; 1.0: 8 KiB from the
     * end, on a 4 KiB boundary. */
    let v090 = (size & !0xFFFF).checked_sub(0x10000);
    let v10 = size.checked_sub(8192).map(|o| o & !0xFFF);
    v090.is_some_and(magic_at) || v10.is_some_and(magic_at)
}
