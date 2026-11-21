#!/usr/bin/env python3
"""Mutants of ext4's writing through its journal that `fs-fuzz.py`'s ext4rw
target must catch: each one rule taken out of the driver
(src/crates/layers/fs/src/ext4/), the fuzzer built over it into a build of
its own, and the target run at its gate's count -- which must fail. The
rules are the group commit's -- a call's transactions staged in a group
the journal commits later -- and what keeps a crash's picture of them
sound:

  freed_reused          blocks a staged transaction freed handed out before
                        the group commits (the transaction's own refusal of
                        them -- `admit` -- left in)
  freed_reused_silently the same with `admit`'s refusal gone too: only the
                        power cut's pictures tell
  no_background_commit  the task that commits in the background committing
                        nothing: a picture after five seconds shows an older
                        tree
  no_flush_before_commit  the log and the data written in place not on the
                        medium before the commit block that vouches for them
  no_revokes            what a group freed not revoked by its commit: a
                        replay writes a stale copy over what the block became
  tail_trusted          past a file's end, in its last block, taken as the
                        device holds it: a picture's file grown over it
                        reads what a write the power cut left uncommitted
                        put there

Not among them, as no crash can tell: a checkpoint writing staged copies
home -- none is staged when one runs, a commit always finding room in a
log a checkpoint keeps at most half full -- and a group let grow past what
one transaction may hold, which bounds the memory a group holds and keeps
to Linux's convention: a replay, Linux's and e2fsck's, takes a transaction
of any size the log holds, and one past the log aborts the journal.

The source is put back after each, whatever happens (an exception, ^C, a
build that fails), and checked against its hash before the next; nothing
here is piped anywhere, so no SIGPIPE can leave a mutant behind.

Usage: python3 tests/fuzz/fs/mutants.py [name...]
Exit 0 when every mutant was caught; 1 when one was not.
"""
import hashlib
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
EXT4 = os.path.join(ROOT, 'src', 'crates', 'layers', 'fs', 'src', 'ext4')
OUT = os.path.join(ROOT, 'out', 'fs-mutants')

# name, then each file it changes with the text there and what it becomes.
MUTANTS = [
    ('freed_reused', [
        ('bitmaps.rs', [
            ("&& !staged.contains(start + i))", ")"),
            ("for freed in [&self.freed, &self.jnl.group.freed] {", "for freed in [&self.freed] {"),
        ]),
    ]),
    ('freed_reused_silently', [
        ('bitmaps.rs', [
            ("&& !staged.contains(start + i))", ")"),
            ("for freed in [&self.freed, &self.jnl.group.freed] {", "for freed in [&self.freed] {"),
        ]),
        ('txn.rs', [
            ("        if self.jnl.group.freed.contains(b) {", "        if false && self.jnl.group.freed.contains(b) {"),
        ]),
    ]),
    ('no_background_commit', [
        ('mod.rs', [
            ("        journal::commit_group(&mut self.io, &mut self.cache, jnl, journal::CommitWhy::Age)\n    }\n\n"
             "    fn new_vnode",
             "        let _ = jnl;\n        true\n    }\n\n    fn new_vnode"),
        ]),
    ]),
    ('no_flush_before_commit', [
        ('journal.rs', [
            ("    if !io.flush() {\n        jnl.abort(io, \"the device does not flush a transaction\");",
             "    if false && !io.flush() {\n        jnl.abort(io, \"the device does not flush a transaction\");"),
        ]),
    ]),
    ('no_revokes', [
        ('journal.rs', [
            ("    let revokes = revokes_of(jnl, &[&jnl.group.freed]);",
             "    let revokes: Vec<u64> = Vec::new();"),
            ("        let revokes = revokes_of(self.jnl, &[&self.jnl.group.freed, &self.freed]);",
             "        let revokes: Vec<u64> = Vec::new();"),
        ]),
    ]),
    ('tail_trusted', [
        ('data.rs', [
            ("                        if end < bs as u64 {\n                            self.io.slot_mut(k)[end as usize..].fill(0);",
             "                        if false && end < bs as u64 {\n                            self.io.slot_mut(k)[end as usize..].fill(0);"),
        ]),
    ]),
]


def digest(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


def run(name, files):
    """Whether the target caught the mutant: (caught, what it said)."""
    kept = []
    try:
        for rel, edits in files:
            path = os.path.join(EXT4, rel)
            with open(path) as f:
                src = f.read()
            kept.append((path, src, digest(path)))
            for old, new in edits:
                if src.count(old) != 1:
                    raise SystemExit('mutant %s: %d matches in %s of %r -- the source moved on' %
                                     (name, src.count(old), rel, old[:60]))
                src = src.replace(old, new)
            with open(path, 'w') as f:
                f.write(src)
        env = dict(os.environ, FS_FUZZ_OUT=OUT)
        r = subprocess.run([sys.executable, os.path.join(ROOT, 'tests', 'fs-fuzz.py'), '--target', 'ext4rw'],
                           cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        said = [l for l in r.stdout.splitlines() if l.startswith('FAIL') or 'build failed' in l]
        if any('build failed' in l for l in said):
            raise SystemExit('mutant %s does not build:\n%s' % (name, r.stdout[-2000:]))
        return r.returncode != 0, (said[0] if said else 'exit %d' % r.returncode)
    finally:
        for path, src, want in kept:
            with open(path, 'w') as f:
                f.write(src)
            if digest(path) != want:
                raise SystemExit('%s is not as it was after mutant %s' % (path, name))


def main():
    names = sys.argv[1:]
    unknown = [n for n in names if n not in dict(MUTANTS)]
    if unknown:
        print('no such mutant: %s' % ', '.join(unknown))
        return 2
    survived = 0
    for name, files in MUTANTS:
        if names and name not in names:
            continue
        caught, said = run(name, files)
        print('%-24s %s' % (name, ('caught: ' + said[:200]) if caught else 'SURVIVED'), flush=True)
        survived += not caught
    return 1 if survived else 0


if __name__ == '__main__':
    sys.exit(main())
