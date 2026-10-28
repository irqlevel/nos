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
# FS_FUZZ_OUT for a build of its own: the nos-builder container's, say,
# beside the host's in one checkout.
OUT = os.environ.get('FS_FUZZ_OUT') or os.path.join(ROOT, 'out', 'fs-fuzz')
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


# What e2fsck says of a filesystem the kernel's ext4 left that is no
# damage: a lost+found the calls removed -- a convention of e2fsck's, which
# would make it again, not a part of the format -- and, after a crash, the
# superblock's free counts, which Linux keeps lazily too.
NO_LPF = re.compile(r'^/lost\+found not found\.')
LAZY_COUNTS = re.compile(r'^Free (blocks|inodes) count wrong \(')


def e2fsck_problems(work, lazy):
    """What `e2fsck -fn` finds in `work`, but for what is no damage: every
    question it answers no, by the problem it asks of."""
    r = subprocess.run(['e2fsck', '-fn', '-E', 'no_optimize_extents', work], capture_output=True, text=True)
    if r.returncode not in (0, 4):
        return ['e2fsck -fn, exit %d: %s' % (r.returncode, r.stdout[-1500:])]
    out = r.stdout.splitlines()
    benign = [NO_LPF] + ([LAZY_COUNTS] if lazy else [])
    asked, bad = 0, []
    for k, line in enumerate(out):
        if not line.rstrip().endswith('? no'):
            continue
        asked += 1
        said = [line, out[k - 1] if k else '']
        if not any(b.match(t) for b in benign for t in said):
            bad.append(' / '.join(t for t in said if t))
    if bad or (r.returncode != 0 and asked == 0):
        return ['e2fsck -fn, exit %d: %s: %s' % (r.returncode, bad[:4], r.stdout[-1500:])]
    return []


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
        # The replay alone: with -f, e2fsck goes on to check and repair.
        r = subprocess.run(['e2fsck', '-y', '-E', 'journal_only', work], capture_output=True, text=True)
        if r.returncode not in (0, 1):
            problems.append('e2fsck -E journal_only, exit %d: %s' % (r.returncode, r.stdout[-600:]))
    # An extent tree wider or deeper than its file needs is the format's to
    # allow, and the kernel's to leave: not e2fsck's to call a problem.
    problems += e2fsck_problems(work, False)
    out = os.path.join(tmp, 'rdump')
    shutil.rmtree(out, ignore_errors=True)
    os.makedirs(out)
    subprocess.run(['debugfs', '-R', 'rdump / %s' % out, work], capture_output=True)
    want = {}
    for l in lines:
        w = l.split()
        if w and w[0] in ('d', 'f'):
            # As os.walk names what rdump wrote: bytes that are not UTF-8
            # escaped, not replaced.
            want[os.fsdecode(bytes.fromhex(w[1]))] = w
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


# The superblock fields e2fsck's replay sets for itself -- its write and
# check times, the kilobytes it counts written -- and the checksum over them.
SB_E2FSCK_OWN = [(0x30, 4), (0x40, 4), (0x178, 8), (0x3FC, 4)]


def masked(img):
    b = bytearray(img)
    for at, n in SB_E2FSCK_OWN:
        b[1024 + at:1024 + at + n] = bytes(n)
    return bytes(b)


