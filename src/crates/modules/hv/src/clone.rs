//! `hv clone <from> <to> [port=N]`, and the host API's clone: a VM's folder
//! copied into a new one -- a VM of the same machine, its disks as they are
//! now, that boots as a machine of its own:
//!
//! - the disks in its folder copied with their holes left out: only what
//!   the source's filesystem keeps on the device is read
//!   (`kcore::fs::Image::extent`), and of that only what is not zeros is
//!   written -- a disk of 20 GiB with 2 GiB on it costs 2 GiB, its copy
//!   too. A read-only disk outside the folder -- an installer's ISO -- is
//!   shared by its path; a written one is refused, since two VMs writing
//!   one disk wreck it;
//! - its kernel and initrd copied when they are in the folder, and its
//!   cloud-init seed made anew: a new instance-id and its own name in the
//!   meta-data, the user-data the source was given (`hv create` keeps it in
//!   the folder) -- so cloud-init runs for it as for a new machine: its own
//!   name, its own SSH host keys. Its /etc/machine-id is the source's,
//!   which a distribution writes once (`docs/hypervisor.md`);
//! - its vm.toml the source's, the operator's lines kept, with what makes a
//!   machine itself made new: a new UUID, a port of the switch -- a MAC and
//!   an address -- that no other VM's vm.toml names, when the source had one
//!   (`port=` says which), no forwards, no `ip=` on its command line, and
//!   not protected.
//!
//! Refused while the source is on the list -- hv stop it first: its disks
//! are held to read while they are copied (`kcore::fs::Image`), which a guest
//! writing them would refuse -- and when the new name has a folder. Both
//! names are held while it runs. The room it takes is asked for before
//! anything is written (`kcore::fs::space`), and what fails, or is stopped,
//! leaves nothing behind.

use alloc::string::String;
use alloc::vec::Vec;

use kcore::consts::NS_PER_SEC;
use kcore::fs::{File, Image};

use crate::cloud::{self, Made, Rate};
use crate::config::Config;
use crate::guest;
use crate::net;
use crate::store::{self, Folder};
use crate::vms::Vms;
use crate::work::{self, Work};

pub const USAGE: &str = "hv clone <from> <to> [port=N]";
/// What a disk is read in, and what of a piece is written at a time: the
/// blocks that are not zeros. A quarter of a MiB, half what the kernel's
/// heap gives in one block (its pages are at most 128 together).
const CHUNK: usize = 256 * 1024;
const BLOCK: usize = 4096;
/// Room asked for beyond what is copied, for the new files' own blocks --
/// ext2's indirect ones are a thousandth of what they point at -- and a
/// MiB for the rest.
const SLACK_SHARE: u64 = 64;
const SLACK_MIN: u64 = 1024 * 1024;
const MIB: u64 = 1024 * 1024;
/// How far apart the progress lines are.
const REPORT_NS: u64 = 5 * NS_PER_SEC;
/// The most user-data read back from the source's folder.
const MAX_USER_DATA: usize = cloud::MAX_USER_DATA;

/// A file of the source's copied into the clone's folder: where from and
/// where to, held to read while it is copied.
struct Copy {
    from: String,
    to: String,
    image: Image,
    size: u64,
    /// What its filesystem keeps of it on the device: what is read.
    data: u64,
}

/// What a clone came to.
pub struct Cloned {
    /// Its vm.toml.
    pub file: String,
    /// Each file copied: its path in the clone's folder, its size and how
    /// much of it was written -- the rest holes.
    pub copied: Vec<(String, u64, u64)>,
    pub port: Option<u32>,
    /// Its seed was made anew.
    pub seed: bool,
}

/// `hv clone <from> <to> [port=N]`.
pub fn command(root: &str, vms: &Vms, args: &str, work: &mut dyn Work) {
    let mut words = args.split_ascii_whitespace();
    let (Some(from), Some(to)) = (words.next(), words.next()) else {
        let _ = writeln!(work, "{}", USAGE);
        return;
    };
    let mut port = None;
    for word in words {
        match word.strip_prefix("port=").map(str::parse::<u32>) {
            Some(Ok(p)) if port.is_none() => port = Some(p),
            _ => {
                let _ = writeln!(work, "{}", USAGE);
                return;
            }
        }
    }
    match clone(root, vms, from, to, port, work) {
        Ok(c) => {
            for (path, size, written) in &c.copied {
                let _ = writeln!(work, "hv: {}: {} MiB written of {} MiB, the rest holes", path,
                                 written.div_ceil(MIB), size.div_ceil(MIB));
            }
            let _ = write!(work, "hv: {} written -- hv start {} boots it", c.file, to);
            if let Some(p) = c.port {
                let _ = write!(work, ", at {} (port {})", net::dotted(net::port_ip(p as usize)), p);
            }
            let _ = writeln!(work, "{}", if c.seed { ", cloud-init given a seed of its own" } else { "" });
        }
        Err(e) => {
            let _ = writeln!(work, "hv: {}", e);
        }
    }
}

