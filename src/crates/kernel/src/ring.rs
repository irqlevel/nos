//! The boot's check of the lockless ring (`kcore::ring`): what a full and an
//! empty ring answer, lap after lap; then producers and consumers on several
//! CPUs at once -- nothing lost, nothing twice, every producer's values in
//! the order it pushed them, and what a producer wrote before a push seen by
//! whoever pops it. The last is what the frame pool and netblk stand on, and
//! what a memory ordering too weak would break on arm64 alone. A failure
//! panics, as the boot's other self-tests do.
//!
//! One producer and one consumer on a ring of two cells, spinning rather than
//! yielding while they wait, is where a wrong ordering shows. A ring whose
//! sequences were stored and loaded relaxed failed every one of eight boots
//! under HVF on an Apple M4 Pro, six hundred values and more given twice
//! each time; with a yield after every miss and a fifth of the values, two
//! boots of eight; and two producers and two consumers on sixteen cells
//! never caught it. TCG, on an x86 host, orders more than arm64 promises, and
//! may not catch it at all.
//!
//! It times a push and a pop on one CPU as well, for the log: the ring is on
//! the receive path, and a number printed every boot is noticed when it
//! moves.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use kcore::consts::MAX_CPUS;
use kcore::ring::LocklessRing;
use kcore::{task, time, trace};

/// The most producers a run has
const MAX_PRODUCERS: usize = 2;
/// What each producer pushes, one to one and two to two -- where there are
/// CPUs for them to run apart. On one CPU a run only checks the ring's
/// logic, since nothing can be reordered there, and a few are enough.
const ONE_TO_ONE: usize = 100_000;
const TWO_TO_TWO: usize = 10_000;
const ONE_CPU: usize = 2_000;
/// How long a task spins on a full or an empty ring before it lets the
/// other tasks on its CPU run: the other side is on another CPU and a moment
/// from done. A yield at every miss put a seventh as many values through in
/// the time, and gave a wrong ordering a seventh of the chances to show.
const SPINS: u32 = 1000;
/// The most CPUs a run's tasks are spread over: one each where there are
/// enough
const MAX_TASKS: usize = 4;
/// Ample for TCG on a loaded host. A run that takes longer has lost a value,
/// and says so rather than hanging the boot.
const DEADLINE_NS: u64 = 30_000_000_000;

