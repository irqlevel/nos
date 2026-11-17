//! A VM saved into a file and restored from it, on one host
//! (plans/vm-migration.md's M0): its state as `vmstate` writes it, and its
//! memory after it -- the VM paused for it and taken off the list, and
//! later running again from where it was.
//!
//!     hv save <vm> <file>     paused, its state and memory written, and gone
//!     hv restore <file>       the VM the file names, running again from it
//!
//! Only a VM of a folder: what is restored is the machine its vm.toml
//! describes -- the same memory, CPUs and devices, which the state is held
//! to -- with the state put into it, rather than a kernel booted. The file:
//!
//! ```text
//! "nosvmsv1" | version u32 | name: length u8, bytes | state: length u32, bytes
//! zeros to the next 4 KiB, where its pages begin
//! each range of RAM the state names, in its order, whole -- a page of zeros
//! not written, a hole
//! ```
//!
//! A restore takes the file away once the VM runs: run again from it, the
//! guest would go on from a point its disks have moved past.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use hv::run::{LinuxGuest, Stop};
use hv::Machine;

use crate::cores::Cores;
use crate::forward::Forwards;
use crate::vms::{Held, Vms};

const MAGIC: [u8; 8] = *b"nosvmsv1";
const VERSION: u32 = 1;
const PAGE: u64 = 4096;
/// What a copy between the guest's memory and the file goes in: a piece of
/// the heap's, read and written whole.
const PIECE: usize = 64 * 1024;
/// The most a state may be, in the file: a few KiB a CPU.
const STATE_MAX: usize = 4 << 20;

pub const SAVE_USAGE: &str = "hv save <vm> <file>";
pub const RESTORE_USAGE: &str = "hv restore <file>";

/// `hv save <vm> <file>`.
pub fn save_command(vms: &Vms, forwards: &Forwards, args: &str, out: &mut dyn Write) {
    let mut w = args.split_ascii_whitespace();
    let (Some(word), Some(path), None) = (w.next(), w.next(), w.next()) else {
        let _ = writeln!(out, "{}", SAVE_USAGE);
        return;
    };
    match save(vms, forwards, word, path) {
        Ok((name, bytes, ms)) => {
            let _ = writeln!(out, "hv: vm {} saved into {} -- {} MiB of its memory written, paused {} ms, and off the \
                                   list: hv restore {} runs it again", name, path, bytes >> 20, ms, path);
        }
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
        }
    }
}

/// The VM `word` names, paused, its state and memory written into a new
/// file at `path`, and ended: its name, the bytes of memory written, and how
/// long it was paused before it ended. Anything that fails before it ends
/// leaves it running, and no file.
fn save(vms: &Vms, forwards: &Forwards, word: &str, path: &str) -> Result<(String, u64, u64), String> {
    let taken = vms.take_for_state(word, Held::Save)?;
    let paused_at = kcore::time::boot_time_ns();
    let written = match write_file(&taken.guest, &taken.name, path) {
        Ok(written) => written,
        Err(e) => {
            taken.give_back();
            return Err(e);
        }
    };
    let ms = kcore::time::boot_time_ns().saturating_sub(paused_at) / kcore::consts::NS_PER_MS;
    let name = taken.name.clone();
    taken.end(Stop::Migrated, forwards)?;
    Ok((name, written, ms))
}

