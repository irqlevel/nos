//! The boot, in order: what hv handed over; the disks, their partitions and
//! the filesystems on them; GRUB's configuration and the entry it would
//! boot; that entry's kernel and initrd read into RAM; the zero page; and
//! the jump into the kernel.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use crate::bzimage::{self, Header, ENTRY64_OFFSET, HEADER_BYTES, ZERO_PAGE_BYTES};
use crate::disk::Part;
use crate::ext::{Fs, Inode};
use crate::gpt::{self, Kind, Partition, TYPE_XBOOTLDR};
use crate::grub::{self, Entry, Root};
use crate::io::rdtsc;
use crate::phys::Ram;
use crate::start::{Options, StartInfo};
use crate::virtio::{self, Disk};

/// Where GRUB keeps its configuration, from a filesystem's root: on the
/// root filesystem, or on a /boot of its own -- and GRUB 2 as Fedora names it.
const CONFIGS: [&str; 4] = ["/boot/grub/grub.cfg", "/grub/grub.cfg", "/boot/grub2/grub.cfg", "/grub2/grub.cfg"];
/// The most of a configuration, and of a grubenv, read.
const CONFIG_MAX: u64 = 1 << 20;
const ENV_MAX: u64 = 64 * 1024;
const PAGE: u64 = 4096;
/// GRUB puts each initrd after the last at this alignment.
const INITRD_ALIGN: u64 = 4;
/// Where an initrd may go at most when the header says nothing lower: the
/// identity map's end.
const FOUR_GIB: u64 = 1 << 32;

/// An ext filesystem found on a disk: where, and what `search` knows it by.
struct Found {
    disk: usize,
    /// The partition's number, as GRUB's `hdN,gptM` counts it; None for a
    /// disk that is a filesystem whole.
    part: Option<u32>,
    first: u64,
    sectors: u64,
    uuid: String,
    label: String,
}

fn mount<'a>(disks: &'a mut [Disk], f: &Found) -> Result<Fs<Part<'a>>, String> {
    Fs::mount(Part { disk: &mut disks[f.disk], first: f.first, sectors: f.sectors })
}

fn letter(i: usize) -> char {
    (b'a' + (i as u8 % 26)) as char
}

fn ms(cycles: u64, khz: Option<u64>) -> u64 {
    khz.map_or(0, |k| cycles / k.max(1))
}

/// Every ext filesystem on every disk -- each disk's XBOOTLDR partitions
/// first, a /boot of its own being where GRUB's files are when there is
/// one; and a disk with no partition table looked at whole.
fn filesystems(disks: &mut [Disk], only: Option<usize>, verbose: bool) -> Vec<Found> {
    let mut found = Vec::new();
    for d in 0..disks.len() {
        if only.is_some_and(|o| o != d) {
            continue;
        }
        let sectors = disks[d].sectors;
        let parts = {
            let disk = &mut disks[d];
            let mut read = |s: u64, buf: &mut [u8]| disk.read(s, buf);
            match gpt::partitions(sectors, &mut read) {
                Ok(p) => p,
                Err(e) => {
                    say!("vd{}: {}; looked at whole", letter(d), e);
                    Vec::new()
                }
            }
        };
        let mut candidates: Vec<(Option<u32>, u64, u64)> = Vec::new();
        if parts.is_empty() {
            candidates.push((None, 0, sectors));
        }
        let boot_first = |p: &Partition| if p.kind == Kind::Gpt(TYPE_XBOOTLDR) { 0 } else { 1 };
        let mut ordered: Vec<&Partition> = parts.iter().filter(|p| !p.never_ext()).collect();
        ordered.sort_by_key(|p| boot_first(p));
        candidates.extend(ordered.iter().map(|p| (Some(p.number), p.first, p.sectors)));
        for (part, first, len) in candidates {
            let probe = Found { disk: d, part, first, sectors: len, uuid: String::new(), label: String::new() };
            match mount(disks, &probe) {
                Ok(fs) => {
                    let (uuid, label) = (fs.uuid(), fs.label());
                    if fs.needs_recovery() {
                        say!("{}: its journal wants replaying, read as it is", name(&probe));
                    }
                    if verbose {
                        say!("{}: ext, UUID {}, label \"{}\"", name(&probe), uuid, label);
                    }
                    found.push(Found { uuid, label, ..probe });
                }
                Err(e) if verbose => say!("{}: {}", name(&probe), e),
                Err(_) => {}
            }
        }
    }
    found
}

