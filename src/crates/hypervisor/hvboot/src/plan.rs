//! What hvboot boots, worked out from the guest's disks alone: the ext
//! filesystems on them, GRUB's configuration and grubenv, the entry GRUB
//! would boot, its kernel's header, where the kernel and its initrds go in
//! RAM, and the command line -- then the kernel and the initrds read into
//! place. No platform in it: the boot (`boot.rs`) runs it over the virtio
//! disks and guest RAM, and the host's harness (fuzz/hvboot) over a disk
//! image and buffers of its own -- one copy of these rules, which
//! scripts/hvboot-test.py holds to what GRUB would boot.
//!
//! What goes wrong on the way is said where it is met, and a boot that
//! finds nothing says everything it passed over: a filesystem refused for
//! a feature is not "no filesystem", and a grub.cfg that could not be read
//! ends the search rather than another disk's being booted.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::bzimage::{Header, HEADER_BYTES};
use crate::disk::{Device, Part, SECTOR};
use crate::ext::{Fs, Inode, Lookup, Unmounted};
use crate::gpt::{self, Kind, Partition, TYPE_XBOOTLDR};
use crate::grub::{self, Root};
use crate::options::Options;
use crate::ram::{Loan, Ram, MAPPED};

/// Where GRUB keeps its configuration, from a filesystem's root: on the
/// root filesystem, or on a /boot of its own -- and GRUB 2 as Fedora names it.
pub const CONFIGS: [&str; 4] = ["/boot/grub/grub.cfg", "/grub/grub.cfg", "/boot/grub2/grub.cfg", "/grub2/grub.cfg"];
/// The most of a grub.cfg read: grub-mkconfig's is some 1.5 KiB a kernel
/// installed, so this is some 85 of them. Its reading, of whatever shape,
/// fits in the loader's heap with the rest of the boot: scripts/hvboot-test.py
/// boots a grub.cfg of this many bytes of each shape that costs the reading
/// the most a byte, beside a grubenv of `ENV_MAX`, inside hvboot's own heap.
pub const CONFIG_MAX: u64 = 128 * 1024;
/// The most of a grubenv read: GRUB's is a block of 1 KiB, which it keeps
/// at that size.
pub const ENV_MAX: u64 = 4 * 1024;
/// The most initrds an entry may load: a distribution's load one or two --
/// microcode, then its own -- and each is held open here until it is read.
pub const MAX_INITRDS: usize = 64;
const PAGE: u64 = 4096;
/// GRUB puts each initrd after the last at this alignment.
const INITRD_ALIGN: u64 = 4;

/// `disk::SECTOR`, kept beside the partition code's.
const _: () = assert!(SECTOR == gpt::SECTOR as u64);

/// An ext filesystem found on a disk: where, and what `search` knows it by.
pub struct Found {
    pub disk: usize,
    /// The partition's number, as GRUB's `hdN,gptM` counts it; None for a
    /// disk that is a filesystem whole.
    pub part: Option<u32>,
    first: u64,
    sectors: u64,
    uuid: String,
    label: String,
}

pub fn letter(i: usize) -> char {
    (b'a' + (i % 26) as u8) as char
}

/// A filesystem's name, as the guest's kernel will name its device.
pub fn name(f: &Found) -> String {
    match f.part {
        Some(p) => alloc::format!("vd{}{}", letter(f.disk), p),
        None => alloc::format!("vd{}", letter(f.disk)),
    }
}

/// Mount a filesystem found before: an error now -- a read that failed
/// since -- named by where.
fn mount<'a, D: Device>(disks: &'a mut [D], f: &Found) -> Result<Fs<Part<'a, D>>, String> {
    Fs::mount(Part { disk: &mut disks[f.disk], first: f.first, sectors: f.sectors })
        .map_err(|e| alloc::format!("{}: {}", name(f), e))
}

/// What was passed over looking for filesystems: each by its name, and why
/// -- None for there being no ext filesystem there, which is said only
/// when nothing is found (or under `hvboot.verbose`).
type Passed = Vec<(String, Option<String>)>;

