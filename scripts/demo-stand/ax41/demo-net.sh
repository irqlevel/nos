#!/bin/bash
# The panel demo's network on the AX41: the bridge nosbr0, 10.77.0.1/24, two
# nos VMs' taps on it, out through the AX41's address. The EX44 -- and
# nothing else from outside -- let in to each VM's ipcnet port, the same
# port inside (39241 -> ax41a, 39242 -> ax41b), and to each one's API
# (39251 -> ax41a:39234, 39252 -> ax41b:39234), the panel on the EX44
# reaching them so; and to the panel's bundle served here (39299). The VMs
# reach each other's ipcnet at the AX41's address, as the registry has them,
# their source made the AX41's address. Every rule carries the comment
# nos-demo; `down` takes off exactly those, then the taps and the bridge.
# usage: demo-net.sh up|down|show
set -u
PUB=65.109.93.213
EX44=65.108.38.184
UP=enp41s0
BR=nosbr0
NET=10.77.0.0/24
TAG=nos-demo
C=(-m comment --comment $TAG)
IPC=(39241 39242)
API=(39251 39252)
ADDRS=(10.77.0.11 10.77.0.12)
TAPS=(nostap-a nostap-b)
BUNDLE_PORT=39299

show() {
    ip -br addr show $BR 2>/dev/null
    for t in "${TAPS[@]}"; do ip -br link show $t 2>/dev/null; done
    iptables -S | grep -- "$TAG"
    iptables -t nat -S | grep -- "$TAG"
}

up() {
    if iptables -S | grep -q -- "$TAG" || iptables -t nat -S | grep -q -- "$TAG"; then
        echo "rules of $TAG are there already: demo-net.sh down first"
        return 1
    fi
    ip link show $BR > /dev/null 2>&1 || ip link add $BR type bridge || return 1
    ip addr show dev $BR | grep -q "10.77.0.1/24" || ip addr add 10.77.0.1/24 dev $BR || return 1
    ip link set $BR up || return 1
    for t in "${TAPS[@]}"; do
        ip link show $t > /dev/null 2>&1 || ip tuntap add dev $t mode tap || return 1
        ip link set $t master $BR || return 1
        ip link set $t up || return 1
    done
    iptables -I FORWARD 1 -i $BR -o $UP "${C[@]}" -j ACCEPT || return 1
    iptables -I FORWARD 1 -i $UP -o $BR -m conntrack --ctstate RELATED,ESTABLISHED,DNAT "${C[@]}" -j ACCEPT || return 1
    iptables -I FORWARD 1 -i $BR -o $BR "${C[@]}" -j ACCEPT || return 1
    iptables -t nat -A POSTROUTING -s $NET -o $UP "${C[@]}" -j MASQUERADE || return 1
    for i in 0 1; do
        a=${ADDRS[$i]}
        p=${IPC[$i]}
        q=${API[$i]}
        iptables -t nat -I PREROUTING 1 -i $UP -s $EX44 -d $PUB -p tcp --dport $p "${C[@]}" \
            -j DNAT --to-destination $a:$p || return 1
        iptables -t nat -I PREROUTING 1 -i $BR -s $NET -d $PUB -p tcp --dport $p "${C[@]}" \
            -j DNAT --to-destination $a:$p || return 1
        iptables -t nat -I PREROUTING 1 -i $UP -s $EX44 -d $PUB -p tcp --dport $q "${C[@]}" \
            -j DNAT --to-destination $a:39234 || return 1
    done
    iptables -t nat -I POSTROUTING 1 -o $BR -s $NET -d $NET -m conntrack --ctstate DNAT "${C[@]}" \
        -j SNAT --to-source $PUB || return 1
    # The panel's bundle: the EX44 alone
    iptables -I INPUT 1 -p tcp --dport $BUNDLE_PORT "${C[@]}" -j DROP || return 1
    iptables -I INPUT 1 -p tcp --dport $BUNDLE_PORT -s $EX44 "${C[@]}" -j ACCEPT || return 1
    show
}

down() {
    local t rule
    for t in filter nat; do
        iptables -t $t -S | grep -- "$TAG" | sed 's/^-A /-D /' | while read -r rule; do
            eval iptables -t $t $rule || echo "could not take off: $rule"
        done
    done
    for t in "${TAPS[@]}"; do ip link del $t 2> /dev/null; done
    ip link del $BR 2> /dev/null
    show
    echo "down"
}

case "${1:-}" in
    up) up ;;
    down) down ;;
    show) show ;;
    *) echo "usage: $0 up|down|show"; exit 2 ;;
esac
