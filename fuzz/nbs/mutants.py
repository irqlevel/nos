#!/usr/bin/env python3
"""The meta target's mutants: one rule of NBS's metadata
(src/crates/common/storage/nbsmeta) taken out at a time, and the gate
(`nbs-fuzz gate`) must fail -- a rule the gate does not catch the loss of is
one it does not hold the crate to.

    python3 fuzz/nbs/mutants.py [name...]

Each file is restored from what it was before, whatever happens, and held to
it by its digest. Exit code 0 = every mutant caught."""

import hashlib
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SRC = os.path.join(ROOT, 'src', 'crates', 'common', 'storage', 'nbsmeta', 'src')
OUT = os.path.join(ROOT, 'out', 'nbs-mutants')
BINARY = os.path.join(OUT, 'release', 'nbs-fuzz')
VENDOR = os.path.join(ROOT, 'src', 'crates', 'vendor')

# name, what it takes out, the file, the text it changes, what it becomes
MUTANTS = [
    ('home_first', "a replica on the volume's own host",
     'place.rs',
     'if self.candidates[h].free > 0 {',
     'if false && self.candidates[h].free > 0 {'),
    ('most_room', 'the hosts with the most room taken first',
     'place.rs',
     'let key = (c.free, tie(volume, index, c.cs));',
     'let key = (0, tie(volume, index, c.cs));'),
    ('rebuild_room', 'room kept to rebuild the biggest host',
     'place.rs',
     'others >= self.candidates[biggest].reserved',
     'true'),
    ('too_few_is_shape', 'three full hosts no room rather than too few hosts',
     'place.rs',
     '        self.candidates.len()\n',
     '        self.candidates.iter().filter(|c| c.free > 0).count()\n'),
    ('room_given_back', "a freed chunk's slots given back",
     'storage.rs',
     '                c.reserved -= 1;',
     '                c.reserved -= 0;'),
    ('whole_or_none', 'a volume that does not fit refused whole',
     'storage.rs',
     'trial.pick(volume, from + k).ok_or(Refusal::NoRoom)?;',
     'if trial.pick(volume, from + k).is_none() { break; }'),
    ('force_fences', 'a forced attach a new generation',
     'storage.rs',
     '            v.generation += 1;',
     '            v.generation += u64::from(!force);'),
    ('device_elsewhere', "a device another host's refused",
     'storage.rs',
     '(false, _) => Err(Refusal::DeviceElsewhere),',
     '(false, _) => Ok(d.id),'),
    ('name_taken', 'a volume made again with another size refused',
     'storage.rs',
     'if v.kind == Kind::Volume && v.size_mib == size_mib { Ok(v.id) } else { Err(Refusal::NameTaken) }',
     'Ok(v.id)'),
    ('delete_attached', 'an attached volume kept',
     'storage.rs',
     'if v.writer.is_some() || !v.readers.is_empty() {\n                return Err(Refusal::Attached);',
     'if false {\n                return Err(Refusal::Attached);'),
    ('divergence_said', "a digest not the member's own said",
     'snapshot.rs',
     '            Some((_, mine)) => {\n                if self.diverged.is_none() {',
     '            Some((_, mine)) => {\n                if false && self.diverged.is_none() {'),
    ('free_list_checked', "a snapshot's free list held to its chunks",
     'snapshot.rs',
     'if walked != free {',
     'if false && walked != free {'),
    ('reserved_checked', "a snapshot's reserved held to its replicas",
     'snapshot.rs',
     'if replicas != c.reserved {',
     'if false && replicas != c.reserved {'),
    ('chunk_count_checked', "a snapshot's chunks held to the maps that name them",
     'snapshot.rs',
     'if ch.count == 0 || ch.count != n || ch.flags & !CHUNK_WRITTEN != 0 {',
     'if ch.count == 0 || ch.flags & !CHUNK_WRITTEN != 0 {'),
    ('digest_streamed', 'a snapshot written in pieces as in one',
     'snapshot.rs',
     '        self.sent = 0;\n        let s = &self.state;',
     '        let s = &self.state;'),
]


def digest(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


def build():
    return subprocess.run(['cargo', 'build', '--release', '--quiet', '--offline',
                           '--manifest-path', os.path.join(ROOT, 'fuzz', 'nbs', 'Cargo.toml'),
                           '--target-dir', OUT,
                           '--config', 'source.crates-io.replace-with="vendored-sources"',
                           '--config', 'source.vendored-sources.directory="%s"' % VENDOR], cwd=ROOT,
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE).returncode


def run(*args):
    """A run's exit code and its first failing line."""
    p = subprocess.run([BINARY] + list(args), cwd=ROOT, capture_output=True, text=True, timeout=900)
    first = next((l for l in p.stdout.splitlines() if l.startswith('FAIL')), '')
    return p.returncode, first


def main():
    wanted = set(sys.argv[1:])
    tried = 0
    missed = []
    for name, what, file, old, new in MUTANTS:
        if wanted and name not in wanted:
            continue
        path = os.path.join(SRC, file)
        with open(path) as f:
            good = f.read()
        good_digest = digest(path)
        tried += 1
        if good.count(old) != 1:
            print('BROKEN: %s -- its text is in %s %d times, not once' % (name, file, good.count(old)))
            missed.append(name)
            continue
        try:
            with open(path, 'w') as f:
                f.write(good.replace(old, new))
            if build() != 0:
                print('BROKEN: %s -- the mutant does not build' % name)
                missed.append(name)
                continue
            code, first = run('gate')
            if code == 0:
                print('SURVIVED: %s (%s)' % (name, what))
                missed.append(name)
            else:
                print('CAUGHT: %s (%s) -- %s' % (name, what, first[:200]))
        finally:
            with open(path, 'w') as f:
                f.write(good)
            if digest(path) != good_digest:
                sys.exit('%s is not what it was: restore it from git' % path)
    print('%d mutants, %d not caught%s' % (tried, len(missed), (': ' + ', '.join(missed)) if missed else ''))
    return 1 if missed else 0


if __name__ == '__main__':
    sys.exit(main())
