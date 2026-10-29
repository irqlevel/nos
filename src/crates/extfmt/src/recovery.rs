//! What replaying a journal writes, worked out before anything is written:
//! JBD2's recovery (`do_one_pass`) as a plan -- which home block each copy
//! in the log goes to -- that a driver applies, or reads through without
//! writing anything on a read-only mount.
//!
//! The log is scanned from where the superblock says it starts: block after
//! block of the expected sequence, a transaction whole once its commit block
//! is found, the log ending at the first block that is not the next of it.
//! Then revokes: a home block revoked by a transaction is not replayed from
//! that transaction or any before it. Then the copies, oldest first, each
//! checked against its tag's checksum.
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

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::jbd2::{self, JournalSuper, Tag};

/// One copy to write: journal block `log` to home block `home`, its first
/// word put back to the magic if it was escaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    pub home: u64,
    pub log: u32,
    pub escaped: bool,
    pub sequence: u32,
}

/// What a replay writes, in order -- a later write of a home block is a
/// newer copy of it -- and what it found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub writes: Vec<Write>,
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

/// A transaction found in the scan.
struct Txn {
    sequence: u32,
    copies: Vec<(Tag, u32)>,
    revokes: Vec<u64>,
    /// A descriptor or revoke block of it failed its checksum.
    damaged: bool,
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
    let (first, last) = (sb.first(), sb.last());
    let ring = last - first;
    let next = |b: u32| if b + 1 >= last { first } else { b + 1 };

    let mut done: Vec<Txn> = Vec::new();
    let mut block = sb.start();
    let mut sequence = sb.sequence();
    let mut current = Txn { sequence, copies: Vec::new(), revokes: Vec::new(), damaged: false };
    let mut last_commit_time = 0u64;
    let mut failed_commit = None;
    let mut buf = vec![0u8; block_size];
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
                    current.damaged = true;
                }
                for tag in jbd2::tags(&buf, sb) {
                    block = next(block);
                    if scanned >= ring {
                        break 'scan;
                    }
                    scanned += 1;
                    current.copies.push((tag, block));
                }
            }
            jbd2::REVOKE_BLOCK => {
                if sb.csum_v2or3() && !jbd2::block_tail_ok(seed, &buf) {
                    current.damaged = true;
                } else {
                    match jbd2::revoke_records(&buf, sb) {
                        Ok(r) => current.revokes.extend(r),
                        Err(_) => current.damaged = true,
                    }
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
                if current.damaged {
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
                let whole = core::mem::replace(&mut current,
                    Txn { sequence, copies: Vec::new(), revokes: Vec::new(), damaged: false });
                done.push(whole);
            }
            _ => break,
        }
        block = next(block);
    }

    /* A block revoked by a transaction is not replayed from it or from any
     * transaction before it: the latest revoke of each block. */
    let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
    for t in &done {
        for &home in &t.revokes {
            revoked.insert(home, t.sequence);
        }
    }
    let revoked_at = |home: u64| revoked.get(&home).copied();

    let mut plan = Plan { transactions: done.len() as u32, next_sequence: sequence.wrapping_add(1), failed_commit,
                          ..Plan::default() };
    for t in &done {
        for &(tag, log) in &t.copies {
            if revoked_at(tag.block).is_some_and(|s| !tid_gt(t.sequence, s)) {
                plan.revoked += 1;
                continue;
            }
            read(log, &mut buf)?;
            if !jbd2::copy_ok(sb, seed, t.sequence, &tag, &buf) {
                return Err(alloc::format!("transaction {}: the copy of block {} at journal block {} fails its \
                                           checksum", t.sequence, tag.block, log));
            }
            plan.writes.push(Write { home: tag.block, log, escaped: tag.escaped(), sequence: t.sequence });
        }
    }
    Ok(plan)
}

/// Whether sequence `a` is after `b`, in sequences that wrap
/// (`tid_gt`).
pub fn tid_gt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}
