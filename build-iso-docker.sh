#!/bin/bash
# Build nos.iso inside Docker (for Mac or when grub-mkrescue isn't available).
set -e
cd "$(dirname "$0")"
docker build --platform linux/amd64 -t nos-builder .
docker run --platform linux/amd64 --rm -v "$(pwd):/src" -w /src nos-builder make nocheck
echo "Built nos.iso"