/// What a producer writes before it pushes `index`, and its consumer
/// expects to find once it has popped it: never 0, which is what was there.
fn mark(index: usize) -> u64 {
    (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1
}

/// A word for every value of a run, found by the value's index: in pieces,
/// since the heap gives out at most 128 pages at a time.
struct Words {
    pieces: Vec<Vec<AtomicU64>>,
}

/// Words in a piece: 64 KiB
const PIECE: usize = 8192;

impl Words {
    fn zeroed(len: usize) -> Self {
        let count = len.div_ceil(PIECE);
        let mut pieces = Vec::new();
        pieces.try_reserve_exact(count).unwrap_or_else(|_| panic!("ring selftest: no memory for {} pieces", count));
        for i in 0..count {
            let n = PIECE.min(len - i * PIECE);
            let mut piece = Vec::new();
            piece.try_reserve_exact(n).unwrap_or_else(|_| panic!("ring selftest: no memory for {} words", n));
            piece.extend((0..n).map(|_| AtomicU64::new(0)));
            pieces.push(piece);
        }
        Self { pieces }
    }

    fn at(&self, index: usize) -> &AtomicU64 {
        &self.pieces[index / PIECE][index % PIECE]
    }

    fn ones(&self) -> u32 {
        self.pieces.iter().flatten().map(|w| w.load(Ordering::Relaxed).count_ones()).sum()
    }
}

struct Shared {
    ring: LocklessRing,
    per_producer: usize,
    total: usize,
    /// Written before the push, read after the pop
    payload: Words,
    /// A bit for every value popped
    seen: Words,
    consumed: AtomicUsize,
    deadline: u64,
    /* What went wrong, counted */
    twice: AtomicUsize,
    stale: AtomicUsize,
    disorder: AtomicUsize,
    stray: AtomicUsize,
    timed_out: AtomicUsize,
}

/// A task waiting on the other side: false once past the deadline.
fn wait(shared: &Shared, spins: &mut u32) -> bool {
    *spins += 1;
    if *spins < SPINS {
        core::hint::spin_loop();
        return true;
    }
    *spins = 0;
    if time::boot_time_ns() > shared.deadline {
        shared.timed_out.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    task::yield_to_runnable();
    true
}

fn produce((shared, producer): (Arc<Shared>, usize)) {
    let mut spins = 0;
    for i in 0..shared.per_producer {
        let index = producer * shared.per_producer + i;
        shared.payload.at(index).store(mark(index), Ordering::Relaxed);
        while !shared.ring.push(index) {
            if !wait(&shared, &mut spins) {
                return;
            }
        }
        spins = 0;
    }
}

fn consume(shared: Arc<Shared>) {
    let mut last = [None::<usize>; MAX_PRODUCERS];
    let mut spins = 0;
    while shared.consumed.load(Ordering::Relaxed) < shared.total {
        let Some(index) = shared.ring.pop() else {
            if !wait(&shared, &mut spins) {
                return;
            }
            continue;
        };
        spins = 0;
        shared.consumed.fetch_add(1, Ordering::Relaxed);

        if index >= shared.total {
            shared.stray.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if shared.payload.at(index).load(Ordering::Relaxed) != mark(index) {
            shared.stale.fetch_add(1, Ordering::Relaxed);
        }
        /* One producer's values reach any one consumer in the order they
         * were pushed: they hold ascending positions, and a consumer's
         * claims ascend. */
        let producer = index / shared.per_producer;
        if last[producer].is_some_and(|before| index <= before) {
            shared.disorder.fetch_add(1, Ordering::Relaxed);
        }
        last[producer] = Some(index);

        let bit = 1u64 << (index % 64);
        if shared.seen.at(index / 64).fetch_or(bit, Ordering::Relaxed) & bit != 0 {
            shared.twice.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What a ring answers on one CPU, full and empty, lap after lap.
fn check_one_cpu() {
    for refused in [0, 3, kcore::ring::MAX_CAPACITY * 2] {
        assert!(LocklessRing::new(refused).is_none(), "ring selftest: a ring of {} cells made", refused);
    }

    let Some(ring) = LocklessRing::new(8) else {
        panic!("ring selftest: no ring of 8");
    };
    /* Any word goes through, these two included */
    assert!(ring.push(usize::MAX) && ring.push(0), "ring selftest: an empty ring refused");
    assert!(ring.pop() == Some(usize::MAX) && ring.pop() == Some(0), "ring selftest: a word changed");

    for lap in 0..1000usize {
        let fill = lap % 9;
        for i in 0..fill {
            assert!(ring.push(lap * 16 + i), "ring selftest: refused below its capacity");
        }
        if fill == 8 {
            assert!(!ring.push(usize::MAX), "ring selftest: a full ring took one more");
        }
        assert!(ring.len() == fill, "ring selftest: len {} with {} in it", ring.len(), fill);
        for i in 0..fill {
            assert!(ring.pop() == Some(lap * 16 + i), "ring selftest: not first in, first out");
        }
        assert!(ring.pop().is_none() && ring.len() == 0, "ring selftest: an empty ring gave something");
    }
}

/// The cost of a push and a pop with nothing contending, in tenths of a
/// nanosecond: the best of a few runs, so that a tick taken in one does not
/// count.
fn time_one_cpu() -> u64 {
    const BATCH: usize = 64;
    const ROUNDS: usize = 1024;
    const RUNS: usize = 3;
    const PAIRS: u64 = (BATCH * ROUNDS) as u64;

    let Some(ring) = LocklessRing::new(BATCH) else {
        panic!("ring selftest: no ring of {}", BATCH);
    };
    let mut best = u64::MAX;
    for _ in 0..RUNS {
        let start = time::boot_time_ns();
        for round in 0..ROUNDS {
            for i in 0..BATCH {
                assert!(ring.push(round * BATCH + i), "ring selftest: refused below its capacity");
            }
            for i in 0..BATCH {
                assert!(ring.pop() == Some(round * BATCH + i), "ring selftest: not first in, first out");
            }
        }
        let elapsed = time::boot_time_ns().saturating_sub(start);
        best = best.min(elapsed * 10 / PAIRS);
    }
    best
}

/// `producers` pushing `per_producer` values each and `consumers` taking
/// them, through a ring of `capacity`, each task on a CPU of `cpus` in turn
/// -- its own where there are as many as tasks, doubled up and taking turns
/// where there are not. What it took, in microseconds; a value lost, given
/// twice, out of order or ahead of what was written with it panics.
fn run(capacity: usize, producers: usize, consumers: usize, per_producer: usize, cpus: &[u32]) -> u64 {
    let total = producers * per_producer;
    let Some(ring) = LocklessRing::new(capacity) else {
        panic!("ring selftest: no ring of {}", capacity);
    };
    let shared = Arc::new(Shared {
        ring,
        per_producer,
        total,
        payload: Words::zeroed(total),
        seen: Words::zeroed(total.div_ceil(64)),
        consumed: AtomicUsize::new(0),
        deadline: time::boot_time_ns() + DEADLINE_NS,
        twice: AtomicUsize::new(0),
        stale: AtomicUsize::new(0),
        disorder: AtomicUsize::new(0),
        stray: AtomicUsize::new(0),
        timed_out: AtomicUsize::new(0),
    });

    let start = time::boot_time_ns();
    let mut tasks = Vec::new();
    for k in 0..producers + consumers {
        let cpu = cpus[k % cpus.len()];
        let task = if k < producers {
            task::spawn_on_with("ringtest", 1u64 << cpu, (shared.clone(), k), produce)
        } else {
            task::spawn_on_with("ringtest", 1u64 << cpu, shared.clone(), consume)
        };
        let Some(task) = task else {
            panic!("ring selftest: cannot start a task on cpu {}", cpu);
        };
        tasks.push(task);
    }
    /* Dropping a handle waits for its task */
    drop(tasks);
    let micros = time::boot_time_ns().saturating_sub(start) / 1000;

    let count = |c: &AtomicUsize| c.load(Ordering::Relaxed);
    let shape = (producers, consumers, capacity);
    assert!(count(&shared.timed_out) == 0,
        "ring selftest {:?}: {} tasks still waiting after {} s -- {} of {} values came out",
        shape, count(&shared.timed_out), DEADLINE_NS / 1_000_000_000, count(&shared.consumed), total);
    assert!(count(&shared.stray) == 0, "ring selftest {:?}: {} values came out that went in nowhere",
        shape, count(&shared.stray));
    assert!(count(&shared.twice) == 0, "ring selftest {:?}: {} values came out twice",
        shape, count(&shared.twice));
    assert!(count(&shared.stale) == 0,
        "ring selftest {:?}: {} values came out before what was written with them", shape, count(&shared.stale));
    assert!(count(&shared.disorder) == 0,
        "ring selftest {:?}: {} values came out ahead of one pushed before them", shape, count(&shared.disorder));
    let seen = shared.seen.ones();
    assert!(seen as usize == total && count(&shared.consumed) == total,
        "ring selftest {:?}: {} of {} values came out", shape, seen, total);
    assert!(shared.ring.pop().is_none() && shared.ring.len() == 0,
        "ring selftest {:?}: something left in the ring", shape);
    micros
}

pub fn selftest() {
    check_one_cpu();
    let tenths = time_one_cpu();

    /* The first CPUs that run */
    let online = kcore::cpu::online_mask();
    let mut cpus = [0u32; MAX_TASKS];
    let mut found = 0;
    for cpu in 0..MAX_CPUS as u32 {
        if found < MAX_TASKS && online & (1u64 << cpu) != 0 {
            cpus[found] = cpu;
            found += 1;
        }
    }
    let cpus = &cpus[..found.max(1)];

    /* One and one on a ring of two: where a wrong ordering shows first. The
     * consumer goes on the second CPU. */
    let apart = cpus.len() > 1;
    let one = if apart { ONE_TO_ONE } else { ONE_CPU };
    let pair = run(2, 1, 1, one, cpus);
    let each = if apart { TWO_TO_TWO } else { ONE_CPU };
    let crowd = run(16, MAX_PRODUCERS, 2, each, cpus);

    let used = cpus.iter().fold(0u64, |mask, &cpu| mask | 1u64 << cpu);
    trace!(0, "ring selftest: passed on cpus {:#x}: {} values one to one through 2 cells in {} us, \
        {} values two to two through 16 in {} us; a push and a pop {}.{} ns on one cpu",
        used, one, pair, MAX_PRODUCERS * each, crowd, tenths / 10, tenths % 10);
}
