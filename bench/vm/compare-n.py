#!/usr/bin/env python3
"""compare-n.py <label>=<summary.txt> ...: the suite's BENCH lines of several
runs side by side -- each metric's median over its repetitions and spread --
the first run the reference, every other also as a share of it (over 100%:
better than the reference). The parser is compare.py's."""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
src = open(os.path.join(HERE, "compare.py")).read()
src = src[:src.rindex("\nmain()")]
ns = {}
exec(compile(src, "compare.py", "exec"), ns)
parse, cell, lower_is_better = ns["parse"], ns["cell"], ns["lower_is_better"]

runs = []
for arg in sys.argv[1:]:
    label, path = arg.split("=", 1)
    nums, facts = parse(path)
    runs.append((label, nums, facts))
ref_label, ref, _ = runs[0]
head = ["metric", ref_label] + ["%s (%% of %s)" % (l, ref_label) for l, _, _ in runs[1:]]
print("| " + " | ".join(head) + " |")
print("|---|" + "---:|" * (len(head) - 1))
for name in ref:
    rm, rs = cell(ref[name])
    row = [name, rs]
    for _, nums, _ in runs[1:]:
        if name not in nums:
            row.append("--")
            continue
        m, s = cell(nums[name])
        if rm and m:
            rel = (rm / m) if lower_is_better(name) else (m / rm)
            row.append("%s (%.0f%%)" % (s, rel * 100))
        else:
            row.append(s)
    print("| " + " | ".join(row) + " |")
