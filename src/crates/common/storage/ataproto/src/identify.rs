//! IDENTIFY DEVICE's 256 words, read for what the driver needs of them.
//!
//! Every number here is the disk's word for itself, and nothing is taken
//! before it has been held to what a disk can be: a capacity of none, or past
//! what 48 bits address, a sector smaller than 512 bytes or larger than a
//! page, or not a power of two, is a refusal -- not a disk registered with a
//! size the block layer would then believe.

/// The words IDENTIFY DEVICE answers with: 512 bytes, one sector.
pub const WORDS: usize = 256;

/// The sector every ATA disk had before a disk could say otherwise.
pub const SECTOR_BYTES: u32 = 512;

/* Where each thing is (ATA8-ACS, 7.16) */
const W_GENERAL: usize = 0;
const W_SERIAL: usize = 10;
const SERIAL_WORDS: usize = 10;
const W_FIRMWARE: usize = 23;
const FIRMWARE_WORDS: usize = 4;
const W_MODEL: usize = 27;
const MODEL_WORDS: usize = 20;
const W_CAPABILITIES: usize = 49;
const W_LBA28_SECTORS: usize = 60;
const W_QUEUE_DEPTH: usize = 75;
const W_SATA_CAPABILITIES: usize = 76;
const W_COMMANDS1_SUPPORTED: usize = 82;
const W_COMMANDS2_SUPPORTED: usize = 83;
const W_COMMANDS3_SUPPORTED: usize = 84;
const W_COMMANDS1_ENABLED: usize = 85;
const W_LBA48_SECTORS: usize = 100;
const W_SECTOR_SIZES: usize = 106;
const W_LOGICAL_SECTOR_WORDS: usize = 117;
const W_DSM: usize = 169;
const W_ROTATION: usize = 217;

/// Word 0 bit 15: an ATAPI device (a CD drive), which IDENTIFY DEVICE is
/// not the command for and this driver does not take.
const GENERAL_NOT_ATA: u16 = 1 << 15;
/// Word 49 bit 9: LBA addressing, which every disk worth taking has.
const CAP_LBA: u16 = 1 << 9;
/// Words 83, 84, 87 and 106 are valid when bit 14 is set and bit 15 clear.
/// Words 82 and 85 carry no such mark -- their bit 14 says the disk has the
/// NOP command, which many have not (Seagate's ST2000VX015) -- and are valid
/// when word 83 is: what Linux takes (`ata_id_has_wcache`), where ACS-3
/// vouches for 85 by 87. Word 82's own bits taken for its mark said "none"
/// of such a disk's write cache, present and on.
const VALID_MASK: u16 = 0xC000;
const VALID: u16 = 0x4000;
/// Word 82/85 bit 5: a volatile write cache, supported / enabled.
const CMD1_WRITE_CACHE: u16 = 1 << 5;
/// Word 83 bit 10: 48-bit addressing; bit 12: FLUSH CACHE; bit 13: FLUSH
/// CACHE EXT.
const CMD2_LBA48: u16 = 1 << 10;
const CMD2_FLUSH: u16 = 1 << 12;
const CMD2_FLUSH_EXT: u16 = 1 << 13;
/// Word 106 bit 12: the logical sector is longer than 256 words (words
/// 117-118 say how long); bit 13: several logical sectors to a physical one,
/// two to the power of bits 0-3 of them.
const SIZES_LONG_LOGICAL: u16 = 1 << 12;
const SIZES_MULTIPLE_LOGICAL: u16 = 1 << 13;
const SIZES_LOG2_PER_PHYSICAL: u16 = 0x000F;
/// Word 75 bits 0-4: the most commands the disk queues, less one.
const QUEUE_DEPTH_MASK: u16 = 0x1F;
/// Word 76 bit 8: NCQ -- READ and WRITE FPDMA QUEUED. A word 76 of 0 or of
/// all ones is a disk that says nothing of SATA's capabilities.
const SATA_NCQ: u16 = 1 << 8;
/// Word 84 bit 5: the General Purpose Logging feature set -- READ LOG EXT,
/// which is how a disk names the queued command that failed.
const CMD3_GPL: u16 = 1 << 5;
/// Word 84 bit 6: writes past the cache by the command's own FUA bit --
/// WRITE DMA FUA EXT, and WRITE FPDMA QUEUED's.
const CMD3_FUA: u16 = 1 << 6;
/// Word 169 bit 0: DATA SET MANAGEMENT's TRIM.
const DSM_TRIM: u16 = 1 << 0;
/// Word 217: 1 is a disk that does not spin; 0x0401 to 0xFFFE its speed.
const ROTATION_NONE: u16 = 1;
const ROTATION_RPM_MIN: u16 = 0x0401;
const ROTATION_RPM_MAX: u16 = 0xFFFE;