fn name(f: &Found) -> String {
    match f.part {
        Some(p) => alloc::format!("vd{}{}", letter(f.disk), p),
        None => alloc::format!("vd{}", letter(f.disk)),
    }
}

/// The first of `fss` with GRUB's configuration on it: which, the
/// configuration's path and text, and grubenv's beside it.
fn config(disks: &mut [Disk], fss: &[Found]) -> Result<(usize, &'static str, String, String), String> {
    for (i, f) in fss.iter().enumerate() {
        let mut fs = mount(disks, f)?;
        for path in CONFIGS {
            let Ok(inode) = fs.lookup(path) else { continue };
            if !inode.is_file() {
                continue;
            }
            let text = fs.read_all(&inode, CONFIG_MAX)?;
            let env_path = alloc::format!("{}env", path.trim_end_matches(".cfg"));
            let env = match fs.lookup(&env_path) {
                Ok(e) if e.is_file() => fs.read_all(&e, ENV_MAX).unwrap_or_default(),
                _ => Vec::new(),
            };
            return Ok((i, path, String::from_utf8_lossy(&text).into_owned(),
                       String::from_utf8_lossy(&env).into_owned()));
        }
    }
    Err(String::from("no grub.cfg on any of the guest's disks (looked for /boot/grub/grub.cfg and \
                      /grub/grub.cfg on every ext filesystem)"))
}

/// The filesystem an entry's files are on: where its configuration was,
/// then each `search` and `set root` in turn -- the last that finds its
/// filesystem winning, as in GRUB.
fn root_of(disks: &mut [Disk], fss: &[Found], cfg_at: usize, e: &Entry) -> usize {
    let mut root = cfg_at;
    for r in &e.roots {
        let hit = match r {
            Root::FsUuid(u) => fss.iter().position(|f| f.uuid.eq_ignore_ascii_case(u)),
            Root::Label(l) => fss.iter().position(|f| &f.label == l),
            Root::Device { disk, part } => fss.iter().position(|f| f.disk == *disk as usize && f.part == *part),
            Root::File(path) => (0..fss.len()).find(|&i| {
                mount(disks, &fss[i]).is_ok_and(|mut fs| fs.lookup(path).is_ok())
            }),
        };
        match hit {
            Some(i) => root = i,
            None => say!("entry \"{}\": nothing found by {:?}; its root stays {}", e.title, r, name(&fss[root])),
        }
    }
    root
}

/// A path as GRUB writes one, perhaps with a device first: `(hd0,gpt16)/x`
/// or `($root)/x`. The filesystem it is on, and the path in it.
fn on<'a>(fss: &[Found], root: usize, path: &'a str) -> Result<(usize, &'a str), String> {
    let Some(rest) = path.strip_prefix('(') else { return Ok((root, path)) };
    let Some((dev, p)) = rest.split_once(')') else {
        return Err(alloc::format!("{}: a device with no closing parenthesis", path));
    };
    if dev.is_empty() || dev == "$root" {
        return Ok((root, p));
    }
    let Some(Root::Device { disk, part }) = grub::device(dev) else {
        return Err(alloc::format!("{}: a device this does not know", path));
    };
    match fss.iter().position(|f| f.disk == disk as usize && f.part == part) {
        Some(i) => Ok((i, p)),
        None => Err(alloc::format!("{}: no ext filesystem at ({})", path, dev)),
    }
}

