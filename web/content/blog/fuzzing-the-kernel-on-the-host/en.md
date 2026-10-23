---
title: "Fuzzing the kernel on the host"
description: "Four fuzzers build the kernel's own code into ordinary host programs: hypervisor, network, storage and C++. How they work and the bugs they found."
date: 2026-09-30
tags:
  - fuzzing
  - testing
  - safety
  - rust
  - hypervisor
  - networking
  - storage
---

Almost everything a kernel reads was written by someone else. A guest controls every byte of its virtio rings, the queues it shares with its virtual devices. The network controls every frame and every DHCP answer. A disk reports its own geometry, and nothing stops it from claiming 520-byte sectors, or 2^64 of them.

Firmware hands over the device tree, the ACPI tables and the memory map before a headless machine has any console at all. A panic on any of these paths stops the machine. On our two Hetzner servers, whose only console is the network, it stops without a word.

A test boot under QEMU only ever sees the inputs that QEMU happens to produce. So in the last two days of September 2026 we added four fuzzers. Each one runs the kernel's own code, not a copy or a model of it, as an ordinary program on the host: a developer's machine or a CI runner.

## Compiling the kernel's sources for the host

Most of the code that needed fuzzing was already portable. The block, network and filesystem layers are Rust crates built on `kcore` and `ffi`, the two crates through which Rust reaches the kernel's services ([how they got there](/blog/cpp-to-rust-layers) is another story). Cargo builds them for the host as easily as for the kernel. [`fuzz/fs`](https://github.com/irqlevel/nos/blob/master/fuzz/fs/Cargo.toml) depends on them by path and uses the build profile of a `RUSTUB=1` kernel. So an overflow on the host is the same panic it would be in that kernel, not a silent wrap-around:

```toml
[dependencies]
# The kernel's crates, built for the host as they are for the kernel.
ffi = { path = "../../src/rust/ffi" }
kcore = { path = "../../src/rust/kcore" }
block = { path = "../../src/rust/block" }
fs = { path = "../../src/rust/fs" }

# What a RUSTUB=1 kernel has on, in every crate: an overflow is a panic, not
# a wrap, and core's checks of its own unsafe preconditions are made.
[profile.release]
overflow-checks = true
debug-assertions = true
panic = "unwind"
```

