#!/usr/bin/env python3
"""Disk image converter test: what `hv convert` makes of a qcow2 image,
decided on the host.

`hv convert` (src/crates/modules/hv/src/image.rs) makes a cloud's qcow2 image
the raw disk a guest is given, through src/crates/diskimage -- a download,
read by code in the kernel. fuzz/diskimage compiles that crate, as it is,
into a host program, and this holds it to QEMU's own reading:

  - images qemu-img writes, from a disk made here: versions 2 and 3,
    clusters of 512 bytes to 2 MiB, stored and compressed, extended L2
    entries; and images qemu-io writes into -- zero clusters, discarded
    ones, writes inside a cluster, an internal snapshot and writes after
    it, metadata preallocated, an image left dirty -- each converted here
    and the disk held, byte for byte, to what qemu-img reads, its size the
    image's and its holes kept;
  - what it must refuse, and why, and that a refusal leaves no disk behind:
    an overlay on a backing file, encryption, zstd, an external data file,
    a corrupt mark, features it does not know, an image cut short, and a
    file that is no qcow2 (raw, ISO, gzip, VMDK, VHDX, VDI, VHD);
  - its DEFLATE decoder against zlib, which QEMU decodes compressed
    clusters with: streams zlib makes, the same broken, and streams written
    here block by block -- stored, fixed, dynamic, valid and not -- each
    asked for a number of bytes, and every answer, good or bad, and every
    byte, zlib's;
  - and each image with its header, tables and clusters changed at random:
    no panic, no read outside the image, no write outside the disk or out
    of order, every run ending; and the same, with only compressed data
    changed, converted by qemu-img too -- the two agreeing on every answer.

    python3 scripts/diskimage-test.py                      # the gate
    python3 scripts/diskimage-test.py --iterations 50000   # a longer campaign
    python3 scripts/diskimage-test.py --image ubuntu.img   # a distribution's

Needs cargo, qemu-img and qemu-io on the host. Exit code 0 = every check
passed.
"""

import argparse
import filecmp
import json
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "diskimage")
OUT = os.path.join(ROOT, "out", "diskimage-test")
BIN = os.path.join(OUT, "release", "diskimage-test")

MIB = 1024 * 1024
SECTOR = 512

failures = []


def check(what, ok, detail=""):
    print("%s: %s" % ("PASS" if ok else "FAIL", what))
    if not ok:
        failures.append(what)
        if detail:
            print("  " + detail.strip().replace("\n", "\n  ")[-2000:])
    return ok


def sh(argv, **kw):
    return subprocess.run(argv, capture_output=True, text=True, **kw)


def convert(image, disk):
    """diskimage-test convert: its exit code and what it said."""
    if os.path.exists(disk):
        os.unlink(disk)
    r = sh([BIN, "convert", image, disk], timeout=3600)
    return r.returncode, (r.stdout + r.stderr).strip()


def same_as_qemu(image, disk):
    """Whether qemu-img reads the image as the disk holds it: the same
    bytes, and the disk exactly the image's size."""
    info = sh(["qemu-img", "info", "--output=json", image])
    size = json.loads(info.stdout)["virtual-size"]
    if os.path.getsize(disk) != size:
        return False, "the disk is %d bytes, the image %d" % (os.path.getsize(disk), size)
    r = sh(["qemu-img", "compare", "-f", "qcow2", "-F", "raw", image, disk])
    return r.returncode == 0, r.stdout + r.stderr


def allocated(path):
    return os.stat(path).st_blocks * 512


# ---- a disk to make images of ----

def pattern(rng, kind, length):
    if kind == "random":
        return rng.randbytes(length)
    if kind == "text":
        words = [b"the", b"disk", b"of", b"a", b"guest", b"nos", b"cluster", b"image", b"hole", b"block", b"\n"]
        b = bytearray()
        while len(b) < length:
            b += rng.choice(words) + b" "
        return bytes(b[:length])
    if kind == "pattern":
        unit = rng.randbytes(rng.randint(1, 64))
        return (unit * (length // len(unit) + 1))[:length]
    return bytes(length)


def master(path, size, rng):
    """A raw disk of `size` bytes: zeros, with pieces of random, text and
    repeated bytes at random places -- some a byte, some across clusters --
    and its last sector written."""
    with open(path, "wb") as f:
        f.truncate(size)
        for _ in range(rng.randint(30, 60)):
            at = rng.randrange(0, size)
            length = min(rng.choice([1, 511, 512, 4095, 4096, 65536, 70000, rng.randrange(1, 400000)]), size - at)
            f.seek(at)
            f.write(pattern(rng, rng.choice(["random", "text", "pattern", "pattern", "zeros"]), length))
        f.seek(size - SECTOR)
        f.write(rng.randbytes(SECTOR))


# ---- the images ----

def from_master(tmp, raw, name, opts, compress):
    path = os.path.join(tmp, name + ".qcow2")
    argv = ["qemu-img", "convert", "-f", "raw", "-O", "qcow2", "-o", opts] + (["-c"] if compress else []) + [raw, path]
    r = sh(argv)
    return path if r.returncode == 0 else None


def qemu_io(path, *cmds):
    argv = ["qemu-io", "-f", "qcow2"]
    for c in cmds:
        argv += ["-c", c]
    r = sh(argv + [path])
    return r.returncode == 0, r.stdout + r.stderr


def written_images(tmp, rng):
    """Images qemu-io writes into: name, path, and what it shows."""
    out = []
    size = 24 * MIB + 3 * SECTOR

    p = os.path.join(tmp, "zeros-discards.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", p, str(size)])
    qemu_io(p, "write -P 0xab 0 3M", "write -z 1M 1M", "write -P 0x11 5M 333k", "discard 5M 64k",
            "write -z 8M 2M", "write -P 0x22 20M 100k", "write -P 0x33 %d 512" % (size - SECTOR))
    out.append(("zero clusters, discards, a write inside a cluster", p))

    p = os.path.join(tmp, "snapshot.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", p, str(size)])
    qemu_io(p, "write -P 0x44 0 2M", "write -P 0x45 10M 1M")
    sh(["qemu-img", "snapshot", "-c", "before", p])
    qemu_io(p, "write -P 0x46 512k 64k", "write -z 10M 512k", "write -P 0x47 12M 4k")
    out.append(("an internal snapshot, and writes after it", p))

    p = os.path.join(tmp, "prealloc-v3.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", "-o", "preallocation=metadata", p, str(size)])
    qemu_io(p, "write -P 0x55 3M 70k")
    out.append(("metadata preallocated, version 3", p))

    p = os.path.join(tmp, "prealloc-v2.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", "-o", "compat=0.10,preallocation=metadata", p, str(size)])
    qemu_io(p, "write -P 0x56 7M 9k")
    out.append(("metadata preallocated, version 2", p))

    p = os.path.join(tmp, "dirty.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", "-o", "lazy_refcounts=on", p, str(size)])
    qemu_io(p, "write -P 0x66 0 128k", "write -P 0x67 4M 64k", "abort")
    out.append(("left dirty, lazy refcounts", p))

    p = os.path.join(tmp, "extended-zeros.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", "-o", "extended_l2=on,cluster_size=64k", p, str(size)])
    qemu_io(p, "write -P 0x77 0 64k", "write -z 8k 4k", "write -P 0x78 70k 6k", "write -z 1M 64k",
            "discard 3k 2k")
    out.append(("extended L2: subclusters written, zeroed and discarded", p))
    return out


