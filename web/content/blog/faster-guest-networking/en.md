---
title: "Guest networking from 5.7 seconds to 2.8"
description: "Posted interrupts, a one-shot timer, receive interrupt moderation, offloads and mapped rings, measured on bare metal, and two ideas that did not help."
date: 2026-10-22
tags: [hypervisor, networking, performance, x86-64, rust]
---

Here is the benchmark. On the EX44, a Hetzner server with an Intel i5-13500 where nos runs directly on the hardware and its hypervisor uses Intel VT-x, we start two Linux guests with two CPUs each. One guest sends the other 500 MB over TCP through the switch in the `hv` module, one kilobyte per write (`dd | nc`). When this work began, on the evening we merged the Intel round of changes ([3912397](https://github.com/irqlevel/nos/commit/39123972ca3197b653636c3373cf14594ac4b3ca)), the transfer took 5.72 s on average. By the next evening it took 2.72–2.83 s ([2a20db5](https://github.com/irqlevel/nos/commit/2a20db5505db5054c06655e04f6330b634db03b2)). Along the way we added a second test: BusyBox `httpd` in one guest serves a 100 MB file to the other five times in a row. It went from 1.63–1.68 s without the offloads to 0.29 s, or 1.8 GB/s.

Each step is a commit whose message records the measurement: the runs, the spread and the comparison. All figures come from the EX44's real hardware unless stated otherwise. The gates, the test suites every change has to pass, ran under nested KVM on the same machine. Two of the steps were ideas that turned out no faster. They went into the documentation, not into the code.

## The whole day in one table

| Commit | What it removed | Result |
|---|---|---|
| [89a4c5a](https://github.com/irqlevel/nos/commit/89a4c5a92bc5fbb86d5338edfc52238047ed19ce) posted interrupts | the exit needed to hand a guest CPU an IPI or an MSI | TCP: 4.85 s on average, down from 5.72 |
| [e98a4de](https://github.com/irqlevel/nos/commit/e98a4de271a70ad4868c4e5f3daec9c1dbac71be) halt polling, taken apart | nothing; not kept | 6.27 and 6.48 s, against 3.94 and 5.08 without it |
| [dc416b9](https://github.com/irqlevel/nos/commit/dc416b9100208afbc2530be4b46d1ba267286e26), [415d29d](https://github.com/irqlevel/nos/commit/415d29d689e96f9e40755d96ad231fc05d51da43) a one-shot timer | up to 10 ms of lateness in a guest's timers | 100 × `usleep 1000` in 0.16 s, down from 0.99 |
| [4696f48](https://github.com/irqlevel/nos/commit/4696f48e2bae699f80bc6fbc04a50e242d9a2c00) TCP polling backs off | the extra wakeups that a real 1 ms sleep causes | idle nos: 741 wakeups a second, down from 2,220 |
| [4445a2d](https://github.com/irqlevel/nos/commit/4445a2d892a63fd9383c58d6b6a608d1c3f8a6dd) receive interrupts at least 100 µs apart | one interrupt per frame | TCP: 4.01 s, down from 4.88 |
| [9595afb](https://github.com/irqlevel/nos/commit/9595afb6773c8f2bb479a82eaa133fd9a9b3c84b), [f37c5a6](https://github.com/irqlevel/nos/commit/f37c5a66e0f9ec9746051bccadb1ce35c4e17619) offloads in both directions | the guest's work on each frame, then some forty frames per send | `httpd`: 0.47 s, down from 1.63–1.68 |
| [23fdb63](https://github.com/irqlevel/nos/commit/23fdb63ebf6849edf5a8ecc784ea79991f71c859) rings kept mapped | mapping a page and flushing its TLB entry on every ring access | TCP: 2.72 s, down from 4.15; `httpd`: 0.29 s, down from 0.47 |
| [78042cf](https://github.com/irqlevel/nos/commit/78042cfb294cb40d5b7bd35d17a49212d6e3023a) a moved moderation window wakes the first CPU | a 10 ms reply every few hundred pings | 2,400 pings, none slower than 0.28 ms |
| [8aa5167](https://github.com/irqlevel/nos/commit/8aa5167d3d072d4d981b61046cf79f71094eb039) a separate task for the NIC | nothing; not merged | `httpd`: 0.42–0.59 s, against 0.21 without it |

## Posted interrupts: an IPI without an exit

With APIC virtualization, a VT-x CPU delivers interrupts to a guest in x2APIC mode and handles the guest's EOIs by itself. Neither needs a VM exit (the CPU leaving the guest and handing control back to the hypervisor). The Intel round turned this on only for guests that asked for it (`apicv`). The reason was IPIs from other CPUs and MSIs from devices: they still went into the target vCPU's mailbox in the hypervisor. To receive one, a CPU running its guest had to be kicked out of it. That costs an IPI plus an exit, which is more than the EOI exit it saved. With `apicv` the guests exited 25–50% less often, yet the test still took 5.88 s on average, against 5.70 without it.

Posted interrupts remove that kick ([89a4c5a](https://github.com/irqlevel/nos/commit/89a4c5a92bc5fbb86d5338edfc52238047ed19ce)). Every guest CPU gets a 64-byte descriptor, [`PostedInterrupts`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/vmx.rs), that its VMCS points to (the VMCS is the structure that describes a guest CPU to VT-x). The descriptor holds one request bit per interrupt vector and an ON bit that says a notification is pending. To send a fixed, edge-triggered interrupt, the sender sets the vector's bit:

```rust
    pub fn post(&self, vector: u8) -> bool {
        self.pir[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::SeqCst);
        self.control.fetch_or(PI_ON, Ordering::SeqCst) & PI_ON == 0
    }
```

If ON was clear and the target is running its guest, the sender sends the notification vector, 0xFC, to the host CPU the target runs on. That CPU moves the posted bits into VIRR, the virtual APIC's interrupt request register, and delivers the interrupt inside the guest. Level-triggered interrupts, NMIs, INIT and start-up IPIs still go through the mailbox.

The cost landed in `hvarch`, the crate that drives the CPU's virtualization extension. Posted interrupts require every exit caused by an external interrupt to acknowledge that interrupt. Otherwise the host would take a notification meant for the guest. So the host's own interrupts now arrive at an exit already acknowledged, but not delivered anywhere. `hvarch` re-delivers each one by executing `int` with its vector through the host's IDT. It waits until it no longer borrows any per-CPU state, because the handler may switch tasks. This added six `unsafe` sites to `hvarch`, from 195 to 201. `hv`, the layer above it, still has four.

We ran the test ten times with posted interrupts and ten times without, alternating. With them it took 3.66–5.89 s, 4.85 on average; without them, 5.58–5.79 s, 5.72 on average. They were faster in 9 of the 10 runs, and the guests made 0.67–1.58 million exits instead of 2.3–2.6 million. Interrupt delivery by the CPU itself is now the default on every CPU that has posted interrupts (`noapicv` turns it off). A new built-in test guest, `posted`, makes sure it stays that way. Its second CPU sends the first a thousand IPIs while the first spins in a loop. On the EX44 the first CPU left its guest only 9 times for the whole thousand.

## Halt polling turned out to be accidental interrupt moderation

The Intel round had left us a puzzle. With halt polling, a vCPU whose guest has halted keeps watching for work for a short while before its task goes to sleep. A halted vCPU that watched its mailbox for 200 µs before sleeping had cut this transfer from 5.7 s to 3.9. An adaptive watch, the way KVM does it, gave 5–7 s. We took this apart with posted interrupts on, two runs per variant ([e98a4de](https://github.com/irqlevel/nos/commit/e98a4de271a70ad4868c4e5f3daec9c1dbac71be)). Real halt polling, which watches both the mailbox and the doorbell (the signal that wakes a vCPU) for 200 µs, was slower: 6.27 and 6.48 s, against 3.94 and 5.08 with no watch at all. It hands over each frame's interrupt immediately. So the receiver takes one frame per interrupt and acknowledges every frame or every second one: 265,000–277,000 ACKs instead of 111,000–151,000. The sender gets answers that often, so it sends more frames, and smaller ones.

The watch that had looked like a win watched only the mailbox. The switch rings the doorbell when there is device work to do. On the receiving guest's first CPU, while it was halted, that doorbell went unanswered until the 200 µs ran out. So frames waited and then went into the ring together: 18.9 frames per interrupt, 41,000 ACKs, 3.94 s. In effect this was interrupt moderation (holding an interrupt back so that it covers several frames), done by accident. When the vCPU slept through the same wait instead of spinning, the transfer took 12.5–14.6 s, because a sleep on the host ended only at its next 100 Hz tick. The numbers asked for two things: moderation on purpose, and a timer finer than the tick.

## A timer finer than the tick

Each CPU's timer (the local APIC timer on x86-64, the generic timer on arm64) used to tick periodically at 100 Hz. A task blocked until a deadline ran at the first tick after it, so a 1 ms sleep took 10 ms. A halted guest's vCPU waits the same way for the next edge of its guest's timer, so `usleep 1000` in a guest took 10 ms. Now the timer is one-shot ([dc416b9](https://github.com/irqlevel/nos/commit/dc416b9100208afbc2530be4b46d1ba267286e26)). It is armed for whichever comes first: the next tick, or the earliest deadline of a task sleeping on that CPU ([The local timer](/docs/scheduler#the-local-timer)). At every boot, a self-test on every CPU sleeps for a millisecond 21 times and fails the boot if the median is over 5 ms. Under QEMU it measures 1.007–1.07 ms with HVF and 1.1–2.4 ms with TCG. As a control, we took the arming out, and the result was 10.0 ms.

Then, before every entry into the guest, the hypervisor arms its host CPU's timer for the guest's next timer edge ([415d29d](https://github.com/irqlevel/nos/commit/415d29d689e96f9e40755d96ad231fc05d51da43)). That way even a guest that computes without exiting is pulled out on time ([`run.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/run.rs)):

```rust
            let edge = pit_edge.unwrap_or(u64::MAX)
                .min(gc.lapic.next_timer_ns().unwrap_or(u64::MAX))
                .min(devices_due.unwrap_or(u64::MAX));
            if edge != u64::MAX {
                kcore::cpu::timer_at(edge);
            }
```

A deadline that has already passed is refused, because it would fire again at every entry. In Linux 6.18 guests, a hundred `usleep 1000` calls took 0.16 s instead of 0.99, whether the guest was halted or running a CPU-bound loop. The TCP test did not move (3.68–5.36 s, four runs). This step was groundwork for the next one.

A real millisecond has a cost of its own ([4696f48](https://github.com/irqlevel/nos/commit/4696f48e2bae699f80bc6fbc04a50e242d9a2c00)). TCP's blocking calls poll with a 1 ms sleep. Once that sleep really lasted 1 ms, an idle nos with sshd and two open sessions woke up 2,220 times a second and was 1.2% busy across its 20 CPUs. Now the sleep doubles each time, from 1 ms up to 10, and that brought it down to 741 wakeups and 0.4%. The commit message names the real fix: a wait that the receive path signals.

## Receive interrupts at least 100 µs apart

With the new timer in place, moderation takes just a few lines in the device ([4445a2d](https://github.com/irqlevel/nos/commit/4445a2d892a63fd9383c58d6b6a608d1c3f8a6dd)). It works like interrupt throttling in a real NIC. A frame goes into the guest's buffers at once, but its interrupt waits until enough time has passed since the previous one. Then a single interrupt covers every frame that came in meanwhile ([`net.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/net.rs)):

```rust
    fn rx_interrupt(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        if !self.rx_owed || now < self.rx_quiet_until {
            return Raise::default();
        }
        self.rx_owed = false;
        if !self.transport.queue(RX_QUEUE).map_or(false, |q| q.wants_interrupt(mem)) {
            return Raise::default();
        }
        if self.rx_quiet_ns != 0 {
            self.rx_quiet_until = now.saturating_add(self.rx_quiet_ns);
            self.backend.quiet_until(self.rx_quiet_until);
        }
        self.transport.interrupt(RX_QUEUE)
    }
```

The device tells the switch when this quiet window ends. Until then the switch kicks nobody for a new frame, unless the port's inbox is half full. The vCPU that does the device work comes back by itself at the end of the window, woken by the one-shot timer. `rxmod=US` sets the window. We made three runs for each window, alternating between them:

| window | frames per interrupt | ACKs from the receiver | 500 MB |
|---|---|---|---|
| none | 1.12–1.17 | 137,000–176,000 | 4.57–5.15 s (4.88) |
| 25 µs | 2.6–2.8 | 140,000–168,000 | 4.47–5.22 s (4.72) |
| 50 µs | 4.7–5.0 | 102,000–144,000 | 4.13–4.80 s (4.57) |
| 100 µs | 9.7–10.2 | 63,000–87,000 | 3.94–4.07 s (4.01) |
| 200 µs | 20.0–20.5 | 33,000–47,000 | 3.70–3.86 s (3.77) |

A 200 µs window is faster still, but it costs latency. A hundred small HTTP fetches, each on its own connection, took 0.07 s with no window, 0.05–0.07 s at 100 µs and 0.08–0.09 s at 200. That is why the default is 100 µs. The hypervisor fuzzer's NIC target now moderates on most inputs, and it found a bug right away. A queue that was taken down left the device owing an interrupt for good. That interrupt would have been due forever, and the run loop would have spun without a pause.

## Offloads in both directions

Offloads let a guest's network driver hand some of its work to the device. The guest's virtio-net now offers `VIRTIO_NET_F_CSUM` and `HOST_TSO4` ([9595afb](https://github.com/irqlevel/nos/commit/9595afb6773c8f2bb479a82eaa133fd9a9b3c84b)). With them, a driver can leave checksums to the device and hand it TCP over IPv4 as a single send of up to 64 KiB. The device cuts the send into frames of at most 1514 bytes before they reach the switch. So nos's network stack, NAT and the other guests still see whole frames with correct checksums. The device checks every offset and length the driver gives against the frame before using it, and one limit is there to protect the host ([`offload.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/offload.rs)):

```rust
/// The least segment size a send may ask to be cut by: a send is then 256
/// frames at most, a ring's worth, where a byte a segment would make one
/// notify 65,000 frames on the switch -- free to the guest that asks, and
/// costing the host's CPU and every inbox on the way. TCP on the switch's
/// MTU cuts at 1,448.
pub const MIN_SEGMENT: usize = 256;
```

The web server test took 1.11 and 1.12 s, against 1.63 s in both runs without offloads. The device cut 8,400 sends into 362,000 frames, and the number of exits barely changed. What the device saved was the guest's own work on each frame. Then came the receiving side ([f37c5a6](https://github.com/irqlevel/nos/commit/f37c5a66e0f9ec9746051bccadb1ce35c4e17619)): `GUEST_CSUM`, `GUEST_TSO4` and mergeable buffers. Between two guests, the switch no longer cuts sends or finishes checksums when it doesn't need to. If the receiving guest's driver accepts these features, it gets an unfinished checksum exactly as the sender left it. (Linux treats such a frame as already checked, as it does its own loopback traffic.) It also gets a send of up to 64 KiB in one piece, spread across its buffers. The web server test now took 0.47 s in both runs, against 1.63 and 1.68. The fetching guest received 8,500 sends instead of 362,000 frames. `hv`, which parses all of this, still has four `unsafe` sites.

But `dd | nc` got 7% slower: 4.45 s against 4.15, five runs each. When we offered the features one at a time, most of the loss came from the sender's checksum offload, while the exits and IPIs stayed the same. A Linux driver that leaves checksums to the device also turns on scatter-gather, so a frame takes three or four descriptors instead of two. And for every descriptor and buffer the device read, the host had to map and unmap a guest page. The commit named this as the likeliest cost, and the next commit measured it.

## Keeping the rings mapped while the queue is up

Every access to guest memory used to be a copy through the CPU's own slot in a window of the kernel's address space. The page was mapped into the slot, the bytes copied, then the slot unmapped and its TLB entry flushed. A virtio NIC's rings alone needed that a dozen times per frame. Now a queue's rings (three pages for 256 entries) stay mapped while the queue is up ([23fdb63](https://github.com/irqlevel/nos/commit/23fdb63ebf6849edf5a8ecc784ea79991f71c859)). The code still reaches them one value at a time: a volatile load or store at a checked, aligned offset, with acquire or release ordering for the indexes. There is still no reference into guest memory. The mapping, [`kcore::frame::Mapped`](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/frame.rs), holds a share of its region's frames, so none of them can be freed while it is mapped. The six `unsafe` sites this needed are all in `kcore`, in its frame run and its mapping, each with the reason it is sound.

The copy window had done two things by accident, and now they had to be done on purpose. First, its INVLPG instruction is serializing. It happened to sit between the device publishing the used index and the device reading the driver's flags ([`virtio.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/virtio.rs)):

```rust
    pub fn wants_interrupt(&self, mem: &GuestMemory) -> bool {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.rings(mem).word(self.avail_at()).map_or(true, |flags| flags & AVAIL_NO_INTERRUPT == 0)
    }
```

Without the fence, the device's load can overtake its own store. The driver then waits for an interrupt that the device has decided not to send. Second, removing a mapping shoots its address down on every host CPU, which means interrupting all of them to flush it from their TLBs. A guest that set up its queues over and over could interrupt the whole host at the cost of one exit each time. So a device gets eight mappings, then one more per second. Past that, its rings are copied as before.

The results, five runs each: `dd | nc` took 2.72 s instead of 4.15, and 2.83 s instead of 4.45 with the offloads on, so their cost now falls within the spread. The web server test took 0.29 s instead of 0.47.

## A 10 ms ping

The mapped-rings build showed something that averages hide. Pings between two guests took 0.2 ms, but one in every few hundred took 10.2 ms. Frames that arrive inside a moderation window wake nobody. The first CPU's loop, which makes the rounds of the devices, comes back for them at the end of the window. It knows when, because it raised the interrupt that opened the window. But Linux refills its receive ring from whichever CPU took the interrupt. That CPU's notify (the driver's signal to the device that there is new work) raises the next interrupt itself, and so opens a window the first CPU never hears about. The first CPU only knew about the old window, so it slept until its 10 ms limit while the frames waited in its inbox. Now a notify from another CPU that moves the window wakes the first CPU ([78042cf](https://github.com/irqlevel/nos/commit/78042cfb294cb40d5b7bd35d17a49212d6e3023a)). Out of 2,400 pings none was slow, and the average was 0.19 ms. The fuzzer knew each port's window, but not one opened behind that vCPU's back.

## A separate task for the NIC: measured and left out

The task of a guest's first CPU does all the device work, so moving the NIC off it was the obvious next lever. We tried it ([8aa5167](https://github.com/irqlevel/nos/commit/8aa5167d3d072d4d981b61046cf79f71094eb039)). A guest's NICs got a task of their own on the spare thread of the guest's core, polling with the driver's notifies turned off. But by default guests get whole cores, so only a guest with one CPU leaves a thread spare. Such a guest has no local APIC: its interrupts come through the 8259 and take it out of the guest anyway. It was no faster: `dd | nc` took 2.7–2.8 s against 2.66, and `httpd` 0.42–0.59 s against 0.21. The documentation notes where it could still pay off (guests with MSI-X and a CPU to spare), and the code stayed out of the merge.

## What is left

Posted interrupts are a VT-x feature. Everything else lives in the devices and in the run loop that both backends share, although we measured it all on the Intel machine. The [Speed](/docs/hypervisor#speed) list in the documentation still has open items: interrupt delivery by the CPU itself for xAPIC guests, AMD's AVIC, fewer exits per request (there is no EVENT_IDX or NO_NOTIFY yet), and fio and iperf in a guest, which nobody has measured yet.

Every step passed the gates before it went in. The hypervisor fuzzer ran 1.2 million inputs with no findings. Both architectures were built, smoke-booted and run through `hv-test`, as plain builds and as `UBSAN=1 RUSTUB=1` builds. `hv-linux-test` ran nested on the EX44. For the rest of the picture, see [the hypervisor page](/hypervisor).