/// The name of a file directly in `folder` that `path` -- one of its
/// vm.toml's, resolved -- is, if it is one.
fn in_folder<'a>(folder: &Folder, path: &'a str) -> Option<&'a str> {
    path.strip_prefix(folder.path.as_str())?.strip_prefix('/').filter(|rest| !rest.is_empty())
}

/// A port of the switch for a clone, or a VM made with one of its own:
/// `asked`, or the lowest no VM's vm.toml under `root` names and no VM on
/// the list holds.
pub fn port_for(root: &str, vms: &Vms, asked: Option<u32>) -> Result<u32, String> {
    let mut used = [false; net::MAX_PORTS];
    for p in vms.ports_held() {
        if let Some(u) = used.get_mut(p) {
            *u = true;
        }
    }
    let mut named: Vec<(u32, String)> = Vec::new();
    for entry in kcore::fs::list(root).map_err(|e| alloc::format!("{}: {}", root, e))? {
        let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
        if !entry.dir || !store::valid_name(name) {
            continue;
        }
        let Ok(folder) = store::folder_of(root, name) else { continue };
        if let Ok(c) = store::load(&folder) {
            if let Some(p) = c.options.port {
                if let Some(u) = used.get_mut(p as usize) {
                    *u = true;
                }
                named.push((p, folder.name));
            }
        }
    }
    match asked {
        Some(p) if p as usize >= net::MAX_PORTS => Err(alloc::format!("port= must be 0..{}", net::MAX_PORTS - 1)),
        Some(p) => match named.iter().find(|(q, _)| *q == p) {
            Some((_, owner)) => Err(alloc::format!("port {} is {}'s, as its vm.toml says", p, owner)),
            None if used[p as usize] => Err(alloc::format!("port {} is held by a vm that runs", p)),
            None => Ok(p),
        },
        None => used.iter().position(|u| !u).map(|p| p as u32)
            .ok_or_else(|| alloc::format!("every one of the switch's {} ports is some vm's", net::MAX_PORTS)),
    }
}

/// What of the image is data: its runs, as its filesystem has them.
fn data_of(image: &Image, path: &str, work: &dyn Work) -> Result<u64, String> {
    let size = image.size().map_err(|e| alloc::format!("{}: {}", path, e))?;
    let (mut at, mut data) = (0u64, 0u64);
    while at < size {
        if work.cancelled() {
            return Err(String::from(work::STOPPED));
        }
        let (start, end) = image.extent(at).map_err(|e| alloc::format!("{}: {}", path, e))?;
        data += end - start;
        at = end.max(start);
    }
    Ok(data)
}