def gate_images(args, tmp):
    rng = random.Random(args.seed)
    size = 40 * MIB + 3 * SECTOR
    raw = os.path.join(tmp, "master.raw")
    master(raw, size, rng)
    # What a disk with its holes kept takes: its blocks that are not all zeros.
    with open(raw, "rb") as f:
        whole = f.read()
    zero_block = bytes(4096)
    data = sum(4096 for i in range(0, len(whole), 4096) if whole[i:i + 4096] != zero_block[:len(whole) - i])
    del whole

    made = []
    for compat in ("0.10", "1.1"):
        for cluster in ("512", "4k", "64k", "2M"):
            for compress in (False, True):
                name = "v%s-%s%s" % ("2" if compat == "0.10" else "3", cluster, "-c" if compress else "")
                path = from_master(tmp, raw, name, "compat=%s,cluster_size=%s" % (compat, cluster), compress)
                if check("qemu-img writes %s" % name, path is not None):
                    made.append((name, path))
    for cluster in ("16k", "64k", "2M"):
        for compress in (False, True):
            name = "v3-%s-extended%s" % (cluster, "-c" if compress else "")
            path = from_master(tmp, raw, name, "compat=1.1,extended_l2=on,cluster_size=%s" % cluster, compress)
            if path is not None:
                made.append((name, path))
            else:
                print("  (qemu-img writes no %s: not checked)" % name)

    for name, path in made:
        disk = path + ".raw"
        code, said = convert(path, disk)
        ok = code == 0 and os.path.getsize(disk) == size and filecmp.cmp(raw, disk, shallow=False)
        check("%s: converted, the disk byte for byte the one it was made from" % name, ok, said)
        if ok:
            # What the converter wrote is its own to answer for: no more than
            # the blocks that are not zeros. What the filesystem took for it
            # is the filesystem's -- APFS takes more than ext4 -- so only held
            # to being nowhere near the disk's whole size.
            written = int(said.split("written=")[1].split()[0])
            check("%s: and its holes kept -- %d KiB written, for %d KiB of blocks not zeros, %d KiB taken of %d"
                  % (name, written // 1024, data // 1024, allocated(disk) // 1024, size // 1024),
                  written <= data and allocated(disk) <= 2 * data + MIB, said)
        if os.path.exists(disk):
            os.unlink(disk)

    for what, path in written_images(tmp, rng):
        disk = path + ".raw"
        code, said = convert(path, disk)
        ok, detail = same_as_qemu(path, disk) if code == 0 else (False, said)
        check("%s: as qemu-img reads it" % what, ok, said + "\n" + detail)
        if "dirty" in path:
            check("and the dirty image said to be", " dirty" in said, said)
        if os.path.exists(disk):
            os.unlink(disk)
    return [p for _, p in made]


def patched(src, dst, at, data):
    shutil.copyfile(src, dst)
    with open(dst, "r+b") as f:
        f.seek(at)
        f.write(data)
    return dst


def refused(path, what, words):
    disk = path + ".raw"
    code, said = convert(path, disk)
    check("refused: %s -- %s" % (what, said.replace("refused: ", "")[:110]),
          code == 2 and all(w in said for w in words) and not os.path.exists(disk), said)


def gate_refusals(tmp, plain, compressed):
    p = os.path.join(tmp, "base.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", p, "8M"])
    overlay = os.path.join(tmp, "overlay.qcow2")
    sh(["qemu-img", "create", "-q", "-f", "qcow2", "-b", p, "-F", "qcow2", overlay])
    refused(overlay, "an overlay", ["backing file", "base.qcow2"])

    enc = os.path.join(tmp, "luks.qcow2")
    r = sh(["qemu-img", "create", "-q", "-f", "qcow2", "--object", "secret,id=sec0,data=nos",
            "-o", "encrypt.format=luks,encrypt.key-secret=sec0", enc, "8M"])
    if r.returncode == 0:
        refused(enc, "encrypted", ["encrypted", "LUKS"])
    else:
        print("  (qemu-img writes no LUKS image here: not checked)")

    zstd = os.path.join(tmp, "zstd.qcow2")
    r = sh(["qemu-img", "convert", "-f", "qcow2", "-O", "qcow2", "-c", "-o", "compression_type=zstd", plain, zstd])
    if r.returncode == 0:
        refused(zstd, "zstd", ["zstd"])
    else:
        print("  (qemu-img has no zstd here: not checked)")

    ext = os.path.join(tmp, "external.qcow2")
    r = sh(["qemu-img", "create", "-q", "-f", "qcow2", "-o", "data_file=" + os.path.join(tmp, "external.data"), ext,
            "8M"])
    if r.returncode == 0:
        refused(ext, "an external data file", ["external"])

    features = struct.unpack(">Q", open(plain, "rb").read(80)[72:80])[0]
    refused(patched(plain, os.path.join(tmp, "corrupt.qcow2"), 72, struct.pack(">Q", features | 2)),
            "marked corrupt", ["corrupt"])
    refused(patched(plain, os.path.join(tmp, "feature.qcow2"), 72, struct.pack(">Q", features | (1 << 20))),
            "a feature it does not know", ["features"])
    refused(patched(plain, os.path.join(tmp, "version4.qcow2"), 4, struct.pack(">I", 4)), "version 4", ["version 4"])
    refused(patched(plain, os.path.join(tmp, "cluster8.qcow2"), 20, struct.pack(">I", 8)), "clusters of 256 bytes",
            ["2^8"])
    refused(patched(plain, os.path.join(tmp, "crypt.qcow2"), 32, struct.pack(">I", 1)), "AES", ["AES"])

    # Cut short, as a download is: inside a compressed cluster's stream --
    # the longest's, a quarter in, where it is data and not the padding to
    # its last sector, which a cut may take with nothing lost -- halfway
    # through the clusters, and before the L2 table.
    ranges = sorted(compressed_ranges(compressed))
    longest, middle = max(ranges, key=lambda r: r[1]), ranges[len(ranges) // 2]
    for keep, what in ((longest[0] + longest[1] // 4, "most of a cluster's stream"),
                       (middle[0] + middle[1] // 2, "its second half"), (200 * 1024, "all but 200 KiB")):
        cut = os.path.join(tmp, "cut-%d.qcow2" % keep)
        shutil.copyfile(compressed, cut)
        os.truncate(cut, keep)
        refused(cut, "an image with %s cut off" % what, ["is the image whole"])

    raw = os.path.join(tmp, "mbr.raw")
    with open(raw, "wb") as f:
        f.write(bytes(510) + b"\x55\xaa" + bytes(4096))
    refused(raw, "a raw disk", ["raw disk"])
    iso = os.path.join(tmp, "cd.iso")
    with open(iso, "wb") as f:
        f.write(bytes(0x8001) + b"CD001" + bytes(4096))
    refused(iso, "an ISO", ["ISO 9660"])
    gz = os.path.join(tmp, "disk.gz")
    with open(gz, "wb") as f:
        f.write(zlib.compressobj(wbits=31).compress(bytes(4096)) + b"\0" * 64)
    refused(gz, "a gzip stream", ["gzip"])
    for fmt, words in (("vmdk", ["VMDK"]), ("vhdx", ["VHDX"]), ("vdi", ["VDI"]), ("vpc", ["VHD"])):
        other = os.path.join(tmp, "other." + fmt)
        if sh(["qemu-img", "convert", "-f", "qcow2", "-O", fmt, plain, other]).returncode == 0:
            refused(other, "a %s image" % fmt, words)

    # And a disk that is there already is not written over.
    there = os.path.join(tmp, "there.raw")
    with open(there, "wb") as f:
        f.write(b"keep me")
    r = sh([BIN, "convert", plain, there])
    check("a disk that is there already: not written over", r.returncode != 0 and open(there, "rb").read() == b"keep me",
          r.stdout + r.stderr)


# ---- DEFLATE, against zlib ----

class Bits:
    """DEFLATE's bits, least significant first; a Huffman code's most."""

    def __init__(self):
        self.out = bytearray()
        self.acc = 0
        self.n = 0

    def put(self, value, count):
        self.acc |= (value & ((1 << count) - 1)) << self.n
        self.n += count
        while self.n >= 8:
            self.out.append(self.acc & 0xFF)
            self.acc >>= 8
            self.n -= 8

    def code(self, code, length):
        self.put(int(format(code, "0%db" % length)[::-1], 2) if length else 0, length)

    def align(self):
        if self.n:
            self.put(0, 8 - self.n)

    def bytes(self):
        return bytes(self.out) + (bytes([self.acc & 0xFF]) if self.n else b"")


def canonical(lengths):
    """symbol -> (code, length), as DEFLATE assigns them."""
    count = [0] * 16
    for l in lengths:
        if l:
            count[l] += 1
    code, nxt = 0, [0] * 16
    for bits in range(1, 16):
        code = (code + count[bits - 1]) << 1
        nxt[bits] = code
    codes = {}
    for sym, l in enumerate(lengths):
        if l:
            codes[sym] = (nxt[l], l)
            nxt[l] += 1
    return codes


def complete_lengths(rng, symbols, max_len, k):
    """Lengths of a complete prefix code over `k` of `symbols`, none longer
    than `max_len`: leaves split at random until there are k of them."""
    k = min(k, len(symbols))
    depths = [0]
    while len(depths) < k:
        splittable = [i for i, d in enumerate(depths) if d < max_len]
        if not splittable:
            break
        i = rng.choice(splittable)
        d = depths.pop(i)
        depths += [d + 1, d + 1]
    if depths == [0]:
        depths = [1]
    lengths = [0] * len(symbols)
    for pos, d in zip(rng.sample(range(len(symbols)), len(depths)), depths):
        lengths[pos] = d
    return lengths


LEN_BASE = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163,
            195, 227, 258]
LEN_EXTRA = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0]
DIST_BASE = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073,
             4097, 6145, 8193, 12289, 16385, 24577]
DIST_EXTRA = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13]
CODELEN_ORDER = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15]
FIXED = [8] * 144 + [9] * 112 + [7] * 24 + [8] * 8


