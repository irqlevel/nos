#!/bin/bash
# tests/gpu-passthrough-test.py --driver nvidia, on the machine with the GPU:
# the guest's disk and nos's root to boot it from. In chroots under <dir>,
# none of it the machine's own packages:
#
#   - the guest's root: Ubuntu noble, minbase, with NVIDIA's userspace of the
#     same version as the prebuilt open modules for the machine's kernel --
#     which the guest boots -- and those modules (made once, kept);
#   - three of cuda-samples (deviceQuery, vectorAdd, matrixMul), built with
#     NVIDIA's CUDA toolkit for the GPU's architecture in a copy of that root,
#     the CUDA runtime linked in statically (made once, kept);
#   - the guest's disk: that root, the samples, nvidia-guest-init as /init;
#   - nos's root: hv.ko, the kernel, the guest's disk, /etc/rc giving the
#     guest the GPU.
#
#   make-nvidia-root.sh <dir> <the GPU in nos> <guest MiB> <secs> [branch] [cuda] [sm]
set -eu
D=$1
PCI=$2
MEM=$3
SECS=$4
BRANCH=${5:-580}
CUDA=${6:-12-9}
SM=${7:-86}
SAMPLES_TAG=v${CUDA/-/.}
K=$(uname -r)
# The machine's own archive, as its sources name it (deb822's URIs, or a
# deb line): what debootstrap and the chroots' apt fetch from
MIRROR=$(awk '/^URIs:/ { print $2; exit }' /etc/apt/sources.list.d/*.sources 2>/dev/null || true)
[ -n "$MIRROR" ] || MIRROR=$(awk '/^deb https?:/ { print $2; exit }' /etc/apt/sources.list 2>/dev/null || true)
MIRROR=${MIRROR:-http://archive.ubuntu.com/ubuntu/}
N=$D/nvidia
G=$N/guest
B=$N/build
mkdir -p $N
trap 'echo "make-nvidia-root: failed at line $LINENO"; tail -5 $N/*.log 2>/dev/null' ERR

binds() {
    for m in proc sys dev dev/pts; do
        mountpoint -q $1/$m || mount --bind /$m $1/$m
    done
}
unbinds() {
    for m in dev/pts dev sys proc; do
        mountpoint -q $1/$m && umount -l $1/$m
    done
    true
}
trap 'unbinds $G; unbinds $B' EXIT
export DEBIAN_FRONTEND=noninteractive

if [ ! -e $N/guest.done ]; then
    rm -rf $G
    debootstrap --variant=minbase noble $G $MIRROR > $N/debootstrap.log
    cat > $G/etc/apt/sources.list <<EOF
deb $MIRROR noble main restricted universe multiverse
deb $MIRROR noble-updates main restricted universe multiverse
deb http://security.ubuntu.com/ubuntu noble-security main restricted universe multiverse
EOF
    cp /etc/resolv.conf $G/etc/resolv.conf
    binds $G
    # The modules, without dpkg: their ./lib over the root's lib -> usr/lib
    # link -- kept a link, or the root loses its merged /usr
    (cd $N && apt-get download -q linux-modules-nvidia-$BRANCH-open-$K > /dev/null)
    dpkg-deb --fsys-tarfile $N/linux-modules-nvidia-$BRANCH-open-${K}_*.deb \
        | tar -x --keep-directory-symlink -C $G
    V=$(modinfo -F version $G/lib/modules/$K/kernel/nvidia-$BRANCH-open/nvidia.ko)
    chroot $G apt-get update -q > /dev/null
    UV=$(chroot $G apt-cache madison nvidia-utils-$BRANCH | awk -v v="$V" '$3 ~ "^"v"-" { print $3; exit }')
    if [ -z "$UV" ]; then
        echo "make-nvidia-root: no userspace of the modules' $V"
        exit 1
    fi
    chroot $G apt-get install -y -q --no-install-recommends nvidia-utils-$BRANCH=$UV \
        libnvidia-compute-$BRANCH=$UV kmod pciutils > $N/guest-apt.log
    unbinds $G
    echo "$V" > $N/guest.done
fi

if [ ! -e $N/samples.done ]; then
    rm -rf $B $N/samples $N/cuda-samples
    cp -a $G $B
    binds $B
    chroot $B apt-get install -y -q --no-install-recommends ca-certificates wget > $N/build-apt.log
    chroot $B wget -q -O /tmp/cuda-keyring.deb \
        https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2404/x86_64/cuda-keyring_1.1-1_all.deb
    chroot $B dpkg -i /tmp/cuda-keyring.deb >> $N/build-apt.log
    chroot $B apt-get update -q >> $N/build-apt.log
    chroot $B apt-get install -y -q --no-install-recommends cuda-nvcc-$CUDA cuda-cudart-dev-$CUDA \
        cuda-profiler-api-$CUDA g++ >> $N/build-apt.log
    git clone -q --depth 1 --branch $SAMPLES_TAG https://github.com/NVIDIA/cuda-samples $B/root/cuda-samples 2>/dev/null
    chroot $B bash -c "cd /root/cuda-samples && mkdir -p out &&
        NVCC='/usr/local/cuda/bin/nvcc -O2 -arch=sm_$SM -I Common' &&
        \$NVCC -o out/deviceQuery Samples/1_Utilities/deviceQuery/deviceQuery.cpp &&
        \$NVCC -o out/vectorAdd Samples/0_Introduction/vectorAdd/vectorAdd.cu &&
        \$NVCC -o out/matrixMul Samples/0_Introduction/matrixMul/matrixMul.cu"
    mkdir -p $N/samples
    cp $B/root/cuda-samples/out/deviceQuery $B/root/cuda-samples/out/vectorAdd $B/root/cuda-samples/out/matrixMul \
        $N/samples/
    unbinds $B
    rm -rf $B
    echo "$CUDA" > $N/samples.done
fi

unbinds $G
rm -rf $G/opt/cuda-samples
mkdir -p $G/opt/cuda-samples
cp $N/samples/* $G/opt/cuda-samples/
cp $D/nvidia-guest-init $G/init
chmod +x $G/init
used=$(du -sm $G | cut -f1)
R=$N/rootdir
rm -rf $R $N/root.img
mkdir -p $R/etc
mke2fs -q -t ext4 -L guest -d $G $R/guest.img $((used * 13 / 10 + 256))M
cp $D/hv.ko $R/
cp /boot/vmlinuz-$K $R/vmlinuz
cat > $R/etc/rc <<EOF
# tests/gpu-passthrough-test.py --driver nvidia: nos gives the GPU to a Linux
# guest running NVIDIA's own driver
insmod /hv.ko
hv on
hv boot /vmlinuz mem=$MEM cpus=2 secs=$SECS pci=$PCI disk=/guest.img cmdline=console=ttyS0 root=/dev/vda rw init=/init nokaslr
EOF
$D/mkrootfs.sh $N/root.img $(( $(du -sm $R | cut -f1) + 512 )) $R > /dev/null
rm -rf $R
echo "make-nvidia-root: driver $(cat $N/guest.done), CUDA $(cat $N/samples.done), kernel $K"
