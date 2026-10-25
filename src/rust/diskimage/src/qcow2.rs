//! QEMU's qcow2 (docs/interop/qcow2.txt in QEMU's tree), what a cloud
//! ships its images in -- Ubuntu's, Debian's genericcloud, Fedora's --
//! read and written out as the raw disk it describes.
//!
//! What is read: versions 2 and 3, any cluster size QEMU makes (512 bytes
//! to 2 MiB), and every kind of cluster -- stored whole, compressed by
//! zlib, all zeros, or not there -- and extended L2 entries, whose 32
//! subclusters are each stored, zeros or not there. What is refused, saying
//! what to do instead: a backing file (the image is an overlay on another),
//! encryption, an external data file, zstd compression, an image QEMU
//! marked corrupt, and features this does not know. Internal snapshots are
//! left out: the disk is the image as it is now.
//!
//! What is never read is the refcounts -- what of the file is in use, not
//! what the disk holds -- and the snapshot table: the L1 table, the L2
//! tables it names and the clusters they name are all the disk is. They are
//! read as QEMU reads them -- the same rules, the same bits ignored, and
//! past the file's end zeros -- with one difference, on purpose: a table or
//! a cluster that starts past the file's end is refused, where QEMU reads
//! zeros, since what has that is a download cut short, and a disk of zeros
//! where its data should be is a failure no one would hear of.

use alloc::vec::Vec;
use core::fmt;

use crate::inflate::{self, Inflater, Stop};
use crate::sparse::Sparse;
use crate::{buffer, Sink, Source, CHUNK};

pub const MAGIC: [u8; 4] = *b"QFI\xfb";
/// A version 2 header's bytes; a version 3 one has at least V3_HEADER.
const V2_HEADER: usize = 72;
const V3_HEADER: usize = 104;
/// What of the file the header is read from: a version 3 header, its
/// compression type and the padding after it.
const HEADER_READ: usize = 112;
/// Where a version 3 header keeps its compression type, when it is long
/// enough to.
const COMPRESSION_TYPE_AT: usize = 104;
const MIN_CLUSTER_BITS: u32 = 9;
const MAX_CLUSTER_BITS: u32 = 21;
/// Extended L2 entries want clusters of at least 16 KiB (QEMU's rule).
const MIN_EXTENDED_CLUSTER_BITS: u32 = 14;
/// QEMU's bound on the L1 table: 32 MiB of entries.
const MAX_L1_ENTRIES: u64 = 32 * 1024 * 1024 / 8;
/// The largest disk this converts: the tables' offsets are 56 bits.
const MAX_SIZE: u64 = 1 << 56;
/// The longest backing file name QEMU reads, and this quotes.
const MAX_BACKING_NAME: usize = 1023;
/// What of an L1 or a standard L2 entry is an offset: bits 9 to 55.
const OFFSET_MASK: u64 = 0x00FF_FFFF_FFFF_FE00;
/// An L2 entry's flags: a compressed cluster; and, in version 3 without
/// extended entries, a cluster of zeros.
const COMPRESSED: u64 = 1 << 62;
const ZERO: u64 = 1;
/// A compressed cluster's data is counted in sectors of this size.
const SECTOR: u64 = 512;
/// The incompatible features (version 3).
const DIRTY: u64 = 1 << 0;
const CORRUPT: u64 = 1 << 1;
const DATA_FILE: u64 = 1 << 2;
const COMPRESSION: u64 = 1 << 3;
const EXTENDED_L2: u64 = 1 << 4;
const KNOWN_FEATURES: u64 = DIRTY | CORRUPT | DATA_FILE | COMPRESSION | EXTENDED_L2;
const COMPRESSION_ZLIB: u8 = 0;
const COMPRESSION_ZSTD: u8 = 1;
const CRYPT_NONE: u32 = 0;
const CRYPT_AES: u32 = 1;
const CRYPT_LUKS: u32 = 2;
/// A cluster's subclusters, with extended L2 entries.
const SUBCLUSTERS: u64 = 32;
/// An L2 entry's bytes: 8, or 16 with extended entries -- the entry, then
/// its subclusters' bitmap: which are stored, low half, and zeros, high.
const ENTRY: usize = 8;
const EXTENDED_ENTRY: usize = 16;

