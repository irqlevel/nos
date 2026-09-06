#!/bin/bash
# Boot with the Intel igb NIC instead of virtio-net.
#
# QEMU's `igb` model is the 82576, the same family as the I210 on real
# hardware, which is the whole reason the driver was written against it: the
# ring setup, the PHY bring-up and the interrupt routing are shared, and here
# they can be debugged with gdb and a packet capture instead of on a machine
# whose only console is the NIC being debugged.
#
#   NOS_PCAP=1 ./scripts/qemu-igb.sh   also writes nos-igb.pcap
#   NOS_TRACE=1 ./scripts/qemu-igb.sh  also writes nos-igb-trace.log
#
# Useful QEMU trace events when the receive side is silent:
#   e1000x_rx_link_down        the model dropped the frame, link is down
#   e1000x_rx_can_recv_disabled  says which of link/rx-enable/bus-master is off
cd "$(dirname "$0")/.."

ARGS=(-m 1G -smp 4 -cdrom nos.iso -serial file:nos.log -s
      -device igb,netdev=net0 -netdev user,id=net0)

if [ -n "$NOS_PCAP" ]; then
    ARGS+=(-object filter-dump,id=f0,netdev=net0,file=nos-igb.pcap)
fi

if [ -n "$NOS_TRACE" ]; then
    ARGS+=(-trace "igb_*" -trace "e1000x_*" -trace "e1000e_rx*"
           -trace "e1000e_irq*" -D nos-igb-trace.log)
fi

exec qemu-system-x86_64 "${ARGS[@]}"
