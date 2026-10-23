---
title: "Run the gates: test a change the way nos does"
description: "What to run after changing nos, and how to read the result: boot self-tests, smoke boots on both architectures, UB-checked builds, subsystem gates, fuzzers."
order: 8
updated: 2026-10-22
---

nos has no test runner and no separate test binary. Instead, a change goes through layers of checks, called gates. From the most often run to the least:

- the self-tests that every boot runs;
- the smoke boots that CI runs on both architectures;
- builds with undefined-behaviour checks;
- a gate for each subsystem, for what a smoke boot cannot notice;
- four fuzzers that run on the host.

By the end of this guide you will know which of them to run for a change and how to read what they report.

You need:

- the nos tree, and Docker or the native toolchain ([Build nos](/howto/build));
- QEMU for both architectures (`qemu-system-x86_64` and `qemu-system-aarch64`), and Python 3;
- for the fuzzers: `cargo` on the host for `hv-fuzz.py`, `net-fuzz.py` and `fs-fuzz.py`, and `clang` with its sanitizers plus `make` for `cpp-fuzz.py`.

[Tests and gates](/docs/testing) is the long version of this page. It explains what every gate checks and which failure it is there to catch.

## The one rule: judge by the exit code

Every check here passes or fails by its **exit code**, never by what it prints. Grepping the output is exactly how a failure gets mistaken for success. For example, a build that stops on a stale dependency file prints `No rule to make target`, not `error`. And a boot log that was never written has no `PANIC:` in it either.

```sh
./scripts/smoke-test.sh --skip-build
echo "exit: $?"
```

If you pipe a check's output, for example through `tail`, the pipeline's exit status is that of the last command, unless you run `set -o pipefail` first.

## 1. Self-tests at every boot

Every boot runs the kernel's self-tests before going any further. They live in [`src/cpp/kernel/test.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/test.cpp), and each one is a `Stdlib::Error TestXxx()` function:

- **When they run.** `Test::Test()` runs early in boot, called from `Main2` in `main.cpp` and from `main_arm64.cpp`. `Test::TestMultiTasking()` runs later. A failing test returns a non-success `Stdlib::Error`.
- **The Rust side** runs from the same place, through `rust_test`. Its tests include the frame test, which writes a page through each CPU's slot in the temporary mapping window, and a test of the lock-free ring.
- **`TestModules`** loads the `modtest` module embedded in the kernel image, then feeds the module loader damaged copies of it.
- **`fstest=on`** on the kernel command line runs the filesystem self-test on `/` once it is mounted, and prints `fstest: passed` or `fstest: FAILED`.

To add a test, write a `TestXxx()` and register it in the `Test()` dispatcher. To run a single test, edit `Test()` so that it calls only that function, then rebuild and boot. `./scripts/qemu.sh` writes the serial console to `nos.log`, so that is the file to watch.

## 2. Smoke boots on both architectures

A smoke boot starts the kernel in QEMU and checks that it comes all the way up. **Every change must keep both smoke boots green.** CI runs them on every push to `master` and on every pull request. A change to common code that builds only on x86 is a broken change.

```sh
./scripts/smoke-test.sh
./scripts/smoke-arm64.sh
SMOKE_HVF=1 ./scripts/smoke-arm64.sh
```

What the scripts do:

- **They build first**, incrementally, in the `nos-builder` Docker image: `make rust && make nocheck` for x86-64 and `make nocheck ARCH=aarch64` for arm64. With `--skip-build` they boot what is already built. `make smoke` runs the x86-64 script.
- **They boot without a display.** The x86-64 boot runs under TCG, QEMU's software CPU emulation, with virtio-blk (modern), virtio-scsi (legacy), NVMe, virtio-net and two virtio-rng devices attached. This covers the Rust drivers and the MSI-X path, along with the C++ beneath them. The arm64 boot runs QEMU `virt` with virtio-mmio block, net and rng devices. It also uses TCG, so that it runs anywhere. On Apple Silicon, `SMOKE_HVF=1` uses HVF (Apple's hardware virtualization) instead, which is about four times as fast. Both boot an ext2 root with 1 KiB blocks and `fstest=on`.
- **They wait for markers**, lines the script expects in the serial log. The boot passes only when every marker has appeared. On x86-64 the markers are `After test`, `Preempt is now on`, `MountRootFs: mounted ext2 on /`, `fstest: passed`, `EntropySource registered: rng0` and `rng1`, and `boot: complete`. On arm64 they are `Self test passed`, `Cpus started`, `After test`, `MountRootFs: mounted ext2 on /`, `fstest: passed`, `EntropySource registered: rng0` and `boot: complete`.
- **They fail fast.** The run stops at the first sign of trouble: `PANIC:`, `fstest: FAILED`, an entropy source that registered but gave no data, QEMU exiting early, or a timeout. It then prints `SMOKE FAIL: <reason>` (or `SMOKE-ARM64 FAIL:`) and the last 30 lines of the log. On success it prints `smoke: OK (<seconds>s)` or `smoke-arm64: OK (<seconds>s)`.