/// The most a 48-bit address reaches.
const LBA48_LIMIT: u64 = 1 << 48;
/// A physical sector this driver will believe: up to 64 KiB.
const PHYSICAL_MAX: u32 = 64 * 1024;

/// What a disk says it does to make its writes durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flush {
    /// No flush command: a disk that keeps no cache, or one too old to say.
    None,
    /// FLUSH CACHE, 28-bit.
    Flush,
    /// FLUSH CACHE EXT.
    FlushExt,
}

/// Whether the disk spins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    NotReported,
    /// A disk with nothing that turns: an SSD.
    Solid,
    /// So many turns a minute.
    Rpm(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// An ATAPI device, not a disk.
    NotAta,
    /// No LBA addressing.
    NoLba,
    /// A capacity of none, or past 48 bits.
    Capacity(u64),
    /// A logical sector of this many bytes.
    SectorSize(u64),
}

/// What the driver takes from the words.
#[derive(Clone, Debug)]
pub struct Identify {
    pub sectors: u64,
    pub lba48: bool,
    /// The logical sector, in bytes: what an LBA counts.
    pub sector_size: u32,
    /// The physical one: a write of less, or not aligned to it, is a
    /// read-modify-write inside the disk (512e).
    pub physical_sector_size: u32,
    pub flush: Flush,
    /// None when the disk says nothing of a write cache.
    pub write_cache: Option<bool>,
    pub trim: bool,
    /// How many commands the disk queues (NCQ), when it queues any.
    pub ncq_depth: Option<u8>,
    /// READ LOG EXT: the NCQ Command Error log's way in.
    pub gpl: bool,
    /// A write's FUA bit honoured: durable when it ends.
    pub fua: bool,
    pub rotation: Rotation,
    pub model: [u8; MODEL_WORDS * 2],
    pub serial: [u8; SERIAL_WORDS * 2],
    pub firmware: [u8; FIRMWARE_WORDS * 2],
}

/// Two characters a word, the first in its high byte.
fn string<const N: usize>(words: &[u16; WORDS], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    for (i, pair) in out.chunks_exact_mut(2).enumerate() {
        let w = words.get(at + i).copied().unwrap_or(0);
        pair.copy_from_slice(&w.to_be_bytes());
    }
    out
}

fn valid(word: u16) -> bool {
    word & VALID_MASK == VALID
}

