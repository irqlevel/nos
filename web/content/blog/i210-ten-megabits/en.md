---
title: "Ten megabits on a gigabit link: bringing up the I210"
description: "The AX41's Intel I210 showed the link up, the receiver on and 255 descriptors posted, yet received nothing. Its MAC had latched 10 Mb/s; the PHY ran at 1000."
date: 2026-09-06
tags: [hardware, networking, drivers, debugging, performance, rust]
---

## One chip in the emulator, another in the rack

The AX41 is the second Hetzner server nos runs on. It has an AMD Ryzen 5 3600, no serial port, no IPMI, and a single network card, an Intel I210 (`8086:1533`). So the driver we were bringing up was also the only channel the machine could report through ([Real hardware](/docs/real-hardware#hetzner-ax41-1-ltd-dedicated-server)).

We developed `drivers/igb` on QEMU's emulated 82576 on purpose ([f773b8a](https://github.com/irqlevel/nos/commit/f773b8a)). The two chips share a register map that Intel publishes. As the commit puts it, "the driver that can be debugged under gdb with a packet capture is the same one that will have to come up on hardware whose only console is the NIC it is bringing up".

QEMU caught the first problem. Nothing on the MAC side starts auto-negotiation: the driver has to request it through the PHY. Until negotiation completes, the receiver drops every frame, and the only sign is a single trace event:

```text
e1000x_rx_can_recv_disabled  link_up: 0, rx_enabled 1, pci_master 1
```

The I210 had more to teach us, and on the AX41 there was no gdb to help.

## A PHY with two masters

On the 82576, the driver has the MDIO bus (the management bus to the PHY) to itself. On the I210, it shares that bus with the manageability firmware. Both sides claim the PHY through the `SW_FW_SYNC` register, and that register is itself guarded by a hardware mutex in `SWSM`. So every claim is a lock inside a lock: take the mutex, set the bit, drop the mutex ([c80c1db](https://github.com/irqlevel/nos/commit/c80c1db)). Skipping the claim does not fail cleanly, because two masters on one MDIO bus produce reads that look like real data.

```rust
/// Claim a resource in SW_FW_SYNC, waiting for firmware to let go of it.
fn swfw_acquire(regs: &io::MmioRegion, mask: u32) -> bool {
    let fwmask = mask << SWFW_FW_SHIFT;

    for _ in 0..200 {
        if !hw_semaphore_get(regs) {
            return false;
        }

        let sync = regs.read32(SW_FW_SYNC);
        if sync & (fwmask | mask) == 0 {
            regs.write32(SW_FW_SYNC, sync | mask);
            hw_semaphore_put(regs);
            return true;
        }

        /* Held by the other side: drop the mutex so it can make progress. */
        hw_semaphore_put(regs);
        udelay(5000);
    }

    false
}
```

Every wait in the driver is bounded. If the firmware never lets go, the cost is a failed bring-up and a line in the log, not a hung boot ([92af233](https://github.com/irqlevel/nos/commit/92af233)). The review behind that commit also looked for ways a first boot with no console could fail silently. The driver mapped only 64 KiB of a 512 KiB BAR, so the NVM block at `0x12010` was out of reach (the NVM is the card's non-volatile memory). It also never waited for the NVM to reload after a reset. The MAC address could then read as zero, the driver would reject it, and the card would simply not appear.

That commit also admits something the next igb commit answers: "None of this is verified against the I210 datasheet, which I do not have."

With the NIC itself still being brought up, the boot log needed another way out. The same day we added `disklog`. It writes every traced line to a raw partition, and after the next boot a script reads the log back under Ubuntu ([6996fd4](https://github.com/irqlevel/nos/commit/6996fd4); there is more about this machine in [a post of its own](/blog/ax41-silent-boot)).

## Three fixes from the datasheet

The datasheet is public (Intel document 333016, 870 pages), "and reading it settled in an afternoon what guessing had got wrong on hardware" ([702753e](https://github.com/irqlevel/nos/commit/702753e)). By then the I210's score was 434 million interrupts, 144 million poll rounds, and not one frame received.

**The storm.** `EICR`, the extended interrupt cause register, is cleared by a read only when `GPIE.Multiple_MSIX` is zero. Our driver sets that bit. So a read left the cause in place, and the vector fired again the moment it was re-enabled:

```rust
        let eicr = regs.read32(EICR);
        if eicr & EICR_VECTOR0 == 0 {
            return;
        }

        /* Clear it by writing the bits back. EICR is documented as cleared
         * on read only when GPIE.Multiple_MSIX is zero, and this driver sets
         * that bit -- so a read alone leaves the cause standing and the
         * interrupt re-asserts the moment it is armed again. Measured on an
         * I210 before this line existed: 434 million interrupts and 144
         * million poll rounds, with not one frame received. (333016 rev 3.7,
         * section 8.8.3.) */
        regs.write32(EICR, eicr);
```

The Realtek card had taught us the same lesson the day before, from the other side ([the RTL8125's lost interrupt](/blog/rtl8125-lost-msix-edge)). Acknowledge explicitly, and do not trust a clear-on-read whose definition you have not read.

**The advertisement.** The driver asked the PHY to negotiate without telling it what to advertise. With negotiation on, the PHY ignores the speed and duplex bits in `BMCR` (its basic control register), and the link partner sees only the advertisement registers. So a gigabit port and a gigabit PHY settled on 10BASE-T full duplex. Now the driver writes `ANAR` (the advertisement register) and the 1000BASE-T control register before it restarts negotiation.

**The PHY address.** The earlier review had started reading it from `MDICNFG`, which was pointless. That field addresses an *external* PHY. On a copper part, every MDIC access goes to the integrated PHY regardless (section 3.7.2.2).

On the next boot the storm was gone: 30 interrupts and 10 poll rounds. But it had been hiding the real problem ([23f1e03](https://github.com/irqlevel/nos/commit/23f1e03)). The link still came up at 10 Mb/s, receive was still dead, and the new advertisement had changed nothing. In this round the driver started resetting the PHY before configuring it, because firmware may leave the PHY on a register page where the advertisement writes land somewhere else.

The round also added a tool. `igbdump` now prints what the PHY advertised, what the partner advertised back, and both 1000BASE-T registers. It reads `BMSR` (the basic status register) twice, because its link bit latches low: the first read can still report a link drop that is already over.

## The latch

The next dump showed nothing wrong ([44ce111](https://github.com/irqlevel/nos/commit/44ce111)): link up, receiver enabled, queue enabled, 255 descriptors posted, the filter correct. Yet the head pointer had never moved. The new lines showed what was wrong. Both sides had advertised 1000BASE-T full duplex with no master/slave fault, but the MAC reported the link at 10 Mb/s.

The datasheet explains it in one sentence about auto-speed detection: the speed "is configured only once after the LINK signal is asserted by the PHY". So it is a latch, set once. If the PHY asserts link before negotiation has finished and then settles on a higher speed, the MAC stays at whatever speed it saw first. A MAC clocked for 10 Mb/s, attached to a PHY running at 1000, receives nothing. And every register a driver would think to check reads healthy.

So the driver now works out what the negotiation resolved to. It sets `CTRL.SPEED` and `CTRL.FD` to match, and clears `ASDE` to turn auto-speed detection off. It derives the result from both sides' advertisements, in the order 802.3 resolves them:

```rust
fn resolve_link(regs: &io::MmioRegion, phy_addr: u32) -> Option<(u32, bool)> {
    let bmsr = phy_read(regs, phy_addr, PHY_BMSR)?;
    if bmsr & BMSR_ANEGCOMPLETE == 0 {
        return None;
    }

    let ours = phy_read(regs, phy_addr, PHY_ANAR)?;
    let theirs = phy_read(regs, phy_addr, PHY_ANLPAR)?;
    let gctl = phy_read(regs, phy_addr, PHY_GCTL)?;
    let gstat = phy_read(regs, phy_addr, PHY_GSTAT)?;

    /* In the order 802.3 resolves them: fastest common ability wins, full
     * duplex ahead of half at the same speed. */
    if gctl & GCTL_1000_FULL != 0 && gstat & GSTAT_PARTNER_1000_FULL != 0 {
        return Some((CTRL_SPEED_1000, true));
    }
    // ... 1000 half, then 100 and 10, full duplex ahead of half ...

    None
}
```

The driver does not use the obvious field, `STATUS.ASDV` (the speed the MAC detected), because the datasheet calls it diagnostic. The first version of this fix trusted it. QEMU does not implement that field and reads it as zero, so on QEMU the driver promptly forced a working gigabit link down to 10 Mb/s. The advertisements cannot mislead it that way. This work runs in the receive poll, not in the link-change interrupt, because reading the PHY means polling `MDIC` for milliseconds. On the I210 the driver must also take the semaphore from the firmware first.

When the driver does force the speed, it says so: `igb: MAC was latched at speed {} duplex {}, link resolved to {} duplex {}; forced`. Under QEMU, where the MAC latched the right speed, the code does nothing, and the commit counts that as half the test: "the case where this code should do nothing is as important as the case where it should act."

With that, the I210 works. DHCP gets an address, ARP and TCP traffic flows, and the [UDP shell](/docs/udp-shell) and netconsole are reachable across the internet.

## Then the load

A flood of small UDP datagrams in 64-byte frames, sent to [`netload`](/docs/modules#netload) from a machine in the same data centre, found the rest. We tracked those bugs down the way the RTL8125 had taught us: read the chip, not just the driver.

The driver reported "dropped 0". But a frame that the MAC discards before it reaches for a descriptor never shows up in the driver's counters. So we taught `igbdump` to print the chip's own statistics ([a937267](https://github.com/irqlevel/nos/commit/a937267)). Over 30 seconds, the MAC took 18,392,028 packets off the wire (613k a second) and called all but seven of them good. The driver harvested 5,815,846, and the FIFO-overflow and no-buffer counters stayed at zero ([f66bae0](https://github.com/irqlevel/nos/commit/f66bae0)).

The per-queue drop counter, `RQDPC`, explained the gap. In the next window, 5,067,156 packets reached the queue and 13,311,963 were dropped for lack of a descriptor, which adds up exactly to that window's good count. Meanwhile the ring pointers said the chip owned every descriptor there was: `rdh 101, rdt 100, clean 101, use 100` ([412f2d0](https://github.com/irqlevel/nos/commit/412f2d0)).

Both readings were true, because the descriptors the chip lacked were the ones on the die. `RXDCTL` controls descriptor prefetch, and its enable bit shares the register with three thresholds. Writing the enable bit on its own zeroed all three, and with `PTHRESH` (the prefetch threshold) at zero the chip never prefetched. Now the driver writes the thresholds explicitly instead of preserving them. Silicon resets them to 12/10/1 and QEMU to zero, "and a fix whose correctness depends on which of the two you are running is not one."

A week later we profiled an echo flood of about 577k datagrams a second. A third of the receive CPU went into two register reads, `EICR` and `ICR`, which every interrupt needed to tell a packet from a link change ([1c292cf](https://github.com/irqlevel/nos/commit/1c292cf)). So the queue got its own MSI-X vector whose handler reads nothing: the chip clears the cause itself (`EIAC`) and masks it (`EIAM` under `GPIE.EIAME`). Link changes moved to a second vector.

We reviewed that change before it ran on the AX41, and the review caught a storm in waiting. On the single-vector fallback, the new handler would have left `EICR` uncleared, bringing back the 434-million storm ([f5da442](https://github.com/irqlevel/nos/commit/f5da442)). Another problem showed up after every boot: the first flood lost 755,105 of 2.06 million frames in two seconds. The ring had been filled from the heap before the frame pool existed, and freeing each of those frames cost a TLB shootdown that waited on every other CPU. Now the pool comes up first ([e02dfe3](https://github.com/irqlevel/nos/commit/e02dfe3)).

Here is where things stand now ([8b64625](https://github.com/irqlevel/nos/commit/8b64625)). An echo at 578–581k datagrams a second answers every one, a sink at 615k receives every one, and `dropped-no-descriptor` stays at zero, all on one CPU. The ceiling is the sender: its single-queue NIC falls well short of the 1.49 million packets a second that 1 GbE carries at this size.

## What the emulator could not show

QEMU's 82576 is still the only place the igb driver runs outside a Hetzner rack. So its gate, the tests a change to the driver has to pass, is `tcp-test.py --nic igb` and `netload-test.py --nic igb`, run by hand because CI's QEMU lacks the device ([Tests and gates](/docs/testing#the-hardware-nic-drivers)). None of the bugs above would have shown up there. The emulated MAC latches the right speed, `ASDV` reads zero, no firmware shares the MDIO bus, and receive works even with the prefetch thresholds at zero. The model also has its interrupt mask only at the old e1000 offsets, so the link-change vector has never fired under QEMU ([7868565](https://github.com/irqlevel/nos/commit/7868565)).

What made the real chip debuggable was a dump that prints the PHY's view next to the MAC's, and the chip's counters next to the driver's ([source](https://github.com/irqlevel/nos/blob/master/src/rust/drivers/igb/src/lib.rs)). We read it on a machine whose only console was the very NIC we were debugging.