The hypervisor takes more work. Its files import `hvarch`, the crate that drives the CPU's virtualization extension and holds all of the hypervisor's `unsafe` code. No host program can run that code, so [`fuzz/hv`](https://github.com/irqlevel/nos/blob/master/fuzz/hv/src/main.rs) compiles the hypervisor's files one by one as modules of its own, and itself answers to the crate names `ffi`, `hv`, `hvarch` and `kcore`. Everything those files reach outside themselves, from the clock to the CPU, is a stand-in in the same program:

```rust
extern crate alloc;
extern crate self as ffi;
extern crate self as hv;
extern crate self as hvarch;
extern crate self as kcore;
// ...
#[path = "../../../src/rust/hv/src/acpi.rs"]
pub mod acpi;
// ...
/// The guests' DHCP server, from the module: what a guest's DHCP client's
/// frames reach.
#[path = "../../../src/rust/modules/hv/src/dhcp.rs"]
pub mod dhcp;
```

The C++ gets the same treatment by other means. [`fuzz/cpp/Makefile`](https://github.com/irqlevel/nos/blob/master/fuzz/cpp/Makefile) compiles the kernel's files unchanged, under ASan and UBSan (the address and undefined-behaviour sanitizers). A host version of the HAL, the kernel's hardware abstraction layer, comes first on the include path and takes the place of each architecture's inline one. It uses no privileged instructions, and it makes the MMU's hooks visible to a fuzz target that models the MMU. All four fuzzers live in `fuzz/` rather than under `src/rust/`, because the `.cargo/config.toml` there turns any cargo build beneath it into a kernel build.

## A machine to run them on

The network and storage layers expect a kernel underneath them. [`fuzz/common`](https://github.com/irqlevel/nos/blob/master/fuzz/common/machine/sched.rs), shared by the net and fs fuzzers, is a kernel written just for this purpose. Every task is a host thread, but only one task runs at a time. Wherever a task blocks or releases a lock, the input decides which task runs next.

Nothing sleeps on a real clock. When every task is waiting, the clock jumps to the next deadline, so an hour of a DHCP lease costs only as much as its timers. And the kernel's rules are checked on the very line that breaks them:

```rust
pub fn check_may_wait(what: &str) {
    match ctx() {
        Ctx::Softirq => panic!("invariant: a soft IRQ handler {}: every packet waits for it -- the receive path \
                                must never sleep", what),
        Ctx::Irq => panic!("invariant: an interrupt handler {}", what),
        Ctx::Task => {}
    }
    if irq_off() != 0 || preempt_off() != 0 {
        panic!("invariant: task {} {} with a spin lock held or interrupts off (irq {}, preempt {})", me(), what,
               irq_off(), preempt_off());
    }
}
```

The same goes for two locks taken in opposite orders, an allocation with interrupts off and a task that ends while holding a lock. Each one is reported as a finding, the fuzzers' word for a caught bug, right where it happens. Every input runs in a process of its own, forked from one machine that has already booted, so one input's static state never leaks into the next.

None of this is coverage-guided: inputs are not steered by which code earlier inputs reached. An input is random bytes generated from a seed. A generator that knows the format reads those bytes as a stream of choices ([`input.rs`](https://github.com/irqlevel/nos/blob/master/fuzz/common/input.rs)). Every read past the end of the input returns zero, and zero is always the ordinary choice. Anything that ends a run early has to be rare, or the runs never get deep:

```rust
/// True about once in 4096: for what ends a run, which has to be much
/// rarer than a run is long for the rest to be reached.
pub fn rare(&mut self) -> bool {
    self.u16() >= 0xFFF0
}
```

## Four fuzzers

| Fuzzer | Built from | Targets | Checked against |
|---|---|---|---|
| `fuzz/hv` | the hypervisor's devices, local APIC and IO-APIC, MMIO decoder, page walker, Linux loader, ACPI tables, CPUID/MSR policy and run loop; the guests' switch, DHCP server and NAT | 22 | invariants on every device; models of the switch, NAT, the policy and guest placement |
| `fuzz/net` | the `net`, `netwire`, `tls`, `fs` and `ssh` crates, and the sshd module's source | 10 | a model of the outside world, one protocol at a time |
| `fuzz/fs` | the `block` and `fs` crates | 9 | a model of the file tree, its own e2fsck, power cuts |
| `fuzz/cpp` | the C++ that reads what firmware, the bootloader or a disk hands it; the module loader; printf; the page tables and the heap | 11 | a model of what each header promises, under ASan and UBSan |

The most ambitious hv target, `platform`, builds a Linux guest's machine the way `hv boot` builds it. Then it runs that machine on the real run loop, `LinuxGuest::run`, one vCPU thread at a time. Wherever the loop would enter the guest, a script decides what the guest does next: an I/O port access, an MSR access, a fault on a device's page together with the instruction behind it, or a halt.

Over many exits (an exit is the moment the CPU leaves the guest and hands control back to the hypervisor), the script does what drivers do, from filling MSI-X tables to starting the secondary CPUs (APs) with INIT and SIPI. Two things count as findings here: a vCPU entering its guest with a lock held, and the run loop reading the clock a million times without either entering the guest or waiting.

`fuzz/net` surrounds the machine with a simulated world ([`src/world`](https://github.com/irqlevel/nos/blob/master/fuzz/net/src/world/check.rs)). It has a link that delays, reorders, duplicates and drops packets, and a TCP peer that keeps every byte of both streams. It also has a rustls server with the fuzzer's own certificate authority, DHCP and DNS servers that lie, and a full SSH client.

`fuzz/fs` gives the storage layers disks with a volatile write cache, a power switch and requests that fail ([`disk.rs`](https://github.com/irqlevel/nos/blob/master/fuzz/fs/src/machine/disk.rs)). It judges the resulting images with an e2fsck of its own, which was checked against e2fsprogs on hundreds of images.

## Gates and campaigns

Run with no arguments, each fuzzer feeds in a fixed number of inputs starting from seed 1, the same inputs every time. That makes it a gate, a check that every change has to pass. CI runs all four on every push: hv and fs in its x86-64 job, net and C++ in the arm64 one. Each takes from half a minute to a couple of minutes. A campaign, a longer hunt for new bugs, is the same program with a seed of its own:

```sh
python3 scripts/hv-fuzz.py                          # the gate
python3 scripts/hv-fuzz.py --seed 7 --seconds 300   # five minutes a target
python3 scripts/hv-fuzz.py --replay platform out/hv-fuzz/findings/X.hex
```

Every finding comes with the seed and iteration that reproduce it, and the input is saved for `--replay`. The exit code is 0 when nothing was found, 1 for a finding and 3 for a hang. We measure how deep the inputs go instead of hoping. `HV_FUZZ_STATS=1` reports how the whole-machine runs ended, and on its first day the hv gate covered 94% of the code regions in the sources it compiles, by clang's coverage.

## What they found

Each fuzzer was merged together with the fixes for what it found. Here is a selection.

### One guest posing as another

The `guestnet` target wires the switch and NAT together the way the hv module does. On its very first input, it found that a guest could send traffic as another guest. The switch passed on frames from any MAC and any address. NAT sends a flow's replies to the MAC that the flow's last packet came from. So a single packet with another guest's address and the ports of one of its flows took that flow over, and the outside world's replies went to the impostor.

The fix ([d1aeecf](https://github.com/irqlevel/nos/commit/d1aeecf09004bba80ed5301eea6b25d60f1dc1d6)) is what a cloud's virtual switch does. [`ingress`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hv/src/net.rs) checks every frame from a guest as it comes in. Only IPv4 and ARP get through, and only from the port's own MAC and address. The one exception is a DHCP client that has no address yet, and the switch answers that one itself. Everything else is dropped and counted against the guest in `hv list`.

One control run, with the check taken out, showed the actual harm: a connection through `hv forward` (which forwards a port of nos to a guest) to one guest's web server landed on the other guest. Now `hv-linux-test.py --net` has a real Linux guest claim its neighbour's address and checks that nothing comes back.

### An address past the top of memory

A guest could put a virtio buffer at the very top of its address space. When the emulated disk then wrote the request's status, it computed an address past 2^64. Under `RUSTUB=1` that was a panic, and in a plain kernel the address silently wrapped around to page 0. The fix is a type rather than a check at every call site. A segment's address is private, and the only way to reach it does the addition with an overflow check ([`virtio.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/virtio.rs)):

```rust
pub struct Seg {
    addr: u64,
    pub len: u32,
    pub write: bool,
}

impl Seg {
    pub fn at(&self, offset: u64) -> Option<u64> {
        if offset < u64::from(self.len) { self.addr.checked_add(offset) } else { None }
    }
}
```

The same first day ([4ce4e77](https://github.com/irqlevel/nos/commit/4ce4e771b0f1f0d8cc2a662e0e10d1e913571b56)) turned up two more bugs. A halted vCPU kept its host CPU spinning: the guest's interrupt priorities held back a periodic APIC timer's interrupt, and the timer reported a deadline that had already passed. And the Linux loader accepted a kernel's preferred address below 1 MiB, on top of the zero page and page tables that the loader itself writes there. After the fixes, campaigns on ten seeds ran 7.9 billion inputs on the final harness (the fuzzer code around the kernel's sources) and found nothing more.

### Sleeping with a spin lock held

Netconsole sends the kernel's log out over the network. Its drain task, which empties the log onto the wire, polls until the network device has an address ([c68452d](https://github.com/irqlevel/nos/commit/c68452d5b7ab1ce09024cce09e050bcc4271090f)):

```diff
-            let nic = match self.log.lock().nic {
+            let attached = self.log.lock().nic;
+            let nic = match attached {
                 Some(nic) if nic.ip() != 0 => nic,
                 _ => {
                     kcore::task::sleep_ms(NO_LINK_POLL_MS);
                     continue;
```

The old line reads like "copy a field, then drop the lock", but that is not what it does. A temporary in the expression a `match` examines lives until the end of the `match`. So the log's `IrqSpinLock` stayed held through the whole 200 ms sleep, with interrupts off, and any CPU that traced a line in the meantime spun on it. Sleeping with a spin lock held is exactly what the machine's rules turn into a finding.

### Failures that looked like success

The net fuzzer's world knows what it sent, so it can tell a truncated answer from a whole one. That is how it found a family of bugs in which the kernel reported success. An HTTP response cut off in the middle of its headers came back as "ok", with a body made of its own status line.

The TLS client took two things for the end of the stream: a stream cut off in the middle of a record, and a receive that timed out. So a stalled body with no length came back as if it were whole. The DHCP client, once it had renewed, started over from DISCOVER while still bound, and never renewed its lease again. Now it keeps a lease the way RFC 2131 says.

### A power cut between two writes

ext2's `create_file` wrote the new inode before the bitmap bit that marks the inode as taken ([07e7409](https://github.com/irqlevel/nos/commit/07e7409436ebde4f340b209d4f7a8b0288e2f5d3)):

```diff
-        if !self.write_inode(ino, &inode) || !self.flush_bitmap() || !self.commit_meta() {
+        if !self.flush_bitmap() || !self.write_inode(ino, &inode) || !self.commit_meta() {
```

If the power fails between the two writes, the inode is in use but the bitmap calls it free, and the next create will take it. The fuzzer's disk simulates exactly that kind of power cut.

The fuzzer found two more bugs elsewhere in storage. A directory whose load failed partway read its entries again every time it was looked at, so each `ls` grew the tree for good. And the disk log, which saves the kernel log to disk for machines that die without a word, overran its buffer by a full batch on a device with 520-byte sectors. With `disklog=on` that was a panic at boot.

### Index −1 in the device tree reader

The device tree, the firmware's description of the hardware, is the first thing the arm64 boot reads. In its reader, an `END_NODE` with no node open took the walk's depth below the root. The next node then wrote its cell counts (`#address-cells` and `#size-cells`) at index −1 of the walker's stacks, which is a write into `.bss` ([00631ab](https://github.com/irqlevel/nos/commit/00631ab0b9cced3c6cb53c7ef623e04da0e24fa5)). Properties are now read through `Fdt::Prop`, and every read is bounded by the property's length.

The same round found the heap's pools allocating a page with their spin lock held. That is the deadlock described in [no allocation under a spin lock](/blog/no-allocation-under-spinlock).

### The wrong IO-APIC on the AX41

The `acpi` target ([516dc26](https://github.com/irqlevel/nos/commit/516dc2680ab97acaa11370cec0e722b8aa590d3e)) found two problems. First, every table the ACPI root table listed stayed mapped in the TmpMap window for good. That window is the small set of slots through which the kernel reaches arbitrary physical pages, and on the AX41 the tables took 21 of its 448 slots. Second, when two IO-APICs were listed, the kernel kept the last one.

The AX41's MADT lists the FCH's IO-APIC (the southbridge's, at GSI 0) first and the GNB's (the northbridge's) second, so the kernel had been driving the GNB's. The legacy IRQs, the HPET's tick among them, were set up on pins that none of those lines reach. Nobody had noticed, because the tick matters only until each CPU runs its own local APIC timer. With the fix booted on the AX41, `irqstat` counts the tick at its proper 100 Hz.

There was a bug in printf as well. `VsnPrintf` read every integer conversion as a 64-bit slot, so the `int` trace level of every `Trace` call arrived with whatever the upper half of its register held ([3743fcf](https://github.com/irqlevel/nos/commit/3743fcf43965be6df80229dd859cd48db5080a8b)). The formats follow C's rules now, the compiler checks them through `PRINTF_FORMAT` and `-Wformat=2`, and a `format` target checks `VsnPrintf` against the host's `vsnprintf`, byte for byte.

## Proving the harnesses can catch bugs

A fuzzer that finds nothing may be good news, or it may be a harness that cannot see. So we also tested each one the other way round: break the code on purpose, and the target must fail. For the three parsers the x86 boot depends on, we took every check out one at a time, 26 in all, and each removal became a finding within 20,000 inputs.

Three were missed at first. In two cases the generators never reached the edge: a FADT (an ACPI table) that ends exactly at the end of a field, and a memory map longer than the table. In the third, the emulated TmpMap window was poisoned a whole page at a time, so a read past the end of a short table stayed inside an unpoisoned page. We fixed the generators, not the kernel.

## What they cannot see

The hv fuzzer's CPU is a stand-in, so nothing in `hvarch` is fuzzed: not its `unsafe` code, not the VMCB and VMCS (the structures in which AMD-V and VT-x keep a guest's state), and not the world switch between host and guest. The interleavings come from the script too. It walks every path between the CPUs, but it cannot find a race that way.

`fuzz/cpp` does not reach `PageTable::Setup`, which runs on the bootstrap linear map, or anything that needs a second CPU. And `fuzz/fs` leaves one known limitation on record: after a device error in the middle of a create or a rename, the directory in memory can disagree with the disk until the next mount.

The fuzzers stay on as gates, and new targets keep coming: guest placement, the CPUID and MSR policy (checked against a model written from the vendors' manuals, not from the code), and Intel's microcode file. [Tests and gates](/docs/testing) says which gate covers what, and running them has [a how-to of its own](/howto/run-the-gates).
