#!/bin/bash
# The demo stand's AX41 half taken down: the two neighbours powered off
# (ax41/demo-vm.sh stop), DHCP and the bundle's HTTP server stopped, the
# network's rules, taps and bridge taken away (ax41/demo-net.sh down). Their
# roots and pools stay in $AX41_DIR -- ax41-up.sh starts them again as they
# were; `rm -rf` it to begin anew.
#
#   scripts/demo-stand/ax41-down.sh
set -u
AX41=${AX41:-65.109.93.213}
D=${AX41_DIR:-/root/nos-demo}
S="ssh -o ConnectTimeout=40 -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=8"
$S root@$AX41 "cd $D && ./demo-vm.sh stop ax41a; ./demo-vm.sh stop ax41b; \
  pkill -f '^python3 $D/dhcp.py' ; pkill -f '^python3 -m http.server 39299' ; ./demo-net.sh down"
