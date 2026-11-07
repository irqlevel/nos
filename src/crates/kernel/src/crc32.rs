//! The `crc32` shell command, which checks a file against its copy on the
//! host (`python3 -c "import zlib,sys; print(hex(zlib.crc32(open(sys.argv[1],'rb').read())))" file`),
//! over the tree's one CRC-32 (the `crc` crate) -- the CRC the partition
//! tables, nanofs and the disk log are checked by.

use core::fmt::Write;

use fs::vfs::{Open, OPEN_READ};
use kcore::cmd::{Command, Output};
use kcore::klog;

/// A file is summed this much at a time.
const CHUNK_SIZE: usize = 64 * 1024;

/// Put the command in front of whoever runs one. Called from `rust_init`.
pub fn init() {
    match Command::register("crc32", "crc32 <path> - CRC-32 of a file", crc32) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => klog!(0, "crc32: cannot register the command"),
    }
}

fn crc32(args: &str, out: &mut Output) {
    let Some(path) = args.split_whitespace().next() else {
        let _ = writeln!(out, "usage: crc32 <path>");
        return;
    };
    let Some(file) = fs::vfs_instance().and_then(|vfs| Open::new(vfs, path.as_bytes(), OPEN_READ)) else {
        let _ = writeln!(out, "open failed");
        return;
    };
    let Some(mut buf) = fs::files::buffer(CHUNK_SIZE) else {
        let _ = writeln!(out, "alloc failed");
        return;
    };

    let (mut crc, mut total) = (0u32, 0u64);
    loop {
        match file.read(&mut buf) {
            Some(0) => break,
            Some(got) => {
                crc = crc::crc32::update(crc, &buf[..got]);
                total += got as u64;
            }
            None => {
                let _ = writeln!(out, "read failed");
                return;
            }
        }
    }
    /* As the command has always said it: scripts/ext4-test.py reads it */
    let _ = writeln!(out, "{}: crc32 0x{:X}, {} bytes", path, crc, total);
}

/// Each CRC's check value -- what every implementation of it gives for
/// "123456789" -- as the kernel's build of the crate computes it, whole and
/// fed in pieces as the command feeds a file. The crate is held to far more
/// on the host (scripts/crc-test.py); this holds the kernel's own build of
/// it, both architectures. Run at boot, from `rust_test`; a wrong value
/// stops the boot.
pub fn selftest() {
    const CHECK: &[u8] = b"123456789";

    klog!(0, "crc32 selftest: started");

    assert!(crc::crc32::checksum(CHECK) == 0xCBF4_3926, "crc32: wrong check value");
    assert!(crc::crc32c::checksum(CHECK) == 0xE306_9283, "crc32c: wrong check value");
    /* CRC-16/ARC: the register from 0, with no inversion -- raw */
    assert!(crc::crc16::raw(0, CHECK) == 0xBB3D, "crc16: wrong check value");
    assert!(crc::crc32::checksum(&[]) == 0, "crc32: the CRC of nothing");

    /* In pieces of every size, the same */
    for piece in 1..=CHECK.len() {
        let pieces = CHECK.chunks(piece).fold(0, |c, p| crc::crc32::update(c, p));
        assert!(pieces == 0xCBF4_3926, "crc32: a different value in pieces");
        let pieces = CHECK.chunks(piece).fold(0, |c, p| crc::crc32c::update(c, p));
        assert!(pieces == 0xE306_9283, "crc32c: a different value in pieces");
    }

    klog!(0, "crc32 selftest: complete");
}