def symbols(rng, w, lit, dist, produced, faults):
    """Literals and matches in the codes given, ended by the end of the
    block -- sometimes a match reaching too far, or a fixed code's symbols
    no stream may hold. How many bytes they stand for."""
    lits = [s for s in lit if s < 256]
    lens = [s for s in lit if 257 <= s <= 285 or (faults and s > 285)]
    for _ in range(rng.choice([0, 1, 5, 40, 300])):
        if lens and dist and rng.random() < 0.4:
            s = rng.choice(lens)
            i = s - 257
            if i >= len(LEN_BASE):
                w.code(*lit[s])
                return produced
            extra = rng.randrange(1 << LEN_EXTRA[i]) if LEN_EXTRA[i] else 0
            reach = [d for d in dist if (d < 30 and DIST_BASE[d] <= produced) or (faults and rng.random() < 0.02)]
            if not reach:
                continue
            d = rng.choice(reach)
            w.code(*lit[s])
            w.put(extra, LEN_EXTRA[i])
            w.code(*dist[d])
            if d >= 30:
                return produced
            dx = rng.randrange(1 << DIST_EXTRA[d]) if DIST_EXTRA[d] else 0
            w.put(dx, DIST_EXTRA[d])
            produced += LEN_BASE[i] + extra
        elif lits:
            w.code(*lit[rng.choice(lits)])
            produced += 1
    if 256 in lit:
        w.code(*lit[256])
    return produced


