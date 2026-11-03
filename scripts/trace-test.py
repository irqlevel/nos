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
import json
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
WAKEUP = re.compile(r"pid=(\d+) comm=(\S+) cpu=(\d+) deadline=([01]) waker_pid=(\d+) late_ns=([0-9.]+)us$")
BLOCK = re.compile(r"pid=(\d+) comm=(\S+) cpu=(\d+) deadline=([01]) ran_ns=([0-9.]+)us$")
MIGRATE = re.compile(r"pid=(\d+) comm=(\S+) from=(\d+) to=(\d+)$")
IPI = re.compile(r"cpu=(\d+) tasks=(\d+)$")
PREEMPT_DEFERRED = re.compile(r"pid=(\d+) comm=(\S+) cpu=(\d+) count=(\d+) idle=([01])$")
IDLE = re.compile(r"cpu=(\d+) pid=(\d+)$")
SLICE = re.compile(r"cpu=(\d+) pid=(\d+) slice_ns=([0-9.]+)us runnable=(\d+)$")
LATENCY = re.compile(
    r"trace latency: (\d+) records on (\d+) cpus -- (\d+) wakeups, (\d+) ran in the window, (\d+) not run in it")
PERCENTILES = re.compile(r"wait p50 (\S+) p90 (\S+) p99 (\S+) max (\S+)")
LONGEST = re.compile(r"^\s+(\S+) pid (\d+) (\S+)\s+cpu (\d+) -> (\d+)$")
WAIT_TIME = re.compile(r"^\d+(\.\d+)?(ns|us|ms|s)$")
DUMPED = re.compile(r'^\s*\{"t":(\d+),"cpu":(\d+),"ev":(null|"[a-z_]+")')
# `trace list`'s lines: on or off, the event, its group
LISTED = re.compile(r"^\s*(on|off)\s+([a-z_]+)\s+([a-z_]+)\s", re.M)
# `trace stat`'s rows: cpu, written, kept, dropped
STAT_ROW = re.compile(r"^\s*(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s*$", re.M)


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
            m = WAKEUP.match(fields)
            if not m:
                return f"a sched_wakeup not as its description says: {fields}"
            # Only a deadline's is late: an unblock()'s is the moment it came
            if m.group(4) == "0" and float(m.group(6)) != 0:
                return f"an unblock()'s sched_wakeup late past a deadline it has none of: {fields}"
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
            # Deferred because preemption was off: the count as found, 1 up
            if int(m.group(4)) < 1:
                return f"a reschedule deferred with preemption on: {fields}"
        elif name in ("idle_enter", "idle_exit"):
            m = IDLE.match(fields)
            if not m or int(m.group(1)) != cpu:
                return f"an {name} not as its description says: {fields}"
        elif name == "sched_slice":
            m = SLICE.match(fields)
            if not m or int(m.group(1)) != cpu:
                return f"a sched_slice not as its description says: {fields}"
    return None


# How far a block's ran_ns may be from what its records' times say: each
# time is read a few hundred ns from the clock the field was computed from
RAN_SLACK_NS = 50000