/// What is wrong with an image, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bad {
    Short { len: u64 },
    Magic,
    Version(u32),
    ClusterBits(u32),
    HeaderLength(u32),
    Features(u64),
    Corrupt,
    DataFile,
    Zstd,
    CompressionType(u8),
    CompressionBit,
    ExtendedSmall(u32),
    Encrypted(u32),
    Backing { name: Vec<u8> },
    Size(u64),
    L1Size { entries: u32, needed: u64 },
    L1Offset(u64),
    L2Offset { index: u64, offset: u64 },
    L2PastEnd { index: u64, offset: u64 },
    ClusterOffset { guest: u64, offset: u64 },
    ClusterPastEnd { guest: u64, offset: u64 },
    ZeroInV2 { guest: u64 },
    Subclusters { guest: u64 },
    Compressed { guest: u64, offset: u64, fault: inflate::Fault },
    CompressedPastEnd { guest: u64, offset: u64 },
    CompressedCut { guest: u64, offset: u64 },
}

const WHOLE: &str = "past the image's end -- is the image whole?";

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Bad::Short { len } => write!(f, "{} bytes, too short for a qcow2 header", len),
            Bad::Magic => f.write_str("not a qcow2 image"),
            Bad::Version(1) => f.write_str("QEMU's old qcow, version 1, not qcow2"),
            Bad::Version(v) => write!(f, "qcow2 version {}, where there are versions 2 and 3", v),
            Bad::ClusterBits(b) => write!(f, "clusters of 2^{} bytes, where qcow2's are 512 bytes to 2 MiB", b),
            Bad::HeaderLength(len) => write!(f, "a header of {} bytes, which a version 3 one is not", len),
            Bad::Features(bits) => write!(f, "incompatible features this does not know ({:#x})", bits),
            Bad::Corrupt => f.write_str("marked corrupt by QEMU -- qemu-img check -r all it first"),
            Bad::DataFile => f.write_str("its data is in an external file, not in the image"),
            Bad::Zstd => f.write_str("compressed by zstd, which this does not decode -- only zlib: qemu-img convert \
                                      -O qcow2 it, with -c and -o compression_type=zlib or without"),
            Bad::CompressionType(t) => write!(f, "compression type {}, which qcow2 does not have", t),
            Bad::CompressionBit => f.write_str("its compression type and the feature bit for it disagree"),
            Bad::ExtendedSmall(b) => write!(f, "extended L2 entries with clusters of 2^{} bytes, under 16 KiB", b),
            Bad::Encrypted(m) => {
                let how = match *m {
                    CRYPT_AES => "AES",
                    CRYPT_LUKS => "LUKS",
                    _ => "a method qcow2 does not have",
                };
                write!(f, "encrypted ({}), and nos has no key -- qemu-img convert it to a plain image first", how)
            }
            Bad::Backing { name } => {
                f.write_str("an overlay on a backing file it does not contain (\"")?;
                for &c in name {
                    if c.is_ascii_graphic() || c == b' ' {
                        write!(f, "{}", c as char)?;
                    } else {
                        write!(f, "\\x{:02x}", c)?;
                    }
                }
                f.write_str("\") -- convert the image it was made from, or qemu-img convert the two into one")
            }
            Bad::Size(0) => f.write_str("a disk of 0 bytes"),
            Bad::Size(s) => write!(f, "a disk of {} bytes, larger than this converts", s),
            Bad::L1Size { entries, needed } => {
                write!(f, "an L1 table of {} entries, for a disk that needs {}", entries, needed)?;
                if u64::from(*entries) > MAX_L1_ENTRIES {
                    write!(f, " -- and more than QEMU's {}", MAX_L1_ENTRIES)?;
                }
                Ok(())
            }
            Bad::L1Offset(o) => write!(f, "its L1 table at {:#x}, which is not a table's place", o),
            Bad::L2Offset { index, offset } => {
                write!(f, "L1 entry {}: an L2 table at {:#x}, off a cluster's start", index, offset)
            }
            Bad::L2PastEnd { index, offset } => write!(f, "L1 entry {}: an L2 table at {:#x}, {}", index, offset, WHOLE),
            Bad::ClusterOffset { guest, offset } => {
                write!(f, "the disk at {:#x}: a cluster at {:#x}, off a cluster's start", guest, offset)
            }
            Bad::ClusterPastEnd { guest, offset } => write!(f, "the disk at {:#x}: a cluster at {:#x}, {}", guest, offset, WHOLE),
            Bad::ZeroInV2 { guest } => write!(f, "the disk at {:#x}: a zero cluster, which version 2 does not have", guest),
            Bad::Subclusters { guest } => write!(f, "the disk at {:#x}: subclusters both stored and zeros", guest),
            Bad::Compressed { guest, offset, fault } => {
                write!(f, "the disk at {:#x}: the compressed cluster at {:#x}: {}", guest, offset, fault)
            }
            Bad::CompressedPastEnd { guest, offset } => {
                write!(f, "the disk at {:#x}: a compressed cluster at {:#x}, {}", guest, offset, WHOLE)
            }
            Bad::CompressedCut { guest, offset } => {
                write!(f, "the disk at {:#x}: a compressed cluster at {:#x} that does not decode, running {}", guest,
                       offset, WHOLE)
            }
        }
    }
}

