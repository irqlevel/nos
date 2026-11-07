# Kernel parameters

Space-separated `key=value` pairs. Set them on the GRUB command line on
x86-64 (`build/grub.cfg`, the `multiboot2` line) or with QEMU `-append` on
arm64 (`scripts/qemu-arm64.sh`). Parsing lives in
`src/base/kernel/parameters.cpp`.

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
- `hwp_epp=N` — x86-64, with HWP: the energy/performance preference each CPU's clock is picked by, 0 (performance) to 255 (energy), 128 -- intel_pstate's `balance_performance` -- by default
- `c1e=firmware` — x86-64: leave C1E auto-promotion as firmware set it. By default nos turns it off on an Intel CPU it idles by MWAIT on bare metal (with HWP, which says the part has the register): with it on, every C1 the idle asks for -- and it asks for C1 whenever its idles are short, which a CPU running a guest's vCPU or its disk always is -- becomes C1E, the core's voltage and clock dropped while it waits, which it takes microseconds to climb back from at each wake. Linux's `intel_idle` makes the same choice (its `c1e_promotion`). The boot says what firmware had (`cpu: C1E auto-promotion on by firmware, now off`)
- `idle=halt` — x86-64: an idle CPU halts (HLT), and nothing else. By default an idle CPU sleeps by MWAIT with interrupts masked, so that how long each idle lasted is known exactly, and in the deepest C-state MWAIT offers (CPUID leaf 5) once its last four idles each lasted 2 ms or more and its next timer is as far off; short of that, in MWAIT's C3 -- a core's C6, its wake a hundred or two microseconds -- once its next timer is 1 ms off or more, unless a guest's vCPU or disk holds the CPU (`kernel_cpu_performance`, `kernel_cpu_latency`); in C1, as a halt, otherwise: a CPU woken often by what no timer predicts -- a disk's completions, a vCPU's IPIs -- so stays where a wake is quick, rather than pay a deep C-state's exit, hundreds of microseconds on a desktop part's C10, at every one. With only the deepest state and C1, the EX44's busy vCPU core ran at 4.55 GHz while idle P-cores' threads spent 36-99% of the window in C1, where under Linux the same core ran at 4.8 (round 23's `cpufreq`): a core that is not deep counts among the busy ones a busy core's turbo is figured from. That is on bare metal, where the CPU has MONITOR/MWAIT with an interrupt ending it though masked, C-states in leaf 5 and an APIC timer that runs in every C-state (ARAT); anywhere else -- under a hypervisor, on AMD's parts, which list no C-state there -- it halts anyway. A busy core then runs at the turbo of few busy cores: the EX44's P-cores ran at 4.46 GHz with every idle core halted and at 4.46-4.72 with them asleep deep, where Linux's ran at 4.77 -- CPU-bound work 2-3% faster, nothing measured slower ([`plans/vm-performance.md`](../plans/vm-performance.md)). `idle=mwait` says the default
- `ucode=off` — x86-64: load no microcode from the module GRUB put beside the kernel (`module2 /boot/ucode ucode`, which [Install](install.md) sets up); every CPU runs the revision firmware loaded. See [Boot](boot.md)

## Memory

- `hugepages=N%` / `hugepages=NG` / `hugepages=NM` — reserve that share of RAM, or that much of it, at boot as the huge pool: 2 MiB blocks, physically contiguous and aligned, taken off the free list right after it is built -- before anything has fragmented it -- the highest first, and kept for guests' memory alone (e.g. `hugepages=50%`, `hugepages=32G`). With a pool, every guest's memory is the pool's, each block one entry of its nested page table -- 2 MiB of TLB where a frame gives 4 KiB -- and a guest the pool has not room for does not start, saying how many blocks it wanted and how many are free; without one, a guest's memory is 4 KiB frames, as it always was. What is asked is cut down to leave twice the kernel's reserve free (a sixteenth of RAM, and no less than 64 MiB, twice over), and the boot says what it took: `mm: hugepages: N 2MiB blocks reserved (M MiB) of K asked`. `meminfo`, `memusage` and `hv info` show the pool. A block is the pool's for as long as the machine is up, guest or no guest. See [The hypervisor](hypervisor.md#how-much-of-it-and-where)

## Console and logging

- `console=serial` — shell input and output on the serial port only
- `console=vga` — shell input and output on the screen only (VGA text or framebuffer)
- `console=both` — both (the default); listed for completeness
- `trace=vga` — x86-64 only: mirror the boot trace on the screen as well as the serial port. Off by default because redrawing glyphs costs a full repaint per scrolled row; on a machine with no UART the mirror is the default anyway, since there is nowhere else to put it
- `tracepoints=<list>` — events to record from the boot on (`kernel/tracepoint.h`): their names, their groups or `all`, comma-separated -- turned on as the boot ends, once every CPU has its ring, and said so in the log; a name it does not know is said too. `trace` in the shell changes them afterwards. See [Tracepoints](tracing.md)
- `loglevel=N` — log level to boot with (0-5, default 1). The `loglevel` shell command moves it afterwards, but only a boot parameter can make the *boot* chatty; the task lifecycle and the multitasking self-test are at level 3
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
- `rxpoll=on` — have the tick look at the receive path as well as the NIC's interrupt (off by default, see `DeviceTable::poll_rx` in `src/crates/layers/net/src/device.rs`)

## Devices and filesystems

- `usb=off` — x86-64 only: skip xHCI bring-up (no USB keyboard; the 8042 keyboard is unaffected)
- `its=off` — arm64 only: disable the GICv3 ITS and degrade PCIe MSI gracefully (default `its=on`; virtio-mmio devices don't need it)
- `hwrng=off` — ignore the CPU's random instruction (RDRAND/RDSEED, RNDR), leaving the entropy pool with virtio-rng and timing jitter. How the fallback path gets exercised on a machine that has one; `entropy` then reports whether anything else reached the pool. See [Randomness](random.md)
- `rc=off` — do not run `/etc/rc`, the shell commands the shell's task runs once the network is up (the `rc` command edits it). The way past a line in it that keeps the machine from coming up. See [sshd](sshd.md#starting-it-at-boot)
- `root=auto` — mount the ext4 (or ext2) filesystem labelled `nos` read-write on `/`; without one, a ramfs on `/`, the first of the ext family found read-only on `/boot` and the first nanofs found read-write on `/data`. Without `root=` the VFS starts empty and filesystems are mounted from the shell
- `root=<device>` / `root=LABEL=<label>` / `root=UUID=<uuid>` — the root by block device name (`vda1`, `nvme01`), ext4 or ext2 volume label or UUID. A root that is not found falls back to the `auto` layout, so the shell is always there; so does a label or a UUID two devices carry -- `auto`'s `nos` among them -- which names neither, the boot saying which two, since one may be a guest's (`hv devices`). See [Filesystems](filesystems.md)
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
