//! The pool's seed, kept from one boot to the next on the root filesystem as
//! `/etc/random-seed`: what seeds it on a machine with no random instruction
//! and a cycle counter too steady for the jitter collector early in boot --
//! the ASUS RS500's Westmere, where sshd otherwise had nothing to make a key
//! with. nos-install writes the first from its Linux's `/dev/urandom`; every
//! boot mixes the one it finds in and writes the next over it, drawn from the
//! pool with it inside, before the pool counts as seeded by it
//! (`random::restore_seed`). A file of another size is not taken; with none,
//! a pool something else seeded keeps one for the next boot.

use fs::vfs::{Open, Vfs, OPEN_CREATE, OPEN_READ, OPEN_TRUNCATE, OPEN_WRITE};
use kcore::klog;

const PATH: &[u8] = b"/etc/random-seed";

/// `seed` written whole over what the file held, and on the medium.
fn keep(vfs: &Vfs, seed: &[u8], create: bool) -> bool {
    let flags = OPEN_WRITE | OPEN_TRUNCATE | if create { OPEN_CREATE } else { 0 };
    let Some(file) = Open::new(vfs, PATH, flags) else { return false };
    file.write(seed) && file.size() == seed.len() && file.sync()
}

fn wipe(bytes: &mut [u8]) {
    bytes.fill(0);
    core::hint::black_box(bytes);
}

/// A seed for the next boot from a pool something has seeded, over a file
/// of the wrong size or where there was none.
fn keep_fresh(vfs: &Vfs, create: bool) {
    let mut next = [0u8; random::STORED_SEED_BYTES];
    if !kcore::random::fill_random(&mut next) {
        klog!(0, "Random: no seed kept for the next boot: the pool is unseeded");
        return;
    }
    if !keep(vfs, &next, create) {
        klog!(0, "Random: the seed for the next boot could not be written");
    }
    wipe(&mut next);
}

/// Boot, once the root filesystem is mounted -- after every CPU is up and
/// interrupts come, and before /etc/rc starts anything that wants keys: the
/// sources asked again, the jitter collector among them, which its first
/// draw, made before either, may have found too quiet; then the kept seed.
#[no_mangle]
pub extern "C" fn rust_random_seed_from_root() {
    random::rust_random_reseed();

    let Some(vfs) = fs::vfs_instance() else { return };
    let mut seed = [0u8; random::STORED_SEED_BYTES];
    let found = Open::new(vfs, PATH, OPEN_READ).map(|file| (file.size(), file.read(&mut seed)));
    match found {
        Some((size, Some(got))) if size == seed.len() && got == seed.len() => {
            random::restore_seed(&seed, |next| keep(vfs, next, false));
        }
        Some((size, _)) => {
            klog!(0, "Random: /etc/random-seed holds {} bytes, not the {} a seed is: not taken", size,
                random::STORED_SEED_BYTES);
            if random::seeded() {
                keep_fresh(vfs, false);
            }
        }
        None if random::seeded() => keep_fresh(vfs, true),
        None => klog!(0, "Random: no seed kept on the root filesystem, and nothing else seeded the pool -- \
            https and sshd will not work (docs/random.md)"),
    }
    wipe(&mut seed);
}
