//! The host's cores and the guests that hold them: where the next guest
//! goes (`hv::place`) and its hold on what it is given, decided and taken
//! under one lock -- two guests started at once from two shells cannot both
//! be given a core -- and given back when the guest goes: an `hv start`
//! guest's at its `hv stop`, its cores kept while it is parked for the
//! restart that runs it there again, an `hv boot` guest's when its run is
//! over. Whether a guest's cores are its own (`hv cores`) is said here too.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use hv::place::{self, Cores as Policy, Placement, Refused, Taken};
use hv::Machine;
use kcore::consts::MAX_CPUS;
use kcore::sync::Mutex;

use crate::disk::Holder;

/// One guest's hold: the CPUs its vCPUs run on, the one its disks' task
/// does when it has disks, and whose it is -- None for a guest being
/// started, which has no number yet.
struct Holding {
    id: u64,
    vcpus: u64,
    disk: u64,
    holder: Option<Holder>,
}

struct Holds {
    next: u64,
    list: Vec<Holding>,
}

struct Inner {
    /// A guest's cores are its own (`Policy::Own`), or may be shared.
    own: AtomicBool,
    holds: Mutex<Holds>,
}

/// The cores the guests hold, and the rule new ones are placed by.
pub struct Cores {
    inner: Arc<Inner>,
}

/// A guest's hold on the CPUs it was given, until this is dropped.
pub struct Hold {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut holds = self.inner.holds.lock();
        if let Some(i) = holds.list.iter().position(|h| h.id == self.id) {
            holds.list.swap_remove(i);
        }
    }
}

impl Hold {
    /// Say whose it is, once the guest has a name: a VM's number is given
    /// after its place, so that a guest refused one takes no number.
    pub fn held_by(&self, holder: Holder) {
        let mut holds = self.inner.holds.lock();
        if let Some(h) = holds.list.iter_mut().find(|h| h.id == self.id) {
            h.holder = Some(holder);
        }
    }
}

/// The CPU `cpu`'s bit, 0 past a mask's 64.
fn bit(cpu: u32) -> u64 {
    1u64.checked_shl(cpu).unwrap_or(0)
}

impl Cores {
    /// Whole cores to a guest, to begin with. Made as the module loads, as
    /// the rest of its state is.
    pub fn new() -> Option<Cores> {
        let holds = Mutex::new(Holds { next: 0, list: Vec::new() })?;
        Some(Cores { inner: Arc::new(Inner { own: AtomicBool::new(true), holds }) })
    }

    pub fn own(&self) -> bool {
        self.inner.own.load(Ordering::Acquire)
    }

    /// Give guests placed from now on cores of their own, or let them
    /// share; the guests running keep where they are.
    pub fn set_own(&self, own: bool) {
        self.inner.own.store(own, Ordering::Release);
    }

    /// A place for a guest of `n` CPUs -- the first on `wanted` if it names
    /// one, its disks' task too where it has `disks` -- and its hold on it,
    /// `holder`'s, or named later ([`Hold::held_by`]); or why not.
    pub fn take(&self, machine: &Machine, wanted: Option<u32>, n: u32, disks: bool, holder: Option<Holder>)
        -> Result<(Placement, Hold), String>
    {
        let policy = if self.own() { Policy::Own } else { Policy::Shared };
        let topo = hv::topology::host();
        let enabled = machine.enabled_mask();
        let online = kcore::cpu::online_mask();
        let mut holds = self.inner.holds.lock();
        holds.list.try_reserve(1).map_err(|_| String::from("out of memory"))?;
        let mut taken = Taken::none();
        for h in &holds.list {
            taken.cpus |= h.vcpus | h.disk;
            for (cpu, load) in taken.load.iter_mut().enumerate() {
                if h.vcpus & bit(cpu as u32) != 0 {
                    *load += 1;
                }
            }
        }
        let p = place::place(&topo, policy, enabled, online, &taken, wanted, n, disks).map_err(describe)?;
        let id = holds.next;
        holds.next += 1;
        let vcpus = p.vcpus.iter().fold(0u64, |m, &cpu| m | bit(cpu));
        holds.list.push(Holding { id, vcpus, disk: if disks { bit(p.disk) } else { 0 }, holder });
        drop(holds);
        Ok((p, Hold { inner: self.inner.clone(), id }))
    }

    /// The rule, and every hold: `hv cores`.
    pub fn report(&self, out: &mut dyn Write) {
        let _ = writeln!(out, "hv: {}", if self.own() {
            "cores own -- a guest is given whole cores no other guest has a CPU on"
        } else {
            "cores shared -- guests may be given threads of one core"
        });
        let holds = self.inner.holds.lock();
        for h in &holds.list {
            let _ = match h.holder {
                Some(holder) => write!(out, "  {}: cpu", holder),
                None => write!(out, "  a guest being started: cpu"),
            };
            let mut first = true;
            for cpu in 0..MAX_CPUS as u32 {
                if h.vcpus & bit(cpu) != 0 {
                    let _ = write!(out, "{}{}", if first { " " } else { "," }, cpu);
                    first = false;
                }
            }
            for cpu in 0..MAX_CPUS as u32 {
                if h.disk & bit(cpu) != 0 {
                    let _ = write!(out, ", disks on cpu {}", cpu);
                }
            }
            let _ = writeln!(out);
        }
    }
}

/// Why a guest got no place, as the shell says it.
fn describe(r: Refused) -> String {
    let mut s = String::new();
    let _ = match r {
        Refused::NoneOn => write!(s, "the extension is on for no CPU -- hv on first"),
        Refused::NotOn(cpu) => write!(s, "the extension is not on for cpu {}", cpu),
        Refused::TooMany { asked, on } => write!(s,
            "{} cpus asked for, and the extension is on for {} -- each of a guest's CPUs runs on a host CPU of its own",
            asked, on),
        Refused::CoreTaken(cpu) => write!(s,
            "cpu {}'s core has another guest's CPU on it -- a guest's cores are its own (hv cores shared lets guests \
             share them)", cpu),
        Refused::NoCores { asked, room } => write!(s,
            "{} cpus asked for, and the cores no other guest has a CPU on hold {} -- a guest's cores are its own (hv \
             cores shared lets guests share them)", asked, room),
        Refused::NoCpus => write!(s, "a guest of no cpus"),
        Refused::NoMemory => write!(s, "out of memory"),
    };
    s
}
