# Real hardware

`nos` boots on bare metal. Eight machines so far, and they pull in opposite
directions: a laptop whose only console is its own screen, and seven
servers — six Intel, one AMD — whose only console is the network, one of
them fifteen years old and two of them of two sockets.

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
  (`src/crates/drivers/usb`) enumerates a USB HID boot keyboard and publishes into the
  same `KeyboardInput` sink the 8042 driver uses. `usb=off` skips it.
- **Firmware leftovers** — LAPIC LVTs the firmware left armed are masked, and a
  stray interrupt is named instead of panicking anonymously. The TCO watchdog is
  located on 100-series PCH and left alone when the ACPI WDAT table says the
  firmware owns it, so it cannot reset the box mid-boot.
- **Diagnostics** — with no UART the screen is the only channel, so boot-path
  failures print through the screen console and the panic handler rather than
  `trace` alone.

## Hetzner EX44 dedicated server

A **Hetzner EX44** (Intel Core i5-13500 — Raptor Lake, 6 P-cores + 8 E-cores,
20 threads) booted with `maxcpus=20`, which on that box is every CPU it has.
All 20 come up and stay scheduled: `ps` lists 20 `idleN` and 20 `softirq/N`
tasks, and the load balancer spreads the multitasking self-test across them.

The hybrid topology is what makes it worth recording. The APIC IDs are sparse
and non-contiguous — `0, 1, 8, 9 … 40, 41` for the SMT pairs on the P-cores,
then `48, 50 … 62`, even-numbered, for the E-cores — so the largest apic id is
62 on a 20-CPU machine. That is why `MAX_CPUS` is 64 rather than a number near
the CPU count, and why IOAPIC redirection entries are only aimed at apic ids a
physical-mode entry can actually name: that field is four bits wide, ids 0..15,
so round-robining an IOAPIC line onto an E-core silently aliased it onto an id
no CPU has and the line stopped being delivered. MSI-X keeps the full 8-bit
destination and the whole CPU mask.

Networking works on the real internet, and this is the first machine to
exercise the Rust **RTL8125** driver against silicon rather than a datasheet —
QEMU has no model of that chip, so until this boot `drivers/net/r8125` had never
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

