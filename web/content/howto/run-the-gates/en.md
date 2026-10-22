---
title: "Run the gates: test a change the way nos does"
description: "Boot self-tests, smoke boots on both architectures, UB-checked builds, per-subsystem gates and host fuzzers: what to run after a change, and how to judge it."
order: 8
updated: 2026-10-22
---

nos has no test runner and no separate test binary. A change is checked by a stack of things, from the most often run to the least: the self-tests every boot runs, the smoke boots CI runs on both architectures, builds with undefined-behaviour checks, a gate per subsystem for what a smoke boot cannot notice, and four fuzzers that run on the host. By the end of this guide you will know which of them to run for a change and how to read what they say.

You need:

- the nos tree, and Docker or the native toolchain ([Build nos](/howto/build));
- QEMU for both architectures (`qemu-system-x86_64` and `qemu-system-aarch64`), and Python 3;
- for the fuzzers: `cargo` on the host for `hv-fuzz.py`, `net-fuzz.py` and `fs-fuzz.py`, and `clang` with its sanitizers plus `make` for `cpp-fuzz.py`.

The long form of this page, with what every gate checks and the failure it is there for, is [Tests and gates](/docs/testing).

## The one rule: judge by the exit code

Every check here passes or fails by its **exit code**, never by what it printed. Grepping the output is exactly how a failure gets read as success. A build that stops on a stale dependency file says `No rule to make target`, not `error`. A boot log that was never written contains no `PANIC:` either.

```sh
./scripts/smoke-test.sh --skip-build
echo "exit: $?"
```

If you pipe a check's output, through `tail` for example, the pipeline's status is the last command's unless you `set -o pipefail` first.

## 1. Self-tests at every boot

Each boot runs the kernel's self-tests before it goes on. They live in [`src/cpp/kernel/test.cpp`](https://github.com/irqlevel/nos/blob/master/src/cpp/kernel/test.cpp), where each one is a `Stdlib::Error TestXxx()` function:

- **When they run.** `Test::Test()` runs early in boot, called from `Main2` in `main.cpp` and from `main_arm64.cpp`; `Test::TestMultiTasking()` runs later. A failing test returns a non-success `Stdlib::Error`.
- **The Rust side** runs from the same place, through `rust_test`. Among its tests are the frame test, which writes a page through each CPU's slot of the temporary mapping window, and a test of the lock-free ring.
- **`TestModules`** loads the `modtest` module embedded in the kernel image, then feeds the module loader damaged copies of it.
- **`fstest=on`** on the kernel command line runs the filesystem self-test on `/` once it is mounted, and prints `fstest: passed` or `fstest: FAILED`.

To add a test, write a `TestXxx()` and register it in the `Test()` dispatcher. To run a single test, edit `Test()` to call only that function, rebuild, boot (`./scripts/qemu.sh` writes the serial console to `nos.log`), and watch `nos.log`.

## 2. Smoke boots on both architectures

**Every change must keep both smoke boots green.** CI runs them on every push to `master` and on every pull request, and a change to common code that only builds on x86 is a broken change.

```sh
./scripts/smoke-test.sh
./scripts/smoke-arm64.sh
SMOKE_HVF=1 ./scripts/smoke-arm64.sh
```

What the scripts do:

- **They build first.** Each builds in the `nos-builder` Docker image, incrementally: `make rust && make nocheck` for x86-64, `make nocheck ARCH=aarch64` for arm64. With `--skip-build` they boot what is already built. `make smoke` runs the x86-64 one.
- **They boot headless.** x86-64 boots under TCG with virtio-blk (modern), virtio-scsi (legacy), NVMe, virtio-net and two virtio-rng devices attached, so the Rust drivers and the MSI-X path are covered along with the C++ under them. arm64 boots QEMU `virt` with virtio-mmio block, net and rng, under TCG so that it runs anywhere; `SMOKE_HVF=1` uses HVF on Apple Silicon, about four times as fast. Both boot a 1 KiB-block ext2 root with `fstest=on`.
- **They wait for markers.** Success is every marker present in the serial log. On x86-64 those are `After test`, `Preempt is now on`, `MountRootFs: mounted ext2 on /`, `fstest: passed`, `EntropySource registered: rng0` and `rng1`, and `boot: complete`. On arm64 they are `Self test passed`, `Cpus started`, `After test`, `MountRootFs: mounted ext2 on /`, `fstest: passed`, `EntropySource registered: rng0` and `boot: complete`.
- **They fail fast.** `PANIC:`, `fstest: FAILED`, an entropy source that registered and gave nothing, QEMU exiting early, or a timeout ends the run with `SMOKE FAIL: <reason>` (or `SMOKE-ARM64 FAIL:`) and the last 30 lines of the log. Success prints `smoke: OK (<seconds>s)`, or `smoke-arm64: OK (<seconds>s)`.

