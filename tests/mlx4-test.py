#!/usr/bin/env python3
"""ConnectX-3 Ethernet driver host tests.

Compiles the driver's hardware-independent modules as they are and checks BAR
numbering, MCG steering entries and chains, and SFP/QSFP/PTYS decoding against
simulated firmware tables and known wire bytes. The real RX/TX path is covered
on the ConnectX-3 machines, which nothing emulates.

Needs rustc on the host. Exit code 0 = every check passed.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CASES = os.path.join(HERE, "fuzz", "mlx4")
OUT = os.path.join(ROOT, "out", "mlx4-test")


def main():
    os.makedirs(OUT, exist_ok=True)
    for case in ("bars", "mcg", "module"):
        source = os.path.join(CASES, case + ".rs")
        binary = os.path.join(OUT, case + "-test")
        r = subprocess.run([
            "rustc", "--edition=2021", "--test", source,
            "-C", "overflow-checks=on", "-C", "debug-assertions=on",
            "-o", binary,
        ])
        if r.returncode != 0:
            sys.exit("mlx4-test: %s did not build" % case)
        r = subprocess.run([binary])
        if r.returncode != 0:
            sys.exit("mlx4-test: %s failed" % case)
    print("mlx4-test: passed")


if __name__ == "__main__":
    main()
