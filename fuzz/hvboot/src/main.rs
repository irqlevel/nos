//! hvboot's parsers on the host, from their own sources: what hvboot would
//! boot off a disk image, read with its own partition, ext and GRUB code.
//!
//!     hvboot-test image <disk.raw> [--dump <dir>] [--entry <spec>]
//!
//! says which grub.cfg it found, the entry it would boot and why, the
//! kernel's header and the command line; with --dump, writes the kernel and
//! the initrd as its reader read them, to compare with e2fsprogs'.
//!
//!     hvboot-test fuzz <disk.raw> [--seed N] [--iterations N]
//!
//! reads the same image with bytes of its partition table, superblock,
//! group descriptors, inode tables, extent and directory blocks and of its
//! grub.cfg changed at random -- each run a different disk -- and holds the
//! code to never panicking, never reading outside the disk and always
//! ending: what a guest's disk says is the guest's to choose.

#![allow(dead_code)]

extern crate alloc;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[path = "../../../src/crates/hvboot/src/crc32.rs"]
mod crc32;
#[path = "../../../src/crates/hvboot/src/gpt.rs"]
mod gpt;
#[path = "../../../src/crates/hvboot/src/ext.rs"]
mod ext;
#[path = "../../../src/crates/hvboot/src/grub.rs"]
mod grub;
#[path = "../../../src/crates/hvboot/src/bzimage.rs"]
mod bzimage;

/// What bzimage.rs reaches of the start info: the memory map's entry.
mod start {
    #[derive(Clone, Copy, Debug)]
    pub struct MemEntry {
        pub addr: u64,
        pub size: u64,
        pub kind: u32,
    }
}

const SECTOR: u64 = 512;
const CONFIGS: [&str; 4] = ["/boot/grub/grub.cfg", "/grub/grub.cfg", "/boot/grub2/grub.cfg", "/grub2/grub.cfg"];

/// A disk: a file's bytes, or a copy of them with some changed, read by
/// whole sectors -- every read held to the disk's bounds, a read past them
/// a finding.
struct Disk {
    file: File,
    len: u64,
    /// Bytes the fuzzer changed: offset, value.
    patches: Vec<(u64, u8)>,
    reads: u64,
    /// The sectors of the reads of metadata's size -- not a kernel's
    /// megabytes -- when they are being noted.
    noted: Option<Vec<u64>>,
}

/// The most a read is that is noted: metadata comes a block or a few at a
/// time, a file's data in runs.
const NOTE_MAX: usize = 64 * 1024;

impl Disk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        assert!(offset % SECTOR == 0 && buf.len() as u64 % SECTOR == 0, "an unaligned read: {} at {}", buf.len(),
                offset);
        assert!(offset + buf.len() as u64 <= self.len, "a read past the disk: {} at {} of {}", buf.len(), offset,
                self.len);
        self.reads += 1;
        assert!(self.reads < 1_000_000, "a million reads: a loop that does not end");
        if let Some(noted) = self.noted.as_mut() {
            if buf.len() <= NOTE_MAX {
                noted.extend((0..buf.len() as u64 / SECTOR).map(|s| offset / SECTOR + s));
            }
        }
        let f = &mut self.file;
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        f.read_exact(buf).map_err(|e| e.to_string())?;
        for &(at, v) in &self.patches {
            if at >= offset && at < offset + buf.len() as u64 {
                buf[(at - offset) as usize] = v;
            }
        }
        Ok(())
    }
}

/// A partition of it, as hvboot's `disk::Part` is.
struct Part<'a> {
    disk: &'a mut Disk,
    first: u64,
    sectors: u64,
}

impl ext::Source for Part<'_> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        if offset % SECTOR != 0 || buf.len() as u64 % SECTOR != 0 {
            return Err(format!("a read of {} bytes at {}, not whole sectors", buf.len(), offset));
        }
        let sector = offset / SECTOR;
        let count = buf.len() as u64 / SECTOR;
        if sector.checked_add(count).is_none_or(|end| end > self.sectors) {
            return Err(format!("a read past the partition's end: sector {} of {}", sector + count, self.sectors));
        }
        self.disk.read((self.first + sector) * SECTOR, buf)
    }
}