def dynamic(rng, w, produced, faults):
    nlen = rng.randint(257, 288 if faults and rng.random() < 0.05 else 286)
    ndist = rng.randint(1, 32 if faults and rng.random() < 0.05 else 30)
    lit_k = rng.choice([1, 2, 3, 20, 100, nlen])
    lit = complete_lengths(rng, range(nlen), 15, lit_k)
    if not lit[256]:
        # The end of the block wanted: swapped in from a symbol that has a code.
        j = next(i for i, l in enumerate(lit) if l)
        lit[256], lit[j] = lit[j], 0
    if faults and rng.random() < 0.05:
        lit[256] = 0
    dist = complete_lengths(rng, range(ndist), 15, rng.choice([1, 2, 5, ndist])) if rng.random() < 0.9 else [0] * ndist
    if faults and rng.random() < 0.1:
        target = rng.choice([lit, dist])
        for _ in range(rng.randint(1, 4)):
            target[rng.randrange(len(target))] = rng.randint(0, 15)

    lengths = lit + dist
    # The lengths run-length coded, the way a compressor would -- and
    # sometimes a repeat with nothing before it, or past the end.
    runs, i = [], 0
    while i < len(lengths):
        l, n = lengths[i], 1
        while i + n < len(lengths) and lengths[i + n] == l:
            n += 1
        if l == 0 and n >= 11:
            take = min(n, 138)
            runs.append((18, take - 11, 7))
        elif l == 0 and n >= 3:
            take = min(n, 10)
            runs.append((17, take - 3, 3))
        elif i > 0 and n >= 3 and lengths[i - 1] == l:
            take = min(n, 6)
            runs.append((16, take - 3, 2))
        else:
            take = 1
            runs.append((l, 0, 0))
        i += take
    if faults and rng.random() < 0.05:
        runs.insert(0, (16, rng.randrange(4), 2))
    if faults and rng.random() < 0.05:
        runs.append((18, 127, 7))
    used = sorted(set(s for s, _, _ in runs))
    cl_symbols = used if len(used) >= 2 else used + [s for s in (0, 8) if s not in used][:1]
    cl_len = [0] * 19
    for s, l in zip(cl_symbols, complete_lengths(rng, cl_symbols, 7, len(cl_symbols))):
        cl_len[s] = l
    if faults and rng.random() < 0.08:
        cl_len[rng.randrange(19)] = rng.randint(0, 7)
    ncode = max([4] + [CODELEN_ORDER.index(s) + 1 for s in range(19) if cl_len[s]])

    w.put(2, 2)
    w.put(nlen - 257, 5)
    w.put(ndist - 1, 5)
    w.put(ncode - 4, 4)
    for s in CODELEN_ORDER[:ncode]:
        w.put(cl_len[s], 3)
    cl = canonical(cl_len)
    for s, extra, bits in runs:
        if s not in cl:
            w.put(rng.getrandbits(8), 8)
            return produced
        w.code(*cl[s])
        w.put(extra, bits)
    return symbols(rng, w, canonical(lit), canonical(dist), produced, faults)


