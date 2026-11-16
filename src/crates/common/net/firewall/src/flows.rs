//! The flows a port keeps: each let through by the rules once, its packets
//! and its answers going by after that without being judged again, until it
//! has been quiet for as long as it may be.
//!
//! A table is taken whole when it is made -- its entries and a hash of
//! them, the hash keyed by a seed of the caller's so that the far end of a
//! flow cannot choose what collides -- and nothing after that allocates: the
//! switch keeps one a port, under a spin lock. When every entry is live, the
//! next flow takes the place of the one used least lately, which is the
//! same VM's: a VM that opens flows as fast as it can puts out its own, and
//! nobody else's.

use alloc::vec::Vec;

use crate::rules::Dir;

/// No entry: the end of a hash chain, of the free list, or of the list from
/// the oldest flow to the newest.
const NONE: u16 = u16::MAX;
/// The most flows a table holds: its places are `u16`s, and `NONE` is none.
pub const CAPACITY_MAX: usize = NONE as usize;

/// A flow, from the VM's side: its protocol, the VM's port -- an echo's
/// identifier -- and the other end's address and port, 0 where there is
/// none. The VM's own address is the port's and is not in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    pub proto: u8,
    pub local_port: u16,
    pub peer: u32,
    pub peer_port: u16,
}

#[derive(Clone, Copy)]
struct Entry {
    key: Key,
    /// Which way its first packet went: what it is judged again by when the
    /// rules change, and which of its packets are answers.
    initiated: Dir,
    /// What its TCP connection has shown (`netwire::flow::SEEN_*`).
    seen: u8,
    live: bool,
    /// When it runs out, on the caller's clock.
    expires: u64,
    /// The next entry in its hash chain -- or, free, on the free list.
    next: u16,
    /// Its neighbours on the list from the one used least lately to the one
    /// used last.
    older: u16,
    newer: u16,
}

const FREE: Entry = Entry {
    key: Key { proto: 0, local_port: 0, peer: 0, peer_port: 0 },
    initiated: Dir::In, seen: 0, live: false, expires: 0, next: NONE, older: NONE, newer: NONE,
};

/// A flow made: its place, and whether a live one was put out for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Added {
    pub at: usize,
    pub evicted: bool,
}

/// One port's flows.
pub struct Flows {
    entries: Vec<Entry>,
    buckets: Vec<u16>,
    seed: u64,
    /// The first free entry.
    free: u16,
    /// The live flow used least lately, and the one used last.
    oldest: u16,
    newest: u16,
    live: usize,
}

impl Flows {
    /// A table of `capacity` flows, its hash keyed by `seed`: None for a
    /// capacity of none or past `CAPACITY_MAX`, or no memory for it.
    pub fn new(capacity: usize, seed: u64) -> Option<Flows> {
        if capacity == 0 || capacity > CAPACITY_MAX {
            return None;
        }
        let mut entries = Vec::new();
        entries.try_reserve_exact(capacity).ok()?;
        entries.resize(capacity, FREE);
        let mut buckets = Vec::new();
        let width = capacity.next_power_of_two();
        buckets.try_reserve_exact(width).ok()?;
        buckets.resize(width, NONE);
        let mut flows = Flows { entries, buckets, seed, free: NONE, oldest: NONE, newest: NONE, live: 0 };
        flows.clear(seed);
        Some(flows)
    }

