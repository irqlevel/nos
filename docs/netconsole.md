# Netconsole

With `netconsole=ip:port` (see [Kernel parameters](kernel-parameters.md)) the
kernel ships its whole log to a UDP collector as each line is produced -- the
debugging channel of choice on a machine with no serial port (a laptop over
Wi-Fi, a cloud VM, a [dedicated server](real-hardware.md#hetzner-ex44-dedicated-server)).

Every line the tracer emits is copied into a 128 KiB ring buffer the moment it
is produced, from any context including IRQ; a dedicated `netcon` task drains
the ring and sends whole lines in ~1400-byte datagrams, one per millisecond.
Nothing is lost while the network is still coming up: the ring is primed from
`dmesg` at boot and keeps buffering until the device has an IP, then the
backlog goes out and live lines follow within a couple of milliseconds. When
the ring fills, the oldest lines are evicted and counted (`netconsole` shell
command). Panics are included -- the panic report is captured and flushed
synchronously, best-effort and only if the collector MAC is already in the ARP
cache, since a blocking ARP resolve could never complete with the other CPUs
halted.

Each datagram carries an eight-byte header: `NOSC` and a little-endian
sequence number, which the collector uses to report gaps. UDP drops datagrams
without saying so, and a log that ends because the network ate the rest of a
burst reads exactly like a log that ends because the machine wedged -- which
is the one distinction that matters when debugging a hang. The same reasoning
sets the pace: by the time the link comes up there can be a hundred datagrams
of boot log queued, and sending them at the rate the NIC accepts overruns
whatever is narrowest on the way to the collector, losing the far end of the
burst. `nctail=N` goes further and caps that backlog at the newest N KiB, so
a machine with a few milliseconds of network left spends them on the lines
nearest its death.

Receive with the included collector:

```sh
python3 scripts/netconsole.py                    # listen on 0.0.0.0:6666
python3 scripts/netconsole.py -p 5555 -o boot.log
```

It reassembles lines per sender, prefixes each with the host receive time,
marks any gap in the sequence (`--- netconsole: N datagram(s) lost ---`) and a
sender that restarted, and optionally appends to a file. Datagrams with no
header, from an older kernel, are still printed -- just without gap
detection. Under QEMU user networking the host is `10.0.2.2`,
so `netconsole=10.0.2.2:6666 dhcp=auto` plus a collector on the host works with
no port forwarding (outbound UDP needs none).

The stream carries the kernel log (everything `Trace()` writes, which is what
`dmesg` holds), not the interactive shell's own console echo. Lines the
netconsole task itself produces are deliberately not captured -- feeding the TX
path's own traces back into the ring would make the drain loop generate its own
work forever. They still reach `dmesg` and the console.

**Warning:** the log is sent in the clear and to whoever holds the address; use
it on a network you trust.
