//! hvboot's own code on the host: what hvboot would boot off disk images,
//! worked out by its `plan` -- the boot's rules, over the partition table,
//! ext and GRUB readers and the bzImage header it reads through, from their
//! own sources -- and under its own heap: every allocation hvboot's code
//! makes here is `arena.rs`'s arithmetic over a stretch no bigger than its
//! heap, so that a run that would spend the loader's heap fails here too.
//!
//!     hvboot-test image <disk.raw> [--also <disk.raw>]... [--dump <dir>] [--entry <spec>]
//!                       [--cmdline <line>] [--write]
//!
//! says what hvboot says -- its console's lines -- and then the kernel's and
//! the initrds' sizes, the command line and the heap it took; with --dump,
//! writes the kernel and the initrd as its reader read them into RAM that
//! held other bytes, to compare with e2fsprogs'; with --write, opens the
//! images to be written, as hvboot finds a guest's disks, and empties
//! grubenv's next_entry once it has booted it, as hvboot does.
//!
//!     hvboot-test fuzz <disk.raw> [--seed N] [--iterations N]
//!
//! reads the same image with bytes of its partition table, superblock,
//! group descriptors, inode tables, extent and directory blocks and of its
//! grub.cfg changed at random -- each run a different disk -- and holds the
//! code to never panicking, never reading outside the disk, always ending,
//! staying inside hvboot's heap and giving all of it back.
//!
//!     hvboot-test grub <grub.cfg> [--env <grubenv>] [--entry <spec>]
//!
//! reads a grub.cfg alone, as hvboot does: the menu, the entry chosen and
//! why, its kernel, initrds and roots, and the command line GRUB's `linux`
//! would give.
//!
//!     hvboot-test arena [--seed N] [--iterations N]
//!     hvboot-test limits
//!
//! hold hvboot's heap to a model of it -- blocks apart, aligned, their bytes
//! kept, all of it one free block again once all is given back -- and say
//! the bounds the gate makes its inputs to.

#![allow(dead_code)]

extern crate alloc;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

/// hvboot's console, here: its lines as it says them.
macro_rules! say {
    ($($arg:tt)*) => {{
        println!("hvboot: {}", format_args!($($arg)*));
    }};
}

#[path = "../../../src/crates/hypervisor/hvboot/src/arena.rs"]
mod arena;
#[path = "../../../src/crates/hypervisor/hvboot/src/disk.rs"]
mod disk;
#[path = "../../../src/crates/hypervisor/hvboot/src/ext.rs"]
mod ext;
#[path = "../../../src/crates/hypervisor/hvboot/src/gpt.rs"]
mod gpt;
#[path = "../../../src/crates/hypervisor/hvboot/src/grub.rs"]
mod grub;
#[path = "../../../src/crates/hypervisor/hvboot/src/options.rs"]
mod options;
#[path = "../../../src/crates/hypervisor/hvboot/src/plan.rs"]
mod plan;
#[path = "../../../src/crates/hypervisor/hvboot/src/ram.rs"]
mod ram;

