//! `hv create <name> <url> [words]`: a VM made of a cloud's image -- what
//! Ubuntu, Debian and Fedora ship, qcow2 or raw -- in a folder of its own,
//! for `hv start <name>` to boot:
//!
//! - the image downloaded into the folder by the kernel's own HTTP client
//!   (`kcore::http`: TLS, and the window a download of gigabytes wants),
//!   made the VM's raw disk (`image::convert`) -- or taken as it is, a raw
//!   one -- and grown to `size=`, for the guest to grow its root into
//!   (cloud-init's growpart does);
//! - cloud-init's NoCloud seed beside it, an ISO 9660 labelled cidata
//!   (`diskimage::iso`): its meta-data naming the machine, its user-data
//!   giving the image's own user the keys nos's sshd lets in
//!   (/etc/ssh/authorized_keys) -- and with `password=` a password, for its
//!   console -- or, given by the host API's caller, user-data of its own.
//!   The user-data says nothing of the machine -- the meta-data names it --
//!   and is kept in the folder as `user-data`, for a clone of the VM to give
//!   its own first boot as it is (`clone`);
//! - and its vm.toml, written last: hvboot as its kernel -- which boots the
//!   one its own disk's GRUB would -- the disk its first disk and the seed
//!   its second, read-only; on the switch, booted again when it reboots
//!   itself, and a GiB of memory unless told.
//!
//! The words after the URL are `hv start`'s but a kernel and an initrd --
//! hvboot is the kernel -- and its own three: `size=` (K, M, G or T),
//! `password=`, and `hvboot=`, where hvboot is: /lib/hvboot, the system
//! image's, unless told. The host API's create gives the same as a job's
//! parameters (`provider`). The name is held while the folder is made, so
//! no VM of it starts and nothing else is done to it meanwhile; what fails,
//! or is stopped, leaves nothing behind: what this made goes, the folder
//! too when this made it.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use diskimage::iso;
use diskimage::{detect, Format};
use kcore::consts::NS_PER_SEC;

use crate::config::{self, Config, Options};
use crate::guest;
use crate::image;
use crate::store::{self, Folder};
use crate::vms::Vms;
use crate::work::{self, Work};

/// Where the system image keeps hvboot.
pub const HVBOOT: &str = "/lib/hvboot";
/// The folder's files: the disk, the seed, and the user-data kept for a
/// clone's seed.
pub const DISK: &str = "disk.raw";
pub const SEED: &str = "seed.iso";
pub const USER_DATA: &str = "user-data";
/// What the image is downloaded as when its URL's last step is no name a
/// file of the folder's may have.
const IMAGE: &str = "image";
const MAX_IMAGE_NAME: usize = 64;
/// A cloud image's memory unless told: what Ubuntu's and Debian's boot to
/// their prompt and run cloud-init in.
const DEFAULT_MEM_MIB: u64 = 1024;
/// The keys nos's sshd lets in, given to the guest's user.
const AUTHORIZED_KEYS: &str = "/etc/ssh/authorized_keys";
const MAX_KEYS_BYTES: usize = 64 * 1024;
pub const MAX_PASSWORD: usize = 128;
/// The most user-data a VM is given -- what a seed of cloud-init's holds
/// many times over.
pub const MAX_USER_DATA: usize = 64 * 1024;
const SECS_PER_DAY: u64 = 86_400;
const SECS_PER_HOUR: u64 = 3600;
const SECS_PER_MINUTE: u64 = 60;
/// The seed's label, as cloud-init's NoCloud looks for it.
const SEED_LABEL: &str = "cidata";
const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// How far apart the download's progress lines are, as wget's: a tenth of
/// the time so far, from two seconds to thirty.
const REPORT_MIN_NS: u64 = 2 * NS_PER_SEC;
const REPORT_MAX_NS: u64 = 30 * NS_PER_SEC;
const REPORT_SHARE: u64 = 10;
const NS_PER_TENTH: u64 = NS_PER_SEC / 10;

/// Bytes over a time, as a rate: MiB/s to a tenth from one up, KiB/s below.
pub struct Rate(u64);