/// Every ext filesystem on every disk -- each disk's XBOOTLDR partitions
/// first, a /boot of its own being where GRUB's files are when there is
/// one; and a disk with no partition table looked at whole.
fn filesystems<D: Device>(disks: &mut [D], only: Option<usize>, verbose: bool, passed: &mut Passed) -> Vec<Found> {
    let mut found = Vec::new();
    for d in 0..disks.len() {
        if only.is_some_and(|o| o != d) {
            continue;
        }
        let sectors = disks[d].sectors();
        let table = {
            let disk = &mut disks[d];
            let mut read = |s: u64, buf: &mut [u8]| disk.read(s, buf);
            match gpt::partitions(sectors, &mut read) {
                Ok(t) => t,
                Err(e) => {
                    say!("vd{}: {}; looked at whole", letter(d), e);
                    gpt::Table::default()
                }
            }
        };
        for (n, why) in table.passed_over {
            say!("vd{}{}: {}: passed over", letter(d), n, why);
            passed.push((alloc::format!("vd{}{}", letter(d), n), Some(why)));
        }
        let mut candidates: Vec<(Option<u32>, u64, u64)> = Vec::new();
        if table.parts.is_empty() {
            candidates.push((None, 0, sectors));
        }
        let boot_first = |p: &Partition| if p.kind == Kind::Gpt(TYPE_XBOOTLDR) { 0 } else { 1 };
        let mut ordered: Vec<&Partition> = table.parts.iter().filter(|p| !p.never_ext()).collect();
        ordered.sort_by_key(|p| boot_first(p));
        candidates.extend(ordered.iter().map(|p| (Some(p.number), p.first, p.sectors)));
        for (part, first, len) in candidates {
            let probe = Found { disk: d, part, first, sectors: len, uuid: String::new(), label: String::new() };
            match Fs::mount(Part { disk: &mut disks[d], first, sectors: len }) {
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
                Err(Unmounted::NotExt) => {
                    if verbose {
                        say!("{}: no ext2/3/4 filesystem", name(&probe));
                    }
                    passed.push((name(&probe), None));
                }
                Err(Unmounted::Refused(why)) => {
                    say!("{}: {}: passed over", name(&probe), why);
                    passed.push((name(&probe), Some(why)));
                }
            }
        }
    }
    found
}

/// What was passed over, said in one line: after "nothing found".
fn listed(passed: &Passed) -> String {
    let mut s = String::new();
    for (i, (n, why)) in passed.iter().enumerate() {
        s.push_str(if i == 0 { " -- " } else { "; " });
        s.push_str(n);
        s.push_str(": ");
        s.push_str(why.as_deref().unwrap_or("no ext2/3/4 filesystem"));
    }
    s
}