    /// Every flow let go -- the port has a new owner -- and the hash keyed
    /// anew by `seed`.
    pub fn clear(&mut self, seed: u64) {
        self.seed = seed;
        let n = self.entries.len();
        for (i, e) in self.entries.iter_mut().enumerate() {
            *e = Entry { next: if i + 1 < n { (i + 1) as u16 } else { NONE }, ..FREE };
        }
        self.buckets.fill(NONE);
        self.free = 0;
        self.oldest = NONE;
        self.newest = NONE;
        self.live = 0;
    }

    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// The flows held, live or run out and not yet let go.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    fn bucket(&self, k: &Key) -> usize {
        let mut h = self.seed ^ u64::from(k.proto);
        for v in [u64::from(k.local_port), u64::from(k.peer), u64::from(k.peer_port)] {
            h = (h ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        (h >> 32) as usize & (self.buckets.len() - 1)
    }

    /// The live flow `k` is, at `now`: one that has run out is let go here.
    pub fn find(&mut self, k: &Key, now: u64) -> Option<usize> {
        let mut at = self.buckets[self.bucket(k)];
        while at != NONE {
            let i = usize::from(at);
            if self.entries[i].key == *k {
                if self.entries[i].expires <= now {
                    self.release(i);
                    return None;
                }
                return Some(i);
            }
            at = self.entries[i].next;
        }
        None
    }

    /// Flow `k`, begun `initiated`, having shown `seen`, to run out at
    /// `expires`: in a free place, or -- none free -- in the place of the
    /// flow used least lately, which `evicted` says was live at `now`. The
    /// caller has found no flow `k` is.
    pub fn add(&mut self, k: Key, initiated: Dir, seen: u8, expires: u64, now: u64) -> Added {
        let mut evicted = false;
        if self.free == NONE {
            let oldest = usize::from(self.oldest);
            evicted = self.entries[oldest].expires > now;
            self.release(oldest);
        }
        let i = usize::from(self.free);
        self.free = self.entries[i].next;
        let bucket = self.bucket(&k);
        self.entries[i] = Entry {
            key: k, initiated, seen, live: true, expires, next: self.buckets[bucket], older: NONE, newer: NONE,
        };
        self.buckets[bucket] = i as u16;
        self.link_newest(i);
        self.live += 1;
        Added { at: i, evicted }
    }

    pub fn initiated(&self, at: usize) -> Dir {
        self.entries[at].initiated
    }

    pub fn seen(&self, at: usize) -> u8 {
        self.entries[at].seen
    }

    /// Flow `at` seen again: what it has shown, when it runs out now, and the
    /// newest of the port's.
    pub fn renew(&mut self, at: usize, seen: u8, expires: u64) {
        self.entries[at].seen = seen;
        self.entries[at].expires = expires;
        if usize::from(self.newest) != at {
            self.unlink(at);
            self.link_newest(at);
        }
    }

    /// Flow `at` let go.
    pub fn remove(&mut self, at: usize) {
        if self.entries.get(at).is_some_and(|e| e.live) {
            self.release(at);
        }
    }

    /// The flows in places `from` to `from + count` -- a part of the table,
    /// so that a pass over it can let its lock go between parts -- each kept
    /// or let go as `keep` says of its key and the way it began; those run
    /// out by `now` let go too. Where the next part begins, the capacity past
    /// the end, and how many went.
    pub fn sweep(&mut self, from: usize, count: usize, now: u64, keep: &mut dyn FnMut(&Key, Dir) -> bool)
        -> (usize, usize)
    {
        let end = from.saturating_add(count).min(self.entries.len());
        let mut gone = 0;
        for i in from..end {
            let e = self.entries[i];
            if e.live && (e.expires <= now || !keep(&e.key, e.initiated)) {
                self.release(i);
                gone += 1;
            }
        }
        (end, gone)
    }

    /// Every live flow -- its key, the way it began, what it has shown and
    /// when it runs out -- for a listing.
    pub fn each(&self, now: u64, f: &mut dyn FnMut(&Key, Dir, u8, u64)) {
        for e in self.entries.iter().filter(|e| e.live && e.expires > now) {
            f(&e.key, e.initiated, e.seen, e.expires);
        }
    }

    /// Entry `i` onto the end of the list: the one used last.
    fn link_newest(&mut self, i: usize) {
        let newest = self.newest;
        self.entries[i].older = newest;
        self.entries[i].newer = NONE;
        if newest == NONE {
            self.oldest = i as u16;
        } else {
            self.entries[usize::from(newest)].newer = i as u16;
        }
        self.newest = i as u16;
    }

    /// Entry `i` off the list.
    fn unlink(&mut self, i: usize) {
        let e = self.entries[i];
        if e.older == NONE {
            self.oldest = e.newer;
        } else {
            self.entries[usize::from(e.older)].newer = e.newer;
        }
        if e.newer == NONE {
            self.newest = e.older;
        } else {
            self.entries[usize::from(e.newer)].older = e.older;
        }
    }

    /// Entry `i`, live, out of its chain and the list, and onto the free
    /// list.
    fn release(&mut self, i: usize) {
        let bucket = self.bucket(&self.entries[i].key);
        let mut at = self.buckets[bucket];
        let mut prev = NONE;
        while at != NONE && usize::from(at) != i {
            prev = at;
            at = self.entries[usize::from(at)].next;
        }
        /* A live entry is in its chain: nothing else unlinks one. */
        if at != NONE {
            let next = self.entries[i].next;
            if prev == NONE {
                self.buckets[bucket] = next;
            } else {
                self.entries[usize::from(prev)].next = next;
            }
        }
        self.unlink(i);
        self.entries[i] = Entry { next: self.free, ..FREE };
        self.free = i as u16;
        self.live -= 1;
    }
}
