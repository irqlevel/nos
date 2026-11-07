#!/usr/bin/env python3
"""Raft test: the raft crate (src/crates/common/cluster/raft), decided on the host.

The crate is the consensus the cluster's registry and NBS's metadata service
run on (plans/raft.md): a member of a group as a state machine with no I/O,
its messages as bytes, a storage in memory. fuzz/raft compiles it, as it is,
into a host program that holds it to:

  - known answers: every kind of message written and read back, the bytes
    of one as the format has them, and bytes that are not a message each
    refused for its reason; the storage's log across its pieces, written
    over and compacted; a snapshot's data read across its pieces; the
    configuration's checks; a lone voter that leads, commits and reads on its
    own; the membership change rules -- the 2015 one among them; a member
    added later making the group's membership of the log it is sent;
  - the known traps, each played out step by step: the paper's figure 8, a
    deposed leader asked for a read, a membership change before the leader's
    first commit (the 2015 erratum), a snapshot overtaken while it is being
    sent, a member back with a wiped disk under its old id -- which the checks
    must catch, and the harness refuses -- a vote not on the disk before it
    is answered, the election timer and a vote given, a follower's commit
    past what its append showed, a PreVote taken for a vote, and an append
    taken after an entry of another term;
  - the node target: groups of one to seven over a network that delays,
    loses, doubles, reorders and cuts, members crashing and coming back with
    what they had written, membership changed, leadership handed over,
    snapshots taken and sent -- held at every step to one leader a term, a
    new leader holding everything committed before it, logs that match,
    committed entries never changed and applied alike, reads never stale,
    elections only once a timer has run out, and progress once healed --
    disks that fail reads in some runs, each member stopping on the first,
    and every read asked of a leader answered once healed;
  - the storage target: MemStorage against a model of its log, over logs
    of tens of thousands of entries snapshotted in the middle;
  - the log target: raft::log's DiskLog on a disk of the fuzzer's own that
    keeps POSIX's promises and no more -- every write since a file's last
    sync kept, lost or torn at a power cut, a run of the directory's changes
    kept -- read back after each cut as the state at the last sync with some
    first records written after it, at a record's boundary, or as a snapshot
    that failed half way would have left it; disks that fail calls, the log
    stopping on the first; a byte of a sealed segment or of the snapshot
    changed and the log refusing to open; noise named as the log names its
    files, refused or read as nothing (RAFT_LOG_TRACE=1 prints each step
    and the disk at each cut);
  - the rsm target: a register through raft::rsm in groups of three or
    five on the log on disk over the crash disk, a network that fails,
    members crashed and started again, clients with sessions retrying after
    timeouts; every client answered once calm, and the history of every
    operation checked for linearizability (RAFT_RSM_STATS=1 says what each
    run did);
  - the registry target: the cluster's registry (src/crates/common/cluster/registry) held
    to a model of its own, alone and as a group through raft::rsm: every
    answer, every host, invitation and removed host, the voters and every
    host's peers; its snapshot read back, changed and refused, and a
    replica from a snapshot part way (RAFT_REGISTRY_STATS=1 says what each
    run did);
  - the wire target: messages written and read back, and noise and broken
    messages read without a panic, anything taken written back to its bytes;
  - the hostile target: one member fed messages no correct member sends --
    of every kind and value, from members and strangers, answers to what it
    sent true and false -- crashed, compacted and started over, never
    panicking, its term, its vote and its commit never going back, nothing
    handed out of order or unwritten, nothing sent the wire would refuse;

and every allocation of all of it held to the 512 KiB one block of the
kernel's heap gives (fuzz/raft/src/heap.rs).

    python3 scripts/raft-test.py                         # the gate
    python3 scripts/raft-test.py --seed 9 --seconds 600  # a node campaign
    python3 scripts/raft-test.py --mutants               # and the mutants

fuzz/raft/mutants.py takes one rule at a time out of the crate and must see
the gate fail, and the scenario written for that rule fail on its own.

Needs cargo on the host. Exit code 0 = every check held.
"""

import argparse
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "raft")
OUT = os.path.join(ROOT, "out", "raft-test")
BIN = os.path.join(OUT, "release", "raft-fuzz")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--seed", type=int, help="a node campaign from this seed, instead of the gate")
    p.add_argument("--seconds", type=int, default=600, help="the campaign's length (600)")
    p.add_argument("--mutants", action="store_true", help="the mutants too, after the gate")
    args = p.parse_args()

    # The manifest is outside src/crates, whose .cargo/config.toml would make
    # any build under it a kernel build; the crate has no dependency, so it
    # builds offline.
    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("raft-test: fuzz/raft did not build")

    if args.seed is not None:
        sys.exit(subprocess.run([BIN, "node", "--seed", str(args.seed), "--seconds", str(args.seconds)]).returncode)
    code = subprocess.run([BIN, "gate"]).returncode
    if code == 0 and args.mutants:
        code = subprocess.run([sys.executable, os.path.join(FUZZ, "mutants.py")]).returncode
    sys.exit(code)


if __name__ == "__main__":
    main()