/// Look `path` up and read the inode, with what went wrong said by where.
fn open(disks: &mut [Disk], f: &Found, path: &str) -> Result<Inode, String> {
    let mut fs = mount(disks, f)?;
    let inode = fs.lookup(path).map_err(|e| alloc::format!("{}: {}", name(f), e))?;
    if !inode.is_file() {
        return Err(alloc::format!("{}: {} is not a file", name(f), path));
    }
    Ok(inode)
}

pub fn run(start_info: u64) -> Result<Infallible, String> {
    let si = StartInfo::read(start_info)?;
    let opts = Options::parse(&si.cmdline)?;
    let ram_bytes: u64 = si.memmap.iter().filter(|e| e.kind == crate::start::E820_RAM).map(|e| e.size).sum();
    say!("started by PVH: {} MiB of RAM, its command line \"{}\"", ram_bytes >> 20, si.cmdline);
    let mut ram = Ram::new(&si.memmap, crate::image_range());
    let t0 = rdtsc();

    let found = crate::pci::virtio_disks();
    if found.is_empty() {
        return Err(String::from("no virtio disk on the guest's PCI bus: give the VM a disk= with its image"));
    }
    let mut disks = Vec::new();
    for (i, d) in found.iter().enumerate() {
        let disk = Disk::new(d.io_base, letter(i), opts.tsc_khz)?;
        if opts.verbose {
            say!("vd{}: {} MiB{}, PCI slot {}", letter(i), disk.sectors >> 11,
                 if disk.read_only { ", read-only" } else { "" }, d.slot);
        }
        disks.push(disk);
    }
    let fss = filesystems(&mut disks, opts.disk, opts.verbose);
    if fss.is_empty() {
        return Err(String::from("no ext2/3/4 filesystem on the guest's disks"));
    }

    let (cfg_at, cfg_path, text, env_text) = config(&mut disks, &fss)?;
    let cfg = grub::parse(&text);
    let env = grub::parse_env(&env_text);
    let (entry, why) = grub::choose(&cfg, &env, opts.entry.as_deref())?;
    say!("{}{}: \"{}\", {}", name(&fss[cfg_at]), cfg_path, entry.title, why);
    if !cfg.unknown.is_empty() {
        say!("grub.cfg's variables {} have no value here, and are read as empty", cfg.unknown.join(", "));
    }
    let Some(kernel_path) = entry.kernel.as_deref() else {
        return Err(alloc::format!("entry \"{}\" has no linux line", entry.title));
    };

    let root = root_of(&mut disks, &fss, cfg_at, entry);
    let (kfs, kpath) = on(&fss, root, kernel_path)?;
    let kernel = open(&mut disks, &fss[kfs], kpath)?;
    let mut first = vec![0u8; HEADER_BYTES.max(1024)];
    if kernel.size < first.len() as u64 {
        return Err(alloc::format!("{}{}: {} bytes, too short for a kernel", name(&fss[kfs]), kpath, kernel.size));
    }
    mount(&mut disks, &fss[kfs])?.read(&kernel, 0, &mut first)?;
    let header = Header::parse(&first).map_err(|e| alloc::format!("{}{}: {}", name(&fss[kfs]), kpath, e))?;
    let pm = header.pm_offset();
    if pm >= kernel.size {
        return Err(alloc::format!("{}{}: shorter than its own setup", name(&fss[kfs]), kpath));
    }
    let kernel_len = kernel.size - pm;
    let span = header.init_size.max(kernel_len);
    let kaddr = header.pref_address;
    let image = ram.lend(kaddr, span).map_err(|e| alloc::format!("the kernel at {:#x}, {:#x} bytes to run in: {}",
                                                                   kaddr, span, e))?;
    mount(&mut disks, &fss[kfs])?.read(&kernel, pm, &mut image[..kernel_len as usize])?;

    /* The initrds, one after another at GRUB's alignment, as high as they
     * go below the header's ceiling. */
    let mut parts: Vec<(usize, u64, Inode)> = Vec::new();
    let mut total = 0u64;
    for p in &entry.initrds {
        let (ifs, ipath) = on(&fss, root, p)?;
        let inode = open(&mut disks, &fss[ifs], ipath)?;
        let at = total.next_multiple_of(INITRD_ALIGN);
        total = at.checked_add(inode.size).ok_or_else(|| String::from("initrds too big"))?;
        parts.push((ifs, at, inode));
    }
    let mut initrd = (0u64, 0u64);
    if total != 0 {
        let ceiling = header.initrd_addr_max.saturating_add(1).min(FOUR_GIB);
        let at = ram.highest(total, PAGE, kaddr + span, ceiling)
            .ok_or_else(|| alloc::format!("no RAM for {} MiB of initrd below {:#x}", total >> 20, ceiling))?;
        let dest = ram.lend(at, total)?;
        for (ifs, offset, inode) in &parts {
            let n = inode.size as usize;
            mount(&mut disks, &fss[*ifs])?.read(inode, 0, &mut dest[*offset as usize..*offset as usize + n])?;
        }
        initrd = (at, total);
    }

    /* The command line: GRUB's BOOT_IMAGE, the entry's own, then hv's. */
    let mut line = alloc::format!("BOOT_IMAGE={}", kernel_path);
    for a in entry.args.iter().chain(opts.kernel_args.iter()) {
        line.push(' ');
        line.push_str(a);
    }
    /* The command line and the zero page are the kernel's from the jump on:
     * kept for good, in the loader's heap -- RAM the kernel's memory map
     * has, which its decompressor stays clear of until it has copied them. */
    let mut cmdline = Vec::with_capacity(line.len() + 1);
    cmdline.extend_from_slice(line.as_bytes());
    cmdline.push(0);
    let cmdline: &'static [u8] = cmdline.leak();
    let page: &'static mut [u8; ZERO_PAGE_BYTES] = alloc::boxed::Box::leak(alloc::boxed::Box::new([0u8; ZERO_PAGE_BYTES]));
    bzimage::zero_page(&first, &header, cmdline.as_ptr() as u64, line.len(), initrd, &si.memmap, si.rsdp, page)?;

    let t1 = rdtsc();
    say!("{}{}: boot protocol {}.{:02}, {} KiB at {:#x}{}, initrd {} KiB at {:#x}, read in {} ms", name(&fss[kfs]),
         kpath, header.version >> 8, header.version & 0xFF, kernel_len >> 10, kaddr,
         if header.relocatable { "" } else { " (not relocatable)" }, initrd.1 >> 10, initrd.0,
         ms(t1.wrapping_sub(t0), opts.tsc_khz));
    say!("its command line: {}", line);
    for d in &disks {
        d.reset();
    }
    jump(kaddr + ENTRY64_OFFSET, page.as_ptr() as u64)
}

/// Into the kernel: its 64-bit entry, `RSI` at the zero page, interrupts
/// off, the GDT's 0x10 and 0x18 the code and data it runs on.
fn jump(entry: u64, zero_page: u64) -> ! {
    // SAFETY: the kernel is in RAM at its load address, with the room its
    // init_size asks for; the zero page, the command line and the initrd it
    // points at are in memory nothing will reuse before the kernel copies
    // them; the identity map covers all of it; and nothing of the loader
    // runs after the jump.
    unsafe {
        core::arch::asm!("cli", "xor ebp, ebp", "jmp rax", in("rax") entry, in("rsi") zero_page,
                         options(noreturn))
    }
}

/// `virtio::SECTOR`, kept beside the partition code's.
const _: () = assert!(virtio::SECTOR == gpt::SECTOR as u64);
