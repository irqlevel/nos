#!/bin/bash
# The panel demo's nos VMs on the AX41 updated in place, one after another:
# each powered off, the modules its /etc/rc loads put into its root from
# modules/ -- nothing else of the root touched, its keys, hosts.toml, the
# cluster's and NBS's state all kept -- the root held to e2fsck -fn, and the
# VM started again on nos-test.iso as it is now, then waited for until its
# UDP shell answers and its ipcnet has its peers up again. One at a time,
# so that the cluster keeps its majority throughout -- and none taken down
# before NBS's chunks are all at three replicas again: a neighbour back
# rejoins its sets by a copy, and the next one down before that copy is
# done leaves the chunks at one replica, the EX44's, for as long as it
# takes. The last waited for too, so that the stand is left whole.
# usage: demo-update.sh ax41a ax41b
set -u
D=/root/nos-demo
cd $D || exit 1
MODS="ipcnet cluster nbs-mds nbs-cs api"
M=$D/mnt-update
WHOLE_SECS=${WHOLE_SECS:-1800}

# NBS's chunks all at three replicas, no copy going on and no cs down, as
# the host at $1 sees them, waited for up to WHOLE_SECS: 0 once they are,
# or where the host has no NBS -- a shell that answers without that line --,
# 1 if not in time. Each line it says that differs from the one before it
# is said.
nbs_whole() {
    local ip=$1 t=0 out line last=
    while [ $t -le $WHOLE_SECS ]; do
        out=$(NSH_TIMEOUT=10 python3 nsh.py $ip "nbs status" 2>/dev/null)
        if [ -n "$out" ]; then
            line=$(echo "$out" | grep '^nbs: chunks at')
            [ -n "$line" ] || return 0
            if echo "$line" | grep -q "at two 0, at one 0 -- 0 being copied, by 0 joins; cs.s down 0,"; then
                return 0
            fi
            [ "$line" = "$last" ] || echo "   $(echo "$line" | cut -c1-120)"
            last=$line
        fi
        sleep 10
        t=$((t + 10))
    done
    return 1
}

for name in "$@"; do
    case $name in
        ax41a) ip=10.77.0.11 ;;
        ax41b) ip=10.77.0.12 ;;
        *) echo "no such VM: $name"; exit 2 ;;
    esac
    [ -e root-$name.img ] || { echo "no root-$name.img"; exit 1; }
    for m in $MODS; do [ -f modules/$m.ko ] || { echo "no modules/$m.ko"; exit 1; }; done
    echo "== $name: $(NSH_TIMEOUT=10 python3 nsh.py $ip version 2>/dev/null | head -1)"
    nbs_whole $ip || { echo "   NBS's chunks not all at three replicas in $WHOLE_SECS s -- $name not taken down"; exit 1; }
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
    last_ip=$ip
done
if [ -n "${last_ip:-}" ]; then
    echo "== NBS's chunks back at three replicas"
    nbs_whole $last_ip || { echo "   not in $WHOLE_SECS s"; exit 1; }
    echo "   $(NSH_TIMEOUT=10 python3 nsh.py $last_ip "nbs status" 2>/dev/null | grep '^nbs: chunks at' | cut -c1-120)"
fi