/// The guest paused, its state taken, and the file written: the bytes of
/// memory in it. The file is taken away again on a failure.
fn write_file(guest: &Arc<LinuxGuest>, name: &str, path: &str) -> Result<u64, String> {
    guest.pause().map_err(|e| alloc::format!("not paused -- {}", e))?;
    let state = match guest.capture() {
        Ok(s) => s,
        Err(e) => {
            guest.resume();
            return Err(alloc::format!("its state not taken -- {}", e));
        }
    };
    let state_bytes = match state.encode() {
        Some(b) if b.len() <= STATE_MAX => b,
        _ => {
            guest.resume();
            return Err(String::from("its state not written -- out of memory"));
        }
    };
    let head = match header(name, &state_bytes) {
        Some(h) => h,
        None => {
            guest.resume();
            return Err(String::from("its state not written -- out of memory"));
        }
    };
    if let Err(e) = kcore::fs::create(path, &head) {
        guest.resume();
        return Err(alloc::format!("{}: {}", path, e));
    }
    let pages_at = (head.len() as u64).next_multiple_of(PAGE);
    match write_pages(guest, &state, path, pages_at) {
        Ok(written) => Ok(written),
        Err(e) => {
            let _ = kcore::fs::remove(path);
            guest.resume();
            Err(e)
        }
    }
}

/// The file's head: its magic, its version, the VM's name, its state.
fn header(name: &str, state: &[u8]) -> Option<Vec<u8>> {
    let mut h = Vec::new();
    h.try_reserve_exact(8 + 4 + 1 + name.len() + 4 + state.len()).ok()?;
    h.extend_from_slice(&MAGIC);
    h.extend_from_slice(&VERSION.to_le_bytes());
    h.push(u8::try_from(name.len()).ok()?);
    h.extend_from_slice(name.as_bytes());
    h.extend_from_slice(&u32::try_from(state.len()).ok()?.to_le_bytes());
    h.extend_from_slice(state);
    Some(h)
}

/// The guest's RAM into the file at `path` from `at`, range after range as
/// the state names them, a piece at a time -- a piece of zeros left a hole:
/// the bytes written, and the file synced.
fn write_pages(guest: &LinuxGuest, state: &vmstate::VmState, path: &str, at: u64) -> Result<u64, String> {
    let total = state.vm.ram_bytes();
    let file = kcore::fs::File::open(path, true).map_err(|e| alloc::format!("{}: {}", path, e))?;
    file.set_len(at.saturating_add(total)).map_err(|e| alloc::format!("{}: {}", path, e))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(PIECE).map_err(|_| String::from("out of memory"))?;
    buf.resize(PIECE, 0);
    let mut offset = at;
    let mut written = 0u64;
    for r in &state.vm.ranges {
        let mut gpa = r.base;
        let end = r.base + r.size;
        while gpa < end {
            let n = (end - gpa).min(PIECE as u64) as usize;
            let piece = &mut buf[..n];
            guest.memory().read(gpa, piece).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
            if piece.iter().any(|&b| b != 0) {
                file.write_at(offset, piece).map_err(|e| alloc::format!("{}: {}", path, e))?;
                written += n as u64;
            }
            gpa += n as u64;
            offset += n as u64;
        }
    }
    file.sync().map_err(|e| alloc::format!("{}: {}", path, e))?;
    Ok(written)
}

/// What a file's head says: the VM's name, its state, and where its pages
/// begin.
struct Saved {
    name: String,
    state: vmstate::VmState,
    pages_at: u64,
}