Since 2026-10-20 it boots nos from the system image ([Install](install.md))
beside Ubuntu, one boot at a time. The halves of Ubuntu's swap array were
cut from 32 GiB to 16 on both disks to make room for a 16 GiB partition on
each: nos's root, ext2, which `nos-install` poured, on one, and on the other
a 64 MiB [disk log](kernel-parameters.md) area, for a boot that dies before
its network is up (`nos_cmdline=disklog=on` in the root's `/grubenv`).
Ubuntu's GRUB boots nos when `nos_next` there asks, and Ubuntu again after;
Linux swaps the two NVMe disks' names from boot to boot, so both partitions
are named by UUID. nos's sshd answers with a host key of its own. VT-x runs
guests there ([The EX44, under VT-x](hypervisor.md#the-ex44-under-vt-x)).

## Hetzner AX41-1-LTD dedicated server

A **Hetzner AX41-1-LTD** (AMD Ryzen 5 3600 — Zen 2, family 17h, 6 cores /
12 threads) booted from its NVMe under GRUB, dual-booting with Ubuntu. It is
the first AMD machine the kernel has run on; the only vendor check in the
tree is the Intel TCO watchdog, which declines politely, and nothing else
noticed the change. Like the EX44 it has no serial port and no IPMI, and
unlike the EX44 its only NIC is an **Intel I210** (`8086:1533`), so the
driver being brought up was also the only channel there was to report
through.

**Network.** The Rust **igb** driver (`drivers/net/igb`, developed against QEMU's
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
  `mm::alloc` before the frame pool existed, and each of those frames cost
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

**Interrupt controllers.** The MADT lists two IO-APICs, the FCH's (id 13,
`0xFEC00000`, its pins from GSI 0) first and the GNB's (id 14, `0xFEC01000`,
from GSI 24) second -- the first machine here with more than one. The
IO-APIC driver takes a GSI for the index of its pin, so the one it can drive
is the one at GSI 0; until 2026-09-30 ACPI kept the last one listed, and the
legacy IRQs -- the HPET's tick, IRQ 0 by way of GSI 2, among them -- were set
up on pins of the GNB's that none of those lines reaches. Nothing noticed:
the tick only wakes the other CPUs until each runs its local APIC timer, and
the kernel's clock reads a counter rather than counting ticks. It drives the
FCH's now, and `irqstat` counts the HPET's tick at its 100 Hz.

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
plain ext2 partition that GRUB can read and write and that nos mounts
read-write as its root (`root=LABEL=nosenv`). Under Ubuntu it is
`nvme1n1p1` on most boots and `nvme0n1p1` on others -- Linux does not number
the two NVMe controllers in the same order every time, and the other disk's
first partition is the disk log -- so name it by label, `blkid -L nosenv`,
never by device: GRUB finds it by UUID and nos by label, for that reason. `/boot` itself is
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

## ASUS RS500-E6-PS4

A 1U **ASUS RS500-E6-PS4** (board Z8NR-D12, AMI BIOS 1501 from 2011, legacy
BIOS boot): two **Xeon L5630** — Westmere-EP, 2010, four cores and eight
threads each, sixteen CPUs whose APIC ids are 0–3, 18–21, 32–35 and 50–53 —
and 19 GiB, Ubuntu beside nos, rented with a KVM console. It is the oldest
CPU nos has run on, and nearly everything about it is a case the newer
machines never were:

- **The CPU has no x2APIC, no TSC-deadline timer, no XSAVE or AVX and no
  RDRAND.** None of that is in the way: the local APIC runs in xAPIC mode,
  its timer one-shot from a calibration, and the SIMD sections take SSE2 and
  SSSE3. What RDRAND's absence takes away is below.
- **Its SATA is in IDE mode**: the BIOS leaves the ICH10R as two PCI IDE
  functions in native mode (8086:3a20, programming interface 0x8f, and 3a26,
  0x85), each channel a master and a slave — two 4 TB Toshiba MG04ACA400E
  (512e, 7200 rpm) and two PNY CS900 SSDs, one of them Ubuntu's. That is what
  brought the `ata` driver (`src/crates/drivers/block/ata`). The line the
  functions' PCI headers name is the PIC's — 11, where their IO-APIC pin is
  18, which only ACPI's `_PRT` says — so the driver polls, with the devices'
  interrupt off. nos's root is a partition of the second PNY, its disk log the
  next one; that disk carries GRUB's boot code too, Ubuntu's GRUB from Ubuntu's
  `/boot`, so that a BIOS that tries it first lands in the same menu instead of
  on zeros, and `grub-pc`'s `install_devices` names both disks. By PIO, a
  16-bit `inw` costs this chipset 0.68 µs: 2.8 MiB/s from the SSD as from the
  HDDs, 1.4 ms a 4 KiB read whatever its place, one CPU busy in `inw` for all
  of it (`blkload`, `profile`) — a hundredth of the link: Ubuntu reads the same
  SSD by bus-master DMA at 241 MiB/s, and 6 200 random reads of 4 KiB a
  second, no more at 32 in flight (no NCQ in IDE mode). The ICH10R has AHCI
  too: with the BIOS's SATA mode set to it, the `ahci` driver takes these
  disks by DMA.
- **Its NICs are two Intel 82574L** (the `e1000e` driver), MSI-X, the cable
  on the one at 02:00.0, which nos names `eth0` by the bus order. The BMC
  shares it — the firmware's manageability is on (ASF), and the driver leaves
  MANC as it found it. The switch port offers 100BASE-TX full duplex and
  nothing else (link partner ability 0x4101, no 1000BASE-T in its status), so
  the link is 100 Mbit/s whatever the PHY advertises.
- **Its network hands out no address**: Ubuntu's is set by hand, and a
  DHCPDISCOVER heard no offer. nos takes eth0's address, gateway and resolver
  from its root's `/etc/net.toml` (`nos-install --ip … --gw … --dns …`;
  before the file, `--cmdline "ip=… gw=… dns=…"`, which an update turns into
  it), and its log goes to a netconsole collector on another host.
- **Nothing seeds the random pool but what it keeps.** No RDRAND, no
  virtio-rng, and the jitter collector finds no variance there — at setup and
  again once every CPU runs, with the walk timed between serialized reads —
  where the same walk in a Linux process on the machine varies plenty. The
  boot that found that would have given sshd nothing to make a host key with;
  `/etc/random-seed`, which `nos-install` writes from Ubuntu's `/dev/urandom`
  and every boot writes over with the next, is what seeds it
  ([Randomness](random.md#a-seed-kept-from-one-boot-to-the-next)).
- **Its BMC watchdog does not outlive a reboot.** Armed from Ubuntu it hard-
  resets the box when it runs out, but armed before a reboot it is the BIOS's
  again by the end of POST, and nos does not speak IPMI. What resets a nos
  that hangs there is the ICH10's TCO watchdog, which nos finds by the LPC
  bridge and keeps fed, 30 s at a time.

POST to GRUB takes about two minutes; nos says `boot: complete` 13 s after
GRUB, and its sshd answers then. A `reboot` in nos is Ubuntu again in under
three minutes, nos's ext4 clean behind it.

The second Toshiba (`sdc` to Ubuntu, `hdc` to nos) is a disk of nos's own
([A disk of its own](install.md#a-disk-of-its-own)): made so from Ubuntu
with `nos-install disk` first, then again by the installer ISO ([From an
ISO](install.md#from-an-iso)), which Ubuntu's GRUB boots out of `/boot` by
loopback as a virtual CD would boot it -- with answers in a third initramfs,
its own kernel log to the netconsole collector, the disk named by its
`wwn-` id, the static address the answers gave, and a reboot after. The
BIOS boots the PNY first, so Ubuntu's GRUB has an entry, `nos-own-disk`,
that finds the Toshiba's root by its UUID and chainloads the disk's first
sector, as the BIOS would boot the disk itself: `grub-reboot nos-own-disk`
boots it once. nos runs from `hdc3` there, its 3.6 TB `nos-data` blank
beside it; GRUB for UEFI is on it too, which this BIOS never runs.

## Supermicro X11SSD-F

A node of a **Supermicro SYS-5039MS-H8TRF** MicroCloud (board X11SSD-F, AMI
BIOS 3.0 of 2023, UEFI, Secure Boot off): a **Xeon E3-1230 v6** — Kaby Lake,
four cores and eight threads, RDRAND and RDSEED, x2APIC and the TSC-deadline
timer — and 31 GiB, Ubuntu beside nos. Nothing about the CPU is new to nos;
its disks and its NICs were:

- **Its SATA is the C236's AHCI HBA** (8086:a102, AHCI 1.3.1, eight ports,
  64-bit, an MSI): two Intel SSDSC2KB240G7 (512e), one of them Ubuntu's. That
  is what brought the `ahci` driver (`src/crates/drivers/block/ahci`), the
  disks `ada` and `adb` at 6 Gbps, the six empty ports passed over in the one
  second their links are given together. nos's root is the first partition
  of the second SSD, its disk log the next one; under UEFI the firmware
  boots by its BootOrder, so that disk carries no boot code. Its MSI is
  32-bit and cannot be masked; on it the driver keeps up to 31 commands out a
  disk (NCQ), the link kept out of its power states. Against Ubuntu's `fio`
  on the same SSD, its CPUs held out of deep C-states (`blkload` in nos,
  writes to a third partition of nos's SSD made for them): 76 500 random
  reads of 4 KiB a second at 32 in flight (Ubuntu's 76 700), 46 200 random
  writes (46 700), 431–434 MiB/s in reads of 128 KiB at 4 in flight and 500
  of 256 KiB at 8 (446 and 504); at one in flight 8 200–8 900 random reads
  (8 100) and 30 400–30 900 random writes (23 800), but 317 MiB/s in reads
  of 128 KiB (364): a quarter of them some 150 µs slower, a tail there
  before NCQ too, the same through the device's queue as by a task, and
  neither the link's power states nor the CPU the MSI goes to sleeping deep
  (kept awake by the other disk's completions, 332) -- open. Through the
  device's queue (`blkload ... aio`, a VM's disk's path) it does what tasks
  do: 92 500 random reads a second at 32 in flight on Ubuntu's SSD by either,
  46 200 random writes for 30 s. Once in some 80 s of load the error
  handler's look found an interrupt pending a millisecond with no MSI taken
  for it, as an early look -- one that held only its first read of IS --
  could find of an ISR already under way; the look that holds every bit at
  every read found none in three minutes of load after. Through ext4 NCQ
  changes nothing: a file's batches go to the disk one after another, so a
  256 MiB file reads at 205–243 MiB/s and copies at 46–48, since a batch's
  run of contiguous blocks is one command (94 and 20 when each block was a
  command of its own); commands past 256 KiB are no faster on this SSD
  (293 MiB/s at 512 KiB to 315 at 256, its Ubuntu disk), so 256 KiB is where
  the driver stops a command.
- **Its NICs are a two-port Intel I350** (8086:1521, Supermicro's
  AOC-CGP-i2 MicroLP), MSI-X. That is what brought the I350 into `igb`, a
  third generation beside the 82576 and the I210: each port claims its own
  PHY semaphore bit and waits for its own config-done bit. Both ports link
  at 1000 Mbit/s full duplex some five seconds after their PHY's reset,
  longer than the driver's one-second wait, so the link comes as a `link
  changed` after `device ready`; `eth0` is 02:00.0, the cable that carries
  the address. MANC is 0x62000000 and the driver leaves it so.
- **Its network hands out no address** either, so nos's comes from
  `/etc/net.toml` (`nos-install --ip … --gw … --dns …`, or
  `--network-here` from its rescue system).
- **Its TCO watchdog is the firmware's**: nos finds it by the SMBus
  controller and leaves NO_REBOOT as firmware set it -- a 100-series PCH
  keeps the bit behind the P2SB, which the firmware hides -- so a nos that
  hangs there is not reset by it. From Cannon Lake on the bit is in the
  TCO's own TCO1_CNT, and nos clears it there.

Its BootOrder tries the I350's PXE first: POST to GRUB takes minutes, a
boot of nos and back to Ubuntu about eleven. nos says `boot: complete` 10 s
after GRUB, its microcode loaded (0xf4 to 0xf8), its root mounted from `adb1`
at 9.4 s, sshd listening on `eth0` at once.

## Hertz: an MSI PRO B760M-P with a GPU

An **MSI PRO B760M-P** (Intel Core i3-14100 — Raptor Lake, four cores and
eight threads, a B760 PCH — UEFI, Secure Boot off, 31 GiB): Ubuntu on one
Samsung 990 PRO, nos on the other, its only console the network -- no
serial port, no BMC, a static address. It is where a host's GPU is given to
a guest ([GPU passthrough](../plans/gpu-passthrough.md)): an **NVIDIA RTX
A2000** at 01:00.0, which is its boot VGA too.

- **Its NIC is an RTL8168H** (10ec:8168 rev 15, XID 541), the first `r8168`
  ran on. Under UEFI the firmware leaves its INTx line unrouted (Interrupt
  Line 0xFF, which is no IO-APIC pin): the driver takes its MSI-X vector 0
  of four, then MSI, and only then the line.
- **Its VT-d is two units**: `dmar0`, the UHD 730's alone, left be, and
  `dmar1` for everything else -- taken over with `iommu=on`, every device
  passed through, queued invalidation, not coherent (each table entry
  written back with `clflush`), 4-level tables (SAGAW 48-bit alone). The
  A2000 goes to a Linux guest running NVIDIA's own driver: `nvidia-smi`,
  CUDA's deviceQuery, vectorAdd and matrixMul pass, matrixMul at
  611 GFlop/s as under QEMU, `boot: complete` to the guest's power-off and
  the reboot after it in 20 seconds.
- **Its console is EGA text**: GRUB hands nos no framebuffer, so the kernel
  writes 0xB8000 and the CRTC's ports, the legacy window the firmware routes
  to the boot VGA -- the A2000. Given to a guest, the GPU takes the screen
  with it: held while its BARs are sized, then let go of for good
  ([The hypervisor](hypervisor.md#a-hosts-pci-device)).
- **Its TCO watchdog does not count**: NO_REBOOT clears, in TCO1_CNT, but
  the timer stays at its reload value with TMR_HLT clear -- started by nos
  and started by hand from Linux alike (TCO_LOCK is set by the firmware; the
  ACPI PM timer runs) -- so a hang there is not reset by it. `panic=10`
  brings back a nos that panics, and `deadman=` one that runs but cannot be
  reached.

`tests/gpu-passthrough/install-bare.sh` boots it once from Ubuntu: nos-install
onto the second NVMe, named by `/dev/disk/by-id` -- Linux swaps `nvme0` and
`nvme1` between boots -- `deadman=900`, the disk log, netconsole to a
collector on another host, and an `/etc/rc` that ends in `reboot`. Its
BootOrder tries the NIC's PXE first: POST to nos takes about two minutes, a
boot of nos and back to Ubuntu about four.

`tests/gpu-passthrough/install-bare-nvme.sh` gives a guest nos's own NVMe as
well, beside the GPU: nos boots instead from two partitions made in the
free space at the end of Ubuntu's disk (`nos`, 699 MiB, and `nos-disklog`,
64 MiB, since 2026-11-19 -- Ubuntu's table dumped before), and
`guestpci=05:00.0` keeps its drivers off the Samsung it usually boots from.
The guest boots its root from that Samsung (`nos-guest`, 8 GiB), writes to
`nos-scratch` (1 GiB), and `--check` sums both from Ubuntu after. Ubuntu's
disk is `02:00.0`, behind the CPU's x4 port; nos's is `05:00.0`, behind the
PCH's `1c.4`: each port has ACS, which the firmware leaves off and hv turns
on, so each NVMe is a group of its own. `hv pci` refuses the integrated
graphics: the DMAR names memory for it (RMRR, 0x7c000000-0x807fffff).

## Newton: a Supermicro X10DRi of two sockets

A **Supermicro X10DRi** (AMI UEFI 3.4a, Secure Boot off, a C610 PCH, an
ASPEED BMC): two **Xeon E5-2670 v3** -- Haswell-EP, twelve cores and
twenty-four threads each -- and 256 GiB, 128 GiB a socket; Ubuntu on one SSD,
nos on the other, its console the network. It is the first machine nos met
of more than one NUMA node, and the one they were measured on
([NUMA](numa.md)).

- **Two nodes.** Its SRAT puts the first socket's 24 CPUs and the memory
  below 2 GiB and from 4 GiB to 130 GiB in one proximity domain, the second
  socket's and the 128 GiB above in the other; its SLIT says 21 between them.
  nos reads both (`numa: node 0 (domain 0): CPUs 0-11,16-27 (24), 131072 MiB
  the firmware placed in 2 ranges`), spreads the free pages over the two
  (`mm: node 0: 128880 MiB free`, `node 1: 131071`), shares the huge pool
  out by them and zeroes each node's pages on its own CPUs. Measured with
  `numabench`, a line of the other socket's memory takes 193 ns against 138,
  and one CPU reads it at 6.4-6.8 GB/s against 8.2-8.7; every page a node's
  memory was given was that node's.
- **Its CPUs are numbered with gaps**: the MADT lists the APIC ids 0x00 to
  0x3B, a socket's twelve cores being 0-5 and 8-13 -- the first threads of
  both sockets' cores, then the second -- and a CPU's number in nos is its
  APIC id, so the 48 are 0-11, 16-27, 32-43 and 48-59, well under
  `MAX_CPUS`. The firmware has x2APIC opted out: xAPIC.
- **Its TCO watchdog is a version-2 TCO**, at PMBASE + 0x60 (0x460), its
  NO_REBOOT bit in the chipset's GCS register (RCBA + 0x3410), set by the
  firmware. nos used to take the LPC bridge's config register 0x50 as the
  TCO's base -- the offset a 100-series PCH and later keep it at in their
  SMBus function, not their bridge; here it holds 0xF8, and the watchdog
  was armed at ports 0xE4-0xE8, nobody's. It now takes PMBASE + 0x60 alone
  (`TCOBASE 0x0460 via LPC 0:1F.0 ACPI base + 0x60, NO_REBOOT clear in
  GCS`), clears NO_REBOOT through RCBA and reads it back, and an MMIO
  mapping that would cover usable RAM is refused rather than made.
- **Its disks are on the C610's sSATA** (00:11.4, 8086:8d62, AHCI): an Intel
  S3500 of 800 GB that is nos's (`nos-install disk`, by its WWN) and Ubuntu's
  SSD; the SATA controller beside it has none.
- **Its NIC is a two-port Intel I350** (03:00.0, MSI-X), the X11SSD-F's;
  the network hands out no address (`--network-here`).
- **Three IO-APICs** -- the PCH's at GSI 0, which nos programs, and one a
  socket at 24 and 48 -- two DMAR units, a PCI root a socket (00-7e,
  80-fe), and an xHCI whose BAR is 64-bit, above 56 TiB.
- **VT-x is off in its firmware** -- its Linux says `VMX (outside TXT)
  disabled by BIOS` -- so no guest of hv's runs there until it is turned on
  in the firmware's setup, which only its console reaches.

Its BootOrder tries both I350 ports' PXE before Ubuntu; `nos-install boot`
has the firmware boot nos next, once. The BMC's watchdog, armed from Linux
(`ipmitool raw 0x06 0x24 0x04 0x01 0x00 0x10 <lsb> <msb>`, in 100 ms, then
`raw 0x06 0x22`), keeps counting through POST and resets the machine into
Ubuntu if nothing stops it: a test boot's way back, beside `deadman=`. A
boot of nos and back to Ubuntu takes some six minutes -- two and a quarter
from Ubuntu's reboot to nos's sshd, four from nos's own `reboot` to
Ubuntu's, through both ports' PXE; nos says `boot: complete` 9 s after
GRUB, sshd listening at once. It was 16 s: nine of them every log line
waiting out its bytes at COM1's 115200 baud, a second each I350 port spent
on a wait for its link that never saw it come, a second each HBA on its
empty ports, and 10 ms after each AP's INIT. What is left of the nine is
the early boot's, before the UART's interrupt is set up -- a few lines a
CPU of the MADT and the SRAT.

## Kovalevskaya: a Supermicro X11SSL-F

A **Supermicro X11SSL-F** (AMI UEFI 3.6 of 2025, Secure Boot off): the
X11SSD-F's **Xeon E3-1230 v6** and C236 again, 32 GiB, Ubuntu on one Intel
SSD and nos on another, its console the network. Nothing on it was new to
nos, and it was set up as [Install](install.md) says a server is: from its
Ubuntu, with the one-line installer -- `curl -fsSL
https://nos-os.tech/install.sh | sh -s -- disk <the SSD's WWN>
--network-here --ssh-key /root/.ssh/authorized_keys --keep-boot-order` --
the SSD made nos's own, Ubuntu left the firmware's default.

- **Its SATA is the C236's AHCI HBA** (8086:a102, six ports): nos's SSD,
  Ubuntu's and two Seagate disks of 2 TB, `adc` to `adf` on ports 2 to 5,
  at 6 Gbps.
- **Its NICs are two Intel I210** (8086:1533) on the board, the AX41's
  part; `eth0` is 01:00.0, the cabled port. Its MAC latches no speed when
  the link first comes, as the AX41's did, and `igb` forces what the PHY
  resolved: 1000 Mbit/s full duplex, 4.5 s after the PHY's reset.
- **Its network hands out no address** (`--network-here`: `/etc/net.toml`).
- **Its TCO watchdog is the firmware's**, as on the X11SSD-F -- NO_REBOOT
  behind the P2SB the firmware hides -- so a boot on trial comes back by
  `deadman=`.
- **VT-x is on** (`hv run` runs its guests), and one VT-d unit, with
  interrupt remapping, waits for `iommu=on`.
- **Its efibootmgr is 18**, Ubuntu 24.04's, which writes its listing an
  entry at a time and turns on (`-a`) only the last entry it lists
  (rhboot/efibootmgr#186). The first made `nos-install boot` warn here that
  it could not turn on the entry it had just made, on all along: its
  `efibootmgr | grep -q` under pipefail read efibootmgr's SIGPIPE as "off".
  The second would leave an entry of the firmware's that is off as the next
  boot, which firmware passes over. nos-install now reads the listing whole
  before it looks, and makes an entry of nos's own where the firmware's
  stays off.

Its BootOrder tries both I210 ports' PXE before Ubuntu; `nos-install boot`
has the firmware boot nos next, once, and BootNext skips the PXE: 62 s from
Ubuntu's reboot to nos's kernel, the root mounted from `adc3` 6.8 s into the
boot, `boot: complete` at 7.2, sshd listening at once. The CPU already runs
microcode 0xfa, newer than Ubuntu's file (0xf8), so nos loads none.

Its two Seagates are the first disks that spin nos has been measured on,
and they are **SMR**: shingled, the drive's own -- Seagate's datasheet has
the ST2000VX015 SMR and its sibling the ST2000VX008 CMR, and IDENTIFY says
nothing of zones (word 69's are 0), only that all write cache is
non-volatile. A write lands in a media cache on the platters and is moved
to its band later, so what the drive does depends on what was written to
it before: each measurement below is on a 100 GiB slice of it nothing had
written since the provider wiped it, Linux's fio (`randrepeat=0`: fio's own
offsets again are reads the drive's cache answers) against `blkload`.

- **Under the block layer nos and Linux are the drive's**: reads at the
  medium's rate at one in flight, 162 MiB/s at its start and 120-150 past
  a terabyte, and nos's through the device's queue at four as well (115);
  random reads of 4 KiB 49-63 a second at one in flight and 85 at 32 --
  its NCQ gains little -- in both; sequential writes 26-49 MiB/s in Linux
  by the drive's state, commands of a MiB or 4 MiB no faster than of 256
  KiB, nos's 31-33 among them. A stream read by several tasks at once
  reaches it out of order and slows (`blkload ... qd=4`: 28 MiB/s), and
  now and then, for minutes, it reads a stream at one revolution a
  command -- 15-22 MiB/s on a slice nothing had written, at one in flight,
  the CPUs idle: the drive's own doing.
- **A task waiting on it held a CPU**: the driver's waits gave the CPU to
  whoever else could run, and with nobody else they spun. 32 readers at
  85 a second kept 6.3 of the 8 CPUs busy, one sequential reader 0.76 of
  one, and `format ext4` of the 2 TB disk -- 29 GiB of inode tables, 17
  minutes of writes -- one throughout. A waiter now sleeps on its slot's
  event, at once on a disk that spins and after 250 us on one that does
  not: 0.04 and 0.11, a 100 GiB slice's format 2% of one, and the SSD's
  random reads as they were (8 877 a second at one in flight, p50 108.5 us,
  p99 129 us). The wait for a slot stays awake: asleep, the 32nd of 32
  readers lost every slot to the 31 that gave them back and waited 6 s.
- **A file wrote at 1.2 MiB/s.** ext4 commits every write as a transaction
  of its own and a commit flushes the disk's cache, 0.2-1.6 s here: `cp`'s
  64 KiB pieces made two of them a piece, as its forced commit block went
  as a write and a flush. Linux wrote the same 2 GiB file at 41.9 MiB/s.
  With the commit block forced by the disk's own FUA and `cp` writing 512
  KiB at a time (the heap's largest block) it writes at 4.2 MiB/s, and
  reads it back cold at 39.5 MiB/s, not 24.0; the rest of the way is
  commits that take in more than one write.
- **It was cut into eighteen slices, and nos saw eight**: the probe read the
  first eight of a GPT's slots and said nothing of the rest. It reads them
  all now (128).

The boxes before Hertz were set up by hand, before there was a release image
to set one up from. A new server is set up with `nos-install` from the system image,
which makes the partition, the GRUB entry and the one-shot flag that the
sections above describe building by hand ([Install](install.md)).

Other firmware, chipsets, NICs and disks are untested; treat bare-metal support
as "works on the eight machines it was debugged on".