impl Rate {
    pub fn of(bytes: u64, ns: u64) -> Rate {
        if ns == 0 {
            return Rate(0);
        }
        Rate(u64::try_from(u128::from(bytes) * u128::from(NS_PER_SEC) / u128::from(ns)).unwrap_or(u64::MAX))
    }
}

impl core::fmt::Display for Rate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 >= MIB {
            write!(f, "{}.{} MiB/s", self.0 / MIB, self.0 % MIB * 10 / MIB)
        } else {
            write!(f, "{} KiB/s", self.0 / KIB)
        }
    }
}

/// Whether the word after `hv create <name>` is a URL: the other form.
pub fn is_url(word: &str) -> bool {
    word.starts_with("http://") || word.starts_with("https://")
}

/// What a VM of a cloud's image is made with, beside what `hv start` takes:
/// the image's URL, the size its disk is grown to, a password for its
/// console, where hvboot is -- and user-data of the caller's own in place of
/// nos's, which only the host API gives.
pub struct Recipe<'a> {
    pub url: &'a str,
    pub size: Option<u64>,
    pub password: Option<&'a str>,
    pub hvboot: &'a str,
    pub user_data: Option<&'a str>,
}

/// What a VM made of a cloud's image came to: its vm.toml, its disk's size,
/// and how many keys its user-data gave.
pub struct Created {
    pub file: String,
    pub disk_bytes: u64,
    pub keys: usize,
}

/// `hv create <name> <url> [words]`, its name checked already.
pub fn command(root: &str, vms: &Vms, name: &str, url: &str, words: &str, work: &mut dyn Work) {
    let made = own_words(words).and_then(|(own, rest)| {
        let o = guest::words(&alloc::format!("{} {}", own.hvboot, rest), store::CREATE_USAGE)?;
        let recipe = Recipe { url, size: own.size, password: own.password, hvboot: own.hvboot, user_data: None };
        create(root, vms, name, &recipe, o, work)
    });
    if let Err(e) = made {
        let _ = writeln!(work, "hv: {}", e);
    }
}

/// What this has made so far, to take away again if it fails: files, and
/// the folder when it made that too.
pub struct Made {
    pub files: Vec<String>,
    pub folder: Option<String>,
}

impl Made {
    pub fn new() -> Made {
        Made { files: Vec::new(), folder: None }
    }

    /// Everything kept from here on: the work is done.
    pub fn keep(&mut self) {
        self.files.clear();
        self.folder = None;
    }

    pub fn undo(&mut self) {
        for path in self.files.iter().rev() {
            let _ = kcore::fs::remove(path);
        }
        if let Some(folder) = &self.folder {
            let _ = kcore::fs::remove(folder);
        }
        self.keep();
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        self.undo();
    }
}

/// The words of this form's own, taken out of the rest -- up to `cmdline=`,
/// whose words are the guest's.
struct Own<'a> {
    size: Option<u64>,
    password: Option<&'a str>,
    hvboot: &'a str,
}

fn own_words<'a>(words: &'a str) -> Result<(Own<'a>, String), String> {
    let mut own = Own { size: None, password: None, hvboot: HVBOOT };
    let mut rest = String::new();
    let mut tail = words.trim_start();
    while !tail.is_empty() {
        if tail.starts_with("cmdline=") {
            rest.push_str(tail);
            break;
        }
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let word = &tail[..end];
        tail = tail[end..].trim_start();
        if let Some(v) = word.strip_prefix("size=") {
            own.size = Some(bytes(v).ok_or_else(|| String::from("size= wants a number of bytes, K, M, G or T after it"))?);
        } else if let Some(v) = word.strip_prefix("password=") {
            if v.is_empty() || v.len() > MAX_PASSWORD {
                return Err(alloc::format!("password= wants 1 to {} characters", MAX_PASSWORD));
            }
            own.password = Some(v);
        } else if let Some(v) = word.strip_prefix("hvboot=") {
            own.hvboot = v;
        } else {
            rest.push_str(word);
            rest.push(' ');
        }
    }
    Ok((own, rest))
}

