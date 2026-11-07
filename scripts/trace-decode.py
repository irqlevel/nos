#!/usr/bin/env python3
"""trace-decode: a trace dump from the kernel, read on the host.

`trace dump [n] [cpu=N] [event...]` prints the newest records of every CPU's
ring as JSON, a record a line. This reads them back -- from a file, a saved
serial log, a netconsole capture -- and says what they hold: how long a
woken task waited for its CPU, which task ran on each CPU and for how long,
how many switches there were. Or it writes them out again for another tool:
a normalised JSON array, or Chrome's trace format (chrome://tracing,
Perfetto), a track a CPU of the tasks that ran on it, wakeups and blocks as
instant events on their own.

The pairing is the kernel's `trace latency`: a `sched_wakeup` and the first
`sched_switch` after it whose next task is the woken one, a deadline's
wakeup counted from its deadline (`late_ns`). It starts where the dump's
first line -- `{"window": ...}` -- says every CPU's part of it is whole: a
CPU whose ring wrapped holds nothing older, and a wakeup from before then
could have its switch among what that ring lost. A wakeup whose switch is
not in the dump is counted, not guessed.

The hypervisor's records (`trace on hv`) and a guest disk's (`trace on blk
disk fs`) are read too, with `--hv` for them alone:

- every exit by its reason, and by what it was about -- the port, the MSR,
  the guest's page, the vector -- with what it cost the host: an exit's
  time out of the guest is the `out_ns` of the vCPU's next record, from
  that exit to the entry after it (a HLT's is the halt's sleep, idle);
- each vCPU's time in its guest, and how its halts ended: polled, rung, its
  time come, advanced; how late its timer's interrupts were asked for, and
  by what; what went in at its entries;
- a ring to a vCPU and its next entry after it: how long a guest CPU an IPI
  or a device's interrupt is for takes to run again;
- a guest disk's requests, each taken from the virtio disk to the device
  and back -- paired by its disk, slot, op, offset and length, which the
  device's records and the disk task's both carry -- in four stretches:
  the disk's task waking for it (`blk_submit` to `disk_start`), the
  image's work (to `disk_done`), the device's share of that (`bio_submit`
  to `bio_done`, its buffer the request's slot), and the vCPU's round
  coming for it (to `blk_complete`); the image's synchronous path and its
  conversions, ext4's commits.

Usage: scripts/trace-decode.py [--chrome FILE] [--json FILE] [--latency]
                               [--hv] [--cpu N] [FILE...]
       scripts/trace-decode.py --selftest

Reads stdin when no file is named. Exit code 0 when it read at least one
record, 1 when it found none. `--selftest` holds the pairing to a window it
makes up, whose answers are known -- what no live gate reaches among them
-- and exits 1 on the first that comes out otherwise.
"""

import argparse
import bisect
import json
import sys

# Chrome's clock is microseconds, and its tracks are named by a process id:
# one per CPU, past the pids a kernel task has; one per vCPU, by its task's
# pid; one per guest disk.
CHROME_PID_BASE = 1000
CHROME_VCPU_BASE = 100000
CHROME_DISK_BASE = 200000


def objects(lines):
    """Every JSON object the lines hold, one a line."""
    for line in lines:
        at = line.find("{")
        if at < 0:
            continue
        try:
            o = json.loads(line[at:])
        except ValueError:
            continue
        if isinstance(o, dict):
            yield o


def records(lines):
    """The dump's records, oldest first."""
    out = [o for o in objects(lines) if "t" in o and "cpu" in o and o.get("ev") is not None]
    out.sort(key=lambda r: (r["t"], r["cpu"]))
    return out


def window_start(lines):
    """From where on every CPU's part of the dump is whole, in ns: the
    latest its `window` lines say -- a dump in parts has one a part -- 0
    when none was cut, None when a part could not be read at all."""
    start = 0
    for o in objects(lines):
        w = o.get("window")
        if not isinstance(w, dict):
            continue
        if w.get("complete_from") is None:
            return None
        start = max(start, w["complete_from"])
    return start


def latencies(recs, start):
    """Every wakeup a switch answered, from `start` on: (ns, pid, comm,
    woken_cpu, ran_cpu) each, and how many were answered, are still
    waiting, and were woken again before any switch to them."""
    waiting = {}
    waits = []
    superseded = 0
    for r in recs:
        if r["t"] < start:
            continue
        f = r.get("f", {})
        if r["ev"] == "sched_wakeup":
            pid = f.get("pid")
            if pid is None:
                continue
            # Woken again with no switch between: the first one's switch is
            # not in the dump, and this is the one the next switch answers
            if pid in waiting:
                superseded += 1
            waiting[pid] = (r["t"] - f.get("late_ns", 0), f.get("cpu", r["cpu"]))
        elif r["ev"] == "sched_switch":
            pid = f.get("next_pid")
            if pid in waiting:
                t0, woken_cpu = waiting.pop(pid)
                waits.append((r["t"] - t0, pid, f.get("next_comm", "?"), woken_cpu, r["cpu"]))
    return waits, len(waits), len(waiting), superseded


