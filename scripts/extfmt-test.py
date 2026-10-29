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

Then the journal. debugfs writes transactions into an image's journal and
leaves them unreplayed -- with v3's checksums and without, 64-bit tags and
32-bit: blocks written and written again, a block whose first word is the
journal's magic, revokes before and after a write, three hundred blocks at
once, one transaction left uncommitted -- and extfmt's recovery planner and
`e2fsck -E journal_only` each replay it: the two images must be the same
byte for byte but for the superblock fields e2fsck keeps for itself, and so
must they with the journal's ring turned so that its log wraps. A torn
commit must end the log where e2fsck ends it; a descriptor damaged under a
sound commit, and a copy that fails its checksum, must be refused; and the
planner, fuzzed over damaged journals, must never panic or plan a copy from
outside the ring.

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


# Journals: images whose journal debugfs wrote and nobody replayed -- with
# checksums (v3) and without, 64-bit tags and 32-bit -- each replayed by
# extfmt's planner and by e2fsck, which must leave the same image.
JOURNAL_IMAGES = [
    ("journal-4k-csum", ["-t", "ext4", "-b", "4096"], "-c"),
    ("journal-4k", ["-t", "ext4", "-b", "4096"], ""),
    ("journal-1k-32bit-csum", ["-t", "ext4", "-b", "1024", "-O", "^64bit"], "-c"),
    ("journal-1k-32bit", ["-t", "ext4", "-b", "1024", "-O", "^64bit"], ""),
]

# The superblock fields e2fsck's replay sets for itself -- the time it wrote
# the superblock, the time of its check, the kilobytes it counts written --
# and the checksum that covers them: the only bytes in which its image and
# extfmt's may differ. (The write time is the second e2fsck ran in: the same
# as debugfs's, mostly.)
SB_E2FSCK_OWN = [(0x30, 4), (0x40, 4), (0x178, 8), (0x3FC, 4)]


def masked(img):
    b = bytearray(img)
    for at, n in SB_E2FSCK_OWN:
        b[1024 + at:1024 + at + n] = bytes(n)
    return bytes(b)


