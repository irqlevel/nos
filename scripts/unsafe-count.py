#!/usr/bin/env python3
"""Count `unsafe` in the Rust crates, the same way every time.

    scripts/unsafe-count.py [crate ...]      # default: net block fs kcore

A crate is named by its package (`hv`, `mod-hv`), by its directory's name
when only one directory has it (`igb`), or by its path under src/crates
(`modules/hypervisor/hv`).

A site is an `unsafe { }` block, an `unsafe fn` (an `unsafe extern "C" fn`
included) or an `unsafe impl`. The layers -- net, block, fs -- are meant to
have it only at the C ABI, where a pointer and a length arrive from C++ or a
module; what needs it otherwise belongs in kcore, under a type that says why
it is sound. `-v` lists the files it is in.
"""

import glob
import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "crates")
SKIP = {"vendor", "target"}
PACKAGE = re.compile(r'^\[package\][^\[]*?^name\s*=\s*"([^"]+)"', re.M | re.S)

BLOCK = re.compile(r"unsafe\s*\{")
FN = re.compile(r"unsafe\s+(?:extern\s+\"C\"\s+)?fn\s")
IMPL = re.compile(r"unsafe\s+impl")


def count(text):
    return len(BLOCK.findall(text)), len(FN.findall(text)), len(IMPL.findall(text))


def workspace():
    """Every crate under src/crates: its path there, to its package's name."""
    found = {}
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP and not d.startswith("."))
        if "Cargo.toml" in filenames and os.path.abspath(dirpath) != os.path.abspath(ROOT):
            m = PACKAGE.search(open(os.path.join(dirpath, "Cargo.toml")).read())
            if m:
                found[os.path.relpath(dirpath, ROOT)] = m.group(1)
    return found


def resolve(arg, crates):
    """A crate's directory under src/crates, by its path, its package's name or
    its directory's name, in that order; a name two crates answer to is said."""
    if arg in crates:
        return arg
    for pick in ([rel for rel, name in crates.items() if name == arg],
                 [rel for rel in crates if os.path.basename(rel) == arg]):
        if len(pick) == 1:
            return pick[0]
        if len(pick) > 1:
            sys.exit("unsafe-count: %s is %s: name one by its path" % (arg, " and ".join(sorted(pick))))
    sys.exit("unsafe-count: no crate %s under src/crates" % arg)


def main():
    args = [a for a in sys.argv[1:] if a != "-v"]
    verbose = "-v" in sys.argv[1:]
    crates = workspace()
    names = args or ["net", "block", "fs", "kcore"]

    total = 0
    for crate in names:
        where = resolve(crate, crates)
        files = sorted(glob.glob(os.path.join(ROOT, where, "src", "**", "*.rs"), recursive=True))
        sums = [0, 0, 0]
        lines = 0
        per_file = []
        for path in files:
            text = open(path).read()
            lines += text.count("\n")
            found = count(text)
            sums = [a + b for a, b in zip(sums, found)]
            if sum(found):
                per_file.append((os.path.relpath(path, os.path.join(ROOT, where, "src")), found))

        total += sum(sums)
        print(f"{crate:8} {lines:6} lines  {sums[0]:4} blocks  {sums[1]:3} unsafe fn  "
              f"{sums[2]:3} unsafe impl  = {sum(sums)}")
        if verbose:
            for name, (blocks, fns, impls) in per_file:
                print(f"    {name:24} {blocks:4} {fns:4} {impls:4}")

    print(f"total unsafe sites: {total}")


if __name__ == "__main__":
    main()
