//! The disk image converter on the host, from its own sources
//! (src/rust/diskimage), as `hv convert` runs it (scripts/diskimage-test.py
//! drives this):
//!
//!     diskimage-test convert <image> <disk.raw>
//!
//! converts as `hv convert` does -- the disk a new file, grown to the
//! image's size with a hole and written where the image is not zeros, and
//! removed again if the conversion fails -- and says what the image held:
//! `ok size=N cluster=N version=N written=N stored=N compressed=N zeros=N
//! holes=N`, or `refused: <why>` (exit 2). Every read is held to the image's
//! bounds, and every write to the disk's and to coming in order, here as
//! well as by the converter itself.
//!
//!     diskimage-test inflate <cases> <answers>
//!
//! decodes each raw DEFLATE stream of <cases> -- records of a u32 limit, a
//! u32 length and that many bytes, little-endian -- asking for `limit`
//! bytes, as QEMU asks for a cluster, and writes a line an answer: `ok
//! <crc32 of the output>` or `fault <what>`, for the script to hold to
//! zlib's.
//!
//!     diskimage-test iso <out.iso> <label> <name>=<file>...
//!
//! makes the ISO 9660 image `hv create` gives cloud-init (`diskimage::iso`)
//! of those files, for the script to read back with xorriso, bsdtar, blkid
//! and Linux's isofs.
//!
//!     diskimage-test fuzz <image> [--seed N] [--iterations N] [--slow-ms N]
//!
//! converts the image with bytes of its header, its tables and its clusters
//! changed at random, or cut short -- each run a different image -- into a
//! disk that checks every write: a panic, a read outside the image, a write
//! outside the disk or out of order, and a run slower than --slow-ms are
//! findings, said with the seed and the iteration that make them again.

use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::FileExt;
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use diskimage::inflate::{Inflater, Input, Stop};
use diskimage::qcow2::{Error, Qcow2, Stats};
use diskimage::{detect, Format, Sink, Source};

/* ---- the image and the disk ---- */

/// The image: a file of the host's, or bytes in memory -- every read held
/// to its bounds, as the converter promises.
trait Bytes {
    fn length(&self) -> u64;
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
}

struct Image<B: Bytes>(B);

impl<B: Bytes> Source for Image<B> {
    type Error = io::Error;

    fn len(&self) -> u64 {
        self.0.length()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let len = self.0.length();
        assert!(offset.checked_add(buf.len() as u64).is_some_and(|end| end <= len),
                "a read outside the image: {} bytes at {:#x}, of {}", buf.len(), offset, len);
        self.0.read(offset, buf)
    }
}

struct HostFile {
    file: File,
    len: u64,
}

impl Bytes for HostFile {
    fn length(&self) -> u64 {
        self.len
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }
}

struct Memory<'a>(&'a [u8]);

impl Bytes for Memory<'_> {
    fn length(&self) -> u64 {
        self.0.len() as u64
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let at = offset as usize;
        buf.copy_from_slice(&self.0[at..at + buf.len()]);
        Ok(())
    }
}

/// The disk: every write held to coming after the last, and inside it.
struct Disk<W: FnMut(u64, &[u8]) -> io::Result<()>> {
    size: u64,
    end: u64,
    written: u64,
    write: W,
}

impl<W: FnMut(u64, &[u8]) -> io::Result<()>> Sink for Disk<W> {
    type Error = io::Error;

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        assert!(!data.is_empty() && offset >= self.end
                && offset.checked_add(data.len() as u64).is_some_and(|end| end <= self.size),
                "a write outside the disk or out of order: {} bytes at {:#x}, after {:#x}, of {}",
                data.len(), offset, self.end, self.size);
        self.end = offset + data.len() as u64;
        self.written += data.len() as u64;
        (self.write)(offset, data)
    }
}

/// Converts `image` into `disk`'s writer, the progress held to never going
/// back and never past the disk.
fn run<B: Bytes, W: FnMut(u64, &[u8]) -> io::Result<()>>(img: &Qcow2, image: &mut Image<B>, disk: &mut Disk<W>)
                                                          -> Result<Stats, Error<io::Error>> {
    let size = img.size();
    let mut last = 0;
    let stats = img.convert(image, disk, &mut |at| {
        assert!(at >= last && at <= size, "progress from {:#x} to {:#x}, of {:#x}", last, at, size);
        last = at;
    })?;
    assert!(last == size, "the conversion ended at {:#x} of {:#x}", last, size);
    assert!(stats.written == disk.written, "{} bytes said written, {} were", stats.written, disk.written);
    Ok(stats)
}

