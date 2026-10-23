---
title: "Debug nos with GDB"
description: "Attach GDB to QEMU, read a panic and its backtrace, raise trace levels, bisect a failing boot, and get the log off a machine with no serial port."
order: 4
updated: 2026-10-22
---

This guide shows you how to stop a running nos in GDB and see where every CPU is. You will also learn to read a panic report, turn up logging for one subsystem, narrow down a failing boot with kernel parameters, and get the log out of a machine that has no serial port.

You need:

- a kernel you [built](/howto/build); the GDB scripts load `bin/kernel64.elf` or `bin/kernel-arm64.elf`;
- QEMU (see [Run nos in QEMU](/howto/run-in-qemu));
- GDB for x86-64. For arm64, a GDB that supports aarch64 (`gdb-multiarch` or `aarch64-elf-gdb`), or LLDB.

## Attach GDB to QEMU

### x86-64

`qemu.sh`, `qemu-disk.sh` and `run.sh` all start QEMU with `-s`. That flag opens QEMU's GDB stub, a GDB server built into QEMU, on TCP port 1234. While one of these scripts is running, start GDB:

```sh
./scripts/gdb64.sh
```

This runs GDB with the commands in [`scripts/gdb-debug64`](https://github.com/irqlevel/nos/blob/master/scripts/gdb-debug64):

```text
set architecture i386:x86-64
target remote 127.0.0.1:1234
symbol-file bin/kernel64.elf
set print pretty on
set height 20000
thread apply all bt
```

QEMU shows each virtual CPU to GDB as a thread, so the last command prints a backtrace for every CPU. When a machine stops responding, that's the first thing to look at. GDB then leaves you at its prompt, with the machine stopped. `info threads`, `thread N`, `bt`, `break` and `continue` work as usual, and Ctrl-C stops the machine again. Here is the same without the script:

```sh
gdb -ex "symbol-file bin/kernel64.elf" \
    -ex "set architecture i386:x86-64" \
    -ex "target remote :1234"
```

The C++ is compiled with full debug information (`-g3`). The Rust code is a staticlib built in release mode without it, so in a Rust frame GDB shows the function name but no source line.

To debug from the very first instruction, add QEMU's `-S` next to `-s`. QEMU then waits for GDB before it runs anything. `qemu.sh` doesn't take extra arguments, so run its QEMU command yourself:

```sh
qemu-system-x86_64 -smp 2 -cdrom nos.iso -serial file:nos.log -s -S
```

### arm64

`qemu-arm64.sh` passes its arguments on to QEMU:

```sh
./scripts/qemu-arm64.sh -s -S
```

Then, in another terminal:

```sh
./scripts/gdb-arm64.sh
```

The script runs `gdb-multiarch` (or `aarch64-elf-gdb`, if that's the one you have) with the commands in [`scripts/gdb-debug-arm64`](https://github.com/irqlevel/nos/blob/master/scripts/gdb-debug-arm64). That file does the same as the x86-64 one, but loads `bin/kernel-arm64.elf`. Type `continue` to start the kernel. If neither GDB is installed, the script says so and suggests LLDB instead:

```sh
lldb bin/kernel-arm64.elf
```

At LLDB's prompt, connect with `gdb-remote 1234`.

## Read a panic

A panic report goes to the serial port and the screen. When netconsole and the disk log are on, it goes to them too. The CPU that panicked stops the other CPUs. The machine then stays stopped with the report on its console, unless it was booted with `panic=N`. In that case it resets N seconds (1–3600) after the report has gone out. Here is the shape of the report, line by line, as [`kernel/panic.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/panic.cpp) prints it:

```text
PANIC:<function>():<file>,<line>: <message>
CPU: <index>
Task: pid <pid> name <task>
Backtrace:
  [0] 0x<address> <function>+0x<offset>
  [1] 0x<address> <function>+0x<offset>
Modules: <name> 0x<base>+0x<size>
Cpu <index> backtrace:
  [0] 0x<address> <function>+0x<offset>
```

- The first line says where the panic happened and why. For a broken invariant caught by `BugOn`, it says `BUG`. For a CPU exception, it shows what the CPU reported: `EXC: PageFault cr2 0x... rip 0x... rsp 0x... err 0x... cr3 0x...` on x86-64, or `sync exception: ... (esr 0x... ec 0x... iss 0x...) far 0x... elr 0x...` on arm64. A Rust panic starts with `RUST PANIC: panicked at <file>:<line>:<column>:`, followed by its message.
- `Backtrace` is the stack of the CPU that panicked. The kernel resolves the names itself, from a symbol table built into the image, so you don't need a symbol file. Each frame is a return address, named after the call just before it.
- A frame in a module's code ends with the module's name in brackets. When any module is loaded, the report also has a `Modules:` line with each module's address range. It lets you place a frame that nothing else could name.
- On x86-64 the kernel asks the other CPUs for their stacks by sending them an NMI. Each answer follows as `Cpu N backtrace:`. A CPU that doesn't answer is named too (`Cpu N did not answer the NMI`).
- A frame printed as a bare address lies outside the kernel's code and outside every loaded module.

Here is an example from [Backtraces](/docs/modules#backtraces):

```text
Backtrace:
  [0] 0xFFFF800012DA1C40 mod_hello::init::{closure#0}+0x3c [hello]
  [1] 0xFFFF80000104A2F1 Kernel::Cmd::DispatchDynamic+0xd1
...
Modules: hello 0xFFFF800012DA0000+0x5000
```

Some hangs are turned into panics so that they get reported. A spinlock held too long panics with `Spinlock 0x... stuck, held ... ns`. A TLB shootdown that gets no answer within ten seconds panics and names the CPUs that didn't answer.

To see a panic report on a throwaway VM, use the shell commands `panic` (a direct panic), `panic pf` (a page fault) and `panic ud` (an invalid instruction).

When a task is alive but stuck, `ps` lists the tasks and `bt <pid>` prints one task's backtrace. To get it, `bt` sends an IPI to the CPU the task runs on. `stacks` shows the deepest point each stack has reached.

## Read the kernel log and raise trace levels

Every trace line has the form `level:seconds.microseconds:function(),file,line: message`. It goes to the kernel log and the serial port, and also to netconsole and the disk log when they are on. Once the shell is running, trace lines no longer reach the console. Read them with `dmesg [lines] [filter]`, for example `dmesg 40` or `dmesg 100 virtio`.

A line is printed if its level is no higher than the current trace level. The trace level runs from 0 to 5 and defaults to 1. You can change it in two ways:

- `loglevel=N` on the kernel command line. It's the only way to get more detail from the boot itself.
- `loglevel N` in the shell, on a running kernel. Run `loglevel` alone to see the current level. The command works over the UDP shell and SSH too, which matters on a machine with no other console.

The noisier subsystems trace at their own levels, which are constants at the top of [`src/cpp/kernel/trace.h`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/trace.h):

| Level | Constants | What you see at that level |
|---|---|---|
| 0 | `ExcLL`, `AcpiLL`, `CmdLL`, `LapicLL`, `IoApicLL` | always printed |
| 3 | `KbdLL`, `UsbLL`, `TestLL`, `IcmpLL`, `TaskLL`, `ModuleLL` | keyboard and USB, the multitasking self-test, three lines per ping, every task's creation and teardown, what each module import was bound to |
| 4 | `PageAllocatorLL`, `AllocatorLL`, `PoolLL`, `MmIoLL` | the allocators (every allocation) and MMIO; very noisy |

To see one subsystem without the rest, lower its constant and rebuild. Rust code traces with `trace!(level, ...)` on the same scale.

## Bisect a failing boot

Where you set the kernel command line depends on how you boot:

| Boot | Where | Then |
|---|---|---|
| x86-64 ISO | the `multiboot2` line in `build/grub.cfg` | rebuild `nos.iso` |
| x86-64 disk image | the `multiboot2` line in `build/grub-disk.cfg` | rerun `./scripts/build-disk.sh` |
| arm64 in QEMU | `APPEND=` in `scripts/qemu-arm64.sh` | boot again |
| a server set up from the system image | `nos-install`'s `--cmdline "ARGS"` | see [Install nos on a dedicated server](/howto/install-on-a-server) |

Nothing on the command line can stop a boot. The kernel logs an unknown key or value and skips it, and the line is limited to 255 characters. Take things out of the boot path one at a time:

1. `smp=off`: the boot CPU alone. If the kernel boots this way, the problem is in bringing up the other CPUs.
2. `maxcpus=2`: the boot CPU and one more, then more. This keeps SMP but limits how many CPUs come up.
3. `loglevel=4`: everything the boot does, allocations included.
4. Then the subsystems, one by one:
   - `usb=off` (x86-64): skips xHCI bring-up;
   - `its=off` (arm64): turns off the GICv3 ITS; PCIe MSI degrades, and virtio-mmio doesn't need it;
   - `hwrng=off`: ignores the CPU's random-number instruction;
   - `rc=off`: skips `/etc/rc`, which gets you past a line in it that keeps the machine from coming up;
   - `dhcp=off`: turns DHCP off entirely;
   - `console=serial` or `console=vga`: keeps the shell on one console only.

On x86-64, a boot that fails before any console exists prints `ERR:` and a digit on the screen. That means one of the early checks in `boot64.asm` failed: the Multiboot, CPUID or long-mode check.

All parameters are listed in [Kernel parameters](/docs/kernel-parameters), and [Boot](/docs/boot) explains what happens between the markers.

## Get the log off a machine with no serial port

On a laptop, a cloud VM or a dedicated server, there's often no serial port you can read. nos offers these alternatives:

- **Netconsole**, for a boot that gets as far as its network. Boot with `netconsole=ip:port dhcp=auto nctail=64`, and run `python3 scripts/netconsole.py -o boot.log` on the receiving machine. With `nctail=64`, a machine that has little time left after its link comes up spends it on the newest lines. The log arrives from the first line of the boot, and it includes a panic report if the collector's MAC address is already known. [Connect to a running nos](/howto/connect) explains the setup.
- **The disk log**, for a boot that stops before its network is up. Under Linux on that machine, prepare a log area on a spare partition. Then boot nos with `disklog=on`, and read the log back at the next Linux boot:

  ```sh
  sudo ./scripts/disklog.py format /dev/nvme0n1p1
  sudo ./scripts/disklog.py read /dev/nvme0n1p1
  ```

  `format` refuses a device that still looks like it holds a filesystem, a RAID member or swap. Without `disklog=on`, nos doesn't even read any disk to look for the area. With it but without a prepared area, the boot logs `DiskLog: disklog=on, but no prepared area on any disk` and carries on. The `disklog` shell command shows what it found.
- **The screen.** `trace=vga` (x86-64) shows the boot log on the screen as well as on the serial port. On a machine with no UART, that's already the default.
- **`panic=N`.** A machine whose only console is the network comes back N seconds after a panic, instead of staying stopped where nobody can see it.

[Real hardware](/docs/real-hardware) describes how these tools were used on the three machines nos runs on.

## Build with undefined-behaviour checks

When a boot hangs in some unrelated place, and only on one architecture, it may be undefined behaviour that surfaces far from its cause. Build with the checks on and boot the same way:

```sh
make nocheck UBSAN=1 RUSTUB=1
./scripts/smoke-test.sh --skip-build
```

The first undefined operation in the C++ panics and names its site. In the Rust, `core` (Rust's base library) checks the preconditions of unsafe operations, and the first one it finds violated panics the same way. With `ubsan=warn` on the command line, the kernel instead prints every C++ report of the boot and keeps going. Each site is reported once, with a backtrace. For more, see [Build nos](/howto/build).

## Run the boot self-tests

The self-tests run on every boot, from `Test()` in [`src/cpp/kernel/test.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/test.cpp). A failing test panics with `Self test failed`. To run just one test, edit `Test()` so that it calls only that function, then rebuild, boot and watch `nos.log`.

## Troubleshooting

- **`target remote` fails.** QEMU isn't running with `-s`, or another QEMU already holds port 1234. Only one QEMU can use the port at a time.
- **`gdb-arm64.sh` prints `No aarch64 GDB found`.** Install `gdb-multiarch` or `aarch64-elf-gdb`, or use LLDB as shown above.
- **`bin/kernel-arm64.elf` is missing.** Building the x86-64 ISO deletes and recreates `bin/`. The same ELF is in the repository root, so copy it back: `cp kernel-arm64.elf bin/`.
- **GDB's symbols don't match what's running.** Load the ELF from the same build as the image you booted. After a rebuild, restart both QEMU and GDB.
- **`loglevel 4` floods everything.** It traces every allocation, and every line also goes out the serial port. Run `loglevel 1` to return to the default.
- **The panic report never reached the netconsole.** During a panic, the report goes out only if the collector's MAC address is in the ARP cache. Also boot with `disklog=on` and a prepared area, or use `panic=N` to get the machine back.

## Next steps

- [Run the gates](/howto/run-the-gates): a check for each subsystem, for the failures a boot can't notice
- [Connect to a running nos](/howto/connect): the UDP shell, SSH and netconsole in detail
- [Run nos in QEMU](/howto/run-in-qemu)
- The full references: [Debug](/docs/debug), [Kernel parameters](/docs/kernel-parameters), [Profiler](/docs/profiler)