/// hvboot's heap here: while an `Hvboot` is held, what is allocated comes
/// from `arena.rs` over a stretch of `HEAP_BYTES` less the platform's share;
/// what the harness itself allocates, from the system's. A block goes back
/// to whichever it came from, by its address.
mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::UnsafeCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::arena::{Arena, HEAP_BYTES};

    /// What the loader's platform takes of its heap besides what `plan`
    /// does: a virtio ring and a header page a disk -- 16 KiB with hv's
    /// queues of 256 -- the zero page and the command line. Under this with
    /// sixteen disks.
    pub const PLATFORM: usize = 256 << 10;

    #[repr(C, align(4096))]
    struct Space(UnsafeCell<[u8; HEAP_BYTES]>);
    // SAFETY: reached through `Heap`'s lock alone.
    unsafe impl Sync for Space {}
    static SPACE: Space = Space(UnsafeCell::new([0; HEAP_BYTES]));

    struct Heap {
        arena: UnsafeCell<Arena>,
        locked: AtomicBool,
        on: AtomicBool,
    }
    // SAFETY: the arena is reached with `locked` taken.
    unsafe impl Sync for Heap {}

    #[global_allocator]
    static HEAP: Heap = Heap { arena: UnsafeCell::new(Arena::EMPTY), locked: AtomicBool::new(false),
                               on: AtomicBool::new(false) };

    /// The arena, held: None while it is held already -- an allocation
    /// made inside one of its own, a panic's -- which then goes elsewhere.
    fn take() -> Option<&'static mut Arena> {
        if HEAP.locked.swap(true, Ordering::Acquire) {
            return None;
        }
        // SAFETY: `locked` was false and is now ours: no other reference.
        let a = unsafe { &mut *HEAP.arena.get() };
        if !a.ready() {
            // SAFETY: SPACE is the arena's alone, 4096-aligned; once.
            unsafe { a.init(SPACE.0.get().cast::<u8>(), HEAP_BYTES - PLATFORM) };
        }
        Some(a)
    }

    fn give() {
        HEAP.locked.store(false, Ordering::Release);
    }

    fn mine(p: *mut u8) -> bool {
        let base = SPACE.0.get() as usize;
        (p as usize) >= base && (p as usize) < base + HEAP_BYTES
    }

    /// An arena that panicked -- found itself broken -- is a finding: and
    /// no panic may leave a global allocator.
    fn guarded<T>(f: impl FnOnce() -> T) -> T {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("FINDING: hvboot's heap found itself broken");
                std::process::exit(1);
            }
        }
    }

    fn exhausted(asked: usize, used: usize) -> ! {
        eprintln!("FINDING: hvboot's heap of {} KiB less the platform's {} KiB has no room for {} more bytes, {} in use",
                  HEAP_BYTES >> 10, PLATFORM >> 10, asked, used);
        std::process::exit(1);
    }

    // SAFETY: each block is the arena's or the system's, given back to the
    // one it came from.
    unsafe impl GlobalAlloc for Heap {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if self.on.load(Ordering::Relaxed) {
                if let Some(a) = take() {
                    let p = guarded(|| a.alloc(l));
                    let used = a.usage().0;
                    give();
                    if p.is_null() {
                        exhausted(l.size(), used);
                    }
                    return p;
                }
            }
            // SAFETY: as the caller's.
            unsafe { System.alloc(l) }
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            if mine(p) {
                /* Held: a panic out of the arena is being reported; the
                 * block is left. */
                if let Some(a) = take() {
                    // SAFETY: an arena block, handed out by it.
                    guarded(|| unsafe { a.dealloc(p) });
                    give();
                }
                return;
            }
            // SAFETY: a system block.
            unsafe { System.dealloc(p, l) }
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            if mine(p) {
                let Some(a) = take() else { return std::ptr::null_mut() };
                // SAFETY: an arena block, its layout.
                let q = guarded(|| unsafe { a.realloc(p, l, n) });
                let used = a.usage().0;
                give();
                if q.is_null() {
                    exhausted(n, used);
                }
                return q;
            }
            // SAFETY: a system block.
            unsafe { System.realloc(p, l, n) }
        }
    }

    /// hvboot's code runs while this is held.
    pub struct Hvboot(bool);

    pub fn enter() -> Hvboot {
        Hvboot(HEAP.on.swap(true, Ordering::Relaxed))
    }

    impl Drop for Hvboot {
        fn drop(&mut self) {
            HEAP.on.store(self.0, Ordering::Relaxed);
        }
    }

    /// `f` with the harness's own allocations: guest RAM, which the
    /// loader lends, is no part of its heap.
    pub fn outside<T>(f: impl FnOnce() -> T) -> T {
        let was = HEAP.on.swap(false, Ordering::Relaxed);
        let v = f();
        HEAP.on.store(was, Ordering::Relaxed);
        v
    }

    /// What hvboot's heap holds now, the most it held, and how big it is.
    pub fn usage() -> (usize, usize, usize) {
        let a = take().expect("hvboot's heap held");
        let (used, peak) = a.usage();
        give();
        (used, peak, HEAP_BYTES - PLATFORM)
    }

    /// Whether hvboot's heap is one free block again, all of it.
    pub fn whole() -> bool {
        let a = take().expect("hvboot's heap held");
        let mut blocks = [(0usize, 0usize); 2];
        let n = a.free_blocks(&mut blocks);
        let whole = n == 1 && blocks[0] == (0, HEAP_BYTES - PLATFORM) && a.usage().0 == 0;
        give();
        whole
    }
}

const SECTOR: u64 = 512;

/// A disk: a file's bytes, or a copy of them with some changed, read by
/// whole sectors -- every read held to the disk's bounds, a read past them
/// a finding.
struct Image {
    file: File,
    len: u64,
    /// Bytes the fuzzer changed: offset, value.
    patches: Vec<(u64, u8)>,
    reads: u64,
    /// The sectors of the reads of metadata's size -- not a kernel's
    /// megabytes -- when they are being noted.
    noted: Option<Vec<u64>>,
    /// Opened to be written (`--write`).
    writable: bool,
}

