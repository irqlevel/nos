---
title: "Guest networking from 5.7 seconds to 2.8 — and two ideas measured and left out"
description: "Posted interrupts, a one-shot timer, moderated receive interrupts, offloads and mapped rings, measured on bare metal; halt polling and a NIC task did not help."
date: 2026-10-22
tags: [hypervisor, networking, performance, x86-64, rust]
---

The test: two Linux guests of two CPUs each on the EX44, the Hetzner server whose i5-13500 runs nos on bare metal and its hypervisor under Intel VT-x. One sends the other 500 MB of TCP through the `hv` module's switch, a kilobyte a write (`dd | nc`). When this work began, the evening the Intel round was merged ([3912397](https://github.com/irqlevel/nos/commit/39123972ca3197b653636c3373cf14594ac4b3ca)), that took 5.72 s on average; by the next evening, 2.72–2.83 s ([2a20db5](https://github.com/irqlevel/nos/commit/2a20db5505db5054c06655e04f6330b634db03b2)). A second test joined on the way — one guest's BusyBox `httpd` serving a 100 MB file five times over to the other — and went from 1.63–1.68 s without the offloads to 0.29 s, 1.8 GB/s.

Each step was a commit whose message carries its measurement: the runs, the spread, the comparison. The figures are from the EX44's silicon unless said otherwise; the gates ran under nested KVM on the same box. Two steps were ideas that measured no faster, and stayed in the documentation, not in the code.

## The day in one table

