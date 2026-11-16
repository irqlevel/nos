#!/bin/bash
# The panel demo's nos VMs on the AX41, made once: for each name given, a
# root (ext4, 512 MiB) with the cluster's, NBS's, vnet's, hv's and the
# API's modules, and hvboot -- what hv boots a cloud image's VM by;
# hosts.toml naming the host alone in a cluster of its own until it joins
# one -- listening on its port, the one the AX41 forwards to it; api.toml
# listening on eth0 and taking the EX44 alone; an /etc/rc that starts them;
# and a pool disk of 40 GiB, sparse, its one partition's GUID fixed --
# `part:<guid>` the device NBS is given. Refuses to make anew a VM whose
# root is there: its keys are in it.
# usage: demo-make.sh ax41a ax41b
set -eu
D=/root/nos-demo
cd $D
MODS="ipcnet cluster vnet nbs-mds nbs-cs hv api"
for name in "$@"; do
    case $name in
        ax41a) port=39241 guid=6e6f7331-0000-4d00-8000-0000000000a1 ;;
        ax41b) port=39242 guid=6e6f7331-0000-4d00-8000-0000000000b1 ;;
        *) echo "no such VM: $name"; exit 2 ;;
    esac
    if [ -e root-$name.img ]; then
        echo "$name: root-$name.img is there -- not made anew"
        continue
    fi
    R=$D/rootdir-$name
    rm -rf $R
    mkdir -p $R/etc/ipc $R/etc/api
    for m in $MODS; do cp modules/$m.ko $R/$m.ko; done
    mkdir -p $R/lib
    cp modules/hvboot $R/lib/hvboot
    printf '# The panel demo: a nos VM on the AX41, a neighbour of the EX44\nname = "%s"\ncluster = "%s"\npeers = []\nport = %d\n' \
        "$name" "$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')" $port > $R/etc/ipc/hosts.toml
    printf '# The panel demo: the API on eth0, the EX44 -- the panel'"'"'s host -- alone let in\nport = 39234\nlisten = ["eth0"]\nallow_eth0 = ["65.108.38.184"]\n' \
        > $R/etc/api/api.toml
    printf '# The panel demo: a nos VM on the AX41\ninsmod /ipcnet.ko\nipcnet start\nipc echo start\ninsmod /cluster.ko\ninsmod /vnet.ko\ninsmod /nbs-mds.ko\ninsmod /nbs-cs.ko\ninsmod /hv.ko\nhv on\ninsmod /api.ko\napi start\n' \
        > $R/etc/rc
    scripts/mkrootfs.sh root-$name.img 512 $R > /dev/null
    rm -f pool-$name.img
    truncate -s 40G pool-$name.img
    sgdisk -o -n 1:2048:0 -t 1:8300 -c 1:nbs-$name -u 1:$guid pool-$name.img > /dev/null
    echo "$name: root-$name.img, pool-$name.img part:$guid, ipcnet port $port"
    cat $R/etc/ipc/hosts.toml $R/etc/api/api.toml
done