def percentile(sorted_values, p):
    if not sorted_values:
        return 0
    return sorted_values[(len(sorted_values) - 1) * p // 100]


def ns_text(ns):
    if ns < 1000:
        return f"{ns}ns"
    if ns < 1000000:
        return f"{ns / 1000:.1f}us"
    return f"{ns / 1000000:.2f}ms"


def spans(recs):
    """Per CPU, the tasks that ran: {cpu: [(pid, comm, ns), ...]} -- from a
    switch in to the switch out that follows it, the first record opening
    the one it caught mid-run."""
    open_at = {}
    out = {}
    for r in recs:
        if r["ev"] != "sched_switch":
            continue
        f = r.get("f", {})
        cpu = r["cpu"]
        prev = open_at.pop(cpu, None)
        if prev is not None:
            pid, comm, t0 = prev
            out.setdefault(cpu, []).append((pid, comm, r["t"] - t0))
        open_at[cpu] = (f.get("next_pid", 0), f.get("next_comm", "?"), r["t"])
    return out


def summarize(recs, start):
    print(f"trace-decode: {len(recs)} records over {len({r['cpu'] for r in recs})} cpus, "
          f"{ns_text(recs[0]['t'])} to {ns_text(recs[-1]['t'])}")
    if start is None:
        print("  a cpu's part could not be read: no wait is paired across it")
    elif start > recs[0]["t"]:
        print(f"  every cpu's part whole from {ns_text(start)}: waits paired from there")

    switches = [r for r in recs if r["ev"] == "sched_switch"]
    print(f"  {len(switches)} switches, {sum(1 for r in recs if r['ev'] == 'sched_wakeup')} wakeups, "
          f"{sum(1 for r in recs if r['ev'] == 'sched_block')} blocks, "
          f"{sum(1 for r in recs if r['ev'] in ('idle_enter', 'idle_exit'))} idles")

    ran = spans(recs)
    for cpu in sorted(ran):
        busy = {}
        for pid, comm, dur in ran[cpu]:
            busy[(pid, comm)] = busy.get((pid, comm), 0) + dur
        top = sorted(busy.items(), key=lambda kv: -kv[1])[:3]
        text = ", ".join(f"{comm} {ns_text(dur)}" for (pid, comm), dur in top)
        print(f"  cpu {cpu}: {len(ran[cpu])} ran spans -- {text}")

    if start is None:
        return
    waits, answered, pending, superseded = latencies(recs, start)
    if answered:
        waits_ns = sorted(w[0] for w in waits)
        print(f"  wait: {answered} wakeups answered, {pending} not run in the window -- "
              f"p50 {ns_text(percentile(waits_ns, 50))} p90 {ns_text(percentile(waits_ns, 90))} "
              f"p99 {ns_text(percentile(waits_ns, 99))} max {ns_text(waits_ns[-1])}")
        for ns, pid, comm, woken_cpu, run_cpu in sorted(waits, reverse=True)[:5]:
            print(f"    {ns_text(ns):>9} pid {pid} {comm} (cpu {woken_cpu} -> {run_cpu})")
    elif pending:
        print(f"  wait: no wakeup of the window was followed by its switch ({pending} still waiting)")
    if superseded:
        print(f"  {superseded} wakeups were followed by another of their task's before any switch to it")


# ---- the hypervisor's records (group hv) ----

# A PC's ports, as hv's guest has them (hypervisor/hv/src/run.rs, hypervisor/hv/src/devices):
# (first, last, name).
PORTS = [
    (0x20, 0x21, "pic"), (0xA0, 0xA1, "pic"), (0x40, 0x43, "pit"), (0x61, 0x61, "port61"),
    (0x70, 0x71, "rtc"), (0xE9, 0xE9, "debug"), (0x3F8, 0x3FF, "com1"), (0x505, 0x505, "pvpanic"),
    (0x600, 0x63F, "acpi pm"), (0xCF8, 0xCFF, "pci config"),
]
# The virtio devices' BARs: the disks' from 0xC000, 0x80 each, four of
# them; the NICs' after, 0x40 each. Their queue's notify is at 0x10.
DISK_IO_BASE, DISK_BAR, MAX_DISKS = 0xC000, 0x80, 4
NIC_IO_BASE, NIC_BAR = DISK_IO_BASE + MAX_DISKS * DISK_BAR, 0x40
VIRTIO_NOTIFY = 0x10

MSRS = {
    0x10: "tsc", 0x1B: "apic_base", 0x48: "spec_ctrl", 0x49: "pred_cmd", 0x6E0: "tsc_deadline",
    0x802: "x2apic id", 0x808: "x2apic tpr", 0x80B: "x2apic eoi", 0x80D: "x2apic ldr",
    0x80F: "x2apic sivr", 0x830: "x2apic icr", 0x832: "x2apic lvt timer", 0x838: "x2apic tmict",
    0x839: "x2apic tmcct", 0x83E: "x2apic tdcr", 0x83F: "x2apic self ipi",
    0xC0000080: "efer", 0xC0000100: "fs_base", 0xC0000101: "gs_base", 0xC0000102: "kernel_gs_base",
    0xC0000103: "tsc_aux", 0x4B564D01: "kvm system_time", 0x4B564D02: "kvm async_pf",
    0x4B564D03: "kvm steal_time", 0x4B564D04: "kvm pv_eoi",
}
EXCEPTIONS = {1: "#db", 3: "#bp", 6: "#ud", 13: "#gp", 14: "#pf", 17: "#ac", 18: "#mc"}
PAGES = {0xFEE00000: "xapic", 0xFEC00000: "ioapic"}
PAGE = 4096


def port_name(port):
    for first, last, name in PORTS:
        if first <= port <= last:
            return name
    if DISK_IO_BASE <= port < NIC_IO_BASE:
        disk, at = divmod(port - DISK_IO_BASE, DISK_BAR)
        what = "notify" if at == VIRTIO_NOTIFY else f"+{at:#x}"
        return f"vd{chr(ord('a') + disk)} {what}"
    if NIC_IO_BASE <= port < NIC_IO_BASE + 8 * NIC_BAR:
        nic, at = divmod(port - NIC_IO_BASE, NIC_BAR)
        what = "notify" if at == VIRTIO_NOTIFY else f"+{at:#x}"
        return f"eth{nic} {what}"
    return f"port {port:#x}"


def detail_name(reason, f):
    """What an exit was about, as a line of the table names it."""
    detail, info = f.get("detail", 0), f.get("info", 0)
    if reason == "io":
        return f"{port_name(detail)} {'in' if info & 0x100 else 'out'}"
    if reason == "msr":
        return f"{MSRS.get(detail, f'msr {detail:#x}')} {'write' if info & 1 else 'read'}"
    if reason == "npf":
        page = detail & ~(PAGE - 1)
        return PAGES.get(page, f"page {page:#x}")
    if reason == "exception":
        return EXCEPTIONS.get(detail, f"vector {detail}")
    if reason == "host":
        return f"vector {detail:#x}" if detail else "an interrupt"
    if reason == "cpuid":
        return f"leaf {detail:#x}"
    if reason == "eoi":
        return f"vector {detail:#x}"
    if reason in ("cr0", "cr4", "cr8", "dr"):
        return "write" if info & 1 else "read"
    return ""


def exit_times(r):
    """An hv_exit's exit and the entry it ended, in ns: its record's time
    less the stretch from the exit to the record, and less its time in the
    guest; None for an entry turned back, which never ran."""
    f = r.get("f", {})
    if f.get("reason") == "kicked":
        return None
    exited = r["t"] - f.get("since_ns", 0)
    return exited - f.get("guest_ns", 0), exited


def by_vcpu(recs, start, events):
    """Each vCPU's records of `events`, oldest first, from `start` on: by
    its task's pid, which tells one VM's CPU 0 from another's."""
    out = {}
    for r in recs:
        if r["ev"] in events and r["t"] >= start:
            out.setdefault(r.get("f", {}).get("pid"), []).append(r)
    return out


def hv_exits(recs, start):
    """Every exit by its reason and by what it was about, with what it cost
    the host -- the `out_ns` of the vCPU's next entry -- and each vCPU's
    time in its guest. From `start` on: a vCPU's records before then may
    have lost one between two of them."""
    reasons = {}
    details = {}
    vcpus = {}
    for pid, rs in by_vcpu(recs, start, ("hv_exit",)).items():
        v = vcpus.setdefault(pid, {"vcpu": rs[0]["f"].get("vcpu"), "exits": 0, "guest_ns": 0,
                                   "first": None, "last": None, "kicked": 0})
        last = None
        for r in rs:
            f = r["f"]
            reason = f.get("reason", "?")
            v["exits"] += 1
            if reason == "kicked":
                v["kicked"] += 1
                continue
            times = exit_times(r)
            v["guest_ns"] += f.get("guest_ns", 0)
            if v["first"] is None:
                v["first"] = times[0]
            v["last"] = times[1]
            row = reasons.setdefault(reason, {"n": 0, "guest": [], "out": [], "since": []})
            row["n"] += 1
            row["guest"].append(f.get("guest_ns", 0))
            row["since"].append(f.get("since_ns", 0))
            key = (reason, detail_name(reason, f))
            drow = details.setdefault(key, {"n": 0, "out": []})
            drow["n"] += 1
            # What the exit before this entry cost: its reason's
            if last is not None and f.get("out_ns", 0) > 0:
                lreason, lkey = last
                reasons[lreason]["out"].append(f["out_ns"])
                details[lkey]["out"].append(f["out_ns"])
            last = (reason, key)
    return reasons, details, vcpus


def hv_wakes(recs, start):
    """How each vCPU's halts ended, how late its timer was, what went in at
    its entries, and how long after a ring the vCPU rung was in its guest
    again -- paired where its number names one vCPU's task alone."""
    halts = {"how": {}, "halted": [], "late": []}
    timers = {}
    injects = {}
    for r in recs:
        if r["t"] < start:
            continue
        f = r.get("f", {})
        if r["ev"] == "hv_wake":
            halts["how"][f.get("how", "?")] = halts["how"].get(f.get("how", "?"), 0) + 1
            halts["halted"].append(f.get("halted_ns", 0))
            halts["late"].append(f.get("late_ns", 0))
        elif r["ev"] == "hv_timer":
            timers.setdefault(f.get("how", "?"), []).append(f.get("late_ns", 0))
        elif r["ev"] == "hv_inject":
            key = (f.get("kind", "?"), f.get("vector", 0))
            injects[key] = injects.get(key, 0) + 1

    # A vCPU's number to its task: from its exits, where one task alone has it.
    pids = {}
    for r in recs:
        if r["ev"] == "hv_exit":
            f = r.get("f", {})
            pids.setdefault(f.get("vcpu"), set()).add(f.get("pid"))
    entries = {}
    for pid, rs in by_vcpu(recs, start, ("hv_exit",)).items():
        entries[pid] = sorted(t[0] for t in map(exit_times, rs) if t is not None)
    rings = {}
    unpaired = 0
    for r in recs:
        if r["ev"] != "hv_ring" or r["t"] < start:
            continue
        f = r.get("f", {})
        target = pids.get(f.get("vcpu"), set())
        row = rings.setdefault(f.get("how", "?"), {"n": 0, "to_entry": []})
        row["n"] += 1
        if f.get("how") == "notified":
            # A posted interrupt, taken by the CPU in the guest it was in:
            # no entry is what delivers it, so none is paired with it.
            continue
        if len(target) != 1:
            unpaired += 1
            continue
        times = entries.get(next(iter(target)), [])
        at = bisect.bisect_left(times, r["t"])
        if at < len(times):
            row["to_entry"].append(times[at] - r["t"])
        else:
            unpaired += 1
    return halts, timers, injects, rings, unpaired


# ---- a guest disk's records (groups blk, disk, fs) ----

def disk_requests(recs, start):
    """Each request a guest's disk took, through its hand-offs: what a
    record of each stage says of it -- its disk, slot, op, offset and length
    on the device's and the disk task's records alike -- pairs it, oldest
    first; a stage whose request is not in the window is counted."""
    requests = []
    pending = {}     # (disk, slot) -> requests submitted, not yet picked up
    serving = {}     # (disk task pid, slot) -> the request it serves
    done = {}        # (disk, slot) -> requests done in the task, not yet back
    commands = {}    # (pid, entry, buffer) -> bio_submit times, oldest first
    device = []
    stray = 0
    syncs, notes, commits = [], [], []
    for r in recs:
        if r["t"] < start:
            continue
        f = r.get("f", {})
        ev, t = r["ev"], r["t"]
        same = lambda q: q["op"] == f.get("op") and q["offset"] == f.get("offset") and q["len"] == f.get("len")
        if ev == "blk_submit":
            q = {"disk": f.get("disk"), "slot": f.get("slot"), "op": f.get("op"), "offset": f.get("offset"),
                 "len": f.get("len"), "submit": t, "start": None, "done": None, "complete": None,
                 "device": 0, "commands": 0, "dev_from": None, "dev_to": None, "how": None, "ok": None}
            requests.append(q)
            pending.setdefault((q["disk"], q["slot"]), []).append(q)
        elif ev == "disk_start":
            queue = pending.get((f.get("disk"), f.get("slot")), [])
            q = next((q for q in queue if same(q)), None)
            if q is None:
                stray += 1
                continue
            queue.remove(q)
            q["start"], q["task"] = t, f.get("pid")
            serving[(f.get("pid"), f.get("slot"))] = q
        elif ev == "disk_done":
            q = serving.pop((f.get("pid"), f.get("slot")), None)
            if q is None:
                stray += 1
                continue
            q["done"], q["how"], q["ok"] = t, f.get("how"), f.get("ok")
            done.setdefault((q["disk"], q["slot"]), []).append(q)
        elif ev == "blk_complete":
            queue = done.get((f.get("disk"), f.get("slot")), [])
            q = next((q for q in queue if same(q)), None)
            if q is None:
                stray += 1
                continue
            queue.remove(q)
            q["complete"] = t
        elif ev == "bio_submit":
            commands.setdefault((f.get("pid"), f.get("entry"), f.get("buffer")), []).append((t, f.get("in_flight", 0)))
        elif ev == "bio_done":
            # Its submit's: by its queue's entry and buffer, the oldest of them
            key = next((k for k in commands if k[1] == f.get("entry") and k[2] == f.get("buffer") and commands[k]),
                       None)
            if key is None:
                stray += 1
                continue
            t0, in_flight = commands[key].pop(0)
            if not f.get("refused"):
                device.append((t - t0, in_flight))
                # The image's buffer is the request's slot: its device time,
                # from its first command handed over to its last one back
                q = serving.get((key[0], key[2]))
                if q is not None:
                    q["dev_from"] = t0 if q["dev_from"] is None else min(q["dev_from"], t0)
                    q["dev_to"] = t if q["dev_to"] is None else max(q["dev_to"], t)
                    q["device"] = q["dev_to"] - q["dev_from"]
                    q["commands"] += 1
        elif ev == "image_sync":
            syncs.append((f.get("ns", 0), f.get("write", 0)))
        elif ev == "image_note":
            notes.append((f.get("ns", 0), f.get("blocks", 0)))
        elif ev == "ext4_commit":
            commits.append((f.get("ns", 0), f.get("blocks", 0), f.get("checkpoint_ns", 0), f.get("ok", 0)))
    return {"requests": requests, "device": device, "stray": stray, "syncs": syncs, "notes": notes,
            "commits": commits}


def spread(values):
    """p50, p99 and the most of `values`, as text."""
    v = sorted(values)
    if not v:
        return "none"
    return f"p50 {ns_text(percentile(v, 50))} p99 {ns_text(percentile(v, 99))} max {ns_text(v[-1])}"


def hv_summary(recs, start):
    """The hypervisor's and a guest disk's records, said: what `--hv` prints,
    and the summary after the scheduler's when there are any."""
    if start is None:
        print("  a cpu's part could not be read: no exit, ring or request is paired across it")
        return
    reasons, details, vcpus = hv_exits(recs, start)
    if vcpus:
        total = sum(v["exits"] for v in vcpus.values())
        print(f"hv: {total} exits of {len(vcpus)} vCPUs")
        for pid, v in sorted(vcpus.items(), key=lambda kv: (kv[1]["vcpu"] or 0, kv[0] or 0)):
            span = (v["last"] or 0) - (v["first"] or 0)
            share = f", {100.0 * v['guest_ns'] / span:.1f}% of it in its guest" if span > 0 else ""
            kicked = f", {v['kicked']} entries turned back" if v["kicked"] else ""
            print(f"  vcpu {v['vcpu']} (pid {pid}): {v['exits']} exits over {ns_text(span)}{share}{kicked}")
        print(f"  {'reason':<14}{'exits':>8}{'share':>8}   out of the guest after it (the host's cost)")
        for reason, row in sorted(reasons.items(), key=lambda kv: -kv[1]["n"]):
            out = spread(row["out"])
            idle = " -- a halt's sleep, idle" if reason == "hlt" else ""
            print(f"  {reason:<14}{row['n']:>8}{100.0 * row['n'] / max(total, 1):>7.1f}%   {out}{idle}")
            subs = sorted(((k[1], d) for k, d in details.items() if k[0] == reason and k[1]),
                          key=lambda kd: -kd[1]["n"])[:5]
            for name, d in subs:
                print(f"      {name:<26}{d['n']:>7}   {spread(d['out'])}")
        host = reasons.get("host")
        if host and host["since"]:
            print(f"  an interrupt of the host's, from the exit to its record: {spread(host['since'])}")

    halts, timers, injects, rings, unpaired = hv_wakes(recs, start)
    if halts["halted"]:
        how = ", ".join(f"{n} {h}" for h, n in sorted(halts["how"].items(), key=lambda kv: -kv[1]))
        print(f"  halts: {len(halts['halted'])} ({how}); halted {spread(halts['halted'])}; "
              f"woke past its time {spread(halts['late'])}")
    for how, lates in sorted(timers.items(), key=lambda kv: -len(kv[1])):
        print(f"  timer interrupts after {how}: {len(lates)}, late {spread(lates)}")
    if injects:
        text = ", ".join(f"{kind} {vector:#x} x{n}" for (kind, vector), n in
                         sorted(injects.items(), key=lambda kv: -kv[1])[:8])
        print(f"  injected: {text}")
    for how, row in sorted(rings.items(), key=lambda kv: -kv[1]["n"]):
        if how == "notified":
            print(f"  rings notified: {row['n']}, posted to a vCPU in its guest and taken there, no entry needed")
        else:
            print(f"  rings {how}: {row['n']}, to the vCPU's next entry {spread(row['to_entry'])}")
    if unpaired:
        print(f"  {unpaired} rings not paired: their vCPU's number more than one task's, or no entry after")

    d = disk_requests(recs, start)
    whole = [q for q in d["requests"] if q["complete"] is not None and q["start"] is not None]
    if d["requests"]:
        print(f"disk: {len(d['requests'])} requests, {len(whole)} of them whole in the window")
        for op in sorted({q["op"] for q in whole}):
            qs = [q for q in whole if q["op"] == op]
            print(f"  {op:<13}{len(qs):>6}   in all {spread([q['complete'] - q['submit'] for q in qs])}")
            print(f"      {'to its task':<14}{spread([q['start'] - q['submit'] for q in qs])}")
            print(f"      {'served':<14}{spread([q['done'] - q['start'] for q in qs])}")
            dev = [q["device"] for q in qs if q["commands"]]
            if dev:
                print(f"      {'by the device':<14}{spread(dev)} ({len(dev)} of them through it)")
            print(f"      {'back':<14}{spread([q['complete'] - q['done'] for q in qs])}")
            how = {}
            for q in qs:
                how[q["how"]] = how.get(q["how"], 0) + 1
            print(f"      served: " + ", ".join(f"{n} {h}" for h, n in sorted(how.items(), key=lambda kv: -kv[1])))
    if d["device"]:
        print(f"  device commands: {len(d['device'])}, {spread([ns for ns, _ in d['device']])}, "
              f"at most {max(n for _, n in d['device'])} in flight")
    if d["syncs"]:
        writes = sum(1 for _, w in d["syncs"] if w)
        print(f"  image served then and there: {len(d['syncs'])} ({writes} writes), {spread([ns for ns, _ in d['syncs']])}")
    if d["notes"]:
        print(f"  image's unwritten made written: {len(d['notes'])}, {sum(b for _, b in d['notes'])} blocks, "
              f"{spread([ns for ns, _ in d['notes']])}")
    if d["commits"]:
        failed = sum(1 for c in d["commits"] if not c[3])
        print(f"  ext4 commits: {len(d['commits'])}{f' ({failed} failed)' if failed else ''}, "
              f"{sum(c[1] for c in d['commits'])} journal blocks, {spread([c[0] for c in d['commits']])}, "
              f"checkpoints {ns_text(sum(c[2] for c in d['commits']))} in all")
    if d["stray"]:
        print(f"  {d['stray']} records whose request is not in the window")


def chrome(recs, path):
    """Chrome's trace format: a slice a (switch in, switch out) pair, an
    instant event for a wakeup, a block or a migration; and for the
    hypervisor a track a vCPU, its time in the guest and each exit's out of
    it, and a guest disk's requests as slices of their own."""
    events = []
    open_at = {}
    for r in recs:
        cpu = r["cpu"]
        f = r.get("f", {})
        pid = CHROME_PID_BASE + cpu
        ts = r["t"] / 1000.0
        if r["ev"] == "sched_switch":
            prev = open_at.pop(cpu, None)
            if prev is not None:
                name, t0, task_pid = prev
                events.append({"name": name, "cat": "sched", "ph": "X", "ts": t0 / 1000.0,
                               "dur": ts - t0 / 1000.0, "pid": pid, "tid": pid,
                               "args": {"pid": task_pid}})
            open_at[cpu] = (f.get("next_comm", "?"), r["t"], f.get("next_pid", 0))
        elif r["ev"] == "sched_wakeup":
            events.append({"name": "wakeup", "cat": "sched", "ph": "i", "s": "t", "ts": ts,
                           "pid": pid, "tid": pid,
                           "args": {"pid": f.get("pid"), "comm": f.get("comm"),
                                    "waker": f.get("waker_pid"), "deadline": f.get("deadline"),
                                    "late_ns": f.get("late_ns")}})
        elif r["ev"] == "sched_block":
            events.append({"name": "block", "cat": "sched", "ph": "i", "s": "t", "ts": ts,
                           "pid": pid, "tid": pid,
                           "args": {"pid": f.get("pid"), "comm": f.get("comm"),
                                    "ran_ns": f.get("ran_ns"), "deadline": f.get("deadline")}})
        elif r["ev"] == "sched_migrate":
            events.append({"name": "migrate", "cat": "sched", "ph": "i", "s": "t", "ts": ts,
                           "pid": pid, "tid": pid,
                           "args": {"pid": f.get("pid"), "from": f.get("from"), "to": f.get("to")}})

    for cpu in sorted({r["cpu"] for r in recs}):
        pid = CHROME_PID_BASE + cpu
        events.insert(0, {"name": "process_name", "ph": "M", "pid": pid, "tid": pid,
                          "args": {"name": f"cpu {cpu}"}})

    # A vCPU a track, by its task's pid: its guest's stretches, and each
    # exit's out of the guest, named by what it was.
    for vpid, rs in by_vcpu(recs, 0, ("hv_exit",)).items():
        track = CHROME_VCPU_BASE + (vpid or 0)
        events.append({"name": "process_name", "ph": "M", "pid": track, "tid": track,
                       "args": {"name": f"vcpu {rs[0]['f'].get('vcpu')} (pid {vpid})"}})
        last = None
        for r in rs:
            times = exit_times(r)
            if times is None:
                continue
            entered, exited = times
            f = r["f"]
            if last is not None:
                events.append({"name": last[1], "cat": "hv", "ph": "X", "ts": last[0] / 1000.0,
                               "dur": max(entered - last[0], 0) / 1000.0, "pid": track, "tid": track})
            events.append({"name": "guest", "cat": "hv", "ph": "X", "ts": entered / 1000.0,
                           "dur": f.get("guest_ns", 0) / 1000.0, "pid": track, "tid": track})
            name = f.get("reason", "?")
            detail = detail_name(name, f)
            last = (exited, f"{name} {detail}".strip())
    # A guest disk's requests: a slice each, on its disk's track.
    for n, q in enumerate(disk_requests(recs, 0)["requests"]):
        if q["complete"] is None:
            continue
        track = CHROME_DISK_BASE + (q["disk"] or 0)
        events.append({"name": f"{q['op']} {q['len']} at {q['offset']:#x}", "cat": "disk", "ph": "X",
                       "ts": q["submit"] / 1000.0, "dur": (q["complete"] - q["submit"]) / 1000.0,
                       "pid": track, "tid": track + (q["slot"] or 0),
                       "args": {"slot": q["slot"], "how": q["how"], "ok": q["ok"],
                                "to_task_ns": None if q["start"] is None else q["start"] - q["submit"],
                                "served_ns": None if q["done"] is None or q["start"] is None
                                else q["done"] - q["start"],
                                "device_ns": q["device"], "back_ns": None if q["done"] is None
                                else q["complete"] - q["done"]}})
    for disk in sorted({q["disk"] for q in disk_requests(recs, 0)["requests"]}):
        track = CHROME_DISK_BASE + (disk or 0)
        events.append({"name": "process_name", "ph": "M", "pid": track, "tid": track,
                       "args": {"name": f"vd{chr(ord('a') + (disk or 0))}"}})

    with open(path, "w") as out:
        json.dump(events, out)
    print(f"trace-decode: {len(events)} events written to {path} (chrome://tracing or Perfetto)")


def selftest():
    """The pairing held to a made-up window whose answers are known: what no
    live gate reaches -- the device's commands, a refusal, an entry turned
    back, a vCPU's number two VMs' tasks have -- beside what one does. The
    first that comes out otherwise is said, and the exit code is 1."""
    recs = []

    def rec(t, cpu, ev, **f):
        recs.append({"t": t, "cpu": cpu, "ev": ev, "f": f})

    def exit_(t, pid, vcpu, reason, guest, out, since, detail=0, info=0):
        rec(t, 0, "hv_exit", vcpu=vcpu, pid=pid, reason=reason, detail=detail, info=info, guest_ns=guest,
            out_ns=out, since_ns=since, rip=0)

    # vCPU 0 of VM A (pid 10): an MSR write, an entry kicked back, a port
    # out, a HLT; what each cost is the next real entry's out_ns.
    exit_(1000, 10, 0, "msr", 500, 0, 100, detail=0x6E0, info=1)
    exit_(1200, 10, 0, "kicked", 0, 0, 0)
    exit_(2000, 10, 0, "io", 400, 300, 50, detail=0xC010, info=2)
    exit_(3000, 10, 0, "hlt", 600, 350, 40)
    exit_(9000, 10, 0, "host", 200, 5700, 60, detail=0xEC)
    # A ring to vCPU 1 of VM A (pid 11): its next entry after the ring --
    # and a posted interrupt's notification, which no entry delivers.
    rec(4000, 1, "hv_ring", vcpu=1, pid=10, how="rung")
    rec(4100, 1, "hv_ring", vcpu=1, pid=10, how="notified")
    exit_(5500, 11, 1, "msr", 700, 0, 100, detail=0x830, info=1)
    # vCPU 0 again, in VM B (pid 20): a ring to vCPU 0 is no longer one
    # task's to pair with -- though each of the two has an entry after it.
    exit_(9500, 20, 0, "io", 300, 0, 10, detail=0xE9, info=1)
    rec(9600, 2, "hv_ring", vcpu=0, pid=20, how="rung")
    exit_(9800, 20, 0, "io", 100, 150, 20, detail=0xE9, info=1)
    exit_(9900, 10, 0, "msr", 50, 10, 10, detail=0x10, info=0)

    # A write of vda's slot 3 through the device, in two commands; and a
    # read whose first command is refused and handed over again.
    rec(20000, 0, "blk_submit", disk=0, slot=3, op="write", offset=0x2000, len=8192, pid=10)
    rec(20500, 1, "disk_start", disk=0, slot=3, op="write", offset=0x2000, len=8192, pid=30)
    rec(20600, 1, "bio_submit", entry=7, buffer=3, write=1, sector=16, count=8, in_flight=1, pid=30)
    rec(20700, 1, "bio_submit", entry=7, buffer=3, write=1, sector=40, count=8, in_flight=2, pid=30)
    rec(26700, 3, "bio_done", entry=7, buffer=3, ok=1, refused=0)
    rec(27600, 3, "bio_done", entry=7, buffer=3, ok=1, refused=0)
    rec(28000, 1, "disk_done", disk=0, slot=3, op="write", how="async", ok=1, pid=30)
    rec(29000, 0, "blk_complete", disk=0, slot=3, op="write", ok=1, offset=0x2000, len=8192, pid=10)
    rec(30000, 0, "blk_submit", disk=0, slot=4, op="read", offset=0x8000, len=4096, pid=10)
    rec(30100, 1, "disk_start", disk=0, slot=4, op="read", offset=0x8000, len=4096, pid=30)
    rec(30200, 1, "bio_submit", entry=7, buffer=4, write=0, sector=64, count=8, in_flight=1, pid=30)
    rec(30200, 1, "bio_done", entry=7, buffer=4, ok=0, refused=1)
    rec(30900, 1, "bio_submit", entry=7, buffer=4, write=0, sector=64, count=8, in_flight=1, pid=30)
    rec(32900, 3, "bio_done", entry=7, buffer=4, ok=1, refused=0)
    rec(33000, 1, "disk_done", disk=0, slot=4, op="read", how="async", ok=1, pid=30)
    rec(33500, 0, "blk_complete", disk=0, slot=4, op="read", ok=1, offset=0x8000, len=4096, pid=10)
    # A completion of nothing in the window: counted, not paired.
    rec(34000, 0, "blk_complete", disk=1, slot=0, op="read", ok=1, offset=0, len=512, pid=10)
    recs.sort(key=lambda r: (r["t"], r["cpu"]))

    failures = []

    def want(what, got, expected):
        if got != expected:
            failures.append(f"{what}: {got!r}, not {expected!r}")

    reasons, details, vcpus = hv_exits(recs, 0)
    want("the MSR write's cost, past the entry kicked back", reasons["msr"]["out"], [300])
    want("the port's", details[("io", "vda notify out")]["out"], [350])
    want("the HLT's, its sleep", reasons["hlt"]["out"], [5700])
    want("VM B's port's, its own vCPU's next", details[("io", "debug out")]["out"], [150])
    want("the entries kicked back", vcpus[10]["kicked"], 1)
    want("vCPU 0's time in its guest", vcpus[10]["guest_ns"], 500 + 400 + 600 + 200 + 50)
    halts, timers, injects, rings, unpaired = hv_wakes(recs, 0)
    want("the ring to vCPU 1, to its next entry", rings["rung"]["to_entry"], [5500 - 100 - 700 - 4000])
    want("the ring to a number two VMs' tasks have, unpaired", unpaired, 1)
    want("the notification, counted and never paired", (rings["notified"]["n"], rings["notified"]["to_entry"]), (1, []))
    d = disk_requests(recs, 0)
    by_op = {q["op"]: q for q in d["requests"]}
    w, r = by_op["write"], by_op["read"]
    want("the write's stretches", (w["start"] - w["submit"], w["done"] - w["start"], w["complete"] - w["done"]),
         (500, 7500, 1000))
    want("its device time, first command handed over to last back", (w["device"], w["commands"]), (7000, 2))
    want("the read's device time, its refused command left out", (r["device"], r["commands"]), (2000, 1))
    want("the device's commands done, the refusal not one", sorted(ns for ns, _ in d["device"]), [2000, 6100, 6900])
    want("what had no request in the window", d["stray"], 1)
    for f in failures:
        print(f"trace-decode: selftest: {f}")
    print(f"trace-decode: selftest: {'failed' if failures else 'every pairing as it should be'}")
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(description="read a `trace dump` back")
    parser.add_argument("files", nargs="*", help="dumps to read; stdin if none")
    parser.add_argument("--chrome", metavar="FILE", help="write Chrome's trace format")
    parser.add_argument("--json", metavar="FILE", help="write the records as a JSON array")
    parser.add_argument("--latency", action="store_true", help="only the wakeup waits")
    parser.add_argument("--hv", action="store_true", help="only the hypervisor's and a guest disk's records")
    parser.add_argument("--cpu", type=int, help="only this cpu's records")
    parser.add_argument("--selftest", action="store_true", help="hold the pairing to a made-up window, and exit")
    args = parser.parse_args()
    if args.selftest:
        return selftest()

    lines = []
    if args.files:
        for path in args.files:
            with open(path, errors="replace") as f:
                lines.extend(f)
    else:
        lines = sys.stdin.readlines()

    recs = records(lines)
    start = window_start(lines)
    if args.cpu is not None:
        recs = [r for r in recs if r["cpu"] == args.cpu]
    if not recs:
        print("trace-decode: no records -- is that a `trace dump`?", file=sys.stderr)
        return 1

    if args.json:
        with open(args.json, "w") as out:
            json.dump(recs, out, indent=1)
        print(f"trace-decode: {len(recs)} records written to {args.json}")
    if args.chrome:
        chrome(recs, args.chrome)

    if args.hv:
        hv_summary(recs, start)
    elif args.latency:
        if start is None:
            print("trace-decode: a cpu's part could not be read: no wait is paired across it")
            return 0
        waits, answered, pending, superseded = latencies(recs, start)
        for ns, pid, comm, woken_cpu, run_cpu in sorted(waits, reverse=True):
            print(f"{ns:>12}ns pid {pid} {comm} (cpu {woken_cpu} -> {run_cpu})")
        print(f"trace-decode: {answered} answered, {pending} not run in the window, "
              f"{superseded} woken again before a switch")
    elif not (args.json or args.chrome):
        summarize(recs, start)
        if any(r["ev"].startswith(("hv_", "blk_", "disk_", "bio_", "image_", "ext4_")) for r in recs):
            hv_summary(recs, start)
    return 0


if __name__ == "__main__":
    sys.exit(main())
