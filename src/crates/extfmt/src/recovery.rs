//! What replaying a journal writes, worked out before anything is written:
//! JBD2's recovery (`do_one_pass`) as a plan -- which home block each copy
//! in the log goes to -- that a driver applies, or reads through without
//! writing anything on a read-only mount.
//!
//! The log is read in three passes, as the kernel's recovery reads it. The
//! scan goes from where the superblock says the log starts: block after
//! block of the expected sequence, a transaction whole once its commit block
//! is found, the log ending at the first block that is not the next of it --
//! and it notes every home block a copy in it names. Then the revokes of the
//! transactions found whole: a home block revoked by a transaction is not
//! replayed from that transaction or any before it. Then the copies, oldest
//! first, each checked against its tag's checksum, the newest of each home
//! block the one the plan keeps.
//!
//! Nothing held grows past the log's own blocks, and nothing is one
//! allocation the size of the log: maps of the homes named, of their
//! revokes and of their newest copies, a node at a time. A list of every
//! transaction and every copy, which this once kept, asked a kernel's heap
//! for 917504 bytes at once for a log of 8193 transactions -- past what one
//! allocation of it gives -- and the mount that read such a log panicked.
//!
//! What may be stale is not trusted and what is corrupt is refused, as the
//! kernel decides between them: a commit block whose checksum fails ends
//! the log there, its transaction not replayed -- said, in the plan, when
//! it is no older than the commit before (a commit torn as it was written,
//! or damaged since, as e2fsck says "was corrupt, replay was aborted"),
//! quietly when it is (a stale block); a descriptor or revoke block that fails its checksum in a
//! transaction whose commit is sound, committed no earlier than the one
//! before, is a journal damaged after it was written -- refused, since
//! replaying around it would leave the filesystem neither as it was nor as
//! it was to be. So is a copy whose checksum fails.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec;

use crate::jbd2::{self, JournalSuper};

/// One copy to write: journal block `log` to home block `home`, its first
/// word put back to the magic if it was escaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    pub home: u64,
    pub log: u32,
    pub escaped: bool,
    pub sequence: u32,
}

/// What a replay writes -- the newest copy of each home block the log
/// holds, by home -- and what it found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub writes: BTreeMap<u64, Write>,
    /// The transactions found whole.
    pub transactions: u32,
    /// The sequence the next transaction is to have: one past the first
    /// not found whole, so that whatever of it is in the log is never taken
    /// for a transaction of the next mount's (`jbd2_journal_recover`).
    pub next_sequence: u32,
    /// Copies left out because a later transaction revoked their block.
    pub revoked: u64,
    /// The transaction whose commit block, of the sequence the log was at
    /// and committed no earlier than the one before, failed its checksum:
    /// not replayed, and the log ended there, but -- as the kernel and
    /// e2fsck have it -- a transaction that was corrupt, for the driver to
    /// say so, not the quiet end a stale block is.
    pub failed_commit: Option<u32>,
}

/// How a journal's blocks are read: whole journal block `block` -- its
/// place in the journal, 0 its superblock -- into the buffer.
pub type ReadJournal<'a> = dyn FnMut(u32, &mut [u8]) -> Result<(), String> + 'a;

/// What the features a journal has let this replay: a refusal names what
/// it does not.
pub fn check_features(sb: &JournalSuper) -> Result<(), String> {
    let known = jbd2::INCOMPAT_REVOKE | jbd2::INCOMPAT_64BIT | jbd2::INCOMPAT_CSUM_V3 | jbd2::INCOMPAT_FAST_COMMIT;
    if sb.incompat() & jbd2::INCOMPAT_CSUM_V2 != 0 {
        return Err(String::from("a journal with v2 checksums (replay it under Linux)"));
    }
    if sb.incompat() & jbd2::INCOMPAT_ASYNC_COMMIT != 0 {
        return Err(String::from("a journal of asynchronous commits"));
    }
    if sb.compat() & jbd2::COMPAT_CHECKSUM != 0 {
        return Err(String::from("a journal with v1 checksums (replay it under Linux)"));
    }
    if sb.incompat() & !known != 0 {
        return Err(alloc::format!("journal features {:#x} this does not know", sb.incompat() & !known));
    }
    if sb.csum_v3() && sb.checksum_type() != jbd2::CHECKSUM_CRC32C {
        return Err(alloc::format!("journal checksums of type {}", sb.checksum_type()));
    }
    if !sb.checksum_ok() {
        return Err(String::from("a journal superblock that fails its checksum"));
    }
    Ok(())
}

