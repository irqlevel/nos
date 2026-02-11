#!/bin/bash
cd "$(dirname "$0")/.."
qemu-system-x86_64 -smp 2 -drive file=nos.qcow2,if=virtio,format=qcow2 \
    -device virtio-net-pci,netdev=net0 -netdev user,id=net0 \
    -s -nographic