/// `20G` and the like: binary multiples.
pub fn bytes(text: &str) -> Option<u64> {
    let (digits, shift) = match text.as_bytes().last()? {
        b'K' | b'k' => (&text[..text.len() - 1], 10),
        b'M' | b'm' => (&text[..text.len() - 1], 20),
        b'G' | b'g' => (&text[..text.len() - 1], 30),
        b'T' | b't' => (&text[..text.len() - 1], 40),
        _ => (text, 0),
    };
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(1u64 << shift)
}

/// What the download is called in the folder: its URL's last step, when
/// that is a name of letters, digits, `.`, `_` and `-` the folder's own
/// files do not have.
fn image_name(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ok = !last.is_empty() && last.len() <= MAX_IMAGE_NAME && !last.starts_with('.')
        && last.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && ![DISK, SEED, USER_DATA, store::CONFIG, store::CONFIG_PREV, "console.log", "console.log.1"].contains(&last);
    if ok { last } else { IMAGE }
}

/// `"text"` as YAML has a double-quoted string: what a key's comment or a
/// password holds read as written, never as YAML.
fn quoted(text: &str) -> String {
    let mut q = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => q.push_str("\\\""),
            '\\' => q.push_str("\\\\"),
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                let _ = write!(q, "\\x{:02x}", c as u32);
            }
            c => q.push(c),
        }
    }
    q.push('"');
    q
}

/// The keys nos's sshd lets in: the file's lines that are not comments.
pub fn keys() -> Vec<String> {
    let Ok(text) = kcore::fs::read(AUTHORIZED_KEYS, MAX_KEYS_BYTES) else { return Vec::new() };
    let Ok(text) = core::str::from_utf8(&text) else { return Vec::new() };
    text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from).collect()
}

/// When it is, as the seed's ISO 9660 records it.
fn now() -> iso::When {
    let wall = kcore::time::wall_clock_secs();
    let (days, secs) = (wall / SECS_PER_DAY, wall % SECS_PER_DAY);
    let (y, m, d) = hv::civil(days);
    iso::When {
        year: y as u16,
        month: m as u8,
        day: d as u8,
        hour: (secs / SECS_PER_HOUR) as u8,
        minute: (secs / SECS_PER_MINUTE % 60) as u8,
        second: (secs % 60) as u8,
    }
}

/// cloud-init's user-data as nos writes it: the image's own user given
/// `keys`, and the password when there is one -- nothing of the machine's
/// own, which the meta-data names, so that a clone's seed gives it as it is.
pub fn user_data(keys: &[String], password: Option<&str>) -> String {
    let mut user = String::from("#cloud-config\n");
    if !keys.is_empty() {
        user.push_str("ssh_authorized_keys:\n");
        for k in keys {
            let _ = writeln!(user, "  - {}", quoted(k));
        }
    }
    if let Some(p) = password {
        let _ = write!(user, "password: {}\nchpasswd:\n  expire: false\n", quoted(p));
    }
    user
}

/// cloud-init's seed for the VM `name`: its meta-data naming the machine,
/// with an instance-id of its own -- a new one for every VM made or cloned,
/// so that cloud-init runs for it as for a new machine: its own name, its
/// own SSH host keys -- and `user_data`.
pub fn seed(name: &str, user_data: &str) -> Result<Vec<u8>, String> {
    let mut meta = String::new();
    let _ = write!(meta, "instance-id: nos-{}-{}\nlocal-hostname: {}\n", name, kcore::time::wall_clock_secs(), name);
    let files = [iso::File { name: "meta-data", data: meta.as_bytes() },
                 iso::File { name: USER_DATA, data: user_data.as_bytes() }];
    iso::image(SEED_LABEL, &files, now()).map_err(|e| alloc::format!("the seed: {}", e))
}

