# Real hardware

`nos` boots on bare metal. Three machines so far, and they pull in opposite
directions: a laptop whose only console is its own screen, and two dedicated
servers — one Intel, one AMD — whose only console is the network.

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

## Hetzner AX41-1-LTD dedicated server

A **Hetzner AX41-1-LTD** (AMD Ryzen 5 3600 — Zen 2, family 17h, 6 cores /
12 threads) booted from its NVMe under GRUB, dual-booting with Ubuntu. It is
the first AMD machine the kernel has run on; the only vendor check in the
tree is the Intel TCO watchdog, which declines politely, and nothing else
noticed the change. Like the EX44 it has no serial port and no IPMI, and
unlike the EX44 its only NIC is an **Intel I210** (`8086:1533`), so the
driver being brought up was also the only channel there was to report
through.

**Network.** The Rust **igb** driver (`drivers/igb`, developed against QEMU's
82576 model) brings up the I210: DHCP takes an address, ARP and TCP flow, and
the [UDP shell](udp-shell.md) and [netconsole](netconsole.md) are reachable
across the internet. The I210 wanted four things the 82576 never did, and
only the last of them was the actual fault:

- Its PHY is shared with the manageability firmware, so every MDIO access
  claims it through `SW_FW_SYNC` behind the `SWSM` hardware mutex — two
  masters on one MDIO bus produce reads that look like data.
- In MSI-X mode `EICR` is not cleared on read; the cause has to be written
  back, or the interrupt re-asserts forever (434 million interrupts and zero
  frames, measured).
- The PHY is reset and its advertisements written before negotiation is
  restarted; firmware leaves it in an arbitrary state, possibly on a
  non-zero register page.
- Auto-speed detection is a latch: the MAC takes its speed once, when the
  PHY first asserts link, and this PHY asserted link early at 10 Mb/s before
  resolving to 1000. A MAC clocked for 10 against a PHY at 1000 receives
  nothing while link, receiver, queue, descriptors and filter all read
  healthy, which is why it took four rounds to find. Per the datasheet the
  MAC now follows the PHY (`ASDE` and `FRCSPD` clear), and `igbdump` prints
  the PHY's own view — what each side advertised and what negotiation
  resolved to.

Under a 613 k pps flood the I210 delivers about 205 k; the rest is dropped by
the chip's on-chip descriptor fetch/writeback pipeline (a 16-descriptor
cache per queue), not by the ring or the driver. `SRRCTL.Drop_En` stays set
even with a single queue: clearing it moved the drops into the 34 KiB packet
FIFO and halved delivery.

**Profiler.** `profile` samples on the AMD core performance counters here:
general counter 0 programmed with PMCx076 ("CPU clocks not halted") through
the six-counter core extension MSRs, overflowing into an NMI at ~1 kHz. Zen 2
has no PerfMonV2, so there is no global status register and an overflow is
recognised by the counter's sign bit going clear; the extra NMI AMD delivers
after each overflow is absorbed, one per sample, and `lscpu` reports how
many. TCG refuses to expose `perfctr-core` at all, so this box is the only
place the arming path has ever run — everything before it was the CPUID
gate declining.

**Diagnostics.** With no serial port and the NIC itself under bring-up, the
boot log had nowhere to go, which is what `disklog` is for: every traced
line is written synchronously to a raw partition set aside for it, from the
first line of boot, straight from the tracer with no task in between (a
drain task would not exist yet where a bring-up hang happens). Under Ubuntu
`scripts/disklog.py format` lays a header on that partition and the kernel
writes only where it finds the header intact; after the next Ubuntu boot
`scripts/disklog.py read` prints the log back. Finding the area is what
brought GPT support and a second partition probe after the Rust NVMe driver
registers its disks. `scripts/nosboot` builds, installs the kernel, arms one
boot of `nos` and reboots. It does not use `grub-reboot`: `/boot` is ext3 on
an mdadm mirror, which GRUB can read but not write, so `next_entry` never
clears and `nos` boots every time — on a box with no console that is one you
do not get back, and how this machine spent an afternoon. The one-shot flag
lives on a plain partition GRUB can write, read and cleared by a
`/etc/grub.d` snippet before the menu, so a hang plus a hardware reset comes
back to Ubuntu.

Other firmware, chipsets, NICs and disks are untested; treat bare-metal support
as "works on the three machines it was debugged on".
