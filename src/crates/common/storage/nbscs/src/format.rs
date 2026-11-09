//! A cs's device as it is written (plans/nbs.md, "The cs"): no filesystem,
//! a format of NBS's own.
//!
//!     slot 0     page 0, 1    the superblock, twice
//!                page 2..34   the volumes' fences, copy A
//!                page 34..66  the volumes' fences, copy B
//!     slot 1..   the table of slots, copy A, then copy B: a record a slot
//!     then       the slots, 4 MiB each, the chunks' data
//!
//! Every record -- a fence, a slot's -- is its own: its sequence number,
//! one more at each change, and a CRC32C seeded with the device's format id,
//! so that a record of an earlier format of the device, or of another
//! device, never passes. A record is changed in copy A, which is flushed,
//! then in copy B, flushed too: one copy of a page is ever being written, so
//! a write torn by a power cut leaves the other good, and the newer of the
//! two good copies is the record. A record of zeros in both copies was never
//! written: a slot free, no fence.

use crc::crc32c;

use crate::{PAGE, SLOT};

/// A slot's record.
pub const RECORD: usize = 64;
pub const RECORDS_PER_PAGE: usize = PAGE as usize / RECORD;
/// A volume's fence.
pub const FENCE: usize = 32;
pub const FENCES_PER_PAGE: usize = PAGE as usize / FENCE;
/// The fences a cs keeps: one a volume, as many as the metadata has.
pub const FENCES_MAX: usize = nbsmeta::VOLUMES_MAX;
pub const FENCE_PAGES: u64 = (FENCES_MAX / FENCES_PER_PAGE) as u64;
/// Where the superblock's two copies and the fences' are, in pages of slot 0.
pub const SUPER_PAGES: [u64; 2] = [0, 1];
pub const FENCE_AT: [u64; 2] = [2, 2 + FENCE_PAGES];
const _: () = assert!(FENCE_AT[1] + FENCE_PAGES <= SLOT / PAGE, "slot 0 holds the superblock and the fences");
const _: () = assert!(FENCES_MAX % FENCES_PER_PAGE == 0, "the fences fill whole pages");

/// "NBS-CS" and the format's version.
pub const MAGIC: [u8; 8] = *b"NBS-CS01";

/// Where a device's own and its slots are, from its size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The slots for chunks.
    pub slots: u32,
    /// The records each copy of the table has room for: one for every slot
    /// of the device, rounded up to a page's, so that copy B starts on a
    /// page of its own.
    pub records: u64,
    /// Where each copy of the table starts.
    pub table: [u64; 2],
    /// Where the first slot for chunks starts.
    pub data: u64,
}

/// A device of `bytes`, laid out: None for one too small for a slot of
/// chunks after its own.
pub fn layout(bytes: u64) -> Option<Layout> {
    let all = bytes / SLOT;
    let per_page = RECORDS_PER_PAGE as u64;
    let records = all.div_ceil(per_page) * per_page;
    let copy = records * RECORD as u64;
    let own = 1 + (2 * copy).div_ceil(SLOT);
    if all <= own {
        return None;
    }
    let slots = (all - own).min(u64::from(nbsmeta::SLOTS_MAX)) as u32;
    Some(Layout { slots, records, table: [SLOT, SLOT + copy], data: own * SLOT })
}

/// The slots for chunks a device of `bytes` holds: what the metadata
/// counts it as (`nbs devices add`), 0 for one too small.
pub fn device_slots(bytes: u64) -> u32 {
    layout(bytes).map_or(0, |l| l.slots)
}

/// Whose a device is: the NBS cluster, the cs, the device -- the cs's and
/// the device's numbers in the metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub cluster: [u8; 16],
    pub cs: u32,
    pub device: u32,
}

/// A record read: good, never written (zeros), or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decoded<T> {
    Valid(T),
    Empty,
    Bad,
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(w)
}

fn le64(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// A record's CRC: over the device's format id, then the record.
fn crc_of(format: u64, bytes: &[u8]) -> u32 {
    crc32c::update(crc32c::checksum(&format.to_le_bytes()), bytes)
}

/// The superblock: whose the device is, and how it was laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Super {
    pub identity: Identity,
    /// The device's sector at its format.
    pub sector: u32,
    pub slots: u32,
    pub records: u64,
    /// The format's id: 64 random bits, every record's CRC seeded with it.
    pub format: u64,
    pub seq: u64,
}

