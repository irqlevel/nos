#!/usr/bin/env python3
"""compare.py <kvm summary.txt> <nos summary.txt>: the suite's BENCH lines
of the two runs side by side -- each metric's median over its repetitions,
its spread, and nos's result as a share of KVM's (over 100%: nos better)."""
import re
import statistics
import sys


def parse(path):
    """{metric: [values]}, {key: text} for the lines that are facts, not numbers."""
    nums, facts = {}, {}

    def add(name, value):
        nums.setdefault(name, []).append(float(value))

    for raw in open(path, encoding="ascii", errors="replace"):
        line = raw.strip()
        if not line.startswith("BENCH "):
            continue
        w = line.split()[1:]
        kind = w[0]
        try:
            if kind == "sysbench_cpu":
                add("sysbench cpu %s, events/s" % w[1], w[3])
            elif kind == "7z":
                add("7-Zip %s, MIPS" % w[1], w[3])
            elif kind == "openssl":
                add("openssl %s 16 KiB, MB/s" % w[1], float(w[3].rstrip("k")) * 1000 / 1e6)
            elif kind == "sysbench_mem":
                add("memory %s %s, MiB/s" % (w[1], w[2]), w[3])
            elif kind == "perf_pipe":
                add("perf sched pipe, us/op", w[2])
            elif kind == "perf_messaging":
                add("perf sched messaging (hackbench), s", w[2])
            elif kind == "perf_syscall":
                add("perf syscall basic, us/op", w[2])
            elif kind == "cyclictest":
                avgs = [int(x) for x in re.findall(r"avg=(\d+)", line)]
                maxs = [int(x) for x in re.findall(r"max=(\d+)", line)]
                if avgs:
                    add("cyclictest avg, us", sum(avgs) / len(avgs))
                    add("cyclictest max, us", max(maxs))
            elif kind == "fio":
                kv = dict(x.split("=", 1) for x in w[2:] if "=" in x)
                if w[1].startswith("layout"):
                    add("fio %s, MiB/s" % w[1], kv["bw_mib"])
                else:
                    seq = w[1].startswith(("read_", "write_"))
                    if seq:
                        add("fio %s, MiB/s" % w[1], kv["bw_mib"])
                    else:
                        add("fio %s, IOPS" % w[1], kv["iops"])
                    add("fio %s, avg latency us" % w[1], kv["lat_us"])
                    add("fio %s, p99 latency us" % w[1], kv["p99_us"])
            elif kind == "kbuild":
                add("kernel %s, s" % w[1].replace("_ms", ""), int(w[2]) / 1000)
            elif kind == "pgbench":
                if w[1].startswith("init"):
                    add("pgbench %s, s" % w[1], int(w[2].split("=")[1]) / 1000)
                elif w[1] in ("ro", "rw"):
                    kv = dict(x.split("=", 1) for x in w[4:] if "=" in x)
                    add("pgbench %s %s, tps" % (w[1], w[2]), kv["tps"])
                    add("pgbench %s %s, latency ms" % (w[1], w[2]), kv["lat_ms"])
                else:
                    facts["pgbench " + w[1]] = " ".join(w[2:])
            else:
                facts[kind] = " ".join(w[1:])
        except (IndexError, KeyError, ValueError):
            facts["unparsed " + kind + " " + str(len(facts))] = line
    return nums, facts


LOWER = ("us/op", ", s", "latency", "us", " ms")


def lower_is_better(name):
    return name.endswith(("us/op", ", s", "latency us", "latency ms", ", us"))


def fmt(v):
    if v >= 1000:
        return "{:,.0f}".format(v).replace(",", " ")
    if v >= 100:
        return "%.0f" % v
    if v >= 10:
        return "%.1f" % v
    return "%.3g" % v


def cell(vals):
    med = statistics.median(vals)
    s = fmt(med)
    if len(vals) > 1 and med:
        spread = (max(vals) - min(vals)) / med * 100
        if spread >= 3:
            s += " (±%.0f%%)" % (spread / 2)
    return med, s


def main():
    kvm, kfacts = parse(sys.argv[1])
    nos, nfacts = parse(sys.argv[2])
    print("| metric | KVM | nos | nos vs KVM |")
    print("|---|---:|---:|---:|")
    for name in kvm:
        if name not in nos:
            print("| %s | %s | -- | |" % (name, cell(kvm[name])[1]))
            continue
        km, ks = cell(kvm[name])
        nm, ns = cell(nos[name])
        if km and nm:
            rel = (km / nm) if lower_is_better(name) else (nm / km)
            r = "%.0f%%" % (rel * 100)
        else:
            r = ""
        print("| %s | %s | %s | %s |" % (name, ks, ns, r))
    print()
    for k in sorted(set(kfacts) | set(nfacts)):
        a, b = kfacts.get(k, "--"), nfacts.get(k, "--")
        if a == b:
            print("- %s: %s (both)" % (k, a))
        else:
            print("- %s:\n  - KVM: %s\n  - nos: %s" % (k, a, b))


main()
