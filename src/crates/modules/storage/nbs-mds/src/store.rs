//! What is stored, as this host's shell and the `nbs` provider ask for it
//! (plans/nbs.md, N6): the hosts' chunk servers and their devices, the
//! volumes and where their chunks are. Every change is one of the metadata's
//! commands, asked of the member; every read is this host's member's view,
//! at the entry it has applied.
//!
//! A device is recorded here, and taken in by its host's cs (nbs-cs.ko),
//! which formats it where it is blank. `nbs devices add` finds the device on
//! this host -- by its id, or by its name -- takes its id and its size in
//! slots, and refuses one that something else holds -- a mounted filesystem,
//! the disk log, a guest of hv's -- or that has something on it that is not
//! this NBS cluster's: a filesystem, a partition table, another cluster's
//! cs. `wipe` takes such a device all the same, its first pages zeroed once
//! the metadata has it, and what was on it gone.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::block::{Disk, DiskClaim, Found};
use kcore::cmd::Output;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use nbscs::Holds;
use nbsmeta::{Answer, Command, DeviceState, Refusal, State, Volume, CHUNK_MIB};

use crate::member::View;
use crate::{caught_up, ModState};

/// What a change came to where it was not done: the metadata's refusal, or
/// what kept it from being asked.
pub enum Failure {
    Refused(Refusal),
    Other(String),
}

impl Failure {
    pub fn says(&self) -> String {
        match self {
            Failure::Refused(why) => String::from(why.says()),
            Failure::Other(why) => why.clone(),
        }
    }
}

impl From<String> for Failure {
    fn from(why: String) -> Failure {
        Failure::Other(why)
    }
}

/// `cmd`, as the metadata answers it: a refusal as a failure of its own.
pub fn ask(state: &ModState, cmd: &Command) -> Result<Answer, Failure> {
    match raftmember::ask(&state.shared, cmd, crate::NO_MEMBER) {
        Ok(Answer::Refused(why)) => Err(Failure::Refused(why)),
        Ok(a) => Ok(a),
        Err(why) => Err(Failure::Other(why)),
    }
}

/// The host this member is.
pub fn this_host(v: &View) -> Result<String, String> {
    v.state.member_of_id(v.node).map(|m| m.host.clone())
        .ok_or_else(|| String::from("this host's member is no member of the NBS cluster any more"))
}

/// The device `word` names on this host: an id (`part:...`, `disk:...`) or
/// a name (`vdb`, `nvme1n1p2`).
fn find(word: &str) -> Result<Disk, String> {
    if nbsmeta::valid_ident(word) {
        return Disk::find_ident(word).map_err(|f| match f {
            Found::Many(n) => alloc::format!("{} devices say they are {}: copies of one disk -- none is taken", n, word),
            _ => alloc::format!("no device of this host's is {} -- `disks` lists what there is", word),
        });
    }
    Disk::open(word).ok_or_else(|| alloc::format!("no device of this host's is called {} -- `disks` lists them", word))
}

/// A cs's page -- its superblock's two are what is read of a device before
/// it is taken, and zeroed by `wipe` -- moved a kernel page at a time.
const PAGE: usize = nbscs::PAGE as usize;
const _: () = assert!(PAGE == PAGE_SIZE, "a cs's page is the kernel's");

/// The sectors of `disk` a page is: the device read and written a page at a
/// time, through a page of the kernel's that the driver DMAs into (Disk's
/// own rule).
fn page_sectors(disk: &Disk) -> Result<u64, String> {
    match disk.sector_size() {
        ss if ss != 0 && PAGE as u64 % ss == 0 => Ok(PAGE as u64 / ss),
        ss => Err(alloc::format!("{} has sectors of {} bytes", disk.name(), ss)),
    }
}

/// What device `disk` holds where a cs's superblock goes.
fn contents(disk: &Disk, page: &mut DmaBuffer) -> Result<Holds, String> {
    let per = page_sectors(disk)?;
    let mut first: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    for (k, copy) in first.iter_mut().enumerate() {
        copy.try_reserve_exact(PAGE).map_err(|_| String::from("no memory to read it"))?;
        disk.read(k as u64 * per, &mut page.as_mut_slice()[..PAGE])
            .map_err(|e| alloc::format!("{}: its page {} not read -- {}", disk.name(), k, e))?;
        copy.extend_from_slice(&page.as_slice()[..PAGE]);
    }
    Ok(nbscs::holds(&first[0], &first[1]))
}

/// The superblock's two pages zeroed, past the device's write cache --
/// under the claim the caller holds, which the writes are made by.
fn wipe_first(claim: &DiskClaim, page: &mut DmaBuffer) -> Result<(), String> {
    let disk = claim.disk();
    let per = page_sectors(&disk)?;
    page.as_mut_slice()[..PAGE].fill(0);
    for k in 0..2u64 {
        claim.write(k * per, &page.as_slice()[..PAGE], true)
            .map_err(|e| alloc::format!("{}: its page {} not zeroed -- {}", disk.name(), k, e))?;
    }
    disk.flush().map_err(|e| alloc::format!("{} not flushed -- {}", disk.name(), e))
}

