//! What is on a disk, as the formats have it: each made from what the input
//! chooses, and read back independently of the kernel -- an image the
//! kernel wrote is judged by what its format says, not by the driver that
//! wrote it.

pub mod disklog;
pub mod ext2;
pub mod nanofs;
pub mod part;