/// The head of the file at `path`, read and checked.
fn read_header(path: &str) -> Result<Saved, String> {
    let file = kcore::fs::File::open(path, false).map_err(|e| alloc::format!("{}: {}", path, e))?;
    let mut fixed = [0u8; 8 + 4 + 1];
    if file.read_at(0, &mut fixed).map_err(|e| alloc::format!("{}: {}", path, e))? != fixed.len()
        || fixed[..8] != MAGIC
    {
        return Err(alloc::format!("{} is no VM saved by hv save", path));
    }
    if u32::from_le_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]) != VERSION {
        return Err(alloc::format!("{} is a VM saved in a version of the file this does not read", path));
    }
    let n = usize::from(fixed[12]);
    let mut name = [0u8; 256];
    let mut len = [0u8; 4];
    if file.read_at(13, &mut name[..n]).ok() != Some(n) || file.read_at(13 + n as u64, &mut len).ok() != Some(4) {
        return Err(alloc::format!("{} is cut short", path));
    }
    let name = core::str::from_utf8(&name[..n]).map_err(|_| alloc::format!("{} names no VM", path))?;
    let state_len = u32::from_le_bytes(len) as usize;
    if state_len > STATE_MAX {
        return Err(alloc::format!("{} holds a state larger than any VM's", path));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(state_len).map_err(|_| String::from("out of memory"))?;
    bytes.resize(state_len, 0);
    let at = 13 + n as u64 + 4;
    if file.read_at(at, &mut bytes).ok() != Some(state_len) {
        return Err(alloc::format!("{} is cut short", path));
    }
    let state = vmstate::VmState::decode(&bytes).map_err(|e| alloc::format!("{}: its state -- {}", path, e))?;
    let need = (at + state_len as u64).next_multiple_of(PAGE).saturating_add(state.vm.ram_bytes());
    if file.size().map_err(|e| alloc::format!("{}: {}", path, e))? < need {
        return Err(alloc::format!("{} is cut short of the memory its state names", path));
    }
    Ok(Saved { name: String::from(name), state, pages_at: (at + state_len as u64).next_multiple_of(PAGE) })
}

/// The guest's RAM from the file at `path`, from `at`, a piece at a time:
/// a piece of zeros left as the guest's fresh memory has it.
fn read_pages(guest: &LinuxGuest, state: &vmstate::VmState, path: &str, at: u64) -> Result<(), String> {
    let file = kcore::fs::File::open(path, false).map_err(|e| alloc::format!("{}: {}", path, e))?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(PIECE).map_err(|_| String::from("out of memory"))?;
    buf.resize(PIECE, 0);
    let mut offset = at;
    for r in &state.vm.ranges {
        let mut gpa = r.base;
        let end = r.base + r.size;
        while gpa < end {
            let n = (end - gpa).min(PIECE as u64) as usize;
            let piece = &mut buf[..n];
            if file.read_at(offset, piece).map_err(|e| alloc::format!("{}: {}", path, e))? != n {
                return Err(alloc::format!("{} is cut short", path));
            }
            if piece.iter().any(|&b| b != 0) {
                guest.memory().write(gpa, piece).map_err(|e| alloc::format!("its memory at {:#x}: {:?}", gpa, e))?;
            }
            gpa += n as u64;
            offset += n as u64;
        }
    }
    Ok(())
}

/// `hv restore <file>`.
pub fn restore_command(machine: &Arc<Machine>, cores: &Cores, forwards: &Forwards, vms: &Vms, root: &str, args: &str,
                       out: &mut dyn Write) {
    let mut w = args.split_ascii_whitespace();
    let (Some(path), None) = (w.next(), w.next()) else {
        let _ = writeln!(out, "{}", RESTORE_USAGE);
        return;
    };
    let saved = match read_header(path) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
            return;
        }
    };
    let folder = match crate::store::find(root, &saved.name) {
        Ok(Some(f)) => f,
        Ok(None) => {
            let _ = writeln!(out, "hv: {} is vm {}'s, and there is no folder of that name to restore it as", path,
                             saved.name);
            return;
        }
        Err(e) => {
            let _ = writeln!(out, "hv: {}", e);
            return;
        }
    };
    let (state, pages_at) = (&saved.state, saved.pages_at);
    let mut fill = |g: &LinuxGuest| read_pages(g, state, path, pages_at);
    let from = crate::vms::Origin::State { state, fill: &mut fill, what: path };
    match vms.start_restored(machine, cores, forwards, folder, from, out) {
        Ok(id) => {
            let _ = writeln!(out, "hv: vm {} ({}) restored from {}, and runs", id, saved.name, path);
            if let Err(e) = kcore::fs::remove(path) {
                let _ = writeln!(out, "hv: {} not taken away -- {}: run again from it, the guest would go on from a \
                                       point its disks have moved past", path, e);
            }
        }
        Err(e) => {
            let _ = writeln!(out, "hv: vm {} not restored -- {}", saved.name, e);
        }
    }
}
