//! The kernel's random number generator: one ChaCha20 pool, seeded from every
//! entropy source the machine turns out to have, and the only place the rest
//! of the kernel takes randomness from -- the TLS client, TCP's initial
//! sequence numbers, DHCP's and DNS's ids, SSH's keys. Sources register and
//! contribute; the pool answers. Reading a source directly is what used to
//! happen, and on a bare-metal box with no virtio-rng there was no source to
//! read: the first TLS handshake on real hardware failed with
//! FailedToGetRandomBytes. docs/random.md has the sources and what is, and is
//! not, being claimed.
//!
//! Both halves of the construction are Linux's, in miniature:
//!
//! - output: fast key erasure. A request runs the ChaCha20 block function
//!   over the 32-byte key, keeps the first half of the result as the next
//!   key and hands the second half out, so the state that produced a value
//!   is gone by the time the caller has it. There is no way back from an
//!   output to an earlier one.
//! - seeding: absorb. Seed material is XORed into the key 32 bytes at a time
//!   and the block function run over the result. ChaCha20's feed-forward
//!   addition is what stops that being walked backwards, and XOR never
//!   destroys entropy already in the key, so a source that turns out to be
//!   worthless cannot make the pool worse.
//!
//! The pool is a `static` with nothing to construct, behind a lock that
//! allocates nothing: the boot seeds it from Main2, long before rust_init.
//! Inside the image a source registers as a trait object (`register_source`).
//! The layers -- net, tls, fs -- and the loadable modules ask for bytes
//! through `kcore::random`, which is the C ABI's `kernel_get_random`
//! defined here: the seam a fuzzer stands in for, so that what is random in
//! a fuzzer is the input's.

#![no_std]

mod chacha20;
mod cpu;
mod jitter;

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::cmd::{Command, Output};
use kcore::sync::IrqSpinLock;
use kcore::time::{cycle_counter, wall_clock_secs};
use kcore::trace;

use chacha20::{BLOCK_SIZE, KEY_SIZE, NONCE_SIZE};

/// Something the pool can ask for raw entropy: a hardware generator's driver,
/// the CPU's instruction, the timing collector. It lives for good -- a source
/// is never taken back -- and is asked in task context, from a reseed, where
/// it may be slow: virtio-rng polls its device.
pub trait Source: Sync + 'static {
    /// Fill `buf`, or say it could not -- the pool then goes on with its
    /// other sources.
    fn fill(&'static self, buf: &mut [u8]) -> bool;
}

/// How much each source is asked for at a seeding: a ChaCha20 key is 32
/// bytes, and there is nothing to gain by seeding it with more at once.
const SEED_BYTES: usize = KEY_SIZE;

/// Four virtio-rng devices, the cpu instruction and the jitter collector,
/// with room to spare
const MAX_SOURCES: usize = 8;

/// The trace level of what a reseed has to say about a source that gave
/// nothing, as kernel/trace.h numbers the levels
const RANDOM_LL: u32 = 3;

struct State {
    key: [u8; KEY_SIZE],
    /// The block function's nonce: monotonic, so no key is ever used with a
    /// repeated one even before the key erasure makes that moot
    nonce_counter: u64,
    /// Whether anything better than timing jitter has ever contributed
    hardware: bool,
    reseeds: u64,
    bytes_out: u64,
}

struct Pool {
    state: IrqSpinLock<State>,
    seeded: AtomicBool,
}

static POOL: Pool = Pool {
    state: IrqSpinLock::new(State { key: [0; KEY_SIZE], nonce_counter: 0, hardware: false, reseeds: 0, bytes_out: 0 }),
    seeded: AtomicBool::new(false),
};

#[derive(Clone, Copy)]
struct Registered {
    name: &'static str,
    source: &'static dyn Source,
    /// Anything but the jitter collector: what makes a pool's entropy
    /// "hardware"
    hardware: bool,
}

struct Sources {
    list: [Option<Registered>; MAX_SOURCES],
    count: usize,
}

static SOURCES: IrqSpinLock<Sources> = IrqSpinLock::new(Sources { list: [None; MAX_SOURCES], count: 0 });

static CPU: cpu::Cpu = cpu::Cpu;
static JITTER: jitter::Jitter = jitter::Jitter;

/// Zeroes what held key material in a way the compiler may not drop as a
/// store nothing reads afterwards.
fn wipe(bytes: &mut [u8]) {
    bytes.fill(0);
    core::hint::black_box(bytes);
}

/// Words as the bytes they are in memory, the way the C++ pool absorbed them
fn bytes_of<const N: usize>(words: [u64; N], out: &mut [u8]) -> usize {
    let mut len = 0;
    for (chunk, word) in out.chunks_exact_mut(8).zip(words) {
        chunk.copy_from_slice(&word.to_ne_bytes());
        len += 8;
    }
    len
}

