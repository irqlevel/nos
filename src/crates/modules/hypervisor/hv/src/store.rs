//! VMs kept in folders: `/vms/<name>/` -- or under the folder `insmod
//! /hv.ko vms=<folder>` names -- a `vm.toml` there saying what to boot the
//! VM with (`config`), its kernel and disks beside it -- the file's paths
//! are the folder's unless they begin with `/` -- and its console's log
//! (`vmlog`). `hv start <name>` boots one; `hv create` makes a folder and
//! its file from the words `hv start` takes; `hv vms` says what the folders
//! have, and which of them run.
//!
//! The name is the folder's, and what the commands know a VM by besides its
//! number: one VM at a time runs from a folder -- a second `hv start` of a
//! name already on the list is refused -- and its disks are claimed as any
//! guest's are, so no other guest writes them meanwhile. While a folder is
//! made, cloned or deleted its name is held (`Vms::reserve`): no VM of it
//! starts, and nothing else of the kind is done to it. What a VM being made
//! or cloned is given before its vm.toml says so -- a port of the switch, a
//! device of nos's -- is kept with that hold, and no other VM is given it
//! meanwhile.
//!
//! What hv changes in a vm.toml after it is made -- a forward added or taken
//! away, a disk, the UUID of a first start, the host API's edits -- it
//! changes key by key (`config::rewrite`), the operator's comments and lines
//! left as they are, and the file as it was kept beside it as
//! `vm.toml.prev`: one change at a time (`Changes`), and none of a folder
//! being deleted. `hv delete` takes a folder away, its disks too unless told
//! to keep them.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;

use crate::config::{self, Config, Options};
use crate::guest;
use crate::net;
use crate::vmlog;
use crate::vms::Vms;
use crate::work::Work;

/// Where the VMs' folders are, unless `insmod` was told otherwise (`vms=`).
pub const DEFAULT_ROOT: &str = "/vms";
/// A folder's configuration, and what it was before hv last changed it.
pub const CONFIG: &str = "vm.toml";
pub const CONFIG_PREV: &str = "vm.toml.prev";
/// The longest a VM's name may be.
pub const NAME_MAX: usize = 32;
/// The longest path the VFS takes (`fs::vfs::MAX_PATH`, less one).
pub const PATH_MAX: usize = 255;
/// What a file is written as before it takes the place of the one there
/// (`kcore::fs::write`).
const REPLACING: &str = ".new";
/// The longest name hv writes in a VM's folder: the console's log moved
/// aside, or vm.toml.prev on its way in.
const LONGEST_NAME: usize = if vmlog::LONGEST_NAME > CONFIG_PREV.len() + REPLACING.len() {
    vmlog::LONGEST_NAME
} else {
    CONFIG_PREV.len() + REPLACING.len()
};
/// The longest the VMs' folder's path may be: room under it for a VM's
/// folder and the longest name hv writes there.
const ROOT_MAX: usize = PATH_MAX - (1 + NAME_MAX + 1 + LONGEST_NAME);
/// How deep `delete` goes into a VM's folder.
const DELETE_DEPTH: usize = 8;

pub(crate) const CREATE_USAGE: &str = "hv create <name> <kernel> [mem=MiB] [cpus=N] [cpu=N] [xapic] [ioapic] \
                            [apicv|noapicv] [rxmod=US] [nooffload] [initrd=path] [disk=<path|device>[:ro]]... \
                            [input=...] [log] [restart] [net] [port=N] [forward=nos:guest]... [force] \
                            [cmdline=...]\n\
                            hv create <name> <http(s)://image> [size=N[K|M|G|T] | onto=<device|nbs:volume>] [password=...] \
                            [hvboot=path] [force] [the words hv start takes, but a kernel and initrd=]\n\
                            hv create <name> image=nbs:<image> [size=N[K|M|G|T]] [onto=nbs:<volume>] [password=...] \
                            [hvboot=path] [the words hv start takes, but a kernel and initrd=]\n\
                            a device is a partition or a whole disk of nos's: dev:<name>, part:<id> or disk:<id> \
                            (hv devices); force gives one over what is on it";

/// A VM's folder: the name it is known by, and its path.
pub struct Folder {
    pub name: String,
    pub path: String,
}

/// Whether `name` may be a VM's: a letter or a digit, then letters, digits,
/// `.`, `_` and `-`, at most `NAME_MAX` in all -- a folder's name, and a
/// word of a command, with nothing in it a path or a shell reads otherwise
/// -- and not a word the commands read as something else (`reserved`).
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= NAME_MAX && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && !reserved(name)
}

