//! The host's PCI functions as guests are given them (plans/gpu-passthrough.md):
//! `hv pci`, every function the scan found and how this module would give it
//! -- kept from the host's drivers or a driver's, its IOMMU group, its reset,
//! its interrupts, the VMs whose vm.toml names it, and why not where it
//! cannot be given (`passthrough::survey`) -- and `hv pci attach|detach`: a
//! function put on a stopped VM's vm.toml, given from its next start, or
//! taken off it. Nothing here touches a function: the survey reads.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;

use crate::config;
use crate::passthrough::{self, Surveyed};
use crate::store;
use crate::vms::Vms;

const USAGE: &str = "usage: hv pci | hv pci attach <vm> <bus:dev.fn> | hv pci detach <vm> <bus:dev.fn>";

/// A function's address as the shell and vm.toml write it.
pub fn address(bus: u8, slot: u8, func: u8) -> String {
    alloc::format!("{:02x}:{:02x}.{:x}", bus, slot, func)
}

/// What a function is to the host: a driver's, kept for guests, or neither.
pub fn state(s: &Surveyed) -> &'static str {
    if s.driven {
        "a driver's"
    } else if s.kept {
        "kept for guests"
    } else {
        "free"
    }
}

/// Its interrupts as a guest would be given them.
pub fn interrupts(s: &Surveyed) -> String {
    match (s.msix, s.msi) {
        (Some(n), _) => alloc::format!("MSI-X {}", n),
        (None, true) => String::from("MSI"),
        (None, false) => String::from("none"),
    }
}

/// Its IOMMU group's other functions, a bridge marked: empty for one alone.
pub fn group(s: &Surveyed) -> String {
    let mut out = String::new();
    for &((b, d, f), bridge) in &s.group {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&address(b, d, f));
        if bridge {
            out.push_str("(bridge)");
        }
    }
    out
}

/// The VMs whose vm.toml names each function: every vm.toml read once.
pub fn named(root: &str) -> Vec<(String, Vec<(u16, u16, u16)>)> {
    let mut out = Vec::new();
    let Ok(entries) = kcore::fs::list(root) else { return out };
    for entry in entries.iter().filter(|e| e.dir) {
        let Ok(name) = core::str::from_utf8(&entry.name) else { continue };
        if !store::valid_name(name) {
            continue;
        }
        let Ok(folder) = store::folder_of(root, name) else { continue };
        let Ok(c) = store::load(&folder) else { continue };
        if !c.options.pci.is_empty() && out.try_reserve(1).is_ok() {
            out.push((folder.name, c.options.pci));
        }
    }
    out
}

/// The VMs of `named` that name `s`, by name, a space between.
pub fn named_by(named: &[(String, Vec<(u16, u16, u16)>)], s: &Surveyed) -> String {
    let at = (u16::from(s.bus), u16::from(s.slot), u16::from(s.func));
    let mut out = String::new();
    for (vm, _) in named.iter().filter(|(_, pci)| pci.contains(&at)) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(vm);
    }
    out
}

/// `hv pci`: every function, and how it would be given; `attach` and
/// `detach`.
pub fn command(root: &str, vms: &Vms, rest: &str, out: &mut Output) {
    let mut words = rest.split_ascii_whitespace();
    match (words.next(), words.next(), words.next(), words.next()) {
        (None, ..) => list(root, out),
        (Some("attach"), Some(vm), Some(word), None) => {
            let _ = match attach(root, vms, vm, word) {
                Ok(at) => writeln!(out, "hv pci: {} on {}'s vm.toml -- given at its next start", at, vm),
                Err(e) => writeln!(out, "hv pci: {}", e),
            };
        }
        (Some("detach"), Some(vm), Some(word), None) => {
            let _ = match detach(root, vms, vm, word) {
                Ok(at) => writeln!(out, "hv pci: {} off {}'s vm.toml", at, vm),
                Err(e) => writeln!(out, "hv pci: {}", e),
            };
        }
        _ => {
            let _ = writeln!(out, "{}", USAGE);
        }
    }
}

fn list(root: &str, out: &mut Output) {
    let surveyed = match passthrough::survey() {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(out, "hv pci: {}", e);
            return;
        }
    };
    if surveyed.is_empty() {
        let _ = writeln!(out, "hv pci: no PCI functions");
        return;
    }
    let named = named(root);
    for s in &surveyed {
        let _ = write!(out, "{} {:04x}:{:04x} {:02x}.{:02x}  {}  reset {}  {}", address(s.bus, s.slot, s.func), s.vendor,
                       s.device, s.class, s.subclass, state(s), s.reset, interrupts(s));
        let g = group(s);
        if !g.is_empty() {
            let _ = write!(out, "  group {}", g);
        }
        if s.held {
            let _ = write!(out, "  -- a guest holds it");
        }
        let by = named_by(&named, s);
        if !by.is_empty() {
            let _ = write!(out, "  -- named by vm {}", by);
        }
        if let Some(why) = &s.refused {
            let _ = write!(out, "  -- not given: {}", why);
        }
        let _ = writeln!(out);
    }
}

/// The function `word` names, as the survey has it: why not, where it is
/// none or would not be given.
fn givable(word: &str) -> Result<(u16, u16, u16), String> {
    let at = config::pci(word).ok_or_else(|| alloc::format!("\"{}\" is no function -- bus:dev.fn, 01:00.0", word))?;
    let surveyed = passthrough::survey()?;
    let s = surveyed.iter()
        .find(|s| (u16::from(s.bus), u16::from(s.slot), u16::from(s.func)) == at)
        .ok_or_else(|| alloc::format!("{}: no such function", word))?;
    match &s.refused {
        Some(why) => Err(alloc::format!("{}: {}", word, why)),
        None => Ok(at),
    }
}

/// The function `word` names put on stopped VM `vm`'s vm.toml, given from
/// its next start: its address, or why not -- the function none, or one
/// that would not be given, the VM named it already or holding all it may.
pub fn attach(root: &str, vms: &Vms, vm: &str, word: &str) -> Result<String, String> {
    let folder = store::folder_of(root, vm)?;
    let at = givable(word)?;
    /* Held while its vm.toml changes -- refused while it is on the list:
     * hv stop it first */
    let _held = vms.reserve(vm, crate::vms::CHANGED)?;
    store::update(vms, &folder, |c| {
        if c.options.pci.contains(&at) {
            return Err(alloc::format!("{}'s vm.toml names {} already", vm, word));
        }
        if c.options.pci.len() >= config::MAX_PCI {
            return Err(alloc::format!("{} holds {} functions already, all a guest takes", vm, config::MAX_PCI));
        }
        c.options.pci.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        c.options.pci.push(at);
        Ok(())
    })?;
    Ok(address(at.0 as u8, at.1 as u8, at.2 as u8))
}

/// The function `word` names taken off stopped VM `vm`'s vm.toml: its
/// address, or why not.
pub fn detach(root: &str, vms: &Vms, vm: &str, word: &str) -> Result<String, String> {
    let folder = store::folder_of(root, vm)?;
    let at = config::pci(word).ok_or_else(|| alloc::format!("\"{}\" is no function -- bus:dev.fn, 01:00.0", word))?;
    let _held = vms.reserve(vm, crate::vms::CHANGED)?;
    store::update(vms, &folder, |c| match c.options.pci.iter().position(|&p| p == at) {
        Some(i) => {
            c.options.pci.remove(i);
            Ok(())
        }
        None => Err(alloc::format!("{}'s vm.toml names no function {}", vm, word)),
    })?;
    Ok(address(at.0 as u8, at.1 as u8, at.2 as u8))
}
