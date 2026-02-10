#!/bin/bash
cd "$(dirname "$0")/.."
qemu-system-x86_64 -smp 2 -cdrom nos.iso -serial file:nos.log -s -vga std
