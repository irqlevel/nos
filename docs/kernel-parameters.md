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
poured onto ([Install](install.md)) -- where the interfaces' settings are its root's
`/etc/net.toml` and not those words (Networking, below). None of them starts the UDP shell: it
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
- `splitlock=off` — x86-64: leave split lock detection off. By default every CPU that has it turns it on (`IA32_CORE_CAPABILITIES` bit 5, behind CPUID leaf 7's EDX bit 30: Intel's parts since Ice Lake's servers and Tremont -- of the machines here the EX44's and Hertz's), and a split lock -- a locked access across two cache lines, which takes the bus from every core while it runs -- then raises #AC: in the kernel a panic that names it, in a guest of `hv`'s a line in `/logs/hv.log`, one a VM every five seconds at most, the guest stepped past it with the detection off for that instruction alone. The boot says how many CPUs detect them (`cpu: split lock detection on N of M cpus`); `panic splitlock` makes one. See [Boot](boot.md#split-locks)

## Memory

- `hugepages=N%` / `hugepages=NG` / `hugepages=NM` — reserve that share of RAM, or that much of it, at boot as the huge pool: 2 MiB blocks, physically contiguous and aligned, taken off the free list right after it is built -- before anything has fragmented it -- the highest first, and kept for guests' memory alone (e.g. `hugepages=50%`, `hugepages=32G`). With a pool, every guest's memory is the pool's, each block one entry of its nested page table -- 2 MiB of TLB where a frame gives 4 KiB -- and a guest the pool has not room for does not start, saying how many blocks it wanted and how many are free; without one, a guest's memory is 4 KiB frames, as it always was. What is asked is cut down to leave twice the kernel's reserve free (a sixteenth of RAM, and no less than 64 MiB, twice over), and the boot says what it took: `mm: hugepages: N 2MiB blocks reserved (M MiB) of K asked`. `meminfo`, `memusage` and `hv info` show the pool. A block is the pool's for as long as the machine is up, guest or no guest. See [The hypervisor](hypervisor.md#how-much-of-it-and-where)

- `zerofree=on|off|verify` — the zeroing of free pages ahead of their allocation. `on`, the default: two tasks of the lowest priority zero free pages -- 4 KiB ones and the huge pool's free blocks -- while nothing else wants the CPU they are on, so that what a guest leaves in its memory is gone from RAM within seconds of its end, and a guest's memory is taken from pages known zero first. `off`: no task; every page is zeroed as it is handed out, as it always was. `verify`: as `on`, and every page and block an allocation takes as zeroed is read first, a word that is not zero a panic naming the page -- a free page written to, a use after free, caught before it reaches a guest. `memusage` says how much of what is free is zeroed. See [Paging](paging.md#zeroed-ahead-free-pages-and-the-two-lists)
- `heapcheck=on` — a debugging mode for a use after free in the heap: every block the page allocator hands the heap (an allocation of half a page and more) is unmapped the moment it is freed, nothing is kept for reuse, and its address is held back from the next allocations of its size for 1024 frees of that size -- or for fewer where the size has fewer addresses than that to spare: no more than half of them are held back at once. A read or a write through a freed block -- which otherwise lands in whoever has the memory next, silently -- is then a page fault whose backtrace is the culprit's. Off by default: every such free is a TLB shootdown. Smaller allocations, from the pools, are not covered
- `memsites=on` — the allocation-site tracker on from the first of Rust's allocations: every block Rust's allocator takes is given the stack that asked for it, for `memsites` to read the heap by -- which code holds what, to the file and the line with `scripts/memsites.py` ([memsites](memsites.md)). Off by default: on, an allocation pays a walk of its frames and a look in the table of stacks. `memsites on` starts it later, the blocks from before then without a site

## Console and logging

- `console=serial` — shell input and output on the serial port only
- `console=vga` — shell input and output on the screen only (VGA text or framebuffer)
- `console=both` — both (the default); listed for completeness
- `trace=vga` — x86-64 only: mirror the boot trace on the screen as well as the serial port. Off by default because redrawing glyphs costs a full repaint per scrolled row; on a machine with no UART the mirror is the default anyway, since there is nowhere else to put it
- `tracepoints=<list>` — events to record from the boot on (`kernel/tracepoint.h`): their names, their groups or `all`, comma-separated -- turned on as the boot ends, once every CPU has its ring, and said so in the log; a name it does not know is said too. `trace` in the shell changes them afterwards. See [Tracepoints](tracing.md)
- `loglevel=N` — log level to boot with (0-5, default 1). The `loglevel` shell command moves it afterwards, but only a boot parameter can make the *boot* chatty; the task lifecycle and the multitasking self-test are at level 3
- `netconsole=ip:port` — stream the kernel log over UDP to that collector (e.g. `netconsole=10.0.2.2:6666`); needs an address -- a lease's (`dhcp=auto`), or a static one, `/etc/net.toml`'s or `ip=`'s (Networking, below), put on eth0 before the netconsole sends a line. See [Netconsole](netconsole.md)
- `nctail=N` — ship only the newest N KiB of the log buffered before the link came up (e.g. `nctail=16`); the rest is dropped. For a machine that wedges shortly after the network appears, this spends the little airtime it has on the lines around the wedge instead of on the head of the boot log
- `disklog=on` — write the kernel log, from its first line, to a disk area laid down with `scripts/disklog.py format`: for a machine with no serial port whose boot stops before its network is up. The boot so far is written when the area is found and every line after by a task of its own, so nothing that traces waits on the disk; a panic report goes down too. Off by default, since every burst of lines is a forced disk write. It takes both, the parameter and a prepared area: without the parameter no disk is so much as read, and with it but no area found the boot logs `disk_log: disklog=on, but no prepared area on any disk` and carries on without. `disklog` in the shell says which. It works on both architectures; `tests/disklog-test.py` is its test. While the log runs the area is its own: a mount of it, or of the disk it is on, and a `blkload` write test are refused. See [Real hardware](real-hardware.md)
- `panic=vga` — parsed and currently has no effect: the panic report already goes to the screen whenever one is ready, on both architectures
- `panic=N` — reset the machine N seconds (1–3600) after a panic's report has gone out: to the screen and the serial port, and flushed to the netconsole and the disk log. Timed on the cycle counter; a panic before it is calibrated resets at once. 0, the default, keeps the machine stopped with the report on its screen. For a machine whose only console is the network: the system image's GRUB entries pass `panic=10`, so a server whose one-shot nos boot dies is back on its own OS by itself ([Install](install.md))
- `deadman=N` — a boot on trial: N seconds (10–86400) into the boot the kernel panics, its report saying why, unless the shell's `deadman off` came first -- and with `panic=M` the machine resets M seconds later. For a server whose only console is its network, booted into nos once to try something: a nos that panics resets by `panic=` alone, but one that never reaches the network, or wedges where no watchdog looks -- its disks gone, `/etc/rc` stuck -- would sit there until somebody reset it by hand; with `deadman=` it is back on its own OS by itself, and netconsole and the disk log say what became of it. Not for a machine that boots nos every time: unless its `/etc/rc` says `deadman off`, that is a reset every N seconds. `deadman N` sets the deadline N seconds from now, `deadman` says where it stands ([Shell commands](shell-commands.md))

## Networking

The interfaces' settings -- static addresses or DHCP, the default route,
additional routes and the resolver -- are `/etc/net.toml`'s on the root
where there is one: what `nos-install` writes on a server ([The
network](install.md#the-network)), read once the root is mounted and before
anything sends. `ip=`, `gw=`, `dns=` and `dhcp=` are what a boot with no
such file goes by -- an ISO's, QEMU's, a test's, a root that was not found,
a kernel from before the file -- and one whose file is refused, which the
boot says with the file's line. They configure eth0 alone. Where the file
is taken they are not used, and the boot names them (`net: the command
line's ... not used`). The network layer reads both
(`layers/net/src/settings.rs`), through the same checks; `net` says which
it took.

- `dhcp=auto` — start DHCP on `eth0` automatically at boot
- `dhcp=off` — disable DHCP entirely (even via shell command)
- `dhcp=on` — enable DHCP only via shell command (default)
- `dns=on` — enable DNS resolver (uses DHCP-provided DNS server; requires `dhcp=auto`)
- `ip=A.B.C.D/N` — a static address on `eth0` and its prefix length, for a network with no DHCP server. It is put on `eth0` as the network's services start, before the netconsole sends a line, and `dhcp=auto` then leaves `eth0` alone. Refused, with a line in the log, without a prefix, with one past 32, for the subnet's own network or broadcast address (up to /30), and for 0.0.0.0/8, loopback, multicast and class E
- `gw=A.B.C.D` — the default gateway for `ip=`: another address of its subnet. One off the subnet, or given without `ip=`, is named in the log and dropped
- `dns=A.B.C.D` — a resolver on that server from the start, on the default route's interface, whether the address is static or a lease's to come (whose own server then does not replace it). `dns=on` keeps taking the lease's
- `udpshell=PORT` — start UDP remote shell on the given port (e.g. `udpshell=9000`). Off unless given: it has no authentication. See [UDP remote shell](udp-shell.md)
- `netframes=N` — frames in the network frame pool (64–65536); watch `netpool` for misses
- `rxpoll=on` — have the tick look at the receive path as well as the NIC's interrupt (off by default, see `DeviceTable::poll_rx` in `src/crates/layers/net/src/device.rs`)

## Devices and filesystems

- `usb=off` — x86-64 only: skip xHCI bring-up (no USB keyboard; the 8042 keyboard is unaffected)
- `iommu=on` — x86-64 only: take over the Intel VT-d units the ACPI DMAR names (`arch/x86_64/vtd.cpp`): translation on with every device passed through, so that no driver changes, and every DMA a unit blocks said with its address, its requester and the reason (`iommu` shows the units and their faults); and, where every unit taken over has it, interrupt remapping: every MSI, MSI-X vector and IO-APIC pin an entry of its unit's table, verified to come from its own requester, compatibility-format interrupts blocked -- what makes giving a device to a guest (`hv ... pci=`) safe from its DMA to the interrupt range. Off by default; the units are then left as the firmware left them, except that translation, protected memory regions or interrupt remapping it left on are turned off and said. A unit whose devices are all display devices is left be either way
- `guestpci=bb:dd.f[,bb:dd.f...]` — PCI functions kept for guests (`drivers/pci.cpp`, `Pci::scan`): no driver of the host's is shown them -- a driver's lookups by ids or by index pass over them -- so that a VM can be given one whole (`hv ... pci=`), which refuses a function a driver of the host's has taken: a disk's or a NIC's needs this. Given more than once, the lists add up, 16 functions at most; a list with one not written `bus:device.function` in hex, or one past the 16, keeps none of it and is said. `pci` marks each `kept for guests`, and each a driver has taken `a driver's`
- `its=off` — arm64 only: disable the GICv3 ITS and degrade PCIe MSI gracefully (default `its=on`; virtio-mmio devices don't need it)
- `hwrng=off` — ignore the CPU's random instruction (RDRAND/RDSEED, RNDR), leaving the entropy pool with virtio-rng and timing jitter. How the fallback path gets exercised on a machine that has one; `entropy` then reports whether anything else reached the pool. See [Randomness](random.md)
- `rc=off` — do not run `/etc/rc`, the shell commands the shell's task runs once the network is up (the `rc` command edits it). The way past a line in it that keeps the machine from coming up. See [sshd](sshd.md#starting-it-at-boot)
- `root=auto` — mount the ext4 (or ext2) filesystem labelled `nos` read-write on `/`; without one, a ramfs on `/`, and the first of the ext family found read-only on `/boot`. Without `root=` the VFS starts empty and filesystems are mounted from the shell
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
