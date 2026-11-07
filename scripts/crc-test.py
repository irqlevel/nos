#!/usr/bin/env python3
"""CRC test: the tree's CRCs (src/crates/common/crc), decided on the host.

One crate computes every CRC the tree's formats are checked by -- CRC-32
(GPT, nanofs, the disk log, the `crc32` command), CRC-32C (ext4's
metadata_csum and journal, raft's log) and CRC-16 (ext4's gdt_csum) -- for
the kernel, for hvboot in a guest and for the host's fuzzers alike. A CRC
computed wrong looks like a disk that is corrupt, or one that is not:
fuzz/crc compiles the crate, as it is, into a host program and holds it to

  - the known answers: each CRC's check value, CRC-32's of the quick brown
    fox, RFC 3720's CRC-32C vectors, CRC-16 from 0 (ARC) and from !0
    (MODBUS, as gdt_csum starts it), the references themselves held to the
    same values;
  - random messages -- of every length around the eight-byte steps the
    crate takes, at every alignment -- each CRC held to the CRC computed
    from its definition: the polynomial as it is written, divided into the
    message a bit at a time, sharing nothing with the crate's reflected
    tables; every form, from any register, whole and across a split, and a
    checksum with a hole against the block with its field zeroed;
  - CRC-32 against zlib's, over messages made here (up to past the 64 KiB
    the `crc32` command reads at a time), whole and fed in random pieces.

    python3 scripts/crc-test.py                          # the gate
    python3 scripts/crc-test.py --seed 7 --cases 1000000 # a campaign

Needs cargo on the host. Exit code 0 = every check passed.
"""

import argparse
import os
import random
import struct
import subprocess
import sys
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "crc")
OUT = os.path.join(ROOT, "out", "crc-test")
BIN = os.path.join(OUT, "release", "crc-test")


def zlib_cases(seed, path):
    """Messages for zlib to answer: the short ones, each length up to 64,
    then long ones around and past 64 KiB."""
    rnd = random.Random(seed)
    lengths = list(range(65)) + [4096, 65535, 65536, 65537, 131072 + 7]
    lengths += [rnd.randrange(200000) for _ in range(40)]
    with open(path, "wb") as f:
        for n in lengths:
            data = rnd.randbytes(n)
            f.write(struct.pack("<I", n) + data + struct.pack("<I", zlib.crc32(data)))
    return len(lengths)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--seed", type=int, default=1, help="where the random messages start (1)")
    p.add_argument("--cases", type=int, default=20000, help="random messages (20000)")
    args = p.parse_args()

    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("crc-test: fuzz/crc did not build")

    r = subprocess.run([BIN, "all", "--seed", str(args.seed), "--cases", str(args.cases)])
    if r.returncode != 0:
        sys.exit("crc-test: the known answers or the definitions failed")

    cases = os.path.join(OUT, "zlib.cases")
    zlib_cases(args.seed, cases)
    r = subprocess.run([BIN, "zlib", cases])
    if r.returncode != 0:
        sys.exit("crc-test: CRC-32 is not zlib's")
    print("crc-test: passed")


if __name__ == "__main__":
    main()
