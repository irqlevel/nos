#!/bin/sh
# The installer ISO, made inside the Alpine container scripts/installer/build.sh
# runs: /installer is scripts/installer, read-only; /work holds in/ (the
# system image, the answers), apk-cache/ and, at the end, the ISO.
#
# The installer is one Linux kernel and its initramfs -- the whole system,
# run from memory, so that the medium is read by the firmware and GRUB
# alone and a slow virtual CD is let go of as soon as it has booted -- and
# the system image in a second initramfs, as it was built. The ISO's GRUB
# boots it, by BIOS or UEFI, or the image's own kernel.
set -eu

BRANCH=v3.24
MIRROR=https://dl-cdn.alpinelinux.org/alpine
CACHE=/work/apk-cache
ROOTFS=/tmp/rootfs
TREE=/tmp/iso

printf '%s/%s/main\n%s/%s/community\n' "$MIRROR" "$BRANCH" "$MIRROR" "$BRANCH" > /etc/apk/repositories
apk add -q --cache-dir "$CACHE" grub grub-bios grub-efi xorriso mtools squashfs-tools cpio

# ---- the installer's system --------------------------------------------

rm -rf "$ROOTFS"
mkdir -p "$ROOTFS/etc/apk" "$ROOTFS/proc" "$ROOTFS/dev" "$ROOTFS/sys"
cp -a /etc/apk/keys "$ROOTFS/etc/apk/"
cp /etc/apk/repositories "$ROOTFS/etc/apk/"
# The packages' install scripts run in a chroot: busybox's makes its links
# through /proc, the kernel's runs depmod and mkinitfs. GRUB's are left out:
# its trigger makes a grub.cfg for the system it is installed in, and finds
# no disk under a chroot's / -- the installer runs grub-install for the
# disk it makes nos's, and needs GRUB's tools and modules alone
mount -t proc proc "$ROOTFS/proc"
mount --bind /dev "$ROOTFS/dev"
mount --bind /sys "$ROOTFS/sys"
trap 'umount "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/proc" 2>/dev/null || true' EXIT
# shellcheck disable=SC2046
apk add -q --root "$ROOTFS" --initdb --cache-dir "$CACHE" $(grep -v -e '^#' -e '^grub' /installer/packages)
# shellcheck disable=SC2046
apk add -q --root "$ROOTFS" --no-scripts --cache-dir "$CACHE" $(grep '^grub' /installer/packages)
umount "$ROOTFS/sys" "$ROOTFS/dev" "$ROOTFS/proc"
trap - EXIT

KVER=$(ls "$ROOTFS/lib/modules")
[ "$(echo "$KVER" | wc -l)" = 1 ] || { echo "mkiso.sh: kernels: $KVER" >&2; exit 1; }
cp "$ROOTFS/boot/vmlinuz-lts" /tmp/vmlinuz
# What the installer has no use for: the kernel and the initramfs Alpine
# makes for itself (the ISO carries them apart), documentation, and the
# drivers of hardware no server's installer meets -- graphics cards' own
# (the console is the firmware's framebuffer, simpledrm), sound, radios,
# cameras -- which are a third of the modules
rm -rf "$ROOTFS/boot"/* "$ROOTFS/usr/share/man" "$ROOTFS/usr/share/doc" "$ROOTFS/usr/share/info"
M=$ROOTFS/lib/modules/$KVER/kernel
rm -rf "$M/drivers/gpu/drm/amd" "$M/drivers/gpu/drm/nouveau" "$M/drivers/gpu/drm/i915" \
    "$M/drivers/gpu/drm/xe" "$M/drivers/gpu/drm/radeon" "$M/sound" "$M/drivers/media" \
    "$M/drivers/net/wireless" "$M/net/wireless" "$M/net/mac80211" "$M/net/bluetooth" \
    "$M/drivers/bluetooth" "$M/drivers/staging" "$M/drivers/iio" "$M/drivers/infiniband"
# The rootfs's own kmod, which reads its kernel's compressed modules; the
# link to the kernel the package keeps beside them points at what /boot no
# longer holds
if [ -L "$ROOTFS/lib/modules/$KVER/vmlinuz" ]; then
    rm -f "$ROOTFS/lib/modules/$KVER/vmlinuz"
fi
chroot "$ROOTFS" depmod "$KVER"

# The installer's own files
install -m 0755 /installer/init "$ROOTFS/init"
install -d "$ROOTFS/usr/lib/nos-installer" "$ROOTFS/etc/nos-installer"
install -m 0755 /installer/sysinit /installer/run-setup "$ROOTFS/usr/lib/nos-installer/"
install -m 0755 /installer/nos-setup "$ROOTFS/usr/sbin/nos-setup"
echo nos-installer > "$ROOTFS/etc/hostname"
apk info -q -v --root "$ROOTFS" | sort > /tmp/packages.txt
cp /tmp/packages.txt "$ROOTFS/etc/nos-installer/packages"
if [ -f /work/in/answers ]; then
    install -m 0600 /work/in/answers "$ROOTFS/etc/nos-installer/answers"
fi

mkdir -p "$TREE/boot/grub"
(cd "$ROOTFS" && find . | sort | cpio -o -H newc --quiet) | gzip -9 > "$TREE/boot/initramfs"

# ---- the system image, and its kernel ----------------------------------

rm -rf /tmp/image /tmp/kernel
mkdir -p /tmp/image/usr/share/nos
cp /work/in/nos-x86_64.squashfs /tmp/image/usr/share/nos/
(cd /tmp/image/usr/share/nos && sha256sum nos-x86_64.squashfs > SHA256SUMS)
# Not compressed again: a squashfs is
(cd /tmp/image && find . | sort | cpio -o -H newc --quiet) > "$TREE/boot/nos.img"
unsquashfs -no-progress -d /tmp/kernel /work/in/nos-x86_64.squashfs boot/kernel64.elf VERSION >/dev/null
cp /tmp/kernel/boot/kernel64.elf "$TREE/boot/kernel64.elf"

cp /tmp/vmlinuz "$TREE/boot/vmlinuz"
cp /installer/grub.cfg "$TREE/boot/grub/grub.cfg"
{
    echo "nos installer"
    sed 's/^/image /' /tmp/kernel/VERSION
    echo "alpine $(cat "$ROOTFS/etc/alpine-release") kernel $KVER"
} > "$TREE/VERSION"

rm -f /work/nos-installer-x86_64.iso
grub-mkrescue -o /work/nos-installer-x86_64.iso "$TREE" -- -volid NOS_INSTALLER >/dev/null 2>&1 ||
    { grub-mkrescue -o /work/nos-installer-x86_64.iso "$TREE" -- -volid NOS_INSTALLER; exit 1; }
cp /tmp/packages.txt /work/packages.txt
echo "mkiso.sh: kernel $KVER, initramfs $(($(wc -c < "$TREE/boot/initramfs") / 1048576)) MiB," \
    "image $(($(wc -c < "$TREE/boot/nos.img") / 1048576)) MiB, $(wc -l < /tmp/packages.txt) packages"
