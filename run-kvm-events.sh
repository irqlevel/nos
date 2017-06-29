#!/bin/bash -xv
trap ctrl_c INT

function ctrl_c() {
	scripts/kvm-events-stop.sh
	sync
	sync
}

scripts/kvm-events-start.sh
qemu-system-x86_64 -cdrom nos.iso -serial file:nos.log -enable-kvm -smp 4 -s
