//! Where guests are put on the host's cores (`hv/src/place.rs`), held to
//! what it promises, by a reckoning of the fuzzer's own: under `Cores::Own`
//! no core with CPUs of two guests on it -- vCPUs or the task that serves a
//! guest's disks -- a guest's vCPUs filling the threads of its cores before
//! another is taken, its disks' task on a spare thread of its own cores
//! where there is one, then on a core no guest has, then on a vCPU's CPU of
//! its own; under either rule each vCPU on a CPU of its own the extension is
//! on for, the first where it was asked to be; and a guest refused only
//! where it has no place, for the reason that comes first. The host is
//! made up -- its threads a core, how many cores share a cache, which CPUs
//! are online and which the extension is on for -- and guests are started
//! and stopped on it, and the rule changed between, as an input says.

use crate::place::{self, Cores, Placement, Refused, Taken, Topology};
use crate::targets::Input;

/// The CPUs a mask has room for.
const CPUS: u32 = 64;

/// A guest the model has started: its vCPUs' CPUs and its disks' task's.
struct Guest {
    vcpus: u64,
    disk: u64,
}

fn bit(cpu: u32) -> u64 {
    1u64.checked_shl(cpu).unwrap_or(0)
}

/// The cores any CPU of `cpus` is a thread of.
fn cores_of(topo: &Topology, cpus: u64) -> u64 {
    (0..CPUS).filter(|&c| cpus & bit(c) != 0).fold(0, |m, c| m | bit(topo.core(c)))
}

/// The CPUs of `mask` on any of `cores`.
fn on_cores(topo: &Topology, mask: u64, cores: u64) -> u64 {
    (0..CPUS).filter(|&c| mask & bit(c) != 0 && cores & bit(topo.core(c)) != 0).fold(0, |m, c| m | bit(c))
}

pub fn place_target(r: &mut Input) {
    /* One, two or four threads a core; a cache shared by one to sixteen. */
    let smt = r.below(3) as u32;
    let topo = Topology::of(smt, smt + r.below(5) as u32);
    /* Up to 64 CPUs, CPU 0 always among them and some missing -- a hybrid
     * part's cores of one thread leave holes in the numbering. */
    let count = 1 + r.below(u64::from(CPUS)) as u32;
    let mut online = 0u64;
    for cpu in 0..count {
        if cpu == 0 || r.u8() >= 32 {
            online |= bit(cpu);
        }
    }
    let mut enabled = 0u64;
    for cpu in 0..CPUS {
        if online & bit(cpu) != 0 && r.u8() >= 24 {
            enabled |= bit(cpu);
        }
    }
    let mut guests: Vec<Guest> = Vec::new();
    let mut own = r.u8() >= 32;
    while let Some(op) = r.op(5) {
        match op {
            0..=2 => {
                let n = 1 + if r.u8() < 200 { r.below(4) } else { r.below(16) } as u32;
                /* Now and then past the 64 a mask has. */
                let wanted = (r.u8() < 48).then(|| r.below(u64::from(CPUS) + 2) as u32);
                let disks = r.bool();
                let mut taken = Taken::none();
                for g in &guests {
                    taken.cpus |= g.vcpus | g.disk;
                    for cpu in 0..CPUS {
                        if g.vcpus & bit(cpu) != 0 {
                            taken.load[cpu as usize] += 1;
                        }
                    }
                }
                let rule = if own { Cores::Own } else { Cores::Shared };
                let got = place::place(&topo, rule, enabled, online, &taken, wanted, n, disks);
                check(&topo, own, enabled, online, taken.cpus, wanted, n, disks, &got);
                if let Ok(p) = got {
                    let vcpus = p.vcpus.iter().fold(0u64, |m, &c| m | bit(c));
                    guests.push(Guest { vcpus, disk: if disks { bit(p.disk) } else { 0 } });
                    placed(if own { "place: placed, own cores" } else { "place: placed, shared" });
                }
            }
            3 => {
                if !guests.is_empty() {
                    let i = r.below(guests.len() as u64) as usize;
                    guests.swap_remove(i);
                }
            }
            _ => own = !own,
        }
    }
}

