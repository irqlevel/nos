---
title: "A console for a laptop with no serial port"
description: "The Dell Latitude has no serial port and no PS/2. How nos got a pixel console and a USB keyboard there, a test that types at it, and a port to Rust."
date: 2026-09-18
tags: [hardware, drivers, rust, testing, qemu, x86-64, boot]
---

## Two ways in, and the laptop has neither

Under QEMU and on the KVM clouds, a kernel gets two things for free: a serial port for its log and an emulated PS/2 keyboard controller, the 8042, for its shell. The Dell Latitude 5480, the one laptop nos boots on, has neither. It is a Skylake-U machine booted under UEFI, and its only console is its own screen and a built-in keyboard that sits on USB ([Real hardware](/docs/real-hardware#dell-latitude-5480)).

So nos needed two drivers there: a text console drawn into the firmware's pixel framebuffer, and a small USB stack for the keyboard. Both landed on 29 August 2026, together with fixes for what a real machine does and QEMU never did. On 18 September we wrote a test that types at the kernel through QEMU, and that evening the USB driver moved from C++ to Rust.

## No text mode under UEFI

Under a legacy BIOS, the console is EGA text mode: character codes written to memory at `0xB8000`. UEFI has no text mode. Its Graphics Output Protocol leaves a linear framebuffer, a plain array of pixels, and the kernel has to draw every glyph itself. The first UEFI boots showed nothing, because the kernel kept writing to `0xB8000`, which on a UEFI machine is ordinary RAM ([8b1ddd1](https://github.com/irqlevel/nos/commit/8b1ddd1)).

GRUB sets a video mode only when the kernel's Multiboot2 header asks for one. So the header got a framebuffer request, marked optional to let BIOS boots keep text mode:

```nasm
    dw 5    ; type: framebuffer
    dw 1    ; flags: optional
    dd 20   ; size
    dd 1024 ; width
    dd 768  ; height
    dd 32   ; depth
```

The screen stayed dark. Without a video driver loaded, GRUB cannot set a mode, so it boots the kernel with no framebuffer and no warning. In the commit's words, a missing `insmod all_video` in `grub.cfg` was "the actual reason UEFI boots came up blind even with the header tag".

The console, [`FbTerm`](https://github.com/irqlevel/nos/blob/master/src/base/drivers/fb_console.cpp), draws an 8x16 font. It keeps a copy of the character grid in RAM and never reads the framebuffer back, because reads from it are slow. Even so, it could not keep up with the kernel log, since the framebuffer was mapped uncached and every store was a bus transaction of its own. Now one entry of the PAT, the x86 table of memory types, is set to write-combining, which lets the CPU gather stores into bursts. The framebuffer uses that type ([6ebed66](https://github.com/irqlevel/nos/commit/6ebed66)).

The framebuffer even broke a self-test. Mapping it moved the kernel's virtual address cursor, so a later test's mapping needed a fresh page-table page, and the kernel never gives those back. The free-page count came back one short, and the UEFI boot panicked long before the shell ([e6afd72](https://github.com/irqlevel/nos/commit/e6afd72)):

```text
TestPageAllocator: AllocMapPages(32) free count mismatch: 257360 expected 257361
PANIC:Main2():main.cpp,808: Self test failed
```

## What the firmware left behind

QEMU starts a kernel with every interrupt entry of the local APIC masked. Firmware on a real laptop hands the local APIC over in "virtual wire" mode, with the old 8259 interrupt controller routed through it and the timer, thermal and performance-counter entries still live. In nos, every unclaimed vector went to one shared stub that panicked without naming the vector, "which is fatal on a real laptop". Now each vector has its own stub, a stray interrupt is named on the serial port and on the screen, and enabling the local APIC masks those entries first ([f584b82](https://github.com/irqlevel/nos/commit/f584b82)).

The missing serial port cost time as well as output. The kernel still wrote every log line to a UART that was not there, waiting up to about a million pauses per character for a transmitter that never reported empty. That was, in the commit's words, "most of why boot crawled" on the laptop. Now the serial driver probes the port's scratch register and skips a port that does not answer ([3006586](https://github.com/irqlevel/nos/commit/3006586)).

## A USB stack for one keyboard

xHCI is the standard register interface of a USB host controller. Software and controller talk through rings of 16-byte TRBs (transfer request blocks) in memory: a command ring, an event ring that the controller writes back, and a transfer ring for each endpoint. Contexts, more structures in memory, describe each device and its endpoints to the controller. A write to a doorbell register tells the controller that a ring has new work ([ff81891](https://github.com/irqlevel/nos/commit/ff81891)).

First the kernel has to take the controller from the firmware. Firmware drives the USB keyboard itself until an OS boots, and it may keep system management interrupts (SMIs) armed on the controller, so its code runs whenever the kernel touches a register. The handover goes through two ownership bits in the controller's USB Legacy Support capability. The kernel sets its bit, waits for the firmware to clear its own, and then turns the SMIs off. Some firmware never lets go, so the wait has a deadline:

```rust
if now_ms() >= deadline {
    /* Some firmware never releases the semaphore.
     * Clear its bit by hand: the controller is about
     * to be reset anyway, and leaving SMIs armed is
     * worse. */
    trace!(0, "Xhci: BIOS handoff timed out, forcing ownership");
    let cur = self.cap_read32(off);
    self.cap.write32(off, (cur & !LEGACY_BIOS_OWNED) | LEGACY_OS_OWNED);
    break;
}
```

The keyboard speaks the HID boot protocol: a fixed 8-byte report, simple enough for a BIOS to read without parsing the device's HID report descriptor. Byte 0 holds the modifier keys, byte 1 is reserved, and bytes 2 to 7 hold the codes of up to six keys held down. A report lists the keys that are down, so the [driver](https://github.com/irqlevel/nos/blob/master/src/crates/drivers/usb/src/hid.rs) compares it with the previous one to find presses. It maps HID codes onto the PS/2 scan codes the rest of the kernel speaks, so to the shell 0x0E still means backspace. It also repeats a held key itself, because a boot keyboard never does.

There is no interrupt handler. A `usb` task wakes every 4 ms, drains the event ring, notices hot-plug and queues the next report, which keeps the keyboard independent of interrupt routing on unfamiliar hardware. Every wait has a timeout, so a dead device cannot hang the task.

The driver also walks hubs up to four tiers deep, reaching each device by its route string, the hub ports on the way to it. A day later that path ran on a Hetzner EX44. Its boot log, which [netconsole](/docs/netconsole) carried off the machine, showed 26 root ports, three hubs and a USB keyboard behind one of them.

## A test that types

Three weeks later, `drivers/usb/` was 3,606 lines of C++ that no test touched: no boot smoke test attaches a USB controller. Yet on the laptop this driver is the only way in, so before the port we wrote its gate, the test the port would have to pass ([33c24ab](https://github.com/irqlevel/nos/commit/33c24ab)). [`scripts/usb-test.py`](https://github.com/irqlevel/nos/blob/master/scripts/usb-test.py) boots the ordinary ISO with an emulated xHCI controller, a keyboard on a root port, a hub with a second keyboard behind it, and a mouse:

```python
argv = ["qemu-system-x86_64", "-m", "1024", "-smp", "2", "-cdrom", iso,
        "-device", "qemu-xhci,id=xhci",
        "-device", "usb-kbd,bus=xhci.0,port=1,id=kbd0",
        "-device", "usb-hub,bus=xhci.0,port=2,id=hub0",
        "-device", "usb-kbd,bus=xhci.0,port=2.1,id=kbd1",
        "-device", "usb-mouse,bus=xhci.0,port=3,id=mouse0",
        "-serial", "file:" + log, "-display", "none",
        "-monitor", "unix:%s,server,nowait" % monitor_path]
```

Then it types. QEMU's monitor, its control console, has a `sendkey` command that presses a key on the emulated keyboard, and the script sends one every 80 ms, as a person would. It reads the shell's answer off the serial log. Seventeen checks cover enumeration through the hub, typed commands and their output, the report count, transfer errors and a keyboard plugged in after boot.

The test is built around a failure that looks like success, and a control experiment showed it catches one. We removed the single doorbell write in `SubmitReport`, the line that tells the controller a report buffer is waiting. The boot log stayed perfect and six checks passed, but the test failed exactly at "a command typed on the usb keyboard reaches the shell". On the laptop that bug would mean no keyboard at all, and nothing else in the suite would have noticed ([Tests and gates](/docs/testing#usb-testpy----typing-at-the-kernel)).

## The port to Rust

The C++ became a Rust crate, `src/crates/drivers/usb`, with a file each for the registers, the rings, the USB descriptors, the per-device state, the HID decoder and [the controller](https://github.com/irqlevel/nos/blob/master/src/crates/drivers/usb/src/controller.rs) ([00a24ef](https://github.com/irqlevel/nos/commit/00a24ef)). It passed 17 of 17 on the first run, and its boot log matched the C++ line for line, down to the slot numbers and route strings.

Two things came out differently on purpose. The C++ read a descriptor by pointing a packed struct at a DMA buffer. The Rust reads each field out of a byte slice at a named offset, because a packed struct over DMA is a misaligned read waiting to happen. And the C++ `usb` command read the device array outside its lock, so the shell could see a report count in the middle of a write. Now the USB task publishes what the command prints, under a lock of its own.

The port still had eight `unsafe` sites, and two follow-ups removed them all that same night. The first gave that published state a lock that owns its data ([01b42d6](https://github.com/irqlevel/nos/commit/01b42d6)). The second replaced three lines of pointer arithmetic, done the way the C++ had done it ([bd03cfb](https://github.com/irqlevel/nos/commit/bd03cfb)):

```rust
let op = MmioRegion::new(unsafe { base.add(cap_length as usize) }, size);
let rt = MmioRegion::new(unsafe { base.add(rts_off) }, size);
let db = MmioRegion::new(unsafe { base.add(db_off) }, size);
```

Each register block starts at an offset the controller reports, yet each window got the size of the whole BAR, the PCI region that holds all the registers, so its bounds check reached past the end of the mapping. `MmioRegion::window` now ends where the mapping ends, and a controller that points a block outside its BAR is refused. The same commit moved the controller table and the task handle from atomic pointers into `TryLock`s, a kcore lock that nobody waits for and whose holder may sleep, as a port reset does. The driver has had no `unsafe` since.

## What QEMU never asks for

The gate covers what QEMU's xHCI does, which is the happy path. The Dell's controller needs the BIOS handoff, may hand back a different context size and number of scratchpad buffers (memory the controller asks the OS to set aside for it), and times its port resets for real. The port's commit says so plainly: those paths are "transcribed carefully but untested until the laptop boots this".

Two gaps are known: a keyboard that speaks only HID report protocol is ignored, and the keyboard LEDs are not driven. Neither stops anyone typing at a shell. What changed is that a bug which breaks the only way into the laptop now fails a test on any machine with QEMU, before it reaches the laptop.
