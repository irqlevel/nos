//! Where a guest's CPUs go, and the host task that serves its disks: over
//! the host's cores as [`Topology`] has them, and the CPUs the guests
//! already running take.
//!
//! Two CPUs on one core's two threads share its execution units, its caches
//! and its buffers -- the ground every SMT side channel stands on (MDS, L1TF,
//! port contention): what one thread runs, the other can learn of. So by
//! default a guest's cores are its own ([`Cores::Own`]): it is given whole
//! cores no other guest has a CPU on, its vCPU or the task that serves its
//! disks, and its vCPUs fill their threads -- a vCPU a thread, as a cloud
//! sells them, where one a core would leave a thread idle beside each. A
//! guest that would need a core another has is refused, saying so; the
//! operator who wants guests on one core anyway -- a test machine of two --
//! says so ([`Cores::Shared`]), and gets the placement there was before.
//!
//! Pure code over masks and counts, nothing of the kernel's: the module
//! decides and takes its holds under one lock with what this says, and the
//! hv fuzzer holds what this says to a model of its own.

use alloc::vec::Vec;

use kcore::consts::MAX_CPUS;

/// The widths an APIC ID divides into: a core's threads, and the CPUs that
/// share the last-level cache. On x86 the kernel numbers a CPU by its APIC
/// ID, so a CPU's number shifted right by the one is its core, by the other
/// its cache domain; `crate::topology` reads the widths from CPUID.
#[derive(Clone, Copy, Debug)]
pub struct Topology {
    smt_shift: u32,
    llc_shift: u32,
}

impl Topology {
    pub const fn of(smt_shift: u32, llc_shift: u32) -> Topology {
        Topology { smt_shift, llc_shift }
    }

    /// The core CPU `cpu` is a thread of.
    #[inline]
    pub fn core(&self, cpu: u32) -> u32 {
        cpu.checked_shr(self.smt_shift).unwrap_or(0)
    }

    /// The last-level cache CPU `cpu` shares.
    #[inline]
    pub fn llc(&self, cpu: u32) -> u32 {
        cpu.checked_shr(self.llc_shift).unwrap_or(0)
    }
}

/// Whether a guest's cores are its own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cores {
    /// Whole cores no other guest has a CPU on, its vCPUs filling their
    /// threads; refused where there are not enough.
    Own,
    /// A guest's CPUs on the cores fewest vCPUs are on, whoever's -- its own
    /// on cores apart, for speed -- as before there was a choice.
    Shared,
}

/// What the guests running take: the CPUs they are on, their vCPUs' and the
/// tasks' that serve their disks, and how many vCPUs are on each.
#[derive(Clone, Copy)]
pub struct Taken {
    pub cpus: u64,
    pub load: [u32; MAX_CPUS],
}

impl Taken {
    pub const fn none() -> Taken {
        Taken { cpus: 0, load: [0; MAX_CPUS] }
    }
}

/// Where a guest goes: its vCPUs' CPUs, its first CPU's first, and the CPU
/// the task that serves its disks runs on -- with no disks, its first
/// vCPU's, which nothing runs on for them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Placement {
    pub vcpus: Vec<u32>,
    pub disk: u32,
}

/// Why a guest was given no place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// The extension is on for no CPU.
    NoneOn,
    /// It is not on for the CPU asked for.
    NotOn(u32),
    /// More CPUs were asked for than it is on for: each of a guest's CPUs
    /// runs on a host CPU of its own.
    TooMany { asked: u32, on: u32 },
    /// The core of the CPU asked for has another guest's CPU on it.
    CoreTaken(u32),
    /// The cores no other guest has a CPU on hold fewer CPUs the extension
    /// is on for than were asked for.
    NoCores { asked: u32, room: u32 },
    /// A guest of no CPUs.
    NoCpus,
    /// Out of memory for the answer.
    NoMemory,
}

/// The CPU `cpu`'s bit in a mask -- 0 past the mask.
#[inline]
fn bit(cpu: u32) -> u64 {
    1u64.checked_shl(cpu).unwrap_or(0)
}

/// Where a guest of `n` CPUs goes, with disks or not, its first CPU on
/// `wanted` if it names one: on CPUs the extension is on for (`enabled`),
/// its disks' task on one that is online (`online`), and as `cores` says,
/// beside what the guests running `taken`.
pub fn place(topo: &Topology, cores: Cores, enabled: u64, online: u64, taken: &Taken, wanted: Option<u32>, n: u32,
             disks: bool) -> Result<Placement, Refused>
{
    if enabled == 0 {
        return Err(Refused::NoneOn);
    }
    if n == 0 {
        return Err(Refused::NoCpus);
    }
    if let Some(cpu) = wanted {
        if enabled & bit(cpu) == 0 {
            return Err(Refused::NotOn(cpu));
        }
    }
    if n > enabled.count_ones() {
        return Err(Refused::TooMany { asked: n, on: enabled.count_ones() });
    }
    match cores {
        Cores::Own => own(topo, enabled, online, taken, wanted, n, disks),
        Cores::Shared => shared(topo, enabled, online, taken, wanted, n),
    }
}

