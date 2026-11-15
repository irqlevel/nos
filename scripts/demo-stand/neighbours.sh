#!/bin/bash
# The demo stand's neighbours let the EX44's panel in, once its VM is made:
# the panel's key, as the EX44's `api clients` says it, added on each
# neighbour as an admin named `panel-<its first 8 digits>` -- what the panel's
# Hosts page tells to type -- through the neighbour's UDP shell on the
# AX41's bridge (ax41/nsh.py); then what the Hosts page asks of each, its
# address and its key, said.
#
#   DEMO_SSH_CONFIG=<ssh config> scripts/demo-stand/neighbours.sh
set -eu
CFG=${DEMO_SSH_CONFIG:?an ssh config with host ex44-nos: ssh_config.example}
AX41=${AX41:-65.109.93.213}
D=${AX41_DIR:-/root/nos-demo}
S="ssh -o ConnectTimeout=40 -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=8"
KEY=$(ssh -F "$CFG" ex44-nos "api clients" | awk '$3 == "panel" && $2 == "admin" { print $1 }')
[ ${#KEY} -eq 64 ] || { echo "no panel among the EX44's clients: is its VM made (api panel)?"; exit 1; }
NAME=panel-${KEY:0:8}
echo "the EX44's panel: $NAME $KEY"
for vm in "ax41a 10.77.0.11 39251" "ax41b 10.77.0.12 39252"; do
    set -- $vm
    $S root@$AX41 "cd $D && python3 nsh.py $2 api client add $NAME admin $KEY | tail -2; \
      python3 nsh.py $2 api status | head -3" | sed "s/^/   $1: /"
    pin=$($S root@$AX41 "cd $D && python3 nsh.py $2 api status" | sed -n 's/.*host key \([0-9a-f]\{64\}\).*/\1/p' | head -1)
    echo "-- on the panel's Hosts page: name $1, address $AX41:$3, key $pin"
done