/// What a VM's name may not be, a word the commands read otherwise: digits
/// alone, a VM's number -- which `hv stop`, `exec`, `send` and the rest
/// take as well as a name, and look up first, so that a VM named `1` was
/// reached as whichever VM was number 1 -- and `all`, every VM to `hv stop`.
pub fn reserved(name: &str) -> bool {
    (!name.is_empty() && name.bytes().all(|c| c.is_ascii_digit())) || name == "all"
}

/// The VMs' folder `vms=` names, as hv keeps it: an absolute path, with no
/// `.`, `..` or empty step in it, its trailing `/` let go -- and not `/`
/// itself, whose every folder would be taken for a VM's. What is wrong
/// with it otherwise, to follow the path.
pub fn root(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err(String::from("is not an absolute path"));
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err(String::from("is / -- every folder of it would be taken for a vm's; the vms want one of their own"));
    }
    if path.len() > ROOT_MAX {
        return Err(alloc::format!("is longer than {} bytes, which leaves no room under it for a vm's folder", ROOT_MAX));
    }
    if let Some(step) = path[1..].split('/').find(|s| s.is_empty() || *s == "." || *s == ".." || s.contains(char::is_control)) {
        return Err(alloc::format!("has \"{}\" in it, which is no folder's name", step));
    }
    let mut kept = String::new();
    kept.try_reserve_exact(path.len()).map_err(|_| String::from("out of memory"))?;
    kept.push_str(path);
    Ok(kept)
}

/// Whether `file` is a name hv itself writes in a VM's folder: its vm.toml
/// and vm.toml.prev -- and the `.new` each is written through first
/// (`kcore::fs::write`) -- the user-data kept for a clone, and the console's
/// logs. No disk or download is given one: a disk added as `vm.toml.new`
/// was written over by the next change of the vm.toml.
pub fn folders_own(file: &str) -> bool {
    let stem = file.strip_suffix(REPLACING).unwrap_or(file);
    [CONFIG, CONFIG_PREV, crate::cloud::USER_DATA].contains(&stem)
        || stem.strip_prefix(vmlog::FILE).is_some_and(|rest| rest.is_empty() || rest == vmlog::OLD_SUFFIX)
}

/// Makes the folder, and the folders above it that are missing.
pub(crate) fn make_dirs(path: &str) -> kcore::error::Result<()> {
    for (i, _) in path.match_indices('/').skip(1) {
        kcore::fs::create_dir(&path[..i])?;
    }
    kcore::fs::create_dir(path)
}

/// `<dir>/<file>`, or None out of memory.
pub(crate) fn join(dir: &str, file: &str) -> Option<String> {
    let mut s = String::new();
    s.try_reserve(dir.len() + file.len() + 1).ok()?;
    s.push_str(dir.trim_end_matches('/'));
    s.push('/');
    s.push_str(file);
    Some(s)
}

/// The folder the first word of `hv start` names, if it names one: a VM's
/// name -- `<root>/<name>` -- or the path of a folder with a vm.toml in it.
/// `Ok(None)` for what is no folder, a kernel's path; an error for a name
/// with no folder.
pub fn find(root: &str, word: &str) -> Result<Option<Folder>, String> {
    if word.contains('=') {
        return Ok(None);
    }
    let path = if word.starts_with('/') {
        String::from(word.trim_end_matches('/'))
    } else if valid_name(word) {
        join(root, word).ok_or_else(|| String::from("out of memory"))?
    } else if reserved(word) {
        /* No kernel's path, and no VM's name: a folder by that name is a
         * VM's from before names were held to this, said so */
        return Err(name_refused(word).unwrap_or_default() + " -- give its folder another name");
    } else {
        return Ok(None);
    };
    let file = join(&path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    if kcore::fs::size(&file).is_err() {
        if word.starts_with('/') {
            return Ok(None);
        }
        return Err(alloc::format!("no vm {}: there is no {} -- hv create {} <kernel> ... makes one", word, file, word));
    }
    let name = path.rsplit('/').next().unwrap_or("");
    if let Some(why) = name_refused(name) {
        return Err(alloc::format!("{} has a {}, but {}", path, CONFIG, why));
    }
    Ok(Some(Folder { name: String::from(name), path }))
}

/// The vm.toml changes this module makes, one at a time: what `update`,
/// `replace` and `keep_uuid` take for a file's read, change and write
/// together. Two at once -- a forward kept and a disk added, through the
/// bus, whose calls run side by side -- read the same text, and the second
/// write took the first's change away, with vm.toml.prev no record of it.
/// One for every folder: a change is an operator's, rare and quick. Held by
/// `Vms`, so that whatever changes a vm.toml has one to give. What is given
/// to a VM before its vm.toml names it -- a port, a device -- is looked for
/// and kept under it too (`take_port`, `give_devices`), as a delete takes
/// its hold of the name (`delete`).
pub struct Changes(kcore::sync::Mutex<()>);

impl Changes {
    pub fn new() -> Option<Changes> {
        kcore::sync::Mutex::new(()).map(Changes)
    }
}

/// The changes' lock taken for a change of `folder`'s vm.toml -- refused
/// while the folder is being deleted: a delete takes its hold of the name
/// under this lock (`delete`), so a change either comes first, and what it
/// wrote is in the delete's walk, or sees the delete and writes nothing. A
/// change beside a delete once wrote vm.toml.prev into a folder the delete
/// had walked, which was then left with that alone in it.
fn changing<'a>(vms: &'a Vms, folder: &Folder) -> Result<kcore::sync::MutexGuard<'a, ()>, String> {
    let one = vms.changes.0.lock();
    if vms.doing(&folder.name) == Some(crate::vms::DELETED) {
        return Err(alloc::format!("{} is being deleted -- its {} is not changed", folder.name, CONFIG));
    }
    Ok(one)
}

