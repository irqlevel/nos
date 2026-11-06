#!/usr/bin/env python3
"""ipc fuzz: the kernel's message channel, fuzzed on the host.

Every service of the kernel's talks through the channel (src/crates/ipc,
docs/ipc.md) -- the image's tasks and the modules alike -- so a message
lost, doubled or out of order, a Down that never comes, a name that outlives
its mailbox or a queue that holds more than it says is a bug in each of them
at once. fuzz/ipc is a host program built from the channel's own crate, over
`kcore` and `ffi` as they are and the fuzzers' common machine (fuzz/common:
the kernel's tasks run one at a time, its mutexes and events, the lock rules
checked), with overflow checks on as a RUSTUB=1 kernel has them.

Its target turns random bytes into mailboxes of the image's and of made-up
modules, and into what is done with them -- messages by address and by name,
names, monitors, watches, receivers asleep until a message, a wake or a
close, modules unloaded with mailboxes left open, the halt -- and holds every
answer, every queue and every count to a model of its own: each message
received once, in order, with the bytes it was sent with; every Down exactly
once, in the place kept for it; a module's names under its own namespace; the
host's queued bytes, mailboxes, names and watches as counted, and nothing of
an input left when it ends. A panic, a broken invariant, a lock rule broken,
a spin and a hang are findings, each with the seed and iteration that make
it again, and the input in a file to replay.

By default the target runs a fixed number of inputs from a fixed seed, the
same every time: a gate. A campaign is `--seconds` and a seed of its own:

    python3 scripts/ipc-fuzz.py                          # the gate
    python3 scripts/ipc-fuzz.py --seed 7 --seconds 300   # five minutes
    python3 scripts/ipc-fuzz.py --replay ipc out/ipc-fuzz/findings/X.hex
    IPC_FUZZ_SAY=1 python3 scripts/ipc-fuzz.py --replay ...   # each step said

Needs cargo on the host; the crates it builds are the kernel's own, from
the vendored sources, offline.
Exit code 0 = no finding; 1 = a finding; 3 = a hang.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, 'out', 'ipc-fuzz')
VENDOR = os.path.join(ROOT, 'src', 'crates', 'vendor')

# The gate: enough inputs that every state the target counts is reached --
# a sleeper woken each of three ways, a module's mailboxes swept, the halt --
# in seconds.
GATE_SEED = '1'
GATE_ITERATIONS = '20000'


def main():
    args = sys.argv[1:]
    # The kernel's vendored crates, as the kernel's own build takes them: the
    # fuzzer's manifest is outside src/crates, whose .cargo/config.toml would
    # make any build under it a kernel build, so it is said here.
    build = subprocess.run(['cargo', 'build', '--release', '--offline', '--quiet',
                            '--manifest-path', os.path.join(ROOT, 'fuzz', 'ipc', 'Cargo.toml'),
                            '--target-dir', OUT,
                            '--config', 'source.crates-io.replace-with="vendored-sources"',
                            '--config', 'source.vendored-sources.directory="%s"' % VENDOR],
                           cwd=ROOT)
    if build.returncode != 0:
        print('ipc-fuzz: the build failed')
        return build.returncode
    findings = os.path.join(OUT, 'findings')
    os.makedirs(findings, exist_ok=True)
    if '--replay' in args:
        # The input's file as given, from where the caller is.
        i = args.index('--replay')
        if i + 2 < len(args):
            args[i + 2] = os.path.abspath(args[i + 2])
    else:
        if '--seed' not in args:
            args = ['--seed', GATE_SEED] + args
        if '--iterations' not in args and '--seconds' not in args:
            args = ['--iterations', GATE_ITERATIONS] + args
    run = subprocess.run([os.path.join(OUT, 'release', 'ipc-fuzz')] + args, cwd=findings)
    if run.returncode not in (0, 2):
        print('ipc-fuzz: the inputs are in %s' % findings)
    return run.returncode


if __name__ == '__main__':
    sys.exit(main())
