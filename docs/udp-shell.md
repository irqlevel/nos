# UDP remote shell

When `udpshell=PORT` is set (see [Kernel parameters](kernel-parameters.md)),
the kernel listens for commands on that UDP port.
A lightweight protocol header (16 bytes: magic, sequence number, chunk index, flags, payload length) frames every packet, enabling the client to validate replies, reassemble multi-chunk responses in order, and detect the end of a response without relying on timeouts.

Connect with the included Python client:

```sh
python3 scripts/udpsh.py <vm-ip> [port] [timeout]
```

- Default port is `9000`, default timeout is `30` seconds (long enough for blocking commands like `ping`).
- On protocol errors or timeouts, the client reconnects automatically and resets state.
- All [shell commands](shell-commands.md) work over the UDP session (including blocking ones like `ping`).
- A reply is assembled in a 32 KB buffer and sent as chunks of 1384 bytes,
  one per millisecond -- paced for the same reason the [netconsole](netconsole.md)
  drain is, since twenty-odd datagrams released at once lose their tail to the
  first narrow queue on the way. Whatever does not fit the buffer is dropped and
  marked `[output truncated]`. The client collects chunks by index, so one
  reordered datagram no longer discards the whole answer, and it names any
  that never arrived.
- `dmesg` still takes a line count, because the log holds 2048 messages and
  even 32 KB is a fraction of it: a dump from the head returns the first
  minutes of boot and nothing since, so on a machine that has been up a while
  `dmesg 40` is the useful form. Same reasoning as `nctail=N` for the
  netconsole backlog -- what you want is the end.
- `loglevel N` raises the trace level on the running kernel, which is the only
  way to see a subsystem whose level constant is above the default 1 (`UsbLL`
  and `KbdLL` are 3, the allocators and MMIO are 4) without a rebuild and a
  reflash. It is loud: level 4 traces every allocation.
- **Warning:** the UDP shell has no authentication — anyone who can reach the port has full kernel shell access. Use only for testing or behind a firewall.