/// The UUID of the VM in `folder` from its vm.toml, or -- one whose
/// vm.toml was written before VMs had them -- `uuid`, written into it key
/// by key as every change is, for every start after to give it the same:
/// the one the file has. Two first starts at once thus agree on one. It was
/// appended to the file, which a write cut short -- a disk full -- left
/// unparseable, as two starts did by appending one each.
pub fn keep_uuid(vms: &Vms, folder: &Folder, uuid: [u8; 16]) -> Result<[u8; 16], String> {
    let _one = changing(vms, folder)?;
    let (text, was) = load_text(folder)?;
    if let Some(had) = was.options.uuid {
        return Ok(had);
    }
    let mut c = was.clone();
    c.options.uuid = Some(uuid);
    let file = join(&folder.path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    let new = config::rewrite(&text, &c).map_err(|e| alloc::format!("{}: {}", file, e))?;
    write_config(folder, text.as_bytes(), &new)?;
    Ok(uuid)
}

/// The folder's configuration, read and parsed; what is wrong with it
/// otherwise, said with its path.
pub fn load(folder: &Folder) -> Result<Config, String> {
    load_text(folder).map(|(_, c)| c)
}

/// The folder's vm.toml as its bytes -- what is wrong otherwise, said with
/// its path -- and that path.
fn read_config(folder: &Folder) -> Result<(String, Vec<u8>), String> {
    let file = join(&folder.path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    match kcore::fs::read(&file, config::MAX_BYTES) {
        Ok(bytes) => Ok((file, bytes)),
        Err(kcore::error::Error::InvalidValue) => {
            Err(alloc::format!("{} is more than the {} bytes a vm.toml may be", file, config::MAX_BYTES))
        }
        Err(e) => Err(alloc::format!("{}: {}", file, e)),
    }
}

/// The folder's vm.toml as it is written, and the configuration it says.
pub fn load_text(folder: &Folder) -> Result<(String, Config), String> {
    let (file, bytes) = read_config(folder)?;
    let text = String::from_utf8(bytes).map_err(|_| alloc::format!("{} is not UTF-8 text", file))?;
    let c = config::parse(&text).map_err(|e| alloc::format!("{}: {}", file, e))?;
    Ok((text, c))
}

/// The folder's vm.toml made `text`, which was `was`: the old kept as
/// vm.toml.prev, and each file replaced whole -- a full disk or a crash
/// midway leaves it as it was, never half written (`kcore::fs::write`).
/// Under `Changes`, which its callers here hold.
fn write_config(folder: &Folder, was: &[u8], text: &str) -> Result<(), String> {
    let file = join(&folder.path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    let prev = join(&folder.path, CONFIG_PREV).ok_or_else(|| String::from("out of memory"))?;
    kcore::fs::write(&prev, was).map_err(|e| alloc::format!("{} could not be written -- {}", prev, e))?;
    kcore::fs::write(&file, text.as_bytes()).map_err(|e| alloc::format!("{} could not be written -- {}", file, e))
}

/// The folder's vm.toml made `text` whole -- the bus's `config_set`, which
/// has checked it as a start would: whether it changed. The one there is
/// read as bytes, and kept as vm.toml.prev as it is: the vm.toml an editor
/// is needed for most is one that does not parse -- a key misspelt by hand,
/// or one this build does not have -- which was refused with its own parse
/// error whatever was sent.
pub fn replace(vms: &Vms, folder: &Folder, text: &str) -> Result<bool, String> {
    let _one = changing(vms, folder)?;
    let (_, was) = read_config(folder)?;
    if was == text.as_bytes() {
        return Ok(false);
    }
    write_config(folder, &was, text)?;
    Ok(true)
}

/// The folder's vm.toml changed by `change` -- checked as a start checks
/// it, and written key by key (`config::rewrite`), the operator's lines left
/// as they are: whether it changed. Nothing is written when `change` leaves
/// it as it was. The read, the change and the write under the changes' lock
/// (`changing`): what `change` checks -- that no other VM's vm.toml names a
/// device it adds -- holds for the write.
pub fn update(vms: &Vms, folder: &Folder, change: impl FnOnce(&mut Config) -> Result<(), String>)
    -> Result<bool, String>
{
    let _one = changing(vms, folder)?;
    let (text, was) = load_text(folder)?;
    let mut c = was.clone();
    change(&mut c)?;
    if c == was {
        return Ok(false);
    }
    let file = join(&folder.path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    options_checked(folder, &c).map_err(|e| alloc::format!("{}: {}", file, e))?;
    let new = config::rewrite(&text, &c).map_err(|e| alloc::format!("{}: {}", file, e))?;
    write_config(folder, text.as_bytes(), &new)?;
    Ok(true)
}

/// The folder of the VM `name` under `root`, when it has a vm.toml: what
/// is wrong otherwise.
pub fn folder_of(root: &str, name: &str) -> Result<Folder, String> {
    if let Some(why) = name_refused(name) {
        return Err(why);
    }
    let path = join(root, name).ok_or_else(|| String::from("out of memory"))?;
    let file = join(&path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    if kcore::fs::size(&file).is_err() {
        return Err(alloc::format!("no vm {}: there is no {}", name, file));
    }
    Ok(Folder { name: String::from(name), path })
}

/// A path of the configuration's, as nos reaches it: the folder's unless it
/// begins with `/` -- or names a device or a volume (`devices::source`),
/// which is no path at all.
fn resolve(folder: &Folder, path: &str) -> Result<String, String> {
    if path.starts_with('/') || !crate::devices::is_file(path) {
        return Ok(String::from(path));
    }
    join(&folder.path, path).ok_or_else(|| String::from("out of memory"))
}

/// What the folder's configuration says of its guest, its paths made nos's.
pub fn options(folder: &Folder, c: &Config) -> Result<Options, String> {
    let mut o = c.options.clone();
    o.kernel = resolve(folder, &o.kernel)?;
    if let Some(initrd) = &o.initrd {
        o.initrd = Some(resolve(folder, initrd)?);
    }
    for (path, _) in o.disks.iter_mut() {
        *path = resolve(folder, path)?;
    }
    Ok(o)
}

/// Why `name` is no VM's, or None for a name a VM may have.
pub fn name_refused(name: &str) -> Option<String> {
    if reserved(name) {
        return Some(alloc::format!("\"{}\" is not a name a vm can have -- digits alone are a vm's number to hv's \
                                    commands, and all is every vm", name));
    }
    (!valid_name(name)).then(|| alloc::format!("\"{}\" is not a name a vm can have -- a letter or a digit, then \
                                                letters, digits, . _ and -, at most {}", name, NAME_MAX))
}

/// `hv create <name> <words>`: the folder made under `root`, and its
/// vm.toml written from the words -- `hv start`'s, and its paths the
/// folder's unless they begin with `/` -- never over one that is there.
pub fn create(root: &str, vms: &Vms, args: &str, work: &mut dyn Work) {
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    let (name, rest) = (&args[..end], args[end..].trim_start());
    if name.is_empty() || rest.is_empty() {
        let _ = writeln!(work, "{}", CREATE_USAGE);
        return;
    }
    if let Some(why) = name_refused(name) {
        let _ = writeln!(work, "hv: {}", why);
        return;
    }
    /* `force`: each device given over what is on it, or another VM's. */
    let Some((rest, force)) = config::take_flag(rest, "force") else {
        let _ = writeln!(work, "hv: out of memory");
        return;
    };
    let rest = rest.as_str();
    /* A cloud's image, by its URL: downloaded, made a disk, given a seed --
     * or an image of NBS's (N10d3), cloned into the VM's own volume. */
    let first = rest.split_ascii_whitespace().next().unwrap_or("");
    if crate::cloud::is_url(first) || crate::cloud::is_image(first) {
        crate::cloud::command(root, vms, name, first, rest[first.len()..].trim_start(), force, work);
        return;
    }
    let out = work;
    let given = match guest::words(rest, CREATE_USAGE) {
        Ok(o) if o.secs.is_none() => o,
        Ok(_) => {
            let _ = writeln!(out, "hv: secs= is hv boot's -- a vm runs until hv stop");
            return;
        }
        Err(e) => {
            let _ = writeln!(out, "{}", e);
            return;
        }
    };
    let (Some(path), Some(file)) = (join(root, name), join(root, name).and_then(|p| join(&p, CONFIG))) else {
        let _ = writeln!(out, "hv: out of memory");
        return;
    };
    let folder = Folder { name: String::from(name), path };
    let mut given = given;
    /* There already: said as such, before the name is asked for -- which a
     * VM of it running would refuse. */
    if kcore::fs::size(&file).is_ok() {
        let _ = writeln!(out, "hv: {} is there already -- not written over", file);
        return;
    }
    /* Held while the folder is made: no start of it meanwhile, nor a
     * clone or delete -- and what it is given, kept with the hold. */
    let held = match vms.reserve(name, crate::vms::MADE) {
        Ok(held) => held,
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
            return;
        }
    };
    /* Each device by its id from here on, only where it may be given, and
     * no other VM's while this is made; a port of its own no other VM's. */
    let gave = give_devices(root, vms, &held, &mut given.disks, None, force)
        .and_then(|()| given.port.map_or(Ok(()), |p| take_port(root, vms, &held, Some(p)).map(|_| ())));
    if let Err(e) = gave {
        let _ = writeln!(out, "hv: {}", e);
        return;
    }
    /* Its UUID, made now: the machine its OS knows it as, at every start. */
    given.uuid = given.uuid.or_else(|| Some(guest::random_uuid()));
    /* Checked as a start would check it -- the paths the folder's -- before
     * anything is written: a vm.toml that cannot boot is no use kept. */
    if let Err(e) = options_checked(&folder, &Config::new(given.clone())) {
        let _ = writeln!(out, "hv: {}", e);
        return;
    }
    let config = Config::new(given);
    let Some(text) = config::render(&config) else {
        let _ = writeln!(out, "hv: out of memory");
        return;
    };
    if make_dirs(&folder.path).is_err() {
        let _ = writeln!(out, "hv: {} could not be made", folder.path);
        return;
    }
    match kcore::fs::create(&file, text.as_bytes()) {
        Ok(()) => {}
        Err(kcore::error::Error::Busy) => {
            let _ = writeln!(out, "hv: {} is there already -- not written over", file);
            return;
        }
        Err(e) => {
            let _ = writeln!(out, "hv: {} could not be written -- {}", file, e);
            return;
        }
    }
    let _ = writeln!(out, "hv: {} written -- hv start {} boots it", file, name);
    /* What it names that is not there yet: a disk is often made after. */
    if let Ok(o) = options(&folder, &config) {
        let mut missing = Vec::new();
        missing.push(o.kernel.as_str());
        missing.extend(o.initrd.as_deref());
        missing.extend(o.disks.iter().map(|(p, _)| p.as_str()).filter(|p| crate::devices::is_file(p)));
        for path in missing {
            if kcore::fs::size(path).is_err() {
                let _ = writeln!(out, "hv: {} is not there yet", path);
            }
        }
    }
}

/// The folder's configuration as a start checks it: the guest it says, its
/// paths made nos's, given its defaults and held to every rule -- and no
/// device named by this boot's name (`dev:nvme1`), which the next boot may
/// give another disk: a vm.toml written by hand that said so was started as
/// it said, and after a reboot that numbered the drives the other way round
/// wrote over the other drive. The id that finds it again is said.
pub fn options_checked(folder: &Folder, c: &Config) -> Result<guest::Spec, String> {
    let by_name = c.options.disks.iter().map(|(w, _)| w.as_str())
        .find(|w| matches!(crate::devices::source(w), crate::devices::Source::Name(_)));
    if let Some(word) = by_name {
        return Err(match crate::devices::given(word) {
            Ok((_, id)) => alloc::format!("its disk {} is this boot's name for a device, which the next boot may give \
                                           another disk -- name it by its id: {}", word, id),
            Err(e) => alloc::format!("its disk {} is a device by this boot's name, which the next boot may give \
                                      another disk -- name it by its id (hv devices): {}", word, e),
        });
    }
    guest::spec(options(folder, c)?)
}

/// The devices among `disks` given as they are -- an id kept, a name made
/// its device's id (`devices::given`) -- and `onto`, the device a cloud's
/// image is poured onto, with its id: each where `devices::check` lets it be
/// given to the VM `held` holds the name of, under `root`, and no two of
/// them on one stretch of a disk -- why not, otherwise. Looked at under the
/// changes' lock, and their ids kept with `held` until its vm.toml names
/// them (`Reserved::keep_devices`): two VMs made at once, each finding no
/// vm.toml that named the device, were both given it.
pub fn give_devices(root: &str, vms: &Vms, held: &crate::vms::Reserved, disks: &mut [(String, bool)],
                    onto: Option<(&kcore::block::Disk, &str)>, force: bool) -> Result<(), String> {
    let oom = || String::from("out of memory");
    let vm = held.name();
    let mut given: Vec<kcore::block::Disk> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    given.try_reserve_exact(disks.len()).map_err(|_| oom())?;
    ids.try_reserve_exact(disks.len() + 1).map_err(|_| oom())?;
    let _one = vms.changes.0.lock();
    if let Some((disk, id)) = onto {
        crate::devices::check(root, vms, vm, disk, force)?;
        let mut kept = String::new();
        kept.try_reserve_exact(id.len()).map_err(|_| oom())?;
        kept.push_str(id);
        ids.push(kept);
    }
    for (word, _) in disks.iter_mut() {
        if !crate::devices::is_device(word) {
            continue;
        }
        let (disk, id) = crate::devices::given(word)?;
        if onto.is_some_and(|(o, _)| o.overlaps(&disk)) {
            return Err(String::from("onto= is one of its disk= too, or overlaps one"));
        }
        if given.iter().any(|d| d.overlaps(&disk)) {
            return Err(alloc::format!("{} is given twice, or overlaps another of its disks", word));
        }
        crate::devices::check(root, vms, vm, &disk, force)?;
        let mut kept = String::new();
        kept.try_reserve_exact(id.len()).map_err(|_| oom())?;
        kept.push_str(&id);
        ids.push(kept);
        given.push(disk);
        *word = id;
    }
    held.keep_devices(ids);
    Ok(())
}

/// A port of the switch for the VM `held` holds the name of, being made or
/// cloned: `asked`, or the lowest no VM's vm.toml under `root` names, no VM
/// on the list holds and no other make or clone has taken -- kept with
/// `held` until its vm.toml says so. Looked for and kept under the changes'
/// lock: two clones at once were each given the lowest free one, the same,
/// and with it one MAC and one address.
pub fn take_port(root: &str, vms: &Vms, held: &crate::vms::Reserved, asked: Option<u32>) -> Result<u32, String> {
    let _one = vms.changes.0.lock();
    let port = port_for(root, vms, asked)?;
    held.keep_port(port);
    Ok(port)
}

/// `asked`, or the lowest port no VM's vm.toml under `root` names and no
/// VM holds or is being given (`Vms::ports_held`). No VMs' folder yet -- the
/// first VM made, which makes it -- is no vm.toml naming any: it was "not
/// found", and the first `hv create ... port=N` refused.
fn port_for(root: &str, vms: &Vms, asked: Option<u32>) -> Result<u32, String> {
    let mut used = [false; net::MAX_PORTS];
    for p in vms.ports_held().ok_or_else(|| String::from("out of memory"))? {
        if let Some(u) = used.get_mut(p) {
            *u = true;
        }
    }
    let entries = match kcore::fs::list(root) {
        Ok(entries) => entries,
        Err(kcore::error::Error::NotFound) if kcore::fs::size(root).is_err() => Vec::new(),
        Err(e) => return Err(alloc::format!("{}: {}", root, e)),
    };
    let mut named: Vec<(u32, String)> = Vec::new();
    for entry in entries {
        let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
        if !entry.dir || !valid_name(name) {
            continue;
        }
        let Ok(folder) = folder_of(root, name) else { continue };
        if let Ok(c) = load(&folder) {
            if let Some(p) = c.options.port {
                if let Some(u) = used.get_mut(p as usize) {
                    *u = true;
                }
                named.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                named.push((p, folder.name));
            }
        }
    }
    match asked {
        Some(p) if p as usize >= net::MAX_PORTS => Err(alloc::format!("port= must be 0..{}", net::MAX_PORTS - 1)),
        Some(p) => match named.iter().find(|(q, _)| *q == p) {
            Some((_, owner)) => Err(alloc::format!("port {} is {}'s, as its vm.toml says", p, owner)),
            None if used[p as usize] => Err(alloc::format!("port {} is held by a vm that runs, or one being made", p)),
            None => Ok(p),
        },
        None => used.iter().position(|u| !u).map(|p| p as u32)
            .ok_or_else(|| alloc::format!("every one of the switch's {} ports is some vm's", net::MAX_PORTS)),
    }
}

/// What `delete` did: how many files and folders it took away, and the
/// disks it kept.
pub struct Deleted {
    /// Its devices of nos's and its volumes of NBS's, which a delete never
    /// touches.
    pub devices: Vec<String>,
    pub removed: usize,
    /// The disks kept, by the paths the folder has them at.
    pub kept: Vec<String>,
}

pub const DELETE_USAGE: &str = "hv delete <name> [keep-disks] [force]";

/// `hv delete <name> [keep-disks] [force]`.
pub fn delete_command(root: &str, vms: &Vms, args: &str, out: &mut dyn Write) {
    let mut words = args.split_ascii_whitespace();
    let Some(name) = words.next() else {
        let _ = writeln!(out, "{}", DELETE_USAGE);
        return;
    };
    let (mut keep, mut force) = (false, false);
    for word in words {
        match word {
            "keep-disks" if !keep => keep = true,
            "force" if !force => force = true,
            _ => {
                let _ = writeln!(out, "{}", DELETE_USAGE);
                return;
            }
        }
    }
    match delete(root, vms, name, keep, force) {
        Ok(d) => {
            let _ = writeln!(out, "hv: {} deleted -- {} files and folders taken away", name, d.removed);
            for disk in &d.kept {
                let _ = writeln!(out, "hv: {} kept", disk);
            }
            for device in &d.devices {
                let what = match crate::devices::source(device) {
                    crate::devices::Source::Volume(_) => "a volume of NBS's",
                    _ => "a device of nos's",
                };
                let _ = writeln!(out, "hv: {} left as it is -- {}, which a delete never touches", device, what);
            }
        }
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
        }
    }
}

/// The VM `name`'s folder under `root` taken away: every file in it and
/// under it -- or, with `keep`, all but the disks its vm.toml names in it,
/// which stay where they are -- its vm.toml last, so that a delete that
/// fails part way is left for another, and the folder once nothing is left
/// in it. Refused while a VM of the name is on the list, or is being made or
/// cloned; and for a protected VM -- or one whose vm.toml cannot be read,
/// which may be -- unless `force`. Disks its vm.toml names outside the
/// folder are not touched, and nothing is followed out of it.
pub fn delete(root: &str, vms: &Vms, name: &str, keep: bool, force: bool) -> Result<Deleted, String> {
    let folder = folder_of(root, name)?;
    /* Held under the changes' lock: a change of its vm.toml made before is
     * done -- what it wrote is in the walk below -- and one after sees the
     * hold and writes nothing (`changing`). */
    let _held = {
        let _one = vms.changes.0.lock();
        vms.reserve(name, crate::vms::DELETED)?
    };
    let oom = || String::from("out of memory");
    let (protected, disks) = match load(&folder) {
        Ok(c) => (c.protected, options(&folder, &c)?.disks),
        Err(e) if force && !keep => {
            let _ = e;
            (false, Vec::new())
        }
        Err(e) if keep => return Err(alloc::format!("{} -- which files are its disks is not known", e)),
        Err(e) => return Err(alloc::format!("{} -- force deletes it all the same", e)),
    };
    if protected && !force {
        return Err(alloc::format!("{} is protected -- force deletes it all the same", name));
    }
    let config = join(&folder.path, CONFIG).ok_or_else(oom)?;
    let devices: Vec<String> = disks.iter().map(|(p, _)| p).filter(|p| !crate::devices::is_file(p)).cloned().collect();
    /* The disks to keep, as files: which a file of the folder's is, is told
     * by the file, not by how the vm.toml spells its path -- the VFS reads
     * `./root.img`, `sub/../root.img` and a doubled slash as the file the
     * walk below names `root.img`, and a delete that compared the strings
     * took away the disk it then said it kept. Each held open for the
     * walk: an identity is the same for two opens of one file while
     * either is open. One the vm.toml names that is not there, or is
     * outside the folder, is in no walk and keeps nothing. */
    let mut keeping: Vec<(kcore::fs::File, kcore::fs::FileId)> = Vec::new();
    if keep {
        for (path, _) in disks.iter().filter(|(p, _)| crate::devices::is_file(p)) {
            let Ok(file) = kcore::fs::File::open(path, false) else { continue };
            if let Some(id) = file.identity() {
                keeping.try_reserve(1).map_err(|_| oom())?;
                keeping.push((file, id));
            }
        }
    }
    let is_kept = |path: &str| !keeping.is_empty()
        && kcore::fs::File::open(path, false).ok().and_then(|f| f.identity())
            .is_some_and(|id| keeping.iter().any(|(_, k)| *k == id));
    let mut kept: Vec<String> = Vec::new();

    /* What is in it, every level down: its files, and its folders with how
     * deep each is. */
    let mut files = Vec::new();
    let mut folders: Vec<(String, usize)> = Vec::new();
    let mut walk = Vec::new();
    walk.push((folder.path.clone(), 0usize));
    while let Some((dir, depth)) = walk.pop() {
        let entries = kcore::fs::list(&dir).map_err(|e| alloc::format!("{}: {}", dir, e))?;
        for entry in entries {
            let Ok(n) = core::str::from_utf8(&entry.name) else {
                return Err(alloc::format!("{} has a file whose name is not text in it -- take it away by hand", dir));
            };
            let path = join(&dir, n).ok_or_else(oom)?;
            if entry.dir {
                if depth + 1 >= DELETE_DEPTH {
                    return Err(alloc::format!("{} is more than {} folders deep -- take it away by hand", path,
                                              DELETE_DEPTH));
                }
                folders.push((path.clone(), depth + 1));
                walk.push((path, depth + 1));
            } else if path == config {
                continue;
            } else if is_kept(&path) {
                kept.try_reserve(1).map_err(|_| oom())?;
                kept.push(path);
            } else {
                files.push(path);
            }
        }
    }
    let left = |what: &str| alloc::format!("{} could not be taken away -- open, or held as a disk? {} is left, for a \
                                            delete again", what, config);
    let mut removed = 0;
    for f in &files {
        kcore::fs::remove(f).map_err(|_| left(f))?;
        removed += 1;
    }
    /* The folders in it, deepest first -- but those that hold a disk kept. */
    folders.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    for (dir, _) in &folders {
        let holds = kept.iter().any(|k| k.len() > dir.len() && k.starts_with(dir.as_str()) && k.as_bytes()[dir.len()] == b'/');
        if !holds {
            kcore::fs::remove(dir).map_err(|_| left(dir))?;
            removed += 1;
        }
    }
    kcore::fs::remove(&config).map_err(|_| alloc::format!("{} could not be taken away", config))?;
    removed += 1;
    if kept.is_empty() {
        kcore::fs::remove(&folder.path).map_err(|_| alloc::format!("{} could not be taken away", folder.path))?;
        removed += 1;
    }
    Ok(Deleted { devices, removed, kept })
}

/// `hv vms`: the folders under `root`, and of each what its vm.toml says
/// and whether it runs.
pub fn list(root: &str, vms: &Vms, out: &mut Output) {
    let entries = match kcore::fs::list(root) {
        Ok(entries) => entries,
        Err(_) => {
            let _ = writeln!(out, "hv: no {} -- hv create <name> <kernel> ... makes it, and a vm in it", root);
            return;
        }
    };
    let mut any = false;
    for entry in entries.iter().filter(|e| e.dir) {
        let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
        if !valid_name(name) {
            /* A VM's folder from before a name could not be a number: said,
             * since it cannot be started by that name */
            let has_config = join(root, name).and_then(|p| join(&p, CONFIG))
                .is_some_and(|file| kcore::fs::size(&file).is_ok());
            if reserved(name) && has_config {
                any = true;
                let _ = writeln!(out, "{}  {} -- give its folder another name", name,
                                 name_refused(name).unwrap_or_default());
            }
            continue;
        }
        let Some(path) = join(root, name) else { continue };
        /* A folder with no vm.toml is no VM's -- what a delete that kept the
         * disks left, say -- unless it is one being made. */
        if join(&path, CONFIG).is_none_or(|file| kcore::fs::size(&file).is_err()) {
            if let Some(doing) = vms.doing(name) {
                any = true;
                let _ = writeln!(out, "{}  being {}", name, doing);
            }
            continue;
        }
        any = true;
        let folder = Folder { name: String::from(name), path };
        let _ = write!(out, "{}  ", name);
        match (vms.status_of(name), vms.doing(name)) {
            (Some((id, true)), _) => {
                let _ = write!(out, "running as vm {}", id);
            }
            (Some((id, false)), _) => {
                let _ = write!(out, "stopped, vm {} on the list", id);
            }
            (None, Some(doing)) => {
                let _ = write!(out, "being {}", doing);
            }
            (None, None) => {
                let _ = write!(out, "not running");
            }
        }
        match load(&folder) {
            Ok(c) => summary(&c, out),
            Err(e) => {
                let _ = write!(out, "  -- {}", e);
            }
        }
        let _ = writeln!(out);
    }
    if !any {
        let _ = writeln!(out, "hv: no vms in {} -- hv create <name> <kernel> ... makes one", root);
    }
}

/// What a configuration boots, in a line's worth.
fn summary(c: &Config, out: &mut Output) {
    let o = &c.options;
    let kernel = o.kernel.rsplit('/').next().unwrap_or(&o.kernel);
    let cpus = o.cpus.unwrap_or(1);
    let _ = write!(out, "  {}  {} MiB  {} cpu{}", kernel, o.mem_mib.unwrap_or(guest::DEFAULT_MEM_MIB), cpus,
                   if cpus == 1 { "" } else { "s" });
    if !o.disks.is_empty() {
        let _ = write!(out, "  disks");
        for (i, (path, ro)) in o.disks.iter().enumerate() {
            let _ = write!(out, "{}{}{}", if i == 0 { " " } else { ", " }, path, if *ro { ":ro" } else { "" });
        }
    }
    if o.net {
        let _ = write!(out, "  net");
        if let Some(p) = o.port {
            if (p as usize) < net::MAX_PORTS {
                let _ = write!(out, " port {} ({})", p, net::dotted(net::port_ip(p as usize)));
            }
        }
        match &o.network {
            Some(network) => {
                let _ = write!(out, " on {}", network);
            }
            None => {
                let _ = write!(out, " alone");
            }
        }
    }
    for (host, guest) in &o.forwards {
        let _ = write!(out, "  forward {}:{}", host, guest);
    }
    if o.restart {
        let _ = write!(out, "  restart");
    }
    if c.protected {
        let _ = write!(out, "  protected");
    }
}