struct Found {
    part: Option<u32>,
    first: u64,
    sectors: u64,
    uuid: String,
    label: String,
}

fn filesystems(disk: &mut Disk) -> Vec<Found> {
    let sectors = disk.len / SECTOR;
    let parts = {
        let mut read = |s: u64, buf: &mut [u8]| disk.read(s * SECTOR, buf);
        gpt::partitions(sectors, &mut read).unwrap_or_default()
    };
    let mut candidates: Vec<(Option<u32>, u64, u64)> = Vec::new();
    if parts.is_empty() {
        candidates.push((None, 0, sectors));
    }
    let mut ordered: Vec<&gpt::Partition> = parts.iter().filter(|p| !p.never_ext()).collect();
    ordered.sort_by_key(|p| if p.kind == gpt::Kind::Gpt(gpt::TYPE_XBOOTLDR) { 0 } else { 1 });
    candidates.extend(ordered.iter().map(|p| (Some(p.number), p.first, p.sectors)));
    let mut found = Vec::new();
    for (part, first, len) in candidates {
        if let Ok(fs) = ext::Fs::mount(Part { disk: &mut *disk, first, sectors: len }) {
            found.push(Found { part, first, sectors: len, uuid: fs.uuid(), label: fs.label() });
        }
    }
    found
}

/// What hvboot would boot: (where its grub.cfg is, the entry, why, the
/// kernel's and the initrds' bytes) -- or why it would not.
struct Boot {
    config: String,
    title: String,
    why: String,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    line: String,
}

fn boot(disk: &mut Disk, entry_spec: Option<&str>) -> Result<Boot, String> {
    let fss = filesystems(disk);
    let mut cfg_at = None;
    'find: for (i, f) in fss.iter().enumerate() {
        let mut fs = ext::Fs::mount(Part { disk: &mut *disk, first: f.first, sectors: f.sectors })?;
        for path in CONFIGS {
            let Ok(inode) = fs.lookup(path) else { continue };
            if !inode.is_file() {
                continue;
            }
            let text = fs.read_all(&inode, 1 << 20)?;
            let env_path = format!("{}env", path.trim_end_matches(".cfg"));
            let env = fs.lookup(&env_path).ok().filter(|e| e.is_file())
                .and_then(|e| fs.read_all(&e, 65536).ok()).unwrap_or_default();
            cfg_at = Some((i, path, String::from_utf8_lossy(&text).into_owned(),
                           String::from_utf8_lossy(&env).into_owned()));
            break 'find;
        }
    }
    let Some((at, path, text, env_text)) = cfg_at else { return Err(String::from("no grub.cfg")) };
    let cfg = grub::parse(&text);
    let env = grub::parse_env(&env_text);
    let (entry, why) = grub::choose(&cfg, &env, entry_spec)?;
    let mut root = at;
    for r in &entry.roots {
        let hit = match r {
            grub::Root::FsUuid(u) => fss.iter().position(|f| f.uuid.eq_ignore_ascii_case(u)),
            grub::Root::Label(l) => fss.iter().position(|f| &f.label == l),
            grub::Root::Device { part, .. } => fss.iter().position(|f| f.part == *part),
            grub::Root::File(_) => None,
        };
        if let Some(i) = hit {
            root = i;
        }
    }
    let kernel_path = entry.kernel.clone().ok_or("no linux line")?;
    let f = &fss[root];
    let mut fs = ext::Fs::mount(Part { disk: &mut *disk, first: f.first, sectors: f.sectors })?;
    let k = fs.lookup(&kernel_path)?;
    let kernel = fs.read_all(&k, 512 << 20)?;
    let mut initrd = Vec::new();
    for p in &entry.initrds {
        let i = fs.lookup(p)?;
        while initrd.len() % 4 != 0 {
            initrd.push(0);
        }
        initrd.extend(fs.read_all(&i, 2 << 30)?);
    }
    let header = bzimage::Header::parse(&kernel)?;
    let mut line = format!("BOOT_IMAGE={}", kernel_path);
    for a in &entry.args {
        line.push(' ');
        line.push_str(a);
    }
    let mut page = [0u8; bzimage::ZERO_PAGE_BYTES];
    let map = [start::MemEntry { addr: 0, size: 0x9FC00, kind: 1 }, start::MemEntry { addr: 1 << 20, size: 1 << 30, kind: 1 }];
    bzimage::zero_page(&kernel, &header, 0x2000, line.len(), (0x3000_0000, initrd.len() as u64), &map, 0xE0000,
                       &mut page)?;
    let name = match fss[at].part { Some(p) => format!("part {}", p), None => String::from("whole disk") };
    Ok(Boot { config: format!("{} {}", name, path), title: entry.title.clone(), why, kernel, initrd, line })
}