| Commit | What it took out | Measured |
|---|---|---|
| [89a4c5a](https://github.com/irqlevel/nos/commit/89a4c5a92bc5fbb86d5338edfc52238047ed19ce) posted interrupts | the exit that hands a guest CPU an IPI or an MSI | TCP 4.85 s on average, where 5.72 |
| [e98a4de](https://github.com/irqlevel/nos/commit/e98a4de271a70ad4868c4e5f3daec9c1dbac71be) halt polling, taken apart | nothing: not kept | 6.27 and 6.48 s, against 3.94 and 5.08 |
| [dc416b9](https://github.com/irqlevel/nos/commit/dc416b9100208afbc2530be4b46d1ba267286e26), [415d29d](https://github.com/irqlevel/nos/commit/415d29d689e96f9e40755d96ad231fc05d51da43) a one-shot timer | up to 10 ms of a guest timer's lateness | 100 × `usleep 1000` in 0.16 s, where 0.99 |
| [4696f48](https://github.com/irqlevel/nos/commit/4696f48e2bae699f80bc6fbc04a50e242d9a2c00) TCP's polls back off | the wakeups a real millisecond costs | an idle nos 741 wakeups a second, where 2,220 |
| [4445a2d](https://github.com/irqlevel/nos/commit/4445a2d892a63fd9383c58d6b6a608d1c3f8a6dd) receive interrupts 100 µs apart | an interrupt a frame | TCP 4.01 s, where 4.88 |
| [9595afb](https://github.com/irqlevel/nos/commit/9595afb6773c8f2bb479a82eaa133fd9a9b3c84b), [f37c5a6](https://github.com/irqlevel/nos/commit/f37c5a66e0f9ec9746051bccadb1ce35c4e17619) offloads, both ways | the guest's own work a frame, then forty-odd frames a send | `httpd` 0.47 s, where 1.63–1.68 |
| [23fdb63](https://github.com/irqlevel/nos/commit/23fdb63ebf6849edf5a8ecc784ea79991f71c859) rings mapped | a page mapped and its TLB entry flushed at every ring access | TCP 2.72 s, where 4.15; `httpd` 0.29 s, where 0.47 |
| [78042cf](https://github.com/irqlevel/nos/commit/78042cfb294cb40d5b7bd35d17a49212d6e3023a) a moved window rings the first CPU | a 10 ms answer every few hundred pings | 2,400 pings, 0.28 ms at most |
| [8aa5167](https://github.com/irqlevel/nos/commit/8aa5167d3d072d4d981b61046cf79f71094eb039) a NIC's own task | nothing: not merged | `httpd` 0.42–0.59 s, against 0.21 |

## Posted interrupts: an IPI with no exit

With APIC virtualization a VT-x CPU delivers the interrupts of a guest in x2APIC mode and performs its EOIs itself, no exit. The Intel round had it only for guests that asked (`apicv`), because another CPU's IPI or a device's MSI still went into a mailbox, and a CPU in its guest had to be kicked out to be handed it — an IPI and an exit, dearer than the EOI exit it replaced. With it the guests exited 25–50% less and still took 5.88 s on average, against 5.70 without.

Posted interrupts take the kick out ([89a4c5a](https://github.com/irqlevel/nos/commit/89a4c5a92bc5fbb86d5338edfc52238047ed19ce)). Each guest CPU has a 64-byte descriptor its VMCS names, [`PostedInterrupts`](https://github.com/irqlevel/nos/blob/master/src/rust/hvarch/src/x86/vmx.rs) — a request bit a vector, and ON, a notification outstanding — where the sender of a fixed, edge-triggered interrupt sets its bit:

```rust
    pub fn post(&self, vector: u8) -> bool {
        self.pir[usize::from(vector / 64)].fetch_or(1 << (vector % 64), Ordering::SeqCst);
        self.control.fetch_or(PI_ON, Ordering::SeqCst) & PI_ON == 0
    }
```

If ON was clear and the target is in its guest, the sender sends its host CPU the notification vector, 0xFC, and the CPU moves what was posted into VIRR and delivers it inside the guest. Level-triggered interrupts, NMIs, INIT and start-up IPIs still go through the mailbox.

The price was paid in `hvarch`. Posted interrupts need every external-interrupt exit to acknowledge the interrupt, or the host would take a notification meant for the guest; so the host's own interrupts now arrive acknowledged and delivered to nothing, and `hvarch` re-delivers each by an `int` of its vector through the host's IDT, once no per-CPU state is borrowed, since the handler may switch tasks. Six new `unsafe` sites in `hvarch`, 195 to 201; `hv` stays at four.

Ten runs with them and ten without, taken in turns: 3.66–5.89 s, 4.85 on average, against 5.58–5.79 s, 5.72 — faster in 9 of the 10, with 0.67–1.58 million exits against 2.3–2.6 million. The CPU's own delivery is now the default wherever it has posted interrupts (`noapicv` turns it off), and a new built-in guest, `posted`, holds it there: its second CPU sends the first a thousand IPIs while the first spins, and on the EX44 the first left its guest 9 times for the thousand.

## Halt polling was interrupt moderation by accident

The Intel round had left a puzzle: a halted vCPU that watched its mailbox for 200 µs before sleeping had taken this transfer from 5.7 s to 3.9, while a watch that adapted, KVM's way, came to 5–7 s. Taken apart with posted interrupts on, two runs each ([e98a4de](https://github.com/irqlevel/nos/commit/e98a4de271a70ad4868c4e5f3daec9c1dbac71be)), real halt polling — mailbox and doorbell watched for 200 µs — was slower: 6.27 and 6.48 s against 3.94 and 5.08 with no watch. It hands each frame's interrupt over at once, so the receiver takes one frame an interrupt and acknowledges every one or two — 265,000–277,000 ACKs against 111,000–151,000 — and the sender, answered that often, sends more and smaller frames.

The watch that had looked like a win watched the mailbox alone. On the receiving guest's halted first CPU it left the doorbell the switch rings for device work unanswered until the 200 µs ran out, so frames waited and went into the ring together: 18.9 an interrupt, 41,000 ACKs, 3.94 s — moderation by accident. Slept instead of spun, the same wait took 12.5–14.6 s, because a sleep on the host ended at its next 100 Hz tick. What the numbers asked for was moderation on purpose, and a timer finer than the tick.

## A timer finer than the tick

Each CPU's timer — the local APIC's on x86-64, the generic timer's on arm64 — was periodic at 100 Hz, and a task blocked until a deadline ran at the first tick after it: a millisecond's sleep took ten. A halted guest's vCPU waits that way for its guest's next timer edge, and a guest's `usleep 1000` took 10 ms. Now the timer is one-shot ([dc416b9](https://github.com/irqlevel/nos/commit/dc416b9100208afbc2530be4b46d1ba267286e26)), armed for the earlier of the next tick and the soonest deadline of a task asleep on that CPU ([The local timer](/docs/scheduler#the-local-timer)). A self-test sleeps a millisecond 21 times on every CPU at every boot and fails the boot if the median is over 5 ms: under QEMU, 1.007–1.07 ms with HVF and 1.1–2.4 ms with TCG; with the arming taken out, 10.0 ms — the control.

The hypervisor then arms its host CPU's timer for the guest's next edge before every entry ([415d29d](https://github.com/irqlevel/nos/commit/415d29d689e96f9e40755d96ad231fc05d51da43)), so a guest computing without an exit is taken out at the edge too ([`run.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/run.rs)):

```rust
            let edge = pit_edge.unwrap_or(u64::MAX)
                .min(gc.lapic.next_timer_ns().unwrap_or(u64::MAX))
                .min(devices_due.unwrap_or(u64::MAX));
            if edge != u64::MAX {
                kcore::cpu::timer_at(edge);
            }
```

A deadline already past is refused: it would fire again at every entry. In Linux 6.18 guests a hundred `usleep 1000` took 0.16 s where they had taken 0.99, halted or running a CPU-bound loop. The TCP pair did not move (3.68–5.36 s, four runs): this was for the next step.

A real millisecond has a cost too ([4696f48](https://github.com/irqlevel/nos/commit/4696f48e2bae699f80bc6fbc04a50e242d9a2c00)): TCP's blocking calls poll with a 1 ms sleep, and an idle nos with sshd and two sessions open woke 2,220 times a second, 1.2% busy over its 20 CPUs. A sleep doubling from 1 ms to 10 made that 741 and 0.4%; the commit names the real fix, a wait the receive path signals.

## Receive interrupts, at least 100 µs apart

With the timer in, moderation is a few lines of the device ([4445a2d](https://github.com/irqlevel/nos/commit/4445a2d892a63fd9383c58d6b6a608d1c3f8a6dd)). As in a real NIC's interrupt throttling, a frame goes into the guest's buffers at once, but its interrupt waits until the time since the last is up, and then one takes all that came ([`net.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/net.rs)):

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

Told the window's end, the switch kicks nobody for a frame until then unless the port's inbox is half full, and the vCPU that does the devices' work comes back at the end by itself, on the one-shot timer. `rxmod=US` sets the window. Three runs at each, taken in turns:

| window | frames an interrupt | receiver's ACKs | 500 MB |
|---|---|---|---|
| none | 1.12–1.17 | 137,000–176,000 | 4.57–5.15 s (4.88) |
| 25 µs | 2.6–2.8 | 140,000–168,000 | 4.47–5.22 s (4.72) |
| 50 µs | 4.7–5.0 | 102,000–144,000 | 4.13–4.80 s (4.57) |
| 100 µs | 9.7–10.2 | 63,000–87,000 | 3.94–4.07 s (4.01) |
| 200 µs | 20.0–20.5 | 33,000–47,000 | 3.70–3.86 s (3.77) |

200 µs is faster still, and costs latency: a hundred small HTTP fetches, each its own connection, took 0.07 s with no window, 0.05–0.07 s at 100 µs and 0.08–0.09 s at 200. Hence 100 µs, the default. The fuzzer's NIC target, moderating on most inputs, found at once that a queue taken down left an interrupt owed for good — due forever, a run loop that would have gone round without a pause.

## Offloads, both ways

The guest's virtio-net now offers `VIRTIO_NET_F_CSUM` and `HOST_TSO4` ([9595afb](https://github.com/irqlevel/nos/commit/9595afb6773c8f2bb479a82eaa133fd9a9b3c84b)): a driver may leave checksums to the device and hand it TCP over IPv4 as one send of up to 64 KiB, which the device cuts into frames of at most 1514 bytes before the switch, so nos's stack, NAT and the other guests still see whole, checksummed frames. Every offset and length the driver gives is checked against the frame before use, and one limit protects the host ([`offload.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/offload.rs)):

```rust
/// The least segment size a send may ask to be cut by: a send is then 256
/// frames at most, a ring's worth, where a byte a segment would make one
/// notify 65,000 frames on the switch -- free to the guest that asks, and
/// costing the host's CPU and every inbox on the way. TCP on the switch's
/// MTU cuts at 1,448.
pub const MIN_SEGMENT: usize = 256;
```

The web server: 1.11 and 1.12 s where it took 1.63 twice, 8,400 sends cut into 362,000 frames, with the exits barely changed — what the device saved was the guest's own work per frame. Then the receiving side ([f37c5a6](https://github.com/irqlevel/nos/commit/f37c5a66e0f9ec9746051bccadb1ce35c4e17619)): `GUEST_CSUM`, `GUEST_TSO4` and mergeable buffers. Between two guests the switch no longer cuts or finishes what it need not: a guest whose driver takes them gets a checksum left to finish as its sender left it — Linux takes it as checked, as it does its own loopback's — and a send of up to 64 KiB whole, spread over its buffers. The web server: 0.47 s twice, against 1.63 and 1.68; the fetching guest took 8,500 sends where it had taken 362,000 frames. `hv`, which parses all of it, stayed at four `unsafe` sites.

But `dd | nc` got 7% slower: 4.45 s against 4.15, five runs each. Offered one feature at a time, most of it was the sender's checksum, with exits and IPIs alike: a Linux driver that leaves checksums to the device also turns on scatter-gather, three or four descriptors a frame where there were two, and every descriptor and buffer the device read was a guest page mapped and unmapped on the host. The commit named that the likeliest cost; the next one measured it.

## Rings mapped while the queue is up

Every access to guest memory was a copy through the CPU's own slot of the kernel's window: the page mapped, the bytes copied, the slot unmapped and its TLB entry flushed — a dozen times a frame for a virtio NIC's rings alone. Now a queue's rings, three pages for 256 entries, stay mapped while the queue is up ([23fdb63](https://github.com/irqlevel/nos/commit/23fdb63ebf6849edf5a8ecc784ea79991f71c859)), still reached one value at a time — a volatile load or store at a checked, aligned offset, an index with acquire or release — and still with no reference into guest memory. The mapping, [`kcore::frame::Mapped`](https://github.com/irqlevel/nos/blob/master/src/rust/kcore/src/frame.rs), holds a share of its region's frames, so none is freed while it is mapped; the six `unsafe` sites this took are in `kcore`'s run and mapping, each with its reason.

The window had done two things by accident that now had to be done on purpose. Its INVLPG is serializing, and stood between the device publishing the used index and reading the driver's flags ([`virtio.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/hv/src/devices/virtio.rs)):

```rust
    pub fn wants_interrupt(&self, mem: &GuestMemory) -> bool {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.rings(mem).word(self.avail_at()).map_or(true, |flags| flags & AVAIL_NO_INTERRUPT == 0)
    }
```

Without the fence the device's load can pass its own store, and the driver waits for an interrupt the device decided not to send. And undoing a mapping shoots its address down on every host CPU, so a guest that set its queues up over and over could interrupt the whole host at the price of an exit each: a device maps eight, then one a second, and past that its rings are copied as before.

Five runs each: `dd | nc` 2.72 s where it took 4.15, and 2.83 where it took 4.45 with the offloads, whose cost fell inside the spread; the web server 0.29 s where it took 0.47.

## A 10 ms ping

The mapped-rings build showed what averages hide: pings between two guests took 0.2 ms, and one in every few hundred 10.2 ms. Frames arriving inside a moderation window wake nobody, because the first CPU's loop, which does the devices' rounds, comes back at the window's end: it raised the interrupt that opened it. But Linux refills its receive ring from the CPU its interrupt is taken on, and that CPU's notify raises the next interrupt itself, opening a window the first CPU never heard of — which, asleep with the old end in mind, slept to its 10 ms cap with the frames in its inbox. Now a notify from another CPU that moves the window rings the first ([78042cf](https://github.com/irqlevel/nos/commit/78042cfb294cb40d5b7bd35d17a49212d6e3023a)): 2,400 pings, none slow, 0.19 ms on average. The fuzzer knew each port's window, but not one opened behind that vCPU's back.

## A NIC's own task, measured and left out

The first CPU's task does every device's work, and moving a NIC off it was the obvious next lever. We tried it ([8aa5167](https://github.com/irqlevel/nos/commit/8aa5167d3d072d4d981b61046cf79f71094eb039)): a guest's NICs served by a task of their own on the spare thread of its core, polling with the driver's notifies off. But guests get whole cores by default, so only a guest of one CPU leaves a thread spare — and that guest has no local APIC: its interrupts come through the 8259 and take it out of its guest anyway. It was no faster: `dd | nc` 2.7–2.8 s against 2.66, `httpd` 0.42–0.59 s against 0.21. The note says where it could still pay — guests with MSI-X and a CPU to spare — and the code stayed out of the merge.

## What is left

Posted interrupts are VT-x's; the rest is in the devices and the run loop both backends share, though it was all measured on the Intel machine. Still open in the doc's [Speed](/docs/hypervisor#speed) list: the CPU's own delivery for xAPIC guests and AMD's AVIC, fewer exits a request (no EVENT_IDX or NO_NOTIFY yet), and fio and iperf in a guest, never measured.

Every step was gated as it went in: the hypervisor fuzzer, 1.2 million inputs with no finding; both architectures built, smoke-booted and through `hv-test`, plain and as `UBSAN=1 RUSTUB=1` builds; `hv-linux-test` nested on the EX44. The rest of the picture is on [the hypervisor page](/hypervisor).