`SMOKE_TIMEOUT` sets the seconds to wait (300 by default), and `SMOKE_LOG` where the serial log goes.

## 3. Builds with undefined-behaviour checks

The same kernel can be built with both halves checking themselves. The first undefined operation then becomes a panic that names its site, and every gate becomes a UB check over the code it drives:

```sh
make nocheck UBSAN=1
make nocheck RUSTUB=1
make nocheck ARCH=aarch64 UBSAN=1 RUSTUB=1
```

- **`UBSAN=1`** builds the C++ with clang's undefined-behaviour sanitizer: shifts past the width, signed overflow, out-of-bounds indexes the compiler knows of, null or misaligned accesses, invalid `bool` and `enum` values, falling off the end of a function. Its objects live apart, in `out/$(ARCH)-ubsan/`. A report looks like `PANIC:Report():ubsan.cpp,267: UBSAN: shift exponent 64 is too large for 64-bit type 'unsigned long' at src/cpp/...`. Boot with `ubsan=warn` to collect every report instead of stopping at the first.
- **`RUSTUB=1`** turns on `core`'s checks of its own unsafe preconditions (`ptr::read` of an unaligned or null pointer, a bad `slice::from_raw_parts`, `get_unchecked` past the end and others), plus overflow checks, in the kernel's Rust and in every module. A report starts with `RUST PANIC: panicked at` and names the precondition. It does not analyse your own `unsafe`: out-of-bounds raw pointers, aliasing and data races are not checked.
- **The two are independent** and can be set together. `version` then says `+ubsan` and `+rustub`.

Run the gate on the images as they are, with `--skip-build` where the script builds: the smoke scripts' own build step would make a plain kernel again. A gate that loads modules takes them from the flavour that was built. CI boots a `UBSAN=1 RUSTUB=1` build on both architectures, as its last step.

## 4. The gate for what you touched

What a smoke boot cannot notice has a test of its own, in `scripts/`. Each is there for a failure that looks like success, and most were written the day after meeting one. They all want the kernel built first, take `--keep` to leave their scratch directory behind, and exit 0 only if every check passed. Several are arm64 only, because they drive the shell over UDP and the arm64 boot is the one whose command line enables it; the code they test is the same on both architectures.

