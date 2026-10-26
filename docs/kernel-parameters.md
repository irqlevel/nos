# Kernel parameters

Space-separated `key=value` pairs. Set them on the GRUB command line on
x86-64 (`build/grub.cfg`, the `multiboot2` line) or with QEMU `-append` on
arm64 (`scripts/qemu-arm64.sh`). Parsing lives in
`src/cpp/kernel/parameters.cpp`.

Nothing on the command line stops the boot. An unknown key, a known key with
a value it does not recognise, and a parameter that cannot be parsed at all
— no `=`, nothing before or after it, a bare word other than `ro`, longer
than 48 characters — are each logged and skipped, and the rest of the line
is still read, as Linux does. The whole line is capped at 255 characters; a
longer one is read up to its last whole parameter, since half of one is a
different one. The warnings land in `dmesg`, which the
[netconsole](netconsole.md) replays from the start of the boot.

The defaults the shipped configs use are `dhcp=auto dns=on root=auto
fstest=on` on the x86-64 ISO, `console=serial dhcp=auto dns=on root=auto` on
the disk image, `dhcp=auto dns=on root=auto` on arm64 (`scripts/qemu-arm64.sh`),
and `root=UUID=<partition> dhcp=auto dns=on` on a server the system image is
poured onto ([Install](install.md)). None of them starts the UDP shell: it
runs any command for anyone who can send the machine a datagram, so it is on
only where `udpshell=` is given -- `nos-install --udpshell`,
`NOS_UDPSHELL=9000 ./scripts/qemu-arm64.sh`, a test's own command line.

## CPUs

- `smp=off` — disable SMP, run on BSP only
- `maxcpus=N` — bring up at most N CPUs, the BSP included (`maxcpus=2` = BSP + one AP); the rest stay parked. Bisects an SMP bring-up failure without giving up SMP entirely
- `hwp=off` — x86-64: leave every CPU's clock where firmware left it, rather than turning on hardware P-states (HWP) and letting the CPU pick its own up to its turbo. Firmware's is usually the base clock: the EX44's cores ran at 2.5 GHz with it, their turbo being 4.8. See [Boot](boot.md)
- `ucode=off` — x86-64: load no microcode from the module GRUB put beside the kernel (`module2 /boot/ucode ucode`, which [Install](install.md) sets up); every CPU runs the revision firmware loaded. See [Boot](boot.md)

## Memory

