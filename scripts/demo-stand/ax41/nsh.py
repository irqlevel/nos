#!/usr/bin/env python3
"""One command to a nos VM's UDP shell on the AX41's bridge (the demo stand), its answer
printed: `nsh.py <ip> <command...>` -- exit 0 once the whole answer came,
1 on a timeout. The client is scripts/udpsh.py's."""
import importlib.util
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
# udpsh.py beside it, as ax41-up.sh lays the AX41 out -- or the tree's own,
# in scripts/ two directories up
WHERE = [HERE, os.path.dirname(os.path.dirname(HERE))]
UDPSH = next((p for p in (os.path.join(d, "udpsh.py") for d in WHERE) if os.path.exists(p)), None)
if UDPSH is None:
    sys.exit("nsh.py: no udpsh.py beside it, nor in the tree's scripts/")
spec = importlib.util.spec_from_file_location("udpsh", UDPSH)
udpsh = importlib.util.module_from_spec(spec)
spec.loader.exec_module(udpsh)

PORT = 9100


def main():
    if len(sys.argv) < 3:
        sys.exit("usage: nsh.py <ip> <command...>")
    timeout = float(os.environ.get("NSH_TIMEOUT", "120"))
    c = udpsh.UdpShellClient(sys.argv[1], PORT, timeout)
    c.connect()
    c.send(" ".join(sys.argv[2:]))
    ok = c.recv()
    c.close()
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
