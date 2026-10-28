#!/usr/bin/env python3
"""extfmt test: the ext2/3/4 format crate held to what e2fsprogs makes.

extfmt (src/crates/extfmt) is the format hvboot reads a guest's /boot
through and the kernel's ext4 driver is to read and write through: views
over the superblock, group descriptors, inodes, extent trees, block maps,
directory and extended attribute blocks, and every checksum metadata_csum
-- and gdt_csum before it -- puts on them. fuzz/extfmt compiles it into a
host program, and this runs it over filesystems mke2fs makes here:

  - ext4 as mke2fs makes it by default (metadata_csum, 64bit, flex_bg,
    extents, 256-byte inodes) with 4 KiB blocks; with metadata_csum_seed and
    the UUID changed after it was made, so that the seed is not the UUID's;
    with 1 KiB blocks, 128-byte inodes and 32-byte descriptors (^64bit);
    with gdt_csum in place of metadata_csum; ext3; ext2;
  - each holding small and large files, a sparse file of two thousand
    pieces whose extent tree is two levels deep, directories of one block
    and of hundreds of entries -- indexed by e2fsck -D --, fast and slow
    symlinks, and extended attributes in the inode and in a block of their
    own;

and holds it to every checksum of every metadata block matching the one
extfmt computes; to agreeing with e2fsck -- bytes of checksummed metadata
changed one at a time, and extfmt and `e2fsck -fn` both say so; and to never
panicking and always ending over each image damaged at random.

    python3 scripts/extfmt-test.py                     # the gate
    python3 scripts/extfmt-test.py --iterations 20000  # a longer campaign

Needs cargo and e2fsprogs (mke2fs, e2fsck, debugfs, tune2fs) on the host --
Linux; on a Mac, in the nos-builder container:

    docker run --platform linux/amd64 --rm -v "$PWD":/src -w /src nos-builder \\
        python3 scripts/extfmt-test.py

Exit code 0 = every check passed.
"""

import argparse
import os
import random
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "extfmt")
OUT = os.path.join(ROOT, "out", "extfmt-test")
BIN = os.path.join(OUT, "release", "extfmt-test")

SIZE_MIB = 96

# What each image is made with, and the kinds of checksummed metadata it
# must have: every kind its features put a checksum on.
CSUM_KINDS = ["superblock", "descriptor", "block_bitmap", "inode_bitmap", "inode", "extent_block", "dir_block",
              "dx_block", "xattr_block"]
IMAGES = [
    ("ext4-4k", ["-t", "ext4", "-b", "4096"], CSUM_KINDS),
    ("ext4-seed", ["-t", "ext4", "-b", "4096", "-O", "metadata_csum_seed"], CSUM_KINDS),
    ("ext4-1k-128", ["-t", "ext4", "-b", "1024", "-I", "128", "-O", "^64bit"], CSUM_KINDS),
    ("ext4-gdtcsum", ["-t", "ext4", "-b", "4096", "-O", "^metadata_csum,uninit_bg"], ["descriptor"]),
    ("ext3", ["-t", "ext3", "-b", "4096"], []),
    ("ext2-1k", ["-t", "ext2", "-b", "1024"], []),
]

failures = []


def check(what, ok, detail=""):
    print("%s: %s" % ("PASS" if ok else "FAIL", what))
    if not ok:
        failures.append(what)
        if detail:
            print("  " + detail.strip().replace("\n", "\n  ")[-3000:])
    return ok


def run(argv, **kw):
    return subprocess.run(argv, capture_output=True, text=True, **kw)


def content(tmp, rng, block):
    """A tree to put on each image: files of every size the format maps
    differently, a sparse file of many pieces, directories small and large,
    symlinks fast and slow."""
    d = os.path.join(tmp, "content-%d" % block)
    if os.path.isdir(d):
        return d
    os.makedirs(os.path.join(d, "big"))
    os.makedirs(os.path.join(d, "a", "b", "c"))
    with open(os.path.join(d, "small.txt"), "wb") as f:
        f.write(b"a small file\n")
    with open(os.path.join(d, "medium.bin"), "wb") as f:
        f.write(rng.randbytes(100 << 10))
    with open(os.path.join(d, "large.bin"), "wb") as f:
        f.write(rng.randbytes(5 << 20))
    # A block of data every fourth block: mke2fs leaves the zeros between
    # as holes, so that each piece is an extent of its own -- two thousand
    # of them, which a tree of depth two holds at either block size.
    with open(os.path.join(d, "sparse.bin"), "wb") as f:
        for i in range(2000):
            f.seek(i * 4 * block)
            f.write(rng.randbytes(block))
    for i in range(400):
        with open(os.path.join(d, "big", "entry-with-a-longish-name-%04d" % i), "wb") as f:
            f.write(b"%d\n" % i)
    with open(os.path.join(d, "a", "b", "c", "deep.txt"), "wb") as f:
        f.write(b"deep\n")
    os.symlink("small.txt", os.path.join(d, "fast-link"))
    os.symlink("a/" * 40 + "nowhere", os.path.join(d, "slow-link"))
    return d


