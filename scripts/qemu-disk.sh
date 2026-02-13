#!/bin/bash
cd "$(dirname "$0")/.."
qemu-system-x86_64 \
    -smp 2 \
    -m 8G \
    -drive file=nos.qcow2,format=qcow2,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off -netdev user,id=net0 \
    -s -nographic
