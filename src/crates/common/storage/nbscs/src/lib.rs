//! NBS's chunk server, as its devices hold it (plans/nbs.md, "The cs"; step
//! N7): the format of a cs's devices (`format.rs`) and the engine that does
//! a cs's requests over them (`store.rs`) -- fences, open marks, reads,
//! writes, flushes, closes, the chunks freed -- each device the caller's
//! (`device.rs`): a device's queue in nbs-cs.ko, a disk that power cuts
//! lose and tear in fuzz/nbs's cs target.
//!
//! A chunk is held by its number and its incarnation (nbsmeta's): a number
//! is taken again once its chunk is freed, and a slot of the chunk it was
//! must never be written or read for the chunk it is. A request names the
//! chunk's epoch -- its replica set and the set's version -- which the cs
//! holds to the one it knows, or has checked with its host's mds.
//!
//! What a request may lean on is in `store.rs`'s head; what makes a record
//! survive a torn write, in `format.rs`'s.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod device;
mod exec;
mod format;
mod store;
mod turns;

pub use device::{Device, IoError};
pub use exec::now;
pub use format::{device_slots, holds, layout, Decoded, Epoch, Holds, Identity, Layout, Record, SlotState, Super,
                 FENCES_MAX};
pub use store::{ChunkRef, DeviceInfo, Error, Holding, OpenError, Repairs, Store};

/// A chunk's slot: 4 MiB, as the metadata's chunks are.
pub const SLOT: u64 = (nbsmeta::CHUNK_MIB as u64) << 20;
/// A page of the cs's own: what a superblock, a page of fences or of the
/// table of slots is written as.
pub const PAGE: u64 = 4096;
/// The smallest sector a device has.
pub const SECTOR: u64 = 512;
/// What a sum covers (N9): a joiner holds its copy to its source's by one
/// CRC-32C of each.
pub const GRANULE: u32 = 64 << 10;

/// A copy-up's piece (N10): a chunk read and written this much at a time,
/// a buffer the kernel's heap gives at once.
pub const COPY_PIECE: u64 = 256 << 10;

/// The sums of a chunk of zeros, one a `GRANULE`, as `Store::sums` and
/// `Store::scrub` give them: what a chunk a cs does not hold reads as -- a
/// slot is zeros before a chunk takes it -- for the leader's scrub to hold
/// it to the copies of members that do (N9e).
pub fn zero_sums() -> alloc::vec::Vec<u32> {
    let zeros = [0u8; SECTOR as usize];
    let mut sum = 0;
    for _ in 0..u64::from(GRANULE) / SECTOR {
        sum = crc::crc32c::update(sum, &zeros);
    }
    alloc::vec![sum; (SLOT / u64::from(GRANULE)) as usize]
}
