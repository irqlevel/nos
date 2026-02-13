#!/bin/bash
cd "$(dirname "$0")/.."
[ -f scsi-disk0.qcow2 ] || qemu-img create -f qcow2 scsi-disk0.qcow2 1G
[ -f scsi-disk1.qcow2 ] || qemu-img create -f qcow2 scsi-disk1.qcow2 512M
[ -f scsi-disk2.qcow2 ] || qemu-img create -f qcow2 scsi-disk2.qcow2 256M
qemu-system-x86_64 \
    -smp 4 \
    -m 8G \
    -drive file=nos.qcow2,format=qcow2,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off,bootindex=0 \
    -drive file=scsi-disk0.qcow2,format=qcow2,id=scsi0,if=none \
    -drive file=scsi-disk1.qcow2,format=qcow2,id=scsi1,if=none \
    -drive file=scsi-disk2.qcow2,format=qcow2,id=scsi2,if=none \
    -device virtio-scsi-pci,id=scsi,disable-legacy=off,disable-modern=on \
    -device scsi-hd,drive=scsi0,bus=scsi.0,channel=0,scsi-id=0,lun=0 \
    -device scsi-hd,drive=scsi1,bus=scsi.0,channel=0,scsi-id=3,lun=0 \
    -device scsi-hd,drive=scsi2,bus=scsi.0,channel=0,scsi-id=5,lun=0 \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off -netdev user,id=net0 \
    -device virtio-rng-pci \
    -s -nographic
