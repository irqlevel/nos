# Kernel parameters

Space-separated `key=value` pairs. Set them on the GRUB command line on
x86-64 (`build/grub.cfg`, the `multiboot2` line) or with QEMU `-append` on
arm64 (`scripts/qemu-arm64.sh`). The whole command line is capped at 255
characters and a single parameter at 48; an unknown key, or a known key with
a value it does not recognise, is logged and skipped rather than refused.
Parsing lives in `src/cpp/kernel/parameters.cpp`.

The defaults the shipped configs use are `dhcp=auto dns=on root=auto` on
x86-64 and `dhcp=auto dns=on udpshell=9000` on arm64.

## CPUs

- `smp=off` — disable SMP, run on BSP only
- `maxcpus=N` — bring up at most N CPUs, the BSP included (`maxcpus=2` = BSP + one AP); the rest stay parked. Bisects an SMP bring-up failure without giving up SMP entirely

## Console and logging

- `console=serial` — shell input and output on the serial port only
- `console=vga` — shell input and output on the screen only (VGA text or framebuffer)
- `console=both` — both (the default); listed for completeness
- `trace=vga` — x86-64 only: mirror the boot trace on the screen as well as the serial port. Off by default because redrawing glyphs costs a full repaint per scrolled row; on a machine with no UART the mirror is the default anyway, since there is nowhere else to put it
- `loglevel=N` — trace level to boot with (0-5, default 1). The `loglevel` shell command moves it afterwards, but only a boot parameter can make the *boot* chatty; the task lifecycle and the multitasking self-test are at level 3
- `netconsole=ip:port` — stream the kernel log over UDP to that collector (e.g. `netconsole=10.0.2.2:6666`); needs an address, so pair it with `dhcp=auto`. See [Netconsole](netconsole.md)
- `nctail=N` — ship only the newest N KiB of the log buffered before the link came up (e.g. `nctail=16`); the rest is dropped. For a machine that wedges shortly after the network appears, this spends the little airtime it has on the lines around the wedge instead of on the head of the boot log
- `panic=vga` — parsed and currently has no effect: the panic report already goes to the screen whenever one is ready, on both architectures

## Networking

- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)
- `dns=on` — enable DNS resolver (uses DHCP-provided DNS server; requires `dhcp=auto`)
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`). See [UDP remote shell](udp-shell.md)
- `netframes=N` — frames in the network frame pool (64–65536); watch `netpool` for misses
- `rxpoll=on` — have the tick look at the receive path as well as the NIC's interrupt (off by default, see `NetDeviceTable::PollRx`)

## Devices and filesystems

- `usb=off` — x86-64 only: skip xHCI bring-up (no USB keyboard; the 8042 keyboard is unaffected)
- `its=off` — arm64 only: disable the GICv3 ITS and degrade PCIe MSI gracefully (default `its=on`; virtio-mmio devices don't need it)
- `root=auto` — mount a ramfs on `/` at boot and the first ext2 filesystem found on a block device read-only on `/boot`. Without it the VFS starts empty and filesystems are mounted from the shell

## Diagnostics

- `wxprobe=on` — after the kernel image has been split into text RX / rodata RO+NX / data RW+NX, deliberately write to `.text`. The machine should die with a page fault (x86-64) or a data abort (arm64); a `W^X probe: text write SUCCEEDED` line means the protection is not doing its job. See [Paging](paging.md)

## Bisecting a boot failure

Try in this order: `smp=off`, then `maxcpus=2`, then `loglevel=4`, then take
subsystems out of the path with `usb=off` / `its=off` / `console=`. On a
machine with no serial port, add `netconsole=ip:port` (with `nctail=64`) so
there is a log to read at all — see [Debug](debug.md) and
[Boot](boot.md).