- `hugepages=N%` / `hugepages=NG` / `hugepages=NM` — reserve that share of RAM, or that much of it, at boot as the huge pool: 2 MiB blocks, physically contiguous and aligned, taken off the free list right after it is built -- before anything has fragmented it -- the highest first, and kept for guests' memory alone (e.g. `hugepages=50%`, `hugepages=32G`). With a pool, every guest's memory is the pool's, each block one entry of its nested page table -- 2 MiB of TLB where a frame gives 4 KiB -- and a guest the pool has not room for does not start, saying how many blocks it wanted and how many are free; without one, a guest's memory is 4 KiB frames, as it always was. What is asked is cut down to leave twice the kernel's reserve free (a sixteenth of RAM, and no less than 64 MiB, twice over), and the boot says what it took: `mm: hugepages: N 2MiB blocks reserved (M MiB) of K asked`. `meminfo`, `memusage` and `hv info` show the pool. A block is the pool's for as long as the machine is up, guest or no guest. See [The hypervisor](hypervisor.md#how-much-of-it-and-where)

## Console and logging

- `console=serial` — shell input and output on the serial port only
- `console=vga` — shell input and output on the screen only (VGA text or framebuffer)
- `console=both` — both (the default); listed for completeness
- `trace=vga` — x86-64 only: mirror the boot trace on the screen as well as the serial port. Off by default because redrawing glyphs costs a full repaint per scrolled row; on a machine with no UART the mirror is the default anyway, since there is nowhere else to put it
- `loglevel=N` — trace level to boot with (0-5, default 1). The `loglevel` shell command moves it afterwards, but only a boot parameter can make the *boot* chatty; the task lifecycle and the multitasking self-test are at level 3
- `netconsole=ip:port` — stream the kernel log over UDP to that collector (e.g. `netconsole=10.0.2.2:6666`); needs an address, so pair it with `dhcp=auto`. See [Netconsole](netconsole.md)
- `nctail=N` — ship only the newest N KiB of the log buffered before the link came up (e.g. `nctail=16`); the rest is dropped. For a machine that wedges shortly after the network appears, this spends the little airtime it has on the lines around the wedge instead of on the head of the boot log
- `disklog=on` — write the kernel log, from its first line, to a disk area laid down with `scripts/disklog.py format`: for a machine with no serial port whose boot stops before its network is up. The boot so far is written when the area is found and every line after by a task of its own, so nothing that traces waits on the disk; a panic report goes down too. Off by default, since every burst of lines is a forced disk write. It takes both, the parameter and a prepared area: without the parameter no disk is so much as read, and with it but no area found the boot logs `disk_log: disklog=on, but no prepared area on any disk` and carries on without. `disklog` in the shell says which. It works on both architectures; `scripts/disklog-test.py` is its test. While the log runs the area is its own: a mount of it, or of the disk it is on, and a `blkload` write test are refused. See [Real hardware](real-hardware.md)
- `panic=vga` — parsed and currently has no effect: the panic report already goes to the screen whenever one is ready, on both architectures
- `panic=N` — reset the machine N seconds (1–3600) after a panic's report has gone out: to the screen and the serial port, and flushed to the netconsole and the disk log. Timed on the cycle counter; a panic before it is calibrated resets at once. 0, the default, keeps the machine stopped with the report on its screen. For a machine whose only console is the network: the system image's GRUB entries pass `panic=10`, so a server whose one-shot nos boot dies is back on its own OS by itself ([Install](install.md))

## Networking

- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)
- `dns=on` — enable DNS resolver (uses DHCP-provided DNS server; requires `dhcp=auto`)
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`). Off unless given: it has no authentication. See [UDP remote shell](udp-shell.md)
- `netframes=N` — frames in the network frame pool (64–65536); watch `netpool` for misses
- `rxpoll=on` — have the tick look at the receive path as well as the NIC's interrupt (off by default, see `DeviceTable::poll_rx` in `src/rust/net/src/device.rs`)

## Devices and filesystems

- `usb=off` — x86-64 only: skip xHCI bring-up (no USB keyboard; the 8042 keyboard is unaffected)
- `its=off` — arm64 only: disable the GICv3 ITS and degrade PCIe MSI gracefully (default `its=on`; virtio-mmio devices don't need it)
- `hwrng=off` — ignore the CPU's random instruction (RDRAND/RDSEED, RNDR), leaving the entropy pool with virtio-rng and timing jitter. How the fallback path gets exercised on a machine that has one; `entropy` then reports whether anything else reached the pool. See [Randomness](random.md)
- `rc=off` — do not run `/etc/rc`, the shell commands the shell's task runs once the network is up (the `rc` command edits it). The way past a line in it that keeps the machine from coming up. See [sshd](sshd.md#starting-it-at-boot)
- `root=auto` — mount the ext2 filesystem labelled `nos` read-write on `/`; without one, a ramfs on `/`, the first ext2 found read-only on `/boot` and the first nanofs found read-write on `/data`. Without `root=` the VFS starts empty and filesystems are mounted from the shell
- `root=<device>` / `root=LABEL=<label>` / `root=UUID=<uuid>` — the root by block device name (`vda1`, `nvme01`), ext2 volume label or UUID. A root that is not found falls back to the `auto` layout, so the shell is always there. See [Filesystems](filesystems.md)
- `ro` — mount the root read-only
- `fstest=on` — run the filesystem self-test on `/` once it is mounted (prints `fstest: passed` or `fstest: FAILED`); the smoke tests boot with it

## Diagnostics

- `wxprobe=text` (also `wxprobe=on`) — after the kernel image has been split into text RX / rodata RO+NX / data RW+NX, deliberately write to `.text`. The machine should die with a page fault (x86-64) or a data abort (arm64); a `W^X probe: text write SUCCEEDED` line means the protection is not doing its job. See [Paging](paging.md)
- `wxprobe=heap` — once the page allocator is up, call into a page it just handed out. Every runtime mapping is non-executable, so the machine should die on the instruction fetch; a `W^X probe: heap execute SUCCEEDED` line means W^X covers the kernel image and nothing else
- `ubsan=warn` — in a `UBSAN=1` build, print every undefined-behaviour report (each site once, with a backtrace, to the serial port and the disk log) and go on, instead of panicking on the first; see [Build](build.md#ubsan). Checks that fail before the command line is read still panic

## Bisecting a boot failure

Try in this order: `smp=off`, then `maxcpus=2`, then `loglevel=4`, then take
subsystems out of the path with `usb=off` / `its=off` / `console=`. On a
machine with no serial port, add `netconsole=ip:port` (with `nctail=64`) so
there is a log to read at all; one that stops before its network is up
needs `disklog=on` and a prepared area instead — see [Debug](debug.md),
[Boot](boot.md) and [Real hardware](real-hardware.md).