def dumped_records(reply):
    """The records `trace dump` printed, as objects, oldest first."""
    out = []
    for line in reply.splitlines():
        if DUMPED.match(line.strip()):
            out.append(json.loads(line[line.find("{"):]))
    return out


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
    reply = shell.run("trace stat")
    kept = [int(m.group(3)) for m in STAT_ROW.finditer(reply)]
    nb.check("none made yet" not in reply and len(kept) > 0 and sum(kept) > 0,
             f"trace off all keeps the rings and what they hold ({sum(kept)} records)")
    ring_cpus = {int(m.group(1)) for m in STAT_ROW.finditer(reply)}

    # `trace latency`: the window's wakeups, each paired with the switch
    # that answered it. Sched alone -- the idle events of a few CPUs fill a
    # ring in milliseconds, and the probe's wakeups would be gone with them.
    nb.check("2 events on" in shell.run("trace on sched_switch,sched_wakeup"),
             "the two events a wait is read from, on")
    shell.run("trace clear")
    shell.run("sched probe spinners=2 ms=300")
    reply = shell.run("trace latency 4000", timeout=60)
    m = LATENCY.search(reply)
    nb.check(m is not None, "trace latency says how many wakeups the window answered")
    if m:
        records, cpus, wakeups, ran, pending = (int(g) for g in m.groups())
        nb.check(records > 0 and wakeups > 0, f"the probe left {wakeups} wakeups in {records} records")
        nb.check(ran > 0, f"and {ran} of them a switch answered")
        nb.check(ran + pending <= wakeups, "no more answered and waiting than there were wakeups")
        nb.check(cpus >= 2, f"records from {cpus} cpus")
    p = PERCENTILES.search(reply)
    nb.check(p is not None, "the waits' percentiles are printed")
    if p:
        nb.check(all(WAIT_TIME.match(v) for v in p.groups()),
                 "each a time in ns, us or ms: " + " ".join(p.groups()))
    longest = [LONGEST.match(line) for line in reply.splitlines() if LONGEST.match(line)]
    nb.check(0 < len(longest) <= 5, f"{len(longest)} of the longest waits, five at most")
    nb.check(all(int(m.group(4)) in ring_cpus and int(m.group(5)) in ring_cpus for m in longest),
             f"each with the cpus it was woken on and ran on, of the {len(ring_cpus)} with rings")

    # `trace freeze wakeup>N`: armed, the rings freeze on the first wait
    # over N, and what froze them is there to read. Armed first at a second
    # -- no wakeup of this machine waits that long -- then re-armed at a
    # microsecond, which the probe's spinners make every wait clear.
    reply = shell.run("trace freeze wakeup>1s")
    nb.check("armed -- the rings freeze when a wakeup waits over 1.000s" in reply,
             "the wakeup trigger armed at a second")
    nb.check("a wakeup trigger is armed" in shell.run("trace stat"),
             "trace stat says a trigger is armed")
    shell.run("sched probe spinners=1 ms=100")
    reply = shell.run("trace stat")
    nb.check("frozen" not in reply and "a wakeup trigger is armed" in reply,
             "a second is past every wait of the machine's, and it is still armed")

    nb.check("the rings freeze when a wakeup waits over 1.000us" in shell.run("trace freeze wakeup>1us"),
             "re-armed at a microsecond")
    shell.run("sched probe spinners=2 ms=200")
    reply = shell.run("trace stat")
    nb.check("frozen" in reply and "frozen by the wakeup trigger" in reply,
             "the probe's wait froze the rings, and trace stat says what did")
    nb.check("is armed" not in reply, "and the trigger is spent")
    written = lambda text: [int(m.group(1)) for m in re.finditer(r"^\s*\d+\s+(\d+)\s+\d+\s+\d+\s*$", text, re.M)]
    before = written(reply)
    shell.run("sched probe spinners=1 ms=100")
    nb.check(written(shell.run("trace stat")) == before, "frozen, nothing more is recorded")
    nb.check("recording" in shell.run("trace thaw"), "trace thaw records again")
    nb.check("is armed" not in shell.run("trace stat"), "and the trigger is off")

    # `trace dump`: the same window as JSON, and the host decoder reading it.
    shell.run("trace clear")
    shell.run("sched probe spinners=1 ms=200")
    reply = shell.run("trace dump 20", timeout=60)
    header = [json.loads(line[line.find("{"):]) for line in reply.splitlines() if '{"window"' in line]
    nb.check(len(header) == 1 and header[0]["window"].get("records") == 20,
             "the dump's first line says what its window holds, and from when it is whole")
    lines = [line.strip() for line in reply.splitlines() if DUMPED.match(line.strip())]
    nb.check(len(lines) == 20, f"trace dump printed 20 records ({len(lines)})")
    parsed = []
    for line in lines:
        parsed.append(json.loads(line[line.find("{"):]))
    nb.check(all({"t", "cpu", "ev", "f"} <= set(r) for r in parsed),
             "each a JSON object of t, cpu, ev and its fields")
    nb.check(all(b["t"] >= a["t"] for a, b in zip(parsed, parsed[1:])), "oldest first, merged by time")
    nb.check(any(r["ev"] == "sched_switch" and "prev_comm" in r["f"] for r in parsed),
             "a switch among them, its two names and its reason there")

    dump_dir = tempfile.mkdtemp(prefix="trace-dump-")
    dump = os.path.join(dump_dir, "dump.txt")
    chrome = os.path.join(dump_dir, "chrome.json")
    try:
        with open(dump, "w") as f:
            f.write(reply)
        out = subprocess.run([sys.executable, os.path.join(HERE, "trace-decode.py"), "--chrome", chrome, dump],
                             capture_output=True, text=True)
        nb.check(out.returncode == 0,
                 "trace-decode.py read the dump" + (f": {out.stderr.strip()}" if out.returncode else ""))
        with open(chrome) as f:
            trace = json.load(f)
        slices = [e for e in trace if e.get("ph") == "X"]
        nb.check(len(slices) > 0 and all(e["dur"] >= 0 for e in slices),
                 f"Chrome trace written: {len(slices)} run slices on the cpus' tracks")
        nb.check(any(e.get("name") == "wakeup" and e.get("ph") == "i" for e in trace),
                 "and a wakeup as an instant event")
    finally:
        shutil.rmtree(dump_dir, ignore_errors=True)

    # A window's words: a group names its events, as `trace on` takes it;
    # a word that names nothing is refused, not taken as a filter that
    # matches nothing and shows an empty trace.
    groups = {m.group(2): m.group(3) for m in LISTED.finditer(shell.run("trace list"))}
    shell.run("trace off all")
    shell.run("trace on sched")
    shell.run("sched probe spinners=1 ms=100")
    recs = sched_records(shell.run("trace show 30 sched", timeout=60))
    nb.check(len(recs) > 0 and all(groups.get(n) == "sched" for _, n, _ in recs),
             f"trace show sched shows the group's events and only them ({len(recs)})")
    nb.check("no event or group called nonesuch" in shell.run("trace show 5 nonesuch"),
             "a window's word that names nothing refused")
    nb.check("a window is 1 to 4096 records" in shell.run("trace show 0"), "a window of no records refused")
    nb.check("no running cpu 63" in shell.run("trace show 5 cpu=63"), "a window of a cpu not running refused")

    # What a block says it ran is the stretch from the switch that gave the
    # task its CPU -- read from the dump, its ns whole -- not from the last
    # decision that let it keep it. `trace bench` keeps the shell's command
    # task on its CPU some 10 ms, across the tick's decisions that leave it
    # there, before it blocks for the next command.
    shell.run("trace off all")
    shell.run("trace on sched_switch,sched_block")
    shell.run("trace clear")
    shell.run("sched probe spinners=2 ms=200")
    shell.run("trace bench", timeout=120)
    dumped = dumped_records(shell.run("trace dump 150", timeout=60))
    switched_in = {}
    held, worst = 0, 0
    for r in dumped:
        if r["ev"] == "sched_switch":
            switched_in[r["cpu"]] = (r["f"]["next_pid"], r["t"])
        elif r["ev"] == "sched_block":
            got = switched_in.get(r["cpu"])
            if got and got[0] == r["f"]["pid"]:
                worst = max(worst, abs(r["f"]["ran_ns"] - (r["t"] - got[1])))
                held += 1
    nb.check(held > 0 and worst < RAN_SLACK_NS,
             f"every block's ran_ns the stretch since its switch in ({held} of them, {worst} ns off at most)")

    # A task's records in the order it lived: none switched to before its
    # sched_new, and none's sched_exit after a later task's sched_new under
    # its pid -- each written before the hand-off that lets the other side on.
    shell.run("trace off all")
    shell.run("trace on sched_new,sched_exit,sched_switch")
    shell.run("trace clear")
    shell.run("sched probe spinners=2 ms=20")
    dumped = dumped_records(shell.run("trace dump 150", timeout=60))
    news = [i for i, r in enumerate(dumped) if r["ev"] == "sched_new"]
    nb.check(len(news) > 0, f"the probe's tasks made in the window ({len(news)})")
    problem = None
    for i in news:
        pid, comm = dumped[i]["f"]["pid"], dumped[i]["f"]["comm"]
        exits = [j for j in range(i) if dumped[j]["ev"] == "sched_exit" and dumped[j]["f"]["pid"] == pid]
        since = exits[-1] + 1 if exits else 0
        if any(r["ev"] == "sched_switch" and r["f"]["next_pid"] == pid for r in dumped[since:i]):
            problem = f"pid {pid} ({comm}) switched to before its sched_new"
        later = [r for r in dumped[i + 1:] if r["ev"] in ("sched_exit", "sched_new") and r["f"]["pid"] == pid]
        if later and later[0]["ev"] == "sched_exit" and later[0]["f"]["comm"] != comm:
            problem = f"pid {pid}: {later[0]['f']['comm']}'s sched_exit after {comm}'s sched_new"
    nb.check(problem is None, "each task's records in the order it lived" + (f" ({problem})" if problem else ""))

    # An idle is over before its CPU switches to anything: every switch on a
    # CPU falls outside its idle_enter and idle_exit.
    shell.run("trace off all")
    shell.run("trace on idle sched_switch")
    shell.run("trace clear")
    shell.run("sched probe spinners=1 ms=20")
    dumped = dumped_records(shell.run("trace dump 150", timeout=60))
    inside, problem, enters = {}, None, 0
    for r in dumped:
        cpu = r["cpu"]
        if r["ev"] == "idle_enter":
            if inside.get(cpu):
                problem = f"cpu {cpu}: an idle_enter inside an idle"
            inside[cpu] = True
            enters += 1
        elif r["ev"] == "idle_exit":
            inside[cpu] = False
        elif r["ev"] == "sched_switch" and inside.get(cpu):
            problem = f"cpu {cpu}: a switch to {r['f']['next_comm']} inside an idle"
    nb.check(enters > 0 and problem is None,
             f"every switch outside its CPU's idles ({enters} idles)" + (f" ({problem})" if problem else ""))

    # `trace bench` times a ring of its own: no CPU's ring moves under it
    shell.run("trace off all")
    before = [(int(m.group(1)), int(m.group(2))) for m in STAT_ROW.finditer(shell.run("trace stat"))]
    nb.check("into a ring of its own" in shell.run("trace bench"), "trace bench times one that is on")
    after = [(int(m.group(1)), int(m.group(2))) for m in STAT_ROW.finditer(shell.run("trace stat"))]
    nb.check(before == after and len(before) > 0, "and no cpu's ring was written by it")

    # Frozen rings say so wherever a trace would look empty, and `trace free`
    # is the state before the first `trace on`: thawed.
    shell.run("trace freeze")
    nb.check("the rings are frozen" in shell.run("trace on test"), "trace on says the rings are frozen")
    nb.check("frozen -- `trace thaw` first" in shell.run("trace test 5"), "trace test refuses frozen rings")
    shell.run("trace free")
    shell.run("trace on test")
    nb.check("5 test records" in shell.run("trace test 5") and len(test_records(shell.run("trace show 5 test"))) == 5,
             "after trace free the next rings record")

    shell.run("trace off all")
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
