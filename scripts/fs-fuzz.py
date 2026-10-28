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

The judges are held to e2fsprogs in turn:

    python3 scripts/fs-fuzz.py --e2fsck [--iterations N] [--seed S]

has the targets write out what they make and judge -- ext4mkfs every image
its mkfs makes, beside a manifest of its tree; ext2 its fresh images and its
power cuts, each tagged with the judge's verdict -- and holds each to
e2fsck: an ext4 image clean by `e2fsck -fn` (its planted journal replayed
by `e2fsck -E journal_only` first) and every directory and file of its
manifest in what `debugfs rdump` reads out, byte for byte; an ext2 image
the judge called clean clean by e2fsck, and one it called unclean not. It
needs e2fsprogs: Linux, or on a Mac the nos-builder container. And always,
a gate of its own: no judge under fuzz/fs/src/image/ may share a line with
extfmt, the format crate the kernel's driver reads through -- a judge that
did would hold the driver to itself.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, 'out', 'fs-fuzz')
VENDOR = os.path.join(ROOT, 'src', 'crates', 'vendor')

# The gate's seed. How many inputs of each target it runs is the target's
# own (targets/mod.rs).
GATE_SEED = '1'
JUDGES = os.path.join(ROOT, 'fuzz', 'fs', 'src', 'image')


def independent():
    """No judge names extfmt: the judges are the fuzzer's own reading of
    the formats, and a judge that shared the driver's would agree with it
    whatever it did."""
    uses = re.compile(r'\bextfmt\s*::|\buse\s+extfmt\b|\bextern\s+crate\s+extfmt\b')
    for d, _, files in os.walk(JUDGES):
        for f in files:
            path = os.path.join(d, f)
            if f.endswith('.rs') and uses.search(open(path, encoding='utf-8', errors='replace').read()):
                print('fs-fuzz: %s uses extfmt: a judge must be the fuzzer\'s own' % os.path.relpath(path, ROOT))
                return False
    return True


def fnv(data):
    h = 0xcbf29ce484222325
    for b in data:
        h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def judge_ext4(img, manifest, tmp):
    """An ext4 image held to e2fsck and to its manifest: what is wrong with
    it, nothing for a sound one."""
    lines = open(manifest).read().split('\n')
    journaled = int([l for l in lines if l.startswith('journaled ')][0].split()[1])
    hidden = int([l for l in lines if l.startswith('hidden ')][0].split()[1])
    work = os.path.join(tmp, 'work.img')
    shutil.copyfile(img, work)
    problems = []
    if journaled:
        r = subprocess.run(['e2fsck', '-fy', '-E', 'journal_only', work], capture_output=True, text=True)
        if r.returncode not in (0, 1):
            problems.append('e2fsck -E journal_only, exit %d: %s' % (r.returncode, r.stdout[-600:]))
    # An extent tree wider or deeper than its file needs is the format's to
    # allow, and the kernel's to leave: not e2fsck's to call a problem.
    r = subprocess.run(['e2fsck', '-fn', '-E', 'no_optimize_extents', work], capture_output=True, text=True)
    if r.returncode != 0:
        problems.append('e2fsck -fn, exit %d: %s' % (r.returncode, r.stdout[-1500:]))
    out = os.path.join(tmp, 'rdump')
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    subprocess.run(['debugfs', '-R', 'rdump / %s' % out, work], capture_output=True)
    want = {}
    for l in lines:
        w = l.split()
        if w and w[0] in ('d', 'f'):
            want[bytes.fromhex(w[1]).decode('utf-8', 'replace')] = w
    seen = set()
    for d, dirs, files in os.walk(out):
        for n in dirs + files:
            seen.add(os.path.relpath(os.path.join(d, n), out))
    for path, w in want.items():
        p = os.path.join(out, path.lstrip('/'))
        if w[0] == 'd':
            if not os.path.isdir(p):
                problems.append('no directory %s' % path)
        elif not os.path.isfile(p):
            problems.append('no file %s' % path)
        else:
            data = open(p, 'rb').read()
            if len(data) != int(w[2]) or '%016x' % fnv(data) != w[3]:
                problems.append('file %s: %d bytes, fnv %016x; made %s bytes, fnv %s'
                                % (path, len(data), fnv(data), w[2], w[3]))
    extra = [s for s in seen if s.lstrip('/') not in {p.lstrip('/') for p in want}
             and not s.startswith('lost+found')]
    if len(extra) > hidden:
        problems.append('entries the manifest does not have: %s' % extra[:5])
    os.unlink(work)
    return problems


def e2fsck_judged(args):
    """--e2fsck: the targets' images, held to e2fsprogs."""
    for tool in ('e2fsck', 'debugfs'):
        if shutil.which(tool) is None:
            print('fs-fuzz --e2fsck needs %s (on a Mac, run it in the nos-builder container)' % tool)
            return 1
    seed = args[args.index('--seed') + 1] if '--seed' in args else GATE_SEED
    it = int(args[args.index('--iterations') + 1]) if '--iterations' in args else 150
    tmp = tempfile.mkdtemp(prefix='nos-fs-e2fsck-')
    dump = os.path.join(tmp, 'dump')
    os.makedirs(dump)
    env = dict(os.environ, FS_FUZZ_DUMP=dump)
    binary = os.path.join(OUT, 'release', 'fs-fuzz')
    for target, n in (('ext4mkfs', it), ('ext2', max(it // 3, 1))):
        r = subprocess.run([binary, '--target', target, '--iterations', str(n), '--seed', seed], env=env, cwd=tmp)
        if r.returncode != 0:
            print('fs-fuzz --e2fsck: the %s target found something first' % target)
            return r.returncode
    failed, judged = [], {}
    for f in sorted(os.listdir(dump)):
        if not f.endswith('.img'):
            continue
        img = os.path.join(dump, f)
        tag = f.rsplit('-', 2)[0]
        if tag == 'ext4mkfs':
            problems = judge_ext4(img, img[:-4] + '.manifest', tmp)
        elif tag in ('mkfs', 'crash-clean', 'crash-unclean'):
            r = subprocess.run(['e2fsck', '-fn', img], capture_output=True, text=True)
            said = r.returncode != 0 or any(l.rstrip().endswith('? no') for l in r.stdout.splitlines())
            problems = [] if said == (tag == 'crash-unclean') else [
                'the judge called it %s, e2fsck %s: %s' % (tag, 'did not agree' if said else 'found it clean',
                                                         r.stdout[-800:])]
        else:
            continue
        judged[tag] = judged.get(tag, 0) + 1
        if problems:
            failed.append(f)
            print('FAIL: %s' % f)
            for p in problems[:4]:
                print('  ' + p.replace('\n', '\n  '))
    print('fs-fuzz --e2fsck: %s judged, %d at odds with e2fsprogs'
          % (', '.join('%d %s' % (n, t) for t, n in sorted(judged.items())), len(failed)))
    if failed:
        print('fs-fuzz --e2fsck: the images are in %s' % dump)
        return 1
    shutil.rmtree(tmp, ignore_errors=True)
    return 0


def main():
    args = sys.argv[1:]
    if not independent():
        return 1
    # The kernel's vendored crates, as the kernel's own build takes them: the
    # fuzzer's manifest is outside src/crates, whose .cargo/config.toml says so
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
    if '--e2fsck' in args:
        return e2fsck_judged(args)
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
