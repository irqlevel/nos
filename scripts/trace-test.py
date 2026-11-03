#!/usr/bin/env python3
"""Tracepoints, end to end: what `trace` does on a running kernel.

The boot's self-test (test_tracepoints) holds the ring to its contract and
emit() to its ring, on every smoke boot. What it cannot reach is the shell's
side: a boot parameter that turns events on, records read back merged and
printed, rings made again at another size, cleared, frozen, freed, and every
word the command refuses. This boots a kernel with `tracepoints=test` and
holds each of those to what docs/tracing.md says, over the UDP shell.

Usage: scripts/trace-test.py [--arch aarch64|x86_64] [--tcg]
x86_64 needs grub-mkrescue (Linux, or the nos-builder container); aarch64
runs anywhere QEMU does. Exit code 0 when every check held.
"""

import argparse
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))

# The boot and shell plumbing is netblk-test.py's: only the command line and
# what is asserted differ.
_spec = importlib.util.spec_from_file_location("netblk_test", os.path.join(HERE, "netblk-test.py"))
nb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(nb)

# What `trace test` puts in a record's value: tracepoint.cpp's test_records.
GOLDEN = 0x9E3779B97F4A7C15
# The smallest ring: 64 KiB of 64-byte slots.
SMALL_KIB = 64
SMALL_RECORDS = SMALL_KIB * 1024 // 64

RECORD = re.compile(r"^\s*\d+\.\d{6}\s+(\d+)\s+test: seq=(\d+) value=0x([0-9A-F]+)$")


def test_records(reply):
    """The test records `trace show` printed: (cpu, seq, value) each."""
    out = []
    for line in reply.splitlines():
        m = RECORD.match(line)
        if m:
            out.append((int(m.group(1)), int(m.group(2)), int(m.group(3), 16)))
    return out


def held_in_order(records, first, count):
    seqs = [seq for _, seq, _ in records]
    values_right = all(value == (seq * GOLDEN) & (2**64 - 1) for _, seq, value in records)
    return seqs == list(range(first, first + count)) and values_right


def exercise(shell):
    reply = shell.run("trace list")
    nb.check(re.search(r"^\s*on\s+test\s", reply, re.M) is not None,
             "tracepoints=test turned the test event on as the boot ended")

    reply = shell.run("dmesg 2000 trace:")
    nb.check("events on from boot (tracepoints=test)" in reply, "the boot said so in its log")

    reply = shell.run("trace stat")
    nb.check("rings of 1024 KiB a cpu (16384 records)" in reply, "a ring of the default 1 MiB a CPU")
    cpus = len(re.findall(r"^\s*\d+\s+\d+\s+\d+\s+\d+\s*$", reply, re.M))
    nb.check(cpus >= 2, f"a ring for each of the {cpus} CPUs")

    reply = shell.run("trace test 300")
    nb.check("300 test records" in reply, "trace test recorded 300")
    records = test_records(shell.run("trace show 5 test"))
    nb.check(len(records) == 5 and held_in_order(records, 295, 5),
             "trace show 5 gives the newest five, oldest first, each as recorded")

    # The rings made again at the smallest size: what they held goes with them
    reply = shell.run(f"trace size {SMALL_KIB}")
    nb.check(f"rings of {SMALL_KIB} KiB a cpu" in reply, "trace size made the rings again, smaller")
    nb.check("nothing recorded" in shell.run("trace show"), "and what the old rings held went with them")

    # Three thousand into a ring of 1024: the newest 1024, the rest written
    # over. Shown 500 at a time at most: the UDP shell's reply is 32 KiB.
    reply = shell.run("trace test 3000")
    cpu = re.search(r"records on cpu (\d+)", reply)
    nb.check(cpu is not None, "trace test says which CPU it recorded on")
    stat = shell.run("trace stat")
    row = re.search(rf"^\s*{cpu.group(1)}\s+(\d+)\s+(\d+)\s+(\d+)\s*$", stat, re.M)
    nb.check(row is not None and int(row.group(1)) == 3000 and int(row.group(2)) == SMALL_RECORDS
             and int(row.group(3)) == 0,
             f"a ring of {SMALL_RECORDS} round three times: 3000 written, the newest {SMALL_RECORDS} kept, none dropped")
    records = test_records(shell.run("trace show 500 test", timeout=60))
    nb.check(len(records) == 500 and held_in_order(records, 2500, 500),
             "the newest 500 of them whole and in order")

    shell.run("trace clear")
    nb.check("nothing recorded" in shell.run("trace show"), "trace clear leaves nothing to show")

    shell.run("trace freeze")
    shell.run("trace test 10")
    nb.check("nothing recorded" in shell.run("trace show"), "frozen, nothing is recorded")
    shell.run("trace thaw")
    shell.run("trace test 10")
    records = test_records(shell.run("trace show 50 test"))
    nb.check(len(records) == 10 and held_in_order(records, 0, 10), "thawed, recording again")

    reply = shell.run("trace bench")
    off = re.search(r"off, ([0-9.]+) ns", reply)
    on = re.search(r"on, ([0-9.]+) ns a record", reply)
    nb.check(off is not None and on is not None, "trace bench times a tracepoint off and on")

    reply = shell.run("trace off test")
    nb.check("1 events off" in reply, "trace off test")
    nb.check("trace on test` first" in shell.run("trace test 1"), "and trace test then says so")

    nb.check("no event or group called nonesuch" in shell.run("trace on nonesuch"), "an unknown name refused")
    nb.check("a ring is 64 KiB times a power of two" in shell.run("trace size 100"), "a size not a ring's refused")
    nb.check("usage: trace" in shell.run("trace frob"), "a word the command does not take")

    shell.run("trace free")
    nb.check("none made yet" in shell.run("trace stat"), "trace free gave every ring back")
    reply = shell.run("trace on test")
    nb.check("1 events on" in reply and "rings of 64 KiB" in shell.run("trace stat"),
             "and the next trace on makes them again, at the size last set")
    shell.run("trace free")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", choices=("x86_64", "aarch64"), default="aarch64")
    parser.add_argument("--tcg", action="store_true", help="x86_64: no KVM (aarch64 always runs under TCG here)")
    args = parser.parse_args()

    if args.arch == "x86_64" and shutil.which("grub-mkrescue") is None:
        print("trace-test: x86_64 needs grub-mkrescue -- run it on Linux or in the nos-builder container")
        return 1

    nb.CMDLINE = nb.CMDLINE + " tracepoints=test"
    tmp = tempfile.mkdtemp(prefix="trace-test-")
    try:
        nb.build_images(tmp, args.arch)
        qemu, log = nb.boot(tmp, args.arch, args.tcg or not os.path.exists("/dev/kvm"))
        try:
            nb.wait_for_boot(qemu, log, 300)
            exercise(nb.Shell(nb.SHELL_PORT))
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=10)
            except subprocess.TimeoutExpired:
                qemu.kill()
            text = open(log, errors="replace").read() if os.path.exists(log) else ""
            if "PANIC:" in text:
                print("trace-test: PANIC in the serial log")
                print(text[-3000:])
                return 1
    except nb.TestFailure as failure:
        print(f"trace-test: FAILED: {failure}")
        return 1
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print(f"trace-test: OK ({args.arch})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
