#!/bin/bash
# Build nos.iso inside Docker (for Mac or when grub-mkrescue isn't available).
set -e
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
docker build --platform linux/amd64 -t nos-builder $PROJECT_ROOT
# The builder image has no git, so the revision is worked out here and passed
# in. -uno counts tracked changes only: a stray untracked file is not a change
# to what was built.
VERSION="${VERSION:-$(git -C "$PROJECT_ROOT" describe --tags --exact-match 2>/dev/null || echo dev)}"
GIT_DESC="$(git -C "$PROJECT_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
if git -C "$PROJECT_ROOT" status --porcelain -uno 2>/dev/null | grep -q .; then
    GIT_DESC="$GIT_DESC-dirty"
fi

docker run --platform linux/amd64 --rm -e "VERSION=$VERSION" -e "GIT_DESC=$GIT_DESC" -v "$PROJECT_ROOT:/src" -w /src nos-builder bash -c 'make clean && make all'
echo "Built nos.iso and kernel64.elf (use kernel64.elf in GDB for symbols)"