def first_diff(a, b, block):
    """Where two images first differ: the block, and the byte and the two
    values there -- None where they do not."""
    for i in range(0, min(len(a), len(b)), block):
        if a[i:i + block] != b[i:i + block]:
            at = next(k for k in range(i, i + block) if a[k] != b[k])
            return "block %d, byte %#x: %#04x and %#04x" % (i // block, at, a[at], b[at])
    return None if len(a) == len(b) else "lengths %d and %d" % (len(a), len(b))


def journal_image(tmp, name, opts, csum, rng):
    """An image whose journal holds what one debugfs session wrote: a
    transaction of three blocks; one of a block whose first word is the
    journal's magic (escaped in the log); one that only revokes a block of
    the first; one writing a block of the first again; a revoke, and then
    the revoked block written again (replayed: the revoke covers only what
    came before it); three hundred blocks at once, more than a descriptor
    holds; and one left uncommitted."""
    block = 1024 if "1024" in opts else 4096
    img = os.path.join(tmp, name + ".img")
    with open(img, "wb") as f:
        f.truncate(64 << 20)
    r = run(["mke2fs", "-q", "-F"] + opts + [img])
    if not check("%s: mke2fs %s" % (name, " ".join(opts)), r.returncode == 0, r.stdout + r.stderr):
        return None, None, None
    # Home blocks in the filesystem's second half: past its metadata and
    # its journal, so that no replayed block is one the replay reads.
    base = (64 << 20) // block // 2
    files = {}
    for fname, n in (("d3", 3), ("d1", 1), ("d1b", 1), ("big", 300)):
        files[fname] = rng.randbytes(n * block)
    files["magic"] = bytes.fromhex("c03b3998") + rng.randbytes(block - 4)
    for fname, data in files.items():
        with open(os.path.join(tmp, "%s.%s" % (name, fname)), "wb") as f:
            f.write(data)
    p = lambda fname: os.path.join(tmp, "%s.%s" % (name, fname))
    b = lambda i: base + i
    cmds = "\n".join([
        "jo %s" % csum,
        "jw -b %d,%d,%d %s" % (b(0), b(1), b(2), p("d3")),
        "jw -b %d %s" % (b(10), p("magic")),
        "jw -r %d" % b(1),
        "jw -b %d %s" % (b(0), p("d1")),
        "jw -r %d" % b(20),
        "jw -b %d %s" % (b(20), p("d1b")),
        "jw -b %d-%d %s" % (b(100), b(399), p("big")),
        "jw -b %d -c %s" % (b(500), p("d1")),
        "jc", ""])
    path = os.path.join(tmp, name + ".debugfs")
    with open(path, "w") as f:
        f.write(cmds)
    r = run(["debugfs", "-w", "-f", path, img])
    ok = check("%s: debugfs writes eight transactions to the journal, one uncommitted" % name,
               r.returncode == 0 and "rror" not in r.stdout + r.stderr, r.stdout + r.stderr)
    # What a replay must leave at each home block: the newest copy of it
    # that no later revoke covers, the escaped block's magic restored.
    with open(img, "rb") as f:
        before = f.read()
    expect = {b(0): files["d1"], b(1): before[b(1) * block:(b(1) + 1) * block], b(2): files["d3"][2 * block:],
              b(10): files["magic"], b(20): files["d1b"], b(500): before[b(500) * block:(b(500) + 1) * block]}
    for i in range(300):
        expect[b(100 + i)] = files["big"][i * block:(i + 1) * block]
    return (img, block, expect) if ok else (None, None, None)


def replay_both(tmp, name, img, block):
    """The image replayed by extfmt's plan and by e2fsck: both images, and
    what each said."""
    ours = img + ".ours"
    r = run([BIN, "replay", img, ours])
    theirs = img + ".theirs"
    shutil.copyfile(img, theirs)
    e = run(["e2fsck", "-fy", "-E", "journal_only", theirs])
    a = open(ours, "rb").read() if r.returncode == 0 else None
    t = open(theirs, "rb").read()
    return r, e, a, t


def journal_tests(tmp, rng, args):
    for name, opts, csum in JOURNAL_IMAGES:
        img, block, expect = journal_image(tmp, name, opts, csum, rng)
        if img is None:
            continue
        r, e, ours, theirs = replay_both(tmp, name, img, block)
        check("%s: extfmt plans the journal: %s" % (name, r.stdout.splitlines()[0] if r.stdout else ""),
              r.returncode == 0 and "replayed: 7 transactions" in r.stdout, r.stdout + r.stderr)
        if ours is None:
            continue
        check("%s: e2fsck -E journal_only replays it" % name, e.returncode in (0, 1), e.stdout + e.stderr)
        d = first_diff(masked(ours), masked(theirs), block)
        check("%s: the image extfmt's replay leaves is e2fsck's, byte for byte but for e2fsck's own fields"
              % name, d is None, "first differing block %s" % d)
        wrong = [h for h, data in expect.items() if ours[h * block:(h + 1) * block] != data]
        check("%s: newest copies home, revoked ones not, the magic restored, the uncommitted left out" % name,
              not wrong, "blocks %s" % wrong[:10])
        r = run([BIN, "verify", img + ".ours"])
        check("%s: the replayed filesystem's checksums all match" % name, r.returncode == 0, r.stdout + r.stderr)

        # The same journal turned round its ring, its log wrapping.
        rot = img + ".rot"
        r = run([BIN, "rotate", img, rot, "5"])
        if check("%s: the ring turned, the log starting 5 blocks short of its end" % name, r.returncode == 0,
                 r.stdout + r.stderr):
            r, e, ours_r, theirs_r = replay_both(tmp, name, rot, block)
            d = first_diff(masked(ours_r), masked(theirs_r), block) if ours_r else "no replay"
            check("%s: the wrapping log replayed as e2fsck replays it" % name, d is None,
                  "first differing block %s: %s" % (d, r.stdout + r.stderr))

        if csum:
            damage_journal(tmp, name, img, block)
        r = run([BIN, "fuzz-journal", img, "--seed", str(args.seed), "--iterations", str(args.iterations)],
                timeout=3600)
        check("%s: %d damaged journals planned: no panic, nothing planned from outside the ring (%s)"
              % (name, args.iterations, r.stdout.strip()), r.returncode == 0, r.stdout + r.stderr)


def damage_journal(tmp, name, img, block):
    """A journal damaged where it matters, with v3's checksums: a torn
    commit ends the log before its transaction, as e2fsck ends it; a
    descriptor damaged under a sound commit, and a copy damaged, are
    refused."""
    r = run([BIN, "log", img])
    lines = r.stdout.splitlines()
    where = {int(l.split()[1]): int(l.split()[3]) for l in lines if l.startswith("map ")}
    blocks = [tuple(int(w) for w in l.split()[1::2]) for l in lines if l.startswith("block ")]
    commits = [b for b in blocks if b[1] == 2]
    descriptors = [b for b in blocks if b[1] == 1]
    with open(img, "rb") as f:
        base = f.read()

    def damaged(at, tag):
        b = bytearray(base)
        b[at] ^= 0x5A
        path = "%s.%s" % (img, tag)
        with open(path, "wb") as f:
            f.write(b)
        return path

    # The last commit torn: its transaction -- the three hundred blocks --
    # is not replayed, and the log ends where e2fsck ends it.
    last = max(commits, key=lambda c: c[2])
    path = damaged(last[3] + 100, "torn")
    r, e, ours, theirs = replay_both(tmp, name, path, block)
    d = first_diff(masked(ours), masked(theirs), block) if ours else "no replay"
    check("%s: a torn commit ends the log before its transaction, as e2fsck ends it (%s)"
          % (name, r.stdout.splitlines()[0] if r.stdout else ""),
          d is None and "replayed: 6 transactions" in r.stdout, "first differing block %s: %s" % (d, r.stdout))
    check("%s: and both say the transaction was corrupt" % name,
          "corrupt: transaction 7's commit" in r.stdout and "transaction 7 was corrupt" in e.stdout,
          r.stdout[:300] + "\n" + e.stdout[:500])

    # A descriptor damaged under its transaction's sound commit.
    desc = sorted(descriptors, key=lambda c: c[2])[1]
    path = damaged(desc[3] + 40, "descriptor")
    r = run([BIN, "replay", path, path + ".ours"])
    check("%s: a descriptor damaged under a sound commit is refused" % name,
          r.returncode == 3 and "REFUSED" in r.stdout, r.stdout + r.stderr)

    # A copy damaged: the first transaction's first copy, after its
    # descriptor.
    first = sorted(descriptors, key=lambda c: c[2])[0]
    path = damaged(where[first[0] + 1] + 7, "copy")
    r = run([BIN, "replay", path, path + ".ours"])
    check("%s: a copy that fails its tag's checksum is refused" % name,
          r.returncode == 3 and "REFUSED" in r.stdout, r.stdout + r.stderr)


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
    # A log the kernel's heap could not have listed whole: planned in maps a
    # node at a time, never one allocation past the 512 KiB it gives.
    r = run([BIN, "long-log"])
    check("a journal of 9000 transactions planned whole, the newest copy of each block, nothing asked of the heap "
          "past the kernel's 512 KiB at once (%s)" % r.stdout.strip(), r.returncode == 0, r.stdout + r.stderr)

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
        journal_tests(tmp, rng, args)
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
