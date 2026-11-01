# The workstation side of the KVM-vs-nos comparison, sourced by kvm.sh,
# nos-deploy.sh and nos-run.sh: the server's two systems reached over ssh,
# and the steps they share. Everything they write goes to $WORK (out/vm-bench
# under the repository unless BENCH_WORK says otherwise); the server's own
# values come from $WORK/bench.conf (bench.conf.example says what they are).

KIT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$KIT/../.." && pwd)
WORK=${BENCH_WORK:-$REPO/out/vm-bench}
CONF=${BENCH_CONF:-$WORK/bench.conf}
if [ ! -f "$CONF" ]; then
    echo "no $CONF: copy $KIT/bench.conf.example there and fill it in" >&2
    exit 2
fi
# shellcheck source=bench.conf.example
. "$CONF"
: "${SSH_CONFIG:?bench.conf: SSH_CONFIG}" "${NOS_HOST:?bench.conf: NOS_HOST}" "${LINUX_HOST:?bench.conf: LINUX_HOST}"
: "${NOS_ROOT:?bench.conf: NOS_ROOT}"
VCPU_APIC=${VCPU_APIC:-41 40 33 32}
IO_APIC=${IO_APIC:-62}
E_CORE_APIC_FIRST=${E_CORE_APIC_FIRST:-48}
PROFILE_CPU=${PROFILE_CPU:-16}
PANEL_VM=${PANEL_VM:-}
mkdir -p "$WORK"

step() { echo "== $(date +%H:%M:%S) $*"; }
nos() { ssh -F "$SSH_CONFIG" "$NOS_HOST" "$@"; }
linux() { ssh -F "$SSH_CONFIG" "$LINUX_HOST" "$@"; }

# wait_for <host> <command> <what> <seconds>: until the command answers there
wait_for() {
    local host=$1 cmd=$2 what=$3 secs=$4 t=0 out
    while [ $t -lt "$secs" ]; do
        if out=$(ssh -F "$SSH_CONFIG" -o ConnectTimeout=10 "$host" "$cmd" 2>/dev/null); then
            echo "   $what after ${t}s: $out"
            return 0
        fi
        sleep 10
        t=$((t + 10))
    done
    echo "   no $what in ${secs}s"
    return 1
}

# The VMs nos runs, by name; and one VM's state
running() { nos "hv list" | awk '$1 == "vm" && $4 == "running" { print $3 }'; }
state() { nos "hv list" | awk -v n="$1" '$1 == "vm" && $3 == n { print $4 }'; }

# stop_vms <file>: nos's running VMs stopped by their power buttons, up to
# 180 s each, their names kept in <file> for nos-run.sh to start again
stop_vms() {
    local vms vm left t
    vms=$(running) || return 1
    echo "   running: $(echo $vms)"
    echo "$vms" > "$1"
    for vm in $vms; do
        nos "hv stop $vm secs=180" > "$WORK/hv-stop-$vm.log" 2>&1 \
            || echo "   (the ssh of hv stop $vm ended badly: the list decides)"
        head -3 "$WORK/hv-stop-$vm.log"
    done
    left=x
    for t in $(seq 0 10 200); do
        left=$(running) || left=x
        [ -z "$left" ] && break
        sleep 10
    done
    if [ -n "$left" ]; then
        echo "   still running: $(echo $left)"
        return 1
    fi
    nos "hv list"
}

# to_linux: nos rebooted into the server's Linux -- its GRUB's default
to_linux() {
    nos reboot
    sleep 30
    wait_for "$LINUX_HOST" "uname -r" "Linux" 900
}

# to_nos: the server's Linux rebooted into the nos installed for one boot
to_nos() {
    linux "setsid reboot > /dev/null 2>&1 < /dev/null &"
    sleep 30
    wait_for "$NOS_HOST" "version" "nos" 900
}

# check_root <log>: nos's root held to e2fsck -fn from Linux, detached (a
# quiet ssh session may be cut on the way), its output kept in <log>
check_root() {
    local st t
    linux "rm -f /root/e2fsck-nos.status /root/e2fsck-nos.log; setsid nohup bash -c 'e2fsck -fn $NOS_ROOT > /root/e2fsck-nos.log 2>&1; echo \$? > /root/e2fsck-nos.status' > /dev/null 2>&1 < /dev/null &" || return 1
    st=
    for t in $(seq 0 10 900); do
        st=$(linux "cat /root/e2fsck-nos.status 2>/dev/null") || st=
        [ -n "$st" ] && break
        sleep 10
    done
    linux "cat /root/e2fsck-nos.log" > "$1" 2>&1
    cat "$1"
    if [ "$st" != "0" ]; then
        echo "   e2fsck -fn said ${st:-nothing in 900 s}"
        return 1
    fi
}

# copy_kit: host/ and guest/ to /root/bench on Linux
copy_kit() {
    linux "mkdir -p /root/bench" || return 1
    scp -F "$SSH_CONFIG" -r "$KIT/host" "$KIT/guest" "$LINUX_HOST:/root/bench/" || return 1
    linux "chmod +x /root/bench/host/*.sh /root/bench/guest/*.sh"
}

# remote <stage> [max secs]: host/bench-host.sh <stage> on Linux with the
# server's values, detached, its log polled; its status is the stage's, its
# log copied to $WORK/stage-<stage>.log
remote() {
    local s=$1 max=${2:-7200} l st t last now status
    l=/root/bench/stage-$s.log
    st=/root/bench/stage-$s.status
    linux "rm -f $st $l; setsid nohup env NOS_ROOT='$NOS_ROOT' VCPU_APIC='$VCPU_APIC' IO_APIC='$IO_APIC' E_CORE_APIC_FIRST='$E_CORE_APIC_FIRST' bash -c 'bash /root/bench/host/bench-host.sh $s > $l 2>&1; echo \$? > $st' > /dev/null 2>&1 < /dev/null &" || return 1
    echo "== $(date +%H:%M:%S) $s started"
    t=0
    last=
    status=
    while [ $t -lt "$max" ]; do
        sleep 30
        t=$((t + 30))
        status=$(linux "cat $st 2>/dev/null") || status=
        now=$(linux "grep -a '^== ' $l | tail -1" 2>/dev/null) || now=
        [ -n "$now" ] && [ "$now" != "$last" ] && { echo "   $now"; last=$now; }
        [ -n "$status" ] && break
    done
    linux "cat $l" > "$WORK/stage-$s.log" 2>&1
    echo "== $(date +%H:%M:%S) $s: status ${status:-none after ${max}s}"
    [ "$status" = 0 ]
}
