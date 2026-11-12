//! A guest's disk that is a device of nos's -- a partition, or a whole disk
//! -- rather than a file: what it is named by, whether it may be given, and
//! `hv devices`.
//!
//! A VM's vm.toml names a device by what finds it again from one boot to the
//! next, whatever order the drivers put it in this time: `part:<GUID>` for a
//! partition -- its GPT unique GUID, or its MBR's signature and number --
//! and `disk:<model>_<serial>` for a whole disk (`kcore::block::Disk::ident`).
//! A name, `dev:nvme14`, is for what is given once -- `hv boot`, a start
//! with a kernel -- and is never written into a vm.toml: a device named so
//! is written down by its id.
//!
//! A device is given to a VM (`hv create ... disk=`, `hv disk attach`) only
//! where the machine does not boot from it -- nor from a partition of it, or
//! one overlapping it -- and nothing holds it or anything overlapping it; and
//! unless told (`force`), only where no other VM's vm.toml names it or
//! anything overlapping it, no other VM being made is being given it, every
//! VM's vm.toml can be read, and nothing is on it -- the refusal saying what
//! is there. The look and the write of the vm.toml that names it are one
//! step, under the lock every vm.toml change takes (`store::give_devices`,
//! `store::update`): two VMs given one device at once were each told no other
//! VM named it.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use diskimage::contents::{self, Found as Holds};
use kcore::block::{Disk, Found};
use kcore::block::PartKind;
use kcore::cmd::Output;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;

use crate::store;
use crate::vms::Vms;

