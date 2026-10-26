---
title: "Seven wrong theories and one register read: the RTL8125's lost interrupt"
description: "Under UDP load, the EX44 stopped receiving for good. Seven theories based on the driver's code were wrong. Reading the chip itself found a lost MSI-X edge."
date: 2026-09-05
tags: [hardware, networking, drivers, debugging, rust]
---

## The only console is the thing that broke

The Hetzner EX44 is one of the three real machines nos boots on. It has a 20-thread Raptor Lake CPU and neither a serial port nor IPMI, so its only console is the network. Its network card is a Realtek RTL8125 with a Rust driver. QEMU does not emulate this chip, so until this server booted, `drivers/r8125` had never run on the hardware it was written for ([Real hardware](/docs/real-hardware#hetzner-ex44-dedicated-server)).

The server had already hung twice, under nothing heavier than UDP shell traffic. Then we started `netload` to give the profiler something to measure. It is a UDP target that echoes every datagram straight from the receive softirq, or drops it in sink mode. From then on, the box went deaf on demand. The kernel kept running and transmitting, but it never received another packet.

For a machine like this, that is the worst kind of bug. Every channel it can report through (the shell, ping, [netconsole](/docs/netconsole)) needs packets to arrive or leave. So a log that ends at `NetLoad: started` could mean a hung kernel, or a healthy one with nothing more to say.

That is why one of the first changes was a heartbeat rather than a fix ([064b222](https://github.com/irqlevel/nos/commit/064b222)). While the load runs, the target writes a trace line every second, and netconsole, which only sends, carries it out. The lines kept arriving, so the kernel was alive and only receiving was broken. The frames-in-flight count in those lines climbed to 257, the whole ring plus one, and stayed there.

## Seven theories, all from the code

Seven explanations came before the right one, and each got a commit. Here are some of them:

- **ARP inside the receive handler.** The target echoed through `SendUdp`, which on an ARP cache miss sleeps for up to three seconds. Here that meant sleeping inside the receive path. That was a real bug, and we fixed it by building the reply from the received frame with its addresses swapped ([8285944](https://github.com/irqlevel/nos/commit/8285944)). But it did not cause the stall.
- **The frame pool running dry.** We measured the pool under load in QEMU, and it never failed an allocation. It got more headroom and a boot parameter anyway ([2e2863a](https://github.com/irqlevel/nos/commit/2e2863a)).
- **The transmit path.** Sink mode transmits nothing, yet the box still went deaf at 350 packets a second. Two earlier echo runs had answered 271 and 358 datagrams before going quiet, about one receive ring's worth ([064b222](https://github.com/irqlevel/nos/commit/064b222)).
- **A lost wakeup that nothing recovers.** Only the receive softirq looked at the ring, and only the interrupt handler raised that softirq. We gave the timer tick a receive poll, so a lost wakeup would cost ten milliseconds instead of the rest of the uptime ([f0b3c93](https://github.com/irqlevel/nos/commit/f0b3c93)). On the EX44 the stall then came even sooner, within seconds, so we put the poll behind `rxpoll=on`, which is off by default ([91e2968](https://github.com/irqlevel/nos/commit/91e2968)).
- **A handler that skips the softirq.** First we had to fix the instrument. `irqstat` showed zero MSI-X interrupts on a machine that had just received thirteen thousand packets, because nothing ever incremented that counter ([cb0aec7](https://github.com/irqlevel/nos/commit/cb0aec7)). Then we made the handler raise the receive softirq on every interrupt, whatever the status bits said ([0e1d680](https://github.com/irqlevel/nos/commit/0e1d680)).
- **A FIFO overflow that needs a receiver restart.** The next run lasted 182 seconds and 44000 packets, then stalled anyway. We started counting the receive error bits and tracing the first ten errors ([f1e3446](https://github.com/irqlevel/nos/commit/f1e3446)). A wrong name had sent us this way. Both Realtek drivers called status bit 4 `RX_OVERFLOW`, but it is really RDU, "Rx Descriptor Unavailable". The name pointed to the vendor's cure for an overflow, which is to restart the receiver. Trying it cost a full round of building, booting and reading logs ([c77280d](https://github.com/irqlevel/nos/commit/c77280d)).

The trace never fired, and the next run stalled after two seconds and 276 packets. The commit that recorded this run is where our approach changed ([94e035c](https://github.com/irqlevel/nos/commit/94e035c)):

> A hundred and sixty fold spread across runs of the same load is not saturation, it is a race, and the 182-second run I described as a threefold improvement was one sample being read as a trend.

We had worked out every wrong explanation from the driver's code, by reasoning about what the hardware must be doing. But the machine survived the stall, and we could still type commands into it. So instead of making another guess, the same commit added `nicdump`, a command that reads the state from the chip itself.

## What the chip said

`nicdump` prints the command register, the interrupt status and mask, and who owns the descriptor at the head of the receive ring. Bit 3 of the command register enables the receiver. If the chip had turned it off, no work on the descriptors would have brought receive back, and none of our guesses had checked that bit. On the stalled machine, `nicdump` printed:

```text
cmd 0xC  rx-en 1 tx-en 1
isr 0x40D5  imr 0x7F
rx head 194  posted 1  own 0
```

The receiver was on. The head descriptor belonged to the driver: the chip had written a valid 558-byte frame into it and handed it back, but the driver never took it. Four status bits were set and enabled in the mask: ROK and TOK (receive OK, transmit OK), RDU, and the receive FIFO overflow. None of them had been acknowledged.

The handler acknowledges every time it runs, so it had not run since those bits were set. Two `ps` snapshots five seconds apart showed all twenty softirq tasks cycling normally. Nothing was stuck. The interrupt had simply never arrived ([6c5411e](https://github.com/irqlevel/nos/commit/6c5411e)).

## Why it never arrived

`INTR_STATUS` is write-1-to-clear: writing a 1 to a bit clears that bit. The chip sends an MSI-X message when `status & mask` goes from 0 to 1, and that is a transition of the whole register, not one per event. The old handler read the status once and wrote the same value back:

```text
read status         -> ROK
[chip sets TOK]        aggregate already non-zero: no message sent
write status (ROK)  -> ROK cleared, TOK still set
return                 TOK set, mask open, nothing pending, nothing
                       ever coming -- the aggregate cannot go 0->1
                       again because it never went back to 0
```

From then on, each new event only adds a bit to a status that is already non-zero, so the chip sends nothing. The window between the read and the write is a few instructions wide. At three hundred events a second the handler hits it sooner or later, which is why the same load stalled after 276 packets in one run and after 44000 in another.

This also explains why every earlier fix failed. Raising the softirq on every interrupt did nothing, because no interrupts came. The error trace never fired, because the handler never ran. And the tick poll did not help, because the chip's message was already lost.

We had seen this race fourteen hours earlier. The commit that added the tick poll lists it: "the ISR acknowledges by writing back the status word it read, which loses any bit the chip sets in between". Then it leaves the race alone "because the poll makes losing that race survivable, which is the better property to have". The fallback was a reasonable idea, but the race was the bug.

## The fix: loop until nothing is pending

The handler now runs in a loop. It acknowledges what it read, does the work, reads the status again, and returns only when no enabled bit is left in the register. From that state, the next event is a 0→1 transition, and the chip will signal it. This is the handler today, after the NAPI work that followed the same evening (a poll drains the ring while the receive interrupt stays masked):

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

Two details are deliberate. First, the loop exits on `status & mask`, not on `status`. While the poll has the receive sources masked, a set receive bit raises no interrupt. Waiting for that bit to clear would spin for as long as packets kept arriving ([44cb4eb](https://github.com/irqlevel/nos/commit/44cb4eb)).

Second, the loop is capped at 32 rounds, after which the handler acknowledges the whole register at once. No work is lost, because the softirqs are raised on every pass. And a status that is still busy after 32 rounds belongs to a chip that would storm with interrupts anyway. The RTL8168, from the same family and with the same register, had the same hole and got the same loop.

That afternoon we ran a rate ladder, stepping the load from 500 to 30,000 packets a second. The commit sums it up: "It survives all of it now, which it did not this morning" ([c8c99df](https://github.com/irqlevel/nos/commit/c8c99df)). Before the day was out, we measured the EX44 receiving 971,310 packets a second with 64-byte payloads on a single core ([cb81e68](https://github.com/irqlevel/nos/commit/cb81e68)).

## The same lost edge, in new places

The bug was really a rule about how to use one register, and the handler was not the only code that could break it. When the ring runs dry, the NAPI poll re-enables the receive sources. If it does that while receive bits are still set in the status, `status & mask` is already non-zero, and no later packet can produce a 0→1 transition. A review of the poll found exactly that: "precisely the lost-edge shape that made this machine deaf for a day, put back in a new place" ([ea6595b](https://github.com/irqlevel/nos/commit/ea6595b)). Now the poll clears the status before it re-enables the sources, and checks the ring once more afterwards:

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

A day later, the Intel I210 on the other server had an interrupt storm for the opposite reason: in MSI-X mode, a read does not clear its interrupt cause register. The commit with the fix says so: "Same lesson the Realtek card taught and this driver did not learn: acknowledge explicitly rather than trusting a clear-on-read you have not read the definition of" ([702753e](https://github.com/irqlevel/nos/commit/702753e); the full story is in [Ten megabits on a gigabit link](/blog/i210-ten-megabits)).

## Keeping it visible

Nothing emulates the RTL8125 or the RTL8168, so none of the tree's test gates can catch this kind of bug in their drivers. A change to either driver is checked by the compiler, and then on the EX44 itself, whose only console is that NIC ([Tests and gates](/docs/testing#the-hardware-nic-drivers)). What the day left behind is a set of tools: [`nicdump`](https://github.com/irqlevel/nos/blob/master/src/crates/drivers/r8125/src/lib.rs), an `irqstat` that counts the Rust drivers' interrupts, receive error counters, and a heartbeat over the one channel that survives.

The tick poll stays as a fallback behind `rxpoll=on` ([Kernel parameters](/docs/kernel-parameters)). Two weeks later its switch silently stopped working. When the network layer was ported to Rust, the tick was wired straight to a poll that had no switch, so every tick polled. Rates held, but on the AX41 a flood now took 70% of each of two CPUs, where before it had taken one. The switch now lives in `DeviceTable::poll_rx` itself ([source](https://github.com/irqlevel/nos/blob/master/src/crates/net/src/device.rs)). `netload-test.py` checks it both ways, booting once without it and once with it, because a switch broken in either direction looks like success ([5eb5535](https://github.com/irqlevel/nos/commit/5eb5535)).
