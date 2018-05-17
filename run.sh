#!/bin/bash -xv
dd if=/dev/zero of=scsi-disk.img bs=4k count=1024
qemu-system-x86_64 -cdrom nos.iso -serial file:nos.log -enable-kvm -smp 4 -s -vga std -vnc :0 -k en-us -device virtio-scsi-pci,id=scsi -device scsi-hd,drive=hd -drive if=none,id=hd,file=scsi-disk.img,format=raw