/// What a disk's word in a vm.toml or on a command line is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source<'a> {
    /// A file of nos's, by its path.
    File(&'a str),
    /// A device, by its id: `part:...`, `disk:...`.
    Id(&'a str),
    /// A device, by this boot's name: `dev:nvme14`.
    Name(&'a str),
    /// A volume of NBS's, by its name: `nbs:disk1` (plans/nbs.md, N8) -- the
    /// NBS cluster's, read and written through its client from any host of
    /// it.
    Volume(&'a str),
}

/// What `word` -- a disk's, without its `:ro` -- names.
pub fn source(word: &str) -> Source<'_> {
    if word.starts_with("part:") || word.starts_with("disk:") {
        Source::Id(word)
    } else if let Some(name) = word.strip_prefix("dev:") {
        Source::Name(name)
    } else if let Some(name) = word.strip_prefix("nbs:") {
        Source::Volume(name)
    } else {
        Source::File(word)
    }
}

/// Whether `word` names a device of this host's.
pub fn is_device(word: &str) -> bool {
    matches!(source(word), Source::Id(_) | Source::Name(_))
}

/// Whether `word` names a file of nos's: neither a device nor a volume.
pub fn is_file(word: &str) -> bool {
    matches!(source(word), Source::File(_))
}

/// The device `word` names.
pub fn find(word: &str) -> Result<Disk, String> {
    match source(word) {
        Source::Id(id) => Disk::find_ident(id).map_err(|f| match f {
            Found::Many(n) => alloc::format!("{} devices say they are {} -- copies of one disk or of one partition \
                                              table: none of them is taken for it", n, id),
            _ => alloc::format!("no device is {} -- it is not in this machine, or its partition table changed (hv \
                                 devices lists what there is)", id),
        }),
        Source::Name(name) => Disk::open(name).ok_or_else(|| alloc::format!("no device is called {}", name)),
        Source::File(path) => Err(alloc::format!("{} is a file, not a device", path)),
        Source::Volume(name) => Err(alloc::format!("nbs:{} is a volume of NBS's, not a device of this host's", name)),
    }
}

/// The device `word` gives -- an id, `dev:` and a name, or a bare name --
/// and the id a vm.toml names it by from then on.
pub fn given(word: &str) -> Result<(Disk, String), String> {
    let disk = match source(word) {
        Source::File(name) => Disk::open(name).ok_or_else(|| alloc::format!("no device is called {}", name))?,
        _ => find(word)?,
    };
    let id = disk.ident().ok_or_else(|| {
        alloc::format!("{} has no id that finds it again at the next boot -- no partition GUID, MBR signature or \
                        serial number -- and a name is not kept: nvme0 one boot can be another disk the next",
                       disk.name())
    })?;
    let mut text = String::new();
    text.try_reserve_exact(id.as_bytes().len()).map_err(|_| String::from("out of memory"))?;
    text.push_str(id.as_str());
    Ok((disk, text))
}

/// How a device is called in what is said of it: its name, and its id.
fn called(disk: &Disk) -> String {
    match disk.ident() {
        Some(id) => alloc::format!("{} ({})", disk.name(), id),
        None => alloc::format!("{}", disk.name()),
    }
}

/// What is on `disk`: its first `contents::HEAD` bytes and its last
/// `contents::TAIL` read and looked at (`diskimage::contents`).
pub fn holds(disk: &Disk) -> Result<Option<Holds>, String> {
    let ss = disk.sector_size();
    let size = disk.sectors().saturating_mul(ss);
    if ss == 0 || ss as usize > PAGE_SIZE || PAGE_SIZE as u64 % ss != 0 {
        return Err(alloc::format!("{} has sectors of {} bytes", disk.name(), ss));
    }
    let mut page = DmaBuffer::new(1).ok_or_else(|| String::from("out of memory"))?;
    let mut read = |from: u64, len: u64| -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        out.try_reserve_exact(len as usize).map_err(|_| String::from("out of memory"))?;
        let mut at = from;
        while at < from + len {
            let n = (from + len - at).min(PAGE_SIZE as u64) as usize;
            disk.read(at / ss, &mut page.as_mut_slice()[..n])
                .map_err(|e| alloc::format!("{}: a read at byte {} -- {}", disk.name(), at, e))?;
            out.extend_from_slice(&page.as_slice()[..n]);
            at += n as u64;
        }
        Ok(out)
    };
    let head_len = size.min(contents::HEAD as u64) / ss * ss;
    let head = read(0, head_len)?;
    let tail_len = size.min(contents::TAIL as u64) / ss * ss;
    let tail = read(size - tail_len, tail_len)?;
    Ok(contents::contents(&head, &tail, size))
}

/// The VMs' vm.tomls under `root`, read once for every device looked at:
/// each VM's name and the devices its vm.toml names, and the VMs whose
/// vm.toml could not be read -- which might name any device -- with why.
pub struct Named {
    vms: Vec<(String, Vec<String>)>,
    unread: Vec<(String, String)>,
}

impl Named {
    pub fn read(root: &str) -> Named {
        let mut named = Named { vms: Vec::new(), unread: Vec::new() };
        let Ok(entries) = kcore::fs::list(root) else { return named };
        for entry in entries.iter().filter(|e| e.dir) {
            let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
            if !store::valid_name(name) {
                continue;
            }
            /* No vm.toml: no VM's folder -- what a delete that kept the
             * disks left. */
            let Ok(folder) = store::folder_of(root, name) else { continue };
            match store::load(&folder) {
                Ok(config) => {
                    let words: Vec<String> = config.options.disks.into_iter().map(|(w, _)| w)
                        .filter(|w| is_device(w)).collect();
                    if named.vms.try_reserve(1).is_ok() {
                        named.vms.push((folder.name, words));
                    }
                }
                Err(why) => {
                    if named.unread.try_reserve(1).is_ok() {
                        named.unread.push((folder.name, why));
                    }
                }
            }
        }
        named
    }

    /// The VMs -- other than `vm` -- whose vm.toml names `disk` or a device
    /// overlapping it, each with the word it names it by.
    fn by(&self, vm: &str, disk: &Disk) -> Vec<(&str, &str)> {
        let mut found = Vec::new();
        for (name, words) in self.vms.iter().filter(|(n, _)| n != vm) {
            for word in words {
                if find(word).is_ok_and(|d| d.overlaps(disk)) && found.try_reserve(1).is_ok() {
                    found.push((name.as_str(), word.as_str()));
                }
            }
        }
        found
    }
}

/// What on `disk` a machine's firmware boots from -- the device itself, a
/// partition of it, or one overlapping it, an EFI System or a BIOS boot
/// partition -- if anything: that device, and what it is. Asked of the
/// device alone, the whole disk that held the machine's ESP on its other
/// drive could be given, and poured over.
pub fn boots_the_machine(disk: &Disk) -> Option<(Disk, &'static str)> {
    if let Some(what) = disk.kind().boots_the_machine() {
        return Some((*disk, what));
    }
    (0..Disk::count()).filter_map(Disk::at)
        .find_map(|d| d.kind().boots_the_machine().filter(|_| d.overlaps(disk)).map(|what| (d, what)))
}

/// Why `disk`, `called` so, is never given to a guest: the machine boots
/// from it or from a partition on it.
fn boots_said(called: &str, disk: &Disk) -> Option<String> {
    let (d, what) = boots_the_machine(disk)?;
    Some(if d.handle() == disk.handle() {
        alloc::format!("{} is {}: the machine boots from it, and no vm is given it", called, what)
    } else {
        alloc::format!("{} holds {}, {}: the machine boots from it, and no vm is given it", called, d.name(), what)
    })
}

/// Whether `disk` may be given to VM `vm` under `root`: Ok -- or why not.
/// Never where the machine boots from it or from a partition on it, or
/// something holds it or anything overlapping it; without `force`, not where
/// another VM's vm.toml names it or anything overlapping it, a VM being made
/// is being given it (`Vms::devices_given`), a VM's vm.toml cannot be read,
/// or anything is on it.
pub fn check(root: &str, vms: &Vms, vm: &str, disk: &Disk, force: bool) -> Result<(), String> {
    let named = if force { Named { vms: Vec::new(), unread: Vec::new() } } else { Named::read(root) };
    check_with(&named, vms, vm, disk, force)
}

/// `check`, with the vm.tomls read already.
pub fn check_with(named: &Named, vms: &Vms, vm: &str, disk: &Disk, force: bool) -> Result<(), String> {
    let called = called(disk);
    if let Some(why) = boots_said(&called, disk) {
        return Err(why);
    }
    /* Held now -- a mount, the disk log, a guest -- on it or on anything
     * overlapping it: asked, nothing taken, so that the look gets in no
     * start's way. */
    if let Some(by) = disk.in_the_way() {
        return Err(alloc::format!("{} is in use by {}", called, by));
    }
    if force {
        return Ok(());
    }
    if let Some((other, word)) = named.by(vm, disk).into_iter().next() {
        return Err(alloc::format!("{} is vm {}'s disk already (its vm.toml names {}) -- two vms on one device wreck \
                                   each other's data; give it anyway with force", called, other, word));
    }
    let given = vms.devices_given(vm);
    if let Some((other, _)) = given.iter().find(|(_, id)| find(id).is_ok_and(|d| d.overlaps(disk))) {
        return Err(alloc::format!("{} is being given to vm {}, which is being made -- two vms on one device wreck \
                                   each other's data; give it anyway with force", called, other));
    }
    if let Some((other, why)) = named.unread.iter().find(|(n, _)| n != vm) {
        return Err(alloc::format!("vm {}'s vm.toml cannot be read -- {}: it may name {}; mend it, or give it anyway \
                                   with force", other, why, called));
    }
    match holds(disk)? {
        Some(what) => Err(alloc::format!("{} holds {} -- the vm would have its data over it; give it anyway with \
                                          force", called, what)),
        None => Ok(()),
    }
}

/// What a partition table says a device is, in a few words.
fn kind_text(kind: PartKind) -> String {
    match kind {
        PartKind::Disk => String::from("whole disk"),
        PartKind::Gpt(guid) => match kind.boots_the_machine() {
            Some(what) => String::from(what),
            None => alloc::format!("GPT type {}", core::str::from_utf8(&kcore::block::guid_text(&guid)).unwrap_or("?")),
        },
        PartKind::Mbr(byte) => match kind.boots_the_machine() {
            Some(what) => String::from(what),
            None => alloc::format!("MBR type {:#04x}", byte),
        },
    }
}

/// A size, as `hv devices` says it.
fn size_text(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    if bytes >= 10 * GIB {
        alloc::format!("{} GiB", bytes / GIB)
    } else {
        alloc::format!("{} MiB", bytes / MIB)
    }
}

/// `hv devices`: every device nos has, what it is found by, what is in the
/// way of giving it -- what holds it, or anything overlapping it -- which VMs
/// name it or are being given it, and, of one nothing is in the way of, what
/// is on it. The vm.tomls read once, and one that cannot be read said: the
/// devices it names are not known.
pub fn command(root: &str, vms: &Vms, out: &mut Output) {
    let count = Disk::count();
    if count == 0 {
        let _ = writeln!(out, "hv: no block devices");
        return;
    }
    let named = Named::read(root);
    let given = vms.devices_given("");
    for index in 0..count {
        let Some(disk) = Disk::at(index) else { continue };
        let _ = write!(out, "{:<8} {:>9}  {}  {}", disk.name().as_str(),
                       size_text(disk.sectors().saturating_mul(disk.sector_size())),
                       disk.ident().as_ref().map_or("(no id)", |id| id.as_str()), kind_text(disk.kind()));
        if let Some((d, what)) = boots_the_machine(&disk).filter(|(d, _)| d.handle() != disk.handle()) {
            let _ = write!(out, "  -- holds {}, {}: never given", d.name(), what);
        }
        /* Who holds it, or -- what is as much in the way of giving it --
         * something it overlaps: the disk under nos's root partition. */
        let in_the_way = disk.in_the_way();
        match (disk.holder(), &in_the_way) {
            (Some(by), _) => {
                let _ = write!(out, "  -- held by {}", by);
            }
            (None, Some(by)) => {
                let _ = write!(out, "  -- in use by {}, on what it overlaps", by);
            }
            (None, None) => {}
        }
        for (vm, _) in named.by("", &disk) {
            let _ = write!(out, "  -- named by vm {}", vm);
        }
        for (vm, _) in given.iter().filter(|(_, id)| find(id).is_ok_and(|d| d.overlaps(&disk))) {
            let _ = write!(out, "  -- being given to vm {}", vm);
        }
        if in_the_way.is_none() {
            match holds(&disk) {
                Ok(Some(what)) => {
                    let _ = write!(out, "  -- holds {}", what);
                }
                Ok(None) => {
                    let _ = write!(out, "  -- empty");
                }
                Err(e) => {
                    let _ = write!(out, "  -- {}", e);
                }
            }
        }
        let _ = writeln!(out);
    }
    for (vm, why) in &named.unread {
        let _ = writeln!(out, "hv: vm {}'s vm.toml cannot be read -- {}: which devices it names is not known", vm, why);
    }
}

const DISK_USAGE: &str = "hv disk attach <vm> <device>[:ro] [force] | hv disk detach <vm> <device> -- a device is \
                          dev:<name>, part:<id> or disk:<id> (hv devices)";

/// `hv disk attach <vm> <device>[:ro] [force]`, `hv disk detach <vm> <device>`.
pub fn disk_command(root: &str, vms: &Vms, args: &str, out: &mut Output) {
    let mut words = args.split_ascii_whitespace();
    let (Some(what), Some(vm), Some(word)) = (words.next(), words.next(), words.next()) else {
        let _ = writeln!(out, "{}", DISK_USAGE);
        return;
    };
    let force = match (words.next(), words.next()) {
        (None, _) => false,
        (Some("force"), None) if what == "attach" => true,
        _ => {
            let _ = writeln!(out, "{}", DISK_USAGE);
            return;
        }
    };
    let done = match what {
        "attach" => crate::config::disk(word).ok_or_else(|| String::from(DISK_USAGE))
            .and_then(|(device, ro)| attach(root, vms, vm, &device, ro, force))
            .map(|id| alloc::format!("{} is {}'s disk now, from its next start", id, vm)),
        "detach" => detach(root, vms, vm, word).map(|id| alloc::format!("{} taken off {}'s vm.toml -- the device is \
                                                                         left as it is", id, vm)),
        _ => Err(String::from(DISK_USAGE)),
    };
    match done {
        Ok(said) => {
            let _ = writeln!(out, "hv: {}", said);
        }
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
        }
    }
}