impl State {
    /// A block from the key, whose first half becomes the next key: the key
    /// erasure both the output and the absorb are built on.
    fn generate(&mut self) -> [u8; BLOCK_SIZE] {
        self.nonce_counter = self.nonce_counter.wrapping_add(1);

        /* The counter in the low eight bytes is what keeps the nonce unique;
         * the cycle counter in the other four is free freshness. */
        let mut nonce = [0u8; NONCE_SIZE];
        nonce[..8].copy_from_slice(&self.nonce_counter.to_le_bytes());
        nonce[8..].copy_from_slice(&(cycle_counter() as u32).to_le_bytes());

        let block = chacha20::block(&self.key, 0, &nonce);
        self.key.copy_from_slice(&block[..KEY_SIZE]);
        block
    }

    fn absorb(&mut self, data: &[u8]) {
        let mut off = 0;
        loop {
            let mut chunk = [0u8; KEY_SIZE];
            let n = (data.len() - off).min(KEY_SIZE);
            chunk[..n].copy_from_slice(&data[off..off + n]);

            /* XOR in, then run the block function over the result. The XOR
             * cannot take entropy out of the key, and the feed-forward
             * addition inside ChaCha20 is what stops the new key leading back
             * to the old one. */
            for (k, c) in self.key.iter_mut().zip(chunk) {
                *k ^= c;
            }
            let mut block = self.generate();
            wipe(&mut block);
            wipe(&mut chunk);

            off += n;
            if off >= data.len() {
                break;
            }
        }
    }
}

impl Pool {
    /// Cannot fail and cannot block. Before `setup` it is ChaCha20 output of
    /// a pool nothing has seeded, which is why `kernel_get_random` -- what a
    /// TLS handshake asks -- refuses until it is seeded.
    fn fill(&self, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }

        /* One draw from the CPU's instruction per request where there is
         * one: a few hundred cycles, no lock and no device, and it means
         * every value the pool hands out on such a machine carries entropy
         * the pool never had to store. */
        let fresh = kcore::random::hw_random();
        let stamp = cycle_counter();

        let mut state = self.state.lock();
        if let Some(fresh) = fresh {
            for (k, f) in state.key[..8].iter_mut().zip(fresh.to_le_bytes()) {
                *k ^= f;
            }
        }
        for (k, s) in state.key[8..16].iter_mut().zip(stamp.to_le_bytes()) {
            *k ^= s;
        }

        for piece in out.chunks_mut(KEY_SIZE) {
            let mut block = state.generate();
            /* The half of the block the key was not taken from is the
             * output. */
            piece.copy_from_slice(&block[KEY_SIZE..KEY_SIZE + piece.len()]);
            wipe(&mut block);
        }
        state.bytes_out = state.bytes_out.wrapping_add(out.len() as u64);
    }

    fn u64(&self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill(&mut bytes);
        u64::from_ne_bytes(bytes)
    }

    /// Mix `data` into the pool -- with the moment it was asked, which is the
    /// material when there is no other. Cheap, never blocks, and worth calling
    /// with material that is only partly unpredictable: the absorb cannot
    /// reduce what the pool already has.
    fn add_entropy(&self, data: &[u8]) {
        let stamp = cycle_counter();
        let mut state = self.state.lock();
        state.absorb(&stamp.to_ne_bytes());
        if !data.is_empty() {
            state.absorb(data);
        }
    }

    fn is_seeded(&self) -> bool {
        self.seeded.load(Ordering::Acquire)
    }
}

/// Puts `source` in front of the pool under `name`, for good: a source is
/// never taken back. False when the table is full. What a hardware
/// generator's driver calls: the next reseed takes from it.
pub fn register_source(name: &'static str, source: &'static dyn Source) -> bool {
    register(Registered { name, source, hardware: true })
}

fn register(entry: Registered) -> bool {
    {
        let mut sources = SOURCES.lock();
        let at = sources.count;
        match sources.list.get_mut(at) {
            Some(slot) => *slot = Some(entry),
            None => return false,
        }
        sources.count = at + 1;
    }
    trace!(0, "EntropySource registered: {}", entry.name);
    true
}

/// The sources as they stand: copied out, so that none is asked with the
/// table's lock held
fn sources() -> ([Option<Registered>; MAX_SOURCES], usize) {
    let sources = SOURCES.lock();
    (sources.list, sources.count)
}

