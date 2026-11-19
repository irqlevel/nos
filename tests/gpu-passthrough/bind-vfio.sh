#!/bin/bash
# tests/gpu-passthrough-test.py, on the machine with the GPU: every function
# of the GPU's IOMMU group -- the GPU and its audio function -- taken from
# whatever driver has it and given to vfio-pci, which QEMU's vfio-pci device
# opens the group through. A reboot of the machine undoes it (nouveau, or
# nothing, for the GPU; snd_hda_intel for its audio). Says each function it
# moved, and fails naming one that did not end up vfio-pci's.
#
#   bind-vfio.sh <the GPU, as 0000:01:00.0>
set -eu
GPU=$1
modprobe vfio-pci
for f in /sys/bus/pci/devices/"$GPU"/iommu_group/devices/*; do
    b=$(basename "$f")
    drv=""
    if [ -e "$f/driver" ]; then
        drv=$(basename "$(readlink "$f/driver")")
    fi
    [ "$drv" = vfio-pci ] && continue
    if [ -n "$drv" ]; then
        echo "$b" > "$f/driver/unbind"
    fi
    echo vfio-pci > "$f/driver_override"
    echo "$b" > /sys/bus/pci/drivers_probe
    if [ ! -e "$f/driver" ] || [ "$(basename "$(readlink "$f/driver")")" != vfio-pci ]; then
        echo "bind-vfio: $b is not vfio-pci's"
        exit 1
    fi
    echo "bind-vfio: $b vfio-pci's, was ${drv:-unbound}"
done
