#!/bin/bash -xv
NCPU="$(grep -c ^processor /proc/cpuinfo)"
dd if=/dev/zero of=scsi-disk.img bs=4k count=1024

for i in `seq 1 $1`
do
echo $i
qemu-system-x86_64 -cdrom nos.iso -serial file:nos.log -enable-kvm -s -vga std -vnc :0 -k en-us -device virtio-scsi-pci,id=scsi -device scsi-hd,drive=hd -drive if=none,id=hd,file=scsi-disk.img,format=raw -device isa-debug-exit,iobase=0xf4,iosize=0x04 -smp $2
done
