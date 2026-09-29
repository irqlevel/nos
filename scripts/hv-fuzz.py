#!/usr/bin/env python3
"""hv fuzz: the hypervisor's guest-facing code, fuzzed on the host.

A guest decides every value the hypervisor's devices are handed -- every
port it writes, every MSR, every byte of its virtio rings and MSI-X tables,
the instruction behind an MMIO fault -- and so a panic, an overflow or a
loop that never ends in any of them is one a guest can cause, on the host,
under everyone else's guests. scripts/hv-fuzz is a host program built from
the hypervisor's own sources (src/rust/hv/src/{devices/*,lapic,acpi,insn,
walk,linux,mmio,policy,smp,run}.rs, hvarch's VMCB layout, and the module's
DHCP server, src/rust/modules/hv/src/dhcp.rs) over stand-ins
for the kernel and the CPU, with overflow checks on as a RUSTUB=1 kernel has
them. Each target turns random bytes into what a guest does to one device --
or, for `platform`, to the whole machine: a Linux guest's platform built and
loaded as `hv boot` builds it, run by the real run loop on its first CPU,
with a script in place of the CPU saying what the guest does at each entry.
A panic, a broken invariant (an interrupt lost, a disk request outside the
disk, a frame longer than a frame) and a spin -- the run loop neither
entering the guest nor sleeping -- are each reported with the seed and
iteration that make them again, and the input in a file to replay.

By default every target runs a fixed number of inputs from a fixed seed, the
same every time: a gate. A campaign is `--seconds` a target and seeds of
its own, each seed a different set of inputs:

    python3 scripts/hv-fuzz.py                          # the gate
    python3 scripts/hv-fuzz.py --seed 7 --seconds 300   # five minutes a target
    python3 scripts/hv-fuzz.py --target platform --target blk
    python3 scripts/hv-fuzz.py --replay platform out/hv-fuzz/findings/X.hex

Needs cargo on the host; the program has no dependencies and builds offline.
Exit code 0 = no finding; 1 = a finding; 3 = a hang.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, 'out', 'hv-fuzz')

# The gate: enough inputs that every target's deep states are reached --
# the whole machine's included -- in well under a minute.
GATE_SEED = '1'
GATE_ITERATIONS = '50000'


def main():
    args = sys.argv[1:]
    build = subprocess.run(['cargo', 'build', '--release', '--offline', '--quiet',
                            '--manifest-path', os.path.join(HERE, 'hv-fuzz', 'Cargo.toml'),
                            '--target-dir', OUT])
    if build.returncode != 0:
        print('hv-fuzz: the build failed')
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
    run = subprocess.run([os.path.join(OUT, 'release', 'hv-fuzz')] + args, cwd=findings)
    if run.returncode not in (0, 2):
        print('hv-fuzz: the inputs are in %s' % findings)
    return run.returncode


if __name__ == '__main__':
    sys.exit(main())
