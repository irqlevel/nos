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

# A scheduler record: the event's name, then its fields as name=value.
SCHED_RECORD = re.compile(r"^\s*\d+\.\d{6}\s+(\d+)\s+([a-z_]+): (.*)$")
SWITCH = re.compile(
    r"prev_pid=(\d+) prev_comm=(\S+) next_pid=(\d+) next_comm=(\S+) next_cpu=(\d+) "
    r"prev_state=(\w+) reason=(\w+)$")
WAKEUP = re.compile(r"pid=(\d+) comm=(\S+) cpu=(\d+) deadline=([01]) waker_pid=(\d+)$")
BLOCK = re.compile(r"pid=(\d+) comm=(\S+) cpu=(\d+) deadline=([01]) ran_ns=([0-9.]+)us$")
MIGRATE = re.compile(r"pid=(\d+) comm=(\S+) from=(\d+) to=(\d+)$")
IPI = re.compile(r"cpu=(\d+) tasks=(\d+)$")
PREEMPT_DEFERRED = re.compile(r"pid=(\d+) comm=(\S+) cpu=(\d+) count=(\d+) idle=([01])$")
IDLE = re.compile(r"cpu=(\d+) pid=(\d+)$")
SLICE = re.compile(r"cpu=(\d+) pid=(\d+) slice_ns=([0-9.]+)us runnable=(\d+)$")


def sched_records(reply):
    """Every scheduler record `trace show` printed: (cpu, event, fields text)."""
    out = []
    for line in reply.splitlines():
        m = SCHED_RECORD.match(line)
        if m and m.group(2) != "test":
            out.append((int(m.group(1)), m.group(2), m.group(3)))
    return out


