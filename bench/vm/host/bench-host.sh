#!/bin/bash
# The server's Linux side of the KVM-vs-nos comparison (bench/vm/README.md):
# one disk image, made here, run under KVM here and under nos's hv after the
# next boot. Run by the workstation's scripts through remote.sh, which hand it
# the server's own values in the environment:
#
#   NOS_ROOT           the partition nos's root filesystem is on, by UUID --
#                      where both runs keep the guest's disk
#   VCPU_APIC          the APIC IDs of the CPUs nos's hv puts a lone 4-CPU guest
#                      on, its vCPUs 0-3 in order (`hv list` says them)
#   IO_APIC            the APIC ID of the CPU its disks' task is put on
#   E_CORE_APIC_FIRST  the lowest APIC ID of the slow cores, where the rest of
#                      QEMU is kept
#
# The defaults are the Hetzner EX44's (an i5-13500: six P-cores and eight
# E-cores), which the published results are of.
#
#   bench-host.sh info       this host: kernel, QEMU, KVM's parameters, the CPUs
#   bench-host.sh prep       golden.raw: Ubuntu 24.04's cloud image (serial
#                            20261017), 8 GiB, provisioned by cloud-init under KVM
#   bench-host.sh check      a copy of it booted: the tools there, the suite QUICK
#   bench-host.sh arm        AUTORUN into golden.raw: its next boot runs the suite
#   bench-host.sh kvm        the suite under KVM, the image on nos's root partition
#   bench-host.sh nos-stage [step...]
#                            a fresh copy and its vm.toml onto nos's root, the
#                            kit's guest/run.sh in it, cut to the steps named
#   bench-host.sh device-stage <device> [step...]
#                            the same onto a partition or a disk of its own
#                            (/dev/disk/by-partuuid/...), given back whole
#                            first so that what is past the image reads as
#                            nothing -- the disk of both runs of the device's
#                            comparison, nos's guest given it by its id
#   bench-host.sh kvm-device <device>
#                            the suite under KVM, the device the guest's disk
#                            as it is: results-kvm-device
#
# Each prints what it did; the caller runs it detached and reads its exit
# status from a file (the Mac's ssh path cuts a quiet session).
set -euo pipefail
W=/root/bench
# The message quoted: an apostrophe bare in it opened a quoted string that
# took the lines below into the expansion's word, unrun, whenever NOS_ROOT
# was set
NOSROOT=${NOS_ROOT:?"NOS_ROOT: the partition nos's root is on, /dev/disk/by-uuid/..."}
MNT=/mnt/nosroot
IMG_URL=https://cloud-images.ubuntu.com/releases/noble/release-20261017/ubuntu-24.04-server-cloudimg-amd64.img
IMG_SHA=04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94
# Where nos's hv puts a guest of four CPUs on this machine with nothing else
# running (hv/src/place.rs: whole cores, the faster kind, the highest first,
# its disks' task on a slow core of its own) -- on the EX44 its vCPUs 0-3 on
# the CPUs of APIC IDs 41,40 (one P-core) and 33,32 (another), the disks on
# 62 (an E-core). KVM's threads are pinned to the same CPUs.
read -r -a VCPU_APIC <<< "${VCPU_APIC:-41 40 33 32}"
IO_APIC=${IO_APIC:-62}
E_CORE_APIC_FIRST=${E_CORE_APIC_FIRST:-48}

step() { echo "== $(date +%T) $*"; }

# cpu_of <apicid>: Linux's number of the CPU with that APIC ID
cpu_of() { awk -v a="$1" '/^processor/ {p = $3} /^apicid/ && $3 == a {print p; exit}' /proc/cpuinfo; }

# e_cores_but <cpu>: the E-cores (APIC IDs E_CORE_APIC_FIRST and up) but
# that one, as a list
e_cores_but() {
    awk -v x="$1" -v f="$E_CORE_APIC_FIRST" '/^processor/ {p = $3} /^apicid/ && $3 >= f && p != x {printf "%s%s", s, p; s = ","}' /proc/cpuinfo
}

mount_nosroot() {
    mkdir -p $MNT
    mountpoint -q $MNT || mount $NOSROOT $MNT
}

