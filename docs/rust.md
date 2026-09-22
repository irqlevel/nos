# Rust in the kernel

Everything that sits on the fundamentals is Rust: every driver, the virtio
bus, the block layer, the network layer, the filesystems, the TLS client,
the SSH protocol and every loadable module. It is a cargo workspace,
`src/rust/`, that compiles to a `#![no_std]` `staticlib` (`libkernel.a`)
linked into the kernel; the crates are listed one by one in
[Project layout](project-layout.md), and modules have a page of their own,
[Loadable modules](modules.md).

The rules are in [`CLAUDE.md`](../CLAUDE.md): new code is Rust unless it has
to be C++, and Rust is written with as little `unsafe` as the job allows,
with what is left in `kcore` and at the C ABI rather than in the layers.
This page is how those rules are kept: how the crates are layered, how a
kernel service reaches Rust, the type each shape that used to need `unsafe`
has now, a map of `kcore`, and how a driver is written.

## The layers

```
ffi/                              raw extern "C" declarations, and nothing else
kcore/                            safe RAII wrappers around kernel services
netwire/                          the frame formats, with no kernel in them
block/  net/  fs/  tls/  virtio/  the layers
drivers/*                         over block, net, virtio and kcore
kernel/                           entry points, the global allocator: what C++ calls
kmod/ + modules/*                 loadable, each linked on its own, over kcore
```

Inside the kernel image the crates call each other as crates: a disk's
driver depends on `block` and a NIC's on `net`, `fs` depends on `block`,
`net` on `fs` (for `wget`) and on `tls`. `tls` sits *below* `net`, so it
takes the connection it speaks over as a trait, `tls::Transport`, which
`net`'s HTTP client implements over a TCP connection.

Two crates have no kernel in them at all, and that is what they are for:
`netwire` (the frame formats and the internet checksum) and `ssh` (the
protocol). A loadable module is linked on its own and cannot link a layer --
the layer's statics would be a second copy -- but it can link these, so what
the network layer and the `netload` module both need of a frame is written
once.

The C ABI is for what is on the other side of a link: C++, and a module,
which is linked on its own and can share no other seam with the kernel.
`kcore`'s `block`, `net`, `tcp` and `fs` modules are exactly that: those
layers *as a loadable module reaches them*. The image gets its disks, NICs,
connections and files from the crates themselves; what it takes from those
four modules is what the two sides of the ABI have to agree on (`BlockIo`,
`SubmitError`, the `IO_*` and `SUBMIT_*` codes) and the few calls by which
the net layer asks C++ what the command line said and replays the log. A
`kernel_*` name defined in Rust that no module imports and no C++ calls has
no reason to exist.

On the C++ side there is no `src/cpp/{block,net,fs}` and no C++ view of any
of those layers. A C++ file that needs one declares the `rust_*` /
`kernel_*` calls it makes where it makes them: the tracer and the panic path
into netconsole and the disk log, the idle path into the receive poll, boot
into the frame pool, TCP, the services and the mounts, the C++ part of the
shell into files.

**Small functions that cross a crate boundary are `#[inline]`.** The build
has no LTO, so a function in another crate can only be inlined if its body
travels with the crate's metadata, and for anything that is not generic that
takes the attribute. Without it a two-instruction accessor is a call, with
its own bounds checks that the caller's cannot cancel -- and a module pays it
twice over, being another crate to everything. That is what `netwire`'s
accessors and header writers carry it for, and `kcore`'s per-frame wrappers
(`Lent`, `NetFrame`, `TxBatch`, `cpu::id`, `time::boot_time_ns`; the ring,
the event and the block calls had it already). It was measured, not
guessed. Moving the frame formats out of `net`, and the load target out of
the image, took its echo handler from 8 calls a packet to 18; with the
attributes it is 10, seven of them the calls into the kernel that a module
cannot avoid. In the image the same move took TCP's `send_segment` from 1
call into the formats to 4 and ICMP's echo answer from 5 to 8; with the
attributes both have none, and there are 2 such call sites left outside the
self-test where there were 57. Count them the same way after adding to
either crate:
`llvm-objdump -d -C` on the `.ko` or the kernel, and the `bl` instructions in
the handler.

One consequence: a disk's or a NIC's driver cannot be a loadable module
today. It registers with its layer as a trait object from inside the image,
and there is no C name for a module to bind. A module *uses* disks and NICs
(`kcore::block::Disk`, `kcore::net::Nic`); it does not provide them.

## Adding a kernel service

