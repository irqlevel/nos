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
  (`src/rust/drivers/usb`) enumerates a USB HID boot keyboard and publishes into the
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

**Load.** Hammered with small UDP datagrams (64-byte frames, the `netload`
target -- a [module](modules.md#netload) now -- from a machine in the same
data centre) the I210 now takes everything that
arrives: an echo at 578–581 k datagrams a second answers every one, a sink
at 615 k receives every one, and the queue's `dropped-no-descriptor` stays
at zero. The ceiling is the sender, whose single-queue NIC holds one of its
CPUs at 60–100% softirq — well short of the 1.49 M a second 1 GbE carries
at that size. On the nos side it is all one CPU's work, the queue vector's:
about three quarters of it for the echo, 60% for the sink. (`top` shows the
receive softirq at 100% under load, since the poll keeps looking at an empty
ring rather than re-arm the interrupt; `igbdump` says how long it looked,
and the rest is the work.) Three things stood between the first
measurements and that, none of them the 82576 model could show:

- The first ceiling — 205 k delivered of 613 k offered, put down at the
  time to the chip's descriptor pipeline — was the driver. `RXDCTL` holds
  the descriptor prefetch thresholds besides the enable bit, and writing
  the bit on its own zeroed them: with `PTHRESH` at zero the chip never
  fetched, and dropped 443 k packets a second with 255 descriptors free in
  host memory. The throttle had one of its own: firmware leaves an `EITR`
  interval that a reset does not clear, which held delivery at 169 k with
  the CPU 93% idle.
- Then interrupts cost more than packets. On one MSI-X vector every
  interrupt read `EICR` and `ICR` to tell a packet from a link change, and
  under the echo those two reads were a third of the profile. The queue
  now has a vector of its own whose handler reads nothing — the chip
  clears and masks its cause (`EIAC`, and `EIAM` under `GPIE.EIAME`) —
  with link changes and overruns on a second; while the rate is high the
  poll goes round again through the softirq instead of re-arming, and the
  throttle widens with the rate. A flood costs one interrupt per thousand
  to five thousand packets, and the echo a quarter less CPU per packet.
- The first burst after every boot lost frames — 755 k in two seconds —
  until the receive ring had turned over once: the rings were filled from
  `Mm::Alloc` before the frame pool existed, and each of those frames cost
  a TLB shootdown to free. The pool now comes up first.

`SRRCTL.Drop_En` stays set: a queue out of descriptors drops at the queue,
where `igbdump` counts it, rather than backing up the packet FIFO.

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
line from the first one of the boot is queued, without a lock, until the raw
partition set aside for it is found; the boot so far is written there in
one go, and every line after by a writer task of its own, so nothing that
traces ever waits on the disk. Under Ubuntu
`scripts/disklog.py format` lays a header on that partition, and a kernel
booted with `disklog=on` writes only where it finds the header intact —
without the parameter it leaves every disk alone, prepared or not, since a
forced write per line is no price for a boot that works; after the next
Ubuntu boot `scripts/disklog.py read` prints the log back. Finding the area is what
brought GPT support and a second partition probe after the Rust NVMe driver
registers its disks. `scripts/nosboot` builds, installs the kernel, arms one
boot of `nos` and reboots. It does not use `grub-reboot`: `/boot` is ext3 on
an mdadm mirror, which GRUB can read but not write, so `next_entry` never
clears and `nos` boots every time — on a box with no console that is one you
do not get back, and how this machine spent an afternoon. The one-shot flag
lives on a plain partition GRUB can write, read and cleared by a
`/etc/grub.d` snippet before the menu, so a hang plus a hardware reset comes
back to Ubuntu.

### Updating the kernel from inside nos

The kernel GRUB boots and the one-shot flag live together on `nosenv`, a
plain ext2 partition (`nvme1n1p1`) that GRUB can read and write and that
nos mounts read-write as its root (`root=LABEL=nosenv`). `/boot` itself is
left alone on purpose: it is ext3 on an md mirror, which nos could read
through one member but never write safely, and nothing about updating nos
needs it. So a new kernel goes in from the [UDP shell](udp-shell.md):

    wget https://github.com/irqlevel/nos/releases/latest/download/kernel-x86_64.elf /nos-kernel64.elf.next
    sha256 /nos-kernel64.elf.next        # against the release's SHA256SUMS
    grubenv /grubenv nos_next=nos-next   # one boot of the candidate
    reboot

`grubenv` edits GRUB's environment block in place, at the same size, the
way `grub-editenv` and GRUB's own `save_env` do — `save_env` writes the
file's disk blocks directly, so the file has to keep them — and GRUB clears
the flag before it loads anything. GRUB carries two entries with the same
command line, `nos` (id `nos-multiboot2`) for `/nos-kernel64.elf` and
`nos-next` for `/nos-kernel64.elf.next`, and the `01_nosenv` snippet sets
`fallback=0` (Ubuntu) whenever it arms one, so a file GRUB cannot load
falls through to Ubuntu without a reset:

    search --no-floppy --fs-uuid --set=nosenv_dev <uuid of nosenv>
    load_env -f (${nosenv_dev})/grubenv
    if [ -n "${nos_next}" ]; then
      set default="${nos_next}"
      set fallback=0
      set nos_next=
      save_env -f (${nosenv_dev})/grubenv nos_next
      set timeout=3
    fi

If the candidate comes up — `version` over udpsh names its commit — it is
promoted from inside itself: `mv /nos-kernel64.elf /nos-kernel64.elf.prev`,
then `mv /nos-kernel64.elf.next /nos-kernel64.elf` (`rm` an older `.prev`
first; `mv` refuses to overwrite). If it does not, a hardware reset brings
Ubuntu back, the `nos` entry still names the kernel that worked, and the
`.next` file just sits there. Promoting only from inside the new kernel is
what keeps a kernel that never reached a shell from replacing one that did,
on a machine where nobody can watch it fail. Under Ubuntu the same
partition is `/nosenv`: `nosboot` builds and installs a kernel there as
`/nosenv/nos-kernel64.elf`, and `grub-editenv /nosenv/grubenv set
nos_next=nos-next` arms a candidate from that side.

Loadable modules come from the same place, and from the same release as the
kernel that is running -- a kernel refuses a module built against another
kernel interface ([Modules](modules.md#releases)):

    wget https://github.com/irqlevel/nos/releases/download/<tag>/blkload-x86_64.ko /blkload.ko
    insmod /blkload.ko

Other firmware, chipsets, NICs and disks are untested; treat bare-metal support
as "works on the three machines it was debugged on".