/* ---- own: whole cores to a guest, its vCPUs filling their threads ---- */

/// The cores, by number, any of the CPUs in `cpus` is a thread of.
fn cores_of(topo: &Topology, cpus: u64) -> u64 {
    let mut cores = 0;
    for cpu in 0..MAX_CPUS as u32 {
        if cpus & bit(cpu) != 0 {
            cores |= bit(topo.core(cpu));
        }
    }
    cores
}

/// The CPUs of `mask` that are threads of core `core`.
fn threads(topo: &Topology, mask: u64, core: u32) -> u64 {
    let mut out = 0;
    for cpu in 0..MAX_CPUS as u32 {
        if mask & bit(cpu) != 0 && topo.core(cpu) == core {
            out |= bit(cpu);
        }
    }
    out
}

fn own(topo: &Topology, enabled: u64, online: u64, taken: &Taken, wanted: Option<u32>, n: u32, disks: bool)
    -> Result<Placement, Refused>
{
    let held = cores_of(topo, taken.cpus);
    if let Some(cpu) = wanted {
        if held & bit(topo.core(cpu)) != 0 {
            return Err(Refused::CoreTaken(cpu));
        }
    }
    /* The CPUs a vCPU may be on: the extension on, the core no other
     * guest's. */
    let mut free = 0u64;
    for cpu in 0..MAX_CPUS as u32 {
        if enabled & bit(cpu) != 0 && held & bit(topo.core(cpu)) == 0 {
            free |= bit(cpu);
        }
    }
    if n > free.count_ones() {
        return Err(Refused::NoCores { asked: n, room: free.count_ones() });
    }

    let mut vcpus = Vec::new();
    vcpus.try_reserve_exact(n as usize).map_err(|_| Refused::NoMemory)?;
    let mut chosen = 0u64;
    let mut home: Option<u32> = None;
    /* The first core: the one asked for, or the best; then the best of the
     * rest until there are threads enough -- each filled, the CPU asked for
     * first, before the next is taken. */
    let mut next = wanted.map(|cpu| topo.core(cpu));
    while (vcpus.len() as u32) < n {
        let core = match next.take() {
            Some(core) => core,
            None => best_core(topo, free, chosen, home).ok_or(Refused::NoCores { asked: n, room: free.count_ones() })?,
        };
        chosen |= bit(core);
        let mut on_core = threads(topo, free, core);
        if let Some(cpu) = wanted.filter(|&cpu| topo.core(cpu) == core && vcpus.is_empty()) {
            vcpus.push(cpu);
            on_core &= !bit(cpu);
        }
        /* The higher threads first: CPU 0, where the boot CPU's own work
         * runs, the last a guest is given. */
        for cpu in (0..MAX_CPUS as u32).rev() {
            if (vcpus.len() as u32) < n && on_core & bit(cpu) != 0 {
                vcpus.push(cpu);
            }
        }
        if home.is_none() {
            home = vcpus.first().map(|&cpu| topo.llc(cpu));
        }
    }

    let disk = if disks { own_disk(topo, online, held, chosen, &vcpus, home) } else { vcpus[0] };
    Ok(Placement { vcpus, disk })
}

/// The free core a guest takes next, of those with a CPU in `free` it has
/// not `chosen`: one sharing the last-level cache `home` with its first,
/// where it has one; then not CPU 0's, where the boot CPU's own work runs;
/// then the one with the most threads to fill; then the higher.
fn best_core(topo: &Topology, free: u64, chosen: u64, home: Option<u32>) -> Option<u32> {
    let mut best: Option<(u32, (bool, bool, u32, u32))> = None;
    for cpu in 0..MAX_CPUS as u32 {
        if free & bit(cpu) == 0 {
            continue;
        }
        let core = topo.core(cpu);
        if chosen & bit(core) != 0 {
            continue;
        }
        let key = (home.map_or(true, |h| topo.llc(cpu) == h), core != topo.core(0),
                   threads(topo, free, core).count_ones(), core);
        if best.map_or(true, |(_, k)| key > k) {
            best = Some((core, key));
        }
    }
    best.map(|(core, _)| core)
}

