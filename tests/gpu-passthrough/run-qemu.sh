#!/bin/bash
# tests/gpu-passthrough-test.py, on the machine with the GPU: nos under
# QEMU/KVM with QEMU's VT-d -- caching mode, so VFIO follows the domains nos
# makes -- in front of the real GPU, bound to vfio-pci and behind a root port
# of its own: the GPU is 01:00.0 in nos. That function alone: behind a vIOMMU
# each function has an address space of its own, and VFIO wants one for the
# whole group ("group used in multiple address spaces"), so the others stay
# bound to vfio-pci, unused.
#
#   run-qemu.sh <iso> <serial log> <the GPU on the machine> <nos's memory> [qemu args...]
set -u
ISO=$1
LOG=$2
GPU=$3
MEM=$4
shift 4
exec qemu-system-x86_64 \
    -machine q35,accel=kvm,kernel-irqchip=split \
    -cpu host \
    -m "$MEM" \
    -smp 4 \
    -device intel-iommu,intremap=on,caching-mode=on \
    -device pcie-root-port,id=rp1,chassis=1,slot=1 \
    -device vfio-pci,host="$GPU",bus=rp1,addr=0.0 \
    -display none \
    -cdrom "$ISO" \
    -serial "file:$LOG" \
    -netdev user,id=n0 \
    -device virtio-net-pci,netdev=n0 \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04 \
    "$@"
