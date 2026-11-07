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
mod format;
mod store;

pub use device::{Device, IoError};
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
