#!/bin/bash
# nos-run.sh <name> [restart 0|1]: the nos half of the comparison, in the nos
# nos-deploy.sh installed. The VM /etc/rc started (PANEL_VM) stopped by its
# power button for the run, the bench VM started alone -- the same disk image
# the KVM run had, its suite run at boot -- and waited for until its guest
# powers itself off; then hv's report of it, its console and the suite's
# results decoded from it, and hv's own exit profile on a fast core and on a
# slow one. All of it in $WORK/results-nos-<name>. With restart 1 the VMs
# that ran before (the list stop_vms kept) are started again after.
set -u
. "$(dirname "$0")/lib.sh"

NAME=${1:?usage: nos-run.sh <name> [restart 0|1]}
RESTART=${2:-0}
R=$WORK/results-nos-$NAME
mkdir -p "$R"

step "nos"
nos version | tee "$R/version.txt" || exit 1
nos "dmesg cpu:" > "$R/dmesg-cpu.txt" 2>&1
cat "$R/dmesg-cpu.txt"

if [ -n "$PANEL_VM" ]; then
    step "$PANEL_VM, started by /etc/rc, waited for and stopped for the run"
    for t in $(seq 0 10 240); do
        [ "$(state "$PANEL_VM")" = running ] && break
        sleep 10
    done
    nos "hv list"
    if [ -n "$(state "$PANEL_VM")" ]; then
        nos "hv stop $PANEL_VM secs=180" > "$R/hv-stop-$PANEL_VM.log" 2>&1 \
            || echo "   (the ssh of hv stop ended badly: the list decides)"
        head -2 "$R/hv-stop-$PANEL_VM.log"
    fi
    for t in $(seq 0 10 200); do
        [ -z "$(state "$PANEL_VM")" ] && break
        sleep 10
    done
    [ -z "$(state "$PANEL_VM")" ] || { echo "   $PANEL_VM is still on the list"; exit 1; }
fi
nos "hv list"

step "the host as hv sees it"
nos "hv" > "$R/hv.txt" 2>&1
nos "hv info" > "$R/hv-info.txt" 2>&1
nos "hv cores" > "$R/hv-cores.txt" 2>&1
head -5 "$R/hv.txt"

step "bench started"
nos "hv start bench" 2>&1 | tee "$R/hv-start.txt"
sleep 5
nos "hv list" | tee "$R/hv-list-start.txt"
PLACED="bench  running  cpus ${VCPU_APIC// /,}  disks on cpu $IO_APIC"
grep -q "$PLACED" "$R/hv-list-start.txt" \
    || echo "   NOTE: not the placement the KVM run was pinned to ($PLACED)"

step "waiting for its guest to power itself off (up to 3 h)"
last=
for t in $(seq 0 60 10800); do
    sleep 60
    s=$(state bench) || s=
    now=$(nos "hv console bench bytes=1500" 2>/dev/null | tr -d '\r' | grep -a '^BENCH-STEP' | tail -1)
    [ -n "$now" ] && [ "$now" != "$last" ] && { echo "   $(date +%H:%M:%S) $now"; last=$now; }
    [ -n "$s" ] && [ "$s" != running ] && break
done
nos "hv list" | tee "$R/hv-list-end.txt"

step "hv's report of it"
nos "hv stop bench" > "$R/hv-stop-bench.txt" 2>&1
cat "$R/hv-stop-bench.txt"

step "its console, and the suite's results out of it"
nos "cat /vms/bench/console.log" > "$R/console.log" 2>&1
ls -la "$R/console.log"
grep -a '^BENCH ' "$R/console.log" | tr -d '\r' | tail -3
python3 "$KIT/decode.py" "$R/console.log" "$R/archive"

step "hv's own exit profile, on a fast core and on a slow one"
nos "hv bench cpu=$PROFILE_CPU profile" > "$R/hv-bench-profile-$PROFILE_CPU.txt" 2>&1
nos "hv bench cpu=$IO_APIC profile" > "$R/hv-bench-profile-$IO_APIC.txt" 2>&1
tail -20 "$R/hv-bench-profile-$PROFILE_CPU.txt"

if [ "$RESTART" = 1 ] && [ -f "$WORK/vms-before" ]; then
    step "the VMs that ran before the measurement started again"
    for vm in $(cat "$WORK/vms-before"); do
        nos "hv start $vm" 2>&1 | head -3
    done
    sleep 20
    nos "hv list"
fi
nos "dmesg" > "$R/dmesg.txt" 2>&1
step "done -- python3 $KIT/compare.py $WORK/results-kvm/archive/run/summary.txt $R/archive/run/summary.txt"
