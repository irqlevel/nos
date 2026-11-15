#!/bin/bash
# The panel's bundle installed anew in its VM on the EX44, its data kept --
# /var/lib/nos-panel: the hosts it was given, its identity, its user: the
# bundle onto the AX41's demo server (www/, which the EX44 alone may fetch
# from), nos's port 2223 forwarded to the VM's ssh for the while, the VM's
# own nos-panel-fetch run as root there, and the forward taken away
# whatever happens. The VM is not stopped.
#
#   DEMO_SSH_CONFIG=<ssh config> scripts/demo-stand/panel-update.sh <bundle>
#
# The VM lets in nos's own ssh keys (its seed's), as its default user,
# debian.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
CFG=${DEMO_SSH_CONFIG:?an ssh config with host ex44-nos: ssh_config.example}
AX41=${AX41:-65.109.93.213}
EX44=${EX44:-65.108.38.184}
D=${AX41_DIR:-/root/nos-demo}
B=${1:?the bundle: build.sh makes it}
NAME=$(basename "$B")
SHA=$(shasum -a 256 "$B" | cut -d' ' -f1)
KH=$ROOT/out/demo/known_hosts-panel
mkdir -p "$ROOT/out/demo"
on_nos() { ssh -F "$CFG" -o ConnectTimeout=20 ex44-nos "$1"; }
step() { echo "== $(date +%H:%M:%S) $*"; }

step "the bundle ($NAME, sha256 ${SHA:0:16}...) onto the AX41's demo server"
scp -q -o BatchMode=yes "$B" root@$AX41:$D/www/$NAME || exit 1

step "nos's 2223 forwarded to the panel's ssh"
on_nos "hv forward add 2223 panel 22" | tail -2
cleanup() {
    step "the forward taken away"
    on_nos "hv forward del 2223" | tail -2
}
trap cleanup EXIT

step "installed by the VM's own nos-panel-fetch"
ok=1
for t in 1 2 3 4 5 6; do
    if ssh -p 2223 -o BatchMode=yes -o ConnectTimeout=20 -o UserKnownHostsFile="$KH" \
        -o StrictHostKeyChecking=accept-new debian@$EX44 \
        "sudo /usr/local/sbin/nos-panel-fetch http://$AX41:39299/$NAME $SHA && cat /opt/nos-panel/VERSION && \
         systemctl is-active nos-panel.service nos-panel-caddy.service"; then
        ok=0
        break
    fi
    sleep 10
done
[ $ok -eq 0 ] || echo "   not installed"
exit $ok
