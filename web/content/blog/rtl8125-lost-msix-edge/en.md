---
title: "Seven wrong theories and one register read: the RTL8125's lost interrupt"
description: "Under UDP load the EX44 went deaf for good. Seven theories read off the driver were wrong; one reading off the chip found a lost MSI-X edge."
date: 2026-09-05
tags: [hardware, networking, drivers, debugging, rust]
---

## A server that reports through the thing that broke

The Hetzner EX44 is one of the three real machines nos boots on: a 20-thread Raptor Lake with no serial port and no IPMI. Its only console is the network, and the network is a Realtek RTL8125 with a Rust driver. QEMU has no model of the chip, so until this box booted, `drivers/r8125` had never run on the hardware it was written for ([Real hardware](/docs/real-hardware#hetzner-ex44-dedicated-server)).

The box had already wedged twice under nothing heavier than UDP shell traffic. Then we gave the profiler something to look at: `netload`, a UDP target that echoes every datagram from the receive softirq, or drops it in sink mode. From then on the box went deaf on demand. The kernel kept running and kept transmitting, and it never received another packet.

That is the worst kind of bug for such a machine. Every channel it can report through (the shell, ping, [netconsole](/docs/netconsole)) needs packets to arrive or leave, so a log that ends at `NetLoad: started` could mean a wedged kernel or a healthy one with nothing more to say. One of the first changes was therefore a heartbeat rather than a fix ([064b222](https://github.com/irqlevel/nos/commit/064b222)): while the load runs, the target traces a line a second, and netconsole, which only sends, carries it out. The lines kept arriving, so the kernel was alive and only receive was dead. Their frames-in-flight count climbed to 257, the whole ring plus one, and stayed there.

## Seven theories, read off the code

Seven explanations came before the right one, and each got a commit. Among them:

- **ARP inside the receive handler.** The target echoed through `SendUdp`, which on an ARP cache miss sleeps for up to three seconds inside the receive path. That was a real bug, fixed by replying from the received frame with its addresses swapped ([8285944](https://github.com/irqlevel/nos/commit/8285944)). It did not cause the stall.
- **Frame-pool exhaustion.** Measured under load in QEMU, the pool never missed an allocation. It got headroom and a boot parameter anyway ([2e2863a](https://github.com/irqlevel/nos/commit/2e2863a)).
- **The transmit path.** Sink mode, which transmits nothing, still went deaf at 350 packets a second. Two echo runs had returned 271 and 358 datagrams before going quiet, about one receive ring's worth ([064b222](https://github.com/irqlevel/nos/commit/064b222)).
- **A lost wakeup with nothing to recover it.** Only the receive softirq looked at the ring, and only the interrupt handler raised it. We gave the tick a receive poll, so a lost wakeup would cost ten milliseconds instead of the rest of the uptime ([f0b3c93](https://github.com/irqlevel/nos/commit/f0b3c93)). On the EX44 the stall then came sooner, within seconds, and the poll went behind `rxpoll=on`, off by default ([91e2968](https://github.com/irqlevel/nos/commit/91e2968)).
- **A handler that skips the softirq.** First the instrument needed fixing. `irqstat` showed zero MSI-X interrupts on a machine that had just received thirteen thousand packets, because nothing ever incremented the counter ([cb0aec7](https://github.com/irqlevel/nos/commit/cb0aec7)). Then the handler raised the receive softirq on every interrupt, whatever the status bits said ([0e1d680](https://github.com/irqlevel/nos/commit/0e1d680)).
- **A FIFO overflow that needs the receiver restarted.** The next run lasted 182 seconds and 44000 packets, then stalled anyway. We started counting the receive error bits and tracing the first ten ([f1e3446](https://github.com/irqlevel/nos/commit/f1e3446)). A wrong name had pointed us here: both Realtek drivers called status bit 4 `RX_OVERFLOW`, but it is RDU, "Rx Descriptor Unavailable". The name led to the vendor's remedy for an overflow, restarting the receiver, and cost a full round of build, boot and log reading ([c77280d](https://github.com/irqlevel/nos/commit/c77280d)).

The trace never fired, and the next run stalled after 276 packets and two seconds. The commit recording it is where the approach changed ([94e035c](https://github.com/irqlevel/nos/commit/94e035c)):

> A hundred and sixty fold spread across runs of the same load is not saturation, it is a race, and the 182-second run I described as a threefold improvement was one sample being read as a trend.

Every wrong explanation had been worked out from what the driver's code said the hardware must be doing. The machine survived the stall and could still be typed at. So instead of making another guess, the same commit added `nicdump`, which reads the state from the chip itself.

## What the chip said

`nicdump` prints the command register, the interrupt status and mask, and who owns the head receive descriptor. Bit 3 of the command register is the receiver. If the chip had turned it off, no work on the descriptors would bring it back, and none of the guesses had checked it. On the stalled machine it printed:

```text
cmd 0xC  rx-en 1 tx-en 1
isr 0x40D5  imr 0x7F
rx head 194  posted 1  own 0
```

The receiver was on. The head descriptor belonged to the driver: the chip had written a valid 558-byte frame into it and handed it back, and the driver never took it. Under the mask four status bits were set (ROK, TOK, RDU and the receive-FIFO overflow), none of them acknowledged. The handler acknowledges whenever it runs, so it had not run since. Two `ps` snapshots five seconds apart showed all twenty softirq tasks cycling normally. Nothing was stuck; the interrupt had simply never arrived ([6c5411e](https://github.com/irqlevel/nos/commit/6c5411e)).

## Why it never arrived

`INTR_STATUS` is write-1-to-clear. The chip signals MSI-X when `status & mask` goes from 0 to 1, and that is a transition of the register as a whole, not one per event. The handler read the status once and wrote the same value back:

```text
read status         -> ROK
[chip sets TOK]        aggregate already non-zero: no message sent
write status (ROK)  -> ROK cleared, TOK still set
return                 TOK set, mask open, nothing pending, nothing
                       ever coming -- the aggregate cannot go 0->1
                       again because it never went back to 0
```

After that, each new event only adds a bit to a status that is already non-zero, and the chip sends nothing. The window is a few instructions wide; at three hundred events a second it is hit sooner or later, which is why the same load stalled after 276 packets once and 44000 another time. It also explains every failed fix: raising the softirq on every interrupt did nothing because no interrupts came, the error trace never fired because the handler never ran, and the tick poll did not help because the chip's message was already lost.

We had seen this race fourteen hours earlier. The commit that added the tick poll lists it, "the ISR acknowledges by writing back the status word it read, which loses any bit the chip sets in between", and leaves it alone "because the poll makes losing that race survivable, which is the better property to have". The fallback was a reasonable idea, but the race was the bug.

## The fix: leave only from zero

The handler now loops. It acknowledges what it read, does the work, reads again, and returns only when nothing enabled is left in the register. From that state, the next event is a 0→1 transition the chip will signal. This is the handler today, after the NAPI work that followed the same evening:

```rust
fn isr(dev: &'static R8125Device) {
    let regs = &dev.regs;
    // ...
    const MAX_ROUNDS: u32 = 32;
    let mut round = 0;

    loop {
        let mask = regs.read32(INTR_MASK);
        let status = regs.read32(INTR_STATUS);

        if status == u32::MAX {
            /* All-ones is what a vanished device reads back, not a status
             * with every event set at once. */
            return;
        }

        if status & mask == 0 {
            return;
        }

        regs.write32(INTR_STATUS, status);

        // ... link change, error counters, the transmit softirq ...

        dev.arm(mask & !RX_INTR_BITS);
        softirq::raise(softirq::TYPE_NET_RX);

        round += 1;
        if round >= MAX_ROUNDS {
            regs.write32(INTR_STATUS, u32::MAX);
            return;
        }
    }
}
```

Two details are deliberate. The loop exits on `status & mask`, not on `status`: while the poll has the receive sources masked, a set receive bit raises no interrupt, and waiting for it to clear would spin for as long as packets kept arriving ([44cb4eb](https://github.com/irqlevel/nos/commit/44cb4eb)). And the loop is capped at 32 rounds, after which the whole register is acknowledged at once. That loses no work, since the softirqs are raised on every pass, and a status still busy after 32 rounds belongs to a chip that would storm regardless. The RTL8168, same family and same register, had the same hole and got the same loop.

That afternoon a rate ladder ran from 500 to 30,000 packets a second: "It survives all of it now, which it did not this morning" ([c8c99df](https://github.com/irqlevel/nos/commit/c8c99df)). Before the day was out, the EX44 was measured receiving 971,310 packets a second with 64-byte payloads on a single core ([cb81e68](https://github.com/irqlevel/nos/commit/cb81e68)).

## The same edge, in new places

The bug was really a rule about how to use one register, and the handler was not the only code that could break it. When the ring runs dry, the NAPI poll re-enables the receive sources. If it does so while receive bits are still set in the status, `status & mask` is already non-zero and no later packet can produce a 0→1 transition. That is exactly what a review of the poll found: "precisely the lost-edge shape that made this machine deaf for a day, put back in a new place" ([ea6595b](https://github.com/irqlevel/nos/commit/ea6595b)). The poll now clears the status before it re-enables the sources, and checks the ring once more afterwards:

```rust
            /* Clear what the chip has reported, so the next packet is a
             * transition rather than an addition to a status already set. */
            self.regs.write32(INTR_STATUS, RX_INTR_BITS);
            // ...
            if !ring.has_work() {
                /* Empty: hand the ring back to the interrupt. */
                self.arm(INTR_MASK_BITS);

                /* Re-checked after arming. A packet landing between the
                 * harvest above and the write to the mask register was
                 * already counted in the status cleared at the top of this
                 * round, so nothing would come for it. */
                if !ring.has_work() {
                    return;
                }

                /* Raced: it is ours to take, so go silent again. */
                self.arm(INTR_MASK_BITS & !RX_INTR_BITS);
            }
```

A day later the Intel I210 on the other server stormed with interrupts for the opposite reason: in MSI-X mode its cause register is not cleared by a read. The fixing commit says so: "Same lesson the Realtek card taught and this driver did not learn: acknowledge explicitly rather than trusting a clear-on-read you have not read the definition of" ([702753e](https://github.com/irqlevel/nos/commit/702753e); the full story is in [Ten megabits on a gigabit link](/blog/i210-ten-megabits)).

## What keeps it visible

Nothing emulates the RTL8125 or the RTL8168, so no gate in the tree can catch this class of bug on them. A change to either driver is checked by the compiler and then on the EX44, whose only console is that NIC ([Tests and gates](/docs/testing#the-hardware-nic-drivers)). What the day left behind is a set of instruments: [`nicdump`](https://github.com/irqlevel/nos/blob/master/src/rust/drivers/r8125/src/lib.rs), an `irqstat` that counts the Rust drivers' interrupts, receive error counters, and a heartbeat over the one channel that survives.

The tick poll stays as a fallback behind `rxpoll=on` ([Kernel parameters](/docs/kernel-parameters)). Two weeks later its switch silently stopped working: the port of the network layer to Rust wired the tick straight to a poll that had no switch, and every tick polled. Rates held, but on the AX41 a flood took 70% of each of two CPUs where it had taken one. The switch now lives in `DeviceTable::poll_rx` itself ([source](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/device.rs)), and `netload-test.py` checks it both ways, booted without it and with it, because a switch broken in either direction looks like success ([5eb5535](https://github.com/irqlevel/nos/commit/5eb5535)).
