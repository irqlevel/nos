#!/bin/bash
# tests/gpu-passthrough-test.py, on the machine with the GPU: the guest -- the
# machine's own kernel, and an initramfs of busybox, nouveau and the modules
# it needs, the GPU's firmware and fence.c -- and nos's root to boot it from:
# hv.ko, the kernel, the initramfs, and /etc/rc giving the guest the GPU.
#
#   make-root.sh <dir> <the GPU in nos> <chip> <guest MiB> <secs>
#
# <dir> holds what the runner copied: hv.ko, mkrootfs.sh, guest-init,
# fence.c. <chip> is the firmware's directory under /lib/firmware/nvidia,
# copied with its links followed (ga106's gsp is ga102's).
set -eu
D=$1
PCI=$2
CHIP=$3
MEM=$4
SECS=$5
K=$(uname -r)
W=$D/guest
rm -rf "$W"
I=$W/initramfs
mkdir -p "$I"/bin "$I"/proc "$I"/sys "$I"/dev "$I"/lib/modules/$K "$I"/lib/firmware/nvidia
cp /usr/bin/busybox "$I"/bin/busybox
gcc -O2 -static -o "$I"/bin/fence "$D"/fence.c
mknod -m 600 "$I"/dev/console c 5 1
mknod -m 666 "$I"/dev/null c 1 3
# nouveau and what it needs, in the order modprobe would load them,
# decompressed: busybox's insmod reads no zstd
: > "$I"/modules
for m in $(modprobe --show-depends nouveau | awk '/^insmod/ { print $2 }'); do
    name=$(basename "${m%.zst}")
    case "$m" in
        *.zst) zstd -dqf "$m" -o "$I/lib/modules/$K/$name" ;;
        *) cp "$m" "$I/lib/modules/$K/$name" ;;
    esac
    echo "$name" >> "$I"/modules
done
cp -rL /lib/firmware/nvidia/"$CHIP" "$I"/lib/firmware/nvidia/
cp "$D"/guest-init "$I"/init
chmod +x "$I"/init
(cd "$I" && find . | cpio -o -H newc --quiet) > "$W"/initrd

R=$W/rootdir
mkdir -p "$R"/etc
cp "$D"/hv.ko "$R"/
cp /boot/vmlinuz-"$K" "$R"/vmlinuz
cp "$W"/initrd "$R"/initrd
cat > "$R"/etc/rc <<EOF
# tests/gpu-passthrough-test.py: nos gives the GPU to a Linux guest
insmod /hv.ko
hv on
hv boot /vmlinuz initrd=/initrd mem=$MEM cpus=2 secs=$SECS pci=$PCI cmdline=console=ttyS0 rdinit=/init nokaslr
EOF
"$D"/mkrootfs.sh "$W"/root.img 256 "$R" > /dev/null
echo "make-root: $(du -h "$W"/initrd | cut -f1) initramfs, $(wc -l < "$I"/modules) modules, kernel $K"
