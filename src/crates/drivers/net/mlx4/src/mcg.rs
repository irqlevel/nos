//! B0 receive steering: the MCG table's hash chains (docs/spec/connectx3.md
//! 5.4.1), attach and detach, and the multicast set an SNP port keeps
//! attached. No UEFI and no commands here: `eth.rs` gives the READ_MCG,
//! WRITE_MCG and MGID_HASH commands as a `Table`, and the host tests
//! (`test/mcg.rs`) give a simulated table (review finding 627bcdb9b490, #36).
//!
//! Invariant kept by both operations: a free entry (no member QPs) is never
//! inside a chain except as an empty hash slot with no chain behind it, which
//! is the state 5.4.1 step 3 takes as "free, use it at index h". Overflow
//! (AMGM) entries a detach empties are unlinked and reused, so a caller that
//! rotates multicast groups never fills the table.

use alloc::vec::Vec;

/// Offset of the member QP list in an entry (5.4.1 MGM layout).
pub const MEMBERS_AT: usize = 0x20;
/// MGM protocol Ethernet, bits 31:30 of members_count (5.4.1).
pub const PROT_ETH: u32 = 1;
/// The QPN bits of a member QP (bits 23:0; bit 30 blocks loopback).
const QPN: u32 = 0xff_ffff;

/// The 16-byte GID for a MAC on a port (5.4.1 GID table).
pub fn gid(port: u8, mac: [u8; 6], unicast: bool) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[5] = port;
    g[7] = if unicast { 0x02 } else { 0x00 };
    g[10..].copy_from_slice(&mac);
    g
}

/// Member QPs an entry of `size` bytes holds: 4 × (size / 16 − 2) (5.4.1).
pub fn capacity(size: usize) -> usize {
    4 * (size / 16 - 2)
}

/// One MCG entry, the fields of the 5.4.1 MGM layout (reserved bytes zero).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// next_gid_index >> 6: the next entry in the chain, 0 at the end.
    pub next: u32,
    /// members_count bits 31:30.
    pub prot: u32,
    pub gid: [u8; 16],
    /// Member QPs as written (QPN in bits 23:0).
    pub qps: Vec<u32>,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

impl Entry {
    /// From the bytes of a READ_MCG output mailbox.
    pub fn decode(b: &[u8]) -> Entry {
        let members = be32(b, 0x04);
        let n = ((members & QPN) as usize).min((b.len() - MEMBERS_AT) / 4);
        let mut gid = [0u8; 16];
        gid.copy_from_slice(&b[0x10..0x20]);
        Entry {
            next: be32(b, 0x00) >> 6,
            prot: members >> 30,
            gid,
            qps: (0..n).map(|i| be32(b, MEMBERS_AT + 4 * i)).collect(),
        }
    }

    /// Into the bytes of a WRITE_MCG input mailbox (all of `b` is written).
    pub fn encode(&self, b: &mut [u8]) {
        b.fill(0);
        b[0x00..0x04].copy_from_slice(&(self.next << 6).to_be_bytes());
        let members = self.qps.len() as u32 | self.prot << 30;
        b[0x04..0x08].copy_from_slice(&members.to_be_bytes());
        b[0x10..0x20].copy_from_slice(&self.gid);
        for (i, q) in self.qps.iter().enumerate() {
            b[MEMBERS_AT + 4 * i..MEMBERS_AT + 4 * i + 4].copy_from_slice(&q.to_be_bytes());
        }
    }

    fn is(&self, gid: &[u8; 16]) -> bool {
        !self.qps.is_empty() && self.gid == *gid && self.prot == PROT_ETH
    }

    fn has(&self, qpn: u32) -> bool {
        self.qps.iter().any(|q| q & QPN == qpn)
    }
}

/// The three commands of 5.4.1. An `Err` is a failed command.
pub trait Table {
    type Error;
    /// MGID_HASH: the hash slot of `gid`.
    fn hash(&mut self, gid: &[u8; 16]) -> Result<u32, Self::Error>;
    /// READ_MCG.
    fn read(&mut self, at: u32) -> Result<Entry, Self::Error>;
    /// WRITE_MCG (op_mod 0).
    fn write(&mut self, at: u32, e: &Entry) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Cmd(E),
    /// No free overflow (AMGM) entry.
    TableFull,
    /// The entry at this index has no room for another member QP.
    EntryFull(u32),
    /// A chain longer than the table: the table is not as this driver wrote it.
    Loop(u32),
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Cmd(e)
    }
}

/// The overflow entries [mcgs/2, mcgs) (5.4.1; 4.1, MCG index):
/// fresh ones in order, then the ones detach freed.
pub struct Amgm {
    start: u32,
    next: u32,
    end: u32,
    free: Vec<u32>,
}

impl Amgm {
    pub fn new(mcgs: u32) -> Amgm {
        Amgm { start: mcgs / 2, next: mcgs / 2, end: mcgs, free: Vec::new() }
    }

    /// Overflow entries in use.
    pub fn in_use(&self) -> u32 {
        self.next - self.start - self.free.len() as u32
    }

    fn take(&mut self) -> Option<u32> {
        self.free.pop().or_else(|| {
            (self.next < self.end).then(|| {
                self.next += 1;
                self.next - 1
            })
        })
    }

    fn give(&mut self, at: u32) {
        self.free.push(at);
    }