def held_sched(records):
    """What each scheduler record must be, by its event: its fields parse,
    its pids are tasks, its reason and state are the ones the kernel has, and
    the CPU it names is the one it was recorded on -- where the event's CPU is
    the recording CPU's (a switch's next, an IPI's, a slice's, the CPU whose
    switch moved a task, an idle's)."""
    states = {"runnable", "blocked", "sleeping", "exited"}
    reasons = {"voluntary", "yield", "preempt"}
    for cpu, name, fields in records:
        if name == "sched_switch":
            m = SWITCH.match(fields)
            if not m or m.group(6) not in states or m.group(7) not in reasons:
                return f"a sched_switch not as its description says: {fields}"
            if int(m.group(5)) != cpu:
                return f"a sched_switch on cpu {cpu} saying next_cpu {m.group(5)}"
            if m.group(2) == "idle0" and m.group(4) == "idle0":
                return f"a switch from a task to itself: {fields}"
        elif name == "sched_wakeup":
            if not WAKEUP.match(fields):
                return f"a sched_wakeup not as its description says: {fields}"
        elif name == "sched_block":
            m = BLOCK.match(fields)
            if not m or float(m.group(5)) < 0:
                return f"a sched_block not as its description says: {fields}"
        elif name == "sched_migrate":
            m = MIGRATE.match(fields)
            if not m or m.group(3) == m.group(4):
                return f"a sched_migrate not as its description says: {fields}"
            # Moved by the switch on the queue it was displaced on
            if int(m.group(3)) != cpu:
                return f"a sched_migrate on cpu {cpu} saying from {m.group(3)}"
        elif name == "sched_ipi":
            m = IPI.match(fields)
            if not m or int(m.group(1)) != cpu:
                return f"a sched_ipi not as its description says: {fields}"
        elif name == "sched_preempt_deferred":
            m = PREEMPT_DEFERRED.match(fields)
            if not m or int(m.group(3)) != cpu:
                return f"a sched_preempt_deferred not as its description says: {fields}"
        elif name in ("idle_enter", "idle_exit"):
            m = IDLE.match(fields)
            if not m or int(m.group(1)) != cpu:
                return f"an {name} not as its description says: {fields}"
        elif name == "sched_slice":
            m = SLICE.match(fields)
            if not m or int(m.group(1)) != cpu:
                return f"a sched_slice not as its description says: {fields}"
    return None


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

    # The scheduler's own events: a boot with a shell, a probe and an idle
    # CPU makes them, each record as its description says. sched_migrate is
    # the exception -- the balancer moves a displaced task, and a probe's are
    # pinned -- so it is listed, and held to its description should it appear.
    reply = shell.run("trace list")
    for name in ("sched_switch", "sched_wakeup", "sched_block", "sched_migrate", "sched_new",
                 "sched_exit", "sched_ipi", "sched_preempt_deferred", "idle_enter", "idle_exit",
                 "sched_slice"):
        nb.check(re.search(rf"^\s*off\s+{name}\s", reply, re.M) is not None, f"the {name} event listed")

    # Rings of 1 MiB again: a CPU with nothing to run writes its two idle
    # events every few microseconds, so the smallest ring is a few
    # milliseconds of the machine, and what a probe left would be gone
    # before it could be read.
    nb.check("rings of 1024 KiB a cpu" in shell.run("trace size 1024"), "rings of 1 MiB again")

    # A task's own records, alone: with the rest on, a ring holds a
    # millisecond of a busy CPU and these are the first of a probe's.
    nb.check("2 events on" in shell.run("trace on sched_new,sched_exit"),
             "trace on names two events, comma-separated")
    shell.run("sched probe spinners=2 ms=300")
    for name in ("sched_new", "sched_exit"):
        reply = shell.run(f"trace show 3 {name}", timeout=60)
        got = [1 for _, n, _ in sched_records(reply) if n == name]
        nb.check(len(got) == 3, f"the probe left records of {name}")
        problem = held_sched(sched_records(reply))
        nb.check(problem is None, f"every {name} record as its description says"
                 + (f" ({problem})" if problem else ""))
    shell.run("trace off all")

    # The decision events and the two the idle loop makes.
    nb.check("8 events on" in shell.run("trace on sched"), "trace on sched turns the decision events on")
    nb.check("2 events on" in shell.run("trace on idle"), "trace on idle the two the idle loop makes")

    shell.run("sched probe spinners=2 ms=400")

    records = sched_records(shell.run("trace show 200", timeout=60))
    names = {name for _, name, _ in records}
    for name in ("sched_switch", "sched_wakeup", "sched_block", "idle_enter", "idle_exit"):
        nb.check(name in names, f"the probe left {name} records")
    problem = held_sched(records)
    nb.check(problem is None, "every scheduler record as its description says" + (f" ({problem})" if problem else ""))

    # A switch's two names are names a task has: not empty, at most the
    # field's 12 bytes, and the idle ones are the CPUs' own -- `ps` has them.
    switches = [(cpu, fields) for cpu, name, fields in records if name == "sched_switch"]
    nb.check(len(switches) > 0, "sched_switch records read back")
    names_seen = set()
    for _, fields in switches:
        m = SWITCH.match(fields)
        names_seen.add(m.group(2))
        names_seen.add(m.group(4))
    nb.check(all(0 < len(n) <= 12 for n in names_seen),
             "every name a switch records is one of at most the field's 12 bytes")
    reply = shell.run("ps")
    idles = set(re.findall(r"^\d+ \d+ 0x[0-9A-F]+ [\d.]+ \d+ (idle\d+)$", reply, re.M))
    nb.check(bool(idles) and bool(idles & names_seen),
             "the idle tasks `ps` has are among the names the switches recorded")
    nb.check(any(n.startswith("probe/") for n in names_seen),
             "and the probe's own tasks are among them")

    # The debug group's, on its own: a record a decision each, so it would
    # fill a ring while the others' were being read.
    shell.run("trace off all")
    nb.check("1 events on" in shell.run("trace on debug"), "trace on debug the slice event")
    shell.run("sched probe spinners=2 ms=300")
    reply = shell.run("trace show 4 sched_slice", timeout=60)
    nb.check(len([1 for _, n, _ in sched_records(reply) if n == "sched_slice"]) == 4,
             "the probe left the debug group's sched_slice records")
    nb.check("1 events off" in shell.run("trace off debug"), "trace off debug")

    shell.run("trace off all")
    nb.check("no rings" not in shell.run("trace stat"), "trace off all keeps the rings")


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
