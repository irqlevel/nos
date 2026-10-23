---
title: "How nos moved its layers from C++ to Rust"
description: "In two days we moved nos's block, network and filesystem layers and virtio to Rust, deleted the C++, and cut the layers' unsafe from 924 sites to 113."
date: 2026-09-19
tags:
  - rust
  - safety
  - drivers
  - networking
  - storage
  - modules
---

On 17 September 2026 the kernel's block layer, network stack and filesystems were written in C++. `src/cpp/net` held 8,909 lines, `src/cpp/fs` 6,849 and `src/cpp/block` 877, and the virtio bus with its four drivers took another 4,839. Rust had been in the tree since March, but only at the edges: four disk and network drivers plugged into C++ tables, the TLS and SSH protocol crates, and a few loadable modules.

By the evening of the 18th all three directories were gone. Judging by the commit timestamps, that took under nine hours. That night and the next day, the new Rust code shed most of its `unsafe`. Here is how it went, and how we showed that nothing broke.

## The rule, and the seam that made the move possible

Those two days produced a rule, and it now lives in [`CLAUDE.md`](https://github.com/irqlevel/nos/blob/master/CLAUDE.md): **new code is Rust unless it has to be C++.** C++ keeps the fundamentals: boot, the MMU and the allocators, the scheduler, locks, interrupts, timers, the HAL and the module loader. Everything built on top of them is Rust.

We could move the layers one at a time, with no flag day (a single switchover where everything changes at once), thanks to the way loadable modules bind to the kernel. The [`Makefile`](https://github.com/irqlevel/nos/blob/master/Makefile) builds their export table like this:

```makefile
# The functions a module may bind to: those the ffi crate declares in its
# extern blocks that the kernel defines. Generated from pass1.elf like the
# symbol table, and linked into the final kernel only -- kernel/module.cpp has
# the empty weak table pass 1 gets instead.
```

The table holds every name that is both declared in the `ffi` crate and defined in the linked kernel. It does not care which language defines the name. So `kernel_netdev_*`, `kernel_net_*` and `kernel_blockdev_*` could move from C++ to Rust under the same names, and every driver and module kept binding to them unchanged. The commit that finished the network layer says this property "is what has made this whole port possible one layer at a time" ([`c87dca6`](https://github.com/irqlevel/nos/commit/c87dca6)).

## Foundations first, and the gate before the move

The order was block, virtio, fs, then net, and each layer started from its foundation: the network stack from the wire formats, the block layer from its partition tables. The very first commit ([`9ce9519`](https://github.com/irqlevel/nos/commit/9ce9519)) moved the MBR and GPT readers, "a reader of bytes nobody here wrote". Its gate (the test every change to this code must pass), `parttest.py`, writes both kinds of table itself, so the test knows exactly which bytes the kernel reads.

The habit that paid off most was to write the gate first and make it pass against the old C++. `ext2-test.py`, which has `e2fsck -fn` judge whatever the driver left on the disk, landed before the ext2 port ([`ea1c7bd`](https://github.com/irqlevel/nos/commit/ea1c7bd)). So did the TCP test and the netconsole test, and the netconsole test proved its worth at once. The first port allocated its ring buffer and its lock, and the kernel came up with netconsole silently disabled. Netconsole is armed from the command line before the page allocator exists, and its whole point is to keep the boot log from that early on. On a Hetzner server, whose only console is the network, the symptom would have been a machine that says nothing at all ([`9c95e67`](https://github.com/irqlevel/nos/commit/9c95e67)).

nanofs, nos's own small filesystem, has no `e2fsck`, so its gate parses the image itself after the kernel has written it. A driver that reads back its own writes "agrees with itself whatever offset it uses", and a control experiment proved the point: moving one field's offset (`IN_BLOCKS`) by four bytes passes every other check in the file and fails only this one ([`29221c9`](https://github.com/irqlevel/nos/commit/29221c9)).

## The C++ views go

After each port, what was left in C++ was a *view*: a class that wraps a handle and the calls on it. The directories emptied thanks to one rule. A C++ caller declares the `kernel_*` or `rust_*` functions it calls right where it calls them, with no shared header. Thirteen network shell commands and seventeen filesystem ones moved into the crates that own their data. Then the commits read like obituaries: [`3a57641`](https://github.com/irqlevel/nos/commit/3a57641) "fs: src/cpp/fs is gone", [`2192392`](https://github.com/irqlevel/nos/commit/2192392) "block: src/cpp/block is gone too", [`d2c0ef2`](https://github.com/irqlevel/nos/commit/d2c0ef2) "net: src/cpp/net is gone". The xHCI driver followed the same evening. Some round trips disappeared on the way. The TCP calls that the SSH server uses had been C++ over the C++ view, and the view called straight back into Rust: "three hops for a Rust-to-Rust call, on every byte sshd sent".

The code moved; it did not grow. `scripts/sloc.py` counts lines of code only, and by its count C++ went from 52,366 lines to 33,858 and Rust from 10,964 to 29,310. The total went from 64,776 to 64,605.

## A failure that looked like success

One regression got past every gate for two hours. At boot, the kernel passes 0 as the size of the network frame pool, meaning "use the layer's default". The deleted C++ view had turned that 0 into a real count. The Rust setup refused it instead, so the pool was never built. Every frame then came from the general allocator, and a free there shoots down the TLB on every CPU, interrupting each one to drop stale address translations. That is exactly what the pool exists to keep off the data path. Nothing failed, because falling back to the allocator is what a pool miss is for.

`netload-test.py` caught it, though it had been written for another reason ([`7a004db`](https://github.com/irqlevel/nos/commit/7a004db)). CI now runs it on arm64, with a comment that the load target "is the one thing here that would notice a frame pool that was never built".

## Then came the unsafe

The fresh Rust had 924 `unsafe` sites across `net`, `block` and `fs`, and almost none of them had to do with hardware. The commit that started the cleanup ([`01b42d6`](https://github.com/irqlevel/nos/commit/01b42d6)) named three causes. First, every lock in `kcore` guarded nothing: the data sat beside the lock, not inside it. Second, the seams between two pieces of Rust still had the shape of the C++ they replaced, an `extern "C" fn(ctx: *mut u8, ..)` with a cast on the other side, and 66 exports had no caller at all. Third, graphs were built from raw pointers. Here is the ARP cache before:

```rust
pub struct ArpTable {
    lock: SpinLock,
    cache: core::cell::UnsafeCell<[Entry; CACHE_SIZE]>,
}

/* Everything inside is touched with the lock held */
unsafe impl Sync for ArpTable {}
unsafe impl Send for ArpTable {}

// ...
        let _guard = self.lock.lock();
        let cache = unsafe { &mut *self.cache.get() };
```

And after, in [`net/src/arp.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/arp.rs):

```rust
pub struct ArpTable {
    cache: SpinLock<[Entry; CACHE_SIZE]>,
    // ...
}

// ...
        let cache = self.cache.lock();
```

The `unsafe` now appears once per lock type, in the guard's `Deref` in [`kcore/src/sync.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/sync.rs). This was not cosmetic. TCP had been reading other connections' state under the pool lock without taking their own locks, and the old shape hid that behind the same `unsafe` as everything else. The lock order is now pool, then connection.

Each pattern that used to need `unsafe` got a type in `kcore`, and the type's own comment explains why it is sound. `Once` is for something there is only one of, `CpuLocal` for per-CPU state, `RxOwned` for what only the receive path touches, and `kcore::pod` for on-disk structures. (ext2's superblock says `unsafe impl Pod` once, and every read after that is bounds-checked.) A filesystem's tree became an arena of ids, each a slot plus a generation, so a stale id finds nothing. A handle that comes from outside is looked up, never dereferenced:

```rust
    pub fn by_handle(&'static self, handle: usize) -> Option<&'static Device> {
        let base = self.devices.as_ptr() as usize;
        let offset = handle.checked_sub(base)?;
        if offset % core::mem::size_of::<Device>() != 0 {
            return None;
        }
        self.devices.get(offset / core::mem::size_of::<Device>())
    }
```

The drivers followed ([`8147157`](https://github.com/irqlevel/nos/commit/8147157)). A driver now implements a trait over a device that is `Box::leak`ed and lives forever, and its descriptor ring is a `&'static` slice of `Volatile` cells. An interrupt handler is a function item (in Rust, a named function whose type takes no space) that receives its `&'static` target. `kcore` checks at compile time that the handler has no bytes, so the context word the kernel carries *is* the target:

```rust
pub(crate) const fn assert_stateless<F>() {
    assert!(
        core::mem::size_of::<F>() == 0,
        "a handler must be a function item or a closure that captures nothing",
    );
}
```

Here are the counts from [`scripts/unsafe-count.py`](https://github.com/irqlevel/nos/blob/master/scripts/unsafe-count.py), which counts blocks, `unsafe fn` and `unsafe impl`:

| Crate | Before, `7a004db` | After, `2f9133f` | Master, October 2026 |
|---|---|---|---|
| `net` | 486 | 61 | 68 |
| `block` | 70 | 14 | 14 |
| `fs` | 368 | 38 | 47 |
| the nine driver crates | 216 | 15 | 15 |
| `kcore` | 274 | 241 | 268 |

`kcore` rose to 363 as the types moved in, then fell as dead wrappers went and 51 FFI functions that are sound for any argument became `safe fn`. The few sites that `net` and `fs` have gained since are at the C ABI through which the hypervisor module reaches them.

## Showing "no behaviour change"

A refactor that only changes types should leave the machine code alone, so we checked the machine code. After the `safe fn` change, the `.text` and `.data` sections of both kernels (x86-64 and arm64) were byte for byte what they had been, and all 10,220 and 10,904 functions kept their sizes. `.rodata` differed only in the module ABI digest, which is a hash of the FFI sources ([`cd94789`](https://github.com/irqlevel/nos/commit/cd94789)). Where a change could not leave the binaries alone, its commit says so, and the gates on both architectures are the evidence ([`4c3f585`](https://github.com/irqlevel/nos/commit/4c3f585)).

On the network side we compared packets. Under QEMU, with a packet dump running, we sent the old and the new `netblk` module the same eight requests from the same port, and the eight replies were byte for byte the same ([`b55366b`](https://github.com/irqlevel/nos/commit/b55366b)). In the drivers, we diffed the sequences of register accesses and DMA barriers against the old code, and they matched. Nothing emulates the r8168 or the r8125, and their commit honestly says so: "compiled, diffed, not run". Since then the EX44, whose only console is its RTL8125, has booted nos, and we have logged into it over ssh ([Real hardware](/docs/real-hardware)).

## Modules

The UDP load target, the receiving end of a UDP load test, was the last optional piece still linked into the network layer. It became the `netload` module, which has no `unsafe` of its own ([`cda33d1`](https://github.com/irqlevel/nos/commit/cda33d1)). The frame formats moved into `netwire`, a crate with no kernel in it, because a module is linked on its own and cannot link the layer. A module's listener also got the same typed shape the layer itself uses, `Nic::listen(port, Arc<H: UdpHandler>)`. The crate boundary had a price. With no LTO (link-time optimization), the echo handler went from 8 calls per packet in the kernel image to 18 as a module, and `#[inline]` on the per-packet accessors brought it down to 10 ([`2f9133f`](https://github.com/irqlevel/nos/commit/2f9133f)). One limit remains: a disk or network driver cannot be a module, because it registers with its layer as a trait object from inside the image.

## What stayed

The two days left rules as well as code. New code is Rust, with as little `unsafe` as the job allows. When a pattern has no type yet, it is added to `kcore` once, with the reason it is sound, rather than as an `unsafe` block at every use. And a change checks itself with `unsafe-count.py`. [Rust in the kernel](/docs/rust) covers all of this at length, and [writing a module](/howto/write-a-module) is the place to start.