/// Device `word` of this host's added to its cs -- with `wipe`, whatever is
/// on it: its id, its slots and its number in the metadata, and whether it
/// was wiped.
pub fn device_add(state: &ModState, word: &str, wipe: bool) -> Result<(String, u32, u32, bool), Failure> {
    let v = caught_up(state)?;
    let host = this_host(&v)?;
    let cluster = v.state.cluster().map(|c| c.id).ok_or_else(|| String::from("no NBS cluster yet"))?;
    let disk = find(word)?;
    let Some(ident) = disk.ident() else {
        return Err(Failure::Other(alloc::format!(
            "{} has no id that finds it again at the next boot -- no partition GUID, MBR signature or serial \
             number -- and a name is not kept", disk.name())));
    };
    let ident = String::from(ident.as_str());
    let bytes = disk.sectors().saturating_mul(disk.sector_size());
    let slots = nbscs::device_slots(bytes);
    if slots == 0 {
        return Err(Failure::Other(alloc::format!("{} is too small for a chunk server: {} MiB", ident, bytes >> 20)));
    }
    /* This host's already, as it is: the same device -- its cs's, which
       holds it -- answered as it is, nothing read or written */
    if let Some((_, d)) = v.state.device(&ident).filter(|(c, d)| c.host == host && d.slots == slots) {
        return Ok((ident, slots, d.id, false));
    }
    /* Nothing else holds it while it is looked at, recorded and wiped; then
       the cs takes it, and keeps it */
    let claim = disk.claim_as("nbs").map_err(|holder| alloc::format!("{} is held by {}", ident, holder))?;
    let mut page = DmaBuffer::new(1).ok_or_else(|| String::from("no memory for a page to read it through"))?;
    let other = match contents(&disk, &mut page)? {
        Holds::Nothing => None,
        Holds::Nbs(s) if s.identity.cluster == cluster => None,
        Holds::Nbs(s) => Some(alloc::format!("is NBS's, of another NBS cluster ({}...)", codec::hex(&s.identity.cluster[..4]))),
        Holds::Other => Some(String::from("has something on it that is not NBS's -- a filesystem, a partition table?")),
    };
    if let (Some(what), false) = (&other, wipe) {
        return Err(Failure::Other(alloc::format!("{} {} -- `nbs devices add {} wipe` takes it all the same, and what \
                                                  is on it is lost", ident, what, word)));
    }
    let id = match ask(state, &Command::DeviceAdd { host, ident: ident.clone(), slots })? {
        Answer::Device { id } => id,
        other => return Err(Failure::Other(alloc::format!("answered {:?}", other))),
    };
    /* Wiped once the metadata has it, and not before: a refusal leaves what
       is on it as it was */
    let wiped = other.is_some();
    if wiped {
        if let Err(why) = wipe_first(&claim, &mut page) {
            return Err(Failure::Other(alloc::format!("{} is device {} in the metadata, and was not wiped: {} -- its \
                                                      cs leaves it alone", ident, id, why)));
        }
    }
    drop(claim);
    Ok((ident, slots, id, wiped))
}

/// Device `ident` of this host's given no new chunks.
pub fn device_drain(state: &ModState, ident: &str) -> Result<u32, Failure> {
    let v = caught_up(state)?;
    let host = this_host(&v)?;
    match ask(state, &Command::DeviceDrain { host, ident: String::from(ident) })? {
        Answer::Device { id } => Ok(id),
        other => Err(Failure::Other(alloc::format!("answered {:?}", other))),
    }
}

fn volume_id(a: Answer) -> Result<u32, Failure> {
    match a {
        Answer::Volume { id } => Ok(id),
        other => Err(Failure::Other(alloc::format!("answered {:?}", other))),
    }
}

/// Volume `name` made, `size_mib` MiB, its own host `host` -- this one where
/// none is given: its number.
pub fn volume_create(state: &ModState, name: &str, size_mib: u64, host: Option<&str>) -> Result<u32, Failure> {
    let v = caught_up(state)?;
    let host = match host {
        Some(h) => String::from(h),
        None => this_host(&v)?,
    };
    volume_id(ask(state, &Command::VolumeCreate { name: String::from(name), size_mib, host })?)
}

/// Volume `name` deleted: its number, 0 where it was gone already.
pub fn volume_delete(state: &ModState, name: &str) -> Result<u32, Failure> {
    caught_up(state)?;
    volume_id(ask(state, &Command::VolumeDelete { name: String::from(name) })?)
}

pub fn volume_resize(state: &ModState, name: &str, size_mib: u64) -> Result<u32, Failure> {
    caught_up(state)?;
    volume_id(ask(state, &Command::VolumeResize { name: String::from(name), size_mib })?)
}