/// The most a read is that is noted: metadata comes a block or a few at a
/// time, a file's data in runs.
const NOTE_MAX: usize = 64 * 1024;

impl disk::Device for Image {
    fn sectors(&self) -> u64 {
        self.len / SECTOR
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), String> {
        let offset = sector * SECTOR;
        assert!(buf.len() as u64 % SECTOR == 0, "an unaligned read: {} bytes at {}", buf.len(), offset);
        assert!(offset + buf.len() as u64 <= self.len, "a read past the disk: {} at {} of {}", buf.len(), offset,
                self.len);
        self.reads += 1;
        assert!(self.reads < 1_000_000, "a million reads: a loop that does not end");
        if let Some(noted) = self.noted.as_mut() {
            if buf.len() <= NOTE_MAX {
                heap::outside(|| noted.extend((0..buf.len() as u64 / SECTOR).map(|s| sector + s)));
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

    /// Refused for an image not opened to be written, and held to its
    /// bounds as a read is.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), String> {
        let offset = sector * SECTOR;
        assert!(buf.len() as u64 % SECTOR == 0, "an unaligned write: {} bytes at {}", buf.len(), offset);
        assert!(offset + buf.len() as u64 <= self.len, "a write past the disk: {} at {} of {}", buf.len(), offset,
                self.len);
        if !self.writable {
            return Err(String::from("read-only"));
        }
        let f = &mut self.file;
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        f.write_all(buf).map_err(|e| e.to_string())
    }
}

fn open(path: &str, writable: bool) -> Image {
    let file = std::fs::OpenOptions::new().read(true).write(writable).open(path)
        .unwrap_or_else(|e| panic!("{}: {}", path, e));
    let len = file.metadata().map(|m| m.len()).unwrap_or(0) / SECTOR * SECTOR;
    Image { file, len, patches: Vec::new(), reads: 0, noted: None, writable }
}

/* The guest the harness boots into, as hv makes one: RAM below 640 KiB and
 * from 1 MiB to 128 MiB, the loader's image at 1 MiB -- what `Ram` places
 * a kernel and its initrds in. */
const MEMMAP: [ram::E820Entry; 2] = [ram::E820Entry { addr: 0, size: 0x9_FC00, kind: ram::E820_RAM },
                                     ram::E820Entry { addr: 1 << 20, size: (127 << 20), kind: ram::E820_RAM }];
const IMAGE: (u64, u64) = (1 << 20, 6 << 20);
const RSDP: u64 = 0xE_0000;
const CMDLINE_AT: u32 = 0x2_0000;
/// What RAM held before the loader read into it: every byte it gives the
/// kernel, the padding between initrds too, it has to have written.
const STALE: u8 = 0xA5;

/// What hvboot booted: the kernel -- where it was, and its 64-bit part as
/// read into RAM -- the initrds as read there, and the command line.
struct Booted {
    kernel_name: String,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    line: String,
}

/// What hvboot would boot off `disks`, its command line `cmdline`: its own
/// `plan` and `load`, under its own heap -- all of which it has given back
/// by the time this returns, the error too, which is the harness's copy.
fn boot(disks: &mut [Image], cmdline: &str) -> Result<Booted, String> {
    boot_in(disks, cmdline).map_err(|e| heap::outside(|| e.clone()))
}

fn boot_in(disks: &mut [Image], cmdline: &str) -> Result<Booted, String> {
    let _in = heap::enter();
    let opts = options::Options::parse(cmdline)?;
    let mut ram = ram::Ram::new(&MEMMAP, IMAGE);
    let (plan, place) = plan::plan(disks, &opts, &mut ram)?;
    let klen = usize::try_from(plan.kernel_len).map_err(|_| "a kernel too long")?;
    let ilen = place.initrd.as_ref().map_or(0, |l| l.len() as usize);
    let (mut kernel, mut initrd) = heap::outside(|| (vec![STALE; klen], vec![STALE; ilen]));
    plan::load(disks, &plan, &mut kernel, &mut initrd)?;
    let mut page = [0u8; bzimage::ZERO_PAGE_BYTES];
    let initrd_at = place.initrd.as_ref().map_or(0, ram::Loan::addr);
    let (Ok(initrd_at), Ok(initrd_len)) = (u32::try_from(initrd_at), u32::try_from(plan.initrd_len)) else {
        return Err(String::from("an initrd above 4 GiB"));
    };
    let loaded = bzimage::Loaded { cmdline: CMDLINE_AT, cmdline_len: plan.line.len(), initrd: (initrd_at, initrd_len),
                                   rsdp: RSDP };
    bzimage::zero_page(&plan.first, &plan.header, &loaded, &MEMMAP, &mut page).map_err(|e| e.to_string())?;
    let (kernel_name, line) = heap::outside(|| (plan.kernel_name.clone(), plan.line.clone()));
    Ok(Booted { kernel_name, kernel, initrd, line })
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
    let mut disks = [open(path, false)];
    disks[0].noted = Some(Vec::new());
    let _ = boot(&mut disks, "");
    let mut seen = disks[0].noted.take().unwrap_or_default();
    seen.sort_unstable();
    seen.dedup();
    seen
}

/// The heap after a run: given back whole, or a finding.
fn heap_back(what: &str) {
    if !heap::whole() {
        let (used, _, _) = heap::usage();
        println!("FINDING: {}: hvboot's heap not given back whole after it, {} bytes still in use", what, used);
        std::process::exit(1);
    }
}

fn image(args: &[String]) {
    let mut paths = Vec::new();
    let (mut dump, mut cmdline, mut write) = (None, String::new(), false);
    let mut i = 0;
    while i < args.len() {
        let value = || args.get(i + 1).cloned().unwrap_or_else(|| panic!("{} wants a value", args[i]));
        match args[i].as_str() {
            "--also" => { paths.push(value()); i += 1; }
            "--dump" => { dump = Some(value()); i += 1; }
            "--entry" => { cmdline = format!("{} hvboot.entry={}", cmdline, value()); i += 1; }
            "--cmdline" => { cmdline = format!("{} {}", cmdline, value()); i += 1; }
            "--write" => write = true,
            p => paths.insert(0, String::from(p)),
        }
        i += 1;
    }
    let mut disks: Vec<Image> = paths.iter().map(|p| open(p, write)).collect();
    let result = boot(&mut disks, &cmdline);
    let (_, peak, size) = heap::usage();
    heap_back("a boot");
    match result {
        Ok(b) => {
            println!("kernel: {}, {} bytes; initrd: {} bytes", b.kernel_name, b.kernel.len(), b.initrd.len());
            println!("command line: {}", b.line);
            println!("reads: {}", disks.iter().map(|d| d.reads).sum::<u64>());
            println!("heap: {} KiB at most of {} KiB", peak >> 10, size >> 10);
            if let Some(dir) = dump {
                std::fs::write(format!("{}/kernel", dir), &b.kernel).expect("write the kernel");
                std::fs::write(format!("{}/initrd", dir), &b.initrd).expect("write the initrd");
            }
        }
        Err(e) => {
            println!("hvboot: cannot boot: {}", e);
            println!("heap: {} KiB at most of {} KiB", peak >> 10, size >> 10);
            std::process::exit(1);
        }
    }
}

fn fuzz(path: &str, seed: u64, iterations: u64) {
    let targets = sectors_read(path);
    heap_back("a clean run");
    assert!(!targets.is_empty(), "a clean run read nothing to change");
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut booted = 0u64;
    let mut peak = 0usize;
    for i in 0..iterations {
        let mut disks = [open(path, false)];
        for _ in 0..1 + rng.below(8) {
            let sector = targets[rng.below(targets.len() as u64) as usize];
            let at = sector * SECTOR + rng.below(SECTOR);
            let v = match rng.below(4) { 0 => 0, 1 => 0xFF, _ => rng.next() as u8 };
            disks[0].patches.push((at, v));
        }
        let patches = disks[0].patches.clone();
        /* What the boot prints, here only noise: kept off the gate's
         * output, a finding printing what it needs. */
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| boot(&mut disks, "").is_ok()));
        match r {
            Ok(true) => booted += 1,
            Ok(false) => {}
            Err(_) => {
                println!("FINDING: seed {} iteration {}: a panic, with the bytes {:?} changed", seed, i, patches);
                std::process::exit(1);
            }
        }
        peak = peak.max(heap::usage().1);
        if !heap::whole() {
            println!("FINDING: seed {} iteration {}: hvboot's heap not given back whole, the bytes {:?} changed", seed,
                     i, patches);
            std::process::exit(1);
        }
    }
    println!("hvboot-test fuzz: {} disks, {} still booted, {} metadata sectors changed among, the heap {} KiB at \
              most, no finding", iterations, booted, targets.len(), peak >> 10);
}

