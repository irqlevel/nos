#!/bin/bash
# The panel demo's nos VMs on the AX41 updated in place, one after another:
# each powered off, the modules its /etc/rc loads put into its root from
# modules/ and hvboot into its /lib -- vnet's and hv's added to /etc/rc
# where it has not them, the rest of the root not touched, its keys,
# hosts.toml, the cluster's and NBS's state all kept -- the root held to
# e2fsck -fn, and the
# VM started again on nos-test.iso as it is now, then waited for until its
# UDP shell answers and its ipcnet has its peers up again. One at a time,
# so that the cluster keeps its majority throughout -- and none taken down
# before NBS's chunks are all at three replicas again: a neighbour back
# rejoins its sets by a copy, and the next one down before that copy is
# done leaves the chunks at one replica, the EX44's, for as long as it
# takes. The last waited for too, so that the stand is left whole.
#
# A neighbour that runs a VM is not taken down: its poweroff stops the VM
# with it, a guest of the stand's -- move it to the other neighbour first
# (`hv migrate <vm> <host>`, live), and back after, as a host is updated
# with its VMs kept running. Its list read through its UDP shell, and a
# list not read is a refusal too.
#
# A neighbour back says what NBS was before it went until its member has
# caught up with the leader -- every chunk at three replicas, its own cs's
# taking down not yet heard of -- so it is not asked before it has applied
# an entry past the last it had applied as it went.
# usage: demo-update.sh ax41a ax41b
set -u
D=/root/nos-demo
cd $D || exit 1
MODS="ipcnet cluster vnet nbs-mds nbs-cs hv api"
M=$D/mnt-update
WHOLE_SECS=${WHOLE_SECS:-1800}
CAUGHT_UP_SECS=${CAUGHT_UP_SECS:-300}

# What `nbs status` says at the host at $1: its chunks' line, empty where it
# has none -- a host with no NBS, or one whose member is not up yet, which
# only the caller can tell apart -- and "?" where its shell does not answer.
nbs_line() {
    local out
    out=$(NSH_TIMEOUT=10 python3 nsh.py $1 "nbs status" 2>/dev/null)
    [ -n "$out" ] || { echo "?"; return; }
    echo "$out" | grep '^nbs: chunks at' || true
}

# The entry the NBS member at $1 has applied; empty where it says none.
nbs_applied() {
    NSH_TIMEOUT=10 python3 nsh.py $1 "nbs status" 2>/dev/null | sed -n 's/.* applied \([0-9][0-9]*\),.*/\1/p' | head -1
}

# NBS's chunks all at three replicas, no copy going on and no cs down, as
# the host at $1 sees them, waited for up to WHOLE_SECS: 0 once they are --
# or, unless $2 is 1, the stand having NBS, at an answer without that line, a
# host with none -- 1 if not in time. Each line it says that differs from
# the one before it is said.
nbs_whole() {
    local ip=$1 need=${2:-0} t=0 line last=
    while [ $t -le $WHOLE_SECS ]; do
        line=$(nbs_line $ip)
        if [ -z "$line" ]; then
            [ "$need" = 1 ] || return 0
        elif [ "$line" != "?" ]; then
            echo "$line" | grep -q "at two 0, at one 0 -- 0 being copied, by 0 joins; cs.s down 0," && return 0
            [ "$line" = "$last" ] || echo "   $(echo "$line" | cut -c1-120)"
            last=$line
        fi
        sleep 10
        t=$((t + 10))
    done
    return 1
}

HAS_NBS=
for name in "$@"; do
    case $name in
        ax41a) ip=10.77.0.11 ;;
        ax41b) ip=10.77.0.12 ;;
        *) echo "no such VM: $name"; exit 2 ;;
    esac
    [ -e root-$name.img ] || { echo "no root-$name.img"; exit 1; }
    for m in $MODS; do [ -f modules/$m.ko ] || { echo "no modules/$m.ko"; exit 1; }; done
    [ -f modules/hvboot ] || { echo "no modules/hvboot"; exit 1; }
    echo "== $name: $(NSH_TIMEOUT=10 python3 nsh.py $ip version 2>/dev/null | head -1)"
    vms=$(NSH_TIMEOUT=20 python3 nsh.py $ip "hv list" 2>/dev/null) || { echo "   $name's VMs not read -- not taken down"; exit 1; }
    running=$(printf '%s\n' "$vms" | awk '$1 == "vm" && $4 != "stopped" { print $3 }' | tr '\n' ' ')
    if [ -n "$running" ]; then
        echo "   $name runs $running-- not taken down: move them to the other neighbour first (hv migrate <vm> <host>)"
        exit 1
    fi
    if [ -z "$HAS_NBS" ]; then
        line=$(nbs_line $ip)
        HAS_NBS=0
        [ -n "$line" ] && [ "$line" != "?" ] && HAS_NBS=1
    fi
    nbs_whole $ip $HAS_NBS || { echo "   NBS's chunks not all at three replicas in $WHOLE_SECS s -- $name not taken down"; exit 1; }
    applied=0
    if [ "$HAS_NBS" = 1 ]; then
        applied=$(nbs_applied $ip)
        applied=${applied:-0}
    fi
    ./demo-vm.sh stop $name || exit 1
    mkdir -p $M
    mount -o loop root-$name.img $M || { echo "$name: its root not mounted"; exit 1; }
    for m in $MODS; do cp modules/$m.ko $M/$m.ko; done
    mkdir -p $M/lib
    cp modules/hvboot $M/lib/hvboot
    # vnet after the registry, hv after N\'s and turned on -- once, the rc
    # before kept
    if ! grep -q '^insmod /vnet.ko' $M/etc/rc || ! grep -q '^insmod /hv.ko' $M/etc/rc || ! grep -q '^hv on' $M/etc/rc
    then
        [ -e $M/etc/rc.before-vnet ] || cp $M/etc/rc $M/etc/rc.before-vnet
        grep -q '^insmod /vnet.ko' $M/etc/rc || sed -i '/^insmod \/cluster.ko/a insmod /vnet.ko' $M/etc/rc
        grep -q '^insmod /hv.ko' $M/etc/rc || sed -i '/^insmod \/nbs-cs.ko/a insmod /hv.ko' $M/etc/rc
        grep -q '^hv on' $M/etc/rc || sed -i '/^insmod \/hv.ko/a hv on' $M/etc/rc
        echo "   its /etc/rc: $(grep -c '^insmod' $M/etc/rc) modules, vnet and hv among them, hv on"
    fi
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
    if [ "$HAS_NBS" = 1 ]; then
        now=
        for t in $(seq 0 5 $CAUGHT_UP_SECS); do
            now=$(nbs_applied $ip)
            [ -n "$now" ] && [ "$now" -gt "$applied" ] && break
            sleep 5
        done
        echo "   its NBS member: entry ${now:-none} applied, ${applied} as it went"
    fi
    last_ip=$ip
done
if [ -n "${last_ip:-}" ]; then
    echo "== NBS's chunks back at three replicas"
    nbs_whole $last_ip $HAS_NBS || { echo "   not in $WHOLE_SECS s"; exit 1; }
    echo "   $(nbs_line $last_ip | cut -c1-120)"
fi
