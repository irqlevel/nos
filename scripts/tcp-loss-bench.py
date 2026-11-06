#!/usr/bin/env python3
"""What loss and reordering cost TCP between two kernels of nos: two of
them on scripts/hub.py's LAN, pinned to each other by ipcnet (as
ipc-test.py has them), and megabytes sent from one to the other's echo by
ipcnet's bulk connection -- each echoed back -- under each impairment of
the hub's in turn: none, loss alone, reordering (a jittered delay) alone, a
plain delay, and loss with jitter, each on links made for it. Says MB/s
each way and what both ends' TCP counted -- retransmissions, recoveries,
timeouts and the spurious ones, probes, undos, D-SACKs -- a line a run.

    scripts/tcp-loss-bench.py                    arm64: HVF on an Apple Silicon Mac
    scripts/tcp-loss-bench.py --mb 8 --runs 3    the megabytes a run, and runs a scenario
    scripts/tcp-loss-bench.py --only loss5       one scenario, by name

The hub is Python: it carries some 10 MB/s at most, which a clean link meets
and every lossy one is far under. Needs a built tree, as ipc-test.py does.
"""

import argparse
import threading
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, file))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


it = load("ipctest", "ipc-test.py")
hubmod = it.hubmod
pt = it.pt

# name: (loss in 256, delay ms, jitter ms)
SCENARIOS = [
    ("clean", 0, 0, 0),
    ("loss1", 3, 0, 0),
    ("loss5", 13, 0, 0),
    ("delay10", 0, 10, 0),
    ("reorder4", 0, 2, 4),
    ("loss5+reorder4", 13, 2, 4),
]


COUNTERS = ("retx", "recoveries", "timeouts", "spurious", "probes", "undos", "dsacks", "outside", "badack", "unkept", "overrun")


def counters(sh):
    """tcpstat's counters on its first line: what each end's TCP made of the
    losses -- segments sent again, recoveries RACK started, timeouts, loss
    probes, recoveries undone, D-SACKs -- and `net`'s frames the receive
    and transmit queues had no room for: what was lost inside nos itself."""
    out = sh.run("tcpstat")
    got = {}
    for name in COUNTERS:
        m = re.search(r"\b%s=(\d+)" % name, out)
        got[name] = int(m.group(1)) if m else 0
    out = sh.run("net")
    for name in ("rxfull", "txfull"):
        m = re.search(r"\b%s:(\d+)" % name, out)
        got[name] = int(m.group(1)) if m else 0
    m = re.search(r"\btx:(\d+) rx:(\d+)", out)
    got["nictx"], got["nicrx"] = (int(m.group(1)), int(m.group(2))) if m else (0, 0)
    return got


