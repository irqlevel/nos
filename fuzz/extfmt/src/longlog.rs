//! A log longer than one allocation of the kernel's heap could list: the
//! replay's plan of a journal of thousands of transactions, held to asking
//! the heap for no more than 128 pages at once -- what the kernel's gives
//! (`PageTable::MAX_CONTIGUOUS_PAGES`), past which `kernel_alloc` returns
//! null and the mount that replays the log panics. The planner once kept a
//! list of every transaction and every copy: a log of 8193 transactions
//! asked for 917504 bytes, which no host allocator refuses and no gate saw.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use extfmt::jbd2::{self, set_be32, set_be64, JournalSuper};
use extfmt::recovery;

/// The system's allocator, the largest single request it is asked for kept.
pub struct Tracking;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

// SAFETY: the system's allocator, every call passed through unchanged; the
// size of each is only noted.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's contract is the system's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as above.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// What one allocation of the kernel's heap is at most.
const KERNEL_MAX: usize = 128 * 4096;

const BS: usize = 1024;
/// More transactions than a list of them fits in 512 KiB.
const TRANSACTIONS: u32 = 9000;
/// The home blocks the copies go to, round and round.
const HOMES: u32 = 61;

/// A journal of `TRANSACTIONS` transactions of one copy each, with checksums
/// (v3): its superblock, and its blocks.
fn journal() -> (JournalSuper, Vec<Vec<u8>>) {
    let maxlen = 3 * TRANSACTIONS + 4;
    let mut raw = vec![0u8; BS];
    set_be32(&mut raw, 0, jbd2::MAGIC);
    set_be32(&mut raw, 4, jbd2::SUPERBLOCK_V2);
    set_be32(&mut raw, jbd2::S_BLOCKSIZE, BS as u32);
    set_be32(&mut raw, jbd2::S_MAXLEN, maxlen);
    set_be32(&mut raw, jbd2::S_FIRST, 1);
    set_be32(&mut raw, jbd2::S_SEQUENCE, 1);
    set_be32(&mut raw, jbd2::S_START, 1);
    set_be32(&mut raw, jbd2::S_FEATURE_INCOMPAT, jbd2::INCOMPAT_REVOKE | jbd2::INCOMPAT_CSUM_V3);
    raw[jbd2::S_CHECKSUM_TYPE] = jbd2::CHECKSUM_CRC32C;
    for (k, b) in raw[jbd2::S_UUID..jbd2::S_UUID + 16].iter_mut().enumerate() {
        *b = 0x40 + k as u8;
    }
    let mut jsb = JournalSuper::parse(&raw).expect("a journal superblock");
    jsb.set_checksum();
    let (seed, uuid) = (jsb.csum_seed(), jsb.uuid());
    let mut blocks = vec![vec![0u8; BS]; maxlen as usize];
    blocks[0][..jbd2::SB_SIZE].copy_from_slice(jsb.raw());
    for t in 0..TRANSACTIONS {
        let (seq, at) = (1 + t, 1 + 3 * t as usize);
        let mut copy = vec![0u8; BS];
        copy[..4].copy_from_slice(&t.to_le_bytes());
        let mut d = vec![0u8; BS];
        set_be32(&mut d, 0, jbd2::MAGIC);
        set_be32(&mut d, 4, jbd2::DESCRIPTOR_BLOCK);
        set_be32(&mut d, 8, seq);
        set_be32(&mut d, jbd2::HEADER, 100 + t % HOMES);
        set_be32(&mut d, jbd2::HEADER + 4, jbd2::FLAG_LAST_TAG);
        set_be32(&mut d, jbd2::HEADER + 12, jbd2::copy_csum(seed, seq, &copy));
        d[jbd2::HEADER + 16..jbd2::HEADER + 16 + jbd2::TAG_UUID].copy_from_slice(&uuid);
        let c = jbd2::block_tail_csum(seed, &d).expect("room for a tail");
        set_be32(&mut d, BS - jbd2::BLOCK_TAIL, c);
        let mut commit = vec![0u8; BS];
        set_be32(&mut commit, 0, jbd2::MAGIC);
        set_be32(&mut commit, 4, jbd2::COMMIT_BLOCK);
        set_be32(&mut commit, 8, seq);
        set_be64(&mut commit, jbd2::H_COMMIT_SEC, 1_750_000_000 + u64::from(t));
        let c = jbd2::commit_csum(seed, &commit).expect("a commit block");
        set_be32(&mut commit, jbd2::H_CHKSUM, c);
        blocks[at] = d;
        blocks[at + 1] = copy;
        blocks[at + 2] = commit;
    }
    (jsb, blocks)
}

/// The plan of the long log: every transaction whole, the newest copy of
/// each home the last transaction's that named it -- and no allocation
/// past what the kernel's heap gives at once. 0, or 1 said.
pub fn run() -> i32 {
    let (jsb, blocks) = journal();
    let mut read = |jb: u32, out: &mut [u8]| match blocks.get(jb as usize) {
        Some(b) => {
            out.copy_from_slice(b);
            Ok(())
        }
        None => Err(format!("journal block {} past the journal", jb)),
    };
    LARGEST.store(0, Ordering::Relaxed);
    let plan = recovery::plan(&jsb, BS, &mut read);
    let largest = LARGEST.load(Ordering::Relaxed);
    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            eprintln!("long-log: refused: {}", e);
            return 1;
        }
    };
    println!("long-log: {} transactions, {} homes written, the largest allocation {} bytes", plan.transactions,
             plan.writes.len(), largest);
    let mut ok = plan.transactions == TRANSACTIONS && plan.writes.len() == HOMES as usize
        && plan.next_sequence == TRANSACTIONS + 2;
    for (&home, w) in &plan.writes {
        /* The last transaction t naming home 100 + h: t = h + HOMES * k. */
        let h = (home - 100) as u32;
        let last = h + HOMES * ((TRANSACTIONS - 1 - h) / HOMES);
        if w.sequence != 1 + last || w.log != 2 + 3 * last {
            eprintln!("long-log: home {} planned from transaction {} at journal block {}, not {}", home, w.sequence,
                      w.log, 1 + last);
            ok = false;
        }
    }
    if largest > KERNEL_MAX {
        eprintln!("long-log: the plan asked for {} bytes at once, past the {} the kernel's heap gives", largest,
                  KERNEL_MAX);
        ok = false;
    }
    if ok { 0 } else { 1 }
}
