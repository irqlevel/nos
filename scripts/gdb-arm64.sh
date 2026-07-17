#!/bin/bash
# Attach GDB to the arm64 kernel. Start QEMU with -s (GDB stub on :1234):
#   ./scripts/qemu-arm64.sh -s -S     (-S also halts at reset)
# then run this. Needs a GDB with aarch64 support (gdb-multiarch / lldb).
cd "$(dirname "$0")/.."
if command -v gdb-multiarch >/dev/null 2>&1; then
    exec gdb-multiarch -x scripts/gdb-debug-arm64
elif command -v aarch64-elf-gdb >/dev/null 2>&1; then
    exec aarch64-elf-gdb -x scripts/gdb-debug-arm64
else
    echo "No aarch64 GDB found (install gdb-multiarch or aarch64-elf-gdb)."
    echo "Alternatively use LLDB: lldb bin/kernel-arm64.elf then"
    echo "  gdb-remote 1234"
    exit 1
fi