/// A size as it is typed: MiB, or with M, G or T after it.
pub fn parse_size(text: &str) -> Option<u64> {
    let (digits, shift) = match text.as_bytes().last()? {
        b'M' | b'm' => (&text[..text.len() - 1], 0),
        b'G' | b'g' => (&text[..text.len() - 1], 10),
        b'T' | b't' => (&text[..text.len() - 1], 20),
        _ => (text, 0),
    };
    let n: u64 = digits.parse().ok()?;
    n.checked_mul(1u64 << shift)
}

/// MiB, as a person reads them.
pub struct Mib(pub u64);

impl core::fmt::Display for Mib {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.0;
        if m >= 1 << 20 && m % (1 << 10) == 0 {
            write!(f, "{}.{} TiB", m >> 20, (m % (1 << 20)) * 10 >> 20)
        } else if m >= 1 << 10 {
            write!(f, "{}.{} GiB", m >> 10, (m % (1 << 10)) * 10 >> 10)
        } else {
            write!(f, "{} MiB", m)
        }
    }
}

fn slots_mib(slots: u64) -> Mib {
    Mib(slots * u64::from(CHUNK_MIB))
}

fn device_state(s: DeviceState) -> &'static str {
    match s {
        DeviceState::Active => "active",
        DeviceState::Draining => "draining",
        DeviceState::Failed => "failed",
    }
}

/// The hosts volume `v`'s replicas are on: each host and how many of its
/// chunks it has, the most first -- what a scheduler starts its VM by.
pub fn replica_hosts(s: &State, v: &Volume) -> Vec<(String, u64)> {
    let mut by: alloc::collections::BTreeMap<u32, u64> = alloc::collections::BTreeMap::new();
    for &c in v.map.iter() {
        let Some(set) = s.chunks().get(c as usize).and_then(|ch| s.sets().get(ch.set as usize)) else { continue };
        for cs in set.cs {
            *by.entry(cs).or_default() += 1;
        }
    }
    let mut out: Vec<(String, u64)> = by.into_iter()
        .filter_map(|(cs, n)| s.cs().get(cs as usize).map(|c| (c.host.clone(), n))).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/* ---- what the shell says ---- */

pub fn show_cs(v: &View, out: &mut Output) {
    if v.state.cs().is_empty() {
        let _ = writeln!(out, "nbs: no chunk server has a device yet -- `nbs devices add <id>` on a member");
        return;
    }
    for c in v.state.cs() {
        let member = if v.state.member(&c.host).is_some() { "" } else { " (no member: takes no new chunks)" };
        let _ = writeln!(out, "  {:<16} {} of {} reserved, {} free, {} devices{}", c.host, slots_mib(c.reserved),
                         slots_mib(c.slots()), slots_mib(c.free()), c.devices.len(), member);
        for d in &c.devices {
            let _ = writeln!(out, "    device {:<4} {:<40} {:>12} {}", d.id, d.ident, alloc::format!("{}",
                             slots_mib(u64::from(d.slots))), device_state(d.state));
        }
    }
}

fn attach_text(v: &Volume) -> String {
    match &v.writer {
        Some(w) => alloc::format!("attached on {} (generation {})", w.host, w.generation),
        None if !v.readers.is_empty() => alloc::format!("read-only on {}", v.readers.join(", ")),
        None => String::from("detached"),
    }
}

pub fn show_volumes(v: &View, out: &mut Output) {
    if v.state.volumes().is_empty() {
        let _ = writeln!(out, "nbs: no volume yet -- `nbs volume create <name> <size>`");
        return;
    }
    let mut vols: Vec<&Volume> = v.state.volumes().iter().collect();
    vols.sort_by(|a, b| a.name.cmp(&b.name));
    for vol in vols {
        let _ = writeln!(out, "  {:<24} volume {:<5} {:>10}  {} chunks, of {}, {}", vol.name, vol.id,
                         alloc::format!("{}", Mib(vol.size_mib)), vol.map.len(), vol.host, attach_text(vol));
    }
}

pub fn show_volume(v: &View, name: &str, out: &mut Output) -> Result<(), String> {
    let vol = v.state.volume(name).ok_or_else(|| alloc::format!("no volume {}", name))?;
    let _ = writeln!(out, "nbs: volume {} ({}), {}, {} chunks of {} MiB, its own host {}, {}", vol.name, vol.id,
                     Mib(vol.size_mib), vol.map.len(), CHUNK_MIB, vol.host, attach_text(vol));
    let hosts = replica_hosts(&v.state, vol);
    let line: Vec<String> = hosts.iter().map(|(h, n)| alloc::format!("{} {}", h, n)).collect();
    let _ = writeln!(out, "  replicas, chunks a host: {}", line.join(", "));
    Ok(())
}