/// Why an image was not read or converted.
#[derive(Debug)]
pub enum Error<E> {
    /// The image is not one this converts, or is damaged.
    Image(Bad),
    /// Reading the image failed.
    Read(E),
    /// Writing the disk failed.
    Write(E),
    NoMemory,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Image(bad) => bad.fmt(f),
            Error::Read(e) => write!(f, "reading the image failed: {}", e),
            Error::Write(e) => write!(f, "writing the disk failed: {}", e),
            Error::NoMemory => f.write_str("out of memory"),
        }
    }
}

fn bad<E>(b: Bad) -> Error<E> {
    Error::Image(b)
}

/// What was in the image, cluster by cluster, and what of it was written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes written to the disk: every block of it that is not all zeros.
    pub written: u64,
    /// Clusters stored whole -- or, with extended entries, any of whose
    /// subclusters are -- compressed, all zeros, and not there: a hole, in
    /// an image with no backing file.
    pub stored: u64,
    pub compressed: u64,
    pub zeros: u64,
    pub holes: u64,
}

/// An image, its header read and checked: what `convert` needs of it.
#[derive(Clone, Debug)]
pub struct Qcow2 {
    version: u32,
    cluster_bits: u32,
    size: u64,
    l1_offset: u64,
    /// The L1 entries the disk's size needs: the rest are not read.
    l1_needed: u64,
    extended: bool,
    dirty: bool,
    file_len: u64,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be64(b: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&b[at..at + 8]);
    u64::from_be_bytes(word)
}

/// `buf.len()` bytes from `offset`: what of them the file has, and zeros
/// for what is past its end -- as QEMU reads a file.
fn read_filled<S: Source>(src: &mut S, file_len: u64, offset: u64, buf: &mut [u8]) -> Result<(), S::Error> {
    let inside = if offset >= file_len { 0 } else { (file_len - offset).min(buf.len() as u64) as usize };
    if inside > 0 {
        src.read_at(offset, &mut buf[..inside])?;
    }
    buf[inside..].fill(0);
    Ok(())
}

