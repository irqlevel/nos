---
title: "Serving an NVMe disk over UDP with zero copies"
description: "netblk serves an NVMe disk over UDP, and the disk DMAs straight into the frame the NIC sends. How the data moves, and the two lost ticks the numbers exposed."
date: 2026-09-13
tags: [storage, networking, modules, performance, rust, hardware]
---

netblk is a loadable module that serves an NVMe disk, or a partition of one, over UDP. The CPU never copies the data. The disk DMAs a read straight into the frame the NIC then transmits. It DMAs a write out of the frame the NIC received it in, and that same frame goes back as the reply. The CPU only writes 72 bytes of headers.

netblk landed on September 12 ([9abe257](https://github.com/irqlevel/nos/commit/9abe257)). By the evening of the 13th it answered 111,100 random 1 KiB reads a second on the AX41, which is all that server's gigabit link can carry. Getting there took two fixes in the scheduler.

```text
netblk start <disk> <port> [ro] [nic=eth0] [mtu=1500] [cpu=N] [poll=us]
netblk list
netblk stop <port>|all
```

## Why no copy is needed

Zero-copy here means that no CPU instruction reads or writes the payload. The devices move it by DMA, and the CPU touches only the headers. Two facts make that possible on ordinary hardware.

Every NIC driver in nos receives into 2 KiB frames from the kernel's frame pool and puts the packet at the frame's first byte. A pool frame sits inside one page, and its data is 8-byte aligned. Every header in front of netblk's payload is a whole number of 4-byte dwords.

An NVMe command describes its memory with PRP entries (physical region pages), and the first one may start at any dword-aligned address. So a received payload is already a buffer the disk can DMA from. A fresh pool frame that the disk has read into, behind room for the headers, is already a packet ([lib.rs](https://github.com/irqlevel/nos/blob/master/src/rust/modules/netblk/src/lib.rs)):

```rust
OP_READ => {
    // ...
    /* Straight into the frame, behind the room for the headers */
    (IO_READ, frame.data_phys() + REPLY_HDRS as u64, s.length >> sh.sector_shift)
}
OP_WRITE => {
    // ...
    /* Straight out of the frame, where the NIC put the data */
    (IO_WRITE, frame.data_phys() + s.data_off as u64, s.length >> sh.sector_shift)
}
```

A datagram carries whole sectors, as many as fit behind 58 bytes of IP, UDP and netblk headers, which makes two 512-byte sectors at an MTU of 1500. The UDP checksum is left at 0, which IPv4 allows to mean "not computed". Computing it would read every byte the disk just wrote.

## The path of a request

```text
NIC --DMA--> pool frame
               |  receive softirq: on_frame parses it, takes a slot
               v
        [requests ring]
               |
               v
worker, pinned to one CPU: every queued I/O to the disk, one doorbell
               |
               v
NVMe --DMA--> into the frame (a read) / out of the frame (a write)
               |  completion interrupt: on_disk_done
               v
          [done ring]
               |
               v
worker: headers in front of the data, replies batched
               |  one TX lock, one doorbell
               v
NIC <--DMA-- the frame
```

The NIC writes a datagram into a pool frame, and the receive softirq picks it up. A softirq is the deferred half of an interrupt, and in nos it runs as a task on each CPU. The softirq calls netblk's listener, `on_frame`, which parses the request, takes one of the instance's 256 slots and pushes the slot onto a ring. A write keeps its frame in the slot. A read gets a fresh frame from the pool when the worker submits it.

The worker is a task pinned to one CPU. It hands every queued I/O to the disk without ringing the doorbell. A doorbell is the register write that tells a device it has new work. It crosses the bus, and under a hypervisor it costs a VM exit, so the worker rings it once per batch (`Disk::kick`).

The disk reports finished commands in its completion queue, a ring the driver acknowledges with a doorbell of its own. The NVMe interrupt handler used to pay one doorbell per completion. Now it takes up to 32 completions and acknowledges them with one write. Then it calls netblk's callback, which runs in interrupt context and so does almost nothing:

```rust
extern "C" fn on_disk_done(ctx: *mut u8, status: i32) {
    let slot = ctx as *mut Slot;
    unsafe {
        (*slot).status = if status == 0 { ST_OK } else { ST_IO };
        /* Read before the push: the slot is the worker's the moment it is on
           the ring */
        let sh = &*(*slot).shared;
        /* Cannot fail: the ring has room for every slot */
        sh.done.push(slot as usize);
        sh.event.signal();
    }
}
```

The worker writes each reply's headers in front of the data and hands the replies to the NIC as one batch, under one lock and with one doorbell. Three lockless rings join the pieces: free slots, requests and completions. Each one is Vyukov's bounded queue, with one compare-and-swap per operation and room for every slot. The NIC side and the disk side meet only in these rings, so nothing on the path waits for a lock the other side holds.

## Polling, but never into idle

With nothing to do, the worker blocks on an `Event`. Anyone can signal it, an interrupt handler included, and a signal wakes a blocked worker with an IPI. But a wakeup costs an interrupt, an IPI and a context switch. So while completions are due, the worker polls for 50 µs before it blocks. With `poll=` it polls for new requests too.

Two other ways to poll came first, and both lost ticks. A plain yield handed the CPU to the idle task, which halts it until the next interrupt that CPU takes. When the disk's and the NIC's interrupts landed on other CPUs, that meant the 10 ms tick. And a completion that finds the worker runnable rather than blocked sends no IPI. With the client keeping one request in flight (a window of 1), that gave 1,700 IOPS and a p99 of 9.4 ms.

A plain spin failed differently. The softirq tasks share the worker's CPU. When the tick preempted one of them in the middle of the transmit path, the spinning worker kept it off the CPU until the next tick, and the queued replies waited with it. At windows of 32 and 64 that cut netblk from 100,000 IOPS to 30,000. So the kernel grew `YieldToRunnable()`, which gives the CPU to any other runnable task and never to the idle one.

## What the kernel had to grow

netblk is an ordinary [loadable module](/docs/modules#netblk), but the kernel's module interface lacked four things:

- **A UDP listener handed the frame itself.** A listener used to get a look at a datagram's bytes. Now `retain()` turns the lent frame into a `NetFrame` that the module may keep, hand to a disk, transmit or release anywhere.
- **virtio-net receiving into pool frames.** Its ring was sixteen static buffers, reposted on release into a queue only the receive softirq may touch, so no frame could leave the receive path. Now it holds up to 128 pool frames and keeps 32 transmits in flight instead of 8.
- **Batched transmit.** A run of frames goes to the driver under one lock and one flush.
- **An asynchronous block path.** `kcore::block::Disk::submit` takes a `BlockIo` (sectors, a physical address and a completion callback) and never blocks. A full queue answers `SubmitError::Busy`. A partition forwards the I/O, shifted onto its disk. NVMe keeps eight command IDs back for its synchronous path, so a server that fills the queue cannot starve a filesystem on the same disk.

Most of this was C++ at first, and within a week the network and block layers [moved to Rust](/blog/cpp-to-rust-layers). On September 19 netblk moved onto the typed listener and the `netwire` frame formats ([b55366b](https://github.com/irqlevel/nos/commit/b55366b)), and its `unsafe` sites went from 29 to 14. The old and the new module got the same eight requests under a packet dump, and their replies matched byte for byte.

## The protocol, the client and the test

Requests go to the instance's UDP port, and every field is big-endian. A request and its reply share a 30-byte header:

```text
offset size field
     0    4 magic    0x4E424C4B, "NBLK"
     4    1 version  1
     5    1 op       1 INFO, 2 READ, 3 WRITE, 4 FLUSH; a reply's has 0x80 set
     6    2 flags    bit 0: FUA, a write through the disk's cache
     8    8 cookie   the client's, returned in every reply
    16    8 offset   bytes into the device, whole sectors
    24    4 length   bytes, whole sectors
    28    2 status   0 OK, 1 bad request, 2 out of range, 3 I/O error,
                     4 busy, 5 read-only
```

INFO returns the device's size and limits. A READ may ask for up to 16 datagrams' worth and is answered one datagram at a time. A WRITE carries one datagram of data and is answered once the disk has it. Every request is idempotent, so a client that hears nothing simply asks again.

[scripts/netblk.py](https://github.com/irqlevel/nos/blob/master/scripts/netblk.py) is the client, with `info`, `read`, `write`, `flush`, `verify` and `bench`. Its `--window` sets how many requests stay in flight.

[scripts/netblk-test.py](https://github.com/irqlevel/nos/blob/master/scripts/netblk-test.py) boots the kernel in QEMU with NVMe disks, loads the module over the [UDP shell](/docs/udp-shell) and works it from outside. Written data must read back, and the kernel's own `diskread` must see it too. Bad requests must be refused, and `stop` and `rmmod` must leave nothing behind. Since September 19 the test serves a partition as well ([977c2d5](https://github.com/irqlevel/nos/commit/977c2d5)). It runs on x86-64 and arm64, and with `--nic igb` over QEMU's 82576, whose driver is the one the AX41's I210 runs.

## The numbers, and two lost ticks

The first numbers came from x86 under QEMU/KVM, limited by QEMU's user-mode network and a Python client. Random 1 KiB reads ran at 7,400 IOPS at a window of 1, 19,800 with `poll=1000` and 99,200 at a window of 64. The first commit also recorded two loose ends. On arm64 under TCG, about one wakeup in a hundred took a whole tick. On x86, the client's p99 jumped between 0.2 and 9 ms from run to run while the server's stayed under 400 µs, so that tail had to lie outside netblk.

The same morning netblk ran on the AX41, a Hetzner server with an Intel I210 on a gigabit link. Most requests took 25 µs of service time, and one in a hundred took 9 to 10 ms.

The cause was in the scheduler. The tick and the IPI handler ended in `Schedule()`, which could give the CPU to the idle task even when the interrupted task could still run. The idle task only halts, and nobody sends a runnable task an IPI, so the worker sat out the tick. Now those handlers end in `Preempt()`, which keeps a task that can still run ([a171a86](https://github.com/irqlevel/nos/commit/a171a86)). That was the arm64 mystery too: a window of 1 went from 675 IOPS to 3,000.

A tail remained that the server could not see. At windows of 16 and 64 the client's p99 was 10 ms, while the server answered every request within half a millisecond. A capture showed the replies stopping for 2 to 10 ms some 80 times a second, and nearly every silence ended on one CPU's 10 ms tick. Pings stalled too. When a silence ended, the reply to a ping sent during it left first, and netblk's replies followed a disk round trip later, so the requests had been waiting in the NIC's receive ring.

This time the scheduler was dropping a reschedule. An interrupt that raises the receive softirq sends its CPU an IPI to switch to the softirq task. When that IPI landed on the idle task inside a spinlock, the reschedule was lost, and the CPU halted with the softirq task runnable. Now the reschedule waits until preemption is back on ([73bad36](https://github.com/irqlevel/nos/commit/73bad36), and [Scheduler](/docs/scheduler#a-reschedule-that-finds-preemption-off) for the details). Here are the same tests on the AX41 a few hours apart, with a client in the same data centre ([43c53ab](https://github.com/irqlevel/nos/commit/43c53ab)):

| AX41, I210, random 1 KiB, 5 s each | before | after |
|---|---|---|
| reads, window 16 | 33,400 IOPS, p99 7.4 ms | 52,200 IOPS, p99 0.39 ms |
| reads, window 64 | 39,100 IOPS, p99 9.8 ms | 111,100 IOPS, p99 0.65 ms |
| writes, window 64 | 25,400 IOPS, p99 9.9 ms | 110,500 IOPS, p99 0.95 ms |
| server's service time, p99.9 | 7.2 ms | 0.47 ms |
| pings during the load, p99 | 8.0 ms | 0.58 ms |

A gigabit link carries about 110,000 such replies a second, so that is the ceiling. The read rate that had looked like the client's limit and the write rate that had looked like the disk's were both this stall. So was the x86 tail from the first day: it was outside netblk, in the kernel's receive path.

## And Linux

We have not run a Linux target side by side, so this compares designs only ([netblk](/docs/netblk#and-linux) has more). NBD's server is a user process that copies both ways. AoE, ATA over Ethernet, is closer in shape, but its target is a user process too. Linux's NVMe-over-TCP target copies the data it receives out of the socket buffers, while over RDMA the NIC places the data itself.

Zero-copy receive in general needs a NIC that splits headers from payload. netblk does without one, because it controls both the frame layout and the protocol.

## Limits

One datagram carries one I/O of 1 KiB at an MTU of 1500, and a disk with 4 KiB sectors is refused, because one sector would not fit in a datagram. Only NVMe has the asynchronous path so far. There is no authentication, and a READ is a reflector: about 70 bytes in, up to 16 datagrams of 1 KiB out, to whatever address the request claims. Serve a disk read-only and filter the port on any network you do not trust.