/// A grub.cfg's menu, as read: each item's number, title and id, entries'
/// kernels -- each entry's body read for it.
fn menu(r: &mut grub::Reading, text: &str, items: &[grub::Item], depth: usize) {
    for (i, it) in items.iter().enumerate() {
        match it {
            grub::Item::Entry(e) => println!("menu: {}{} \"{}\" id {:?} kernel {:?}", "  ".repeat(depth), i, e.title,
                                             e.id, grub::read_entry(r, text, e).kernel),
            grub::Item::Submenu(s) => {
                println!("menu: {}{} submenu \"{}\" id {:?} roots {:?}", "  ".repeat(depth), i, s.title, s.id, s.roots);
                menu(r, text, &s.items, depth + 1);
            }
        }
    }
}

fn grub_cfg(path: &str, env: Option<&str>, entry: Option<&str>) {
    let text = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {}", path, e));
    let env = env.map(|p| std::fs::read(p).unwrap_or_else(|e| panic!("{}: {}", p, e)));
    let ok = {
        let _in = heap::enter();
        let text = String::from_utf8_lossy(&text).into_owned();
        let mut cfg = grub::parse(&text);
        menu(&mut cfg.reading, &text, &cfg.menu.items, 0);
        let vars = match &env {
            None => Vec::new(),
            Some(bytes) => match grub::parse_env(bytes) {
                Some(v) => v,
                None => {
                    println!("env: not an environment block");
                    Vec::new()
                }
            },
        };
        for (n, v) in &vars {
            println!("env: {}={:?}", n, v);
        }
        for r in &cfg.menu.roots {
            println!("root outside: {:?}", r);
        }
        let ok = match grub::choose(&cfg.menu, &vars, entry) {
            Ok(c) => {
                println!("chosen: \"{}\", {}{}", c.entry.title, c.why, if c.once { ", once" } else { "" });
                let b = grub::read_entry(&mut cfg.reading, &text, c.entry);
                println!("kernel: {:?}", b.kernel);
                println!("initrds: {:?}", b.initrds().collect::<Vec<_>>());
                for r in c.roots.iter().flat_map(|s| s.iter()).chain(b.roots.iter()) {
                    println!("root: {:?}", r);
                }
                let mut line = String::from("BOOT_IMAGE=");
                grub::quote(b.kernel.as_deref().unwrap_or(""), &mut line);
                println!("command line: {}{}", line, b.args);
                true
            }
            Err(why) => {
                println!("no entry: {}", why);
                false
            }
        };
        for n in &cfg.reading.notes {
            println!("note: {}", n);
        }
        if !cfg.reading.unknown.is_empty() {
            println!("unknown: {}", cfg.reading.unknown.join(", "));
        }
        ok
    };
    let (_, peak, size) = heap::usage();
    heap_back("a grub.cfg read");
    println!("heap: {} KiB at most of {} KiB", peak >> 10, size >> 10);
    std::process::exit(if ok { 0 } else { 1 });
}

