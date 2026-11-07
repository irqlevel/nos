//! The two checksums of the format: CRC32C (Castagnoli), which
//! metadata_csum -- and the journal -- put on everything, and the CRC16
//! that gdt_csum, metadata_csum's predecessor, puts on group descriptors.
//!
//! Both as the kernel computes them (`ext4_chksum`, lib/crc16.c): the
//! running value in and out, with no inversion at either end, so that one
//! call carries on where another stopped and a checksum over pieces --
//! an inode's number, then its bytes -- is a chain of calls. The usual
//! CRC32C of a message is `!crc32c(!0, message)`. Each is the `raw` form of
//! the tree's one (the `crc` crate), under the name the format has it.

pub use ::crc::crc16::raw as crc16;
pub use ::crc::crc32c::raw as crc32c;