impl Qcow2 {
    /// The image `src` holds, its header checked -- or what it is instead.
    pub fn open<S: Source>(src: &mut S) -> Result<Qcow2, Error<S::Error>> {
        let file_len = src.len();
        let mut head = [0u8; HEADER_READ];
        let n = file_len.min(HEADER_READ as u64) as usize;
        src.read_at(0, &mut head[..n]).map_err(Error::Read)?;
        if n < MAGIC.len() || head[..MAGIC.len()] != MAGIC {
            return Err(bad(Bad::Magic));
        }
        if n < V2_HEADER {
            return Err(bad(Bad::Short { len: file_len }));
        }
        let version = be32(&head, 4);
        if version != 2 && version != 3 {
            return Err(bad(Bad::Version(version)));
        }
        let backing_offset = be64(&head, 8);
        let backing_size = be32(&head, 16);
        let cluster_bits = be32(&head, 20);
        let size = be64(&head, 24);
        let crypt = be32(&head, 32);
        let l1_size = be32(&head, 36);
        let l1_offset = be64(&head, 40);
        if !(MIN_CLUSTER_BITS..=MAX_CLUSTER_BITS).contains(&cluster_bits) {
            return Err(bad(Bad::ClusterBits(cluster_bits)));
        }
        let cluster = 1u64 << cluster_bits;

        let mut incompatible = 0;
        let mut compression = COMPRESSION_ZLIB;
        if version == 3 {
            if n < V3_HEADER {
                return Err(bad(Bad::Short { len: file_len }));
            }
            incompatible = be64(&head, 72);
            let header_length = be32(&head, 100);
            if (header_length as usize) < V3_HEADER || u64::from(header_length) > cluster {
                return Err(bad(Bad::HeaderLength(header_length)));
            }
            /* Past the file's end it reads as 0, zlib's, as QEMU reads it. */
            if header_length as usize > COMPRESSION_TYPE_AT {
                compression = head[COMPRESSION_TYPE_AT];
            }
        }
        if incompatible & !KNOWN_FEATURES != 0 {
            return Err(bad(Bad::Features(incompatible & !KNOWN_FEATURES)));
        }
        if incompatible & CORRUPT != 0 {
            return Err(bad(Bad::Corrupt));
        }
        if incompatible & DATA_FILE != 0 {
            return Err(bad(Bad::DataFile));
        }
        match (compression, incompatible & COMPRESSION != 0) {
            (COMPRESSION_ZLIB, false) => {}
            (COMPRESSION_ZSTD, true) => return Err(bad(Bad::Zstd)),
            (COMPRESSION_ZLIB, true) | (COMPRESSION_ZSTD, false) => return Err(bad(Bad::CompressionBit)),
            (other, _) => return Err(bad(Bad::CompressionType(other))),
        }
        let extended = incompatible & EXTENDED_L2 != 0;
        if extended && cluster_bits < MIN_EXTENDED_CLUSTER_BITS {
            return Err(bad(Bad::ExtendedSmall(cluster_bits)));
        }
        if crypt != CRYPT_NONE {
            return Err(bad(Bad::Encrypted(crypt)));
        }
        if backing_size != 0 {
            /* Its name, for the message: as much of it as the file has. */
            let len = (backing_size as usize).min(MAX_BACKING_NAME);
            let mut name = Vec::new();
            if backing_offset.checked_add(len as u64).is_some_and(|end| end <= file_len)
                && name.try_reserve_exact(len).is_ok()
            {
                name.resize(len, 0);
                src.read_at(backing_offset, &mut name).map_err(Error::Read)?;
            }
            return Err(bad(Bad::Backing { name }));
        }
        if size == 0 || size > MAX_SIZE {
            return Err(bad(Bad::Size(size)));
        }

        let entry = if extended { EXTENDED_ENTRY } else { ENTRY } as u64;
        let span = cluster / entry * cluster;
        let needed = size.div_ceil(span);
        if u64::from(l1_size) < needed || u64::from(l1_size) > MAX_L1_ENTRIES {
            return Err(bad(Bad::L1Size { entries: l1_size, needed }));
        }
        if l1_offset % cluster != 0 || l1_offset.checked_add(u64::from(l1_size) * 8).is_none() {
            return Err(bad(Bad::L1Offset(l1_offset)));
        }
        Ok(Qcow2 {
            version,
            cluster_bits,
            size,
            l1_offset,
            l1_needed: needed,
            extended,
            dirty: incompatible & DIRTY != 0,
            file_len,
        })
    }

    /// The disk's size in bytes: what the raw disk is to be.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn cluster_size(&self) -> u64 {
        1 << self.cluster_bits
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    /// Whether QEMU left it without closing it cleanly: its refcounts may be
    /// wrong, which a conversion does not read.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Whether its L2 entries are extended, with subclusters.
    pub fn extended_l2(&self) -> bool {
        self.extended
    }

