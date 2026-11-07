#!/usr/bin/env python3
"""NBS fuzz: NBS's crates on the host (fuzz/nbs, plans/nbs.md, "Tested"),
each held to a model of its own. The metadata's state machine, the `meta`
target (src/crates/common/storage/nbsmeta, N6):
  - random histories of every command -- members, devices, volumes made,
    grown and deleted, attaches and detaches, the leader's digests -- spoiled
    bytes among them, every answer held to a model written from the plan's
    rules, refusals in the crate's order;
  - where every chunk of every volume is: the model places it by the plan's
    rules on its own count of room, and the chunk's set must name the same
    three hosts' chunk servers; a volume refused for room must not fit by
    counting (its chunks fit three distinct hosts exactly when the hosts'
    free room, each counted up to the chunks, adds up to three times them);
  - every cs's reserved its replicas', every chunk counted by its maps, the
    free list every free chunk once -- by the fuzzer's own counting, not the
    crate's check;
  - its snapshot read back as itself, written in pieces of any size as in
    one; bytes spoiled refused or read as a state that keeps every rule
    through more commands;
  - a replica applying the same entries the same state with the same
    digests, one restored part way the same at the end; a checkpoint's digest
    the state's own, a false digest of the leader's said as a divergence;
and every allocation the crate makes held to the 512 KiB one block of the
kernel's heap gives (fuzz/raft's heap.rs, which the crate shares).

The chunk server's engine, the `cs` target (src/crates/common/storage/nbscs,
N7): fences, writes, reads, flushes, closes and frees over disks with a write
cache, the power cut within and between them -- each write not yet flushed
kept, lost or torn, a sector of the cs's own torn inside now and then, a bit
of a copy rotted -- and the cs opened again after every cut, held to every
fence and open mark answered, every chunk closed reading what it held, every
sector a value it may have, no record lost; a disk with something else on it
never taken for blank. NBS's messages, the `wire` target (nbswire): every
message read back as itself, and spoiled bytes read as none or as a message
that encodes to them. NBS end to end, the `cluster` target (nbsclient, N8):
the client's session through six hosts' cs's and mds, hosts that crash and
writers taken over, held to every write done on every replica in, every
read, what a crash may leave, the fences, and the replicas alike.

    python3 scripts/nbs-fuzz.py                              # the gate: every target
    python3 scripts/nbs-fuzz.py --seconds 600                # a meta campaign from seed 1
    python3 scripts/nbs-fuzz.py --target cs --seconds 600    # a cs campaign
    python3 scripts/nbs-fuzz.py --target cluster --seconds 600   # the client's
    python3 scripts/nbs-fuzz.py --mutants                    # and fuzz/nbs/mutants.py

fuzz/nbs/mutants.py takes one rule at a time out of the crate and must see the
gate fail. Needs cargo on the host. Exit code 0 = every check held.
"""

import argparse
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "nbs")
OUT = os.path.join(ROOT, "out", "nbs-fuzz")
VENDOR = os.path.join(ROOT, "src", "crates", "vendor")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--seed", type=int, default=1, help="the campaign's first seed (1)")
    p.add_argument("--seconds", type=int, help="a campaign this long, instead of the gate")
    p.add_argument("--target", choices=["meta", "cs", "wire", "cluster"], default="meta",
                   help="the campaign's target (meta)")
    p.add_argument("--mutants", action="store_true", help="the mutants too, after the gate")
    args = p.parse_args()

    # The manifest is outside src/crates, whose .cargo/config.toml would make
    # any build under it a kernel build; sha2, the digest's, is the kernel's
    # vendored copy, as net-fuzz takes it, so it builds offline.
    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT,
                        "--config", 'source.crates-io.replace-with="vendored-sources"',
                        "--config", 'source.vendored-sources.directory="%s"' % VENDOR], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("nbs-fuzz: fuzz/nbs did not build")
    binary = os.path.join(OUT, "release", "nbs-fuzz")
    if args.seconds is not None:
        run = [binary, args.target, "--seed", str(args.seed), "--seconds", str(args.seconds)]
    else:
        run = [binary, "gate"]
    code = subprocess.run(run, cwd=ROOT).returncode
    if code == 0 and args.mutants:
        code = subprocess.run([sys.executable, os.path.join(FUZZ, "mutants.py")], cwd=ROOT).returncode
    print("nbs-fuzz: " + ("passed" if code == 0 else "FAILED"))
    return code


if __name__ == "__main__":
    sys.exit(main())