def make(tmp, name, opts, rng):
    """An image made by mke2fs from the tree, its directories indexed by
    e2fsck -D, extended attributes added by debugfs; and e2fsck's word that
    it is sound."""
    block = 1024 if "1024" in opts else 4096
    src = content(tmp, rng, block)
    img = os.path.join(tmp, name + ".img")
    with open(img, "wb") as f:
        f.truncate(SIZE_MIB << 20)
    r = run(["mke2fs", "-q", "-F"] + opts + ["-L", "extfmt", "-d", src, img])
    if not check("%s: mke2fs %s" % (name, " ".join(opts)), r.returncode == 0, r.stdout + r.stderr):
        return None
    if name == "ext4-seed":
        # metadata_csum_seed keeps the checksums' seed where it was when the
        # UUID changes: the seed is then no longer the UUID's.
        r = run(["tune2fs", "-U", "random", img])
        check("%s: tune2fs -U random" % name, r.returncode == 0, r.stdout + r.stderr)
    # -D indexes every directory of more than a block (dir_index), as Linux
    # would have as it grew. 1 means it changed something.
    r = run(["e2fsck", "-fyD", img])
    check("%s: e2fsck -fyD indexes the directories" % name, r.returncode in (0, 1), r.stdout + r.stderr)
    val = os.path.join(tmp, name + ".xattr")
    with open(val, "wb") as f:
        f.write(rng.randbytes(1500).hex()[:3000].encode())
    cmds = os.path.join(tmp, name + ".debugfs")
    with open(cmds, "w") as f:
        f.write("ea_set /small.txt user.small hello\nea_set -f %s /medium.bin user.big\n" % val)
    r = run(["debugfs", "-w", "-f", cmds, img])
    check("%s: debugfs ea_set, in the inode and in a block" % name, r.returncode == 0, r.stdout + r.stderr)
    r = run(["e2fsck", "-fn", img])
    check("%s: e2fsck -fn passes it" % name, r.returncode == 0, r.stdout + r.stderr)
    return img


def verify(img):
    r = run([BIN, "verify", img, "--regions"])
    counts, regions = {}, []
    for line in r.stdout.splitlines():
        w = line.split()
        if w[:1] == ["count"]:
            counts[w[1]] = int(w[2])
        elif w[:1] == ["region"]:
            regions.append((w[1], int(w[2]), int(w[3])))
    return r, counts, regions


def damage(tmp, name, img, regions, rng, trials):
    """Bytes the checksums cover changed one at a time -- each kind of
    metadata in turn -- and both extfmt and e2fsck held to saying so."""
    by_kind = {}
    for kind, offset, length in regions:
        if length > 0:
            by_kind.setdefault(kind, []).append((offset, length))
    with open(img, "rb") as f:
        base = f.read()
    bad = os.path.join(tmp, name + ".damaged")
    kinds = sorted(by_kind)
    missed = []
    for t in range(trials):
        kind = kinds[t % len(kinds)]
        offset, length = rng.choice(by_kind[kind])
        at = offset + rng.randrange(length)
        b = bytearray(base)
        b[at] ^= rng.randrange(1, 256)
        with open(bad, "wb") as f:
            f.write(b)
        ours = run([BIN, "verify", bad]).returncode
        r = run(["e2fsck", "-fn", bad])
        # e2fsck -n answers no to every fix it would make, but leaves some
        # (a descriptor's checksum, its fields sound) out of its exit code:
        # a question it asked is its saying so as much as the code is.
        theirs = r.returncode != 0 or any(line.rstrip().endswith("? no") for line in r.stdout.splitlines())
        if ours == 0 or not theirs:
            missed.append("%s byte %d: extfmt %d, e2fsck %d: %s" % (kind, at, ours, r.returncode,
                                                                   r.stdout.strip()[-300:]))
    check("%s: %d checksummed bytes changed, over %s: extfmt and e2fsck -fn both say so each time"
          % (name, trials, ", ".join(kinds)), not missed, "\n".join(missed[:20]))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--iterations", type=int, default=2000, help="damaged images walked per image")
    ap.add_argument("--trials", type=int, default=45, help="bytes changed one at a time per image")
    ap.add_argument("--keep", action="store_true", help="keep the images made")
    args = ap.parse_args()

    for t in ("cargo", "mke2fs", "e2fsck", "debugfs", "tune2fs"):
        if shutil.which(t) is None:
            sys.exit("extfmt-test needs %s (on a Mac, run it in the nos-builder container)" % t)
    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("extfmt-test: fuzz/extfmt did not build")

    r = run([BIN, "crc"])
    check("CRC32C and CRC16 against known answers", r.returncode == 0, r.stdout + r.stderr)

    rng = random.Random(args.seed)
    tmp = tempfile.mkdtemp(prefix="nos-extfmt-")
    try:
        for name, opts, kinds in IMAGES:
            img = make(tmp, name, opts, rng)
            if img is None:
                continue
            r, counts, regions = verify(img)
            check("%s: every checksum matches, every structure parses (%s)" % (name, ", ".join(
                  "%s %d" % kv for kv in sorted(counts.items()))), r.returncode == 0, r.stdout + r.stderr)
            missing = [k for k in kinds if counts.get(k, 0) == 0]
            check("%s: checksums of every kind its features put one on" % name, not missing,
                  "none of: " + ", ".join(missing))
            if kinds:
                damage(tmp, name, img, regions, rng, args.trials)
            r = run([BIN, "fuzz", img, "--seed", str(args.seed), "--iterations", str(args.iterations)],
                    timeout=3600)
            check("%s: %d images damaged at random walked: no panic, every walk ending (%s)"
                  % (name, args.iterations, r.stdout.strip()), r.returncode == 0, r.stdout + r.stderr)
    finally:
        if args.keep or failures:
            print("images in " + tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print()
    if failures:
        print("FAILED: " + ", ".join(failures))
        return 1
    print("extfmt-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
