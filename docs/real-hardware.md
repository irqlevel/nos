# Real hardware

`nos` boots on bare metal. Two machines so far, and they pull in opposite
directions: a laptop whose only console is its own screen, and a dedicated
server whose only console is the network.

## Dell Latitude 5480

A **Dell Latitude 5480** (Intel Core i5-6200U, Skylake-U / 100-series PCH, BIOS
1.16.0) booted under UEFI. That laptop has no serial port and no 8042/PS/2
controller, yet the kernel comes up on its own screen and gives an interactive
shell driven by the built-in USB keyboard.

That machine is the reason for four pieces of the tree, none of which QEMU
ever demanded:

- **Screen** — under UEFI there is no EGA text mode. GRUB hands the kernel the
  pixel framebuffer and `drivers/fb_console.cpp` draws the console with an 8x16
  font; without it a UEFI boot is simply blind. See
  [Firmware: BIOS and UEFI](build.md#firmware-bios-and-uefi).
- **Keyboard** — a real UEFI laptop often has no 8042 at all, so the emulated
  PS/2 controller QEMU and the KVM clouds provide is not there. The xHCI driver
  (`drivers/usb/`) enumerates a USB HID boot keyboard and publishes into the
  same `KeyboardInput` sink the 8042 driver uses. `usb=off` skips it.
- **Firmware leftovers** — LAPIC LVTs the firmware left armed are masked, and a
  stray interrupt is named instead of panicking anonymously. The TCO watchdog is
  located on 100-series PCH and left alone when the ACPI WDAT table says the
  firmware owns it, so it cannot reset the box mid-boot.
- **Diagnostics** — with no UART the screen is the only channel, so boot-path
  failures print through the screen console and the panic handler rather than
  `Trace` alone.

## Hetzner EX44 dedicated server

A **Hetzner EX44** (Intel Core i5-13500 — Raptor Lake, 6 P-cores + 8 E-cores,
20 threads) booted with `maxcpus=20`, which on that box is every CPU it has.
All 20 come up and stay scheduled: `ps` lists 20 `idleN` and 20 `softirq/N`
tasks, and the load balancer spreads the multitasking self-test across them.

The hybrid topology is what makes it worth recording. The APIC IDs are sparse
and non-contiguous — `0, 1, 8, 9 … 40, 41` for the SMT pairs on the P-cores,
then `48, 50 … 62`, even-numbered, for the E-cores — so the largest apic id is
62 on a 20-CPU machine. That is why `MaxCpus` is 64 rather than a number near
the CPU count, and why IOAPIC redirection entries are only aimed at apic ids a
physical-mode entry can actually name: that field is four bits wide, ids 0..15,
so round-robining an IOAPIC line onto an E-core silently aliased it onto an id
no CPU has and the line stopped being delivered. MSI-X keeps the full 8-bit
destination and the whole CPU mask.

Networking works on the real internet, and this is the first machine to
exercise the Rust **RTL8125** driver against silicon rather than a datasheet —
QEMU has no model of that chip, so until this boot `drivers/r8125` had never
seen the hardware it was written for. The DHCP client takes a public /26 lease
from the host's network, the DNS resolver comes up on the server handed out
with it, the box answers pings from anywhere, and the [UDP shell](udp-shell.md)
(`udpshell=9000` + `scripts/udpsh.py`) is usable across the internet — `ps` and
friends answer from a remote machine.

`netconsole=ip:port` ([Netconsole](netconsole.md)) is how any of this was seen
at all. With no screen and no serial console reachable from outside, the whole
boot log — self-tests, AP bring-up, xHCI enumeration, DHCP — went out over UDP
to `scripts/netconsole.py` as it was produced. xHCI also came up on that board:
26 root ports, three hubs, and a USB keyboard enumerated behind one of them.

Other firmware, chipsets, NICs and disks are untested; treat bare-metal support
as "works on the two machines it was debugged on".
