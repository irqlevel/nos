#!/bin/bash
# What tests/hv-passthrough-test.py's guest is made of, into <out>: its kernel
# -- Linux of the version below from kernel.org, checked by its digest,
# tinyconfig with guest.config merged in -- and nvmepoke.c, static:
#
#     tests/hv-passthrough/build-guest.sh <out>   -> <out>/bzImage, <out>/nvmepoke
#
# On x86-64 Linux with gcc, make, flex, bison, bc and libelf; elsewhere with an
# x86-64 cross compiler, CROSS_COMPILE=x86_64-linux-gnu- (docs/testing.md has
# a Docker recipe for a Mac). The source and the build are kept in <out>, so
# a second run builds nothing that did not change.
set -eu
if [ $# -ne 1 ]; then
    echo "usage: $0 <out>" >&2
    exit 2
fi
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$1"
OUT=$(cd "$1" && pwd)
VERSION=6.18.54
SHA256=9df30b02dd8102bbd0be52556288ef6889ddbe7f1ddb96fbf847d0becf3eacac
CROSS=${CROSS_COMPILE:-}
CC=${CROSS}gcc

cd "$OUT"
if [ ! -d linux-$VERSION ]; then
    [ -f linux-$VERSION.tar.xz ] || curl -fsSLO https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$VERSION.tar.xz
    echo "$SHA256  linux-$VERSION.tar.xz" | sha256sum -c --quiet
    tar xf linux-$VERSION.tar.xz
    rm linux-$VERSION.tar.xz
fi
K="make -C linux-$VERSION ARCH=x86_64 CROSS_COMPILE=$CROSS O=$OUT/build"
$K tinyconfig > /dev/null
linux-$VERSION/scripts/kconfig/merge_config.sh -m -O build build/.config "$HERE"/guest.config > /dev/null
$K olddefconfig > /dev/null
for option in $(grep -o '^CONFIG_[A-Z0-9_]*=y' "$HERE"/guest.config); do
    grep -qx "$option" build/.config || { echo "build-guest: $option did not stay set" >&2; exit 1; }
done
$K -j"$(nproc)" bzImage > build.log 2>&1 || { tail -30 build.log >&2; exit 1; }
cp build/arch/x86/boot/bzImage "$OUT"/bzImage
$CC -static -O2 -Wall -Wextra -o "$OUT"/nvmepoke "$HERE"/nvmepoke.c
echo "build-guest: $OUT/bzImage (Linux $VERSION), $OUT/nvmepoke"