/* ---- convert ---- */

fn convert(from: &str, to: &str) -> i32 {
    let file = match File::open(from) {
        Ok(file) => file,
        Err(e) => {
            println!("error: {}: {}", from, e);
            return 3;
        }
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            println!("error: {}: {}", from, e);
            return 3;
        }
    };
    let mut image = Image(HostFile { file, len });
    let mut head = vec![0u8; (len as usize).min(diskimage::detect::HEAD)];
    if let Err(e) = image.read_at(0, &mut head) {
        println!("error: {}: {}", from, e);
        return 3;
    }
    let format = detect(&head);
    if format != Format::Qcow2 {
        println!("refused: {} is {}", from, format.name());
        return 2;
    }
    let img = match Qcow2::open(&mut image) {
        Ok(img) => img,
        Err(Error::Image(bad)) => {
            println!("refused: {}", bad);
            return 2;
        }
        Err(e) => {
            println!("error: {}", e);
            return 3;
        }
    };

    let out = match OpenOptions::new().write(true).create_new(true).open(to) {
        Ok(out) => out,
        Err(e) => {
            println!("error: {}: {}", to, e);
            return 3;
        }
    };
    let result = out.set_len(img.size()).map_err(Error::Write).and_then(|()| {
        let mut disk = Disk { size: img.size(), end: 0, written: 0, write: |offset: u64, data: &[u8]| out.write_all_at(data, offset) };
        run(&img, &mut image, &mut disk)
    });
    drop(out);
    match result {
        Ok(s) => {
            println!("ok size={} cluster={} version={} written={} stored={} compressed={} zeros={} holes={}{}",
                     img.size(), img.cluster_size(), img.version(), s.written, s.stored, s.compressed, s.zeros,
                     s.holes, if img.dirty() { " dirty" } else { "" });
            0
        }
        Err(e) => {
            let _ = std::fs::remove_file(to);
            match e {
                Error::Image(bad) => {
                    println!("refused: {}", bad);
                    2
                }
                e => {
                    println!("error: {}", e);
                    3
                }
            }
        }
    }
}

/* ---- iso ---- */

/// `iso <out> <label> <name>=<file>...`: the image of those files.
fn iso(out: &str, label: &str, specs: &[String]) -> i32 {
    let mut named = Vec::new();
    for spec in specs {
        let Some((name, path)) = spec.split_once('=') else {
            println!("error: {} is not <name>=<file>", spec);
            return 3;
        };
        match std::fs::read(path) {
            Ok(data) => named.push((name.to_string(), data)),
            Err(e) => {
                println!("error: {}: {}", path, e);
                return 3;
            }
        }
    }
    let files: Vec<diskimage::iso::File> =
        named.iter().map(|(name, data)| diskimage::iso::File { name, data }).collect();
    let when = diskimage::iso::When { year: 2026, month: 10, day: 25, hour: 12, minute: 0, second: 0 };
    match diskimage::iso::image(label, &files, when) {
        Ok(img) => match std::fs::write(out, &img) {
            Ok(()) => {
                println!("ok {} bytes", img.len());
                0
            }
            Err(e) => {
                println!("error: {}: {}", out, e);
                3
            }
        },
        Err(e) => {
            println!("refused: {}", e);
            2
        }
    }
}

/* ---- inflate ---- */

/// A stream's bytes, one at a time.
struct Stream<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Input for Stream<'_> {
    type Error = io::Error;

    fn byte(&mut self) -> io::Result<Option<u8>> {
        let b = self.data.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        Ok(b)
    }
}

/// CRC-32 as zlib's crc32() has it.
struct Crc {
    table: [u32; 256],
    value: u32,
}

impl Crc {
    fn new() -> Crc {
        let mut table = [0u32; 256];
        for (n, slot) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        Crc { table, value: 0xFFFF_FFFF }
    }

    fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.value = self.table[((self.value ^ u32::from(b)) & 0xFF) as usize] ^ (self.value >> 8);
        }
    }

    fn finish(&self) -> u32 {
        self.value ^ 0xFFFF_FFFF
    }
}

