#!/bin/bash
# nos-deploy.sh <nos-x86_64.squashfs>: a system image (`make image`) put on
# the server for one boot of nos, with a fresh copy of the benchmark's disk.
# From nos running -- its VMs stopped by their power buttons, nos rebooted
# into Linux -- or from Linux already (after kvm.sh): nos's root held to
# e2fsck -fn, the image copied over, the golden image copied fresh onto
# nos's root with its vm.toml (host/bench-host.sh nos-stage, which checks the
# root again after), the image installed for the next boot only -- the
# server's kernel line, /etc/rc and the panel's password kept, its GRUB's
# default left as it was (nos-install update --boot-once) -- and Linux
# rebooted into it.
set -u
. "$(dirname "$0")/lib.sh"

SRC=${1:?usage: nos-deploy.sh <nos-x86_64.squashfs>}
[ -f "$SRC" ] || { echo "no $SRC" >&2; exit 2; }
IMG=/root/nos-x86_64-$(shasum -a 256 "$SRC" | cut -c1-12).squashfs

if nos version > /dev/null 2>&1; then
    step "nos's VMs before, each stopped by its power button"
    nos version
    stop_vms "$WORK/vms-before" || exit 1
    step "nos rebooted into Linux"
    to_linux || exit 1
fi

step "nos's root held to e2fsck -fn"
check_root "$WORK/e2fsck-before-nos.log" || { echo "   stopping here, Linux left running"; exit 1; }

step "the image copied"
scp -F "$SSH_CONFIG" "$SRC" "$LINUX_HOST:$IMG" || exit 1
linux "sha256sum $IMG"

step "the kit, and a fresh copy of the bench disk and its vm.toml onto nos's root"
copy_kit || exit 1
remote nos-stage 1800 || { tail -30 "$WORK/stage-nos-stage.log"; exit 1; }
tail -12 "$WORK/stage-nos-stage.log"

step "the image installed for one boot, the kernel line kept"
cat > "$WORK/nos-update.sh" <<UPDATE
#!/bin/bash
set -e
IMG=$IMG
sha256sum \$IMG
mkdir -p /mnt/nosimg
if mountpoint -q /mnt/nosimg; then umount /mnt/nosimg; fi
mount -o loop,ro \$IMG /mnt/nosimg
echo "image: \$(cat /mnt/nosimg/VERSION)"
/mnt/nosimg/nos-install status || true
/mnt/nosimg/nos-install update --image \$IMG --boot-once
/mnt/nosimg/nos-install status
umount /mnt/nosimg
UPDATE
scp -F "$SSH_CONFIG" "$WORK/nos-update.sh" "$LINUX_HOST:/root/nos-update.sh" || exit 1
linux "bash /root/nos-update.sh" > "$WORK/nos-install.log" 2>&1 || { echo "   failed:"; cat "$WORK/nos-install.log"; exit 1; }
cat "$WORK/nos-install.log"

step "Linux rebooted into nos"
to_nos || exit 1
step "done"