/// The VM `from`'s folder under `root` cloned into a new one, `to`, at
/// `port` of the switch when asked: what it came to, or why not. Its lines,
/// its progress and whether to stop are `work`'s.
pub fn clone(root: &str, vms: &Vms, from: &str, to: &str, port: Option<u32>, work: &mut dyn Work)
             -> Result<Cloned, String> {
    let oom = || String::from("out of memory");
    let source = store::folder_of(root, from)?;
    if let Some(why) = store::name_refused(to) {
        return Err(why);
    }
    if from == to {
        return Err(String::from("a vm is cloned to a name of its own"));
    }
    let target = Folder { name: String::from(to), path: store::join(root, to).ok_or_else(oom)? };
    if kcore::fs::list(&target.path).is_ok() || kcore::fs::size(&target.path).is_ok() {
        return Err(alloc::format!("{} is there already -- not written over", target.path));
    }
    /* Both names held while it runs: the source started -- its disks
     * written -- or deleted meanwhile, and the new name made twice, are
     * refused. */
    let _from = vms.reserve(from, "cloned")?;
    let _to = vms.reserve(to, "made")?;
    let (text, was) = store::load_text(&source)?;
    let o = store::options(&source, &was)?;

    /* What is copied, what is shared, what is made anew: the new
     * configuration's paths are the clone's folder's, by name. */
    let mut c: Config = was.clone();
    let mut copies: Vec<Copy> = Vec::new();
    let mut seed = false;
    let mut copy = |path: &str, what: &str| -> Result<String, String> {
        let name = in_folder(&source, path)
            .ok_or_else(|| alloc::format!("its {} {} is outside its folder", what, path))?;
        if name.contains('/') {
            return Err(alloc::format!("its {} {} is in a folder of its folder's -- a clone copies the files of the \
                                       folder alone", what, path));
        }
        let image = Image::open(path, false)
            .map_err(|_| alloc::format!("{} could not be held to read -- a guest writes it?", path))?;
        let size = image.size().map_err(|e| alloc::format!("{}: {}", path, e))?;
        let data = data_of(&image, path, work)?;
        let to_path = store::join(&target.path, name).ok_or_else(oom)?;
        copies.push(Copy { from: String::from(path), to: to_path, image, size, data });
        Ok(String::from(name))
    };
    if in_folder(&source, &o.kernel).is_some() {
        c.options.kernel = copy(&o.kernel, "kernel")?;
    }
    if let Some(initrd) = o.initrd.as_deref().filter(|i| in_folder(&source, i).is_some()) {
        c.options.initrd = Some(copy(initrd, "initrd")?);
    }
    for (i, (path, ro)) in o.disks.iter().enumerate() {
        match in_folder(&source, path) {
            Some(name) if name == cloud::SEED && *ro => {
                c.options.disks[i].0 = String::from(cloud::SEED);
                seed = true;
            }
            Some(_) => c.options.disks[i].0 = copy(path, "disk")?,
            None if *ro => {}
            None => {
                return Err(alloc::format!("its disk {} is outside its folder, and written: two vms writing one disk \
                                           wreck it -- copy it into the folder, or make it :ro", path));
            }
        }
    }
    drop(copy);
    c.options.uuid = Some(guest::random_uuid());
    c.options.port = match (was.options.port, port) {
        (None, None) => None,
        (_, asked) => Some(port_for(root, vms, asked)?),
    };
    c.options.forwards.clear();
    c.options.cmdline = c.options.cmdline.as_deref().map(crate::config::without_ip);
    c.protected = false;
    store::options_checked(&target, &c)?;
    let config = crate::config::rewrite(&text, &c)?;

    /* The user-data it is given: the source's, as its folder keeps it --
     * or, for a VM made before folders kept it, the keys nos's sshd lets in,
     * which the source's disk already has. */
    let user_path = store::join(&source.path, cloud::USER_DATA).ok_or_else(oom)?;
    let user = match kcore::fs::read(&user_path, MAX_USER_DATA) {
        Ok(bytes) => Some(String::from_utf8(bytes).map_err(|_| alloc::format!("{} is not UTF-8 text", user_path))?),
        Err(_) if seed => {
            let _ = writeln!(work, "hv: {} has no user-data kept -- {}'s seed gives the keys nos's sshd lets in, and \
                                    no password", from, to);
            Some(cloud::user_data(&cloud::keys(), None))
        }
        Err(_) => None,
    };
    let seed_image = match (&user, seed) {
        (Some(u), true) => Some(cloud::seed(to, u)?),
        _ => None,
    };

    /* Room for it all, asked for before anything is written. */
    let data: u64 = copies.iter().map(|c| c.data).sum();
    let small = seed_image.as_ref().map_or(0, Vec::len) + user.as_ref().map_or(0, String::len) + config.len();
    let need = data + small as u64;
    match kcore::fs::space(&target.path) {
        Ok(space) => {
            let want = need + need / SLACK_SHARE + SLACK_MIN;
            if space.free_bytes < want {
                return Err(alloc::format!("{} MiB to copy, and {} MiB free where it goes", want.div_ceil(MIB),
                                          space.free_bytes / MIB));
            }
        }
        Err(_) => {
            let _ = writeln!(work, "hv: {}'s filesystem does not say how much room it has -- copied without asking",
                             target.path);
        }
    }

    let mut made = Made::new();
    store::make_dirs(&target.path).map_err(|_| alloc::format!("{} could not be made", target.path))?;
    made.folder = Some(target.path.clone());
    let mut copied = Vec::new();
    let mut buf = Vec::new();
    buf.try_reserve_exact(CHUNK).map_err(|_| oom())?;
    buf.resize(CHUNK, 0);
    let started = kcore::time::boot_time_ns();
    let mut progress = Progress { done: 0, total: data, started, reported: started };
    for each in &copies {
        let _ = writeln!(work, "hv: {} -- {} MiB, {} MiB of it data -- into {}", each.from, each.size.div_ceil(MIB),
                         each.data.div_ceil(MIB), each.to);
        kcore::fs::create(&each.to, &[]).map_err(|e| alloc::format!("{} could not be made -- {}", each.to, e))?;
        made.files.push(each.to.clone());
        let written = copy_file(each, &mut buf, &mut progress, work)?;
        copied.push((each.to.clone(), each.size, written));
    }
    if let Some(u) = &user {
        let path = store::join(&target.path, cloud::USER_DATA).ok_or_else(oom)?;
        kcore::fs::create(&path, u.as_bytes()).map_err(|e| alloc::format!("{} could not be written -- {}", path, e))?;
        made.files.push(path);
    }
    if let Some(image) = &seed_image {
        let path = store::join(&target.path, cloud::SEED).ok_or_else(oom)?;
        kcore::fs::create(&path, image).map_err(|e| alloc::format!("{} could not be written -- {}", path, e))?;
        made.files.push(path);
    }
    let file = store::join(&target.path, store::CONFIG).ok_or_else(oom)?;
    kcore::fs::create(&file, config.as_bytes()).map_err(|e| alloc::format!("{} could not be written -- {}", file, e))?;
    made.files.push(file.clone());
    if work.cancelled() {
        return Err(String::from(work::STOPPED));
    }
    made.keep();
    Ok(Cloned { file, copied, port: c.options.port, seed: seed_image.is_some() })
}

