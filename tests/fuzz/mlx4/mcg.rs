//! `src/mcg.rs` against a simulated MCG table (review finding 627bcdb9b490,
//! #36): detach frees what attach took, so rotating multicast groups never
//! fills the table, and a multicast list is applied as one step. Run by
//! `tests/mlx4-test.py`.
extern crate alloc;

#[path = "../../../src/crates/drivers/net/mlx4/src/mcg.rs"]
mod mcg;
use mcg::{attach, capacity, detach, gid, sync, Amgm, Entry, Error, Members, Table};

const QP: u32 = 0x48;
const SIZE: usize = 1024;

/// A table of `n` entries: the first half hashed, the second overflow.
struct Sim {
    e: Vec<Entry>,
    /// Hash slots used; 1 sends every GID to slot 0, forcing chains.
    slots: u32,
    writes: usize,
    /// Refuse the write with this number (counting from 1).
    fail_write: Option<usize>,
}

impl Sim {
    fn new(n: usize, slots: u32) -> Sim {
        Sim { e: vec![Entry::default(); n], slots, writes: 0, fail_write: None }
    }

    /// What the device does with a frame: walk the chain from the hash slot
    /// comparing GIDs, and deliver to the member QPs of the match.
    fn lookup(&self, g: &[u8; 16]) -> Vec<u32> {
        let mut at = self.slot(g);
        for _ in 0..self.e.len() {
            let e = &self.e[at as usize];
            if !e.qps.is_empty() && e.gid == *g {
                return e.qps.clone();
            }
            if e.next == 0 {
                break;
            }
            at = e.next;
        }
        Vec::new()
    }

    fn slot(&self, g: &[u8; 16]) -> u32 {
        u32::from(g[15]) % self.slots
    }

    /// Entries holding members.
    fn used(&self) -> usize {
        self.e.iter().filter(|e| !e.qps.is_empty()).count()
    }

    /// Every empty entry is unreachable, or an empty hash slot ending its chain.
    fn check(&self) {
        let half = self.e.len() as u32 / 2;
        for h in 0..half {
            let mut at = h;
            let mut n = 0;
            loop {
                let e = &self.e[at as usize];
                if e.qps.is_empty() {
                    assert!(at == h && e.next == 0, "empty entry {at} inside chain {h}");
                }
                if e.next == 0 {
                    break;
                }
                assert!(e.next >= half, "chain {h} links into the hash half");
                at = e.next;
                n += 1;
                assert!(n <= self.e.len(), "chain {h} loops");
            }
        }
    }
}

impl Table for Sim {
    type Error = &'static str;
    fn hash(&mut self, g: &[u8; 16]) -> Result<u32, &'static str> {
        Ok(self.slot(g))
    }
    fn read(&mut self, at: u32) -> Result<Entry, &'static str> {
        Ok(self.e[at as usize].clone())
    }
    fn write(&mut self, at: u32, e: &Entry) -> Result<(), &'static str> {
        self.writes += 1;
        if self.fail_write == Some(self.writes) {
            return Err("WRITE_MCG failed");
        }
        self.e[at as usize] = e.clone();
        Ok(())
    }
}

fn mac(i: u32) -> [u8; 6] {
    let b = i.to_be_bytes();
    [0x01, 0x00, 0x5e, b[1], b[2], b[3]]
}

fn g(i: u32) -> [u8; 16] {
    gid(1, mac(i), false)
}

#[test]
fn gid_layout() {
    let x = gid(2, [0x01, 0x00, 0x5e, 1, 2, 3], false);
    assert_eq!(x, [0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0x01, 0x00, 0x5e, 1, 2, 3]);
    assert_eq!(gid(1, [0; 6], true)[7], 0x02);
    assert_eq!(capacity(1024), 248);
}

#[test]
fn entry_round_trips_through_mailbox_bytes() {
    let e = Entry { next: 0x1234, prot: 1, gid: g(7), qps: vec![QP, 0x4000_0049] };
    let mut b = [0xaau8; SIZE];
    e.encode(&mut b);
    assert_eq!(&b[0..8], &[0x00, 0x04, 0x8d, 0x00, 0x40, 0x00, 0x00, 0x02]);
    assert!(b[8..0x10].iter().all(|&x| x == 0), "reserved bytes zero");
    assert_eq!(Entry::decode(&b), e);
}

