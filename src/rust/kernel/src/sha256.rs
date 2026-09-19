//! The `sha256` shell command, which checks a downloaded kernel against its
//! release's SHA256SUMS. RustCrypto's sha2 is in the tree already for TLS
//! (and is pinned to its software backend in this crate's Cargo.toml).

use alloc::vec::Vec;
use core::fmt::Write;

use fs::vfs::{Open, OPEN_READ};
use kcore::cmd::{Command, Output};
use kcore::trace;
use sha2::{Digest, Sha256};

/// A file is hashed this much at a time.
const CHUNK_SIZE: usize = 64 * 1024;

const DIGEST_SIZE: usize = 32;

/// Put the command in front of whoever runs one. Called from `rust_init`.
pub fn init() {
    match Command::register(
        "sha256", "sha256 <path> - SHA-256 of a file, as sha256sum prints it", sha256,
    ) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => trace!(0, "sha256: cannot register the command"),
    }
}

fn sha256(args: &str, out: &mut Output) {
    let path = match args.split_whitespace().next() {
        Some(path) => path,
        None => {
            let _ = writeln!(out, "usage: sha256 <path>");
            return;
        }
    };

    let file = match fs::vfs_instance().and_then(|vfs| Open::new(vfs, path.as_bytes(), OPEN_READ)) {
        Some(file) => file,
        None => {
            let _ = writeln!(out, "open failed");
            return;
        }
    };

    let mut buf: Vec<u8> = Vec::new();
    if buf.try_reserve_exact(CHUNK_SIZE).is_err() {
        let _ = writeln!(out, "alloc failed");
        return;
    }
    buf.resize(CHUNK_SIZE, 0);

    let mut hash = Sha256::new();
    loop {
        match file.read(&mut buf) {
            Some(0) => break,
            Some(got) => hash.update(&buf[..got]),
            None => {
                let _ = writeln!(out, "read failed");
                return;
            }
        }
    }

    /* As sha256sum prints it, so a line of a release's SHA256SUMS compares
     * by eye */
    for byte in hash.finalize() {
        let _ = write!(out, "{:02x}", byte);
    }
    let _ = writeln!(out, "  {}", path);
}

/// FIPS 180-2's "abc" and the empty message. The command is what says a
/// downloaded kernel is the one its release lists, so this is checked
/// against the standard, not against itself. Run at boot, from `rust_test`;
/// a wrong digest stops the boot.
pub fn selftest() {
    const ABC_DIGEST: [u8; DIGEST_SIZE] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea,
        0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23,
        0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c,
        0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad,
    ];
    const EMPTY_DIGEST: [u8; DIGEST_SIZE] = [
        0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14,
        0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
        0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c,
        0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
    ];

    trace!(0, "sha256 selftest: started");

    let mut hash = Sha256::new();
    hash.update(b"abc");
    assert!(hash.finalize()[..] == ABC_DIGEST, "sha256: wrong digest for \"abc\"");

    assert!(Sha256::new().finalize()[..] == EMPTY_DIGEST, "sha256: wrong digest for the empty message");

    /* Fed in pieces, as the command feeds a file, the same digest */
    let mut hash = Sha256::new();
    hash.update(b"ab");
    hash.update(b"c");
    assert!(hash.finalize()[..] == ABC_DIGEST, "sha256: wrong digest for \"abc\" in pieces");

    trace!(0, "sha256 selftest: complete");
}