/// How far the copy has got, over every file: what of the data has been
/// read, and when it was last said.
struct Progress {
    done: u64,
    total: u64,
    started: u64,
    reported: u64,
}

/// One file copied into its new place, grown to its size with a hole first:
/// its data read a piece at a time, and of each piece the blocks that are
/// not zeros written. How much was written; synced before it returns.
fn copy_file(each: &Copy, buf: &mut [u8], progress: &mut Progress, work: &mut dyn Work) -> Result<u64, String> {
    let dst = File::open(&each.to, true).map_err(|e| alloc::format!("{}: {}", each.to, e))?;
    dst.set_len(each.size).map_err(|e| alloc::format!("{}: {} bytes do not fit there -- {}", each.to, each.size, e))?;
    let write_err = |e: kcore::error::Error| alloc::format!("{}: writing it failed -- {} (is the filesystem full?)",
                                                            each.to, e);
    let mut written = 0u64;
    let mut at = 0u64;
    while at < each.size {
        let (start, end) = each.image.extent(at).map_err(|e| alloc::format!("{}: {}", each.from, e))?;
        let mut pos = start;
        while pos < end {
            if work.cancelled() {
                return Err(String::from(work::STOPPED));
            }
            let n = usize::try_from(end - pos).map_or(buf.len(), |n| n.min(buf.len()));
            let piece = &mut buf[..n];
            each.image.read_at(pos, piece).map_err(|e| alloc::format!("{}: reading it failed -- {}", each.from, e))?;
            /* The blocks that are not zeros, in runs, each run one write. */
            let mut k = 0;
            while k < n {
                let end_of = |k: usize| (k + BLOCK).min(n);
                if piece[k..end_of(k)].iter().all(|&b| b == 0) {
                    k = end_of(k);
                    continue;
                }
                let mut run = end_of(k);
                while run < n && !piece[run..end_of(run)].iter().all(|&b| b == 0) {
                    run = end_of(run);
                }
                dst.write_at(pos + k as u64, &piece[k..run]).map_err(write_err)?;
                written += (run - k) as u64;
                k = run;
            }
            pos += n as u64;
            progress.done += n as u64;
            work.progress(progress.done, progress.total);
            let now = kcore::time::boot_time_ns();
            if now.saturating_sub(progress.reported) >= REPORT_NS {
                progress.reported = now;
                let ns = now.saturating_sub(progress.started);
                let _ = writeln!(work, "hv: {} of {} MiB copied, {} s, {}", progress.done / MIB,
                                 progress.total.div_ceil(MIB), ns / NS_PER_SEC, Rate::of(progress.done, ns));
            }
        }
        at = end.max(start);
    }
    dst.sync().map_err(|e| alloc::format!("{}: syncing it failed -- {}", each.to, e))?;
    Ok(written)
}