#[test]
fn attach_is_delivered_and_detach_stops_it() {
    let mut t = Sim::new(8, 4);
    let mut a = Amgm::new(8);
    let r = attach(&mut t, &mut a, &g(1), QP, capacity(SIZE)).unwrap();
    assert!(r.added);
    assert_eq!(t.lookup(&g(1)), [QP]);
    let again = attach(&mut t, &mut a, &g(1), QP, capacity(SIZE)).unwrap();
    assert!(!again.added, "a member is not added twice");
    assert_eq!(t.lookup(&g(1)), [QP]);
    assert!(detach(&mut t, &mut a, &g(1), QP).unwrap());
    assert!(t.lookup(&g(1)).is_empty());
    assert!(!detach(&mut t, &mut a, &g(1), QP).unwrap(), "not a member any more");
    assert_eq!(t.used(), 0);
    t.check();
}

#[test]
fn detach_of_one_qp_keeps_the_others() {
    let mut t = Sim::new(8, 4);
    let mut a = Amgm::new(8);
    attach(&mut t, &mut a, &g(1), QP, capacity(SIZE)).unwrap();
    attach(&mut t, &mut a, &g(1), QP + 1, capacity(SIZE)).unwrap();
    assert!(detach(&mut t, &mut a, &g(1), QP).unwrap());
    assert_eq!(t.lookup(&g(1)), [QP + 1]);
}

/// The finding: groups rotating through one hash chain, at most 3 at once,
/// in a table with 4 overflow entries. Without detach freeing entries this
/// fills the table after 4 groups.
#[test]
fn rotating_groups_never_fill_the_table() {
    let mut t = Sim::new(8, 1);
    let mut a = Amgm::new(8);
    for i in 0..1000u32 {
        attach(&mut t, &mut a, &g(i), QP, capacity(SIZE)).unwrap_or_else(|e| panic!("group {i}: {e:?}"));
        if i >= 3 {
            assert!(detach(&mut t, &mut a, &g(i - 3), QP).unwrap());
        }
        assert!(t.used() <= 4);
        assert!(a.in_use() <= 3);
        t.check();
        for j in i.saturating_sub(2)..=i {
            assert_eq!(t.lookup(&g(j)), [QP], "group {j} after attaching {i}");
        }
    }
}

#[test]
fn detach_from_a_chain_head_middle_and_end_keeps_the_rest_reachable() {
    for victim in 0..4u32 {
        let mut t = Sim::new(8, 1);
        let mut a = Amgm::new(8);
        for i in 0..4 {
            attach(&mut t, &mut a, &g(i), QP, capacity(SIZE)).unwrap();
        }
        assert_eq!(a.in_use(), 3);
        assert!(detach(&mut t, &mut a, &g(victim), QP).unwrap());
        assert_eq!(a.in_use(), 2, "victim {victim}: one overflow entry freed");
        t.check();
        for i in (0..4).filter(|&i| i != victim) {
            assert_eq!(t.lookup(&g(i)), [QP], "victim {victim}: group {i}");
        }
        assert!(t.lookup(&g(victim)).is_empty());
    }
}

#[test]
fn a_full_table_says_so() {
    let mut t = Sim::new(4, 1);
    let mut a = Amgm::new(4);
    for i in 0..3 {
        attach(&mut t, &mut a, &g(i), QP, capacity(SIZE)).unwrap();
    }
    assert_eq!(attach(&mut t, &mut a, &g(3), QP, capacity(SIZE)), Err(Error::TableFull));
    // A detach makes room again.
    detach(&mut t, &mut a, &g(1), QP).unwrap();
    attach(&mut t, &mut a, &g(3), QP, capacity(SIZE)).unwrap();
    t.check();
}

#[test]
fn a_failed_link_gives_the_overflow_entry_back() {
    let mut t = Sim::new(4, 1);
    let mut a = Amgm::new(4);
    attach(&mut t, &mut a, &g(0), QP, capacity(SIZE)).unwrap();
    // The new entry is written (write 2), linking it from the head fails (write 3).
    t.fail_write = Some(3);
    assert_eq!(attach(&mut t, &mut a, &g(1), QP, capacity(SIZE)), Err(Error::Cmd("WRITE_MCG failed")));
    assert_eq!(a.in_use(), 0);
    assert!(t.lookup(&g(1)).is_empty());
    attach(&mut t, &mut a, &g(1), QP, capacity(SIZE)).unwrap();
    attach(&mut t, &mut a, &g(2), QP, capacity(SIZE)).unwrap();
    t.check();
}