def stream(rng, faults):
    """A DEFLATE stream written block by block, and how much it holds."""
    w = Bits()
    produced = 0
    blocks = rng.randint(1, 4)
    for b in range(blocks):
        last = b == blocks - 1 and not (faults and rng.random() < 0.05)
        w.put(1 if last else 0, 1)
        kind = rng.choice([0, 1, 1, 2, 2, 2] + ([3] if faults else []))
        if kind == 0:
            w.put(0, 2)
            w.align()
            n = rng.choice([0, 1, 100, 3000])
            w.put(n, 16)
            w.put(n ^ 0xFFFF if not (faults and rng.random() < 0.1) else rng.getrandbits(16), 16)
            for byte in rng.randbytes(n):
                w.put(byte, 8)
            produced += n
        elif kind == 1:
            w.put(1, 2)
            produced = symbols(rng, w, canonical(FIXED), canonical([5] * 32), produced, faults)
        elif kind == 2:
            produced = dynamic(rng, w, produced, faults)
        else:
            w.put(3, 2)
    data = w.bytes()
    if faults and rng.random() < 0.2:
        data = data[:rng.randrange(len(data) + 1)]
    return data, produced


def header(w, nlen, ndist, cl_len, runs, rng):
    """A dynamic block's header, written as given -- right or not: its
    counts, its code length code's lengths and the lengths run-length coded
    with that code, a run the code has no code for written as noise."""
    w.put(2, 2)
    w.put(nlen - 257, 5)
    w.put(ndist - 1, 5)
    ncode = max([4] + [CODELEN_ORDER.index(s) + 1 for s in range(19) if cl_len[s]])
    w.put(ncode - 4, 4)
    for s in CODELEN_ORDER[:ncode]:
        w.put(cl_len[s], 3)
    cl = canonical(cl_len)
    for s, extra, bits in runs:
        if s not in cl:
            w.put(rng.getrandbits(16), 16)
            return
        w.code(*cl[s])
        w.put(extra, bits)


def plain_runs(lengths):
    """Lengths as code length symbols, one a length."""
    return [(l, 0, 0) for l in lengths]


def edge_cases(rng):
    """A stream for each rule zlib holds a dynamic block's header and codes
    to, and for the faults a block's symbols can have -- each after a stored
    block of ten bytes, so that asked for those ten it is read "dry", as
    zlib reads on past a full buffer. Each is then tried cut at every byte,
    which is where what a rule needs of the input before it fails shows."""
    # A complete code length code over the lengths the cases use and the
    # repeats: 2/4 + 3/8 + 2/16 = 1.
    full_cl = [0] * 19
    for s, l in ((0, 2), (8, 2), (1, 3), (2, 3), (18, 3), (16, 4), (17, 4)):
        full_cl[s] = l
    # A complete literal/length code of 'A', the end of block and length 3.
    lit_a_eob = [0] * 286
    lit_a_eob[65], lit_a_eob[256], lit_a_eob[257] = 2, 2, 1
    a_eob = canonical(lit_a_eob)
    cases = {}

    def dyn(name, nlen, ndist, cl_len, lengths=None, runs=None, body=None):
        w = Bits()
        w.put(1, 1)
        header(w, nlen, ndist, cl_len, runs if runs is not None else plain_runs(lengths), rng)
        if body:
            body(w)
        cases[name] = w

    single = [0] * 19
    single[8] = 1
    dyn("code length code: a single code of one bit", 257, 1, single, [8] * 258)
    dyn("code length code: none", 257, 1, [0] * 19, runs=[(0, 0, 0)] * 300)
    over = [0] * 19
    over[0] = over[8] = over[1] = 1
    dyn("code length code: over-subscribed", 257, 1, over, [8] * 258)
    inc = [0] * 19
    inc[0], inc[8] = 1, 2
    dyn("code length code: incomplete", 257, 1, inc, [8] * 258)
    dyn("a repeat with no length before it", 257, 1, full_cl, runs=[(16, 1, 2)] + plain_runs([8] * 258))
    dyn("a repeat past the end", 257, 1, full_cl, runs=plain_runs([8] * 255) + [(18, 127, 7)])
    no_eob = [8] * 256 + [0] + [1]
    dyn("no end of block", 257, 1, full_cl, no_eob)
    dyn("literal/length code: over-subscribed", 257, 1, full_cl, [8] * 257 + [1])
    dyn("literal/length code: incomplete", 257, 1, full_cl, [8] * 200 + [0] * 56 + [8] + [1])
    eob_only = [0] * 256 + [1]
    dyn("literal/length code: a single code of one bit, the end of block", 257, 1, full_cl, eob_only + [1],
        body=lambda w: w.put(0, 1))
    dyn("literal/length code: a single code of one bit, its other bit", 257, 1, full_cl, eob_only + [1],
        body=lambda w: w.put(1, 1))
    dyn("distance code: none, and literals", 286, 1, full_cl, lit_a_eob + [0],
        body=lambda w: [w.code(*a_eob[65]) for _ in range(5)] + [w.code(*a_eob[256])])
    dyn("distance code: none, and a length", 286, 1, full_cl, lit_a_eob + [0],
        body=lambda w: [w.code(*a_eob[65]), w.code(*a_eob[257]), w.put(rng.getrandbits(8), 8)])
    dyn("distance code: a single code of one bit, used and then its other bit", 286, 1, full_cl, lit_a_eob + [1],
        body=lambda w: [w.code(*a_eob[65]), w.code(*a_eob[257]), w.put(0, 1), w.code(*a_eob[257]), w.put(1, 1)])
    dyn("distance code: incomplete", 286, 2, full_cl, lit_a_eob + [1, 2])
    dyn("too many literal/length codes", 287, 1, full_cl, [8] * 288)
    dyn("too many distance codes", 257, 31, full_cl, [8] * 288)

    fixed = canonical(FIXED)
    dist5 = canonical([5] * 32)

    def fixed_block(name, symbols):
        w = Bits()
        w.put(1, 1)
        w.put(1, 2)
        symbols(w)
        cases[name] = w

    fixed_block("a fixed block's literal/length 286", lambda w: [w.code(*fixed[65]), w.code(*fixed[286])])
    fixed_block("a fixed block's distance 30", lambda w: [w.code(*fixed[65]), w.code(*fixed[257]), w.code(*dist5[30])])
    # After the ten bytes before the block and its 'A', eleven are out: a
    # distance of 13 reaches past the first, one of 11 to it.
    fixed_block("a match too far back", lambda w: [w.code(*fixed[65]), w.code(*fixed[257]), w.code(*dist5[7]),
                                                   w.put(0, 2), w.code(*fixed[256])])
    fixed_block("a match just far enough back", lambda w: [w.code(*fixed[65]), w.code(*fixed[257]),
                                                           w.code(*dist5[6]), w.put(2, 2), w.code(*fixed[256])])
    w = Bits()
    w.put(1, 1)
    w.put(0, 2)
    w.align()
    w.put(5, 16)
    w.put(5, 16)
    cases["a stored block's lengths that disagree"] = w
    w = Bits()
    w.put(1, 1)
    w.put(3, 2)
    cases["block type 3"] = w

    out = []
    for name, w in cases.items():
        prefix = Bits()
        prefix.put(0, 1)
        prefix.put(0, 2)
        prefix.align()
        prefix.put(10, 16)
        prefix.put(10 ^ 0xFFFF, 16)
        lead = prefix.bytes() + rng.randbytes(10)
        body = w.bytes()
        data = lead + body
        for cut in range(len(lead), len(data) + 1):
            out.append((data[:cut], 10))
            out.append((data[:cut], 11))
        out.append((data, 10 + rng.randint(2, 300)))
    return out


