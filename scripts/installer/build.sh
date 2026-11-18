#!/bin/bash
# The nos installer ISO (docs/install.md, "From an ISO"):
# nos-installer-x86_64.iso, which a server boots from its provider's virtual
# media or from a USB stick, BIOS or UEFI, to make a whole disk nos's own.
# A small Linux -- Alpine, of one pinned release -- runs the dialog,
# nos-setup, over the nos-install of the system image the ISO carries as it
# was built (nos-install disk); the ISO's menu also boots that image's
# kernel by itself, to see what nos finds on the machine, writing nothing.
#
#   scripts/installer/build.sh [--image FILE] [--answers FILE] [--out FILE]
#
#   --image FILE    the system image (nos-x86_64.squashfs at the top of the
#                   tree unless given: make image, or scripts/build-image.sh)
#   --answers FILE  answers baked in (scripts/installer/answers.example): an
#                   ISO that installs with no questions -- for a test, a fleet
#   --out FILE      where the ISO goes (nos-installer-x86_64.iso at the top)
#
# Runs Docker. The Alpine packages are kept in out/installer/apk-cache, so
# a build after the first downloads nothing that has not changed.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# The Alpine release the installer runs, and builds with: its packages come
# from that release's branch alone (mkiso.sh)
ALPINE_IMAGE=alpine:3.24
IMAGE=$ROOT/nos-x86_64.squashfs
ANSWERS=
OUT=$ROOT/nos-installer-x86_64.iso

die() { echo "build.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image) [ $# -ge 2 ] || die "--image FILE"; IMAGE=$2; shift ;;
        --answers) [ $# -ge 2 ] || die "--answers FILE"; ANSWERS=$2; shift ;;
        --out) [ $# -ge 2 ] || die "--out FILE"; OUT=$2; shift ;;
        -h|--help) sed -n '2,/^$/s/^# \{0,1\}//p' "$0"; exit 0 ;;
        *) die "unknown argument $1 (--help)" ;;
    esac
    shift
done

[ -f "$IMAGE" ] || die "no $IMAGE: make image, or scripts/build-image.sh"
[ -z "$ANSWERS" ] || [ -f "$ANSWERS" ] || die "no $ANSWERS"
command -v docker >/dev/null 2>&1 || die "this needs Docker"

WORK=$ROOT/out/installer
mkdir -p "$WORK/apk-cache" "$WORK/in"
rm -f "$WORK/in/answers" "$WORK/nos-installer-x86_64.iso"
cp "$IMAGE" "$WORK/in/nos-x86_64.squashfs"
[ -z "$ANSWERS" ] || cp "$ANSWERS" "$WORK/in/answers"

# --privileged: the rootfs's packages run their install scripts in a chroot
# with /proc and /dev, which takes mounts
docker run --platform linux/amd64 --rm --privileged \
    -v "$HERE:/installer:ro" -v "$WORK:/work" "$ALPINE_IMAGE" sh /installer/mkiso.sh

[ -f "$WORK/nos-installer-x86_64.iso" ] || die "no ISO made"
mkdir -p "$(dirname "$OUT")"
cp "$WORK/nos-installer-x86_64.iso" "$OUT"
echo "build.sh: $OUT, $(($(wc -c < "$OUT") / 1048576)) MiB"
