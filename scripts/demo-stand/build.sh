#!/bin/bash
# What the demo stand runs, built from the tree as git has it: the panel's
# bundle (scripts/panel-bundle.py, into out/demo), then -- in the nos-builder
# image -- the kernel and modules, nos-test.iso with the UDP shell the
# neighbours are driven by, and the system image the EX44 boots, its
# /lib/panel/bundle naming the bundle at the AX41's HTTP server, where
# ax41-up.sh serves it, and its SHA-256.
#
#   scripts/demo-stand/build.sh
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
AX41=${AX41:-65.109.93.213}
cd "$ROOT"
mkdir -p out/demo
python3 scripts/panel-bundle.py --out out/demo | tee out/demo/bundle.txt
BUNDLE=$(sed -n 's/^panel-bundle: \([^,]*\),.*/\1/p' out/demo/bundle.txt)
[ -f "$BUNDLE" ] || { echo "no bundle made"; exit 1; }
NAME=$(basename "$BUNDLE")
REL=out/demo/$NAME
# What nos says it is (`version`, the system provider's info, the panel's
# host pages): worked out here -- the builder image has no git, and a
# worktree's .git is a file naming a directory it does not mount
VERSION=${VERSION:-$(git describe --tags --exact-match 2>/dev/null || echo dev)}
GIT_DESC=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
if git status --porcelain -uno 2>/dev/null | grep -q .; then GIT_DESC=$GIT_DESC-dirty; fi
docker run --platform linux/amd64 --rm -e "VERSION=$VERSION" -e "GIT_DESC=$GIT_DESC" -v "$ROOT":/src -w /src \
    nos-builder sh -c \
    "make nocheck && make nos-test.iso TEST_ISO_ARGS=udpshell=9100 && \
     make image PANEL_BUNDLE=$REL PANEL_BUNDLE_URL=http://$AX41:39299/$NAME"
ls -la nos-x86_64.squashfs nos-test.iso "$BUNDLE"
echo "-- next: ax41-up.sh $BUNDLE; then ex44-deploy.sh nos-x86_64.squashfs"