const SUPER_BYTES: usize = 128;
const SUPER_CRC: usize = SUPER_BYTES - 4;

impl Super {
    /// Into the first bytes of a page; the rest of it zeros.
    pub fn encode(&self, page: &mut [u8]) {
        page.fill(0);
        page[0..8].copy_from_slice(&MAGIC);
        put32(page, 8, self.sector);
        page[12..28].copy_from_slice(&self.identity.cluster);
        put32(page, 28, self.identity.cs);
        put32(page, 32, self.identity.device);
        put32(page, 36, self.slots);
        put64(page, 40, self.records);
        put64(page, 48, self.format);
        put64(page, 56, self.seq);
        let crc = crc32c::checksum(&page[..SUPER_CRC]);
        put32(page, SUPER_CRC, crc);
    }

    pub fn decode(page: &[u8]) -> Decoded<Super> {
        if page.len() < SUPER_BYTES {
            return Decoded::Bad;
        }
        if page[..SUPER_BYTES].iter().all(|&b| b == 0) {
            return Decoded::Empty;
        }
        if page[0..8] != MAGIC || crc32c::checksum(&page[..SUPER_CRC]) != le32(page, SUPER_CRC) {
            return Decoded::Bad;
        }
        let mut cluster = [0u8; 16];
        cluster.copy_from_slice(&page[12..28]);
        Decoded::Valid(Super {
            identity: Identity { cluster, cs: le32(page, 28), device: le32(page, 32) },
            sector: le32(page, 8),
            slots: le32(page, 36),
            records: le64(page, 40),
            format: le64(page, 48),
            seq: le64(page, 56),
        })
    }
}

/// What a device holds where a cs's superblock goes, its first two pages:
/// whether a cs formats it, opens it, or leaves it alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holds {
    /// Zeros: never formatted, or a format cut short -- for a cs to format.
    Nothing,
    /// A cs's superblock: the newer of its good copies.
    Nbs(Super),
    /// Neither: a filesystem's, a partition table's, or a superblock of
    /// NBS's both copies of which are damaged -- to be left alone.
    Other,
}

/// What pages `a` and `b` -- a device's first two -- hold.
///
/// A format writes zeros over both pages first, then a superblock into the
/// first bytes of A, then of B, and nothing past them: a page that has
/// anything past a superblock's bytes was never written by a cs. That is
/// what keeps a device with something else at its start -- a boot sector, a
/// filesystem's superblock at 1 KiB, a partition table's header -- from
/// being taken for blank where a superblock's own first bytes are zeros, or
/// for a format cut in copy A where B is.
pub fn holds(a: &[u8], b: &[u8]) -> Holds {
    let ours = |p: &[u8]| p.get(SUPER_BYTES..).is_some_and(|rest| rest.iter().all(|&x| x == 0));
    if !ours(a) || !ours(b) {
        return Holds::Other;
    }
    match choose(Super::decode(a), Super::decode(b), |s: &Super| s.seq) {
        Some(Decoded::Valid(s)) => Holds::Nbs(s),
        Some(Decoded::Empty) => Holds::Nothing,
        _ => Holds::Other,
    }
}

/// A volume's fence: the newest attach generation the cs has been told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fence {
    /// 0: no fence -- volumes are numbered from 1.
    pub volume: u32,
    pub generation: u64,
    pub seq: u64,
}

const FENCE_CRC: usize = FENCE - 4;

impl Fence {
    pub fn encode(&self, format: u64, out: &mut [u8]) {
        out[..FENCE].fill(0);
        put32(out, 0, self.volume);
        put64(out, 8, self.generation);
        put64(out, 16, self.seq);
        let crc = crc_of(format, &out[..FENCE_CRC]);
        put32(out, FENCE_CRC, crc);
    }

    pub fn decode(b: &[u8], format: u64) -> Decoded<Fence> {
        let b = &b[..FENCE];
        if b.iter().all(|&x| x == 0) {
            return Decoded::Empty;
        }
        if crc_of(format, &b[..FENCE_CRC]) != le32(b, FENCE_CRC) || le32(b, 4) != 0 {
            return Decoded::Bad;
        }
        Decoded::Valid(Fence { volume: le32(b, 0), generation: le64(b, 8), seq: le64(b, 16) })
    }
}

/// A chunk's epoch: the replica set it is in, and the set's version
/// (plans/nbs.md, "Replication: the rules").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch {
    pub set: u32,
    pub version: u32,
}

