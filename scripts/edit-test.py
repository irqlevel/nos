#!/usr/bin/env python3
"""Editor test: `edit` (src/crates/edit), decided on the host.

The edit module runs src/crates/edit in an SSH session; fuzz/edit compiles that
crate, as it is, into a host program that runs it the same way -- the bytes
a terminal sends through the key decoder, the editor's saves and quits
answered, its frames drawn on a terminal of the test's own, which does what
xterm does with them -- and holds it to:

  - sessions of keys, each with what it must leave in the file, at the
    cursor, in the clipboard and on the screen: a Mac's keys on Control,
    Option's words, the selection, undo and redo, finding, going to a line,
    the questions before a quit and before a write over a file that changed
    on disk, pastes -- a ^S or an ESC in one is text, not a key -- and what
    is not text (control bytes, invalid UTF-8) shown for what it is;
  - files of random bytes read and written back untouched, byte for byte;
  - sessions of random keys, pastes, garbage, pauses and resizes on random
    files, after each of which the editor keeps its promises -- the cursor
    and the selection in the text, where glyphs start -- and the terminal has
    been sent nothing it would wrap, scroll or not know; now and then what
    was drawn a row at a time must be what a full redraw draws, and undoing
    everything must give back the file as it was read, and redoing it, the
    file as it was.

    python3 scripts/edit-test.py                        # the gate
    python3 scripts/edit-test.py --seed 7 --seconds 600 # a campaign
    python3 scripts/edit-test.py --session N            # one session again

Needs cargo on the host. Exit code 0 = every check passed.
"""

import argparse
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, "fuzz", "edit")
OUT = os.path.join(ROOT, "out", "edit-test")
BIN = os.path.join(OUT, "release", "edit-test")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--seed", type=int, default=1, help="where the random sessions start (1)")
    p.add_argument("--sessions", type=int, default=3000, help="random sessions (3000)")
    p.add_argument("--seconds", type=int, help="random sessions for this long instead")
    p.add_argument("--session", type=int, help="one session again, by the seed a failure said")
    args = p.parse_args()

    r = subprocess.run(["cargo", "build", "--release", "--offline", "--quiet", "--target-dir", OUT], cwd=FUZZ)
    if r.returncode != 0:
        sys.exit("edit-test: fuzz/edit did not build")

    if args.session is not None:
        argv = [BIN, "--session", str(args.session)]
    else:
        argv = [BIN, "all", "--seed", str(args.seed), "--sessions", str(args.sessions)]
        if args.seconds is not None:
            argv += ["--seconds", str(args.seconds)]
    sys.exit(subprocess.run(argv).returncode)


if __name__ == "__main__":
    main()
