#!/bin/bash
# One of the panel demo's nos VMs on the AX41 under KVM, detached:
# nos-test.iso (its kernel's line has udpshell=9100, reached on the bridge
# alone), its root and its pool disk, and its tap on nosbr0. `stop` asks nos
# to power off (its shell, through the UDP shell), then QMP's quit if it has
# not in 60 s.
# usage: demo-vm.sh start|stop|status <name>
set -u
D=/root/nos-demo
what=${1:?start, stop or status}
name=${2:?a name}
case $name in
    ax41a) tap=nostap-a mac=52:54:00:77:00:11 ip=10.77.0.11 ;;
    ax41b) tap=nostap-b mac=52:54:00:77:00:12 ip=10.77.0.12 ;;
    *) echo "no such VM: $name"; exit 2 ;;
esac
cd $D
pid() { [ -s qemu-$name.pid ] && kill -0 "$(cat qemu-$name.pid)" 2>/dev/null && cat qemu-$name.pid; }
case $what in
start)
    if p=$(pid); then echo "$name runs already ($p)"; exit 1; fi
    [ -e root-$name.img ] || { echo "no root-$name.img: demo-make.sh $name"; exit 1; }
    [ -e serial-$name.log ] && mv serial-$name.log serial-$name.log.$(date +%H%M%S)
    setsid nohup qemu-system-x86_64 -enable-kvm -cpu host -m 4G -smp 4 -display none \
        -cdrom nos-test.iso -serial file:serial-$name.log \
        -drive file=root-$name.img,format=raw,id=root,if=none,cache=none,aio=native \
        -device virtio-blk-pci,drive=root,disable-legacy=on,disable-modern=off \
        -drive file=pool-$name.img,format=raw,id=pool,if=none,cache=none,aio=native \
        -device virtio-blk-pci,drive=pool,disable-legacy=on,disable-modern=off \
        -netdev tap,id=lan,ifname=$tap,script=no,downscript=no,vhost=on \
        -device virtio-net-pci,netdev=lan,mac=$mac,disable-legacy=on,disable-modern=off \
        -device virtio-rng-pci -qmp unix:$D/qmp-$name.sock,server,nowait \
        -pidfile $D/qemu-$name.pid > $D/qemu-$name.out 2>&1 < /dev/null &
    sleep 2
    if p=$(pid); then echo "$name started ($p)"; else echo "$name did not start:"; cat qemu-$name.out; exit 1; fi
    ;;
stop)
    p=$(pid) || { echo "$name is not running"; exit 0; }
    NSH_TIMEOUT=10 python3 $D/nsh.py $ip poweroff > /dev/null 2>&1 || true
    for i in $(seq 1 60); do kill -0 $p 2>/dev/null || break; sleep 1; done
    if kill -0 $p 2>/dev/null; then
        python3 - "$D/qmp-$name.sock" <<'EOF'
import json, socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sys.argv[1])
f = s.makefile("rwb")
f.readline()
for cmd in ({"execute": "qmp_capabilities"}, {"execute": "quit"}):
    f.write(json.dumps(cmd).encode() + b"\n")
    f.flush()
    f.readline()
EOF
    fi
    echo "$name stopped"
    ;;
status)
    if p=$(pid); then echo "$name runs ($p)"; else echo "$name is not running"; fi
    ;;
*) echo "usage: $0 start|stop|status <name>"; exit 2 ;;
esac