/// Seeds the pool from what needs no device -- the CPU's instruction and the
/// timing collector -- and registers those of them that work. Touches no
/// heap and no device, so it can run as early in boot as the trace log does,
/// and it has to: nothing may ask for bytes before it. Whether anything
/// seeded the pool.
fn setup() -> bool {
    /* What this early in the boot can be told apart by at all. Not secret,
     * and on a machine that boots the same image twice not even different
     * -- which is why it is the first thing mixed and not the only one. */
    let here = 0u8;
    let mut marks = [cycle_counter(), &here as *const u8 as u64, 0];
    marks[2] = marks.as_ptr() as u64;
    let mut bytes = [0u8; 24];
    let len = bytes_of(marks, &mut bytes);
    POOL.add_entropy(&bytes[..len]);

    let mut seed = [0u8; SEED_BYTES];
    let mut hardware = false;
    if let Some(name) = cpu::name() {
        if cpu::self_test() {
            register(Registered { name, source: &CPU, hardware: true });
            if CPU.fill(&mut seed) {
                POOL.add_entropy(&seed);
                hardware = true;
            }
        } else {
            trace!(0, "Random: {} failed its self test, not using it", name);
        }
    }

    let jitter = JITTER.fill(&mut seed);
    if jitter {
        register(Registered { name: "jitter", source: &JITTER, hardware: false });
        POOL.add_entropy(&seed);
    }
    wipe(&mut seed);

    POOL.state.lock().hardware = hardware;
    let seeded = hardware || jitter;
    POOL.seeded.store(seeded, Ordering::Release);

    match (hardware, jitter) {
        (true, true) => trace!(0, "Random: seeded from {} and timing jitter", cpu::name().unwrap_or("none")),
        (true, false) => trace!(0, "Random: seeded from {}", cpu::name().unwrap_or("none")),
        (false, true) => trace!(0, "Random: seeded from timing jitter only -- this cpu has no \
            random instruction, see docs/random.md"),
        (false, false) => trace!(0, "Random: nothing seeded the pool, https will not work"),
    }
    seeded
}

/// Folds in a fresh draw from every registered source, virtio-rng included:
/// once the devices are up, and by `entropy reseed`. Never with a spinlock
/// held -- a source may poll its device for milliseconds.
fn reseed() {
    let (list, count) = sources();

    let mut contributed = 0;
    let mut hardware = false;
    for entry in list.iter().flatten() {
        let mut seed = [0u8; SEED_BYTES];
        if !entry.source.fill(&mut seed) {
            trace!(RANDOM_LL, "Random: source {} gave nothing", entry.name);
            continue;
        }
        POOL.add_entropy(&seed);
        wipe(&mut seed);

        contributed += 1;
        hardware |= entry.hardware;
    }

    /* Neither of these is secret; both differ between two boots. */
    let mut bytes = [0u8; 16];
    let len = bytes_of([wall_clock_secs(), cycle_counter()], &mut bytes);
    POOL.add_entropy(&bytes[..len]);

    {
        let mut state = POOL.state.lock();
        state.reseeds = state.reseeds.wrapping_add(1);
        if hardware {
            state.hardware = true;
        }
    }
    if contributed != 0 {
        POOL.seeded.store(true, Ordering::Release);
    }

    trace!(0, "Random: reseeded from {} of {} sources", contributed, count);
}

/* ---- the C ABI ---- */

/// Boot, on the BSP, once Hal::ProbeHwRandom has found what instruction the
/// CPU has, and before anything can ask the pool for bytes. Whether anything
/// seeded it.
#[no_mangle]
pub extern "C" fn rust_random_setup() -> bool {
    setup()
}

/// Boot, once the devices that can do better than the boot itself are up.
#[no_mangle]
pub extern "C" fn rust_random_reseed() {
    reseed()
}

/// `len` random bytes at `buf`, and 1 -- or 0, and nothing written, while
/// the pool is unseeded: a TLS handshake has to fail there
/// (FailedToGetRandomBytes) rather than be keyed from a pool nothing has
/// seeded. How a loadable module asks, and the layers, through
/// `kcore::random`.
///
/// # Safety
/// `buf` is `len` writable bytes, or null.
#[no_mangle]
pub unsafe extern "C" fn kernel_get_random(buf: *mut u8, len: usize) -> i32 {
    if buf.is_null() || len == 0 || !POOL.is_seeded() {
        return 0;
    }
    // SAFETY: the caller's `len` bytes, for the length of the call.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    POOL.fill(out);
    1
}

/* ---- the shell ---- */

/// The most `random` prints
const RANDOM_MAX: usize = 1024;
const RANDOM_DEFAULT: usize = 16;

