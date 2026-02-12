#!/bin/bash
# Build nos.iso inside Docker (for Mac or when grub-mkrescue isn't available).
set -e
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
docker build --platform linux/amd64 -t nos-builder $PROJECT_ROOT
docker run --platform linux/amd64 --rm -e VERSION -v "$PROJECT_ROOT:/src" -w /src nos-builder bash -c 'make clean && make all'
echo "Built nos.iso and kernel64.elf (use kernel64.elf in GDB for symbols)"
