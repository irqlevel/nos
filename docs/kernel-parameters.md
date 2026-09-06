# Kernel parameters

Pass via GRUB command line on x86-64 (edit `build/grub.cfg`) or QEMU `-append` on arm64.
Parsing lives in `src/cpp/kernel/parameters.cpp`.

## CPUs

- `smp=off` — disable SMP, run on BSP only
- `maxcpus=N` — bring up at most N CPUs, the BSP included (`maxcpus=2` = BSP + one AP); the rest stay parked. Bisects an SMP bring-up failure without giving up SMP entirely

## Console and logging

- `console=serial` — direct shell output to serial port only
- `console=vga` — direct shell output to the screen only (VGA text or framebuffer)
- `loglevel=N` — trace level to boot with (0-5, default 1). The `loglevel` shell command moves it afterwards, but only a boot parameter can make the *boot* chatty; the task lifecycle and the multitasking self-test are at level 3
- `netconsole=ip:port` — stream the kernel log over UDP to that collector (e.g. `netconsole=10.0.2.2:6666`); needs an address, so pair it with `dhcp=auto`. See [Netconsole](netconsole.md)
- `nctail=N` — ship only the newest N KiB of the log buffered before the link came up (e.g. `nctail=16`); the rest is dropped. For a machine that wedges shortly after the network appears, this spends the little airtime it has on the lines around the wedge instead of on the head of the boot log

## Networking

- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)
- `dns=on` — enable DNS resolver (uses DHCP-provided DNS server; requires `dhcp=auto`)
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`). See [UDP remote shell](udp-shell.md)
- `netframes=N` — frames in the network frame pool; watch `netpool` for misses
- `rxpoll=on` — have the tick look at the receive path as well as the NIC's interrupt (off by default, see `NetDeviceTable::PollRx`)

## Devices

- `usb=off` — x86-64 only: skip xHCI bring-up (no USB keyboard; the 8042 keyboard is unaffected)
- `its=off` — arm64 only: disable the GICv3 ITS and degrade PCIe MSI gracefully (default `its=on`; virtio-mmio devices don't need it)
