//! What the hypervisor's sources reach of the kernel (`kcore`), as the
//! fuzzer has it: constants, a clock the fuzzer moves, and the locks and the
//! event a vCPU waits on -- for one thread, which is all a run here has.

pub mod consts {
    pub const NS_PER_SEC: u64 = 1_000_000_000;
    pub const NS_PER_MS: u64 = 1_000_000;
    pub const NS_PER_US: u64 = 1_000;
    pub const PAGE_SIZE: usize = 4096;
    pub const MAX_CPUS: usize = 64;
}

/// The host's clock, as the devices read it: the fuzzer's to move, forward
/// only and by as much as it likes -- but no further than a host's clock
/// can go, nanoseconds since boot: a clock near 2^64 would make findings of
/// sums no host will ever do. Each read moves it on a little too, as time
/// passes while code runs: a loop that waits for the clock to reach a
/// point ends, as it would on a host, rather than hanging the fuzzer.
pub mod time {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};
    /// One clock for every thread: a guest's vCPUs run on threads of their
    /// own, one at a time (`crate::vm`).
    static NOW: AtomicU64 = AtomicU64::new(START);
    thread_local! {
        /// Reads since a guest was last entered or a vCPU last waited.
        static IDLE_READS: Cell<u64> = const { Cell::new(0) };
    }
    /// That many reads with neither is a loop that neither runs the guest
    /// nor sleeps: on a host, a CPU spun for as long as it lasts.
    const SPIN_READS: u64 = 1_000_000;
    /// Where each iteration's clock starts: a machine up for a while.
    pub const START: u64 = 1_000_000_000_000;
    /// The latest it gets: a host up for 146 years.
    pub const END: u64 = 1 << 62;
    /// What a read of the clock costs, in time passed.
    const READ_NS: u64 = 50;

    /// `now` moved on by `ns`, as far as the clock goes.
    pub fn later(now: u64, ns: u64) -> u64 {
        now.saturating_add(ns).min(END)
    }

    pub fn boot_time_ns() -> u64 {
        let reads = IDLE_READS.with(|r| {
            r.set(r.get() + 1);
            r.get()
        });
        if reads == SPIN_READS {
            panic!("a spin: the clock read {} times with no guest entered and no wait", reads);
        }
        let now = NOW.load(Ordering::Relaxed);
        NOW.store(later(now, READ_NS), Ordering::Relaxed);
        now
    }

    /// A guest entered, or a vCPU waited: not a spin.
    pub fn progressed() {
        IDLE_READS.with(|r| r.set(0));
    }
    pub fn wall_clock_secs() -> u64 {
        1_790_000_000
    }
    pub fn reset() {
        NOW.store(START, Ordering::Relaxed);
        progressed();
    }
    pub fn advance(ns: u64) {
        NOW.store(later(NOW.load(Ordering::Relaxed), ns), Ordering::Relaxed);
    }

    #[derive(Clone, Copy, Debug)]
    pub struct Duration(u64);

    impl Duration {
        pub fn from_nanos(ns: u64) -> Duration {
            Duration(ns)
        }
        pub fn as_nanos(&self) -> u64 {
            self.0
        }
    }
}

pub mod sync {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};

    thread_local! {
        /// The kernel's locks this thread holds.
        static HELD: Cell<u32> = const { Cell::new(0) };
    }

    /// How many of the kernel's locks this thread holds: none, where a vCPU
    /// enters its guest or waits to be woken -- another CPU wanting one
    /// would wait that long.
    pub fn held() -> u32 {
        HELD.with(|h| h.get())
    }

    /// A lock that owns what it guards; `new` is fallible, as the kernel's is.
    pub struct Mutex<T>(std::sync::Mutex<T>);

    pub struct MutexGuard<'a, T>(std::sync::MutexGuard<'a, T>);

    impl<T> Mutex<T> {
        pub fn new(value: T) -> Option<Mutex<T>> {
            Some(Mutex(std::sync::Mutex::new(value)))
        }
        pub fn lock(&self) -> MutexGuard<'_, T> {
            /* A panic under the lock is a finding already; the lock is not
             * used again after one. */
            let guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
            HELD.with(|h| h.set(h.get() + 1));
            MutexGuard(guard)
        }
    }

    impl<T> Drop for MutexGuard<'_, T> {
        fn drop(&mut self) {
            HELD.with(|h| h.set(h.get() - 1));
        }
    }

    impl<T> core::ops::Deref for MutexGuard<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.0
        }
    }

    impl<T> core::ops::DerefMut for MutexGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.0
        }
    }

    /// What a vCPU's task waits on. There is one thread: a wait for a
    /// signal that has not come is the time passing that it waits for.
    pub struct Event {
        signalled: AtomicBool,
    }

    impl Event {
        pub fn new() -> Option<Event> {
            Some(Event { signalled: AtomicBool::new(false) })
        }
        pub fn signal(&self) {
            self.signalled.store(true, Ordering::Relaxed);
        }
        /// True when signalled; otherwise the other vCPUs run (`vm::sleep`)
        /// and the time passes, and false unless one of them signalled it.
        pub fn wait_for(&self, d: crate::time::Duration) -> bool {
            crate::time::progressed();
            invariant!(held() == 0, "a vCPU waits with {} of the kernel's locks held", held());
            if self.signalled.swap(false, Ordering::Relaxed) {
                return true;
            }
            crate::vm::sleep();
            if self.signalled.swap(false, Ordering::Relaxed) {
                return true;
            }
            crate::time::advance(d.as_nanos());
            false
        }
        pub fn wait(&self) {
            if !self.signalled.swap(false, Ordering::Relaxed) {
                panic!("a wait for good, with nothing to end it");
            }
        }
    }
}

pub mod dma {
    /// Never made here: only the VMCB's page is one, and no VMCB is.
    pub struct DmaBuffer(Vec<u8>);

    impl DmaBuffer {
        pub fn new(_pages: usize) -> Option<DmaBuffer> {
            None
        }
        pub fn phys(&self) -> u64 {
            0
        }
        pub fn as_mut_slice(&mut self) -> &mut [u8] {
            &mut self.0
        }
        pub fn as_pod<T: crate::pod::Pod>(&self) -> Option<&T> {
            None
        }
        pub fn as_pod_mut<T: crate::pod::Pod>(&mut self) -> Option<&mut T> {
            None
        }
    }
}