/// The plan of replaying the journal whose superblock is `sb`, its blocks
/// `block_size` bytes, read through `read`. An empty journal plans nothing.
pub fn plan(sb: &JournalSuper, block_size: usize, read: &mut ReadJournal) -> Result<Plan, String> {
    check_features(sb)?;
    if sb.start() == 0 {
        return Ok(Plan { next_sequence: sb.sequence(), ..Plan::default() });
    }
    if sb.incompat() & jbd2::INCOMPAT_FAST_COMMIT != 0 {
        /* jbd2 alone cannot see whether fast commits are pending past the
         * log: replaying the log without them would be half a replay. */
        return Err(String::from("a journal with fast commits, not empty"));
    }
    if sb.blocksize() as usize != block_size {
        return Err(alloc::format!("journal blocks of {} bytes in a filesystem of {}", sb.blocksize(), block_size));
    }
    let seed = sb.csum_seed();
    let ring = sb.last() - sb.first();
    let mut buf = vec![0u8; block_size];

    /* The scan: where the log ends, and every home a copy in it names. */
    let mut homes: BTreeSet<u64> = BTreeSet::new();
    let mut block = sb.start();
    let mut sequence = sb.sequence();
    let mut whole = 0u32;
    let mut damaged = false;
    let mut last_commit_time = 0u64;
    let mut failed_commit = None;
    let mut scanned = 0u32;
    'scan: while scanned < ring {
        read(block, &mut buf)?;
        scanned += 1;
        let Some((kind, seq)) = jbd2::header(&buf) else { break };
        if seq != sequence {
            break;
        }
        match kind {
            jbd2::DESCRIPTOR_BLOCK => {
                if sb.csum_v2or3() && !jbd2::block_tail_ok(seed, &buf) {
                    damaged = true;
                }
                for tag in jbd2::tags(&buf, sb) {
                    block = next(sb, block);
                    if scanned >= ring {
                        break 'scan;
                    }
                    scanned += 1;
                    homes.insert(tag.block);
                }
            }
            jbd2::REVOKE_BLOCK => {
                if (sb.csum_v2or3() && !jbd2::block_tail_ok(seed, &buf)) || jbd2::revoke_records(&buf, sb).is_err() {
                    damaged = true;
                }
            }
            jbd2::COMMIT_BLOCK => {
                let time = jbd2::commit_time(&buf);
                if sb.csum_v2or3() && !jbd2::commit_ok(seed, &buf) {
                    /* The log ends here either way. A commit older than
                     * the one before is a stale block of an older pass
                     * round the ring; one no older is this transaction's,
                     * damaged -- torn as it was written, or since. */
                    if time >= last_commit_time {
                        failed_commit = Some(sequence);
                    }
                    break;
                }
                if damaged {
                    if time < last_commit_time {
                        /* Stale blocks from before the log last passed
                         * here, taken for this transaction's: the log ends. */
                        break;
                    }
                    return Err(alloc::format!("transaction {}: a descriptor or revoke block fails its checksum \
                                               under a commit that does not", sequence));
                }
                last_commit_time = time;
                sequence = sequence.wrapping_add(1);
                whole += 1;
                damaged = false;
            }
            _ => break,
        }
        block = next(sb, block);
    }

    /* The revokes of the transactions found whole: a block revoked by a
     * transaction is not replayed from it or from any transaction before
     * it -- the latest revoke of each block, of the blocks a copy names. */
    let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
    let mut walk = Walk { block: sb.start(), sequence: sb.sequence(), left: whole };
    while let Some(kind) = walk.step(sb, read, &mut buf)? {
        match kind {
            jbd2::DESCRIPTOR_BLOCK => {
                walk.pass_copies(sb, jbd2::tags(&buf, sb).len());
            }
            jbd2::REVOKE_BLOCK => {
                for home in jbd2::revoke_records(&buf, sb)? {
                    if homes.contains(&home) {
                        revoked.insert(home, walk.sequence);
                    }
                }
            }
            _ => {}
        }
    }
    drop(homes);

    /* The copies, oldest first: the newest of each home kept. */
    let mut plan = Plan { transactions: whole, next_sequence: sequence.wrapping_add(1), failed_commit,
                          ..Plan::default() };
    let mut copy = vec![0u8; block_size];
    let mut walk = Walk { block: sb.start(), sequence: sb.sequence(), left: whole };
    while let Some(kind) = walk.step(sb, read, &mut buf)? {
        if kind != jbd2::DESCRIPTOR_BLOCK {
            continue;
        }
        let seq = walk.sequence;
        for tag in jbd2::tags(&buf, sb) {
            let log = walk.pass_copies(sb, 1);
            if revoked.get(&tag.block).is_some_and(|&s| !tid_gt(seq, s)) {
                plan.revoked += 1;
                continue;
            }
            read(log, &mut copy)?;
            if !jbd2::copy_ok(sb, seed, seq, &tag, &copy) {
                return Err(alloc::format!("transaction {}: the copy of block {} at journal block {} fails its \
                                           checksum", seq, tag.block, log));
            }
            plan.writes.insert(tag.block, Write { home: tag.block, log, escaped: tag.escaped(), sequence: seq });
        }
    }
    Ok(plan)
}