def delta(after, before):
    return " ".join("%s=%d" % (n, after[n] - before[n]) for n in COUNTERS + ("rxfull", "txfull"))


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--arch", choices=["x86_64", "aarch64"], default="aarch64")
    ap.add_argument("--mb", type=int, default=8, help="megabytes a run")
    ap.add_argument("--window", type=int, default=4, help="megabyte messages in flight at once")
    ap.add_argument("--runs", type=int, default=2, help="runs a scenario")
    ap.add_argument("--only", help="the one scenario to run")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"))
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--path", action="store_true", help="where frames went missing: NICs and hub, both ways")
    ap.add_argument("--dmesg", metavar="FILTER", help="each host's log lines with FILTER in them, after each run")
    ap.add_argument("--trace", metavar="EVENTS", help="tracepoints turned on, and the rings cleared, before each run "
                    "(`tcp`, `tcp_recv,tcp_ack`...)")
    ap.add_argument("--show", metavar="ARGS", action="append", default=[],
                    help="`trace show ARGS` on both hosts after each run, e.g. '50 tcp_recv verdict=outside'")
    ap.add_argument("--top", metavar="MS", type=int, help="b's `top MS` while the ping runs, from 100 ms into it: "
                    "what the echoing host's CPUs were busy with")
    args = ap.parse_args()
    accel = it.accel_of(args.arch)
    tmp = tempfile.mkdtemp(prefix="nos-tcploss-")
    print("tcp-loss-bench: %s, %s, %d MB a run, %d runs a scenario" % (args.arch, accel, args.mb, args.runs),
          flush=True)
    keys = {h: it.make_key(tmp, h) for h in ("a", "b")}
    ip = hubmod.Hub.ip
    hosts = [("a", keys["a"], [("b", ip(1), keys["b"][2])]), ("b", keys["b"], [("a", ip(0), keys["a"][2])])]
    images = [it.rootfs(tmp, args.arch, name, key, peers) for name, key, peers in hosts]
    hub = hubmod.Hub(2, it.HUB_BASE, shell_port=it.SHELL_PORT).start()
    procs, logs = [], []
    ok = True
    try:
        for i in range(2):
            log = os.path.join(tmp, "serial-%s.log" % "ab"[i])
            logs.append(log)
            procs.append(subprocess.Popen(it.qemu(args.arch, accel, images[i], log, hub, i, args.iso), cwd=ROOT,
                                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        for i in range(2):
            if not pt.wait_log(logs[i], "boot: complete", 300):
                sys.exit("tcp-loss-bench: %s did not boot" % "ab"[i])
        it.wait_for(lambda: 0 in hub.acked and 1 in hub.acked, 60)
        time.sleep(1)
        a, b = it.Shell(hub.shell(0), timeout=600), it.Shell(hub.shell(1), timeout=600)
        results = []
        for name, loss, delay, jitter in SCENARIOS:
            if args.only and name != args.only:
                continue
            # Each scenario on links of its own, as fresh as a boot's: what
            # one left of a round trip, a window and a reordering window
            # would otherwise be the next one's start
            a.run("ipcnet stop")
            b.run("ipcnet stop")
            hub.impair(0, 1)
            a.run("ipcnet start")
            b.run("ipcnet start")
            if not it.wait_for(lambda: it.peer_up(a, "b") and it.peer_up(b, "a"), 30, 1):
                sys.exit("tcp-loss-bench: the two never came up for %s" % name)
            hub.impair(0, 1, loss=loss, delay_ms=delay, jitter_ms=jitter)
            for run in range(args.runs):
                if args.trace:
                    for sh in (a, b):
                        sh.run("trace on %s" % args.trace)
                        sh.run("trace clear")
                ca, cb = counters(a), counters(b)
                failed = [dict(q.send_failed) for q in hub.ports]
                path = [(q.frames_in, q.frames_out) for q in hub.ports]
                mix = [dict(q.segments) for q in hub.ports]
                top = []
                if args.top:
                    def take_top():
                        time.sleep(0.1)
                        top.append(b.run("top %d" % args.top))
                    topper = threading.Thread(target=take_top)
                    topper.start()
                start = time.time()
                r = it.ping(a, "kernel.echo@b", "count=%d bytes=1048576 window=%d bulk timeout=60000" % (args.mb, args.window))
                took = time.time() - start
                if args.top:
                    topper.join()
                ca2, cb2 = counters(a), counters(b)
                da, db = ca2["retx"] - ca["retx"], cb2["retx"] - cb["retx"]
                if r is None or r["answered"] != args.mb or r["wrong"] != 0 or r["reordered"] != 0:
                    ok = False
                    print("tcp-loss-bench: %s run %d: %s" % (name, run, r), flush=True)
                    continue
                mbs = args.mb / took
                results.append((name, mbs, took, da, db, r["p50"] / 1000.0))
                print("%-16s run %d: %6.2f MB/s each way, %5.1f s for %d MB there and back, retransmitted a=%d "
                      "b=%d, a megabyte's round trip p50 %.0f ms" % (name, run, mbs, took, args.mb, da, db,
                                                                     r["p50"] / 1000.0), flush=True)
                print("%-16s        a: %s" % ("", delta(ca2, ca)), flush=True)
                print("%-16s        b: %s" % ("", delta(cb2, cb)), flush=True)
                # What the hub could not hand on: lost on the host, not the LAN
                lost = [{e: n - was.get(e, 0) for e, n in q.send_failed.items() if n != was.get(e, 0)}
                        for q, was in zip(hub.ports, failed)]
                if any(lost):
                    print("%-16s        hub could not send: to a %s, to b %s" % ("", lost[0], lost[1]), flush=True)
                for show in args.show:
                    for sh, host in ((a, "a"), (b, "b")):
                        print("%s: trace show %s\n%s" % (host, show, sh.run("trace show %s" % show)),
                              flush=True)
                if args.dmesg:
                    for sh, host in ((a, "a"), (b, "b")):
                        print("%s's log:\n%s" % (host, sh.run("dmesg 1000 %s" % args.dmesg)), flush=True)
                if args.path:
                    # Where frames went missing, a to b and b to a: what each
                    # NIC sent, what the hub took in and handed on, what the
                    # other NIC received
                    for (src, dst, cs, cs2, cd, cd2) in ((0, 1, ca, ca2, cb, cb2), (1, 0, cb, cb2, ca, ca2)):
                        print("%-16s        %s->%s: nic tx %d, hub in %d, hub out %d, nic rx %d; segments %s" % (
                            "", "ab"[src], "ab"[dst], cs2["nictx"] - cs["nictx"],
                            hub.ports[src].frames_in - path[src][0], hub.ports[dst].frames_out - path[dst][1],
                            cd2["nicrx"] - cd["nicrx"],
                            " ".join("%s=%d" % (k, v - mix[src][k]) for k, v in hub.ports[src].segments.items())),
                            flush=True)
            hub.impair(0, 1)
        print("tcp-loss-bench: summary (median of runs)")
        for name, *_ in SCENARIOS:
            got = sorted(x[1] for x in results if x[0] == name)
            if got:
                print("  %-16s %6.2f MB/s" % (name, got[len(got) // 2]))
    finally:
        for p in procs:
            pt.kill(p)
        hub.close()
        if args.keep or not ok:
            print("tcp-loss-bench: logs in %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