/// hvboot's heap's arithmetic held to a model of it: blocks of every size
/// and alignment allocated, grown, shrunk and given back in random order,
/// each filled with its own bytes and found with them; none overlapping,
/// each aligned; and with everything given back, one free block again.
fn arena_check(seed: u64, iterations: u64) {
    use std::alloc::Layout;

    const LEN: usize = 1 << 20;
    #[repr(C, align(4096))]
    struct Stretch([u8; LEN]);
    let stretch: &'static mut Stretch = Box::leak(Box::new(Stretch([0; LEN])));
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut a = arena::Arena::EMPTY;
    // SAFETY: a stretch of the harness's own, aligned, for this arena alone.
    unsafe { a.init(stretch.0.as_mut_ptr(), LEN) };
    let mut live: Vec<(*mut u8, Layout, u8)> = Vec::new();
    let fill = |p: *mut u8, n: usize, v: u8| {
        // SAFETY: a block the arena handed out, `n` bytes long.
        unsafe { std::ptr::write_bytes(p, v, n) }
    };
    let holds = |p: *mut u8, n: usize, v: u8| {
        // SAFETY: as `fill`.
        (0..n).all(|i| unsafe { *p.add(i) } == v)
    };
    let mut failed = 0u64;
    for step in 0..iterations {
        match rng.below(10) {
            0..=4 => {
                let size = match rng.below(4) { 0 => rng.below(16) as usize + 1, 1 => rng.below(256) as usize + 1,
                                                2 => rng.below(4096) as usize + 1, _ => rng.below(64 << 10) as usize + 1 };
                let align = 1usize << rng.below(13);
                let l = Layout::from_size_align(size, align).unwrap();
                let p = a.alloc(l);
                if p.is_null() {
                    failed += 1;
                    continue;
                }
                assert!(p as usize % align == 0, "step {}: a block at {:p} for an alignment of {}", step, p, align);
                let start = stretch.0.as_ptr() as usize;
                assert!(p as usize >= start && p as usize + size <= start + LEN, "step {}: a block outside", step);
                for &(q, ql, _) in &live {
                    let apart = p as usize + size <= q as usize || q as usize + ql.size() <= p as usize;
                    assert!(apart, "step {}: a block over another", step);
                }
                let v = rng.next() as u8;
                fill(p, size, v);
                live.push((p, l, v));
            }
            5..=7 if !live.is_empty() => {
                let (p, l, v) = live.swap_remove(rng.below(live.len() as u64) as usize);
                assert!(holds(p, l.size(), v), "step {}: a block's bytes changed under it", step);
                // SAFETY: a live block of this arena's.
                unsafe { a.dealloc(p) };
            }
            8..=9 if !live.is_empty() => {
                let k = rng.below(live.len() as u64) as usize;
                let (p, l, v) = live[k];
                let n = match rng.below(3) { 0 => rng.below(l.size() as u64) as usize + 1,
                                             _ => l.size() + rng.below(8192) as usize + 1 };
                // SAFETY: a live block of this arena's, its layout.
                let q = unsafe { a.realloc(p, l, n) };
                if q.is_null() {
                    failed += 1;
                    continue;
                }
                assert!(q as usize % l.align() == 0, "step {}: a block moved off its alignment", step);
                assert!(holds(q, l.size().min(n), v), "step {}: a block's bytes not kept as it changed size", step);
                fill(q, n, v);
                live[k] = (q, Layout::from_size_align(n, l.align()).unwrap(), v);
            }
            _ => {}
        }
    }
    let peak = a.usage().1;
    for (p, l, v) in live.drain(..) {
        assert!(holds(p, l.size(), v), "at the end: a block's bytes changed under it");
        // SAFETY: as above.
        unsafe { a.dealloc(p) };
    }
    let mut blocks = [(0usize, 0usize); 2];
    let n = a.free_blocks(&mut blocks);
    assert!(n == 1 && blocks[0] == (0, LEN) && a.usage().0 == 0,
            "everything given back, and the arena is {} free blocks: {:?}", n, &blocks[..n.min(2)]);
    println!("hvboot-test arena: {} steps, {} with no room, the most in use {} KiB of {} KiB, given back whole, no \
              finding", iterations, failed, peak >> 10, LEN >> 10);
}

