#!/bin/bash
# Build the nos system image, nos-x86_64.squashfs (docs/install.md), inside
# Docker: the kernel and its modules, then the image made of them.
#
#   scripts/build-image.sh           what changed since the last build
#   scripts/build-image.sh --clean   everything, as a release is built
#
# Env: VERSION (default: the tag HEAD is on, else "dev"), GIT_DESC (default:
# the commit, "-dirty" with uncommitted changes) -- worked out here, since
# the builder image has no git.
set -e
PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

CLEAN=
case "${1:-}" in
    --clean) CLEAN="make clean && " ;;
    "") ;;
    *) echo "usage: $0 [--clean]" >&2; exit 2 ;;
esac

# The image is built when there is none; after a change to the Dockerfile,
# docker build -t nos-builder . makes it again
docker image inspect nos-builder > /dev/null 2>&1 ||
    docker build --platform linux/amd64 -t nos-builder "$PROJECT_ROOT"

VERSION="${VERSION:-$(git -C "$PROJECT_ROOT" describe --tags --exact-match 2>/dev/null || echo dev)}"
if [ -z "${GIT_DESC:-}" ]; then
    GIT_DESC="$(git -C "$PROJECT_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    if git -C "$PROJECT_ROOT" status --porcelain -uno 2>/dev/null | grep -q .; then
        GIT_DESC="$GIT_DESC-dirty"
    fi
fi

# make rust first: the kernel's staticlib is not rebuilt for a change to a
# .rs file otherwise (docs/build.md)
docker run --platform linux/amd64 --rm -e "VERSION=$VERSION" -e "GIT_DESC=$GIT_DESC" \
    -v "$PROJECT_ROOT:/src" -w /src nos-builder \
    bash -c "${CLEAN}make rust && make image"

IMAGE="$PROJECT_ROOT/nos-x86_64.squashfs"
if command -v sha256sum > /dev/null 2>&1; then
    SUM=$(sha256sum "$IMAGE" | cut -d' ' -f1)
else
    SUM=$(shasum -a 256 "$IMAGE" | cut -d' ' -f1)
fi
echo "Built nos-x86_64.squashfs ($VERSION, $GIT_DESC): $(wc -c < "$IMAGE" | tr -d ' ') bytes, sha256 $SUM"