Three steps, and a module gets the service by the same three, because the
module export table is generated from the `ffi` declarations:

1. the C++ function in `src/cpp/kernel/rust_ffi.cpp`;
2. its raw declaration in `src/rust/ffi/src/<module>.rs` (+ `pub mod` in
   `ffi/src/lib.rs`);
3. the safe wrapper in `src/rust/kcore/src/<module>.rs` (+ `pub mod` in
   `kcore/src/lib.rs`).

**`ffi/`** holds `extern "C"` declarations and `#[repr(C)]` structs and no
logic. Every pointer is raw (`*mut u8`, `*const u8`): no reference crosses
the boundary. A kernel object is an opaque `usize` handle, 0 for null or
failure. The blocks are `unsafe extern "C" { .. }` -- **at the start of a
line**, which is what the Makefile's module export table looks for -- and a
function that is sound for *any* argument at *any* time is declared
`pub safe fn`, so that calling it needs no `unsafe` block and the blocks
that are left mean something.

Check the definition before writing `safe`. It qualifies if it is a C++
function that takes plain values and validates them (soft IRQ types are
range-checked), or a Rust export that is itself a safe `extern "C" fn` and
only looks its handle up in a table. It does not if it takes a buffer, a
callback or a C++ handle. Three that look safe and are not: the PCI config
reads, because the arm64 ECAM accessor bounds the bus and not the slot or
the function; and `irq_restore` and `preempt_enable`, which can open a
window inside somebody else's critical section. A change of the
declarations changes the digest every module's header carries, so modules
are rebuilt with the kernel ([What a module may
call](modules.md#what-a-module-may-call)).

**`kcore/`** is where a handle becomes a type. Whatever needs cleanup
implements `Drop` calling the matching `kernel_*_destroy` / `kernel_*_stop`
/ `kernel_*_put`; a handle that must not be `Send`/`Sync` carries
`PhantomData<*const ()>`; ownership crosses to C++ by value, inside the
wrapper, so no call site has a `forget` to remember.

**`rust_ffi.cpp`** is the C++ side, and all of it lives in that one file.
Register/create functions heap-allocate their adapters with
`Mm::TAlloc<T, RustAllocTag>()`, never a static array of non-trivial
objects. The slot pools for IRQ, timer and MSI-X callbacks are under a
`RawRwSpinLock`: read-locked in the handlers, write-locked with interrupts
saved to register or unregister. Handles are raw pointers cast to
`unsigned long`, 0 for failure.

## Where `unsafe` goes

`block/`, `net/`, `fs/` and the drivers are ordinary Rust. What is left of
`unsafe` in them is where a pointer and a length arrive from C++ or from a
module, an `unsafe impl` saying a structure is plain words, and the entry
point C++ calls a crate by. Everything else that used to need it has a
`kcore` type, listed below, with the reason it is sound in the type's own
comment.

When the shape you need has no type, add the type to `kcore` -- once, with
that reason in its comment -- rather than an `unsafe` block at every use.
And do not lower the count by hiding it: a pointer kept as a `usize`, a
macro that folds ten blocks into one, a safe wrapper that trusts what it is
given, all leave the risk and remove the warning.

`scripts/unsafe-count.py [crate...]` counts the sites -- blocks,
`unsafe fn`, `unsafe impl` -- for the layers and `kcore`, or for the crates
it is given. A change that raises a layer's or a driver's number should be
able to say why.

## The shapes that used to need `unsafe`, and the type each has now

- **A lock owns its data**: `Mutex<T>`, `SpinLock<T>`, `IrqSpinLock<T>`,
  `PreemptSpinLock<T>`, `TryLock<T>` -- never a bare lock beside an
  `UnsafeCell`. A lock that guards nothing is `Lock<()>`. Guards are values:
  hold two, drop them in either order, return one from a function
  (`Tcp::take_fresh`).
- **What waits when dropped** -- a `UdpListener` (for the receive path), a
  `TaskHandle` (for the task), an interrupt handle -- sits in its own
  `Lock<Option<..>>`: `take()` it under the lock, drop it after. Dropped
  under the lock its wait needs, it is a deadlock.
- **The one of something** is a `Once`/`OnceBox` static, not an `AtomicPtr`
  and a `&*ptr`.
- **Per-CPU state** is `PerCpu<LocalCounter…>` for numbers and `CpuLocal<T>`
  for anything exclusive -- never an `UnsafeCell<[T; MAX_CPUS]>` indexed by
  `cpu::id()`, which is a data race the moment a task migrates between the
  index and the access.
- **What only the receive path touches** is an `RxOwned<T>`, reached with
  the `&mut RxContext` the receive pass makes once and lends down: a
  driver's receive ring, and the replies a listener gathers during a batch.
  A service's `UdpHandler` is lent each frame as a `Lent` (`net::nic`); a
  module's is the same shape across the C ABI (`kcore::net`), told when a
  receive batch ends too, and gathers its replies in a `TxBatch`.
- **An on-disk or on-wire structure** goes through `kcore::pod`:
  `unsafe impl Pod` said once, of a `#[repr(C)]` structure of integers with
  no padding, and then `read`/`write`/`zeroed` are safe -- bounds-checked
  and unaligned.
- **A handle from outside is looked up, not dereferenced**: a net device, a
  TCP connection, a block device and an open file are each a slot of a table
  that lives as long as the kernel (`DEVICES.by_handle`, `TCP.by_handle`,
  block `device()`, `vfs::Handle`), so a word that is not one is `None`
  rather than undefined behaviour, and the functions taking them need not be
  `unsafe`.
- **A graph is an arena and ids**, not raw pointers: `fs::vnode::Tree` hands
  out `NodeId`s -- a slot and a generation -- so a stale one finds nothing.
  Open files are the same shape.
- **A seam between two pieces of Rust is a trait** or a `&mut dyn`
  (`vfs::FileSystem`, `http::Sink`, `UdpHandler`, `tls::Transport`,
  `BlockDriver`, `NetDriver`), not an `extern "C" fn(ctx: *mut u8, …)`
  table. An export that trusts a pointer is `unsafe extern "C" fn` with a
  `# Safety` section; one that takes only validated handles and plain values
  is a safe `extern "C" fn`; **an export nobody calls is deleted**, not kept
  for symmetry.
- **A callback is a function item over its target**:
  `register_for(.., target: &'static T, handler)`, the handler an
  `fn(&'static T)` named directly (`Nic::interrupt`), never a closure that
  captures. `kcore` checks at compile time that it has no bytes and stamps
  out the trampoline per handler type, so the context word *is* the target
  (`kcore/src/callback.rs`).
- **Device memory is cells, not `read_volatile` through a cast pointer**: a
  descriptor is a `#[repr(C)]` struct of `Volatile<u32>`
  (`unsafe impl Descriptor` says so once), a ring is
  `DmaBuffer::leak_ring`'s `&'static [D]` -- shared by the device, the poll
  and the state dump, which reads it live -- and anything else in a
  `DmaBuffer` crosses through `load`/`store`/`bytes`. The barriers stay
  where they were: `dma_wmb` before the store that hands ownership over,
  `dma_rmb` after the check that takes it back and before the payload reads.
  `core::sync::atomic::fence` is not a substitute for either (the barrier
  rule in `CLAUDE.md` says why).

## What `kcore` has

| Module | What is in it |
|---|---|
| `consts` | `PAGE_SIZE`, `SECTOR_SIZE`, `KB`/`MB`/`GB`, `NS_PER_SEC`/`MS`/`US` |
| `trace` | `trace!(level, "fmt {}", arg)`; level 0 is always visible |
| `time` | `Duration`, `boot_time()`, `boot_time_ns()` (full resolution), `wall_clock_secs()` |
| `random` | `fill_random(&mut [u8])`, `random_u64()` |
| `sync` | **Locks own what they guard**: `Mutex<T>` (sleeps), `SpinLock<T>` (the kernel's), `IrqSpinLock<T>` and `PreemptSpinLock<T>` (const-constructible, for statics; the first also `try_lock`), `TryLock<T>` (never waited for; the holder may sleep). `lock()` gives a guard that derefs to the data. `WaitGroup`, `Event` (one waiter blocks, anyone signals — hard IRQ included; a blocked waiter's CPU gets an IPI) |
| `once` | `Once<T>` (set once by boot or a `setup`, read from anywhere) and `OnceBox<T>` (made on first use; racing makers get the same one) |
| `percpu` | `PerCpu<T>` + `LocalCounter` (statistics any CPU may read, added to with no bus lock), `CpuLocal<T>` (state only its own CPU touches, reached with interrupts off through `with`) |
| `ring` | `LocklessRing` — the kernel's bounded MPMC queue of words, one CAS an operation, safe from any context |
| `static_ring` | `StaticRing<N>` (the same, in static storage) and `Mailbox<T, N>` (fixed-shape messages written and read in place, from any context, no allocation) |
| `task` | `spawn`, `spawn_for(name, &'static T, fn(&'static T))` (a service's task over its one instance — no raw context), `spawn_with(name, ctx, fn(C))` and `spawn_on_with(name, cpus, ..)` for a task that owns what it starts from — a module's, holding an `Arc` of its state, since nothing of a module lives for good — and the raw `spawn_with_ctx`, which sshd and blkload still use (each takes the name `ps` and `top` show), `TaskHandle` (`id`), `sleep`, `yield_to_runnable`, `current_id` (is this call from one of my own tasks?) |
| `cpu` | `id`, `count`, `online_mask`, `run_on` (IPI) and `run_on_with(cpu, &arg, f)` (the same, the context a borrowed value rather than a word to cast -- sound because the call waits for the handler), `synchronize` (returns once every interrupt handler running at the call has returned), `with_interrupts_off(\|cpu\| ..)` (the scoped `irq_save`/`irq_restore`: a check of this CPU's state and the step that relies on it, with nothing in between) |
| `msix` | `MsixTable`, `MsixInterrupt::register_for(table, index, &'static T, handler)` (16 slots, shared by every Rust driver) |
| `interrupt` | `LegacyInterrupt::register_level_for(dev, &'static T, handler)` / `register_irq_for(irq, …)` (INTx, 8 slots) |
| `softirq` | `raise`, `register_for(type, &'static T, handler)` (`TYPE_NET_RX`, `TYPE_BLK_IO`, …). There is no unregister, which makes a module that registers one permanent |
| `timer` | `Timer::start_for(period, &'static T, handler)` (periodic, 8 slots, fires on CPU 0 via IPI) |
| `pci` | device scan, config r/w, BARs (`get_bar`, `get_bar64`) |
| `io` | `MmioRegion` (8/16/32/64-bit, `window(offset)` for a register block inside it), `Port<T>` |
| `dma` | `DmaBuffer` (contiguous; `bytes`/`bytes_mut`, and `load`/`store` of a `Pod` at an offset — volatile, aligned and bounds-checked; `as_pod`/`as_pod_mut`, the whole buffer as one `Pod` structure the hardware touches only inside an instruction the owner executes, an SVM VMCB), `PhysMapping`, `virt_to_phys`; for a descriptor ring, `Volatile<T>` cells, `unsafe trait Descriptor` and `DmaBuffer::leak_ring::<D>() -> (&'static [D], phys)` |
| `barrier` | `dma_wmb`, `dma_rmb` (`dmb oshst`/`oshld` on arm64) |
| `pod` | `unsafe trait Pod`, then `pod::read`/`write`/`zeroed`: how an on-disk or on-wire structure is read out of a buffer; `bytes_of`/`bytes_of_mut`, a value as the bytes it is |
| `cmd` | `Command::register` — a shell command whose handler writes to an `Output`; unregistered on drop, after any running call returns. `dispatch` — run a command line as the console would, its output handed to a closure as it is printed |
| `entropy` | `trait Source` + `register_source(name, &'static S)` — a hardware generator the pool reseeds from |
| `block` | **for a module.** `Disk` — an existing disk or partition by name: synchronous `read`/`write`/`flush`, `partitions`; asynchronous `submit(&BlockIo, kick)`/`kick` straight to physical memory, the completion callback from interrupt context (`can_submit`: NVMe and its partitions); `claim` → `DiskClaim` before writing, refused while a mount, the disk log or another writer holds the device or one overlapping it. The image uses the `block` crate itself: `block::Disk`, the same shape with no ABI in between, plus `count`/`at`, `name`/`parent`/`handle` and `claim_as` |
| `net` | **for a module.** `Nic` — an existing device by name: `ip`, `mac`; `listen(port, Arc<H>)` → `UdpListener`, which owns its `UdpHandler` until it is dropped — `on_frame(Lent, &mut RxContext)` for each datagram, `on_batch_end(&mut RxContext)` when a receive batch ends, `RxOwned<T>` for what only those two touch, `TxBatch<N>` for the frames that go to the NIC together; `resolve(ip)` (the Ethernet address a frame to `ip` goes to: route, then ARP; sleeps), `tx_room` (how much the transmit queue will take), `transmit`, `transmit_raw`; `NetFrame` (`alloc_tx`, `data_mut`, `data_raw_mut`, `data_phys`, `set_len`); `rx_stats` (whether the receive path is keeping up). A frame is an owned value from the moment a listener keeps one (`Lent::retain`) to the moment it goes to the NIC: no module sees the word the kernel knows it by. And for the net layer, what the kernel command line said (`dhcp_off`, `dns_on`, `netconsole_params`) and `replay_kernel_log`. The image uses the `net` crate itself: `net::Nic`, `net::Frame`, typed `UdpHandler` listeners |
| `tcp` | **for a module.** `TcpListener::bind(&Nic, port)` → `accept(timeout)` → `TcpStream` (`send_all`, `send` with a timeout, `recv`, `peer`), each closed on drop; a listener's drop resets what it never accepted |
| `fs` | **for a module.** `read(path, max)`, `write(path, data)` (replaces, then syncs), `create`, `create_dir` — its configuration files. The image opens files through the `fs` crate itself: `fs::vfs::Open::new(fs::vfs_instance()?, path, flags)` |

## Writing a driver

The traits' own comments (`block/src/table.rs`, `net/src/device.rs`,
`kcore/src/entropy.rs`) are the contract, and the smallest real driver of
each kind is the example: `drivers/virtio_blk` (block; `drivers/nvme` for
the asynchronous path), `drivers/r8168` (a NIC), `drivers/virtio_rng` (an
entropy source -- the one kind that still goes through a `kcore` trampoline,
because the pool is C++).

**The device lives for good.** It is `Box::leak`ed, so it is `&'static`:
what the interrupt is pointed at and what the layer's table keeps, as a
trait object -- no ops table, no `ctx`. Shutdown is `quiesce(&self)`, not a
`Drop`: the register writes that stop the hardware, then the interrupt
handles. Nothing is freed -- the device, its rings and its BAR mapping stay,
because the tables that point at them do.

**Interrupts.** `MsixInterrupt::register_for(&table, index, dev,
Dev::interrupt)`, or `LegacyInterrupt::register_level_for(pci_dev, dev,
Dev::interrupt)`, where the line may be shared, so the handler says by its
own status register whether the interrupt was its. The handler runs in IRQ
context -- no sleeping, no allocating -- and may run on two CPUs at once
when its vector moves, so of the device it touches registers, atomics and
IRQ-safe locks. The handle unregisters on drop: keep it in a lock inside the
device, and have `quiesce` take it out and drop it after the lock.

**A block driver** depends on the `block` crate, implements
`block::BlockDriver` and calls `block::register_driver(name, dev)`. Calls
arrive from any task on any CPU, several at once: what one needs exclusively
sits behind a lock in the device. A buffer is whole sectors and never empty,
and the device may DMA straight into it.
`fn is_async(&self) -> bool { true }` adds `submit`/`kick`.

**A NIC** depends on the `net` crate and implements `net::NetDriver` with
its rings as `type Tx` / `type Rx`.

- The rings stay the init function's until `net::register(name, mac, dev,
  tx_ring, rx_ring)` hands them over, which comes last, because from there
  on the driver is called. If `register` refuses, it leaks the rings rather
  than dropping them -- the hardware is running on them -- and the driver
  quiesces.
- The transmit ring then lives *inside* the device's transmit lock, next to
  the queue it drains, and the receive ring behind the receive pass's
  `RxContext`: `flush_tx` and `process_rx` are lent them as `&mut`, nothing
  else can reach them, and the interrupt handler gets registers and atomics
  and no more. What a state dump needs of a ring the poll publishes in
  atomics.
- A frame from `TxQueue::dequeue()` holds the DMA buffer the hardware is
  about to read: keep it in the ring's `Vec<Option<Frame>>` shadow until the
  hardware is done, then hand it to `stack.done(frame)` -- the only way out
  of `flush_tx` a frame has. Never drop it there: that is a free with a
  spinlock held, and the net layer releases it off the lock instead.
- On receive, `Frame::alloc_rx(max)` gives a frame of `len() == 0`; post its
  `data_phys()`, and once the hardware has filled it `set_len(received)` and
  hand it up with `RxQueue::enqueue`, or gather a `FrameQueue` and `deliver`
  it under one lock.
- Consuming what the device wrote needs a read barrier *after* the
  index/OWN check and before the payload reads: an address-independent
  pair of loads is not ordered by a control dependency on arm64. See
  `virtio::Queue::take_used`, whose comment spells it out, nvme `read_cqe`
  and r8168 `harvest`.

`igb` is the only one of the three hardware NIC drivers that runs in QEMU
(`--nic igb`, see [Tests and gates](testing.md#the-hardware-nic-drivers));
`r8168` and `r8125` have no emulation at all, so keep their register
sequences as they are.
