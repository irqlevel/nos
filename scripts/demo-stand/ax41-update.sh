#!/bin/bash
# The demo stand's neighbours updated to a built tree, their state kept:
# nos-test.iso and the modules a neighbour loads onto the AX41 -- a running
# VM keeps the ISO it opened -- then there, ax41/demo-update.sh restarts
# each in turn on them, its root's modules replaced and nothing else of it.
#
#   scripts/demo-stand/ax41-update.sh [ax41a] [ax41b]
#
# The tree is built as build.sh builds it. The AX41 is reached as
# root@$AX41 by the ssh of this machine's own configuration.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
AX41=${AX41:-65.109.93.213}
D=${AX41_DIR:-/root/nos-demo}
H=root@$AX41
S="ssh -o ConnectTimeout=40 -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=40"
R() { for i in 1 2 3 4 5; do rsync -rlpt --checksum --partial -z -e "$S" "$@" && return 0; echo "rsync again ($i)"; sleep 10; done; return 1; }
M=$ROOT/out/x86_64/modules
for f in "$ROOT/nos-test.iso" $M/ipcnet.ko $M/cluster.ko $M/nbs-mds.ko $M/nbs-cs.ko $M/api.ko; do
    [ -f "$f" ] || { echo "no $f: build.sh first"; exit 1; }
done
NAMES=${*:-ax41a ax41b}
R "$ROOT/nos-test.iso" "$HERE"/ax41/demo-vm.sh "$HERE"/ax41/demo-update.sh "$HERE"/ax41/nsh.py $H:$D/
R $M/ipcnet.ko $M/cluster.ko $M/nbs-mds.ko $M/nbs-cs.ko $M/api.ko $H:$D/modules/
for name in $NAMES; do
    $S $H "cd $D && chmod +x demo-*.sh && ./demo-update.sh $name"
done