def zlib_answer(data, limit):
    """What zlib says of the stream asked for `limit` bytes, as QEMU asks:
    good with these bytes, or not."""
    try:
        out = zlib.decompressobj(-15).decompress(data, limit)
    except zlib.error:
        return "fault"
    return "ok %08x" % zlib.crc32(out) if len(out) == limit else "short"


def gate_inflate(args, tmp):
    rng = random.Random(args.seed * 7919 + 1)
    cases = []

    def add(data, limit):
        if limit > 0:
            cases.append((data, limit))

    # zlib's own, every level and strategy, asked for what they hold, less and more.
    strategies = [zlib.Z_DEFAULT_STRATEGY, zlib.Z_FILTERED, zlib.Z_HUFFMAN_ONLY, zlib.Z_RLE, zlib.Z_FIXED]
    for _ in range(max(400, args.iterations // 4)):
        n = rng.choice([1, 10, 300, 4096, 65536, 70000, rng.randrange(1, 140000)])
        raw = b"".join(pattern(rng, rng.choice(["random", "text", "pattern", "zeros"]), max(1, n // 4))
                       for _ in range(4))
        c = zlib.compressobj(rng.randint(0, 9), zlib.DEFLATED, -15, rng.randint(1, 9), rng.choice(strategies))
        cut = rng.randrange(len(raw) + 1)
        data = c.compress(raw[:cut]) + c.flush(rng.choice([zlib.Z_SYNC_FLUSH, zlib.Z_FULL_FLUSH, zlib.Z_NO_FLUSH]))
        data += c.compress(raw[cut:]) + c.flush(zlib.Z_FINISH)
        for limit in (len(raw), rng.randint(1, len(raw)), len(raw) + rng.randint(1, 100)):
            add(data, limit)
        # The same, broken: a bit, a byte, a cut, bytes after.
        broken = bytearray(data)
        how = rng.randrange(4)
        if how == 0 and broken:
            i = rng.randrange(len(broken))
            broken[i] ^= 1 << rng.randrange(8)
        elif how == 1 and broken:
            broken[rng.randrange(len(broken))] = rng.getrandbits(8)
        elif how == 2:
            broken = broken[:rng.randrange(len(broken) + 1)]
        else:
            broken += rng.randbytes(rng.randint(1, 20))
        add(bytes(broken), rng.choice([len(raw), rng.randint(1, len(raw))]))
    # Written here, block by block, good and broken.
    for _ in range(max(1000, args.iterations)):
        data, produced = stream(rng, faults=rng.random() < 0.5)
        add(data, rng.choice([produced, produced, rng.randint(1, max(1, produced)), produced + rng.randint(1, 9),
                              rng.randint(1, 70000)]))
    # Each rule's own stream, cut at every byte.
    for data, limit in edge_cases(rng):
        add(data, limit)
    # And noise.
    for _ in range(300):
        add(rng.randbytes(rng.randint(0, 300)), rng.randint(1, 70000))

    path = os.path.join(tmp, "inflate.cases")
    with open(path, "wb") as f:
        for data, limit in cases:
            f.write(struct.pack("<II", limit, len(data)) + data)
    answers = os.path.join(tmp, "inflate.answers")
    r = sh([BIN, "inflate", path, answers], timeout=3600)
    if not check("%d streams decoded" % len(cases), r.returncode == 0, r.stdout + r.stderr):
        return
    ours = open(answers).read().splitlines()
    wrong = []
    kinds = {"ok": 0, "fault": 0}
    for i, ((data, limit), mine) in enumerate(zip(cases, ours)):
        theirs = zlib_answer(data, limit)
        kinds["ok" if theirs.startswith("ok") else "fault"] += 1
        if theirs.startswith("ok") != mine.startswith("ok") or (theirs.startswith("ok") and theirs != mine):
            wrong.append((i, theirs, mine))
    if wrong:
        i, theirs, mine = wrong[0]
        keep = os.path.join(OUT, "inflate-mismatch.bin")
        with open(keep, "wb") as f:
            f.write(cases[i][0])
        detail = "case %d (limit %d, kept in %s): zlib %s, here %s" % (i, cases[i][1], keep, theirs, mine)
    else:
        detail = ""
    check("every stream answered as zlib answers it -- %d good, %d not -- and every good one's bytes zlib's"
          % (kinds["ok"], kinds["fault"]), not wrong and len(ours) == len(cases), detail)


# ---- the seed: an ISO 9660 image of a few files ----

def read_back(iso, into):
    """The files of an ISO by the names a Rock Ridge reader gives them --
    xorriso's or libarchive's, whichever the host has: name -> bytes."""
    os.makedirs(into, exist_ok=True)
    if shutil.which("xorriso"):
        r = sh(["xorriso", "-osirrox", "on", "-indev", iso, "-extract", "/", into])
    elif shutil.which("bsdtar"):
        r = sh(["bsdtar", "-xf", iso, "-C", into])
    else:
        return None, "no xorriso or bsdtar to read it with"
    # The image's root is read-only, and an extraction takes its mode along.
    os.chmod(into, 0o755)
    if r.returncode != 0:
        return None, r.stdout + r.stderr
    got = {}
    for name in os.listdir(into):
        with open(os.path.join(into, name), "rb") as f:
            got[name] = f.read()
    return got, ""


def gate_iso(tmp):
    rng = random.Random(5)
    cases = {
        "a cloud-init seed": {"meta-data": b"instance-id: nos-noble-1\nlocal-hostname: noble\n",
                              "user-data": b"#cloud-config\nhostname: noble\nssh_authorized_keys:\n"
                                           b"  - ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test\n"},
        "an empty file, and names alike in 8.3": {"user-data": b"", "user_data": b"x", "user.data": b"y" * 3000,
                                                    "user-dat": rng.randbytes(5000)},
        "eight files, one of 1 MiB less the rest": dict(
            [("f%d-%s" % (i, "n" * 50), rng.randbytes(rng.randint(1, 9000))) for i in range(7)]),
    }
    big = cases["eight files, one of 1 MiB less the rest"]
    big["the-last.one"] = rng.randbytes((1 << 20) - sum(len(v) for v in big.values()))
    have_blkid = shutil.which("blkid") is not None
    mount = sys.platform.startswith("linux") and (os.geteuid() == 0 or sh(["sudo", "-n", "true"]).returncode == 0)
    for what, files in cases.items():
        d = os.path.join(tmp, "iso-" + str(len(files)))
        os.makedirs(d, exist_ok=True)
        specs = []
        for i, (name, data) in enumerate(files.items()):
            p = os.path.join(d, "in%d" % i)
            with open(p, "wb") as f:
                f.write(data)
            specs.append("%s=%s" % (name, p))
        iso = os.path.join(d, "seed.iso")
        r = sh([BIN, "iso", iso, "cidata"] + specs)
        if not check("an ISO made: %s" % what, r.returncode == 0, r.stdout + r.stderr):
            continue
        got, why = read_back(iso, os.path.join(d, "out"))
        check("  read back by Rock Ridge's names, every byte its own", got == files,
              why or "got %s" % sorted(got or {}))
        with open(iso, "rb") as f:
            pvd = f.read(17 * 2048)[16 * 2048:]
        check("  labelled cidata, a volume of ISO 9660",
              pvd[1:6] == b"CD001" and pvd[40:72].rstrip(b" ") == b"cidata", repr(pvd[:72]))
        if have_blkid:
            b = sh(["blkid", "-o", "export", iso]).stdout
            check("  as blkid sees it -- what cloud-init looks for", "LABEL=cidata" in b and "TYPE=iso9660" in b, b)
        if mount:
            m = os.path.join(d, "mnt")
            os.makedirs(m, exist_ok=True)
            pre = [] if os.geteuid() == 0 else ["sudo", "-n"]
            r = sh(pre + ["mount", "-o", "loop,ro", "-t", "iso9660", iso, m])
            if check("  mounted by Linux's isofs", r.returncode == 0, r.stdout + r.stderr):
                seen = {}
                for name in os.listdir(m):
                    with open(os.path.join(m, name), "rb") as f:
                        seen[name] = f.read()
                sh(pre + ["umount", m])
                check("  which reads every file by its name", seen == files, "got %s" % sorted(seen))
    for what, label, specs, word in (
            ("a label of a space", "ci data", ["a=" + BIN], "label"),
            ("a name with a slash", "cidata", ["a/b=" + BIN], "no name"),
            ("two files of one name", "cidata", ["a=" + BIN, "a=" + BIN], "no name"),
            ("nine files", "cidata", ["f%d=%s" % (i, BIN) for i in range(9)], "more than 8"),
    ):
        r = sh([BIN, "iso", os.path.join(tmp, "refused.iso"), label] + specs)
        check("an ISO refused: %s" % what, r.returncode == 2 and word in r.stdout, r.stdout + r.stderr)
    big_file = os.path.join(tmp, "big.bin")
    with open(big_file, "wb") as f:
        f.write(bytes((1 << 20) + 1))
    r = sh([BIN, "iso", os.path.join(tmp, "refused.iso"), "cidata", "big=" + big_file])
    check("an ISO refused: more than 1 MiB of files", r.returncode == 2 and "bytes of files" in r.stdout, r.stdout)


# ---- fuzzing ----

def gate_fuzz(args, images):
    for path in images:
        r = sh([BIN, "fuzz", path, "--seed", str(args.seed), "--iterations", str(args.iterations)], timeout=7200)
        check("%s: %d images of its metadata changed at random: no panic, no read or write out of bounds, every "
              "run ending" % (os.path.basename(path), args.iterations), r.returncode == 0, r.stdout + r.stderr)
        print("  " + r.stdout.strip().splitlines()[-1] if r.stdout.strip() else "")


def compressed_ranges(path):
    """Where an image's compressed clusters' data is: (offset, length)."""
    b = open(path, "rb").read()
    cluster_bits = struct.unpack(">I", b[20:24])[0]
    l1_size, l1_off = struct.unpack(">IQ", b[36:48])
    shift = 62 - (cluster_bits - 8)
    out = []
    for i in range(l1_size):
        l2 = struct.unpack(">Q", b[l1_off + 8 * i:l1_off + 8 * i + 8])[0] & 0x00FFFFFFFFFFFE00
        if not l2:
            continue
        for j in range((1 << cluster_bits) // 8):
            e = struct.unpack(">Q", b[l2 + 8 * j:l2 + 8 * j + 8])[0]
            if e & (1 << 62):
                off = e & ((1 << shift) - 1)
                sectors = ((e >> shift) & ((1 << (cluster_bits - 8)) - 1)) + 1
                out.append((off, min(sectors * 512 - off % 512, len(b) - off)))
    return out


def gate_against_qemu(args, tmp, images):
    """Compressed data changed at random, converted here and by qemu-img:
    the same answer, and where both convert it the same disk."""
    rng = random.Random(args.seed * 31 + 7)
    agreed = both = 0
    wrong = []
    rounds = max(40, args.iterations // 50)
    for path in images:
        ranges = compressed_ranges(path)
        if not ranges:
            continue
        for n in range(rounds):
            bad = os.path.join(tmp, "changed.qcow2")
            shutil.copyfile(path, bad)
            with open(bad, "r+b") as f:
                for _ in range(rng.randint(1, 3)):
                    off, length = rng.choice(ranges)
                    f.seek(off + rng.randrange(max(1, length)))
                    f.write(bytes([rng.getrandbits(8)]))
            mine = bad + ".raw"
            theirs = bad + ".ref"
            code, said = convert(bad, mine)
            if os.path.exists(theirs):
                os.unlink(theirs)
            q = sh(["qemu-img", "convert", "-f", "qcow2", "-O", "raw", bad, theirs])
            if (code == 0) != (q.returncode == 0):
                wrong.append("%s round %d: here %s, qemu-img %s" % (os.path.basename(path), n, said,
                             (q.stdout + q.stderr).strip() or "converted it"))
            elif code == 0:
                both += 1
                if not filecmp.cmp(mine, theirs, shallow=False):
                    wrong.append("%s round %d: both converted it, to different disks" % (os.path.basename(path), n))
                else:
                    agreed += 1
            else:
                agreed += 1
            for p in (mine, theirs):
                if os.path.exists(p):
                    os.unlink(p)
    check("compressed data changed at random: here and in qemu-img, the same answer every time (%d, %d of them "
          "converted by both, to the same disk)" % (agreed, both), not wrong, "\n".join(wrong[:5]))


def image(args):
    for path in args.image:
        disk = os.path.join(tempfile.mkdtemp(prefix="nos-diskimage-"), os.path.basename(path) + ".raw")
        code, said = convert(path, disk)
        print("  " + said)
        ok, detail = same_as_qemu(path, disk) if code == 0 else (False, said)
        check("%s: converted, as qemu-img reads it" % os.path.basename(path), ok, detail)
        if code == 0:
            print("  (%d MiB of the disk's %d taken)" % (allocated(disk) // MIB, os.path.getsize(disk) // MIB))
        shutil.rmtree(os.path.dirname(disk), ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--image", action="append", default=[], help="a qcow2 image to convert and hold to qemu-img")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--iterations", type=int, default=2000, help="fuzzed images per image, and streams")
    ap.add_argument("--keep", action="store_true", help="keep the images made")
    args = ap.parse_args()

    for t in ("cargo", "qemu-img", "qemu-io"):
        if shutil.which(t) is None:
            sys.exit("diskimage-test needs %s" % t)
    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("diskimage-test: fuzz/diskimage did not build")

    if args.image:
        image(args)
    else:
        tmp = tempfile.mkdtemp(prefix="nos-diskimage-")
        try:
            made = gate_images(args, tmp)
            plain = next(p for p in made if p.endswith("v3-64k.qcow2"))
            compressed = next(p for p in made if p.endswith("v3-64k-c.qcow2"))
            gate_refusals(tmp, plain, compressed)
            gate_iso(tmp)
            gate_inflate(args, tmp)
            fuzzed = [p for p in made if os.path.basename(p) in
                      ("v3-64k-c.qcow2", "v2-4k.qcow2", "v3-512-c.qcow2", "v3-64k-extended.qcow2", "v2-64k-c.qcow2")]
            gate_fuzz(args, fuzzed)
            gate_against_qemu(args, tmp, [p for p in fuzzed if p.endswith("-c.qcow2")])
        finally:
            if args.keep or failures:
                print("images in " + tmp)
            else:
                shutil.rmtree(tmp, ignore_errors=True)
    print()
    if failures:
        print("FAILED: " + ", ".join(failures))
        return 1
    print("diskimage-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