fn main() {
    /* stdout's buffer made now, by the harness: not inside hvboot's heap,
     * where it would outlive every run. */
    println!("hvboot-test");
    let args: Vec<String> = std::env::args().collect();
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let number = |name: &str, default: u64| flag(name).and_then(|s| s.parse().ok()).unwrap_or(default);
    match args.get(1).map(String::as_str) {
        Some("image") => image(&args[2..]),
        Some("fuzz") => fuzz(args.get(2).expect("hvboot-test fuzz <disk.raw>"), number("--seed", 1),
                             number("--iterations", 2000)),
        Some("grub") => grub_cfg(args.get(2).expect("hvboot-test grub <grub.cfg>"), flag("--env").as_deref(),
                                 flag("--entry").as_deref()),
        Some("arena") => arena_check(number("--seed", 1), number("--iterations", 200_000)),
        Some("limits") => {
            println!("CONFIG_MAX {}", plan::CONFIG_MAX);
            println!("ENV_MAX {}", plan::ENV_MAX);
            println!("HEAP {}", arena::HEAP_BYTES - heap::PLATFORM);
        }
        _ => {
            eprintln!("hvboot-test image <disk.raw> [--also <disk.raw>]... [--dump <dir>] [--entry <spec>] \
                       [--cmdline <line>] [--write] | fuzz <disk.raw> [--seed N] [--iterations N] | grub <grub.cfg> \
                       [--env <grubenv>] [--entry <spec>] | arena [--seed N] [--iterations N] | limits");
            std::process::exit(2);
        }
    }
}
