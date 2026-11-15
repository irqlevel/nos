#!/bin/bash
# The panel demo's nos VMs on the AX41 updated in place, one after another:
# each powered off, the modules its /etc/rc loads put into its root from
# modules/ -- nothing else of the root touched, its keys, hosts.toml, the
# cluster's and NBS's state all kept -- the root held to e2fsck -fn, and the
# VM started again on nos-test.iso as it is now, then waited for until its
# UDP shell answers and its ipcnet has its peers up again. One at a time,
# so that the cluster keeps its majority throughout.
# usage: demo-update.sh ax41a ax41b
set -u
D=/root/nos-demo
cd $D || exit 1
MODS="ipcnet cluster nbs-mds nbs-cs api"
M=$D/mnt-update
for name in "$@"; do
    case $name in
        ax41a) ip=10.77.0.11 ;;
        ax41b) ip=10.77.0.12 ;;
        *) echo "no such VM: $name"; exit 2 ;;
    esac
    [ -e root-$name.img ] || { echo "no root-$name.img"; exit 1; }
    for m in $MODS; do [ -f modules/$m.ko ] || { echo "no modules/$m.ko"; exit 1; }; done
    echo "== $name: $(NSH_TIMEOUT=10 python3 nsh.py $ip version 2>/dev/null | head -1)"
    ./demo-vm.sh stop $name || exit 1
    mkdir -p $M
    mount -o loop root-$name.img $M || { echo "$name: its root not mounted"; exit 1; }
    for m in $MODS; do cp modules/$m.ko $M/$m.ko; done
    sync
    umount $M
    e2fsck -fn root-$name.img > /dev/null 2>&1 || { echo "$name: e2fsck -fn says its root is not clean"; exit 1; }
    ./demo-vm.sh start $name || exit 1
    up=
    for t in $(seq 1 60); do
        up=$(NSH_TIMEOUT=5 python3 nsh.py $ip version 2>/dev/null | head -1) && [ -n "$up" ] && break
        sleep 5
    done
    echo "   $name back: ${up:-no answer in 300 s}"
    [ -n "$up" ] || exit 1
    for t in $(seq 1 30); do
        n=$(NSH_TIMEOUT=5 python3 nsh.py $ip ipcnet 2>/dev/null | grep -c ' -- up ')
        [ "${n:-0}" -ge 2 ] && break
        sleep 5
    done
    NSH_TIMEOUT=5 python3 nsh.py $ip ipcnet 2>/dev/null | sed -n '2,3p' | cut -c1-100 | sed 's/^/   /'
done
