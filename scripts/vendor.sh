#!/bin/bash
# Refresh src/rust/vendor, the checked-in copy of every Rust dependency, so
# that a build never reaches crates.io (the Makefile passes --offline).
#
# Run this after adding, removing or bumping a dependency in src/rust, and
# commit the result together with Cargo.toml/Cargo.lock. It needs network
# access itself, once.
#
# It belongs to one nightly. -Z build-std resolves the compiler's own library
# workspace alongside ours (--sync below), so what lands here depends on the
# toolchain that ran the script -- and std's dependencies move between
# nightlies. src/rust/rust-toolchain.toml therefore pins a dated channel, and
# bumping that pin and re-running this script are one commit, not two.
#
#   scripts/vendor.sh
#
# Why it is not just `cargo vendor`: that copies every package in Cargo.lock,
# and the lock holds far more than the kernel compiles -- ring (an optional
# rustls backend we do not use), the Windows import libraries it would need on
# a platform we are not, wasi, libc. Verbatim, that is 130 MB of a 143 MB
# directory. They cannot simply be deleted: cargo still reads their manifests
# to resolve the lock, and still looks for the source files those manifests
# point at. So the tree stays and every file in it is emptied instead, bar the
# manifest -- along with the checksum list, which is cargo's own convention for
# a vendored package whose files have been touched.
#
# If a future feature change makes the kernel compile one of them, the build
# fails loudly (an empty crate root), and the fix is to run this script again.
set -eu

cd "$(dirname "$0")/../src/rust"

# --sync the standard library's own workspace as well: -Z build-std resolves
# it alongside ours (see .cargo/config.toml), and it has crates.io
# dependencies of its own -- resolution fails without them even though
# core and alloc never build one.
STD_MANIFEST="$(rustc --print sysroot)/lib/rustlib/src/rust/library/Cargo.toml"
[ -f "$STD_MANIFEST" ] || { echo "vendor: no rust-src component ($STD_MANIFEST)" >&2; exit 1; }

echo "vendor: for $(rustc --version)"
echo "vendor: copying dependency sources from crates.io..."
cargo vendor --versioned-dirs --sync "$STD_MANIFEST" vendor > /dev/null

python3 - <<'PY'
import json, os, subprocess

TARGETS = ["x86_64-unknown-none", "aarch64-unknown-none-softfloat"]

# The packages some target actually compiles, as cargo sees them with this
# tree's .cargo/config.toml (the --cfg flags there decide, for instance,
# which curve25519-dalek backend -- and so which of its dependencies -- is
# in the graph at all).
# What -Z build-std compiles on top of core and alloc. cargo tree does not
# cover the sysroot workspace, so these are named outright.
keep = {"compiler_builtins", "rustc-std-workspace-core"}
for target in TARGETS:
    out = subprocess.run(
        ["cargo", "tree", "--target", target, "-e", "normal,build",
         "--prefix", "none", "-f", "{p}"],
        capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            keep.add(parts[0] + "-" + parts[1][1:])

stripped = 0
for name in sorted(os.listdir("vendor")):
    path = os.path.join("vendor", name)
    if not os.path.isdir(path) or name in keep:
        continue

    checksum_path = os.path.join(path, ".cargo-checksum.json")
    with open(checksum_path) as f:
        checksum = json.load(f)

    # Emptied, not removed: cargo still wants to find src/lib.rs (or whatever
    # the manifest points at) to work out which targets the package has.
    for root, _dirs, files in os.walk(path):
        for entry in files:
            if entry in (".cargo-checksum.json", "Cargo.toml"):
                continue
            with open(os.path.join(root, entry), "w"):
                pass

    # An empty file list tells cargo not to verify what is left.
    with open(checksum_path, "w") as f:
        json.dump({"files": {}, "package": checksum.get("package")}, f)
    stripped += 1

print(f"vendor: {len(keep)} packages kept whole, {stripped} emptied")
PY

du -sh vendor
echo "vendor: done -- commit src/rust/vendor along with Cargo.lock"
