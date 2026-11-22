#!/bin/bash
set -eu

EDK2_URL=https://github.com/tianocore/edk2.git
EDK2_TAG=edk2-stable202608
EDK2_COMMIT=2970e5699ba6267f3384ffab20f96647578aebc8
ROOT=/src
WORK=$ROOT/out/cloudhv
SRC=$WORK/edk2

CHECK=0
case "${1:-}" in
    --check) CHECK=1 ;;
    "") ;;
    *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

mkdir -p "$WORK"
if [ ! -d "$SRC/.git" ]; then
    git clone --depth 1 --branch "$EDK2_TAG" --no-recurse-submodules "$EDK2_URL" "$SRC"
fi
[ "$(git -C "$SRC" remote get-url origin)" = "$EDK2_URL" ]
git -C "$SRC" fetch --depth 1 origin "refs/tags/$EDK2_TAG:refs/tags/$EDK2_TAG"
git -C "$SRC" checkout --detach "$EDK2_COMMIT"
[ "$(git -C "$SRC" rev-parse HEAD)" = "$EDK2_COMMIT" ]
git -C "$SRC" submodule sync --recursive
git -C "$SRC" submodule update --init --depth 1 --force
git -C "$SRC" submodule foreach --quiet 'git submodule deinit -f --all'
[ -z "$(git -C "$SRC" status --porcelain --untracked-files=no)" ]

export LC_ALL=C
export TZ=UTC
export PYTHONHASHSEED=0
export SOURCE_DATE_EPOCH="$(git -C "$SRC" show -s --format=%ct "$EDK2_COMMIT")"
export WORKSPACE="$SRC"
export EDK_TOOLS_PATH="$SRC/BaseTools"
JOBS=${JOBS:-$(nproc)}

cd "$SRC"
make -C BaseTools -j"$JOBS"
set +u
. ./edksetup.sh BaseTools > /dev/null
set -u

FLAGS="-D NETWORK_ISCSI_ENABLE=FALSE -D SECURE_BOOT_ENABLE=FALSE -D TPM2_ENABLE=FALSE -D SMM_REQUIRE=FALSE -D BUILD_SHELL=TRUE -D FD_SIZE_4MB=TRUE"
for target in RELEASE DEBUG; do
    rm -rf "$SRC/Build/CloudHvX64/${target}_GCC"
    EXTRA=
    if [ "$target" = DEBUG ]; then
        EXTRA="-D DEBUG_ON_SERIAL_PORT=TRUE"
    fi
    build -p OvmfPkg/CloudHv/CloudHvX64.dsc -a X64 -b "$target" -t GCC -n "$JOBS" $FLAGS $EXTRA
    firmware="$SRC/Build/CloudHvX64/${target}_GCC/FV/CLOUDHV.fd"
    [ -f "$firmware" ]
    [ "$(wc -c < "$firmware")" -eq 4194304 ]
done

install -m 0644 "$SRC/Build/CloudHvX64/RELEASE_GCC/FV/CLOUDHV.fd" "$WORK/CLOUDHV-release.fd"
install -m 0644 "$SRC/Build/CloudHvX64/DEBUG_GCC/FV/CLOUDHV.fd" "$WORK/CLOUDHV-debug.fd"
if [ "$CHECK" -eq 1 ]; then
    cmp "$WORK/CLOUDHV-release.fd" "$ROOT/build/image/CLOUDHV.fd"
    echo "CLOUDHV.fd matches $EDK2_TAG ($EDK2_COMMIT)"
else
    install -m 0644 "$WORK/CLOUDHV-release.fd" "$ROOT/build/image/CLOUDHV.fd"
    sha256sum "$ROOT/build/image/CLOUDHV.fd" "$WORK/CLOUDHV-debug.fd"
fi
