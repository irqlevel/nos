#!/bin/bash
# The demo stand's EX44: the nos it runs taken to the system image given, for
# one boot -- the machine's own default, Ubuntu, boots after it:
#
# - the panel's VM stopped and deleted, so that /etc/rc's `api panel create`
#   at the next boot makes it anew of the image's bundle; every other VM
#   running stopped by its power button, its name kept to start it again --
#   each given DEMO_STOP_SECS (600, hv's most) to turn itself off: a guest
#   installing updates declines the button until they are in (Ubuntu's
#   logind, inhibited by unattended-upgrades), hv pressing it again every
#   15 s, and one cut off midway is left with its packages half installed;
# - nos rebooted into Ubuntu; nos's root held to e2fsck -fn, then made ready
#   for the demo: /etc/rc loading ipcnet, the registry and NBS before the
#   API (once), a fresh hosts.toml -- this host alone, in a cluster of its
#   own until it makes or joins one, through the panel -- and what a cluster
#   before left there, the registry's folder (/etc/cluster) and NBS's mds
#   (/nbs/mds), renamed *.before-demo-<time>, never removed; e2fsck -fn again;
# - the image copied and installed by its own nos-install for one boot, the
#   kernel's line kept; Ubuntu rebooted into it;
# - the panel's VM followed until it runs, the VMs stopped before started
#   again.
#
#   DEMO_SSH_CONFIG=<ssh config> scripts/demo-stand/ex44-deploy.sh <image>
#
# With DEMO_KEEP=1 the stand's state is kept, the image alone changed: the
# panel's VM stopped, not deleted -- /etc/rc's `api panel create` starts it
# again, panel-update.sh installs a bundle in it -- and nos's root not made
# ready: the cluster, NBS and hosts.toml as they are. e2fsck -fn still. And
# nothing stopped before NBS's chunks are all at three replicas -- a
# neighbour just updated may be copying its sets back still, and this host's
# replicas the only ones active -- nor said done before they are again,
# this host's copied back.
#
# The ssh config names hosts ex44-nos and ex44-ubuntu (ssh_config.example).
# A step that fails stops it where it is, said; one in Ubuntu leaves Ubuntu
# running.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
CFG=${DEMO_SSH_CONFIG:?an ssh config with hosts ex44-nos and ex44-ubuntu: ssh_config.example}
ROOT_UUID=${EX44_ROOT_UUID:-83e77975-48f2-4356-819b-7b01d482c5de}
NAME=${EX44_NAME:-ex44}
KEEP=${DEMO_KEEP:-0}
STOP_SECS=${DEMO_STOP_SECS:-600}
LOCAL=${1:?the system image: build.sh makes nos-x86_64.squashfs}
LOG=$ROOT/out/demo
mkdir -p "$LOG"
SHA=$(shasum -a 256 "$LOCAL" | cut -c1-16)
IMG=/root/nos-x86_64-demo-$SHA.squashfs
DEV=/dev/disk/by-uuid/$ROOT_UUID

step() { echo "== $(date +%H:%M:%S) $*"; }

wait_for() { # wait_for <host> <command> <what> <seconds>
    local host=$1 cmd=$2 what=$3 secs=$4 t=0 out
    while [ $t -lt $secs ]; do
        if out=$(ssh -F "$CFG" -o ConnectTimeout=10 $host "$cmd" 2>/dev/null); then
            echo "   $what after ${t}s: $out"
            return 0
        fi
        sleep 10
        t=$((t + 10))
    done
    echo "   no $what in ${secs}s"
    return 1
}

on_nos() { ssh -F "$CFG" ex44-nos "$1"; }
on_ubuntu() { ssh -F "$CFG" ex44-ubuntu "$1"; }
running() { on_nos "hv list" | awk '$1 == "vm" && $4 == "running" { print $3 }'; }