`SMOKE_TIMEOUT` sets how many seconds to wait (300 by default), and `SMOKE_LOG` sets where the serial log goes.

## 3. Builds with undefined-behaviour checks

You can build the same kernel so that both halves, the C++ and the Rust, check themselves. The first undefined operation then becomes a panic that names its location, and every gate also checks for undefined behaviour (UB) in the code it exercises:

```sh
make nocheck UBSAN=1
make nocheck RUSTUB=1
make nocheck ARCH=aarch64 UBSAN=1 RUSTUB=1
```

- **`UBSAN=1`** builds the C++ with clang's undefined-behaviour sanitizer. It catches shifts past the width of the type, signed overflow, out-of-bounds indexes into arrays whose size the compiler knows, null or misaligned accesses, invalid `bool` and `enum` values, and falling off the end of a function. Its object files are kept apart, in `out/$(ARCH)-ubsan/`. A report looks like `PANIC:Report():ubsan.cpp,267: UBSAN: shift exponent 64 is too large for 64-bit type 'unsigned long' at src/cpp/...`. Boot with `ubsan=warn` to collect every report instead of stopping at the first.
- **`RUSTUB=1`** turns on overflow checks and `core`'s checks of its own unsafe preconditions (`ptr::read` of an unaligned or null pointer, a bad `slice::from_raw_parts`, `get_unchecked` past the end and others), in the kernel's Rust and in every module. A report starts with `RUST PANIC: panicked at` and names the precondition. It does not analyse your own `unsafe` code: out-of-bounds raw pointers, aliasing and data races go unchecked.
- **The two are independent**, and you can set both at once. `version` then says `+ubsan` and `+rustub`.

Run the gates on these images as they are: pass `--skip-build` to the scripts that build on their own, or the smoke scripts' build step will make a plain kernel again. A gate that loads modules takes them from whichever flavour was built. As its last step, CI boots a `UBSAN=1 RUSTUB=1` build on both architectures.

## 4. The gate for what you touched

What a smoke boot cannot notice has a test of its own in `scripts/`. Each one exists for a failure that looks like success, and most were written the day after we ran into one. They all expect the kernel to be built already. Each takes `--keep` to leave its scratch directory behind, and exits with 0 only if every check passed. Several run on arm64 only: they drive the shell over UDP, and the arm64 boot is the one whose command line turns that shell on. The code they test is the same on both architectures.

