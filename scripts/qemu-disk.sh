#!/bin/bash
cd "$(dirname "$0")/.."
[ -f secondary-scsi-disk.qcow2 ] || qemu-img create -f qcow2 secondary-scsi-disk.qcow2 1G
qemu-system-x86_64 \
    -smp 4 \
    -m 8G \
    -drive file=nos.qcow2,format=qcow2,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off,bootindex=0 \
    -drive file=secondary-scsi-disk.qcow2,format=qcow2,id=scsi0,if=none \
    -device virtio-scsi-pci,id=scsi,disable-legacy=off,disable-modern=on \
    -device scsi-hd,drive=scsi0,bus=scsi.0,channel=0,scsi-id=0,lun=0 \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off -netdev user,id=net0 \
    -device virtio-rng-pci \
    -s -nographic
