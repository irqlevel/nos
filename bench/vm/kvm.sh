#!/bin/bash
# kvm.sh: the KVM half of the comparison (README.md). From nos running: its
# VMs stopped by their power buttons, nos rebooted into the server's Linux,
# nos's root held to e2fsck -fn, the kit copied over, and then on Linux, each
# a stage of host/bench-host.sh -- the golden image made from Ubuntu's cloud
# image (prep), a copy of it booted and the suite run QUICK (check), the
# suite armed to run at the next boot (arm), and the suite under QEMU/KVM,
# the image on nos's root partition, pinned where nos's hv would put it
# (kvm). The results come back to $WORK/results-kvm. The server is left on
# Linux: nos-deploy.sh takes it back to nos.
#
#   kvm.sh            all of it
#   kvm.sh <stage>... only those stages, the server already on Linux
set -u
. "$(dirname "$0")/lib.sh"

stages=("$@")
if [ ${#stages[@]} -eq 0 ]; then
    step "nos's VMs before, each stopped by its power button"
    nos version || exit 1
    stop_vms "$WORK/vms-before" || exit 1
    step "nos rebooted into Linux"
    to_linux || exit 1
    step "nos's root held to e2fsck -fn"
    check_root "$WORK/e2fsck-before-kvm.log" || { echo "   stopping here, Linux left running"; exit 1; }
    stages=(info prep check arm kvm)
fi

step "the kit copied"
copy_kit || exit 1
for s in "${stages[@]}"; do
    remote "$s" 14400 || { tail -30 "$WORK/stage-$s.log"; exit 1; }
    tail -5 "$WORK/stage-$s.log"
done

if printf '%s\n' "${stages[@]}" | grep -qx kvm; then
    step "the KVM run's results"
    rm -rf "$WORK/results-kvm"
    scp -F "$SSH_CONFIG" -r "$LINUX_HOST:/root/bench/results-kvm" "$WORK/results-kvm" || exit 1
    ls "$WORK/results-kvm"
fi
step "done"