| After changing | Run | Where |
|---|---|---|
| code that maps memory | `wx-test.sh [--arch x86_64\|aarch64]` | both |
| the panic path, `panic=N`, a reset | `panic-test.sh [--arch x86_64\|aarch64]` | both |
| the block layer, the partition tables | `parttest.py [--arch aarch64\|x86_64]` | both, in CI |
| the block layer's asynchronous path, or the C ABI that modules use to reach it | `netblk-test.py [--arch x86_64\|aarch64]` | both |
| ext2 | `ext2-test.py` (`e2fsck` judges the image) | arm64, in CI |
| nanofs | `nanofs-test.py` | arm64, in CI |
| the disk log | `disklog-test.py` | arm64 |
| the trace path, a net device, netconsole | `netconsole-test.py` | arm64, in CI |
| TCP, the HTTP client | `tcp-test.py` | arm64, in CI |
| TCP's listening side, the module loader, the `ffi` declarations, `ssh`, `modules/sshd` | `sshd-test.py [--arch aarch64\|x86_64]` | both |
| the receive path, the frame pool, `modules/netload` | `netload-test.py [--arch aarch64\|x86_64]` (about seven minutes on arm64, by design) | both; arm64 in CI |
| `drivers/igb`, the interface between NIC drivers and the network layer | `tcp-test.py --nic igb`, `netload-test.py --nic igb` (QEMU 8.0 or later) | by hand |
| `drivers/usb/` | `usb-test.py` | x86-64 |
| the hypervisor: `hv`, `hvarch`, `modules/hv` | `hv-test.py [--arch x86_64\|aarch64]` | both |
| the hypervisor's MMIO decoder and guest page walker | `insn-test.py` | host, in CI |
| anything a guest reaches in `hv`: devices, APICs, MMIO, the loader, ACPI, the CPUID and MSR policy, the run loop, the guests' switch and NAT | `hv-fuzz.py` | host, in CI |
| a Linux guest: the loader, the policy, the devices, more than one CPU, ACPI, the network | `hv-linux-test.py --bzimage <bzImage> --initrd <cpio>`, with `--disk`, `--net`, `--cpus N`, `--acpi`, `--attach` as needed | x86-64, by hand |
| what a distribution's kernel runs into | `hv-distro-test.py --iso <alpine-virt.iso> [--debian <raw>]` | x86-64, by hand |
| anything the network hands the kernel: the stack, DHCP, DNS, HTTP and TLS, the UDP shell, netconsole, `ssh`, `sshd` | `net-fuzz.py` | host, in CI |
| anything a disk hands the kernel: partition tables, ext2, nanofs, the VFS, `root=` | `fs-fuzz.py` | host, in CI |
| what firmware, a bootloader or a disk hands the C++, and the page tables and heap under everything | `cpp-fuzz.py` | host, in CI |
| the system image, `nos-install`, `root=UUID=` | `image-test.py` | x86-64, in CI |
| a wait primitive, the scheduler's choice of the idle task | `idle-wait-test.py` (needs 12 or more CPUs and KVM) | x86-64 |
| `drivers/r8168`, `drivers/r8125` | nothing emulates them, so only the compiler and then the real machine | — |

This table is a summary. The authoritative version, with the exact files each row covers, is in the project's [`CLAUDE.md`](https://github.com/irqlevel/nos/blob/master/CLAUDE.md#gates), and [Tests and gates](/docs/testing) explains every gate. The hypervisor gates that boot a real Linux need a guest kernel and an initramfs; [Boot a Linux guest](/howto/linux-guest) shows how to build them.

## 5. The host fuzzers

Four programs compile the kernel's own sources for the host and feed them random inputs. Each one holds the code to a model of what it should do. A panic, a broken invariant, a broken locking rule, a spin and a hang all count as findings, and each finding is reported with the seed and iteration that reproduce it.

| Script | What it fuzzes | Needs |
|---|---|---|
| `hv-fuzz.py` | everything a guest reaches in the hypervisor: devices, local APIC and IO-APIC, MMIO, the Linux loader and ACPI tables, the CPUID and MSR policy, the run loop on every CPU of a guest, the guests' switch, DHCP and NAT, and where guests are placed | `cargo` |
| `net-fuzz.py` | everything the network hands the kernel: frames of every kind, TCP, HTTP and HTTPS, DNS, DHCP, ICMP and ARP, the UDP shell, netconsole, SSH | `cargo` |
| `fs-fuzz.py` | everything a disk hands the kernel: MBR and GPT, the disk log, sound and damaged ext2 and nanofs images, the VFS, the storage commands, `root=` | `cargo` |
| `cpp-fuzz.py` | the C++ under ASan and UBSan: the device tree, Multiboot2 tags, ACPI tables, the memory map, the command line, the GRUB environment block, the module loader, `printf`, the page tables and the heap | `clang` with sanitizers, `make` |

With no arguments, a fuzzer works as a **gate**: it runs fixed inputs from a fixed seed, the same on every run. That takes from well under a minute (`hv-fuzz.py`) to a couple of minutes (`net-fuzz.py`, `fs-fuzz.py`). In a **campaign**, each target gets a number of seconds and a seed of its own:

```sh
python3 scripts/hv-fuzz.py
python3 scripts/net-fuzz.py --seed 7 --seconds 300
python3 scripts/fs-fuzz.py --target ext2 --target nanofs
python3 scripts/net-fuzz.py --replay tcp out/net-fuzz/findings/X.hex --trace
```