| After touching | Run | Where |
|---|---|---|
| a mapping path | `wx-test.sh [--arch x86_64\|aarch64]` | both |
| the panic path, `panic=N`, a reset | `panic-test.sh [--arch x86_64\|aarch64]` | both |
| the block layer, the partition tables | `parttest.py [--arch aarch64\|x86_64]` | both, in CI |
| the block layer's asynchronous path, the C ABI modules reach it through | `netblk-test.py [--arch x86_64\|aarch64]` | both |
| ext2 | `ext2-test.py` (lets `e2fsck` judge the image) | arm64, in CI |
| nanofs | `nanofs-test.py` | arm64, in CI |
| the disk log | `disklog-test.py` | arm64 |
| the trace path, a net device, netconsole | `netconsole-test.py` | arm64, in CI |
| TCP, the HTTP client | `tcp-test.py` | arm64, in CI |
| TCP's listening side, the module loader, the `ffi` declarations, `ssh`, `modules/sshd` | `sshd-test.py [--arch aarch64\|x86_64]` | both |
| the receive path, the frame pool, `modules/netload` | `netload-test.py [--arch aarch64\|x86_64]` (about seven minutes on arm64, by design) | both; arm64 in CI |
| `drivers/igb`, the NIC driver seam | `tcp-test.py --nic igb`, `netload-test.py --nic igb` (QEMU 8.0 or later) | by hand |
| `drivers/usb/` | `usb-test.py` | x86-64 |
| the hypervisor: `hv`, `hvarch`, `modules/hv` | `hv-test.py [--arch x86_64\|aarch64]` | both |
| the hypervisor's MMIO decoder and guest page walker | `insn-test.py` | host, in CI |
| anything a guest reaches in `hv`: devices, APICs, MMIO, the loader, ACPI, the CPUID and MSR policy, the run loop, the guests' switch and NAT | `hv-fuzz.py` | host, in CI |
| a Linux guest: the loader, the policy, the devices, more than one CPU, ACPI, the network | `hv-linux-test.py --bzimage <bzImage> --initrd <cpio>`, with `--disk`, `--net`, `--cpus N`, `--acpi`, `--attach` as needed | x86-64, by hand |
| what a distribution's kernel meets | `hv-distro-test.py --iso <alpine-virt.iso> [--debian <raw>]` | x86-64, by hand |
| anything the network hands the kernel: the stack, DHCP, DNS, HTTP and TLS, the UDP shell, netconsole, `ssh`, `sshd` | `net-fuzz.py` | host, in CI |
| anything a disk hands the kernel: partition tables, ext2, nanofs, the VFS, `root=` | `fs-fuzz.py` | host, in CI |
| what firmware, a bootloader or a disk hands the C++, and the page tables and heap under everything | `cpp-fuzz.py` | host, in CI |
| the system image, `nos-install`, `root=UUID=` | `image-test.py` | x86-64, in CI |
| a wait primitive, the scheduler's choice of the idle task | `idle-wait-test.py` (needs 12 or more CPUs and KVM) | x86-64 |
| `drivers/r8168`, `drivers/r8125` | nothing emulates them: the compiler, then the real machine | — |

This table is a summary. The authoritative one, with the exact files each row covers, is in the project's [`CLAUDE.md`](https://github.com/irqlevel/nos/blob/master/CLAUDE.md#gates), and [Tests and gates](/docs/testing) explains every gate. The hypervisor gates that boot a real Linux need a guest kernel and initramfs, built as in [Boot a Linux guest](/howto/linux-guest).

## 5. The host fuzzers

Four programs build the kernel's own sources for the host and drive them with random inputs, each held to a model of what the code should do. A panic, a broken invariant, a broken lock rule, a spin and a hang are findings, each reported with the seed and iteration that reproduce it.

| Script | What it fuzzes | Needs |
|---|---|---|
| `hv-fuzz.py` | everything a guest reaches in the hypervisor: devices, local APIC and IO-APIC, MMIO, the Linux loader and ACPI tables, the CPUID and MSR policy, the run loop on every CPU of a guest, the guests' switch, DHCP and NAT, and where guests are placed | `cargo` |
| `net-fuzz.py` | everything the network hands the kernel: frames of every kind, TCP, HTTP and HTTPS, DNS, DHCP, ICMP and ARP, the UDP shell, netconsole, SSH | `cargo` |
| `fs-fuzz.py` | everything a disk hands the kernel: MBR and GPT, the disk log, sound and damaged ext2 and nanofs images, the VFS, the storage commands, `root=` | `cargo` |
| `cpp-fuzz.py` | the C++ under ASan and UBSan: the device tree, Multiboot2 tags, ACPI tables, the memory map, the command line, the GRUB environment block, the module loader, `printf`, the page tables and the heap | `clang` with sanitizers, `make` |

With no arguments a fuzzer is a **gate**: fixed inputs from a fixed seed, the same every run, from well under a minute (`hv-fuzz.py`) to a couple of minutes (`net-fuzz.py`, `fs-fuzz.py`). A **campaign** gives each target a number of seconds and a seed of its own:

```sh
python3 scripts/hv-fuzz.py
python3 scripts/net-fuzz.py --seed 7 --seconds 300
python3 scripts/fs-fuzz.py --target ext2 --target nanofs
python3 scripts/net-fuzz.py --replay tcp out/net-fuzz/findings/X.hex --trace
```