def replay_agrees(img, replayed, tmp):
    """The judge's replay of an image's journal, held to e2fsck's: the same
    image byte for byte, but for e2fsck's own superblock fields."""
    work = os.path.join(tmp, 'replay.img')
    shutil.copyfile(img, work)
    # The replay alone: with -f e2fsck would go on to check, and with -y to
    # put right silently what it keeps for itself (the superblock's free
    # counts, a group's unused inodes) -- its repairs, not its replay.
    r = subprocess.run(['e2fsck', '-y', '-E', 'journal_only', work], capture_output=True, text=True)
    if r.returncode not in (0, 1):
        return ['e2fsck -E journal_only, exit %d: %s' % (r.returncode, r.stdout[-400:])]
    a, b = masked(open(work, 'rb').read()), masked(open(replayed, 'rb').read())
    if a == b:
        return []
    at = next((k for k in range(min(len(a), len(b))) if a[k] != b[k]), min(len(a), len(b)))
    return ['the judge\'s replay differs from e2fsck\'s at byte %#x (block %d of 4 KiB)' % (at, at // 4096)]


def judge_of(binary, img, tmp):
    """The judge's check of an image from outside: its verdict line, and
    what it found."""
    env = dict(os.environ, FS_FUZZ_IMAGE=img)
    r = subprocess.run([binary, '--target', 'ext4judge', '--iterations', '1', '--seed', '1'], env=env, cwd=tmp,
                       capture_output=True, text=True)
    lines = r.stdout.splitlines()
    verdict = next((l.split()[1] for l in lines if l.startswith('verdict ')), 'none')
    return verdict, '\n'.join(l for l in lines if l.startswith(('verdict', 'corrupt:', 'unclean:')))


def e2fsck_says(img):
    r = subprocess.run(['e2fsck', '-fn', '-E', 'no_optimize_extents', img], capture_output=True, text=True)
    return r.returncode != 0 or any(l.rstrip().endswith('? no') for l in r.stdout.splitlines()), r.stdout


def mke2fs_images(tmp, rng):
    """ext4 images as mke2fs makes them -- 1.46's defaults, 1 KiB blocks
    with 128-byte inodes and no 64bit, metadata_csum_seed with the UUID
    changed after -- holding files of every size, a sparse one of many
    pieces, a directory e2fsck -D indexed, a long name, a symlink and
    attributes in a block of their own."""
    content = os.path.join(tmp, 'content')
    os.makedirs(os.path.join(content, 'big'))
    os.makedirs(os.path.join(content, 'a', 'b'))
    open(os.path.join(content, 'small'), 'wb').write(b'small\n')
    open(os.path.join(content, 'medium'), 'wb').write(rng.randbytes(70000))
    with open(os.path.join(content, 'sparse'), 'wb') as f:
        for k in range(600):
            f.seek(k * 16384)
            f.write(rng.randbytes(4096))
    for k in range(200):
        open(os.path.join(content, 'big', 'entry-%04d' % k), 'wb').write(b'%d' % k)
    open(os.path.join(content, 'a', 'b', 'deep'), 'wb').write(b'deep')
    open(os.path.join(content, 'n' * 70), 'wb').write(b'long')
    os.symlink('small', os.path.join(content, 'link'))
    open(os.path.join(tmp, 'value'), 'wb').write(rng.randbytes(1200).hex().encode())
    made = []
    for name, opts in (('mke2fs-4k', ['-b', '4096']), ('mke2fs-1k', ['-b', '1024', '-I', '128', '-O', '^64bit']),
                       ('mke2fs-seed', ['-b', '4096', '-O', 'metadata_csum_seed'])):
        img = os.path.join(tmp, name + '.img')
        with open(img, 'wb') as f:
            f.truncate(48 << 20)
        r = subprocess.run(['mke2fs', '-q', '-F', '-t', 'ext4'] + opts + ['-d', content, img], capture_output=True,
                           text=True)
        if r.returncode != 0:
            print('fs-fuzz --e2fsck: mke2fs %s: %s' % (' '.join(opts), r.stderr))
            return None
        if name == 'mke2fs-seed':
            subprocess.run(['tune2fs', '-U', 'random', img], capture_output=True)
        subprocess.run(['e2fsck', '-fyD', img], capture_output=True)
        cmds = os.path.join(tmp, 'cmds')
        open(cmds, 'w').write('ea_set -f %s /medium user.big\n' % os.path.join(tmp, 'value'))
        subprocess.run(['debugfs', '-w', '-f', cmds, img], capture_output=True)
        made.append((name, img))
    return made


def inode_of(img, path):
    r = subprocess.run(['debugfs', '-R', 'stat %s' % path, img], capture_output=True, text=True)
    m = re.search(r'Inode: (\d+)', r.stdout)
    return int(m.group(1)) if m else None


def orphans_agree(binary, tmp, base):
    """Orphans, as a crash between the transactions of one removal or one
    truncation leaves them: a file of no links named nowhere, and one cut
    to a block whose extents still run past it, each on the superblock's
    list -- left by debugfs. The judge releases them as e2fsck does: the
    image clean to it, and the tree it reads the one `e2fsck -fy` leaves."""
    problems = []
    for kind in ('deleted', 'truncated'):
        img = os.path.join(tmp, 'orphan-%s.img' % kind)
        shutil.copyfile(base, img)
        name = '/small' if kind == 'deleted' else '/medium'
        n = inode_of(img, name)
        cmds = ['sif <%d> links_count 0' % n, 'unlink %s' % name] if kind == 'deleted' else ['sif <%d> size 4096' % n]
        cmds += ['sif <%d> dtime 0' % n, 'ssv last_orphan %d' % n]
        open(os.path.join(tmp, 'cmds'), 'w').write('\n'.join(cmds) + '\n')
        subprocess.run(['debugfs', '-w', '-f', os.path.join(tmp, 'cmds'), img], capture_output=True)
        ours, said = judge_of(binary, img, tmp)
        fixed = img + '.fixed'
        shutil.copyfile(img, fixed)
        subprocess.run(['e2fsck', '-fy', fixed], capture_output=True)
        clean, out = e2fsck_says(fixed)
        theirs, said_fixed = judge_of(binary, fixed, tmp)
        tree = lambda s: re.search(r'files \d+ dirs \d+', s).group(0) if re.search(r'files \d+ dirs \d+', s) else s
        if ours != 'clean' or 'orphans 1' not in said:
            problems.append('orphan-%s: the judge does not release the orphan as e2fsck does:\n%s' % (kind, said))
        elif clean or theirs != 'clean' or tree(said) != tree(said_fixed):
            problems.append('orphan-%s: the tree after the judge releases the orphan, %s, is not the one e2fsck -fy '
                            'leaves, %s:\n%s' % (kind, tree(said), tree(said_fixed), out[-400:]))
    return problems


def judge_against_mke2fs(binary, tmp, seed):
    """The judge's check held to e2fsck on mke2fs's images: clean where
    e2fsck finds them clean, and every byte of their metadata e2fsck notices
    changed noticed by the judge too."""
    import random
    rng = random.Random(int(seed))
    made = mke2fs_images(tmp, rng)
    if made is None:
        return ['mke2fs failed']
    problems, judged, stricter, resize_only = [], 0, 0, 0
    for name, img in made:
        verdict, said = judge_of(binary, img, tmp)
        judged += 1
        if verdict != 'clean':
            problems.append('%s: the judge finds mke2fs\'s image %s:\n%s' % (name, verdict, said))
            continue
        base = open(img, 'rb').read()
        for trial in range(30):
            at = rng.randrange(min(len(base), 2 << 20))
            bad = bytearray(base)
            bad[at] ^= rng.randrange(1, 256)
            path = os.path.join(tmp, 'damaged.img')
            open(path, 'wb').write(bad)
            theirs, out = e2fsck_says(path)
            # The resize inode's tree of reserved descriptor blocks is
            # e2fsck's to judge and nos's never to write: the judge leaves
            # it alone, and a byte of it changed is not held against it.
            said_lines = [l for l in out.splitlines() if l.rstrip().endswith('? no') or 'IGNORED' in l]
            if said_lines and all(re.search(r'\b[Ii]node 7\b', l) for l in said_lines):
                resize_only += 1
                continue
            ours, said = judge_of(binary, path, tmp)
            judged += 1
            if theirs and ours == 'clean':
                problems.append('%s, byte %#x changed: e2fsck says so and the judge does not:\n%s' % (name, at, out[-600:]))
            elif ours != 'clean' and not theirs:
                stricter += 1
    problems += orphans_agree(binary, tmp, made[0][1])
    if stricter:
        print('fs-fuzz --e2fsck: %d changed bytes the judge noticed and e2fsck did not' % stricter)
    print('fs-fuzz --e2fsck: %d of mke2fs\'s images judged, damaged and not; %d bytes of the resize inode\'s left '
          'to e2fsck' % (judged, resize_only))
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
    for target, n in (('ext4mkfs', it), ('ext2', max(it // 3, 1)), ('ext4rw', it)):
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
        if tag in ('ext4mkfs', 'ext4rw'):
            # ext4rw's: what the kernel's driver left at a clean unmount.
            problems = judge_ext4(img, img[:-4] + '.manifest', tmp)
            if os.path.exists(img[:-4] + '.replayed'):
                problems += replay_agrees(img, img[:-4] + '.replayed', tmp)
                judged['judge-replay'] = judged.get('judge-replay', 0) + 1
        elif tag == 'ext4rw-crash':
            # What the power going left of the kernel's writing: e2fsck's
            # replay of its journal leaves nothing for e2fsck to find.
            work = os.path.join(tmp, 'work.img')
            shutil.copyfile(img, work)
            # The replay alone: with -f, e2fsck goes on to check and repair.
            r = subprocess.run(['e2fsck', '-y', '-E', 'journal_only', work], capture_output=True, text=True)
            problems = [] if r.returncode in (0, 1) else ['e2fsck -E journal_only, exit %d: %s'
                                                          % (r.returncode, r.stdout[-600:])]
            problems += e2fsck_problems(work, True)
            os.unlink(work)
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
    for p in judge_against_mke2fs(binary, tmp, seed):
        failed.append(p.split(':')[0])
        print('FAIL: ' + p.replace('\n', '\n  '))
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