    /// Chain steps beyond this mean a loop.
    fn bound(&self) -> u32 {
        self.end - self.start + 1
    }
}

/// Where an attach put the QP, for the trace.
#[derive(Debug, PartialEq, Eq)]
pub struct Attached {
    pub at: u32,
    pub hash: u32,
    /// False when the QP already was a member.
    pub added: bool,
}

/// Attach `qpn` to `gid` (5.4.1 steps 1–5); `cap` member QPs fit an entry.
pub fn attach<T: Table>(t: &mut T, amgm: &mut Amgm, gid: &[u8; 16], qpn: u32, cap: usize) -> Result<Attached, Error<T::Error>> {
    let hash = t.hash(gid)?;
    let mut at = hash;
    let mut free = None;
    let mut steps = 0;
    let last = loop {
        let mut e = t.read(at)?;
        if e.is(gid) {
            if e.has(qpn) {
                return Ok(Attached { at, hash, added: false });
            }
            if e.qps.len() >= cap {
                return Err(Error::EntryFull(at));
            }
            e.qps.push(qpn);
            t.write(at, &e)?;
            return Ok(Attached { at, hash, added: true });
        }
        if e.qps.is_empty() && free.is_none() {
            free = Some((at, e.next));
        }
        if e.next == 0 {
            break at;
        }
        steps += 1;
        if steps > amgm.bound() {
            return Err(Error::Loop(hash));
        }
        at = e.next;
    };
    let fresh = |next| Entry { next, prot: PROT_ETH, gid: *gid, qps: alloc::vec![qpn] };
    if let Some((at, next)) = free {
        t.write(at, &fresh(next))?;
        return Ok(Attached { at, hash, added: true });
    }
    // Step 4: a new overflow entry, written first and then linked from the
    // end of the chain, so the device never follows a link to a half entry.
    let a = amgm.take().ok_or(Error::TableFull)?;
    let linked = t.write(a, &fresh(0)).and_then(|()| {
        let mut prev = t.read(last)?;
        prev.next = a;
        t.write(last, &prev)
    });
    if let Err(e) = linked {
        amgm.give(a);
        return Err(Error::Cmd(e));
    }
    Ok(Attached { at: a, hash, added: true })
}

/// Detach `qpn` from `gid` (5.4.1 "Detach"). An entry left with no members
/// is unlinked: an emptied overflow entry is cut out of its chain and freed;
/// an emptied hash slot takes over the next entry of its chain (whose
/// overflow entry is freed), or is cleared when it ends the chain. True when
/// the QP was a member.
pub fn detach<T: Table>(t: &mut T, amgm: &mut Amgm, gid: &[u8; 16], qpn: u32) -> Result<bool, Error<T::Error>> {
    let hash = t.hash(gid)?;
    let (mut at, mut prev) = (hash, None);
    let mut steps = 0;
    let mut e = loop {
        let e = t.read(at)?;
        if e.is(gid) {
            break e;
        }
        if e.next == 0 {
            return Ok(false);
        }
        steps += 1;
        if steps > amgm.bound() {
            return Err(Error::Loop(hash));
        }
        prev = Some(at);
        at = e.next;
    };
    if !e.has(qpn) {
        return Ok(false);
    }
    e.qps.retain(|q| q & QPN != qpn);
    if !e.qps.is_empty() {
        t.write(at, &e)?;
        return Ok(true);
    }
    match prev {
        Some(p) => {
            let mut pe = t.read(p)?;
            pe.next = e.next;
            t.write(p, &pe)?;
            t.write(at, &Entry::default())?;
            amgm.give(at);
        }
        None if e.next != 0 => {
            let n = e.next;
            let ne = t.read(n)?;
            t.write(at, &ne)?;
            t.write(n, &Entry::default())?;
            amgm.give(n);
        }
        None => t.write(at, &Entry::default())?,
    }
    Ok(true)
}

/// Joining and leaving one multicast MAC, for `sync`.
pub trait Members {
    type Error;
    fn join(&mut self, mac: [u8; 6]) -> Result<(), Self::Error>;
    fn leave(&mut self, mac: [u8; 6]) -> Result<(), Self::Error>;
}

/// Make `joined` (the MACs attached in hardware) the set `want`. New ones
/// are joined first and as one step: if any join fails, the ones this call
/// joined are left again and `joined` is as it was. Then the ones no longer
/// wanted are left; one whose leave fails stays in `joined`, so the next
/// sync tries again. Returns how many stayed that way.
pub fn sync<M: Members>(m: &mut M, joined: &mut Vec<[u8; 6]>, want: &[[u8; 6]]) -> Result<usize, M::Error> {
    let mut added: Vec<[u8; 6]> = Vec::new();
    for &mac in want {
        if joined.contains(&mac) || added.contains(&mac) {
            continue;
        }
        if let Err(e) = m.join(mac) {
            for &a in added.iter().rev() {
                if m.leave(a).is_err() {
                    // Still attached: keep tracking it.
                    joined.push(a);
                }
            }
            return Err(e);
        }
        added.push(mac);
    }
    joined.extend(added);
    let mut stuck = 0;
    joined.retain(|mac| {
        if want.contains(mac) {
            return true;
        }
        let keep = m.leave(*mac).is_err();
        stuck += keep as usize;
        keep
    });
    Ok(stuck)
}