- **Findings** are saved under `out/<fuzzer>/findings/`, and `--replay` runs one again. `--trace` is available on `net-fuzz.py`, `fs-fuzz.py` and `cpp-fuzz.py`.
- **Exit codes:** 0 means no finding, 1 a finding, 3 a hang.
- **Statistics:** `HV_FUZZ_STATS=1`, `NET_FUZZ_STATS=1`, `FS_FUZZ_STATS=1` and `CPP_FUZZ_STATS=1` add each fuzzer's statistics: its slowest inputs and how often it reached the states it counts.
- **New code:** when you add a parser of untrusted input, a new device or a new protocol, give its fuzzer a new target.

## What CI runs

[`.github/workflows/ci.yml`](https://github.com/irqlevel/nos/blob/master/.github/workflows/ci.yml) runs on every push to `master` and every pull request, as one job per architecture:

- **Both legs:** `make check` (cppcheck, which fails on any finding), `make nocheck ARCH=<arch>`, the smoke boot, `parttest.py --arch <arch> --tcg`, and last a `UBSAN=1 RUSTUB=1` build with its own smoke boot.
- **The x86-64 leg** adds `make image` with `image-test.py`, `insn-test.py`, `hv-fuzz.py` and `fs-fuzz.py`.
- **The aarch64 leg** adds `net-fuzz.py`, `cpp-fuzz.py`, and `ext2-test.py`, `nanofs-test.py`, `netconsole-test.py`, `tcp-test.py` and `netload-test.py`, each with `--tcg`.

Every gate in the table that CI does not run, you run yourself when your change reaches it. That includes the hypervisor's Linux and distribution gates (a `bzImage` is megabytes, and the distribution images are downloads), `hv-test.py`, `idle-wait-test.py` and the hardware NIC gates.

## Before you call a change done

1. `make check`, and builds for both architectures: `make nocheck` and `make nocheck ARCH=aarch64`.
2. Both smoke boots.
3. The gate, or gates, for what you touched.
4. The same smoke boots and gate on a `UBSAN=1 RUSTUB=1` build, with `--skip-build`.
5. The fuzzer for the code, if the code reads anything from outside: a packet, a disk, firmware, a guest.
6. For C++, a second read of the diff for undefined behaviour and nothing else: values from outside used as an index, a length or a shift count before they are checked, overflow, uninitialised reads, lifetimes, shared state without a lock. The sanitizer only catches what the gates drive.
7. For Rust, `scripts/unsafe-count.py` on the crates you changed. A higher count needs a reason.

If you met a failure that looked like success along the way, write the gate that would have caught it.

## Troubleshooting

**`No rule to make target ...` and nothing else.** After moving, renaming or deleting a header, the `.d` files in `out/` still name the old path. Run a clean build, or delete those `.d` files.

**A smoke boot times out under TCG on a slow machine.** Raise `SMOKE_TIMEOUT`. On Apple Silicon, `SMOKE_HVF=1` makes the arm64 boot about four times as fast.

**`hv-test.py` or `hv-linux-test.py --cpus` refuses to run under TCG.** They need QEMU 9.2 or later, whose TCG puts the accesses of a guest with paging off through the nested page table.

**A check of `hv-linux-test.py --cpus 2` that is bound to time fails on a loaded machine.** The restarting guest's six resets in a minute and the log flood within `--vm-secs` are such checks. Run the gate again on a quiet machine before you read the failure as your change's.

**A build suddenly wants the network.** Builds are offline: the Rust dependencies are vendored and the toolchain is pinned in `src/rust/rust-toolchain.toml`. Either a dependency was never vendored, in which case run `scripts/vendor.sh` and commit what it produces, or the `nos-builder` image predates the pin, in which case rebuild it with `docker build --platform linux/amd64 -t nos-builder .`.

## Next steps

- [Tests and gates](/docs/testing): what every gate does and the failure it is there for.
- [Build nos](/howto/build) and [Run nos in QEMU](/howto/run-in-qemu).
- [Write a loadable module](/howto/write-a-module), and give it a gate.
- [Boot a Linux guest](/howto/linux-guest): what the hypervisor gates do, by hand.