qemu_wait() { # qemu_wait <pid> <secs>: until the process is gone
    local pid=$1 secs=$2 t=0
    while kill -0 "$pid" 2>/dev/null; do
        [ $t -ge "$secs" ] && return 1
        sleep 5
        t=$((t + 5))
    done
}

info() {
    step "host"
    uname -a
    lsb_release -ds 2>/dev/null || true
    qemu-system-x86_64 --version | head -1
    grep -m1 'model name' /proc/cpuinfo
    grep -m1 microcode /proc/cpuinfo
    cat /proc/cmdline
    echo "driver $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_driver)"
    echo "governors: $(cat /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor | sort | uniq -c | xargs)"
    echo "epp: $(cat /sys/devices/system/cpu/cpu*/cpufreq/energy_performance_preference 2>/dev/null | sort | uniq -c | xargs)"
    echo "no_turbo: $(cat /sys/devices/system/cpu/intel_pstate/no_turbo 2>/dev/null)"
    echo "cpuidle: $(cat /sys/devices/system/cpu/cpuidle/current_driver) states: $(cat /sys/devices/system/cpu/cpu0/cpuidle/state*/name | xargs)"
    echo "thp: $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
    for p in halt_poll_ns halt_poll_ns_grow halt_poll_ns_shrink; do
        echo "kvm.$p=$(cat /sys/module/kvm/parameters/$p)"
    done
    for p in enable_apicv enable_ipiv preemption_timer ple_gap ple_window ept eptad pml; do
        echo "kvm_intel.$p=$(cat /sys/module/kvm_intel/parameters/$p 2>/dev/null)"
    done
    for a in "${VCPU_APIC[@]}" $IO_APIC; do echo "apic $a -> cpu $(cpu_of $a)"; done
    echo "e-cores but cpu $(cpu_of $IO_APIC): $(e_cores_but "$(cpu_of $IO_APIC)")"
    lscpu -e
    free -g
    df -h / /root
    cat /proc/mdstat
    lsblk -o NAME,SIZE,TYPE,FSTYPE,LABEL,MOUNTPOINT
    blkid $NOSROOT
}

