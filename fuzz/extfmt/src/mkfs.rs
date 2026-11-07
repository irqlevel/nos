//! `extfmt-test mkfs`: the filesystem extfmt's mkfs makes on a device,
//! written to a file for e2fsprogs to judge.
//!
//!     extfmt-test mkfs <ext2|ext4> <bytes> <image> [--bs N] [--label L]
//!                      [--sector N] [--garbage] [--seed N]
//!
//! makes the file `bytes` long and every write of the making into it. With
//! --garbage the file is filled with bytes of a seeded PRNG's first, so
//! that a filesystem that left anything to a device's former zeros fails
//! e2fsck. Prints the geometry it chose; exit 0 when made, 1 when mkfs
//! refused the device (its reason said), 2 for anything else.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};

use extfmt::mkfs::{Kind, Layout, Options, Piece};

/// A PRNG of the test's own: xorshift64*, the same bytes for a seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn fill(&mut self, b: &mut [u8]) {
        for chunk in b.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

const CHUNK: usize = 1 << 20;

pub fn run(args: &[String]) -> i32 {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let (Some(kind), Some(bytes), Some(path)) = (args.get(2), args.get(3), args.get(4)) else {
        eprintln!("usage: extfmt-test mkfs <ext2|ext4> <bytes> <image> [--bs N] [--label L] [--sector N] \
                   [--garbage] [--seed N]");
        return 2;
    };
    let kind = match kind.as_str() {
        "ext2" => Kind::Ext2,
        "ext4" => Kind::Ext4,
        other => {
            eprintln!("mkfs: no filesystem {}", other);
            return 2;
        }
    };
    let Ok(bytes) = bytes.parse::<u64>() else {
        eprintln!("mkfs: {} is no size", bytes);
        return 2;
    };
    let block_size = match flag("--bs").map(|b| b.parse::<u32>()) {
        None => None,
        Some(Ok(b)) => Some(b),
        Some(Err(_)) => {
            eprintln!("mkfs: --bs takes a number");
            return 2;
        }
    };
    let sector: u64 = flag("--sector").and_then(|s| s.parse().ok()).unwrap_or(512);
    let seed: u64 = flag("--seed").and_then(|s| s.parse().ok()).unwrap_or(1);
    let mut label = [0u8; 16];
    if let Some(l) = flag("--label") {
        let n = l.len().min(16);
        label[..n].copy_from_slice(&l.as_bytes()[..n]);
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let (mut uuid, mut hash_seed) = ([0u8; 16], [0u8; 16]);
    rng.fill(&mut uuid);
    rng.fill(&mut hash_seed);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let options = Options { kind, block_size, label, uuid, hash_seed, now: now as u32 };

    let layout = match Layout::new(bytes, sector, &options) {
        Ok(l) => l,
        Err(why) => {
            println!("refused: {}", why);
            return 1;
        }
    };

    let mut file = match OpenOptions::new().create(true).write(true).truncate(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("mkfs: {}: {}", path, e);
            return 2;
        }
    };
    if file.set_len(bytes).is_err() {
        eprintln!("mkfs: {}: cannot make it {} bytes", path, bytes);
        return 2;
    }
    if args.iter().any(|a| a == "--garbage") {
        let mut buf = vec![0u8; CHUNK];
        let mut at = 0u64;
        while at < bytes {
            let n = (bytes - at).min(CHUNK as u64) as usize;
            rng.fill(&mut buf[..n]);
            if file.write_all(&buf[..n]).is_err() {
                eprintln!("mkfs: {}: write failed", path);
                return 2;
            }
            at += n as u64;
        }
    }

    let zeros = vec![0u8; CHUNK];
    let (mut pieces, mut barriers, mut bad) = (0u64, 0u64, None);
    let made = layout.write(&mut |p| {
        pieces += 1;
        let ok = match p {
            Piece::Bytes { at, data } => {
                if at % sector != 0 || data.len() as u64 % sector != 0 {
                    bad = Some(format!("{} bytes at {}: not whole sectors", data.len(), at));
                }
                at + data.len() as u64 <= bytes
                    && file.seek(SeekFrom::Start(at)).is_ok()
                    && file.write_all(data).is_ok()
            }
            Piece::Zeros { at, len } => {
                if at % sector != 0 || len % sector != 0 {
                    bad = Some(format!("{} zeros at {}: not whole sectors", len, at));
                }
                let mut ok = at + len <= bytes && file.seek(SeekFrom::Start(at)).is_ok();
                let mut left = len;
                while ok && left > 0 {
                    let n = left.min(CHUNK as u64) as usize;
                    ok = file.write_all(&zeros[..n]).is_ok();
                    left -= n as u64;
                }
                ok
            }
            Piece::Barrier => {
                barriers += 1;
                true
            }
        };
        ok && bad.is_none()
    });
    if let Some(why) = bad {
        eprintln!("mkfs: a write of {}", why);
        return 2;
    }
    if let Err(why) = made {
        eprintln!("mkfs: {}", why);
        return 2;
    }
    println!("made: {:?}, {}-byte blocks, {} blocks in {} groups, {} inodes, journal {} blocks, {} free, \
              {} writes, {} barriers",
             layout.kind(), layout.block_size(), layout.blocks(), layout.groups(), layout.inodes(),
             layout.journal_blocks(), layout.free_blocks(), pieces, barriers);
    0
}
