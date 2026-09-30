#!/usr/bin/env python3
"""cpp fuzz: what firmware, a bootloader or a disk hands the kernel's C++,
fuzzed on the host.

The kernel's C++ reads things nobody checked before it: the device tree the
arm64 boot is handed, the Multiboot2 information and the ACPI tables the x86
one is, the firmware's memory map, the command line, the GRUB environment
block `grubenv` reads off /boot, a module's .ko. What is in them is whoever
made them's to choose, and a read past an end, an index out of range or a
wrap there is a boot that dies without a word, or a page handed out that the
firmware still uses. Under all of it is the memory management, whose error
paths no boot takes.
fuzz/cpp holds a host program for each: the kernel's own sources, compiled
as they are under the address and undefined-behaviour sanitizers, over
stand-ins for the kernel around them (fuzz/cpp/common) and a host HAL in
place of each arch's inline one (fuzz/cpp/host). Each target builds its
input from random bytes -- a tree of QEMU virt's shape, a firmware map, a
block grub-editenv could have written, and each of them damaged -- and holds
what the code makes of it to a model of what its header says it does.

The targets: fdt (the device tree reader and Board::Setup), memmap (the
memory map and the free-page scan's questions of it), grubenv (the GRUB
environment block), module (the module loader), pagetable (the page tables
and the physical page allocator), heap (the kernel heap), cmdline (the
command line), multiboot (Multiboot2's tags), acpi (the ACPI tables, over
an emulated TmpMap window) and format (the kernel's printf, against the
host's). docs/testing.md says what each checks.

By default every target runs its own number of inputs from a fixed seed, the
same every time: the gate, a minute and a half. A campaign is `--seconds` a
target and seeds of its own:

    python3 scripts/cpp-fuzz.py                          # the gate
    python3 scripts/cpp-fuzz.py --seed 7 --seconds 300   # five minutes a target
    python3 scripts/cpp-fuzz.py --target fdt --keep-going
    python3 scripts/cpp-fuzz.py --replay fdt out/cpp-fuzz/findings/X.hex [--trace]

CPP_FUZZ_STATS=1 adds how often each target reached each state it counts.
Needs clang with its sanitizers on the host, and make: nothing else. Exit
code 0 = no finding; 1 = a finding; 3 = a hang.
"""

import os
import platform
import re
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FUZZ = os.path.join(ROOT, 'fuzz', 'cpp')
# The findings, whichever host found them; the programs, one host's apiece
# -- a tree shared with a container would otherwise take a Mac's for its own.
FINDINGS = os.path.join(ROOT, 'out', 'cpp-fuzz', 'findings')
OUT = os.path.join(ROOT, 'out', 'cpp-fuzz', '%s-%s' % (platform.system().lower(), platform.machine()))

# The gate's seed. How many inputs of each target it runs is the target's
# own (fuzz/cpp/targets).
GATE_SEED = '1'


def usage():
    print('usage: cpp-fuzz.py [--seed N] [--iterations N | --seconds S] [--target NAME]... [--keep-going]')
    print('       cpp-fuzz.py --replay NAME HEXFILE [--trace]')
    return 2


def symbolizer_env():
    """The sanitizers name a report's frames through llvm-symbolizer, which a
    distribution may have only under a versioned name."""
    env = dict(os.environ)
    if 'ASAN_SYMBOLIZER_PATH' not in env and shutil.which('llvm-symbolizer') is None:
        for v in range(30, 10, -1):
            path = shutil.which('llvm-symbolizer-%d' % v)
            if path:
                env['ASAN_SYMBOLIZER_PATH'] = path
                break
    return env


def main():
    args = sys.argv[1:]
    make = ['make', '-s', '-C', FUZZ, 'OUT=' + OUT]
    listed = subprocess.run(make + ['print-targets'], capture_output=True, text=True)
    if listed.returncode != 0:
        sys.stdout.write(listed.stdout + listed.stderr)
        print('cpp-fuzz: cannot list the targets')
        return 2
    all_targets = listed.stdout.split()

    only = []
    replay = None
    trace = False
    passed = []
    i = 0
    while i < len(args):
        a = args[i]
        if a == '--target' and i + 1 < len(args):
            only.append(args[i + 1])
            i += 2
        elif a == '--replay' and i + 2 < len(args):
            # The input's file as given, from where the caller is.
            replay = (args[i + 1], os.path.abspath(args[i + 2]))
            i += 3
        elif a == '--trace':
            trace = True
            i += 1
        elif a in ('--seed', '--iterations', '--seconds') and i + 1 < len(args):
            passed += [a, args[i + 1]]
            i += 2
        elif a == '--keep-going':
            passed.append(a)
            i += 1
        else:
            return usage()
    for name in only + ([replay[0]] if replay else []):
        if name not in all_targets:
            print('cpp-fuzz: no target %s (the targets: %s)' % (name, ', '.join(all_targets)))
            return 2
    targets = [replay[0]] if replay else (only or all_targets)

    build = subprocess.run(make + ['-j', str(os.cpu_count() or 4)] +
                           [os.path.join(OUT, 'bin', t) for t in targets], cwd=ROOT)
    if build.returncode != 0:
        print('cpp-fuzz: the build failed')
        return build.returncode

    findings = FINDINGS
    os.makedirs(findings, exist_ok=True)
    env = symbolizer_env()

    if replay:
        cmd = [os.path.join(OUT, 'bin', replay[0]), '--replay', replay[1]] + (['--trace'] if trace else [])
        return subprocess.run(cmd, cwd=findings, env=env).returncode

    if '--seed' not in passed:
        passed = ['--seed', GATE_SEED] + passed
    worst = 0
    total = 0
    for t in targets:
        run = subprocess.run([os.path.join(OUT, 'bin', t)] + passed, cwd=findings, env=env,
                             stdout=subprocess.PIPE, text=True)
        sys.stdout.write(run.stdout)
        sys.stdout.flush()
        m = re.search(r'^\S+\s+(\d+) inputs', run.stdout, re.M)
        if m:
            total += int(m.group(1))
        if run.returncode not in (0, 1, 3):
            print('cpp-fuzz: %s ended with exit code %d' % (t, run.returncode))
            return run.returncode
        if run.returncode == 3 or worst == 3:
            worst = 3
        elif run.returncode == 1:
            worst = 1
        if run.returncode != 0 and '--keep-going' not in passed:
            break
    if worst == 0:
        print('cpp-fuzz: %d inputs, no finding' % total)
    else:
        print('cpp-fuzz: the inputs are in %s' % findings)
    return worst


if __name__ == '__main__':
    sys.exit(main())