/// An SNP port over the simulated table, as `eth.rs`'s `Steer`.
struct Port {
    t: Sim,
    a: Amgm,
    refuse_join: Option<[u8; 6]>,
    refuse_leave: bool,
}

impl Port {
    fn new() -> Port {
        Port { t: Sim::new(64, 8), a: Amgm::new(64), refuse_join: None, refuse_leave: false }
    }

    fn attached(&self) -> Vec<[u8; 6]> {
        (0..64).filter(|&i| !self.t.lookup(&g(i)).is_empty()).map(mac).collect()
    }
}

impl Members for Port {
    type Error = &'static str;
    fn join(&mut self, m: [u8; 6]) -> Result<(), &'static str> {
        if self.refuse_join == Some(m) {
            return Err("refused");
        }
        attach(&mut self.t, &mut self.a, &gid(1, m, false), QP, capacity(SIZE)).map(|_| ()).map_err(|_| "attach")
    }
    fn leave(&mut self, m: [u8; 6]) -> Result<(), &'static str> {
        if self.refuse_leave {
            return Err("refused");
        }
        detach(&mut self.t, &mut self.a, &gid(1, m, false), QP).map(|_| ()).map_err(|_| "detach")
    }
}

#[test]
fn sync_replaces_the_list_and_detaches_what_left_it() {
    let mut p = Port::new();
    let mut joined = Vec::new();
    assert_eq!(sync(&mut p, &mut joined, &[mac(1), mac(2), mac(3)]), Ok(0));
    assert_eq!(p.attached(), [mac(1), mac(2), mac(3)]);
    assert_eq!(sync(&mut p, &mut joined, &[mac(3), mac(4)]), Ok(0));
    assert_eq!(p.attached(), [mac(3), mac(4)]);
    assert_eq!(joined, [mac(3), mac(4)]);
    // A reset: nothing listed, nothing attached.
    assert_eq!(sync(&mut p, &mut joined, &[]), Ok(0));
    assert!(p.attached().is_empty() && joined.is_empty());
    assert_eq!(p.t.used(), 0);
    p.t.check();
}

#[test]
fn sync_rotating_16_groups_keeps_the_table_bounded() {
    let mut p = Port::new();
    let mut joined = Vec::new();
    for round in 0..200u32 {
        let want: Vec<[u8; 6]> = (0..16).map(|i| mac(round * 16 + i)).collect();
        sync(&mut p, &mut joined, &want).unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert_eq!(joined, want);
        assert_eq!(p.t.used(), 16);
        p.t.check();
    }
}

/// A failure partway through a new list leaves no partial membership.
#[test]
fn sync_join_failure_rolls_back() {
    let mut p = Port::new();
    let mut joined = Vec::new();
    sync(&mut p, &mut joined, &[mac(1), mac(2)]).unwrap();
    p.refuse_join = Some(mac(5));
    assert_eq!(sync(&mut p, &mut joined, &[mac(2), mac(3), mac(4), mac(5), mac(6)]), Err("refused"));
    assert_eq!(joined, [mac(1), mac(2)], "the list is as it was");
    assert_eq!(p.attached(), [mac(1), mac(2)], "no partial memberships left behind");
    p.t.check();
}

#[test]
fn sync_leave_failure_stays_tracked_for_the_next_change() {
    let mut p = Port::new();
    let mut joined = Vec::new();
    sync(&mut p, &mut joined, &[mac(1), mac(2)]).unwrap();
    p.refuse_leave = true;
    assert_eq!(sync(&mut p, &mut joined, &[mac(2)]), Ok(1));
    assert_eq!(joined, [mac(1), mac(2)]);
    p.refuse_leave = false;
    assert_eq!(sync(&mut p, &mut joined, &[mac(2)]), Ok(0));
    assert_eq!(joined, [mac(2)]);
    assert_eq!(p.attached(), [mac(2)]);
}