    /// The disk, written to `disk` -- which reads as zeros wherever it is
    /// not written: only what is not zeros is -- a piece at a time and in
    /// order, `progress` told how far it has got after every cluster.
    /// What was in the image, or why it could not be read: the disk is
    /// then only partly written, and the caller's to throw away.
    pub fn convert<S, D>(&self, src: &mut S, disk: &mut D, progress: &mut dyn FnMut(u64))
                         -> Result<Stats, Error<S::Error>>
    where
        S: Source,
        D: Sink<Error = S::Error>,
    {
        let cluster = self.cluster_size();
        let entry = if self.extended { EXTENDED_ENTRY } else { ENTRY };
        let span = cluster / entry as u64 * cluster;
        /* An L2 table, or a cluster, is read a piece at a time: its whole,
         * up to CHUNK. */
        let piece = (CHUNK as u64).min(cluster) as usize;
        let (Some(mut l1), Some(mut table), Some(mut data), Some(mut inflater), Some(mut out)) =
            (buffer(CHUNK), buffer(piece), buffer(piece), Inflater::new(), Sparse::new(disk, self.size))
        else {
            return Err(Error::NoMemory);
        };
        let mut stats = Stats::default();

        let mut index = 0u64;
        while index < self.l1_needed {
            let n = (self.l1_needed - index).min((CHUNK / 8) as u64) as usize;
            /* Within the bounds `open` checked; past the file's end, zeros. */
            read_filled(src, self.file_len, self.l1_offset + index * 8, &mut l1[..n * 8]).map_err(Error::Read)?;
            for k in 0..n {
                let i = index + k as u64;
                let base = i * span;
                let mapped = span.min(self.size - base);
                let clusters = mapped.div_ceil(cluster);
                let l2 = be64(&l1, k * 8) & OFFSET_MASK;
                if l2 == 0 {
                    stats.holes += clusters;
                    progress(base + mapped);
                    continue;
                }
                if l2 % cluster != 0 {
                    return Err(bad(Bad::L2Offset { index: i, offset: l2 }));
                }
                if l2 + cluster > self.file_len {
                    return Err(bad(Bad::L2PastEnd { index: i, offset: l2 }));
                }

                let per_piece = (piece / entry) as u64;
                let mut j = 0u64;
                while j < clusters {
                    let m = (clusters - j).min(per_piece);
                    src.read_at(l2 + j * entry as u64, &mut table[..m as usize * entry]).map_err(Error::Read)?;
                    for t in 0..m as usize {
                        let guest = base + (j + t as u64) * cluster;
                        let len = cluster.min(self.size - guest);
                        let word = be64(&table, t * entry);
                        let bitmap = if self.extended { be64(&table, t * entry + ENTRY) } else { 0 };
                        let at = Cluster { guest, len, word, bitmap };
                        self.cluster(src, &mut out, &mut inflater, &mut data, at, &mut stats)?;
                        progress(guest + len);
                    }
                    j += m;
                }
            }
            index += n as u64;
        }
        out.flush().map_err(Error::Write)?;
        stats.written = out.written();
        Ok(stats)
    }