prep() {
    mkdir -p $W
    cd $W
    step "tools"
    local need=()
    command -v qemu-system-x86_64 > /dev/null || need+=(qemu-system-x86)
    command -v qemu-img > /dev/null || need+=(qemu-utils)
    command -v cloud-localds > /dev/null || need+=(cloud-image-utils)
    command -v jq > /dev/null || need+=(jq)
    if [ ${#need[@]} -gt 0 ]; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y "${need[@]}"
    fi
    step "the cloud image"
    if [ ! -f noble.img ]; then
        curl -fsSLo noble.img.part $IMG_URL
        mv noble.img.part noble.img
    fi
    echo "$IMG_SHA  noble.img" | sha256sum -c
    qemu-img info noble.img | head -5
    step "golden.raw: raw, 8 GiB"
    rm -f golden.raw
    qemu-img convert -f qcow2 -O raw noble.img golden.raw
    qemu-img resize -f raw golden.raw 8G
    step "the seed"
    [ -f id_bench ] || ssh-keygen -q -t ed25519 -N '' -C bench -f id_bench
    python3 $W/host/make-user-data.py $W/guest id_bench.pub > user-data
    printf 'instance-id: bench-golden-20261031\nlocal-hostname: bench\n' > meta-data
    cloud-localds seed.iso user-data meta-data
    step "the provisioning boot (KVM, slirp, up to an hour)"
    rm -f prov-console.log
    timeout 3600 qemu-system-x86_64 -name prov -machine q35,accel=kvm -cpu host -smp 4 -m 8192 \
        -drive file=golden.raw,format=raw,if=virtio,cache=none,aio=native,discard=unmap \
        -drive file=seed.iso,format=raw,if=virtio,readonly=on \
        -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
        -display none -vga none -serial file:$W/prov-console.log -monitor none -no-reboot
    grep -a 'PROVISION-' prov-console.log
    grep -aq PROVISION-OK prov-console.log
    ls -ls golden.raw
}

# boot_copy <image> <console log>: QEMU in the background, ssh on 127.0.0.1:2222
boot_copy() {
    rm -f "$2" $W/check.pid
    qemu-system-x86_64 -name check -machine q35,accel=kvm -cpu host -smp 4,sockets=1,cores=2,threads=2 -m 8192 \
        -drive file="$1",format=raw,if=virtio,cache=none,aio=native,discard=unmap \
        -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n0 \
        -display none -vga none -serial file:"$2" -monitor none -no-reboot \
        -pidfile $W/check.pid -daemonize
}

gssh() {
    ssh -i $W/id_bench -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o ConnectTimeout=5 -o BatchMode=yes -o LogLevel=ERROR root@127.0.0.1 "$@"
}

check() {
    cd $W
    step "a copy of golden.raw booted under KVM"
    rm -f test.raw
    cp --sparse=always golden.raw test.raw
    boot_copy $W/test.raw $W/check-console.log
    local pid t=0
    pid=$(cat $W/check.pid)
    until gssh true 2> /dev/null; do
        [ $t -ge 300 ] && { echo "no ssh in 300 s"; kill "$pid"; return 1; }
        sleep 5
        t=$((t + 5))
    done
    echo "ssh after ${t}s"
    gssh 'uname -r; cat /opt/bench/provisioned /opt/bench/provisioned-under-kernel /opt/bench/linux-tools;
          systemctl is-system-running; systemctl --failed --no-legend; systemctl list-timers --all --no-legend;
          for c in sysbench fio 7z cyclictest pgbench psql jq bc make gcc; do printf "%s " $c; command -v $c || echo MISSING; done;
          ls /usr/lib/linux-tools/*/perf; systemctl is-enabled bench.service; systemctl is-active postgresql;
          psql -qXAt -d bench -c "select version()"; df -h /; ls -la /opt/bench; snap version 2>&1 | head -1; true'
    step "the suite, QUICK"
    gssh 'touch /opt/bench/QUICK /opt/bench/AUTORUN; systemctl start --no-block bench.service'
    qemu_wait "$pid" 1800 || { echo "the quick suite did not power the guest off in 30 min"; kill "$pid"; return 1; }
    grep -a '^BENCH ' check-console.log
    decode check-console.log check-results
    rm -f test.raw
}

# decode <console log> <dir>: the archive between the markers -- its lines
# of base64 alone, in case a kernel line came between -- held to the sha256
# its own line says, unpacked into <dir>
decode() {
    local want
    want=$(tr -d '\r' < "$1" | grep -a '^BENCH archive' | tail -1 | awk '{print $6}')
    tr -d '\r' < "$1" | sed -n '/^BENCH-ARCHIVE-BEGIN/,/^BENCH-ARCHIVE-END/p' \
        | grep -aE '^[A-Za-z0-9+/=]+$' | base64 -d > "$2.tar.xz"
    echo "$want  $2.tar.xz" | sha256sum -c
    rm -rf "$2"
    mkdir -p "$2"
    tar -C "$2" -xJf "$2.tar.xz"
}

arm() {
    cd $W
    step "AUTORUN into golden.raw"
    local loop part
    loop=$(losetup -f --show -P golden.raw)
    udevadm settle
    part=${loop}p1
    [ "$(blkid -o value -s LABEL "$part")" = cloudimg-rootfs ]
    mkdir -p /mnt/golden
    mount "$part" /mnt/golden
    touch /mnt/golden/opt/bench/AUTORUN
    rm -f /mnt/golden/opt/bench/QUICK
    ls -la /mnt/golden/opt/bench
    umount /mnt/golden
    losetup -d "$loop"
    sha256sum golden.raw | tee golden.sha256
}

# kvm [device]: the suite under KVM, on a fresh copy on nos's root -- or on
# the device, staged already by device-stage
kvm() {
    cd $W
    local D=$MNT/vms/bench-kvm R=$W/results-kvm DISK=${1:-} pid c0 c1 c2 c3 cio rest t0 t1
    if [ -n "$DISK" ]; then
        R=$W/results-kvm-device
        device_free "$DISK"
        step "the guest's disk: $DISK ($(readlink -f "$DISK"), $(blockdev --getsize64 "$DISK") bytes)"
    else
        mount_nosroot
        step "a fresh copy on nos's root: $D/disk.raw"
        mkdir -p $D
        rm -f $D/disk.raw
        cp --sparse=always golden.raw $D/disk.raw
        sync
        ls -ls $D/disk.raw
        DISK=$D/disk.raw
    fi
    step "8 GiB of 2 MiB pages, as nos's huge pool gives a guest"
    echo 4352 > /proc/sys/vm/nr_hugepages
    grep -E 'HugePages_(Total|Free)' /proc/meminfo
    [ "$(awk '/HugePages_Free/ {print $2}' /proc/meminfo)" -ge 4096 ]
    c0=$(cpu_of ${VCPU_APIC[0]}); c1=$(cpu_of ${VCPU_APIC[1]})
    c2=$(cpu_of ${VCPU_APIC[2]}); c3=$(cpu_of ${VCPU_APIC[3]})
    cio=$(cpu_of $IO_APIC)
    rest=$(e_cores_but "$cio")
    echo "vCPUs on cpus $c0 $c1 $c2 $c3 (APIC ${VCPU_APIC[*]}), the iothread on $cio (APIC $IO_APIC), the rest of QEMU on $rest"
    mkdir -p $R
    rm -f $W/kvm-console.log $W/kvm.pid
    step "QEMU"
    t0=$(date +%s)
    taskset -c "$rest" qemu-system-x86_64 -name bench,debug-threads=on \
        -machine q35,accel=kvm,memory-backend=ram0 -cpu host -smp 4,sockets=1,cores=2,threads=2 -m 8192 \
        -object memory-backend-file,id=ram0,size=8G,mem-path=/dev/hugepages,prealloc=on \
        -object iothread,id=io0 \
        -drive file=$DISK,format=raw,if=none,id=vd0,cache=none,aio=native,discard=unmap \
        -device virtio-blk-pci,drive=vd0,iothread=io0 \
        -netdev user,id=n0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=n0 \
        -display none -vga none -serial file:$W/kvm-console.log -monitor none -no-reboot \
        -pidfile $W/kvm.pid -daemonize
    pid=$(cat $W/kvm.pid)
    local task comm cpu
    for task in /proc/$pid/task/*; do
        comm=$(cat $task/comm)
        case "$comm" in
            "CPU 0/KVM") cpu=$c0 ;;
            "CPU 1/KVM") cpu=$c1 ;;
            "CPU 2/KVM") cpu=$c2 ;;
            "CPU 3/KVM") cpu=$c3 ;;
            "IO io0") cpu=$cio ;;
            *) cpu=$rest ;;
        esac
        taskset -pc "$cpu" "${task##*/}" > /dev/null
    done
    for task in /proc/$pid/task/*; do
        echo "${task##*/} $(cat $task/comm) $(taskset -pc ${task##*/} | sed 's/.*: //')"
    done | tee $R/qemu-threads.txt
    cat /proc/$pid/cmdline | tr '\0' ' ' > $R/qemu-cmdline.txt
    echo
    step "waiting for the guest to power itself off (up to 3 h)"
    # KVM's counters are the VM's own directory in debugfs, gone with it:
    # kept every 30 s while it runs, the last copy within 30 s of its end
    local t=0
    while kill -0 "$pid" 2> /dev/null; do
        [ $t -ge 10800 ] && { echo "still running after 3 h"; return 1; }
        if [ $((t % 30)) = 0 ]; then
            grep -r . /sys/kernel/debug/kvm/$pid-*/ > $R/kvm-stats.tmp 2> /dev/null && mv $R/kvm-stats.tmp $R/kvm-stats.txt
        fi
        sleep 5
        t=$((t + 5))
    done
    t1=$(date +%s)
    echo "QEMU ran $((t1 - t0)) s"
    cp $W/kvm-console.log $R/console.log
    grep -a '^BENCH ' $R/console.log | tail -5
    decode $R/console.log $R/archive
    echo 0 > /proc/sys/vm/nr_hugepages
}

# suite_into <disk> [step...]: the kit's guest/run.sh into the disk's
# /opt/bench, so that the suite run is the tree's whatever golden.raw was
# provisioned with, and the steps named -- run.sh's ONLY -- or every one
suite_into() {
    local disk=$1 loop
    shift
    [ -f $W/guest/run.sh ] || [ $# -gt 0 ] || return 0
    loop=$(losetup -f --show -P "$disk")
    mkdir -p /mnt/benchdisk
    mount "${loop}p1" /mnt/benchdisk
    if [ -f $W/guest/run.sh ]; then
        install -m 755 $W/guest/run.sh /mnt/benchdisk/opt/bench/run.sh
    fi
    rm -f /mnt/benchdisk/opt/bench/ONLY
    if [ $# -gt 0 ]; then
        echo "$*" > /mnt/benchdisk/opt/bench/ONLY
        step "the suite cut to: $*"
    fi
    ls -la /mnt/benchdisk/opt/bench
    umount /mnt/benchdisk
    losetup -d "$loop"
}

# device_free <device>: a block device nothing on this machine uses --
# not mounted, not swap, not under an array or a mapping -- and big enough
# for golden.raw, or the step stops here
device_free() {
    local dev real name
    dev=$1
    [ -b "$dev" ] || { echo "$dev is no block device"; return 1; }
    real=$(readlink -f "$dev")
    name=$(basename "$real")
    [ -z "$(lsblk -no MOUNTPOINTS "$real" | tr -d '[:space:]')" ] || { echo "$real is mounted"; return 1; }
    ! grep -q "^$real " /proc/swaps || { echo "$real is swap"; return 1; }
    [ -z "$(ls /sys/class/block/$name/holders)" ] || { echo "$real is held: $(ls /sys/class/block/$name/holders)"; return 1; }
    [ "$(blockdev --getsize64 "$real")" -ge "$(stat -c %s $W/golden.raw)" ] || { echo "$real is smaller than golden.raw"; return 1; }
}

# device_stage <device> [step...]: golden.raw onto the device, every byte of
# it -- a device reads as what was last there, not as a hole -- and the rest
# of the device given back, the suite in it cut to the steps named
device_stage() {
    cd $W
    local dev=${1:?"device-stage: a device, /dev/disk/by-partuuid/..."}
    shift
    device_free "$dev"
    step "$dev given back whole, then golden.raw onto it"
    blkdiscard -f "$dev"
    dd if=golden.raw of="$dev" bs=4M oflag=direct conv=fsync status=none
    cmp -n "$(stat -c %s golden.raw)" golden.raw "$dev" && echo "the device holds golden.raw byte for byte"
    suite_into "$dev" "$@"
    sync
}

nos_stage() {
    cd $W
    mount_nosroot
    step "the KVM run's copy taken away, a fresh one for nos"
    rm -rf $MNT/vms/bench-kvm
    mkdir -p $MNT/vms/bench
    rm -f $MNT/vms/bench/disk.raw $MNT/vms/bench/console.log $MNT/vms/bench/console.log.1
    cp --sparse=always golden.raw $MNT/vms/bench/disk.raw
    suite_into $MNT/vms/bench/disk.raw "$@"
    cat > $MNT/vms/bench/vm.toml <<'TOML'
# The KVM-vs-nos comparison's guest: the same disk image QEMU/KVM ran on
# Ubuntu, under nos's hv -- 4 CPUs, 8 GiB, its suite run at boot, then off.
kernel = "/lib/hvboot"
memory = 8192
cpus = 4
disks = ["disk.raw"]
net = true
restart = false
console_log = true
TOML
    sync
    ls -lsa $MNT/vms/bench
    umount $MNT
    step "nos's root held to e2fsck -fn"
    e2fsck -fn $NOSROOT
}

case "${1:-}" in
    info) info ;;
    prep) prep ;;
    check) check ;;
    arm) arm ;;
    kvm) kvm ;;
    kvm-device) kvm "${2:?"kvm-device: a device"}" ;;
    nos-stage) shift; nos_stage "$@" ;;
    device-stage) shift; device_stage "$@" ;;
    *) echo "usage: $0 info|prep|check|arm|kvm|kvm-device <device>|nos-stage|device-stage <device>" >&2; exit 2 ;;
esac
