#!/usr/bin/env python3
"""fs fuzz: the storage layers, fuzzed on the host.

What is on a disk is whoever wrote it's to choose -- a disk that came with
the machine, an image somebody made, a filesystem another kernel left half
written -- and a device says itself how big it is and how big a sector is.
So a panic, an overflow, a lock broken, a loop that never ends, a write
where nothing may write, anywhere on the paths that read a disk, is one
somebody else can cause. fuzz/fs is a host program built from the kernel's
own crates -- `block` (the device table, the partition tables, the claims,
the disk log) and `fs` (the VFS, ext2, nanofs, ramfs, procfs, the file ABI,
the shell's commands), over `kcore` and `ffi` as they are -- linked with the
rest of a kernel written for the purpose (fuzz/common) and disks whose media
are the fuzzer's: each with a volatile write cache, a power switch, and
requests that fail.

The targets: part (MBRs and GPTs, I/O through the partitions, the claims),
disklog, ext2 and nanofs (sound images, worked through the VFS and held to a
model of the tree, e2fsck's judgement after a clean unmount, and what any
power cut or failed request leaves), ext2bad and nanofsbad (images damaged,
and made to lie with their checksums right), vfs (many mounts, the C ABI and
the file ABI a module uses, tasks at work at once), shell (the storage
commands) and rootfs (what `root=` mounts at boot). The images are the
fuzzer's own, made and judged from the formats -- the judge agrees with
e2fsck -fn, which FS_FUZZ_DUMP=<dir> is for checking -- and not from the
drivers. Overflow checks are on, as a RUSTUB=1 kernel has them. Each input
runs in a process of its own, forked from one booted machine.

By default every target runs its own number of inputs from a fixed seed, the
same every time: a gate, a couple of minutes. A campaign is `--seconds` a
target and seeds of its own:

    python3 scripts/fs-fuzz.py                          # the gate
    python3 scripts/fs-fuzz.py --seed 7 --seconds 300   # five minutes a target
    python3 scripts/fs-fuzz.py --target ext2 --target nanofs
    python3 scripts/fs-fuzz.py --replay ext2 out/fs-fuzz/findings/X.hex [--trace]

FS_FUZZ_STATS=1 adds each target's slowest input and how often it reached
each state it counts. Needs cargo on the host, and nothing else: the program
depends only on the kernel's crates, and builds offline. Exit code 0 = no
finding; 1 = a finding; 3 = a hang.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, 'out', 'fs-fuzz')
VENDOR = os.path.join(ROOT, 'src', 'rust', 'vendor')

# The gate's seed. How many inputs of each target it runs is the target's
# own (targets/mod.rs).
GATE_SEED = '1'


def main():
    args = sys.argv[1:]
    # The kernel's vendored crates, as the kernel's own build takes them: the
    # fuzzer's manifest is outside src/rust, whose .cargo/config.toml says so
    # for the kernel -- and makes any build under it a kernel build -- so it
    # is said here, and cargo is run from the root.
    build = subprocess.run(['cargo', 'build', '--release', '--offline', '--quiet',
                            '--manifest-path', os.path.join(ROOT, 'fuzz', 'fs', 'Cargo.toml'),
                            '--target-dir', OUT,
                            '--config', 'source.crates-io.replace-with="vendored-sources"',
                            '--config', 'source.vendored-sources.directory="%s"' % VENDOR],
                           cwd=ROOT)
    if build.returncode != 0:
        print('fs-fuzz: the build failed')
        return build.returncode
    findings = os.path.join(OUT, 'findings')
    os.makedirs(findings, exist_ok=True)
    if '--replay' in args:
        # The input's file as given, from where the caller is.
        i = args.index('--replay')
        if i + 2 < len(args):
            args[i + 2] = os.path.abspath(args[i + 2])
    elif '--seed' not in args:
        args = ['--seed', GATE_SEED] + args
    run = subprocess.run([os.path.join(OUT, 'release', 'fs-fuzz')] + args, cwd=findings)
    if run.returncode not in (0, 2):
        print('fs-fuzz: the inputs are in %s' % findings)
    return run.returncode


if __name__ == '__main__':
    sys.exit(main())
