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
//! guest's are, so no other guest writes them meanwhile.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;

use crate::config::{self, Config, Options};
use crate::guest;
use crate::net;
use crate::vmlog;
use crate::vms::Vms;

/// Where the VMs' folders are, unless `insmod` was told otherwise (`vms=`).
pub const DEFAULT_ROOT: &str = "/vms";
/// A folder's configuration.
pub const CONFIG: &str = "vm.toml";
/// The longest a VM's name may be.
const NAME_MAX: usize = 32;
/// The longest path the VFS takes (`fs::vfs::MAX_PATH`, less one).
const PATH_MAX: usize = 255;
/// The longest the VMs' folder's path may be: room under it for a VM's
/// folder and the longest name hv writes there.
const ROOT_MAX: usize = PATH_MAX - (1 + NAME_MAX + 1 + vmlog::LONGEST_NAME);

const CREATE_USAGE: &str = "hv create <name> <bzImage> [mem=MiB] [cpus=N] [cpu=N] [xapic] [ioapic] [apicv|noapicv] \
                            [rxmod=US] [nooffload] [initrd=path] [disk=path[:ro]]... [input=...] [log] [restart] \
                            [net] [port=N] [forward=nos:guest]... [cmdline=...]";

/// A VM's folder: the name it is known by, and its path.
pub struct Folder {
    pub name: String,
    pub path: String,
}

/// Whether `name` may be a VM's: a letter or a digit, then letters, digits,
/// `.`, `_` and `-`, at most `NAME_MAX` in all -- a folder's name, and a
/// word of a command, with nothing in it a path or a shell reads otherwise.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= NAME_MAX && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
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

/// Makes the folder, and the folders above it that are missing.
fn make_dirs(path: &str) -> kcore::error::Result<()> {
    for (i, _) in path.match_indices('/').skip(1) {
        kcore::fs::create_dir(&path[..i])?;
    }
    kcore::fs::create_dir(path)
}

/// `<dir>/<file>`, or None out of memory.
fn join(dir: &str, file: &str) -> Option<String> {
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
    } else {
        return Ok(None);
    };
    let file = join(&path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    if kcore::fs::size(&file).is_err() {
        if word.starts_with('/') {
            return Ok(None);
        }
        return Err(alloc::format!("no vm {}: there is no {} -- hv create {} <bzImage> ... makes one", word, file, word));
    }
    let name = path.rsplit('/').next().unwrap_or("");
    if !valid_name(name) {
        return Err(alloc::format!("{} has a {}, but \"{}\" is not a name a vm can have -- a letter or a digit, then \
                                   letters, digits, . _ and -, at most {}", path, CONFIG, name, NAME_MAX));
    }
    Ok(Some(Folder { name: String::from(name), path }))
}

/// The folder's configuration, read and parsed; what is wrong with it
/// otherwise, said with its path.
pub fn load(folder: &Folder) -> Result<Config, String> {
    let file = join(&folder.path, CONFIG).ok_or_else(|| String::from("out of memory"))?;
    let bytes = match kcore::fs::read(&file, config::MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(kcore::error::Error::InvalidValue) => {
            return Err(alloc::format!("{} is more than the {} bytes a vm.toml may be", file, config::MAX_BYTES));
        }
        Err(e) => return Err(alloc::format!("{}: {}", file, e)),
    };
    let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not UTF-8 text", file))?;
    config::parse(text).map_err(|e| alloc::format!("{}: {}", file, e))
}

/// A path of the configuration's, as nos reaches it: the folder's unless it
/// begins with `/`.
fn resolve(folder: &Folder, path: &str) -> Result<String, String> {
    if path.starts_with('/') {
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

/// `hv create <name> <words>`: the folder made under `root`, and its
/// vm.toml written from the words -- `hv start`'s, and its paths the
/// folder's unless they begin with `/` -- never over one that is there.
pub fn create(root: &str, args: &str, out: &mut Output) {
    let args = args.trim_start();
    let end = args.find(char::is_whitespace).unwrap_or(args.len());
    let (name, rest) = (&args[..end], args[end..].trim_start());
    if name.is_empty() || rest.is_empty() {
        let _ = writeln!(out, "{}", CREATE_USAGE);
        return;
    }
    if !valid_name(name) {
        let _ = writeln!(out, "hv: \"{}\" is not a name a vm can have -- a letter or a digit, then letters, digits, \
                               . _ and -, at most {}", name, NAME_MAX);
        return;
    }
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
        missing.extend(o.disks.iter().map(|(p, _)| p.as_str()));
        for path in missing {
            if kcore::fs::size(path).is_err() {
                let _ = writeln!(out, "hv: {} is not there yet", path);
            }
        }
    }
}

/// The folder's configuration as a start checks it: the guest it says, its
/// paths made nos's, given its defaults and held to every rule.
pub fn options_checked(folder: &Folder, c: &Config) -> Result<guest::Spec, String> {
    guest::spec(options(folder, c)?)
}

/// `hv vms`: the folders under `root`, and of each what its vm.toml says
/// and whether it runs.
pub fn list(root: &str, vms: &Vms, out: &mut Output) {
    let entries = match kcore::fs::list(root) {
        Ok(entries) => entries,
        Err(_) => {
            let _ = writeln!(out, "hv: no {} -- hv create <name> <bzImage> ... makes it, and a vm in it", root);
            return;
        }
    };
    let mut any = false;
    for entry in entries.iter().filter(|e| e.dir) {
        let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
        if !valid_name(name) {
            continue;
        }
        let Some(path) = join(root, name) else { continue };
        any = true;
        let folder = Folder { name: String::from(name), path };
        let _ = write!(out, "{}  ", name);
        match vms.status_of(name) {
            Some((id, true)) => {
                let _ = write!(out, "running as vm {}", id);
            }
            Some((id, false)) => {
                let _ = write!(out, "stopped, vm {} on the list", id);
            }
            None => {
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
        let _ = writeln!(out, "hv: no vms in {} -- hv create <name> <bzImage> ... makes one", root);
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
    }
    for (host, guest) in &o.forwards {
        let _ = write!(out, "  forward {}:{}", host, guest);
    }
    if o.restart {
        let _ = write!(out, "  restart");
    }
}
