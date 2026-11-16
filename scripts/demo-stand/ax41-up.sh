#!/bin/bash
# The demo stand's AX41 half, from a built tree: the two neighbours' files
# onto the AX41 in $AX41_DIR (never /root/nos) -- nos-test.iso and the
# modules a neighbour loads, mkrootfs.sh and udpsh.py, ax41/'s scripts, and
# the panel's bundle in www/ -- then, there: the network up (ax41/demo-net.sh),
# DHCP on the bridge and the bundle's HTTP server started detached, the two
# VMs made (ax41/demo-make.sh) and started (ax41/demo-vm.sh).
#
#   scripts/demo-stand/ax41-up.sh <bundle>
#
# The tree is the repository's root, built as build.sh builds it: nos-test.iso
# (TEST_ISO_ARGS=udpshell=9100) and out/x86_64/modules. The AX41 is reached
# as root@$AX41 by the ssh of this machine's own configuration.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
AX41=${AX41:-65.109.93.213}
D=${AX41_DIR:-/root/nos-demo}
BUNDLE=${1:?"the panel's bundle: build.sh makes it"}
H=root@$AX41
S="ssh -o ConnectTimeout=40 -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=8"
R() { for i in 1 2 3 4 5; do rsync -rlpt --checksum --partial -z -e "$S" "$@" && return 0; echo "rsync again ($i)"; sleep 10; done; return 1; }
M=$ROOT/out/x86_64/modules
for f in "$ROOT/nos-test.iso" "$BUNDLE" $M/ipcnet.ko $M/cluster.ko $M/vnet.ko $M/nbs-mds.ko $M/nbs-cs.ko $M/hv.ko \
         $M/api.ko "$ROOT/out/x86_64/hvboot"; do
    [ -f "$f" ] || { echo "no $f: build.sh first"; exit 1; }
done

$S $H "mkdir -p $D/scripts $D/modules $D/www"
R "$ROOT/nos-test.iso" "$HERE"/ax41/demo-net.sh "$HERE"/ax41/demo-make.sh "$HERE"/ax41/demo-vm.sh \
  "$HERE"/ax41/dhcp.py "$HERE"/ax41/nsh.py $H:$D/
R "$ROOT/scripts/mkrootfs.sh" $H:$D/scripts/
R "$ROOT/scripts/udpsh.py" $H:$D/
R $M/ipcnet.ko $M/cluster.ko $M/vnet.ko $M/nbs-mds.ko $M/nbs-cs.ko $M/hv.ko $M/api.ko "$ROOT/out/x86_64/hvboot" \
    $H:$D/modules/
R "$BUNDLE" $H:$D/www/

# Each long-lived process its own session, every stream of it a file's: the
# ssh ends as the commands do, whatever runs on
$S $H "cd $D && chmod +x demo-*.sh scripts/mkrootfs.sh && ./demo-net.sh up && \
  setsid -f python3 $D/dhcp.py > $D/dhcp.log 2>&1 < /dev/null && \
  setsid -f sh -c 'cd $D/www && exec python3 -m http.server 39299' > $D/http.log 2>&1 < /dev/null && \
  sleep 1 && ./demo-make.sh ax41a ax41b && ./demo-vm.sh start ax41a && ./demo-vm.sh start ax41b"
echo "-- the neighbours boot; once the EX44's panel is there: neighbours.sh"