/// A VM made of a cloud's image, `<root>/<name>`, as `recipe` says, with
/// `o` -- `hv start`'s words, or the host API's parameters -- for its
/// vm.toml: hvboot its kernel, the disk and the seed its first two disks.
/// Its lines, its progress and whether to stop are `work`'s.
pub fn create(root: &str, vms: &Vms, name: &str, recipe: &Recipe, mut o: Options, work: &mut dyn Work)
              -> Result<Created, String> {
    /* Checked as a start would check it, before anything is fetched: a VM
     * that cannot boot is no use made. */
    if o.initrd.is_some() {
        return Err(String::from("initrd= is no cloud image's: hvboot boots the one its disk's GRUB names"));
    }
    if o.secs.is_some() {
        return Err(String::from("secs= is hv boot's -- a vm runs until hv stop"));
    }
    if recipe.user_data.is_some_and(|u| u.len() > MAX_USER_DATA) {
        return Err(alloc::format!("its user-data is more than {} bytes", MAX_USER_DATA));
    }
    o.kernel = String::from(recipe.hvboot);
    o.mem_mib = o.mem_mib.or(Some(DEFAULT_MEM_MIB));
    o.net = true;
    o.restart = true;
    o.uuid = o.uuid.or_else(|| Some(crate::guest::random_uuid()));
    let theirs = core::mem::take(&mut o.disks);
    o.disks.push((String::from(DISK), false));
    o.disks.push((String::from(SEED), true));
    o.disks.extend(theirs);
    let path = store::join(root, name).ok_or_else(|| String::from("out of memory"))?;
    let folder = Folder { name: String::from(name), path };
    let config = Config::new(o);
    store::options_checked(&folder, &config)?;
    let file = store::join(&folder.path, store::CONFIG).ok_or_else(|| String::from("out of memory"))?;
    if kcore::fs::size(&file).is_ok() {
        return Err(alloc::format!("{} is there already -- not written over", file));
    }
    if kcore::fs::size(recipe.hvboot).is_err() {
        return Err(alloc::format!("no {} -- hvboot= says where hvboot is", recipe.hvboot));
    }
    /* Held from here on: no start of it, and no other create, clone or
     * delete of it, until this is done -- the vm.toml written last, by a
     * create that refuses one there. */
    let _held = vms.reserve(name, "made")?;
    let text = config::render(&config).ok_or_else(|| String::from("out of memory"))?;

    let mut made = Made::new();
    if kcore::fs::list(&folder.path).is_err() {
        store::make_dirs(&folder.path).map_err(|_| alloc::format!("{} could not be made", folder.path))?;
        made.folder = Some(folder.path.clone());
    }
    let created = fill(&folder, recipe, &text, &file, &mut made, work)?;
    made.keep();
    Ok(created)
}

