# netblk: a disk served over UDP, zero-copy

`netblk` is a loadable module that serves a disk -- an NVMe disk, or a
partition of one -- to the network as a block device, over UDP, without the
CPU ever copying the data. A read is DMA'd by the disk straight into the frame
the NIC then transmits; a write is DMA'd by the disk straight out of the frame
the NIC received it in, and that same frame goes back out as the reply. The
CPU writes 72 bytes of headers in front of the data and never touches the data
itself.

```
$ insmod /netblk.ko
$ netblk start nvme0 7000
netblk: serving nvme0 (256 MiB, read-write) on eth0 10.0.2.15:7000 -- 1024 bytes a datagram, worker on cpu 3
$ netblk list
port 7000: nvme0 (256 MiB, rw) on eth0 10.0.2.15, 1024 bytes a datagram (mtu 1500), worker on cpu 3, up 21 s
  requests 1246965, bad 0, refused 0 (busy 0), in flight 0 of 256: queued 0, at the disk 0, done 0
  reads 765567 (747 MiB), writes 481394 (470 MiB), flushes 3, I/O errors 0
  disk full 369 times, replies dropped 0, worker slept 192176 times
  service time us (arrival to reply): p50 75.7, p99 499.7, p99.9 704.5, max 225757.1
$ netblk stop 7000
netblk: stopped serving nvme0 on port 7000
```

and from any other machine:

```
python3 scripts/netblk.py 10.0.2.15 7000 info
python3 scripts/netblk.py 10.0.2.15 7000 read 0 1m image.bin
python3 scripts/netblk.py 10.0.2.15 7000 write 0 image.bin
python3 scripts/netblk.py 10.0.2.15 7000 bench --mode randread --window 64
```

## Using it

    netblk start <disk> <port> [ro] [nic=eth0] [mtu=1500] [cpu=N] [poll=us]
    netblk list
    netblk stop <port>|all

- `<disk>` is a name `disks` lists. It has to have the asynchronous block
  path (below), which today means NVMe and the partitions of an NVMe disk; a
  virtio disk is refused.
- `<port>` is a UDP port on `nic`, 1024 or above: below are the well-known
  ones, DHCP's 68 among them, which the kernel takes for each attempt and
  gives back after. One the UDP shell, DNS or any other listener already has
  is refused rather than taken over, and so is one another instance serves,
  on whatever NIC -- `stop` names an instance by its port.
- Served read-write, the device is claimed as a `blkload` write test claims it
  (`kcore::block::Disk::claim`): refused while a mounted filesystem, the disk log
  or another writer holds it -- or the disk it is a partition of, or one of its
  partitions -- and holding all of those off until `stop`. `ro` takes no claim
  and refuses writes and flushes.
- `mtu` is the path's, 576 to 2034 -- a frame from the kernel's pool is 2 KiB,
  Ethernet header included. It decides how much a datagram carries: whole
  sectors, as many as fit behind 58 bytes of IP, UDP and netblk headers -- two
  512-byte sectors at 1500, three at 2034. A disk whose sectors do not fit at
  all (4 KiB ones) is refused: carrying one would take several datagrams, and a
  copy to put them together.
- `cpu` pins the instance's worker; by default instances take the online CPUs
  from the highest down.
