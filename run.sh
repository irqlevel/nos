#!/bin/bash -xv
qemu-system-x86_64 -cdrom nos.iso -serial file:nos.log -enable-kvm -smp 4 -s
