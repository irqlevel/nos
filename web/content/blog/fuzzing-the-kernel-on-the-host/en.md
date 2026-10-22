---
title: "Fuzzing the kernel on the host"
description: "Four fuzzers that compile the kernel's own sources into host programs — hypervisor, network, storage and the C++ — and the bugs they found."
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

Almost everything a kernel reads was written by someone else. A guest chooses every byte of its virtio rings; the network, every frame and every DHCP answer; a disk, its own geometry — 520-byte sectors, 2^64 of them, if it likes. Firmware hands over the device tree, the ACPI tables and the memory map before a headless machine has any console at all. A panic on any of those paths is a machine that stops, and on the two Hetzner servers, whose only console is the network, it stops without a word.

A boot under QEMU only ever sees the inputs QEMU happens to produce. So over the last two days of September 2026 we landed four fuzzers, each of which runs the kernel's own code — not a copy, not a model of it — as an ordinary program on the host.

## The kernel's sources, compiled for the host

Most of what needed fuzzing was already portable. The block, network and filesystem layers are Rust crates over `kcore` and `ffi` ([how they got there](/blog/cpp-to-rust-layers) is another story), and Cargo builds them for the host as readily as for the kernel. [`fuzz/fs`](https://github.com/irqlevel/nos/blob/master/fuzz/fs/Cargo.toml) depends on them by path, with the profile a `RUSTUB=1` kernel has, so an overflow on the host is the same panic, not a silent wrap:

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

The hypervisor's files import `hvarch` — the CPU's virtualization extension and all of its `unsafe` — so [`fuzz/hv`](https://github.com/irqlevel/nos/blob/master/fuzz/hv/src/main.rs) compiles them one by one and itself answers to the names `ffi`, `hv`, `hvarch` and `kcore`. Whatever they reach outside themselves, from the clock to the CPU, is a stand-in in the same program:

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

The C++ gets the same by other means: [`fuzz/cpp/Makefile`](https://github.com/irqlevel/nos/blob/master/fuzz/cpp/Makefile) compiles the kernel's files as they are, under ASan and UBSan, with a host HAL first on the include path in place of each architecture's inline one — no privileged instruction, and the MMU's hooks visible to a target that models the MMU. All four fuzzers live in `fuzz/` rather than under `src/rust/`, whose `.cargo/config.toml` turns any cargo build beneath it into a kernel build.

## A machine to run them on

The network and storage layers expect a kernel under them. [`fuzz/common`](https://github.com/irqlevel/nos/blob/master/fuzz/common/machine/sched.rs), shared by the net and fs fuzzers, is one written for the purpose: every task a host thread, only one of them holding the turn at a time, and the input deciding who runs next wherever a task blocks or lets a lock go. Nothing sleeps on a real clock — when every task waits, the clock jumps to the next deadline, so an hour of a DHCP lease costs only what its timers cost. And the kernel's rules are checked at the line that breaks them:

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

Two locks taken in both orders, an allocation with interrupts off, a task that ends holding a lock: each is a finding where it happens. Each input runs in a process of its own, forked from one booted machine, so one input's statics never leak into the next.

None of this is coverage-guided. An input is random bytes from a seed, read as a stream of choices by a generator that knows the format ([`input.rs`](https://github.com/irqlevel/nos/blob/master/fuzz/common/input.rs)). Past the input's end every read is zero, and zero is always the ordinary choice; and whatever ends a run early must be rare, or the runs never get deep:

```rust
/// True about once in 4096: for what ends a run, which has to be much
/// rarer than a run is long for the rest to be reached.
pub fn rare(&mut self) -> bool {
    self.u16() >= 0xFFF0
}
```

## Four fuzzers

| Fuzzer | Built from | Targets | Held to |
|---|---|---|---|
| `fuzz/hv` | the hypervisor's devices, local APIC and IO-APIC, MMIO decoder, page walker, Linux loader, ACPI tables, CPUID/MSR policy and run loop; the guests' switch, DHCP server and NAT | 22 | invariants on every device; models of the switch, NAT, the policy and guest placement |
| `fuzz/net` | the `net`, `netwire`, `tls`, `fs` and `ssh` crates, and the sshd module's source | 10 | a modelled world, protocol by protocol |
| `fuzz/fs` | the `block` and `fs` crates | 9 | a model of the tree, an e2fsck of its own, power cuts |
| `fuzz/cpp` | the C++ that reads what firmware, the bootloader or a disk hands it; the module loader; printf; the page tables and the heap | 11 | a model of what each header promises, under ASan and UBSan |

The most ambitious hv target, `platform`, builds a Linux guest's machine as `hv boot` builds it and runs it on the real run loop, `LinuxGuest::run`, one vCPU thread at a time. Where the loop would enter the guest, a script says what the guest does next — a port, an MSR, a fault on a device's page with the instruction behind it, a halt — and does what drivers do over many exits, from filling MSI-X tables to starting APs by INIT and SIPI. A vCPU entering its guest with a lock held, or the run loop reading the clock a million times with neither an entry nor a wait, is a finding.

`fuzz/net` surrounds the machine with a world ([`src/world`](https://github.com/irqlevel/nos/blob/master/fuzz/net/src/world/check.rs)): a link that delays, reorders, duplicates and loses, a TCP peer that keeps every byte of both streams, a rustls server with the fuzzer's own CA, DHCP and DNS servers that lie, a full SSH client. `fuzz/fs` gives the layers disks with a volatile write cache, a power switch and requests that fail ([`disk.rs`](https://github.com/irqlevel/nos/blob/master/fuzz/fs/src/machine/disk.rs)), and judges their images with an e2fsck of its own, checked against e2fsprogs over hundreds of images.

## Gates and campaigns

With no arguments each fuzzer runs a fixed number of inputs from seed 1, the same inputs every time. That makes it a gate: CI runs all four on every push, hv and fs on its x86-64 leg, net and C++ on the arm64 one, at half a minute to a couple of minutes each. A campaign is the same program with a seed of its own:

```sh
python3 scripts/hv-fuzz.py                          # the gate
python3 scripts/hv-fuzz.py --seed 7 --seconds 300   # five minutes a target
python3 scripts/hv-fuzz.py --replay platform out/hv-fuzz/findings/X.hex
```

A finding comes with the seed and iteration that make it again and the input saved for `--replay`; the exit code is 0 for nothing, 1 for a finding, 3 for a hang. How deep the inputs go is measured, not hoped for: `HV_FUZZ_STATS=1` says how the whole-machine runs ended, and on its first day the hv gate reached 94% of the regions of the sources it compiles, by clang's coverage.

## What they found

Each fuzzer landed together with the fixes for what it found. A selection.

### A guest that could be another guest

`guestnet` wires the switch and NAT together as the hv module does. On its first input it found that a guest could send as another: the switch passed on frames from any MAC and any address, and NAT answers a flow at the MAC its last packet came from — so one packet with another guest's address and the ports of one of its flows took the flow, and the world's answers went to the impostor.

The fix ([d1aeecf](https://github.com/irqlevel/nos/commit/d1aeecf09004bba80ed5301eea6b25d60f1dc1d6)) is what a cloud's virtual switch does: [`ingress`](https://github.com/irqlevel/nos/blob/master/src/rust/modules/hv/src/net.rs) checks every guest frame where it comes in — IPv4 or ARP only, from the port's own MAC and address, the one exception a DHCP client that has no address yet, which the switch answers itself. The rest is dropped and counted against the guest in `hv list`. A control run with the check taken out once showed the harm itself: `hv forward`'s connection to one guest's web server landed on the other. `hv-linux-test.py --net` now has a real Linux guest claim its neighbour's address and checks that nothing comes back.

### An address past the top of memory

A virtio buffer at the very top of the guest's address space made the disk's status write compute its address past 2^64 — a panic under `RUSTUB=1`, a silent wrap to page 0 in a plain kernel. The fix is a type rather than a check at every call site: a segment's address is private, and the only way to it does the sum checked ([`virtio.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/virtio.rs)):

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

The same first day ([4ce4e77](https://github.com/irqlevel/nos/commit/4ce4e771b0f1f0d8cc2a662e0e10d1e913571b56)) found a halted vCPU spinning its host CPU — a periodic APIC timer's interrupt held back by the guest's priorities, and a timer that reported a deadline already past — and a Linux loader that took a kernel's preferred address below 1 MiB, over the zero page and page tables it writes there. After the fixes, campaigns on ten seeds ran 7.9 billion inputs on the final harness and found nothing more.

### A sleep inside a spin lock

The netconsole's drain task polls until the device has an address ([c68452d](https://github.com/irqlevel/nos/commit/c68452d5b7ab1ce09024cce09e050bcc4271090f)):

```diff
-            let nic = match self.log.lock().nic {
+            let attached = self.log.lock().nic;
+            let nic = match attached {
                 Some(nic) if nic.ip() != 0 => nic,
                 _ => {
                     kcore::task::sleep_ms(NO_LINK_POLL_MS);
                     continue;
```

The old line reads like "copy a field, drop the lock". It is not: a temporary in a `match`'s scrutinee lives to the end of the `match`, so the log's `IrqSpinLock` was held through the 200 ms sleep, interrupts off, every line any CPU traced meanwhile spinning on it. Sleeping with a spin lock held is exactly what the machine's rules make a finding.

### Failures that looked like success

The net fuzzer's world knows what it sent, so it can tell a short answer from a whole one — and found a family of bugs in which the kernel reported success. An HTTP response cut inside its headers came back as a body made of its own status line: "ok". The TLS client took a stream cut inside a record, and a receive that timed out, for the end of the stream, so a stalled body with no length came back whole. The DHCP client, once it had renewed, started over from DISCOVER while still bound, and never renewed its lease again; it now keeps one as RFC 2131 says.

### A power cut between two writes

ext2's `create_file` wrote the new inode before the bitmap bit that marks it taken ([07e7409](https://github.com/irqlevel/nos/commit/07e7409436ebde4f340b209d4f7a8b0288e2f5d3)):

```diff
-        if !self.write_inode(ino, &inode) || !self.flush_bitmap() || !self.commit_meta() {
+        if !self.flush_bitmap() || !self.write_inode(ino, &inode) || !self.commit_meta() {
```

A power cut between the two leaves an inode in use that the bitmap calls free, for the next create to take — and the fuzzer's disk pictures exactly that cut. Elsewhere in storage: a directory whose load failed part way read its entries again at every look, so each `ls` grew the tree for good; and on a device with 520-byte sectors the disk log — the tool for machines that die without a word — ran a full batch past its buffer: a panic at boot with `disklog=on`.

### Index −1, from the first thing the arm64 boot reads

In the device tree reader, an `END_NODE` with no node open took the walk's depth below the root, and the next node wrote its cell counts at index −1 of the walker's stacks — a write into `.bss` from the first thing the arm64 boot reads ([00631ab](https://github.com/irqlevel/nos/commit/00631ab0b9cced3c6cb53c7ef623e04da0e24fa5)). Properties are now read through `Fdt::Prop`, every read bounded by its length. The same round found the heap's pools allocating a page with their spin lock held: the deadlock in [no allocation under a spin lock](/blog/no-allocation-under-spinlock).

### The AX41's other IO-APIC

The `acpi` target ([516dc26](https://github.com/irqlevel/nos/commit/516dc2680ab97acaa11370cec0e722b8aa590d3e)) found every table the root listed staying mapped in the TmpMap window for good — 21 of its 448 slots on the AX41 — and, of two IO-APICs listed, the kernel keeping the last. The AX41's MADT lists the FCH's IO-APIC (GSI 0) first and the GNB's second, so the kernel had been driving the GNB's: the legacy IRQs, the HPET's tick among them, were set up on pins none of those lines reach. Nothing had noticed, since the tick matters only until each CPU runs its own local APIC timer; booted there with the fix, `irqstat` counts the tick at its 100 Hz.

And printf: `VsnPrintf` read every integer conversion as a 64-bit slot, so the `int` trace level of every `Trace` came with whatever the other half of its register held ([3743fcf](https://github.com/irqlevel/nos/commit/3743fcf43965be6df80229dd859cd48db5080a8b)). The formats are C's now, checked by the compiler through `PRINTF_FORMAT` and `-Wformat=2`, and a `format` target holds `VsnPrintf` to the host's `vsnprintf`, byte for byte.

## Proving a harness bites

A fuzzer that finds nothing may be good news, or a harness that cannot see. So each was also tested the other way round: break the code on purpose, and the target must fail. For the three readers the x86 boot depends on, every check was taken out one at a time — 26 of them — and each was a finding within 20,000 inputs. Three were missed at first: two because the generators never reached the edge (a FADT ending exactly at a field's end, a memory map longer than the table), one because the emulated window was poisoned by the page, so a read past a short table stayed inside it. The generators were fixed, not the kernel.

## What they cannot see

The hv fuzzer's CPU is a stand-in, so nothing of `hvarch` — its `unsafe`, the VMCB and VMCS, the world switch — is fuzzed, and the interleavings are the script's: every path between the CPUs is walked, but no race can be found that way. `fuzz/cpp` does not reach `PageTable::Setup`, which runs on the bootstrap linear map, or anything that needs a second CPU. And `fuzz/fs` leaves one known limitation on the record: after a device error in the middle of a create or a rename, the directory in memory can disagree with the disk until the next mount.

The fuzzers stay on as gates, and the targets keep coming — guest placement, the CPUID and MSR policy (held to a model written from the vendors' manuals, not from the code), Intel's microcode file. Which gate covers what is in [Tests and gates](/docs/testing); running them is [a how-to of its own](/howto/run-the-gates).