fn inflate(cases: &str, answers: &str) -> i32 {
    let bytes = match std::fs::read(cases) {
        Ok(b) => b,
        Err(e) => {
            println!("error: {}: {}", cases, e);
            return 3;
        }
    };
    let mut inflater = Inflater::new().expect("memory for the inflater");
    let mut out = String::new();
    let mut pos = 0;
    let mut n = 0;
    while pos < bytes.len() {
        let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let limit = u64::from(word(pos));
        let len = word(pos + 4) as usize;
        let data = &bytes[pos + 8..pos + 8 + len];
        pos += 8 + len;

        let mut crc = Crc::new();
        let mut got = 0u64;
        let mut emit = |piece: &[u8]| {
            got += piece.len() as u64;
            crc.update(piece);
            Ok(())
        };
        let answer = inflater.inflate(&mut Stream { data, pos: 0 }, limit, &mut emit);
        match answer {
            Ok(()) => {
                assert!(got == limit, "case {}: {} bytes out, of {}", n, got, limit);
                out.push_str(&format!("ok {:08x}\n", crc.finish()));
            }
            Err(Stop::Fault(fault)) => out.push_str(&format!("fault {:?}\n", fault)),
            Err(_) => unreachable!("a stream in memory fails neither its reads nor its writes"),
        }
        n += 1;
    }
    if let Err(e) = std::fs::write(answers, out) {
        println!("error: {}: {}", answers, e);
        return 3;
    }
    println!("inflate: {} streams", n);
    0
}

/* ---- fuzz ---- */

/// The iteration under way, for the panic hook to say.
static ITERATION: AtomicU64 = AtomicU64::new(u64::MAX);
static SEED: AtomicU64 = AtomicU64::new(0);

/// xorshift64*: the same inputs for the same seed, everywhere.
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

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3], b[at + 4], b[at + 5], b[at + 6], b[at + 7]])
}

/// Where the image keeps what it is made of: ranges of its bytes, by kind,
/// for the changes to land on.
struct Map {
    header: (usize, usize),
    l1: (usize, usize),
    l2: Vec<(usize, usize)>,
    clusters: Vec<(usize, usize)>,
}

/// The image's own layout, as its tables give it -- read naively, the
/// image being one qemu-img made.
fn map(image: &[u8]) -> Map {
    let cluster_bits = be32(image, 20);
    let cluster = 1usize << cluster_bits;
    let version = be32(image, 4);
    let extended = version == 3 && be64(image, 72) & (1 << 4) != 0;
    let entry = if extended { 16 } else { 8 };
    let l1_size = be32(image, 36) as usize;
    let l1_offset = be64(image, 40) as usize;
    let within = |at: usize, len: usize| at.checked_add(len).is_some_and(|end| end <= image.len());
    let mut m = Map { header: (0, 112.min(image.len())), l1: (l1_offset, l1_size * 8), l2: Vec::new(), clusters: Vec::new() };
    for i in 0..l1_size {
        let at = l1_offset + i * 8;
        if !within(at, 8) {
            break;
        }
        let l2 = (be64(image, at) & 0x00FF_FFFF_FFFF_FE00) as usize;
        if l2 == 0 || !within(l2, cluster) {
            continue;
        }
        m.l2.push((l2, cluster));
        for j in 0..cluster / entry {
            let word = be64(image, l2 + j * entry);
            if word & (1 << 62) != 0 {
                let shift = 62 - (cluster_bits - 8);
                let offset = (word & ((1u64 << shift) - 1)) as usize;
                let sectors = (((word >> shift) & ((1u64 << (cluster_bits - 8)) - 1)) + 1) as usize;
                let len = (sectors * 512 - offset % 512).min(image.len().saturating_sub(offset));
                if len > 0 && within(offset, len) {
                    m.clusters.push((offset, len));
                }
            } else {
                let offset = (word & 0x00FF_FFFF_FFFF_FE00) as usize;
                if offset != 0 && within(offset, 1) {
                    m.clusters.push((offset, cluster.min(image.len() - offset)));
                }
            }
        }
    }
    m
}