fn random_command(args: &str, out: &mut Output) {
    let len = if args.is_empty() {
        RANDOM_DEFAULT
    } else {
        match args.parse::<usize>() {
            Ok(len) if (1..=RANDOM_MAX).contains(&len) => len,
            _ => {
                let _ = writeln!(out, "usage: random [len] (1..{}, default {})", RANDOM_MAX, RANDOM_DEFAULT);
                return;
            }
        }
    };

    if !POOL.is_seeded() {
        let _ = writeln!(out, "entropy pool is not seeded");
        return;
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; RANDOM_MAX];
    let mut text = [0u8; 2 * RANDOM_MAX + 1];
    POOL.fill(&mut bytes[..len]);
    for (pair, byte) in text.chunks_exact_mut(2).zip(&bytes[..len]) {
        pair[0] = HEX[usize::from(byte >> 4)];
        pair[1] = HEX[usize::from(byte & 0xF)];
    }
    text[2 * len] = b'\n';
    out.write_bytes(&text[..2 * len + 1]);
    wipe(&mut bytes);
}

fn entropy_command(args: &str, out: &mut Output) {
    if args == "reseed" {
        /* Worth having by hand: a source can appear after the pool was
         * seeded (a virtio-rng behind a bus that was scanned late), and on a
         * machine whose only console is a UDP socket this is how one finds
         * out whether it answers. */
        reseed();
    } else if !args.is_empty() {
        let _ = writeln!(out, "usage: entropy [reseed]");
        return;
    }

    let (hardware, reseeds, bytes_out) = {
        let state = POOL.state.lock();
        (state.hardware, state.reseeds, state.bytes_out)
    };
    let _ = writeln!(out, "pool: chacha20 {}, hardware entropy {}, reseeds {}, bytes out {}",
        if POOL.is_seeded() { "seeded" } else { "UNSEEDED" }, if hardware { "yes" } else { "no" },
        reseeds, bytes_out);
    let _ = writeln!(out, "sources:");

    let (list, count) = sources();
    if count == 0 {
        let _ = writeln!(out, "no entropy sources");
    }
    for entry in list.iter().flatten() {
        let _ = writeln!(out, "{}", entry.name);
    }
}

/// Puts the pool's commands in front of whoever runs one. Called from
/// rust_init.
pub fn init() {
    let commands: &[(&str, &str, fn(&str, &mut Output))] = &[
        ("random", "random [len] - get random bytes as hex", random_command),
        ("entropy", "entropy [reseed] - show the random pool and its sources", entropy_command),
    ];
    for (name, help, handler) in commands {
        let handler = *handler;
        match Command::register(name, help, move |args, out| handler(args, out)) {
            /* The command is the kernel's own and stays for good. */
            Ok(cmd) => core::mem::forget(cmd),
            Err(_) => trace!(0, "random: cannot register the {} command", name),
        }
    }
}

/* ---- the boot's check ---- */

/// The block function against its RFC, then the pool: what a request gets
/// is written, is not one block over and over, reaches the bytes past the
/// first block, and goes on being so after an absorb. A failure panics, as
/// the boot's other self-tests do.
pub fn selftest() {
    chacha20::selftest();

    /* A pool that no source could seed is a property of the machine, not a
     * bug in this code -- boot says so already, and the checks below still
     * mean something, since they are about the generator and not the seed. */
    if !POOL.is_seeded() {
        trace!(0, "random selftest: the pool is unseeded on this machine");
    }

    /* 33 bytes spans two blocks' output and ends mid-block, which is where a
     * length or an offset slip would show. Eight draws, and at least two of
     * them have to differ in the byte past the first block's: a generator
     * stuck on one block, or one that never writes the tail, fails this, and
     * a working one with probability 256^-7. */
    const DRAWS: usize = 8;
    const LEN: usize = 33;
    let mut first = [0u8; LEN];
    let mut differs = false;
    let mut nonzero = false;
    for i in 0..DRAWS {
        let mut buf = [0u8; LEN];
        POOL.fill(&mut buf);
        nonzero |= buf.iter().any(|&b| b != 0);
        if i == 0 {
            first = buf;
        } else if buf[KEY_SIZE] != first[KEY_SIZE] {
            differs = true;
        }
    }
    assert!(nonzero && differs, "random selftest: the pool gives the same bytes, or none");

    let changes = || {
        let mut prev = POOL.u64();
        let mut differs = false;
        for _ in 0..DRAWS {
            let value = POOL.u64();
            differs |= value != prev;
            prev = value;
        }
        differs
    };
    assert!(changes(), "random selftest: a u64 at a time, the pool gives the same one");

    /* An absorb must not wedge the pool or empty it: a lock left held, or a
     * zeroed key, would show as the next draws failing the check above. */
    POOL.add_entropy(b"random selftest");
    POOL.add_entropy(&[]);
    assert!(changes(), "random selftest: after an absorb, the pool gives the same u64");

    trace!(0, "random selftest: passed");
}