- `poll` keeps the worker polling for the next request this many
  microseconds after it has run out of work, instead of going to sleep at
  once: at a low queue depth the wakeup is most of what a request costs (see
  [Numbers](#numbers)). It burns the worker's CPU while the disk is idle,
  which is why it is 0 unless asked for; up to 100000.
- `list` shows each instance's counters. *Refused* are requests answered with
  an error on the spot, *busy* the ones refused because all 256 of the
  instance's slots were taken; *in flight* are the slots in use, *queued* on
  their way to the worker, *at the disk*, and *done* back from it; *disk full*
  counts the times the NVMe queue had no room for another asynchronous
  command (below), *worker
  slept* the times its worker ran out of work. The *service time* runs from a
  request's arrival on the receive path to its reply going to the NIC -- what
  this machine adds, the network not included; a flush's includes the disk
  writing out its cache.
- `rmmod netblk` stops every instance first. Stopping waits for whatever the
  disk still has in flight; whatever was queued and not yet at the disk is
  dropped unanswered, for the client to retry elsewhere.

There is no authentication, as with the [UDP shell](udp-shell.md): whoever can
reach the port can read, and unless `ro`, overwrite the disk. Only a datagram
addressed to the NIC's own IP address is answered, and never one that is
itself a reply -- so neither a broadcast nor a second server can start a storm
-- but a READ is a reflector all the same: some 70 bytes in, up to 16
datagrams of 1 KiB out, to whatever source address the request claims. Keep the
port off networks you do not trust.

`scripts/netblk.py` is a client: `info`, `read`, `write`, `flush`, `verify`
(random data written over a range and read back -- it destroys what is there)
and `bench` (`randread`, `randwrite`, `read`, `write`, with latency
percentiles). `--window` sets how many requests it keeps in flight.

`scripts/netblk-test.py` is the end-to-end test: it boots the kernel in QEMU
with an NVMe disk, loads the module over the UDP shell, and checks from outside
that what is written reads back -- also through the kernel's own `diskread` --
that bad requests are refused, and that `stop` and `rmmod` leave nothing
behind. `--arch aarch64` does the same on QEMU's `virt` board:

    docker run --rm --device /dev/kvm -v /root/nos:/root/nos -w /root/nos \
        nos-builder scripts/netblk-test.py [--arch aarch64]

`--nic igb` puts QEMU's igb (an 82576) in place of virtio-net, which runs the
kernel's Rust igb driver -- the one that drives the I210 on real hardware. It
needs QEMU 8.0 or later, which nos-builder's 6.2 is not; with the host's, in a
network namespace of its own so that its port forwards touch nothing of the
host's:

    unshare -n bash -c 'ip link set lo up && scripts/netblk-test.py --nic igb'

### Which network cards

netblk works over any NIC driver in the kernel, because what it relies on is
what every one of them does. A received frame comes from the frame pool and is
handed up whole, the driver refilling its ring with a fresh one -- so it can be
kept past the receive callback and released from anywhere. The NIC puts the
packet at the frame's first byte -- so the data lands dword-aligned, where the
disk can take it. And a transmit takes any frame, by its physical address and
length.

- **virtio-net**: its ring holds pool frames (below); the end-to-end test runs
  over it on both architectures.
- **igb** -- the Intel I210 of the AX41, the 82576 under QEMU: the same test
  passes over QEMU's igb (`--nic igb`), at virtio-net's rates.
- **r8125, r8168** -- the Realtek parts of the EX44 and the dev machine: the
  same receive and transmit paths as igb, by reading the drivers. QEMU emulates
  neither, so they are untested until netblk runs on those machines.

What differs on the hardware drivers:

- They are set up for standard frames -- igb with `RCTL.LPE` off, the Realtek
  parts with `RX_MAX_SIZE` at 1526 bytes -- so `mtu` above 1500 is for
  virtio-net only. The default is right for them.
- Their rings hold pool frames all the time, besides the up to 256 an instance
  keeps: 1024 for igb, 256 for each Realtek ring. The pool's default 4096 covers
  that; `netframes=N` raises it for more NICs or instances.
- A burst of replies bigger than the ring waits in the device's transmit queue
  (256 frames) and goes out as descriptors free up: each of these drivers
  raises the transmit softirq on a transmit completion.
- The link is a ceiling: at 1 KiB a datagram, 1 GbE (I210, r8168) carries some
  110 000 replies a second, about 108 MiB/s of reads; 2.5 GbE (r8125) some
  270 000. The AX41's I210 reaches it: 111 000 random 1 KiB reads a second,
  108.5 MiB/s, and as many writes ([numbers](#the-receive-path-lost-ticks-too)).

On a machine on the internet -- the Hetzner ones -- the port is open to
whoever can reach it, with no authentication, as with the UDP shell. Serve a
disk read-only unless its data can be lost, filter the port (Hetzner's
firewall), and mind that the device claim knows nothing of Linux md: a
partition that is an md member -- the AX41's `p2` and `p3` hold its `/boot` and
`/` -- looks free to it.

## How the data moves

```
  NIC -DMA-> frame ---- receive softirq: on_frame ----> [requests] --+
                                                                      |
                                              worker (pinned task) <--+
                                                |  one SQ doorbell a batch
                                                v
                                  NVMe -DMA-> / <-DMA- the frame's payload
                                                |  completion interrupt
                                                v
  NIC <-DMA- frame <-- worker: 72 bytes of headers, [done] <-- on_disk_done
                       one TX lock and doorbell a batch
```

- **A write** arrives in a frame from the kernel's frame pool. The listener
  (`NetDevice::ListenUdpFrames`) hands the module the frame itself rather than
  a look at its bytes; the module keeps it, with a reference of its own, and
  queues the request. The worker gives the disk an I/O whose PRP entry points
  at the data inside that frame. When the disk is done, the worker rewrites the
  frame's first 72 bytes as the reply -- addresses swapped, status in -- and
  transmits it.
- **A read** takes a frame from the pool on the worker's CPU and has the disk
  read into byte 72 of it. When the disk is done, the worker writes the 72
  bytes of headers in front of the data and transmits the frame.

It can be done without a copy here because of where a frame's bytes lie. An
NVMe PRP entry may begin anywhere that is dword aligned and a command of two
PRP entries may span two pages. A pool frame sits inside one page, its data
8-byte aligned; every header in front of netblk's payload is a whole number of
dwords, IP options included; and the payload, at most a few sectors, never
leaves the frame's page. So the payload of any datagram the NIC receives is
already a buffer the disk can DMA from, and a frame the disk has read into is
already a packet. (Linux has no such luck: an skb's payload lies wherever the
NIC put it, and handing it to a disk needs it page-aligned -- header/data split
in the NIC. See [below](#and-linux).)

For the same reason nothing computes the UDP checksum: it is left 0, "not
computed", which IPv4 allows. Computing it would read every byte the disk just
wrote, the one thing this path never does. What protects the data on the wire
is the Ethernet CRC of each hop.

### What connects the pieces

- **Three lockless rings** (the kernel's `LocklessRing`, one compare-and-swap
  an operation): free slots; requests, from the receive softirq to the worker;
  completions, from the disk's interrupt handler to the worker. They are as
  large as the instance has slots, so a push never fails. Nothing on the path
  takes a lock the other side holds: the NIC side and the disk side meet only
  in the rings.
- **Slots**: 256 an instance, allocated once, a cache line each so that two
  CPUs working on neighbours never share one. A request is a slot from the
  moment the receive path takes it to the moment its reply is queued; a read
  takes a slot for each datagram of its answer.
- **The worker**, a task pinned to one CPU (`netblk/<port>` in `ps` and
  `top`), submits what is queued with no
  doorbell and rings it once for the batch (`kcore::block::Disk::kick`), then
  hands every finished reply to the NIC in one `SubmitTxBatch`: one lock, one
  doorbell. A doorbell is a write across the bus -- and under a hypervisor, an
  exit.
- **Waking it** is the kernel's `Event`: the worker blocks, out of the
  scheduler's walk; the receive path and the disk's interrupt handler signal
  it, and a signal that finds it blocked sends its CPU an IPI. While it is owed
  completions it polls for them instead, for 50 µs, because a wakeup costs an
  interrupt, an IPI and a context switch; with `poll=` it goes on polling for
  the next request too. Polling gives the CPU to any other task runnable there
  and never to the idle task (`YieldToRunnable`). A plain yield would: idle
  halts the CPU until the next interrupt that CPU itself takes -- the tick,
  when the disk's and the NIC's land elsewhere -- and a completion finding the
  worker runnable rather than blocked sends it no IPI. A plain spin starves
  the softirq task on the same CPU instead: one the tick preempted halfway
  through the transmit path held it until the next tick, and every reply
  that had not fit the NIC's ring waited with it.
- **No allocation, no atomics on the datapath's counters**: frames come from
  the pool's per-CPU caches, and every counter has one writer (the receive
  softirq, which runs on one CPU at a time, or the worker), so it is a load and
  a store rather than a locked instruction.
- **Stopping** unregisters the listener (which waits out a receive callback
  still running), tells the worker, waits for it to drain what the disk has,
  and then waits once more -- for an interrupt handler on some CPU still on its
  way out of the module's completion callback -- by sending every CPU an IPI
  and waiting for each (`kcore::cpu::synchronize`): a handler runs with
  interrupts off, so the IPI is only taken after it. Only then do the rings,
  the event and the slots go, and after them the module's code.

### What it stands on

netblk is an ordinary module, and everything it does goes through the kernel
interface any module has ([Loadable modules](modules.md)). What it needed that
the kernel did not have:

- **Frames a listener can keep.** A UDP listener is lent the frame itself
  (`kcore::net::Lent`), not a copy of what is in it; it may take a reference
  (`retain`, a `NetFrame`) and keep it past its return, hand it to a disk,
  transmit it, or release it from any context.
  virtio-net's receive ring used to be sixteen static buffers, each reposted by
  its frame's release -- into a queue only the receive softirq may touch, so a
  frame could never leave the receive path. Its ring now holds frames from the
  pool (up to 128, each posted as a header descriptor and a frame descriptor),
  refilled from the pool as they are handed up, as the Rust NIC drivers
  already did; and it keeps 32 transmits in flight instead of 8.
- **Batched transmit.** `kernel_net_submit_tx` (`kcore::net::TxBatch`): a run
  of frames under one lock and one `flush_tx`. It may be called with interrupts off; what the driver
  has finished with is then left for the transmit softirq to release, since a
  frame from the allocator is freed through a TLB shootdown that a CPU with
  interrupts off cannot take part in.
- **Asynchronous block I/O.** `kcore::block::Disk::submit` takes a
  `BlockIo` -- sectors, a physical address, a completion callback -- and
  never blocks: a full queue is `SubmitError::Busy`, to be tried again after
  a completion. A partition forwards it, moved onto its disk. The NVMe driver
  implements it with the command IDs its synchronous path uses, calling the
  callback from its interrupt handler, and rings its doorbell only on
  `kick` when asked to. Its interrupt handler now acknowledges a batch of
  completions with one CQ doorbell instead of one per completion -- before any
  of their command IDs can be reused, which is what keeps the completion queue
  from ever needing more room than it has. Eight of the IDs are kept from the
  asynchronous path for the synchronous one: a server keeping the queue full
  would otherwise take every ID back the moment it came free, and a filesystem
  on another partition of the disk would wait out its retries and fail.
- **An event** (`kernel/event.h`) a task can block on and anyone can signal,
  hard IRQ handlers included -- `WaitGroup` spins on `Schedule()` instead.
- **Polling that neither halts nor hogs the CPU.** `YieldToRunnable()` gives
  the CPU to another task runnable on it and returns at once when there is
  none, where `Schedule()` would hand it to the idle task.
- **For Rust**, in `kcore`: `ring::LocklessRing`, `sync::Event`,
  `task::yield_to_runnable`, `task::spawn_on_with` (the worker, pinned, holding
  the instance's state rather than a pointer to it), `cpu::synchronize`,
  `net::Nic` (a device to send and receive on, found by name) with a listener
  that owns its handler (`listen`, `UdpHandler`, `Lent`), `NetFrame::alloc_tx`
  and `TxBatch`, and `block::Disk::submit` and `kick`. The frame formats --
  a request taken apart, a reply's headers put in front of data the CPU never
  touches -- are the `netwire` crate's, which the network layer is built on
  too.

What is left of `unsafe` in the module is what those do not cover yet: a
slot travels the rings as a word, from the receive path to the worker, to the
disk's interrupt and back, and whoever takes it off a ring reaches it through
that word; and the disk's completion callback is a C function handed a
context. The frames themselves, the listener, the worker and the batch that
goes to the NIC are ordinary owned values.

## The protocol

UDP to the instance's port; every field big-endian. A request and its reply
begin with the same 30-byte header:

| Offset | Size | Field | |
|---|---|---|---|
| 0 | 4 | magic | `0x4E424C4B`, "NBLK" |
| 4 | 1 | version | 1 |
| 5 | 1 | op | 1 INFO, 2 READ, 3 WRITE, 4 FLUSH; a reply's is the request's with `0x80` set |
| 6 | 2 | flags | bit 0: FUA, a write through the disk's cache |
| 8 | 8 | cookie | the client's, returned in every reply |
| 16 | 8 | offset | bytes into the device, a whole number of sectors |
| 24 | 4 | length | bytes, a whole number of sectors |
| 28 | 2 | status | 0 in a request. A reply: 0 OK, 1 bad request, 2 out of range, 3 I/O error, 4 busy, 5 read-only |

What follows the header:

- **INFO** asks for nothing and is answered with 32 bytes: the device's size
  in bytes (u64), its sector size (u32), `max_io` -- the most one datagram
  carries -- (u32), `max_read` -- the most one READ asks for -- (u32), flags
  (u32, bit 0 read-only), how many requests the instance holds at once (u32),
  and a u32 kept for later.
- **READ** carries nothing; `length` is at most `max_read` (16 × `max_io`).
  It is answered one datagram per `max_io` bytes, each with its own offset and
  length and the data after the header; the client puts them together by
  offset.
- **WRITE** carries `length` bytes, at most `max_io`, and is answered by a
  header alone once the disk has the data.
- **FLUSH** is answered once the disk's cache has been written out.

A request refused as a whole -- out of range, misaligned, too long, or no
slots -- is answered by one header with the request's offset and length and
the status. A datagram with the reply bit set is dropped unanswered, whatever
else it says, and so is one not addressed to the NIC's own IP address. Busy is worth a retry a moment later: the instance holds 256
requests, and a READ takes one for each datagram of its answer.

Every request is idempotent, so a client need not know whether a lost answer
means a lost request: it sends the request again. `netblk.py` does, after a
quarter of a second, and sizes its READ window so that the reads it keeps in
flight fit in the instance's slots.

## Numbers

`scripts/netblk.py bench` on the host, against the x86 kernel under QEMU/KVM
through QEMU's user-mode network -- which, with a Python client, is what limits
the rates -- and QEMU's NVMe disk, whose own read latency from inside
(`blkload nvme0 randread qd=1`) is 22 µs. The latencies are the client's:

| | window | IOPS | MiB/s | latency p50 / p99 |
|---|---|---|---|---|
| random 1 KiB reads | 1 | 7 400 | 7.2 | 54 / 191 µs |
| random 1 KiB reads, `poll=1000` | 1 | 19 800 | 19 | 46 / 79 µs |
| random 1 KiB reads | 2 | 21 500 | 21 | 65 / 109 µs |
| random 1 KiB reads | 8 | 61 500 | 60 | 103 / 199 µs |
| random 1 KiB reads | 64 | 99 200 | 97 | 520 / 1339 µs |
| random 1 KiB writes | 8 | 62 100 | 61 | 105 / 197 µs |
| random 1 KiB writes | 64 | 93 800 | 92 | 558 / 1410 µs |

The server's own service time over all of those, 1.2 million requests: p50
88 µs, p99 573 µs, p99.9 835 µs.

At a window of 1 the worker is asleep by the time each request arrives, and
waking it -- an IPI to a CPU that has halted, which under a hypervisor means a
vCPU to wake -- is most of what the request costs: its service time is p50
56 µs, and 24 µs with `poll=1000`. The client's p99 there used to change
from one run to the next, anywhere from 0.2 to 9 ms, while the server's
stayed under 400 µs: the tail lay outside netblk -- the kernel's own UDP echo
(`netload`, answered from the receive softirq, netblk not involved) showed it
too, p99.9 9.9 ms. It was the scheduler's, in the receive path
([below](#the-receive-path-lost-ticks-too)). (When the worker yielded
instead of polling, the tail was the server's own: its CPU halted while the
disk worked, and a window of 1 ran at 1 700 IOPS, p99 9.4 ms.)

On arm64 the test runs under TCG, and there the worker used to lose a whole
tick now and then -- the server's p99 was 10.7 ms -- which x86 under KVM does
not show. The cause was the scheduler's. The tick and the IPI handler ended in
`Schedule()`, which hands the CPU to the idle task when nothing else is
runnable, even while the task the interrupt landed on can still run; the idle
task only halts, and a worker that is runnable rather than blocked gets no IPI
from the requests and completions meant for it, so it waited for the next
tick. They end in `Preempt()` now, which keeps a task that can still run (see
[Scheduler](scheduler.md) -- the AX41 lost a quarter of a polling worker's CPU
the same way). One run of each, 5 seconds, before and after:

| arm64 under TCG, random 1 KiB reads | before | after |
|---|---|---|
| window 1 | 675 IOPS | 3 000 IOPS |
| window 16 | 10 500 IOPS | 29 900 IOPS |
| window 64 | 41 500 IOPS | 57 300 IOPS |
| the server's p99 over those three | 10.7 ms | 0.8 ms |
| window 16, `poll=100000` | 34 500 IOPS, p99.9 10.2 ms | 49 000 IOPS, p99.9 0.3 ms |
| the idle task's share of that worker's CPU | 24% | 0 |

The synchronous path still pays the tick on every command: `blkload nvme0
randread qd=1` measures 10 ms, because `WaitGroup` waits by yielding, its CPU
halts in the idle task, and on that board nothing but the tick wakes it.

### The receive path lost ticks too

That left a tail the server's service time cannot see. On the AX41, with the
fix above, a window of 16 or 64 still had a client p99 of 10 ms while the
server answered every request within half a millisecond. A capture on the
client's NIC showed the replies stopping for 2 to 10 ms some 80 times a
second with 63 requests outstanding, nearly every silence ending on the same
10 ms grid -- one CPU's tick -- and pings stalling with them, answered from the
receive softirq with netblk not involved. At the end of a silence the pong
for a ping sent in the middle of it left first, and netblk's replies a disk
round trip later: the requests had been sitting in the NIC's receive ring.
The reschedule that was to run the receive softirq had been dropped, because
it landed on the idle task inside a spinlock, and the idle task halted (see
[Scheduler](scheduler.md#a-reschedule-that-finds-preemption-off)). It is
deferred now rather than dropped. Under QEMU/KVM with virtio-net and 12
vCPUs, where it showed as plainly -- 5 seconds each, the last row from a
capture of a fourth run at window 64:

| QEMU/KVM, virtio-net, 12 vCPUs, random 1 KiB reads | before | after |
|---|---|---|
| window 1 | 10 700 IOPS, p99 0.36 ms, max 10.6 ms | 19 800 IOPS, p99 0.07 ms, max 1.4 ms |
| window 16 | 33 000 IOPS, p99 8.4 ms | 96 000 IOPS, p99 0.3 ms |
| window 64 | 89 700 IOPS, p99 6.3 ms, p99.9 10.2 ms | 105 600 IOPS, p99 1.2 ms, p99.9 1.5 ms |
| requests over 5 ms, all four runs | 14 864 of 1.1 million | 0 of 1.6 million |
| reply gaps over 2 ms with requests outstanding | 110, 103 of them on one 10 ms grid | 1 |

And on the AX41 itself, the same tests before and after the fix, a few hours
apart: a `scripts/netblk.py` client on another machine in the same data
centre (0.3 ms away), a partition of one of the NVMe disks served on the
I210, the benchmarks against a read-only instance with no `poll`, the
capture against one with `poll=100000`, 5 seconds each:

| AX41, I210, random 1 KiB unless said | before | after |
|---|---|---|
| reads, window 1 | 2 900 IOPS, p99 0.51 ms | 3 000 IOPS, p99 0.43 ms |
| reads, window 16 | 33 400 IOPS, p99 7.4 ms | 52 200 IOPS, p99 0.39 ms |
| reads, window 64 | 39 100 IOPS, p99 9.8 ms | 111 100 IOPS, 108.5 MiB/s, p99 0.65 ms |
| 16 KiB reads, window 16 | 100.7 MiB/s, p99 9.2 ms | 109.0 MiB/s, p99 2.6 ms |
| writes, window 64 | 25 400 IOPS, p99 9.9 ms | 110 500 IOPS, 107.9 MiB/s, p99 0.95 ms |
| the server's service time, p99 / p99.9 | 0.37 / 7.2 ms | 0.12 / 0.47 ms |
| window 64 at the client's NIC: round trips over 2 ms | 12.7% | none |
| reply gaps over 2 ms with requests outstanding | 397 | none |
| pings during that load, p99 | 8.0 ms | 0.58 ms |

The window-64 rate that had looked like the Python client's limit, and the
write rate that had looked like the disk's, were this stall: without it both
run at the link's 110 000 a second. A write with FUA (4.3 ms) and a flush
(1.4 ms) are the disk's own, and did not move. Four clients at once for 30
seconds -- two reading, two writing and verifying what they wrote -- got 1.8
million requests through, where before they got 0.9 million.

Re-run after the igb interrupt rework -- a queue vector that reads no
register, a poll that goes round again under load, the frame pool up before
the NIC ([real hardware](real-hardware.md#hetzner-ax41-1-ltd-dedicated-server))
-- every number held within a few percent: reads at window 64 110 400 IOPS,
p99 0.79 ms; writes 110 500, p99 0.92 ms; round trips at the NIC p99 0.69
ms, none over 2 ms, no reply gaps; pings under the load p99 0.57 ms; four
clients, 1.8 million requests again. At 110 000 requests a second netblk
stays below the rate where the poll repolls, and the link is still its
ceiling.

## Limits

- One datagram, one I/O: 1 KiB at an MTU of 1500 with 512-byte sectors. No
  jumbo frames -- the kernel's frames are 2 KiB -- so a disk formatted with 4
  KiB sectors cannot be served without a copy, and is refused.
- NVMe and its partitions only: the asynchronous block path has no other
  implementation yet.
- The NVMe driver has one I/O queue, 63 commands deep: 55 open to
  asynchronous I/O -- every instance's on the disk -- and the rest kept for
  the synchronous path. A filesystem's batch of blocks is synchronous and
  takes up to half the queue while it lasts, so on a disk with a mounted
  filesystem that is being written netblk meets Busy sooner. netblk's queue
  in front of it is what absorbs a burst.
- IPv4, and no fragments -- there is no reassembly, and a datagram is never
  bigger than the MTU anyway. Only datagrams to the NIC's own address are
  answered: no broadcast, no multicast.
- No UDP checksum, no authentication; nothing keeps two clients off each
  other's blocks.

## And Linux

Linux has several ways to serve a block device over a network, none of them
this one:

- **NBD**: the kernel has the client (`nbd`); the server, `nbd-server` or
  `qemu-nbd`, is a user process reading and writing the device through the page
  cache and a TCP socket -- copies both ways.
- **AoE**, ATA over Ethernet, is the closest in shape: raw Ethernet frames,
  one frame one ATA command of whole 512-byte sectors -- two of them in a
  1500-byte frame, as here. The kernel has the initiator (`aoe`); the target,
  `vblade`, is a user process. (An in-kernel target, kvblade, never left its
  own tree.)
- **NVMe over Fabrics**: the kernel has targets (`nvmet`) for TCP, RDMA and
  Fibre Channel. Over TCP, sends are zero-copy -- the data's pages are attached
  to the socket buffer (`MSG_SPLICE_PAGES`) -- but received data is copied out
  of the socket buffers into the I/O's pages. Over RDMA the NIC places the data
  itself, and with `p2pmem` the transfer can bypass host memory altogether,
  going between the NIC and a controller memory buffer on the NVMe drive: the
  real thing, for hardware that has it.
- **Zero-copy receive** in general -- `io_uring`'s zero-copy receive (6.15),
  device memory TCP (6.12) -- needs the NIC to split headers from payload and
  land the payload page-aligned in buffers the application supplied: a packet's
  payload is otherwise wherever the NIC put it, among other packets' bytes.
  netblk gets away without that because it owns both the frame layout and the
  protocol: a datagram carries whole sectors behind headers that are whole
  dwords, and a whole datagram fits in a page.