- **Findings** are saved under `out/<fuzzer>/findings/`, and `--replay` runs one again. `--trace` works with `net-fuzz.py`, `fs-fuzz.py` and `cpp-fuzz.py`.
- **Exit codes:** 0 means no finding, 1 means a finding, and 3 means a hang.
- **Statistics:** `HV_FUZZ_STATS=1`, `NET_FUZZ_STATS=1`, `FS_FUZZ_STATS=1` and `CPP_FUZZ_STATS=1` make the matching fuzzer print its statistics: its slowest inputs and how often it reached the states it counts.
- **New code:** when you add a parser of untrusted input, a new device or a new protocol, give its fuzzer a new target.

## What CI runs

[`.github/workflows/ci.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/ci.yml) runs on every push to `master` and on every pull request, with one job per architecture:

- **Both jobs** run `make check` (cppcheck, which fails on any finding), `make nocheck ARCH=<arch>`, the smoke boot and `parttest.py --arch <arch> --tcg`, and last a `UBSAN=1 RUSTUB=1` build with its own smoke boot.
- **The x86-64 job** adds `make image` with `image-test.py`, `insn-test.py`, `hv-fuzz.py` and `fs-fuzz.py`.
- **The aarch64 job** adds `net-fuzz.py`, `cpp-fuzz.py`, and `ext2-test.py`, `nanofs-test.py`, `netconsole-test.py`, `tcp-test.py` and `netload-test.py`, each with `--tcg`.

You run every other gate in the table yourself, whenever your change reaches what it covers. That includes the hypervisor's Linux and distribution gates (a `bzImage` is megabytes in size, and the distribution images have to be downloaded), `hv-test.py`, `idle-wait-test.py` and the hardware NIC gates.

## Before you call a change done

1. Run `make check`, and build for both architectures: `make nocheck` and `make nocheck ARCH=aarch64`.
2. Run both smoke boots.
3. Run the gate, or gates, for what you touched.
4. Run the same smoke boots and gates on a `UBSAN=1 RUSTUB=1` build, with `--skip-build`.
5. If the code reads anything from outside (a packet, a disk, firmware, a guest), run its fuzzer.
6. For C++, read the diff a second time, looking for undefined behaviour and nothing else: values from outside used as an index, a length or a shift count before they are checked, overflow, uninitialised reads, lifetimes, shared state without a lock. The sanitizer catches only what the gates exercise.
7. For Rust, run `scripts/unsafe-count.py` on the crates you changed. If the count went up, you need a reason.

If you hit a failure that looked like success along the way, write the gate that would have caught it.

## Troubleshooting

**`No rule to make target ...` and nothing else.** After you move, rename or delete a header, the `.d` files in `out/` still name the old path. Run a clean build, or delete those `.d` files.

**A smoke boot times out under TCG on a slow machine.** Raise `SMOKE_TIMEOUT`. On Apple Silicon, `SMOKE_HVF=1` makes the arm64 boot about four times as fast.

**`hv-test.py` or `hv-linux-test.py --cpus` refuses to run under TCG.** They need QEMU 9.2 or later. In that version, TCG sends the memory accesses of a guest with paging off through the nested page table.

**A time-bound check of `hv-linux-test.py --cpus 2` fails on a busy machine.** The restarting guest's six resets within a minute and the log flood within `--vm-secs` are checks of this kind. Before you blame your change, run the gate again on a quiet machine.

**A build suddenly wants the network.** Builds run offline: the Rust dependencies are vendored (their sources are in the tree), and the toolchain is pinned in `src/rust/rust-toolchain.toml`. So there are two possible causes. If a dependency was never vendored, run `scripts/vendor.sh` and commit what it produces. If the `nos-builder` image is older than the pin, rebuild it with `docker build --platform linux/amd64 -t nos-builder .`.

## Next steps

- [Tests and gates](/docs/testing): what every gate does and which failure it is there to catch.
- [Build nos](/howto/build) and [Run nos in QEMU](/howto/run-in-qemu).
- [Write a loadable module](/howto/write-a-module), and give it a gate.
- [Boot a Linux guest](/howto/linux-guest): do by hand what the hypervisor gates do.
