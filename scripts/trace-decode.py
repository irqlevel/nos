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
`sched_switch` after it whose next task is the woken one. A record the dump
cut off (a wakeup whose switch is in the next page) is counted, not guessed.

Usage: scripts/trace-decode.py [--chrome FILE] [--json FILE] [--latency]
                               [--cpu N] [FILE...]

Reads stdin when no file is named. Exit code 0 when it read at least one
record, 1 when it found none.
"""

import argparse
import json
import sys

# Chrome's clock is microseconds, and its tracks are named by a process id:
# one per CPU, past the pids a kernel task has.
CHROME_PID_BASE = 1000


def records(lines):
    """The dump's records: every JSON object a line holds, oldest first."""
    out = []
    for line in lines:
        at = line.find("{")
        if at < 0:
            continue
        try:
            r = json.loads(line[at:])
        except ValueError:
            continue
        if not isinstance(r, dict) or "t" not in r or "cpu" not in r or "ev" not in r:
            continue
        if r["ev"] is None:
            continue
        out.append(r)
    out.sort(key=lambda r: (r["t"], r["cpu"]))
    return out


def latencies(recs):
    """Every wakeup a switch answered: (ns, pid, comm, woken_cpu, ran_cpu)."""
    waiting = {}
    waits = []
    answered = 0
    for r in recs:
        f = r.get("f", {})
        if r["ev"] == "sched_wakeup":
            pid = f.get("pid")
            if pid is not None and pid not in waiting:
                waiting[pid] = (r["t"], f.get("cpu", r["cpu"]))
        elif r["ev"] == "sched_switch":
            pid = f.get("next_pid")
            if pid in waiting:
                t0, woken_cpu = waiting.pop(pid)
                waits.append((r["t"] - t0, pid, f.get("next_comm", "?"), woken_cpu, r["cpu"]))
                answered += 1
    return waits, answered, len(waiting)


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


def summarize(recs):
    print(f"trace-decode: {len(recs)} records over {len({r['cpu'] for r in recs})} cpus, "
          f"{ns_text(recs[0]['t'])} to {ns_text(recs[-1]['t'])}")

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

    waits, answered, pending = latencies(recs)
    if answered:
        waits_ns = sorted(w[0] for w in waits)
        print(f"  wait: {answered} wakeups answered, {pending} not run in the window -- "
              f"p50 {ns_text(percentile(waits_ns, 50))} p90 {ns_text(percentile(waits_ns, 90))} "
              f"p99 {ns_text(percentile(waits_ns, 99))} max {ns_text(waits_ns[-1])}")
        for ns, pid, comm, woken_cpu, run_cpu in sorted(waits, reverse=True)[:5]:
            print(f"    {ns_text(ns):>9} pid {pid} {comm} (cpu {woken_cpu} -> {run_cpu})")
    elif pending:
        print(f"  wait: no wakeup of the window was followed by its switch ({pending} still waiting)")


def chrome(recs, path):
    """Chrome's trace format: a slice a (switch in, switch out) pair, an
    instant event for a wakeup, a block or a migration."""
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
                                    "waker": f.get("waker_pid"), "deadline": f.get("deadline")}})
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

    with open(path, "w") as out:
        json.dump(events, out)
    print(f"trace-decode: {len(events)} events written to {path} (chrome://tracing or Perfetto)")


def main():
    parser = argparse.ArgumentParser(description="read a `trace dump` back")
    parser.add_argument("files", nargs="*", help="dumps to read; stdin if none")
    parser.add_argument("--chrome", metavar="FILE", help="write Chrome's trace format")
    parser.add_argument("--json", metavar="FILE", help="write the records as a JSON array")
    parser.add_argument("--latency", action="store_true", help="only the wakeup waits")
    parser.add_argument("--cpu", type=int, help="only this cpu's records")
    args = parser.parse_args()

    lines = []
    if args.files:
        for path in args.files:
            with open(path, errors="replace") as f:
                lines.extend(f)
    else:
        lines = sys.stdin.readlines()

    recs = records(lines)
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

    if args.latency:
        waits, answered, pending = latencies(recs)
        for ns, pid, comm, woken_cpu, run_cpu in sorted(waits, reverse=True):
            print(f"{ns:>12}ns pid {pid} {comm} (cpu {woken_cpu} -> {run_cpu})")
        print(f"trace-decode: {answered} answered, {pending} not run in the window")
    elif not (args.json or args.chrome):
        summarize(recs)
    return 0


if __name__ == "__main__":
    sys.exit(main())