/// The ring's block after `b`.
fn next(sb: &JournalSuper, b: u32) -> u32 {
    if b + 1 >= sb.last() { sb.first() } else { b + 1 }
}

/// A walk of the transactions the scan found whole, again: block by block
/// from the log's start, each held to be what the scan read there.
struct Walk {
    block: u32,
    sequence: u32,
    /// Transactions not yet walked.
    left: u32,
}

impl Walk {
    /// The next descriptor, revoke or commit block into `buf`, and its
    /// kind -- the sequence moved on past a commit, whose kind is still
    /// returned -- or None past the last transaction found whole. A block
    /// that is not the scan's is a journal changed under the replay.
    fn step(&mut self, sb: &JournalSuper, read: &mut ReadJournal, buf: &mut [u8]) -> Result<Option<u32>, String> {
        if self.left == 0 {
            return Ok(None);
        }
        read(self.block, buf)?;
        let kind = match jbd2::header(buf) {
            Some((kind @ (jbd2::DESCRIPTOR_BLOCK | jbd2::REVOKE_BLOCK | jbd2::COMMIT_BLOCK), seq))
                if seq == self.sequence => kind,
            _ => return Err(alloc::format!("journal block {}, of transaction {}, is not what it was a pass before",
                                           self.block, self.sequence)),
        };
        self.block = next(sb, self.block);
        if kind == jbd2::COMMIT_BLOCK {
            self.sequence = self.sequence.wrapping_add(1);
            self.left -= 1;
        }
        Ok(Some(kind))
    }

    /// `n` blocks of copies after a descriptor passed: where the last of
    /// them is.
    fn pass_copies(&mut self, sb: &JournalSuper, n: usize) -> u32 {
        let mut at = self.block;
        for _ in 0..n {
            at = self.block;
            self.block = next(sb, self.block);
        }
        at
    }
}

/// Whether sequence `a` is after `b`, in sequences that wrap
/// (`tid_gt`).
pub fn tid_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