/// Where a guest whose vCPUs have the cores `chosen` serves its disks: on a
/// thread of those cores no vCPU of its is on, where there is one online;
/// then on a core no guest has, sharing its cache where one does -- which
/// the guest then holds as well; then on its own last vCPU's CPU, sharing
/// time with it rather than a core with another guest. CPU 0 last at each.
fn own_disk(topo: &Topology, online: u64, held: u64, chosen: u64, vcpus: &[u32], home: Option<u32>) -> u32 {
    let mine = vcpus.iter().fold(0u64, |m, &cpu| m | bit(cpu));
    let mut best: Option<(u32, (bool, bool, bool, u32))> = None;
    for cpu in 0..MAX_CPUS as u32 {
        if online & bit(cpu) == 0 || mine & bit(cpu) != 0 {
            continue;
        }
        let core = topo.core(cpu);
        let own_core = chosen & bit(core) != 0;
        if !own_core && held & bit(core) != 0 {
            continue;
        }
        let key = (own_core, home.map_or(true, |h| topo.llc(cpu) == h), cpu != 0, cpu);
        if best.map_or(true, |(_, k)| key > k) {
            best = Some((cpu, key));
        }
    }
    match best {
        Some((cpu, _)) => cpu,
        None => vcpus.last().copied().unwrap_or(0),
    }
}

/* ---- shared: as before there was a choice ---- */

/// How many vCPUs run on the core CPU `cpu` is a thread of: on it and on its
/// SMT siblings, with whom a vCPU there shares the core's execution units.
fn core_load(topo: &Topology, cpu: u32, load: &[u32; MAX_CPUS]) -> u32 {
    let core = topo.core(cpu);
    (0..MAX_CPUS as u32).filter(|&c| topo.core(c) == core).map(|c| load[c as usize]).sum()
}

/// The first CPU: `wanted`, or the CPU the extension is on for whose core
/// has fewest vCPUs already, and of those the CPU itself -- the higher of
/// any two that tie: CPU 0 is where the boot CPU's own work runs, and the
/// last to be given a guest. Each other on a CPU none of the guest's has
/// yet: one whose core none of the guest's CPUs is on where there is one,
/// since two threads of a core share its execution units; then one sharing
/// the first's last-level cache, where the CPUs' lines and IPIs go no
/// further; then the least loaded, as the first. And its disks' task on an
/// online CPU other than its first vCPU's -- that one's only on a machine of
/// one CPU -- with the fewest vCPUs, the higher of two that tie, CPU 0 last.
fn shared(topo: &Topology, enabled: u64, online: u64, taken: &Taken, wanted: Option<u32>, n: u32)
    -> Result<Placement, Refused>
{
    let mut load = taken.load;
    let first = match wanted {
        Some(cpu) => cpu,
        None => {
            let mut best: Option<(u32, (u32, u32))> = None;
            for cpu in 0..MAX_CPUS as u32 {
                if enabled & bit(cpu) == 0 {
                    continue;
                }
                let key = (core_load(topo, cpu, &load), load[cpu as usize]);
                if best.map_or(true, |(_, k)| key <= k) {
                    best = Some((cpu, key));
                }
            }
            best.map(|(cpu, _)| cpu).ok_or(Refused::NoneOn)?
        }
    };
    let mut vcpus = Vec::new();
    vcpus.try_reserve_exact(n as usize).map_err(|_| Refused::NoMemory)?;
    vcpus.push(first);
    load[first as usize] += 1;
    let home = topo.llc(first);
    while (vcpus.len() as u32) < n {
        let mut best: Option<(u32, (bool, bool, u32, u32))> = None;
        for cpu in 0..MAX_CPUS as u32 {
            if enabled & bit(cpu) == 0 || vcpus.contains(&cpu) {
                continue;
            }
            let shares_core = vcpus.iter().any(|&c| topo.core(c) == topo.core(cpu));
            let key = (shares_core, topo.llc(cpu) != home, core_load(topo, cpu, &load), load[cpu as usize]);
            if best.map_or(true, |(_, k)| key <= k) {
                best = Some((cpu, key));
            }
        }
        let (cpu, _) = best.ok_or(Refused::TooMany { asked: n, on: enabled.count_ones() })?;
        load[cpu as usize] += 1;
        vcpus.push(cpu);
    }

    let mut disk: Option<u32> = None;
    for cpu in 1..MAX_CPUS as u32 {
        if cpu == first || online & bit(cpu) == 0 {
            continue;
        }
        if disk.map_or(true, |d| taken.load[cpu as usize] <= taken.load[d as usize]) {
            disk = Some(cpu);
        }
    }
    let disk = match disk {
        Some(cpu) => cpu,
        None if first != 0 => 0,
        None => first,
    };
    Ok(Placement { vcpus, disk })
}