/// The folder's files, each kept in `made` as it is: the image, the disk
/// made of it, the user-data and the seed, and last the vm.toml.
fn fill(folder: &Folder, recipe: &Recipe, text: &str, file: &str, made: &mut Made, work: &mut dyn Work)
        -> Result<Created, String> {
    let join = |f: &str| store::join(&folder.path, f).ok_or_else(|| String::from("out of memory"));
    let url = recipe.url;
    /* The URL's name where the path it makes is one the VFS takes. */
    let named = image_name(url);
    let named = if folder.path.len() + 1 + named.len() <= store::PATH_MAX { named } else { IMAGE };
    let download = join(named)?;
    let disk = join(DISK)?;
    let seed_path = join(SEED)?;
    let user_path = join(USER_DATA)?;

    let _ = writeln!(work, "hv: {} -- into {}", url, download);
    let started = kcore::time::boot_time_ns();
    let mut reported = started;
    let mut progress = |done: u64, total: u64| {
        if work.cancelled() {
            return false;
        }
        work.progress(done, total);
        let now = kcore::time::boot_time_ns();
        let every = (now.saturating_sub(started) / REPORT_SHARE).clamp(REPORT_MIN_NS, REPORT_MAX_NS);
        if now.saturating_sub(reported) >= every {
            reported = now;
            let _ = write!(work, "download: {} MiB", done / MIB);
            if total != 0 {
                let _ = write!(work, " of {} MiB, {}%", total / MIB, (u128::from(done) * 100 / u128::from(total)).min(100));
            }
            let _ = writeln!(work, ", {}", Rate::of(done, now.saturating_sub(started)));
        }
        true
    };
    let len = kcore::http::fetch(url, &download, &mut progress).map_err(|e| match e {
        kcore::http::Error::Cancelled => String::from(work::STOPPED),
        e => alloc::format!("{}: {}", url, e),
    })?;
    let ns = kcore::time::boot_time_ns().saturating_sub(started);
    let _ = writeln!(work, "download: {} bytes in {}.{} s, {}", len, ns / NS_PER_SEC, ns % NS_PER_SEC / NS_PER_TENTH,
                     Rate::of(len, ns));
    made.files.push(download.clone());

    /* What it is, by its first bytes: from the heap, the task's stack
     * being 64 KiB. */
    let mut head = Vec::new();
    head.try_reserve_exact(diskimage::detect::HEAD).map_err(|_| String::from("out of memory"))?;
    head.resize(diskimage::detect::HEAD, 0);
    let n = kcore::fs::read_at(&download, 0, &mut head).map_err(|e| alloc::format!("{}: {}", download, e))?;
    match detect(&head[..n]) {
        Format::Qcow2 => {
            image::convert(&download, &disk, work)?;
            made.files.push(disk.clone());
            /* The disk is what the VM has; the image is the download's. */
            if kcore::fs::remove(&download).is_ok() {
                made.files.retain(|f| *f != download);
            }
        }
        Format::Raw => {
            kcore::fs::rename(&download, &disk).map_err(|e| alloc::format!("{} could not become {} -- {}", download, disk, e))?;
            made.files.retain(|f| *f != download);
            made.files.push(disk.clone());
        }
        format => return Err(image::refused(&download, format)),
    }
    if work.cancelled() {
        return Err(String::from(work::STOPPED));
    }

    let size = grow(&disk, recipe.size)?;
    let keys = keys();
    let user = match recipe.user_data {
        Some(u) => String::from(u),
        None => user_data(&keys, recipe.password),
    };
    kcore::fs::create(&user_path, user.as_bytes())
        .map_err(|e| alloc::format!("{} could not be written -- {}", user_path, e))?;
    made.files.push(user_path);
    let seed = seed(&folder.name, &user)?;
    kcore::fs::create(&seed_path, &seed).map_err(|e| alloc::format!("{} could not be written -- {}", seed_path, e))?;
    made.files.push(seed_path);
    kcore::fs::create(file, text.as_bytes()).map_err(|e| alloc::format!("{} could not be written -- {}", file, e))?;
    made.files.push(String::from(file));

    if recipe.user_data.is_some() {
        let _ = writeln!(work, "hv: {} written -- hv start {} boots it: hvboot, a {} MiB disk, cloud-init given the \
                                user-data it was asked to", file, folder.name, size.div_ceil(MIB));
    } else {
        let _ = writeln!(work, "hv: {} written -- hv start {} boots it: hvboot, a {} MiB disk, cloud-init given {} \
                                key{}{}", file, folder.name, size.div_ceil(MIB), keys.len(),
                         if keys.len() == 1 { "" } else { "s" },
                         if recipe.password.is_some() { " and a password" } else { "" });
        if keys.is_empty() && recipe.password.is_none() {
            let _ = writeln!(work, "hv: no key in {} and no password= -- nobody can log in to it", AUTHORIZED_KEYS);
        }
    }
    Ok(Created { file: String::from(file), disk_bytes: size, keys: if recipe.user_data.is_some() { 0 } else { keys.len() } })
}

/// The disk grown to `size`, when given: never shrunk under what the image
/// holds. Its size, either way.
pub fn grow(disk: &str, size: Option<u64>) -> Result<u64, String> {
    let f = kcore::fs::File::open(disk, true).map_err(|e| alloc::format!("{}: {}", disk, e))?;
    let now = f.size().map_err(|e| alloc::format!("{}: {}", disk, e))?;
    let Some(want) = size else { return Ok(now) };
    if want < now {
        return Err(alloc::format!("size= {} MiB is less than the image's disk, {} MiB", want / MIB, now.div_ceil(MIB)));
    }
    if want > now {
        f.set_len(want).map_err(|e| alloc::format!("{}: {} bytes do not fit there -- {}", disk, want, e))?;
    }
    Ok(want)
}