/// What a slot holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SlotState {
    /// Nothing: to be taken by a chunk's first write.
    #[default]
    Free,
    /// A chunk, closed: every replica flushed, the same on all.
    Written,
    /// A chunk being written by the record's generation: after a crash, a
    /// chunk whose replicas may differ.
    Open,
    /// Neither copy of its record good: never used, said (in memory only).
    Lost,
}

/// A slot's record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub state: SlotState,
    pub chunk: u32,
    pub incarnation: u32,
    /// The chunk's epoch as the cs knows it.
    pub known: Epoch,
    /// The epoch it was last written at.
    pub written: Epoch,
    /// The volume whose map names the chunk, as its last write said -- or
    /// the copy-up or the copy that took the slot (N10) -- and the newest
    /// generation written to it.
    pub volume: u32,
    pub generation: u64,
    pub seq: u64,
}

const RECORD_CRC: usize = RECORD - 4;

impl Record {
    pub fn encode(&self, format: u64, out: &mut [u8]) {
        out[..RECORD].fill(0);
        let state = match self.state {
            SlotState::Free | SlotState::Lost => 0,
            SlotState::Written => 1,
            SlotState::Open => 2,
        };
        put32(out, 0, self.chunk);
        put32(out, 4, self.incarnation);
        out[8] = state;
        put32(out, 12, self.known.set);
        put32(out, 16, self.known.version);
        put32(out, 20, self.written.set);
        put32(out, 24, self.written.version);
        put32(out, 28, self.volume);
        put64(out, 32, self.generation);
        put64(out, 40, self.seq);
        let crc = crc_of(format, &out[..RECORD_CRC]);
        put32(out, RECORD_CRC, crc);
    }

    pub fn decode(b: &[u8], format: u64) -> Decoded<Record> {
        let b = &b[..RECORD];
        if b.iter().all(|&x| x == 0) {
            return Decoded::Empty;
        }
        if crc_of(format, &b[..RECORD_CRC]) != le32(b, RECORD_CRC) || b[9..12] != [0, 0, 0]
            || b[48..RECORD_CRC].iter().any(|&x| x != 0)
        {
            return Decoded::Bad;
        }
        let state = match b[8] {
            0 => SlotState::Free,
            1 => SlotState::Written,
            2 => SlotState::Open,
            _ => return Decoded::Bad,
        };
        let r = Record {
            state,
            chunk: le32(b, 0),
            incarnation: le32(b, 4),
            known: Epoch { set: le32(b, 12), version: le32(b, 16) },
            written: Epoch { set: le32(b, 20), version: le32(b, 24) },
            volume: le32(b, 28),
            generation: le64(b, 32),
            seq: le64(b, 40),
        };
        /* A chunk's record names one, and its volume; a free one, none. Its
           generation is 0 where a join's copy took the chunk in and no
           writer has written it since (N9): found as bad, every chunk of a
           copy's was lost at the cs's next start (found by fuzz/nbs's join
           target) */
        let holds = state != SlotState::Free;
        if holds == (r.incarnation == 0) || (holds && r.volume == 0) {
            return Decoded::Bad;
        }
        Decoded::Valid(r)
    }
}

/// The good one of a record's two copies: the newer where both are -- one
/// copy torn by a power cut in its write, the other is the record. None
/// where neither is: the record lost.
///
/// Two good copies differ only while copy B of a change is not yet flushed:
/// a change not yet answered, which either copy may stand for. The newer is
/// taken, so that what was done is kept where it can be.
pub fn choose<T: Copy>(a: Decoded<T>, b: Decoded<T>, seq: impl Fn(&T) -> u64) -> Option<Decoded<T>> {
    match (a, b) {
        (Decoded::Valid(x), Decoded::Valid(y)) => Some(Decoded::Valid(if seq(&y) > seq(&x) { y } else { x })),
        (Decoded::Valid(x), _) | (_, Decoded::Valid(x)) => Some(Decoded::Valid(x)),
        (Decoded::Empty, Decoded::Empty) => Some(Decoded::Empty),
        /* The record's first write, cut in copy A: B is written only once A
           is durable, so it was never written -- the record never was.
           (Taken for lost before; found by fuzz/nbs's cs target) */
        (Decoded::Bad, Decoded::Empty) => Some(Decoded::Empty),
        /* Copy A never written, B neither good nor zeros: no tear of ours */
        _ => None,
    }
}
