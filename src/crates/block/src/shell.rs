//! The shell's two raw-device commands: a sector read, and a sector write.
//!
//! They belong here rather than in the shell because everything they do is
//! this layer's -- a lookup by name, a claim, an I/O -- and the shell has no
//! view of a device to do it through.

use core::fmt::Write;

use crate::disk::{self as block, Disk};
use kcore::cmd::Output;
use kcore::dma::DmaBuffer;

/// What a hex dump shows, and what a write puts down: one sector as the
/// shell has always meant it, whatever the device's own sector size is.
const DUMP_BYTES: usize = 512;

/// What diskwrite's claim on its device says to whoever is refused it. NUL
/// terminated because the claim keeps the pointer, and whoever is refused
/// prints it.
const DISKWRITE_HOLDER: &core::ffi::CStr = c"diskwrite";

pub fn diskread(args: &str, out: &mut Output) {
    let mut tokens = args.split_whitespace();
    let (name, sector) = match (tokens.next(), tokens.next()) {
        (Some(name), Some(sector)) => (name, sector),
        _ => {
            let _ = writeln!(out, "usage: diskread <disk> <sector>");
            return;
        }
    };

    let sector: u64 = match sector.parse() {
        Ok(sector) => sector,
        Err(_) => {
            let _ = writeln!(out, "invalid sector number");
            return;
        }
    };

    let disk = match Disk::open(name) {
        Some(disk) => disk,
        None => {
            let _ = writeln!(out, "disk '{}' not found", name);
            return;
        }
    };

    /* From the page allocator and not the stack: the driver may DMA into
       this, and only memory the allocator tracks has a physical address it
       can find. */
    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => {
            let _ = writeln!(out, "alloc failed");
            return;
        }
    };

    if disk.read(sector, &mut buf.as_mut_slice()[..DUMP_BYTES]).is_err() {
        let _ = writeln!(out, "read error");
        return;
    }

    let data = &buf.as_slice()[..DUMP_BYTES];
    for (i, row) in data.chunks(16).enumerate() {
        let _ = write!(out, "{:X}: ", sector * DUMP_BYTES as u64 + (i * 16) as u64);
        for byte in row {
            let _ = write!(out, "{:X} ", byte);
        }
        let _ = writeln!(out);
    }
}

pub fn diskwrite(args: &str, out: &mut Output) {
    let mut tokens = args.split_whitespace();
    let (name, sector, hex) = match (tokens.next(), tokens.next(), tokens.next()) {
        (Some(name), Some(sector), Some(hex)) => (name, sector, hex),
        _ => {
            let _ = writeln!(out, "usage: diskwrite <disk> <sector> <hex>");
            return;
        }
    };

    let sector: u64 = match sector.parse() {
        Ok(sector) => sector,
        Err(_) => {
            let _ = writeln!(out, "usage: diskwrite <disk> <sector> <hex>");
            return;
        }
    };

    let disk = match Disk::open(name) {
        Some(disk) => disk,
        None => {
            let _ = writeln!(out, "disk '{}' not found", name);
            return;
        }
    };

    let mut buf = match DmaBuffer::new(1) {
        Some(buf) => buf,
        None => {
            let _ = writeln!(out, "alloc failed");
            return;
        }
    };

    let written = match hex_decode(hex, &mut buf.as_mut_slice()[..DUMP_BYTES]) {
        Some(written) => written,
        None => {
            let _ = writeln!(out, "invalid hex data");
            return;
        }
    };

    if written == 0 {
        return;
    }

    /* Not around a mounted filesystem, the disk log or a write test */
    let claim = match block::claim_as(disk.handle(), DISKWRITE_HOLDER) {
        Ok(claim) => claim,
        Err(held_by) => {
            let _ = writeln!(out, "disk '{}' is in use by {}", name, held_by);
            return;
        }
    };

    if disk.write(sector, &buf.as_slice()[..DUMP_BYTES], false).is_err() {
        let _ = writeln!(out, "write error");
    } else {
        let _ = writeln!(out, "wrote {} bytes to sector {}", written, sector);
    }

    block::release(claim);
}

/// Hex text into bytes, a pair at a time: how many were written, or None at
/// the first character that is not a hex digit. A trailing odd character is
/// left out, as is anything past the end of `out`.
fn hex_decode(hex: &str, out: &mut [u8]) -> Option<usize> {
    let hex = hex.as_bytes();
    let mut written = 0;
    let mut i = 0;
    while i + 1 < hex.len() && written < out.len() {
        let hi = nibble(hex[i])?;
        let lo = nibble(hex[i + 1])?;
        out[written] = (hi << 4) | lo;
        written += 1;
        i += 2;
    }
    Some(written)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