/// The first of `fss` with GRUB's configuration on it: which, the
/// configuration's path and bytes, and grubenv beside it -- its inode and
/// its bytes -- when there is one. A path that is not there is looked for
/// on; one that could not be read ends the search, said -- a configuration
/// on some other filesystem is not booted for a read that failed.
#[allow(clippy::type_complexity)]
fn config<D: Device>(disks: &mut [D], fss: &[Found], passed: &Passed)
    -> Result<(usize, &'static str, Vec<u8>, Option<(Inode, Vec<u8>)>), String>
{
    for (i, f) in fss.iter().enumerate() {
        let mut fs = mount(disks, f)?;
        for path in CONFIGS {
            let inode = match fs.lookup(path) {
                Ok(inode) => inode,
                Err(Lookup::NotThere(_)) => continue,
                Err(Lookup::Unreadable(e)) => {
                    return Err(alloc::format!("{}: GRUB's configuration could not be looked for: {}", name(f), e));
                }
            };
            if !inode.is_file() {
                continue;
            }
            let text = fs.read_all(&inode, CONFIG_MAX).map_err(|e| alloc::format!("{}{}: {}", name(f), path, e))?;
            let env_path = alloc::format!("{}env", path.trim_end_matches(".cfg"));
            let unseen = |e: &str| {
                say!("{}{}: {} -- its saved_entry and next_entry are not seen", name(f), env_path, e);
            };
            let env = match fs.lookup(&env_path) {
                Ok(e) if e.is_file() => match fs.read_all(&e, ENV_MAX) {
                    Ok(bytes) => Some((e, bytes)),
                    Err(err) => {
                        unseen(&err);
                        None
                    }
                },
                Ok(_) | Err(Lookup::NotThere(_)) => None,
                Err(Lookup::Unreadable(err)) => {
                    unseen(&err);
                    None
                }
            };
            return Ok((i, path, text, env));
        }
    }
    let mut on = String::new();
    for (i, f) in fss.iter().enumerate() {
        on.push_str(if i == 0 { "" } else { ", " });
        on.push_str(&name(f));
    }
    Err(alloc::format!("no grub.cfg on any of the guest's disks: looked for {}, {}, {} and {} on {}{}", CONFIGS[0],
                       CONFIGS[1], CONFIGS[2], CONFIGS[3], on, listed(passed)))
}

/// A root, as a person would write it.
fn describe(r: &Root) -> String {
    match r {
        Root::Device { disk, part: Some(p) } => alloc::format!("(hd{},{})", disk, p),
        Root::Device { disk, part: None } => alloc::format!("(hd{})", disk),
        Root::FsUuid(u) => alloc::format!("UUID {}", u),
        Root::Label(l) => alloc::format!("label {}", l),
        Root::File(p) => alloc::format!("a file {}", p),
        Root::Other(s) => alloc::format!("a device {}", s),
    }
}

/// The filesystem an entry's files are on: where its configuration was,
/// then each root set outside every entry (`outside`), then those each
/// submenu it is in set (`submenus`), then its own -- the last that finds
/// its filesystem winning, as in GRUB, where the root is whichever was set
/// last. A root not the entry's own that finds nothing is said when the
/// entry sets none of its own.
fn root_of<D: Device>(disks: &mut [D], fss: &[Found], cfg_at: usize, outside: &[Root], submenus: &[&[Root]],
                      title: &str, own: &[Root], verbose: bool) -> usize
{
    let mut root = cfg_at;
    let around = outside.iter().chain(submenus.iter().flat_map(|s| s.iter())).map(|r| (r, false));
    for (r, mine) in around.chain(own.iter().map(|r| (r, true))) {
        let hit = match r {
            Root::FsUuid(u) => fss.iter().position(|f| f.uuid.eq_ignore_ascii_case(u)),
            Root::Label(l) => fss.iter().position(|f| &f.label == l),
            Root::Device { disk, part } => fss.iter().position(|f| f.disk == *disk as usize && f.part == *part),
            Root::File(path) => (0..fss.len()).find(|&i| {
                mount(disks, &fss[i]).is_ok_and(|mut fs| fs.lookup(path).is_ok())
            }),
            Root::Other(_) => None,
        };
        match hit {
            Some(i) => root = i,
            None if mine => say!("entry \"{}\": nothing found by {}; its root stays {}", title, describe(r),
                                 name(&fss[root])),
            None if own.is_empty() || verbose => {
                say!("grub.cfg: nothing found by {}; the root stays {}", describe(r), name(&fss[root]))
            }
            None => {}
        }
    }
    root
}

/// A path as GRUB writes one, perhaps with a device first: `(hd0,gpt16)/x`
/// or `($root)/x`. The filesystem it is on, and the path in it. An entry's
/// `$root` comes here as nothing -- `()` -- and is its root.
fn on<'a>(fss: &[Found], root: usize, path: &'a str) -> Result<(usize, &'a str), String> {
    let Some(rest) = path.strip_prefix('(') else { return Ok((root, path)) };
    let Some((dev, p)) = rest.split_once(')') else {
        return Err(alloc::format!("{}: a device with no closing parenthesis", path));
    };
    if dev.is_empty() {
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

/// grubenv's `next_entry` emptied on its disk -- what the script
/// grub-mkconfig writes does before it boots that entry -- so that the
/// entry `grub-reboot` named is booted once, as under GRUB; and said, done
/// or not: left, it was booted at every boot after, a trial kernel's panic
/// a restart's loop.
fn clear_next_entry<D: Device>(disks: &mut [D], f: &Found, env: &Inode, was: &[u8]) {
    let done = match grub::env_without(was, "next_entry") {
        None => Err(String::from("grubenv is not a block GRUB wrote")),
        Some(new) => mount(disks, f).and_then(|mut fs| fs.rewrite(env, was, &new)),
    };
    match done {
        Ok(()) => say!("{}: grubenv's next_entry cleared, as GRUB clears it -- the next boot takes the default",
                       name(f)),
        Err(e) => say!("{}: grubenv's next_entry could not be cleared, and is taken again at the next boot -- {}",
                       name(f), e),
    }
}

/// Look `path` up and read the inode, with what went wrong said by where.
fn open<D: Device>(disks: &mut [D], f: &Found, path: &str) -> Result<Inode, String> {
    let mut fs = mount(disks, f)?;
    let inode = fs.lookup(path).map_err(|e| alloc::format!("{}: {}", name(f), e))?;
    if !inode.is_file() {
        return Err(alloc::format!("{}: {} is not a file", name(f), path));
    }
    Ok(inode)
}

/// A file of the boot: on which filesystem, its name there, its inode.
struct File {
    fs: usize,
    name: String,
    inode: Inode,
}

/// What is booted, and where it goes: the entry chosen, and why, is said
/// as it is chosen.
pub struct Plan {
    /// The kernel: its name, its header, and the image's first bytes the
    /// header is in -- what the zero page is made from.
    pub kernel_name: String,
    pub header: Header,
    pub first: Vec<u8>,
    /// The kernel's 64-bit part: where it begins in the file, and how long.
    pm: u64,
    pub kernel_len: u64,
    kernel: File,
    /// The initrds, each with where it goes in the one range they are read
    /// into, and that range's length.
    initrds: Vec<(File, u64)>,
    pub initrd_len: u64,
    /// The command line GRUB would give, and hv's after it.
    pub line: String,
    fss: Vec<Found>,
}

/// Where in RAM the kernel and the initrds go: ranges taken from `Ram`.
pub struct Place {
    pub kernel: Loan,
    pub initrd: Option<Loan>,
}

/// Work out the boot: everything up to reading the kernel and initrds,
/// which `load` does into the ranges `Place` says.
pub fn plan<D: Device>(disks: &mut [D], opts: &Options, ram: &mut Ram) -> Result<(Plan, Place), String> {
    if let Some(n) = opts.disk.filter(|&n| n >= disks.len()) {
        return Err(match disks.len() {
            1 => alloc::format!("hvboot.disk={}: the guest has 1 disk, vda (hvboot.disk=0)", n),
            k => alloc::format!("hvboot.disk={}: the guest has {} disks, vda to vd{} (hvboot.disk=0 to {})", n, k,
                                letter(k - 1), k - 1),
        });
    }
    let mut passed = Passed::new();
    let fss = filesystems(disks, opts.disk, opts.verbose, &mut passed);
    if fss.is_empty() {
        return Err(alloc::format!("no ext2/3/4 filesystem on the guest's disks{}", listed(&passed)));
    }

    let (cfg_at, cfg_path, bytes, env_file) = config(disks, &fss, &passed)?;
    let config = alloc::format!("{}{}", name(&fss[cfg_at]), cfg_path);
    /* The text as it was read when it is UTF-8, as grub-mkconfig writes
     * it: no second copy of it in the heap. */
    let text = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    let mut cfg = grub::parse(&text);
    let env = match &env_file {
        None => Vec::new(),
        Some((_, bytes)) => grub::parse_env(bytes).unwrap_or_else(|| {
            say!("{}: grubenv does not begin as GRUB's environment block does -- its saved_entry and next_entry are \
                  not seen, as GRUB would not see them", config);
            Vec::new()
        }),
    };
    let choice = grub::choose(&cfg.menu, &env, opts.entry.as_deref())?;
    let title = choice.entry.title.as_str();
    say!("{}: \"{}\", {}", config, title, choice.why);
    if let (true, Some((inode, bytes))) = (choice.once, &env_file) {
        clear_next_entry(disks, &fss[cfg_at], inode, bytes);
    }
    let entry = grub::read_entry(&mut cfg.reading, &text, choice.entry);
    drop(text);
    if !cfg.reading.unknown.is_empty() {
        say!("grub.cfg's variables {} have no value here, and are read as empty", cfg.reading.unknown.join(", "));
    }
    for n in &cfg.reading.notes {
        say!("grub.cfg's {}", n);
    }
    let Some(kernel_path) = entry.kernel.as_deref() else {
        return Err(alloc::format!("entry \"{}\" has no linux line", title));
    };

    let root = root_of(disks, &fss, cfg_at, &cfg.menu.roots, &choice.roots, title, &entry.roots, opts.verbose);
    let (kfs, kpath) = on(&fss, root, kernel_path)?;
    let kernel_name = alloc::format!("{}{}", name(&fss[kfs]), kpath);
    let kernel = open(disks, &fss[kfs], kpath)?;
    let mut first = vec![0u8; HEADER_BYTES.max(1024)];
    if kernel.size < first.len() as u64 {
        return Err(alloc::format!("{}: {} bytes, too short for a kernel", kernel_name, kernel.size));
    }
    mount(disks, &fss[kfs])?.read(&kernel, 0, &mut first).map_err(|e| alloc::format!("{}: {}", kernel_name, e))?;
    let header = Header::parse(&first).map_err(|e| alloc::format!("{}: {}", kernel_name, e))?;
    let pm = header.pm_offset();
    if pm >= kernel.size {
        return Err(alloc::format!("{}: shorter than its own setup", kernel_name));
    }
    let kernel_len = kernel.size - pm;
    let span = header.init_size.max(kernel_len);
    let kaddr = header.pref_address;
    let kloan = ram.take(kaddr, span).map_err(|e| alloc::format!("the kernel at {:#x}, {:#x} bytes to run in: {}",
                                                                  kaddr, span, e))?;

    /* The initrds, one after another at GRUB's alignment, as high as they
     * go below the header's ceiling. */
    let count = entry.initrds().count();
    if count > MAX_INITRDS {
        return Err(alloc::format!("entry \"{}\" names {} initrds, more than the {} this loads", title, count,
                                  MAX_INITRDS));
    }
    let mut initrds = Vec::with_capacity(count);
    let mut total = 0u64;
    for p in entry.initrds() {
        let (ifs, ipath) = on(&fss, root, p)?;
        let inode = open(disks, &fss[ifs], ipath)?;
        let at = total.next_multiple_of(INITRD_ALIGN);
        total = at.checked_add(inode.size).ok_or_else(|| String::from("initrds too big"))?;
        initrds.push((File { fs: ifs, name: alloc::format!("{}{}", name(&fss[ifs]), ipath), inode }, at));
    }
    let mut iloan = None;
    if total != 0 {
        let ceiling = header.initrd_addr_max.saturating_add(1).min(MAPPED);
        /* The kernel's range was taken: below 4 GiB, so no overflow. */
        let at = ram.highest(total, PAGE, kaddr + span, ceiling)
            .ok_or_else(|| alloc::format!("no RAM for {} MiB of initrd below {:#x}", total >> 20, ceiling))?;
        iloan = Some(ram.take(at, total)?);
    }

    /* The command line: GRUB's BOOT_IMAGE, the entry's own, as GRUB's
     * `linux` gives them -- then hv's, as hv gave them. */
    let mut line = String::from("BOOT_IMAGE=");
    grub::quote(kernel_path, &mut line);
    line.push_str(&entry.args);
    if !opts.kernel_line.is_empty() {
        line.push(' ');
        line.push_str(&opts.kernel_line);
    }

    let kernel = File { fs: kfs, name: kernel_name.clone(), inode: kernel };
    let plan = Plan { kernel_name, header, first, pm, kernel_len, kernel, initrds, initrd_len: total, line, fss };
    Ok((plan, Place { kernel: kloan, initrd: iloan }))
}

/// Read the kernel and the initrds where `plan` says: the kernel's 64-bit
/// part at the start of `kernel`, the initrds one after another in
/// `initrd`, the bytes between two -- GRUB's alignment -- zeros, as GRUB's
/// `grub_initrd_load` writes them, whatever the RAM held.
pub fn load<D: Device>(disks: &mut [D], plan: &Plan, kernel: &mut [u8], initrd: &mut [u8]) -> Result<(), String> {
    let n = usize::try_from(plan.kernel_len).map_err(|_| String::from("a kernel too long"))?;
    let Some(dest) = kernel.get_mut(..n) else {
        return Err(alloc::format!("{} bytes of kernel for {} of room", n, kernel.len()));
    };
    let k = &plan.kernel;
    mount(disks, &plan.fss[k.fs])?.read(&k.inode, plan.pm, dest).map_err(|e| alloc::format!("{}: {}", k.name, e))?;
    let mut done = 0usize;
    for (f, at) in &plan.initrds {
        /* From where the last ended: the padding, then this one. */
        let at = usize::try_from(*at).ok().filter(|&a| a >= done);
        let end = at.zip(usize::try_from(f.inode.size).ok()).and_then(|(a, n)| a.checked_add(n));
        let (Some(at), Some(end)) = (at, end) else {
            return Err(alloc::format!("{}: not after the initrd before it", f.name));
        };
        let Some(room) = initrd.get_mut(done..end) else {
            return Err(alloc::format!("{}: past the {} bytes of initrd room", f.name, initrd.len()));
        };
        let (pad, dest) = room.split_at_mut(at - done);
        pad.fill(0);
        mount(disks, &plan.fss[f.fs])?.read(&f.inode, 0, dest).map_err(|e| alloc::format!("{}: {}", f.name, e))?;
        done = end;
    }
    Ok(())
}