fn open(path: &str) -> Disk {
    let file = File::open(path).unwrap_or_else(|e| panic!("{}: {}", path, e));
    let len = file.metadata().map(|m| m.len()).unwrap_or(0) / SECTOR * SECTOR;
    Disk { file, len, patches: Vec::new(), reads: 0, noted: None }
}

/// A seeded generator: xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// The sectors a clean run reads as metadata -- partition tables,
/// superblocks, descriptors, inodes, extent and directory blocks, grub.cfg
/// -- where the fuzzer's changes go, so that each lands on something the
/// code parses.
fn sectors_read(path: &str) -> Vec<u64> {
    let mut disk = open(path);
    disk.noted = Some(Vec::new());
    let _ = boot(&mut disk, None);
    let mut seen = disk.noted.take().unwrap_or_default();
    seen.sort_unstable();
    seen.dedup();
    seen
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    match args.get(1).map(String::as_str) {
        Some("image") => {
            let path = args.get(2).expect("hvboot-test image <disk.raw>");
            let mut disk = open(path);
            match boot(&mut disk, flag("--entry").as_deref()) {
                Ok(b) => {
                    println!("grub.cfg: {}", b.config);
                    println!("entry: \"{}\", {}", b.title, b.why);
                    println!("kernel: {} bytes; initrd: {} bytes", b.kernel.len(), b.initrd.len());
                    println!("command line: {}", b.line);
                    println!("reads: {}", disk.reads);
                    if let Some(dir) = flag("--dump") {
                        std::fs::write(format!("{}/kernel", dir), &b.kernel).expect("write the kernel");
                        std::fs::write(format!("{}/initrd", dir), &b.initrd).expect("write the initrd");
                    }
                }
                Err(e) => {
                    println!("hvboot would not boot it: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Some("fuzz") => {
            let path = args.get(2).expect("hvboot-test fuzz <disk.raw>");
            let seed: u64 = flag("--seed").and_then(|s| s.parse().ok()).unwrap_or(1);
            let iterations: u64 = flag("--iterations").and_then(|s| s.parse().ok()).unwrap_or(2000);
            let targets = sectors_read(path);
            assert!(!targets.is_empty(), "a clean run read nothing to change");
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut booted = 0u64;
            for i in 0..iterations {
                let mut disk = open(path);
                for _ in 0..1 + rng.below(8) {
                    let sector = targets[rng.below(targets.len() as u64) as usize];
                    let at = sector * SECTOR + rng.below(SECTOR);
                    let v = match rng.below(4) { 0 => 0, 1 => 0xFF, _ => rng.next() as u8 };
                    disk.patches.push((at, v));
                }
                let patches = disk.patches.clone();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| boot(&mut disk, None).is_ok()));
                match r {
                    Ok(true) => booted += 1,
                    Ok(false) => {}
                    Err(_) => {
                        println!("FINDING: seed {} iteration {}: a panic, with the bytes {:?} changed", seed, i,
                                 patches);
                        std::process::exit(1);
                    }
                }
            }
            println!("hvboot-test fuzz: {} disks, {} still booted, {} metadata sectors changed among, no finding",
                     iterations, booted, targets.len());
        }
        _ => {
            eprintln!("hvboot-test image <disk.raw> [--dump <dir>] [--entry <spec>] | fuzz <disk.raw> [--seed N] \
                       [--iterations N]");
            std::process::exit(2);
        }
    }
}
