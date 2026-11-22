#!/bin/bash
# Build OVMF's CloudHv firmware from the pinned edk2 source in Docker.
# The release replaces build/image/CLOUDHV.fd; the debug build stays in
# out/cloudhv for firmware bring-up. --check only compares the release.
set -e
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

MODE=
case "${1:-}" in
    --check) MODE=--check ;;
    "") ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

docker image inspect nos-cloudhv-builder > /dev/null 2>&1 ||
    docker build --platform linux/amd64 -t nos-cloudhv-builder "$PROJECT_ROOT/build/cloudhv"

docker run --platform linux/amd64 --rm -e "JOBS=${JOBS:-}" \
    -v "$PROJECT_ROOT:/src" -w /src nos-cloudhv-builder \
    bash build/cloudhv/build.sh "$MODE"