/// The device `word` names made a disk of VM `vm`'s, read-only to it with
/// `ro`, where `check` lets it be given: written into its vm.toml by its id,
/// read at its next start. The id, or why not. The check is made under the
/// lock the write is (`store::update`), so that two VMs given one device at
/// once are not both told it is nobody's.
pub fn attach(root: &str, vms: &Vms, vm: &str, word: &str, ro: bool, force: bool) -> Result<String, String> {
    let folder = store::folder_of(root, vm)?;
    /* Held while its vm.toml changes -- refused while it is on the list:
     * hv stop it first. */
    let _held = vms.reserve(vm, crate::vms::CHANGED)?;
    let (disk, id) = given(word)?;
    store::update(vms, &folder, |c| {
        check(root, vms, vm, &disk, force)?;
        for (other, _) in c.options.disks.iter().filter(|(w, _)| is_device(w)) {
            if find(other).is_ok_and(|d| d.overlaps(&disk)) {
                return Err(alloc::format!("{} is {}'s disk already, as {}", called(&disk), vm, other));
            }
        }
        if c.options.disks.len() >= hv::run::MAX_DISKS {
            return Err(alloc::format!("{} has {} disks already, all a guest takes", vm, hv::run::MAX_DISKS));
        }
        c.options.disks.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        c.options.disks.push((id.clone(), ro));
        Ok(())
    })?;
    Ok(id)
}

/// The device `word` names -- by its id, or by its name or id resolved to
/// the device -- taken off VM `vm`'s vm.toml: what its vm.toml named it by,
/// or why not. The device itself is left as it is.
pub fn detach(root: &str, vms: &Vms, vm: &str, word: &str) -> Result<String, String> {
    let folder = store::folder_of(root, vm)?;
    /* Held while its vm.toml changes -- refused while it is on the list:
     * hv stop it first. */
    let _held = vms.reserve(vm, crate::vms::CHANGED)?;
    let wanted = given(word).ok().map(|(disk, _)| disk);
    let mut named = None;
    store::update(vms, &folder, |c| {
        let at = c.options.disks.iter().position(|(w, _)| {
            is_device(w) && (w.eq_ignore_ascii_case(word)
                             || wanted.as_ref().is_some_and(|d| find(w).is_ok_and(|x| x.handle() == d.handle())))
        });
        match at {
            Some(i) => {
                named = Some(c.options.disks.remove(i).0);
                Ok(())
            }
            None => Err(alloc::format!("{}'s vm.toml names no device {}", vm, word)),
        }
    })?;
    named.ok_or_else(|| alloc::format!("{}'s vm.toml names no device {}", vm, word))
}