    /// One cluster of the disk, as its L2 entry says it is.
    fn cluster<S, D>(&self, src: &mut S, out: &mut Sparse<D>, inflater: &mut Inflater, data: &mut [u8], at: Cluster,
                     stats: &mut Stats) -> Result<(), Error<S::Error>>
    where
        S: Source,
        D: Sink<Error = S::Error>,
    {
        let cluster = self.cluster_size();
        let Cluster { guest, len, word, bitmap } = at;

        if word & COMPRESSED != 0 {
            /* The data's offset, in bytes, and how many sectors more than
             * the one it starts in it takes; its bitmap, extended, is
             * reserved, and not read. */
            let shift = 62 - (self.cluster_bits - 8);
            let offset = word & ((1u64 << shift) - 1);
            let sectors = ((word >> shift) & ((1u64 << (self.cluster_bits - 8)) - 1)) + 1;
            let bytes = sectors * SECTOR - offset % SECTOR;
            if offset >= self.file_len {
                return Err(bad(Bad::CompressedPastEnd { guest, offset }));
            }
            let mut input = Packed {
                src,
                file_len: self.file_len,
                next: offset,
                end: offset + bytes,
                buf: data,
                pos: 0,
                filled: 0,
            };
            /* What comes out past the disk's end -- a last cluster the size
             * cuts short -- is not the disk's. */
            let mut done = 0u64;
            let mut emit = |piece: &[u8]| {
                let start = done;
                done += piece.len() as u64;
                if start >= len {
                    return Ok(());
                }
                let visible = (len - start).min(piece.len() as u64) as usize;
                out.put(guest + start, &piece[..visible])
            };
            return match inflater.inflate(&mut input, cluster, &mut emit) {
                Ok(()) => {
                    stats.compressed += 1;
                    Ok(())
                }
                /* Data that runs on past the file's last sector -- where an
                 * image's last compressed cluster may end -- and does not
                 * decode is an image cut short, and said to be. */
                Err(Stop::Fault(_)) if offset + bytes > self.file_len.next_multiple_of(SECTOR) => {
                    Err(bad(Bad::CompressedCut { guest, offset }))
                }
                Err(Stop::Fault(fault)) => Err(bad(Bad::Compressed { guest, offset, fault })),
                Err(Stop::Read(e)) => Err(Error::Read(e)),
                Err(Stop::Write(e)) => Err(Error::Write(e)),
            };
        }

        let offset = word & OFFSET_MASK;
        if !self.extended {
            if word & ZERO != 0 {
                if self.version < 3 {
                    return Err(bad(Bad::ZeroInV2 { guest }));
                }
                if offset % cluster != 0 {
                    return Err(bad(Bad::ClusterOffset { guest, offset }));
                }
                stats.zeros += 1;
                return Ok(());
            }
            if offset == 0 {
                stats.holes += 1;
                return Ok(());
            }
            if offset % cluster != 0 {
                return Err(bad(Bad::ClusterOffset { guest, offset }));
            }
            self.copy(src, out, data, guest, offset, len)?;
            stats.stored += 1;
            return Ok(());
        }

        /* Extended: each of 32 subclusters stored, zeros, or not there. */
        let stored = bitmap as u32;
        let zeros = (bitmap >> 32) as u32;
        if offset == 0 {
            if stored != 0 {
                return Err(bad(Bad::Subclusters { guest }));
            }
            if zeros != 0 {
                stats.zeros += 1;
            } else {
                stats.holes += 1;
            }
            return Ok(());
        }
        if stored & zeros != 0 {
            return Err(bad(Bad::Subclusters { guest }));
        }
        if offset % cluster != 0 {
            return Err(bad(Bad::ClusterOffset { guest, offset }));
        }
        let sub = cluster / SUBCLUSTERS;
        for s in 0..SUBCLUSTERS {
            let from = s * sub;
            if from >= len {
                break;
            }
            if stored & (1 << s) != 0 {
                self.copy(src, out, data, guest + from, offset + from, sub.min(len - from))?;
            }
        }
        if stored != 0 {
            stats.stored += 1;
        } else if zeros != 0 {
            stats.zeros += 1;
        } else {
            stats.holes += 1;
        }
        Ok(())
    }

    /// `len` bytes of the disk at `guest`, stored at `offset`: what of
    /// them the file has, and zeros past its end -- unless they start
    /// there.
    fn copy<S, D>(&self, src: &mut S, out: &mut Sparse<D>, data: &mut [u8], guest: u64, offset: u64, len: u64)
                  -> Result<(), Error<S::Error>>
    where
        S: Source,
        D: Sink<Error = S::Error>,
    {
        if offset >= self.file_len {
            return Err(bad(Bad::ClusterPastEnd { guest, offset }));
        }
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(data.len() as u64) as usize;
            read_filled(src, self.file_len, offset + done, &mut data[..n]).map_err(Error::Read)?;
            out.put(guest + done, &data[..n]).map_err(Error::Write)?;
            done += n as u64;
        }
        Ok(())
    }
}

/// A cluster of the disk: where it is, how much of it the disk has -- the
/// last may be cut short by the disk's size -- and its L2 entry.
#[derive(Clone, Copy)]
struct Cluster {
    guest: u64,
    len: u64,
    word: u64,
    bitmap: u64,
}

/// A compressed cluster's data, as the inflater reads it: from the image a
/// piece at a time, and past the file's end as zeros -- as QEMU reads it --
/// up to the end its L2 entry gives.
struct Packed<'a, S: Source> {
    src: &'a mut S,
    file_len: u64,
    /// Where the next piece is read from, and where the data ends.
    next: u64,
    end: u64,
    buf: &'a mut [u8],
    pos: usize,
    filled: usize,
}

impl<S: Source> inflate::Input for Packed<'_, S> {
    type Error = S::Error;

    #[inline]
    fn byte(&mut self) -> Result<Option<u8>, S::Error> {
        if self.pos == self.filled {
            if self.next >= self.end {
                return Ok(None);
            }
            let n = (self.end - self.next).min(self.buf.len() as u64) as usize;
            read_filled(self.src, self.file_len, self.next, &mut self.buf[..n])?;
            self.next += n as u64;
            self.pos = 0;
            self.filled = n;
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(b))
    }
}