# NBS's chunks all at three replicas, no copy going on and no cs down, as
# nos here sees them, waited for up to $1 seconds: 0 once they are, or where
# this host has no NBS -- an answer without that line --, 1 if not in time.
# Each line it says that differs from the one before it is said.
nbs_whole() {
    local secs=$1 t=0 out line last=
    while [ $t -le $secs ]; do
        if out=$(on_nos "nbs status" 2>/dev/null) && [ -n "$out" ]; then
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

# nos's root held to e2fsck -fn in Ubuntu, detached: its ssh may drop
fsck_nos() {
    local st=
    on_ubuntu "rm -f /root/e2fsck-nos.status; setsid -f bash -c 'e2fsck -fn $DEV > /root/e2fsck-nos.log 2>&1; \
echo \$? > /root/e2fsck-nos.status' > /dev/null 2>&1 < /dev/null" || return 1
    for t in $(seq 0 10 900); do
        st=$(on_ubuntu "cat /root/e2fsck-nos.status 2>/dev/null") || st=
        [ -n "$st" ] && break
        sleep 10
    done
    on_ubuntu "cat /root/e2fsck-nos.log" | tail -4
    [ "$st" = "0" ] || { echo "   e2fsck -fn said ${st:-nothing in 900 s} -- stopping here, Ubuntu left running"; return 1; }
}

step "nos as it is"
on_nos version || exit 1
on_nos "hv list" | cut -c1-110
VMS=$(running) || exit 1
echo "$VMS" > "$LOG/ex44-vms-before"
echo "   running: $(echo $VMS)"

if [ "$KEEP" = 1 ]; then
    step "NBS's chunks all at three replicas before this host goes down -- up to 30 minutes"
    nbs_whole 1800 || { echo "   not in time -- nothing stopped: this host's replicas may be the only ones active"; exit 1; }
fi

if [ "$KEEP" = 1 ]; then
    step "the panel's VM stopped, kept: the next boot starts it again"
else
    step "the panel's VM stopped and deleted: the next boot makes it anew, of the image's bundle"
fi
if on_nos "hv vms" | grep -qw panel; then
    if running | grep -qx panel; then
        on_nos "hv stop panel secs=$STOP_SECS" | head -3
    fi
    for t in $(seq 0 5 $((STOP_SECS + 60))); do running | grep -qx panel || break; sleep 5; done
    if running | grep -qx panel; then echo "   the panel runs still -- not going on"; exit 1; fi
    [ "$KEEP" = 1 ] || on_nos "hv delete panel force" | head -5
fi

step "every other VM stopped by its power button, up to $STOP_SECS s -- together, not one after another"
on_nos "hv stop all secs=$STOP_SECS" | grep '^hv: '
left=x
for t in $(seq 0 10 $((STOP_SECS + 60))); do
    left=$(running) || left=x
    [ -z "$left" ] && break
    sleep 10
done
[ -z "$left" ] || { echo "   still running: $(echo $left) -- not rebooting"; exit 1; }

step "nos rebooted into Ubuntu"
on_nos reboot
sleep 30
wait_for ex44-ubuntu "uname -r" "Ubuntu" 900 || exit 1

step "nos's root held to e2fsck -fn"
fsck_nos || exit 1

if [ "$KEEP" != 1 ]; then
step "nos's root made ready for the demo"
cat > "$LOG/ex44-prepare.sh" <<'PREP'
#!/bin/bash
# On the EX44's Ubuntu: nos's root, unmounted by nos, made ready for the demo.
set -e
DEV=$1
NAME=$2
M=/mnt/nosroot
STAMP=$(date +%Y%m%d-%H%M%S)
mkdir -p $M
if mountpoint -q $M; then umount $M; fi
mount $DEV $M
trap 'umount $M' EXIT
for d in etc/cluster nbs/mds; do
    if [ -e $M/$d ]; then mv $M/$d $M/$d.before-demo-$STAMP; echo "kept aside: /$d.before-demo-$STAMP"; fi
done
mkdir -p $M/etc/ipc
if [ -e $M/etc/ipc/hosts.toml ]; then cp $M/etc/ipc/hosts.toml $M/etc/ipc/hosts.toml.before-demo-$STAMP; fi
printf '# The demo stand: this host alone, in a cluster of its own until it makes or joins one\nname = "%s"\ncluster = "%s"\npeers = []\n' \
    "$NAME" "$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')" > $M/etc/ipc/hosts.toml
if ! grep -q '^insmod /lib/modules/ipcnet.ko' $M/etc/rc; then
    cp $M/etc/rc $M/etc/rc.before-demo-$STAMP
    LINES='insmod /lib/modules/ipcnet.ko\nipcnet start\nipc echo start\ninsmod /lib/modules/cluster.ko\ninsmod /lib/modules/nbs-mds.ko\ninsmod /lib/modules/nbs-cs.ko'
    if grep -q '^insmod /lib/modules/api.ko' $M/etc/rc.before-demo-$STAMP; then
        awk -v l="$LINES" '/^insmod \/lib\/modules\/api.ko/ && !d { gsub(/\\n/, "\n", l); print l; d = 1 } { print }' \
            $M/etc/rc.before-demo-$STAMP > $M/etc/rc
    else
        { cat $M/etc/rc.before-demo-$STAMP; printf "$LINES\n"; } > $M/etc/rc
    fi
fi
echo "--- /etc/rc"
cat $M/etc/rc
echo "--- /etc/ipc/hosts.toml"
cat $M/etc/ipc/hosts.toml
sync
PREP
scp -F "$CFG" "$LOG/ex44-prepare.sh" ex44-ubuntu:/root/ex44-prepare.sh || exit 1
on_ubuntu "bash /root/ex44-prepare.sh $DEV $NAME" || { echo "   not made ready -- stopping here, Ubuntu left running"; exit 1; }
fsck_nos || exit 1
fi

step "the image copied"
scp -F "$CFG" "$LOCAL" ex44-ubuntu:$IMG || exit 1
on_ubuntu "sha256sum $IMG" | cut -c1-16
echo "$SHA (here)"

step "the image installed for one boot, the kernel's line kept"
cat > "$LOG/ex44-update.sh" <<UPDATE
#!/bin/bash
set -e
mkdir -p /mnt/nosimg
if mountpoint -q /mnt/nosimg; then umount /mnt/nosimg; fi
mount -o loop,ro $IMG /mnt/nosimg
echo "image: \$(cat /mnt/nosimg/VERSION | tr '\n' ' ')"
/mnt/nosimg/nos-install status || true
/mnt/nosimg/nos-install update --image $IMG --boot-once
/mnt/nosimg/nos-install status
umount /mnt/nosimg
UPDATE
scp -F "$CFG" "$LOG/ex44-update.sh" ex44-ubuntu:/root/ex44-update.sh || exit 1
on_ubuntu "bash /root/ex44-update.sh" || { echo "   not installed -- stopping here, Ubuntu left running"; exit 1; }

step "Ubuntu rebooted into nos"
on_ubuntu "setsid -f reboot > /dev/null 2>&1 < /dev/null"
sleep 30
wait_for ex44-nos "version" "nos" 900 || exit 1

step "the panel made anew -- or started, DEMO_KEEP -- by /etc/rc, followed until it runs -- up to 20 minutes"
out=
for t in $(seq 0 20 1200); do
    out=$(on_nos "api panel" 2>/dev/null) || out=
    echo "$out" | grep -q "VM is running" && break
    sleep 20
done
echo "$out"

step "the VMs that ran before started again"
for vm in $VMS; do
    [ "$vm" = panel ] && continue
    on_nos "hv start $vm" | head -3
done
on_nos "hv list" | cut -c1-110
on_nos "ipcnet" | head -3
if [ "$KEEP" = 1 ]; then
    step "NBS's chunks back at three replicas, this host's copied back -- up to 30 minutes"
    nbs_whole 1800 || { echo "   not in time"; exit 1; }
fi
step "done"