fn fuzz(path: &str, seed: u64, iterations: u64, slow_ms: u128) -> i32 {
    let mut image = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!("error: {}: {}", path, e);
            return 3;
        }
    };
    if image.len() < 112 || detect(&image) != Format::Qcow2 {
        println!("error: {}: not a qcow2 image to start from", path);
        return 3;
    }
    let m = map(&image);
    SEED.store(seed, Ordering::Relaxed);
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let (mut converted, mut refused) = (0u64, 0u64);
    let mut reasons: Vec<(String, u64)> = Vec::new();
    let mut slowest = 0u128;
    let mut undo: Vec<(usize, u8)> = Vec::new();

    for iteration in 0..iterations {
        ITERATION.store(iteration, Ordering::Relaxed);
        /* A handful of changes, each to a byte of a part of the image. */
        for _ in 0..1 + rng.below(8) {
            let (at, len) = match rng.below(10) {
                0 | 1 => m.header,
                2 | 3 => m.l1,
                4..=6 if !m.l2.is_empty() => m.l2[rng.below(m.l2.len() as u64) as usize],
                7 | 8 if !m.clusters.is_empty() => m.clusters[rng.below(m.clusters.len() as u64) as usize],
                _ => (0, image.len()),
            };
            if len == 0 || at >= image.len() {
                continue;
            }
            let i = at + rng.below(len.min(image.len() - at) as u64) as usize;
            undo.push((i, image[i]));
            image[i] = match rng.below(4) {
                0 => image[i] ^ (1 << rng.below(8)),
                1 => 0,
                2 => 0xFF,
                _ => rng.next() as u8,
            };
        }
        let cut = if rng.below(10) == 0 { rng.below(image.len() as u64 + 1) as usize } else { image.len() };

        let started = Instant::now();
        let bytes = &image[..cut];
        let _ = detect(&bytes[..bytes.len().min(diskimage::detect::HEAD)]);
        let mut source = Image(Memory(bytes));
        let outcome = Qcow2::open(&mut source).and_then(|img| {
            let mut disk = Disk { size: img.size(), end: 0, written: 0, write: |_: u64, _: &[u8]| Ok(()) };
            run(&img, &mut source, &mut disk)
        });
        let took = started.elapsed().as_millis();
        slowest = slowest.max(took);
        if took > slow_ms {
            println!("FINDING: seed {} iteration {}: {} ms", seed, iteration, took);
            return 1;
        }
        match outcome {
            Ok(_) => converted += 1,
            Err(e) => {
                refused += 1;
                let why = match e {
                    Error::Image(bad) => {
                        let s = format!("{:?}", bad);
                        String::from(s.split(|c: char| !c.is_alphanumeric()).next().unwrap_or(""))
                    }
                    other => format!("{}", other),
                };
                match reasons.iter_mut().find(|(r, _)| *r == why) {
                    Some((_, n)) => *n += 1,
                    None => reasons.push((why, 1)),
                }
            }
        }
        for (i, b) in undo.drain(..).rev() {
            image[i] = b;
        }
    }
    reasons.sort_by(|a, b| b.1.cmp(&a.1));
    let list: Vec<String> = reasons.iter().map(|(r, n)| format!("{} {}", r, n)).collect();
    println!("fuzz: {} iterations, {} converted, {} refused ({}), slowest {} ms", iterations, converted, refused,
             list.join(", "), slowest);
    0
}

fn main() {
    std::panic::set_hook({
        let default = std::panic::take_hook();
        Box::new(move |info| {
            let iteration = ITERATION.load(Ordering::Relaxed);
            if iteration != u64::MAX {
                println!("FINDING: seed {} iteration {}: a panic", SEED.load(Ordering::Relaxed), iteration);
                let _ = io::stdout().flush();
            }
            default(info);
        })
    });

    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str, default: u64| {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map_or(default, |v| {
            v.parse().unwrap_or_else(|_| {
                eprintln!("{} wants a number", name);
                exit(3)
            })
        })
    };
    let code = match args.get(1).map(String::as_str) {
        Some("convert") if args.len() == 4 => convert(&args[2], &args[3]),
        Some("inflate") if args.len() == 4 => inflate(&args[2], &args[3]),
        Some("iso") if args.len() >= 4 => iso(&args[2], &args[3], &args[4..]),
        Some("fuzz") if args.len() >= 3 => {
            fuzz(&args[2], flag("--seed", 1), flag("--iterations", 1000), u128::from(flag("--slow-ms", 5000)))
        }
        _ => {
            eprintln!("usage: diskimage-test convert <image> <disk.raw>\n       diskimage-test inflate <cases> <answers>\n       \
                       diskimage-test fuzz <image> [--seed N] [--iterations N] [--slow-ms N]\n       \
                       diskimage-test iso <out.iso> <label> <name>=<file>...");
            3
        }
    };
    let _ = io::stdout().flush();
    exit(code);
}
