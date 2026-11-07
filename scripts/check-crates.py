#!/usr/bin/env python3
"""The Rust workspace's layout, held to its rules (docs/project-layout.md).

  - Every crate under src/crates is in an area's directory -- base/,
    layers/, drivers/, hypervisor/, modules/, common/ -- but kernel/, the
    staticlib itself; and every one is a member of the workspace.
  - Every crate under common/ has no kernel in it and no unsafe: it depends
    by path on nothing outside common/, and its lib.rs says
    #![forbid(unsafe_code)] -- so that what a module, hvboot and the host's
    fuzzers link as it is stays something they can, and a block of unsafe
    written into one does not compile.

Run by `make check`. Exit code 22 if a rule is broken, as cppcheck's there.
"""

import os
import re
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
CRATES = os.path.normpath(os.path.join(ROOT, "src", "crates"))
AREAS = {"base", "layers", "drivers", "hypervisor", "modules", "common"}
TOP = {"kernel"}
SKIP = {"vendor", "target"}
PATH = re.compile(r'\bpath\s*=\s*"([^"]+)"')
FORBID = re.compile(r"^#!\[forbid\(unsafe_code\)\]", re.M)


def crates():
    for dirpath, dirnames, filenames in os.walk(CRATES):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP and not d.startswith("."))
        if "Cargo.toml" in filenames and os.path.normpath(dirpath) != CRATES:
            yield os.path.relpath(dirpath, CRATES)


def members():
    text = open(os.path.join(CRATES, "Cargo.toml")).read()
    m = re.search(r"^members\s*=\s*\[(.*?)\]", text, re.M | re.S)
    return set(re.findall(r'"([^"]+)"', m.group(1))) if m else set()


def main():
    wrong = []
    listed = members()
    for rel in crates():
        top = rel.split(os.sep)[0]
        if rel not in TOP and (top not in AREAS or os.sep not in rel):
            wrong.append("%s: a crate outside an area's directory (%s)" % (rel, ", ".join(sorted(AREAS))))
        if rel.replace(os.sep, "/") not in listed:
            wrong.append("%s: not a member of the workspace (src/crates/Cargo.toml)" % rel)
        if top != "common":
            continue
        here = os.path.join(CRATES, rel)
        for dep in PATH.findall(open(os.path.join(here, "Cargo.toml")).read()):
            target = os.path.normpath(os.path.join(here, dep))
            inside = os.path.commonpath([target, here]) == here
            if not inside and os.path.commonpath([target, os.path.join(CRATES, "common")]) != os.path.join(CRATES, "common"):
                wrong.append("%s: depends on %s, outside common/" % (rel, os.path.relpath(target, CRATES)))
        lib = os.path.join(here, "src", "lib.rs")
        if not os.path.isfile(lib) or not FORBID.search(open(lib).read()):
            wrong.append("%s: its lib.rs does not say #![forbid(unsafe_code)]" % rel)
    for w in wrong:
        print("check-crates: " + w)
    sys.exit(22 if wrong else 0)


if __name__ == "__main__":
    main()