/// Hold one answer to the model: `taken` the CPUs of the guests already
/// running, as the answer was given them.
#[allow(clippy::too_many_arguments)]
fn check(topo: &Topology, own: bool, enabled: u64, online: u64, taken: u64, wanted: Option<u32>, n: u32, disks: bool,
         got: &Result<Placement, Refused>)
{
    /* The reason that comes first, or none. */
    let held = cores_of(topo, taken);
    let free = on_cores(topo, enabled, !held);
    let due = if enabled == 0 {
        Some(Refused::NoneOn)
    } else if let Some(w) = wanted.filter(|&w| enabled & bit(w) == 0) {
        Some(Refused::NotOn(w))
    } else if n > enabled.count_ones() {
        Some(Refused::TooMany { asked: n, on: enabled.count_ones() })
    } else if let Some(w) = wanted.filter(|&w| own && held & bit(topo.core(w)) != 0) {
        Some(Refused::CoreTaken(w))
    } else if own && n > free.count_ones() {
        Some(Refused::NoCores { asked: n, room: free.count_ones() })
    } else {
        None
    };
    let p = match (got, due) {
        (Err(e), Some(d)) => {
            invariant!(*e == d, "refused for {:?} where the reason that comes first is {:?}", e, d);
            placed("place: refused, as due");
            return;
        }
        (Err(e), None) => panic!("invariant: refused for {:?} where a place exists: {} cpus, own {}, enabled \
                                  {:#x}, taken {:#x}, wanted {:?}", e, n, own, enabled, taken, wanted),
        (Ok(p), Some(d)) => panic!("invariant: placed at {:?} where it is due a refusal, {:?}", p, d),
        (Ok(p), None) => p,
    };

    let mine = p.vcpus.iter().fold(0u64, |m, &c| m | bit(c));
    invariant!(p.vcpus.len() as u32 == n && mine.count_ones() == n,
               "{} cpus asked for and given {:?}: not that many, each its own", n, p.vcpus);
    invariant!(mine & !enabled == 0, "vCPUs {:?} on CPUs the extension is not on for ({:#x})", p.vcpus, enabled);
    if let Some(w) = wanted {
        invariant!(p.vcpus.first() == Some(&w), "cpu {} asked for and the first given {:?}", w, p.vcpus);
    }
    if disks {
        invariant!(online & bit(p.disk) != 0, "the disks' task on cpu {}, which is not online", p.disk);
    }
    if !own {
        return;
    }

    /* Its cores are its own. */
    let cores = cores_of(topo, mine);
    invariant!(cores & held == 0, "vCPUs {:?} on cores another guest has a CPU on ({:#x})", p.vcpus, held);
    /* Filled: every core of its but one with no thread the extension is on
     * for left over. */
    let partial = (0..CPUS).filter(|&c| cores & bit(c) != 0)
        .filter(|&core| on_cores(topo, free & !mine, bit(core)) != 0).count();
    invariant!(partial <= 1, "vCPUs {:?} leave {} of their cores with threads unused: not filled before the next \
                              was taken", p.vcpus, partial);
    if partial == 1 {
        placed("place: own cores, the last not filled");
    }
    if !disks {
        return;
    }
    let disk_core = bit(topo.core(p.disk));
    invariant!(disk_core & held == 0, "the disks' task on cpu {}, whose core another guest has a CPU on", p.disk);
    let spare = on_cores(topo, online & !mine, cores);
    let elsewhere = on_cores(topo, online & !mine, !cores & !held);
    if spare != 0 {
        invariant!(spare & bit(p.disk) != 0, "the disks' task on cpu {} with a thread of its own cores spare ({:#x})",
                   p.disk, spare);
        placed("place: disks on a spare thread of its own");
    } else if elsewhere != 0 {
        invariant!(elsewhere & bit(p.disk) != 0, "the disks' task on cpu {}, with no thread of its own cores spare \
                                                  and a core no guest has ({:#x})", p.disk, elsewhere);
        placed("place: disks on a core no guest had");
    } else {
        invariant!(mine & bit(p.disk) != 0, "the disks' task on cpu {}, with nowhere but its own vCPUs' CPUs",
                   p.disk);
        placed("place: disks beside a vCPU of its own");
    }
}

/// What the answers were, for a run's report.
static PLACED: std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn placed(what: &'static str) {
    *PLACED.lock().unwrap_or_else(|e| e.into_inner()).entry(what).or_insert(0) += 1;
}

pub fn report() {
    for (what, n) in PLACED.lock().unwrap().iter() {
        println!("  {}: {}", what, n);
    }
}