impl Identify {
    pub fn parse(words: &[u16; WORDS], page_size: usize) -> Result<Identify, Refusal> {
        if words[W_GENERAL] & GENERAL_NOT_ATA != 0 {
            return Err(Refusal::NotAta);
        }
        if words[W_CAPABILITIES] & CAP_LBA == 0 {
            return Err(Refusal::NoLba);
        }

        let cmd1 = words[W_COMMANDS1_SUPPORTED];
        let cmd2 = words[W_COMMANDS2_SUPPORTED];
        let enabled1 = words[W_COMMANDS1_ENABLED];
        let cmd2_valid = valid(cmd2);

        let lba48 = cmd2_valid && cmd2 & CMD2_LBA48 != 0;
        let lba28 = u64::from(words[W_LBA28_SECTORS]) | u64::from(words[W_LBA28_SECTORS + 1]) << 16;
        let sectors48 = (0..4).fold(0u64, |acc, i| acc | u64::from(words[W_LBA48_SECTORS + i]) << (16 * i));
        let sectors = if lba48 && sectors48 != 0 { sectors48 } else { lba28 };
        if sectors == 0 || sectors >= LBA48_LIMIT {
            return Err(Refusal::Capacity(sectors));
        }

        let sizes = words[W_SECTOR_SIZES];
        let mut logical = u64::from(SECTOR_BYTES);
        let mut per_physical_log2 = 0u32;
        if valid(sizes) {
            if sizes & SIZES_LONG_LOGICAL != 0 {
                let words_per = u64::from(words[W_LOGICAL_SECTOR_WORDS])
                    | u64::from(words[W_LOGICAL_SECTOR_WORDS + 1]) << 16;
                logical = words_per * 2;
            }
            if sizes & SIZES_MULTIPLE_LOGICAL != 0 {
                per_physical_log2 = u32::from(sizes & SIZES_LOG2_PER_PHYSICAL);
            }
        }
        if logical < u64::from(SECTOR_BYTES) || logical > page_size as u64 || !logical.is_power_of_two() {
            return Err(Refusal::SectorSize(logical));
        }
        let sector_size = logical as u32;
        let physical_sector_size = sector_size
            .checked_shl(per_physical_log2)
            .filter(|&p| p <= PHYSICAL_MAX)
            .unwrap_or(sector_size);

        let flush = if cmd2_valid && cmd2 & CMD2_FLUSH_EXT != 0 && lba48 {
            Flush::FlushExt
        } else if cmd2_valid && cmd2 & CMD2_FLUSH != 0 {
            Flush::Flush
        } else {
            Flush::None
        };

        let write_cache = (cmd2_valid && cmd1 & CMD1_WRITE_CACHE != 0)
            .then_some(enabled1 & CMD1_WRITE_CACHE != 0);

        let sata = words[W_SATA_CAPABILITIES];
        let ncq_depth = (sata != 0 && sata != 0xFFFF && sata & SATA_NCQ != 0)
            .then(|| (words[W_QUEUE_DEPTH] & QUEUE_DEPTH_MASK) as u8 + 1);
        let cmd3 = words[W_COMMANDS3_SUPPORTED];
        let gpl = valid(cmd3) && cmd3 & CMD3_GPL != 0;
        let fua = valid(cmd3) && cmd3 & CMD3_FUA != 0 && lba48;

        let rotation = match words[W_ROTATION] {
            ROTATION_NONE => Rotation::Solid,
            rpm @ ROTATION_RPM_MIN..=ROTATION_RPM_MAX => Rotation::Rpm(rpm),
            _ => Rotation::NotReported,
        };

        Ok(Identify {
            sectors,
            lba48,
            sector_size,
            physical_sector_size,
            flush,
            write_cache,
            trim: words[W_DSM] & DSM_TRIM != 0,
            ncq_depth,
            gpl,
            fua,
            rotation,
            model: string(words, W_MODEL),
            serial: string(words, W_SERIAL),
            firmware: string(words, W_FIRMWARE),
        })
    }
}

/// `bytes` without the spaces and NULs a string field is padded with.
pub fn trimmed(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|&c| c != b' ' && c != 0).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|&c| c != b' ' && c != 0).map_or(start, |i| i + 1);
    &bytes[start..end.max(start)]
}

/// The bytes as text for a log line: what is not printable ASCII as `?`.
pub fn printable(bytes: &[u8], out: &mut [u8]) -> usize {
    let n = bytes.len().min(out.len());
    for (o, &b) in out.iter_mut().zip(bytes.iter()).take(n) {
        *o = if (0x20..0x7F).contains(&b) { b } else { b'?' };
    }
    n
}

/// What a disk is known by from one boot to the next: `ata-<model>_<serial>`,
/// as Linux's `/dev/disk/by-id` names it -- each trimmed of its padding and
/// anything but `[A-Za-z0-9._-]` made `_`. None when either is empty.
pub fn identity_word(model: &[u8], serial: &[u8]) -> Option<alloc::string::String> {
    let (model, serial) = (trimmed(model), trimmed(serial));
    if model.is_empty() || serial.is_empty() {
        return None;
    }
    let mut word = alloc::string::String::new();
    word.try_reserve_exact(4 + model.len() + 1 + serial.len()).ok()?;
    word.push_str("ata-");
    let keep = |c: u8| if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-') { char::from(c) } else { '_' };
    word.extend(model.iter().map(|&c| keep(c)));
    word.push('_');
    word.extend(serial.iter().map(|&c| keep(c)));
    Some(word)
}
