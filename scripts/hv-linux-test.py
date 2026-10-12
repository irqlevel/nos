#!/usr/bin/env python3
"""hv Linux test: load a real Linux bzImage into a guest under the nos
hypervisor and watch its serial console come out -- once for a set time
(`hv boot`), and then as a VM that runs until it is stopped (`hv start`)
and is typed at while it runs.

This is the third and fourth of the four demos in plans/03-hypervisor.md --
a bzImage printing its early console, and booting to a shell that runs a
command -- and it is a *manual* gate, like the hardware NIC ones: it needs a
bzImage, which is megabytes and which CI cannot build in the time it has, so
it is not in ci.yml and is pointed at a kernel by hand.

Build a small 64-bit guest kernel (a tinyconfig with 8250 serial and an early
console is enough; PCI, ACPI and SMP off), then:

    scripts/hv-linux-test.py --bzimage /path/to/bzImage
    scripts/hv-linux-test.py --bzimage bzImage --initrd initramfs.cpio.gz \\
        --cmdline "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"

What it checks: nos boots, the hv module loads and turns the extension on,
`hv boot` streams the guest's console to the kernel log (hvguest| ...), and
the guest gets far enough into early boot to print the kernel banner and set
up its memory -- under AMD-V, which on a machine with no hardware SVM is
QEMU's TCG (slow: the whole run is a minute or two).

With an initrd, then the long-lived VMs: two started side by side, one
stopped while it boots; a line typed at the other before its shell is up,
answered once it is (`hv exec`), and one typed at its prompt; `hv send` and
`hv wait`; `hv console`; `hv off` refusing to pull the extension from under
a running guest; and `rmmod` stopping that guest itself, with nothing left
on for the next load to find.

With --acpi -- a guest kernel with ACPI, the power button and evdev -- also
that it takes the machine's ACPI tables: the boot finds the RSDP, the PM
timer, the PCI host bridge and the power button, and says nothing of a
firmware bug; its reboots go through the FADT's reset register; its SCI is
the 8259's IRQ 9; and `hv stop` presses its power button, which busybox's
acpid answers with a poweroff -- S5 -- while `rmmod` waits out a guest that
hears the button and does nothing.

Exit code 0 = every required marker appeared.
"""

import argparse
import importlib.util
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("hvt", os.path.join(HERE, "hv-test.py"))
hvt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hvt)
pt = hvt.pt

# The lines the guest has to reach: the banner, and enough of early setup to
# show it is a real kernel running and not just the decompressor stub. Each
# is matched inside the guest's console as `hv boot` reports it, between its
# ttyS0 markers, so nothing of nos's own console can satisfy one.
REQUIRED = [
    r"Linux version",
    r"Command line:",
    r"NX \(Execute Disable\) protection: active",
    r"console \[ttyS0\] enabled",
]
# A later line that shows the guest got well past the banner into device
# setup; not required, but reported when it appears.
NICE = r"Kernel command line:"

# With an initramfs, the fourth demo: the guest runs its init and reaches an
# interactive shell over the emulated UART. These are checked only when an
# initrd is given (the init here is a busybox that prints and execs a shell).
SHELL_MARKERS = [
    r"Run /init as init process",
    r"BusyBox",
]

# What no boot may say, with ACPI or without: that its kernel took an MSR
# for there and was refused it at an access not ready for a refusal -- the
# policy answers what the guest's CPUID says it has and refuses the rest
# (`hv/src/policy.rs`) -- and the panic a refused read of a machine-check
# register is.
NEVER = [
    r"unchecked MSR access error",
    r"MCA architectural violation",
]

# With --acpi: what the boot says of the tables it was given, in the order
# it says it -- the RSDP where the loader put it, the FADT's PM timer, the
# namespace up, S5 found, PIC mode, the host bridge the DSDT describes and
# PCI's interrupts routed by its _PRT, the fixed power button, and the PM
# timer a clocksource -- and what it must not say: nothing of a firmware
# bug, and nothing of a register the tables gave it that did not work.
ACPI_BOOT = [
    r"ACPI: RSDP 0x00000000000E0000 000024 \(v02 NOS   \)",
    r"ACPI: FACP 0x",
    r"ACPI: DSDT 0x",
    r"ACPI: FACS 0x",
    r"ACPI: PM-Timer IO Port: 0x608",
    r"ACPI: Interpreter enabled",
    r"ACPI: PM: \(supports S0 S5\)",
    r"ACPI: Using PIC for interrupt routing",
    r"ACPI: PCI Root Bridge \[PCI0\] \(domain 0000 \[bus 00-ff\]\)",
    r"PCI: Using ACPI for IRQ routing",
    r"input: Power Button as ",
    r"clocksource: acpi_pm: mask: 0xffffff",
]
ACPI_NEVER = [
    r"ACPI (BIOS )?(Error|Warning)",
    r"\[Firmware Bug\]",
    r"setting ELCR",
    r"PM-Timer (had inconsistent|failed consistency|running at invalid)",
    # The clocksource watchdog, which has the PM timer to check the TSC
    # against now: by the skew, or by a read-back it took too long for. (Not
    # the bring-up's warp test, whose "TSCs unsynchronized" is the host's TSC
    # and no business of ACPI's: TCG's differ between its CPUs.)
    r"(clocksource 'tsc' as unstable|marking tsc unstable|TSC unstable due to clocksource watchdog)",
    r"no GSI",
]

DEFAULT_CMDLINE = "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"
# The image two guests' disks claim at once, on nos's root.
CLAIMED_IMAGE = "claimed.img"
CLAIMED_BYTES = 1 << 20
# A guest of more than one CPU needs its local APIC: the same, without
# nolapic. (The loader adds noapic itself: the machine has no IO-APIC.)
# nos's own CPUs: two cores of two threads each, so that where a guest's CPUs
# are placed has a topology to go wrong on -- APIC IDs 0 and 1 are one core,
# 2 and 3 the other. A guest's cores are its own (`hv cores`): one of two
# CPUs fills a core's two threads, and leaves the other core to the next.
NOS_SMP = "4,sockets=1,cores=2,threads=2"
DEFAULT_CMDLINE_SMP = "earlyprintk=serial,ttyS0,115200 console=ttyS0 no_timer_check"

# The last line of /etc/rc, and how the console shows it has run: its echo
# and the line it prints. Once the shell has the console, the kernel's own
# log -- "rc: /etc/rc done", and every trace line -- goes to dmesg and not
# to the console, so the end is marked with a command's output instead.
RC_LAST = "version"
RC_DONE = re.compile(r"(?m)^> version\n.+\n")
# The module's own lines from the kernel log, printed to the console: what
# the vCPU tasks and the unload say, which no command's output does.
RC_LOG = "dmesg hv:"


def interrupt_check(args, text, queue):
    """How a guest's virtio device interrupts it, from its /proc/interrupts:
    with a local APIC -- more than one CPU -- by MSI-X, a vector a queue,
    or, told pci=nomsi, by its INTx line through the IO-APIC, a level; with
    none, through the 8259, as a guest of one CPU always was."""
    if args.cpus > 1 and args.ioapic and "pci=nomsi" in args.cmdline.split():
        return ("the virtio device interrupts by its INTx line, level-triggered through the IO-APIC",
                re.search(r"IO-APIC\s+\d+-fasteoi\s+virtio\d+", text) is not None, text[-600:])
    if args.cpus > 1:
        return ("the virtio device interrupts by MSI-X, a vector for its %s queue" % queue,
                re.search(r"PCI-MSIX\S*\s+\S+\s+virtio\d+-%s" % queue, text) is not None, text[-600:])
    return ("the virtio device interrupts through the 8259", re.search(r"XT-PIC\s+virtio\d+", text) is not None,
            text[-600:])


def cpus_opt(args):
    """`cpus=N` for a guest of N CPUs, or nothing for one; `xapic` for a
    guest whose local APICs come out of reset in xAPIC mode; and `ioapic`
    for one with an IO-APIC."""
    return ((" cpus=%d" % args.cpus if args.cpus > 1 else "") + (" xapic" if args.xapic else "")
            + (" ioapic" if args.ioapic else ""))


def vm_commands(args):
    """The long-lived VM phase, as /etc/rc lines, and what each must print.

    Each check is (command, occurrence, what, pattern, must): the output of
    the occurrence-th run of that line must (or must not) match."""
    c = cpus_opt(args)
    # How the guest resets its machine: with ACPI, by the FADT's reset
    # register first; without, by the 8042's reset line.
    reset_by = r"0x06 to port 0xcf9" if args.acpi else r"0xfe to port 0x64"
    start0 = "hv start /bzImage mem=%d%s initrd=/initrd cmdline=%s" % (args.mem, c, args.cmdline)
    start1 = "hv start /bzImage mem=64%s cmdline=%s" % (c, args.cmdline)
    # A third guest while the two hold both cores: refused, its cores being
    # its own -- before it takes a number, so the numbers after are as they
    # were.
    start_full = "hv start /bzImage mem=80%s cmdline=%s" % (c, args.cmdline)
    # Typed while vm 0 still boots: held until its shell asks for input,
    # and answered at the prompt printed after the line went in.
    exec_early = "hv exec 0 secs=%d id" % args.vm_secs
    exec_prompt = "hv exec 0 uname -r"
    send = r"hv send 0 echo nos$((6*7))nos\n"
    wait = "hv wait 0 secs=120 nos42nos"
    # A guest that reboots itself: no init, so it panics, and panic=1 makes
    # the panic a reboot a second later. `hv wait` for text it never prints
    # returns when the guest stops, saying why.
    start2 = "hv start /bzImage mem=64%s cmdline=%s panic=1" % (c, args.cmdline)
    wait_reboot = "hv wait 2 secs=300 Rebooting in"
    wait_stop = "hv wait 2 secs=300 nos-never-printed"
    # The same stopped guest booted again by hand; then one with `restart`,
    # booted again by itself each time it resets -- until the sixth reset in
    # a minute says it is a loop; then the running guest reset by hand, and
    # typed at again once it is back.
    wait_stop2 = "hv wait 2 secs=300 nos-never-printed-2"
    start3 = "hv start /bzImage mem=64%s restart cmdline=%s panic=1" % (c, args.cmdline)
    wait_loop = "hv wait 3 secs=500 nos-never-printed-3"
    exec_again = "hv exec 0 secs=%d id" % args.vm_secs
    # A guest whose `input=` its first boot never reads: held until its
    # shell's first prompt, and restarted before that -- so the boot after
    # throws it away unfed, every time -- and then a line typed at it must be
    # answered all the same. (`hv exec` once waited for good on any line
    # after input a boot had thrown away.) Its number follows the guests
    # before it, two more with --acpi.
    lost_vm = 6 if args.acpi else 4
    start_lost = "hv start /bzImage mem=64%s initrd=/initrd input=id\\n cmdline=%s" % (c, args.cmdline)
    restart_lost = "hv restart %d" % lost_vm
    exec_after_lost = "hv exec %d secs=%d echo nos-after-lost" % (lost_vm, args.vm_secs)
    stop_lost = "hv stop %d" % lost_vm
    # One image under two guests' disks: a writer has it alone, readers
    # share it -- among the VMs; nos's own shell is not held to it. The
    # guests have no initrd and get no further than their panic, holding
    # their disks the while (`panic=0`: a `--cmdline` that reboots on one
    # would take the claim away); a start refused still takes a number.
    # Stopped without the button, which a panicked guest does not hear.
    claim_vm = lost_vm + 1
    claim_rw = "hv start /bzImage mem=64%s disk=/%s cmdline=%s panic=0" % (c, CLAIMED_IMAGE, args.cmdline)
    claim_ro = "hv start /bzImage mem=64%s disk=/%s:ro cmdline=%s panic=0" % (c, CLAIMED_IMAGE, args.cmdline)
    claim_stop = ["hv stop %d secs=0" % (claim_vm + k) for k in (0, 3, 4)]
    # Three guests at once beside vm 0, on a machine of two cores: their
    # cores shared for it (`hv cores shared`), which is not what this tests,
    # and their own again after.
    claim_lines = ["hv cores shared", claim_rw, claim_rw, claim_ro, claim_stop[0],
                   claim_ro, claim_ro, claim_rw, claim_stop[1], claim_stop[2], "hv cores own"]
    # A guest whose console goes to nos's log, printing as fast as it can:
    # the log takes a boot's worth and then a rate (`modules/hv/src/guest.rs`,
    # LogLine), and says how many it left out; the guest's own console keeps
    # all of it. dmesg's count of the lines it has pushed out is read just
    # before the flood and just after (`flood_checks`).
    fv = flood_vm(args)
    flood_start = "hv start /bzImage mem=64%s initrd=/initrd log cmdline=%s" % (c, args.cmdline)
    flood_ready = "hv exec %d secs=%d echo flood-ready" % (fv, args.vm_secs)
    # The marker the guest prints is not the one typed, which the shell's
    # echo would show at once: it waits for the end of the flood.
    flood_send = r"hv send %d seq 1 %d; echo flood-$((6*7))\n" % (fv, FLOOD_LINES)
    flood_wait = "hv wait %d secs=%d flood-42" % (fv, args.vm_secs)
    flood_console = "hv console %d bytes=200" % fv
    flood_stop = "hv stop %d secs=0" % fv
    flood_lines = [flood_start, flood_ready, FLOOD_BEFORE, flood_send, flood_wait, FLOOD_AFTER,
                   flood_console, flood_stop]
    # Where a guest's CPUs run, as hv start says: its one host CPU, or a list
    # of as many as it has.
    on = r"cpu \d+"
    # A guest of several CPUs: every one online, each taking its own APIC
    # timer's interrupts, and the IPIs between them -- rescheduling and
    # function calls -- delivered.
    smp_lines = []
    smp_checks = []
    if args.cpus > 1:
        on = r"cpus \d+(,\d+){%d}" % (args.cpus - 1)
        online = "hv exec 0 cat /sys/devices/system/cpu/online"
        irqs = "hv exec 0 grep -e LOC -e RES -e CAL /proc/interrupts"
        smp_lines = [online, irqs]
        smp_checks = [
            (online, 0, "every one of the guest's %d CPUs is online" % args.cpus,
             r"(?m)^0-%d$" % (args.cpus - 1), True),
            (irqs, 0, "each CPU took its local timer's interrupts",
             r"(?m)^\s*LOC:" + r"\s+[1-9]\d*" * args.cpus + r"\s", True),
            (irqs, 0, "and IPIs went between them: rescheduling ones",
             r"(?m)^\s*RES:(\s+\d+)*\s+[1-9]\d*", True),
            ("hv list", 0, "hv list says where each of its CPUs runs",
             r"vm 0  running  cpus \d+(,\d+){%d}  " % (args.cpus - 1), True),
        ]
        if args.cpus == 2:
            # Its cores its own, its CPUs filling their threads: one core's
            # two, a vCPU a thread, and the other core left to vm 1.
            smp_checks.append(("hv list", 0, "its two CPUs are one core's two threads, a core of its own",
                               r"vm 0  running  cpus (2,3|3,2|0,1|1,0)  ", True))
        if args.ioapic:
            # The PC's own lines through the IO-APIC: the timer's on pin 2,
            # which the boot counts its ticks by before the local APICs'
            # timers take over, and the serial port's, which the shell's
            # every line went out by -- each an edge, and taken.
            pins = "hv exec 0 grep IO-APIC /proc/interrupts"
            smp_lines.append(pins)
            taken = r"(\s+\d+)*\s+[1-9]\d*(\s+\d+)*"
            smp_checks += [
                (pins, 0, "the timer's interrupts came through the IO-APIC's pin 2",
                 r"(?m)^\s*0:%s\s+IO-APIC\s+2-edge\s+timer" % taken, True),
                (pins, 0, "and the serial port's through its pin 4",
                 r"(?m)^\s*4:%s\s+IO-APIC\s+4-edge\s+ttyS0" % taken, True),
            ]
    # With ACPI, a guest whose power button busybox's acpid answers with a
    # poweroff: its SCI and PM timer looked at, then stopped by hv stop,
    # which presses the button and has it turn itself off.
    acpi_lines = []
    acpi_checks = []
    if args.acpi:
        # Its own words, in another order: a line that is start0's again
        # would find start0's output.
        start4 = "hv start /bzImage initrd=/initrd mem=%d%s cmdline=%s" % (args.mem, c, args.cmdline)
        handler = "/etc/acpi/PWRF/00000080"
        irq9 = "hv exec 4 grep -e acpi -e virtio /proc/interrupts"
        clocks = "hv exec 4 cat /sys/devices/system/clocksource/clocksource0/available_clocksource"
        # acpid reads the button from its input device, whose node only a
        # devtmpfs makes -- which the initramfs's /init does not mount, as
        # the disk phase knows too: without it, /dev/input is not there and
        # the button goes unanswered.
        inputs = "hv exec 4 ls /dev/input"
        acpi_lines = [start4, "hv exec 4 secs=%d mkdir -p /etc/acpi/PWRF" % args.vm_secs,
                      "hv exec 4 mount -t devtmpfs devtmpfs /dev", inputs,
                      "hv exec 4 echo '#!/bin/sh' > %s" % handler,
                      "hv exec 4 echo 'poweroff -f' >> %s" % handler,
                      "hv exec 4 chmod +x %s" % handler,
                      "hv exec 4 acpid -l /acpid.log -p /acpid.pid", irq9, clocks, "hv stop 4"]
        # And one that hears the button and has nothing to answer it: at its
        # shell, so its kernel has the event enabled; stopped when its time
        # is up, and saying so.
        start5 = "hv start /bzImage initrd=/initrd mem=64%s cmdline=%s" % (c, args.cmdline)
        acpi_lines += [start5, "hv exec 5 secs=%d id" % args.vm_secs, "hv stop 5 secs=5"]
        acpi_checks = [
            (start4, 0, "vm 4 starts, to be shut down by its power button", r"hv: vm 4 started on %s " % on, True),
            (inputs, 0, "its power button is an input device acpid can read", r"(?m)^event\d+\s*$", True),
            (irq9, 0, "its SCI comes through the IO-APIC's pin 9, a level" if args.ioapic
             else "its SCI comes through the 8259, as IRQ 9",
             r"(?m)^\s*9:(\s+\d+)+\s+%s\s+acpi" % (r"IO-APIC\s+9-fasteoi" if args.ioapic else "XT-PIC"), True),
            (clocks, 0, "the PM timer is one of its clocksources", r"acpi_pm", True),
            ("hv stop 4", 0, "hv stop presses its power button, and acpid has it turn itself off",
             r"hv: vm 4 stopped -- the guest powered itself off, S5 by its ACPI PM1 control register", True),
            ("hv stop 4", 0, "the report: ACPI taken, the SCI raised, the power button pressed once",
             r"acpi\s+taken by the guest's OS; [1-9]\d* SCIs, the power button pressed 1 times", True),
            ("hv stop 5 secs=5", 0, "a guest that hears its power button and does nothing is stopped when its time is up",
             r"hv: vm 5 stopped -- on request -- its power button went unanswered for 5 s", True),
        ]
    # vm 1, stopped mid-boot: a kernel without ACPI never hears its power
    # button, and is stopped at once all the same; one with ACPI may have got
    # as far as hearing it, and is stopped without asking.
    stop1 = "hv stop 1 secs=0" if args.acpi else "hv stop 1"
    stopped1 = (r"(?m)hv: vm 1 stopped -- on request\s*$" if args.acpi
                else r"hv: vm 1 stopped -- on request -- nothing in it listens to its power button")
    # After the reload, two guests at their shells stopped together: every
    # power button pressed at once, and each guest's stop saying what became
    # of its press -- a kernel with ACPI hears it and does nothing, one
    # without never hears it.
    start_a = "hv start /bzImage mem=64%s initrd=/initrd cmdline=%s" % (c, args.cmdline)
    start_b = "hv start /bzImage mem=72%s initrd=/initrd cmdline=%s" % (c, args.cmdline)
    stop_all = "hv stop all secs=3"
    all_note = (r"its power button went unanswered for 3 s" if args.acpi
                else r"nothing in it listens to its power button")
    lines = ["hv help", start0, start1, "hv list", start_full, "hv cores", stop1, exec_early, exec_prompt] + smp_lines + [
             send, wait, "hv console 0 bytes=400", "hv list",
             start2, wait_reboot, wait_stop, "hv list",
             "hv restart 2", wait_stop2, "hv list", "hv stop 2",
             start3, wait_loop, "hv list", "hv stop 3"] + acpi_lines + [
             "hv restart 0", exec_again, "hv list",
             start_lost, restart_lost, exec_after_lost, stop_lost] + claim_lines + flood_lines + [
             "hv off", "rmmod hv", "insmod /hv.ko", "hv",
             "hv on", start_a, start_b, "hv exec 0 secs=%d id" % args.vm_secs, "hv exec 1 secs=%d id" % args.vm_secs,
             stop_all, "rmmod hv", RC_LOG]

    checks = [
        ("hv help", 0, "hv help lists the vm commands", r"hv exec <id>", True),
        (start0, 0, "vm 0 starts", r"hv: vm 0 started on %s " % on, True),
        (start1, 0, "vm 1 starts beside it", r"hv: vm 1 started on %s " % on, True),
        ("hv list", 0, "hv list shows vm 0 running", r"vm 0  running", True),
        ("hv list", 0, "hv list shows vm 1 running", r"vm 1  running", True),
        ("hv list", 0, "each on cores of its own: one on CPUs 2 and 3's, the other on 0 and 1's",
         r"(?s)vm 0  running  cpus? [23](,[23])?  .*vm 1  running  cpus? [01](,[01])?  "
         r"|vm 0  running  cpus? [01](,[01])?  .*vm 1  running  cpus? [23](,[23])?  ", True),
        (start_full, 0, "a third, both cores held, is refused for want of a core of its own",
         r"hv: %d cpus asked for, and the cores no other guest has a CPU on hold 0 -- a guest's cores are its own"
         % args.cpus, True),
        ("hv cores", 0, "hv cores says a guest's cores are its own, and which guest holds which CPUs",
         r"cores own -- a guest is given whole cores[\s\S]*vm 0: cpu[\s\S]*vm 1: cpu", True),
        ("hv cores shared", 0, "hv cores shared lets guests share a core's threads",
         r"cores shared -- guests may be given threads of one core", True),
        ("hv cores own", 0, "and hv cores own gives each its own again", r"cores own -- a guest is given whole cores",
         True),
        (stop1, 0, "vm 1 stops on request, mid-boot", stopped1, True),
        (exec_early, 0, "a line typed during boot is answered at the prompt after it",
         r"# id\nuid=0 gid=0\n", True),
        (exec_early, 0, "... and nothing was left waiting", r"hv: vm 0: ", False),
        (exec_prompt, 0, "a line typed at the prompt comes back whole", r"(?m)^uname -r\n\d+\.\d+", True),
        (send, 0, "hv send queues", r"hv: vm 0: \d+ bytes queued", True),
        (wait, 0, "hv wait sees what the sent line printed", r'hv: vm 0 printed "nos42nos"', True),
        ("hv console 0 bytes=400", 0, "hv console has it too", r"nos42nos", True),
        ("hv list", 1, "vm 0 still running", r"vm 0  running", True),
        ("hv list", 1, "vm 1 gone from the list", r"vm 1 ", False),
        (start2, 0, "vm 2 starts, to panic and reboot", r"hv: vm 2 started on %s " % on, True),
        (wait_reboot, 0, "vm 2 panics and goes to reboot", r'hv: vm 2 printed "Rebooting in"', True),
        (wait_stop, 0, "its reset stops it, and says so",
         r"hv: vm 2 stopped without printing .* -- the guest asked for a reset, %s" % reset_by, True),
        ("hv list", 2, "hv list keeps it, stopped, with its reason",
         r"vm 2  stopped .* -- the guest asked for a reset", True),
        ("hv stop 2", 0, "hv stop takes it off the list", r"hv: vm 2 stopped -- the guest asked for a reset", True),
        ("hv restart 2", 0, "hv restart boots a stopped guest again", r"hv: vm 2 restarted", True),
        (wait_stop2, 0, "... which resets again, and stops again",
         r"hv: vm 2 stopped without printing .* -- the guest asked for a reset", True),
        ("hv list", 3, "hv list counts the restart", r"vm 2  stopped .* restarts 1 ", True),
        (start3, 0, "a guest with restart starts", r"hv: vm 3 started on %s .*restarted when it resets" % on, True),
        (wait_loop, 0, "it is booted again at each reset, until the loop is called one",
         r"hv: vm 3 stopped without printing .* -- the guest asked for a reset.* -- reset 6 times in 60 s, left stopped", True),
        ("hv list", 4, "five restarts before it was left stopped", r"vm 3  stopped .* restarts 5 ", True),
        ("hv restart 0", 0, "hv restart resets the running guest", r"hv: vm 0 restarted", True),
        (exec_again, 0, "and its new boot answers a line typed during it", r"# id\nuid=0 gid=0\n", True),
        ("hv list", 5, "running again, one restart counted", r"vm 0  running .* restarts 1 ", True),
        (start_lost, 0, "a guest started with input for its first prompt", r"hv: vm %d started" % lost_vm, True),
        (restart_lost, 0, "restarted before that prompt, the input thrown away unread",
         r"hv: vm %d restarted" % lost_vm, True),
        (exec_after_lost, 0, "a line typed after a boot threw input away is answered all the same",
         r"(?m)^nos-after-lost$", True),
        # The guest runs the line either way: what broke was `hv exec`, which
        # waited its whole time for a feed that never came and then said the
        # line was not read -- under its answer, which the check above finds.
        (exec_after_lost, 0, "... at its prompt, not after waiting out its time",
         r"hv: vm %d: " % lost_vm, False),
        (stop_lost, 0, "and it stops", r"hv: vm %d stopped" % lost_vm, True),
        (claim_rw, 0, "a guest's disk on an image", r"hv: vm %d started" % claim_vm, True),
        (claim_rw, 1, "a second guest's disk writing it is refused, saying whose it is",
         r"hv: vm %d not started -- /%s: vm %d's disk writes it" % (claim_vm + 1, CLAIMED_IMAGE, claim_vm), True),
        (claim_ro, 0, "and one reading it too",
         r"hv: vm %d not started -- /%s: vm %d's disk writes it" % (claim_vm + 2, CLAIMED_IMAGE, claim_vm), True),
        (claim_stop[0], 0, "the writer stopped, its claim goes with it", r"hv: vm %d stopped" % claim_vm, True),
        (claim_ro, 1, "then a guest reading it", r"hv: vm %d started" % (claim_vm + 3), True),
        (claim_ro, 2, "and another beside it: readers share it", r"hv: vm %d started" % (claim_vm + 4), True),
        (claim_rw, 2, "but none writes it under them",
         r"hv: vm %d not started -- /%s: vm (%d|%d)'s disk reads it, and this one would write it"
         % (claim_vm + 5, CLAIMED_IMAGE, claim_vm + 3, claim_vm + 4), True),
        (claim_stop[1], 0, "the readers stopped", r"hv: vm %d stopped" % (claim_vm + 3), True),
        (claim_stop[2], 0, "both", r"hv: vm %d stopped" % (claim_vm + 4), True),
        (flood_start, 0, "a guest whose console goes to nos's log", r"hv: vm %d started" % fv, True),
        (flood_ready, 0, "reaches its shell", r"flood-ready", True),
        (flood_wait, 0, "and prints %d lines as fast as it can" % FLOOD_LINES, r"flood-42", True),
        (flood_console, 0, "its own console keeps all of them", r"(?m)^%d\s*$" % FLOOD_LINES, True),
        (flood_stop, 0, "and it stops", r"hv: vm %d stopped" % fv, True),
        ("hv off", 0, "hv off will not pull the extension from under vm 0",
         r"hv: vm 0 is running on cpu \d+ -- hv stop it first", True),
        ("hv", 0, "the next load finds it off everywhere", r"on for cpu none of", True),
        (RC_LOG, 0, "vm 1's vCPU said how it ended", r"hv: vm 1 stopped after \d+ ms -- on request", True),
        (RC_LOG, 0, "rmmod stopped vm 0 itself, and only then turned the extension off",
         r"hv: vm 0 stopped for the unload -- on request[\s\S]*hv: unloaded, extension off for cpu mask", True),
        (RC_LOG, 0, "and without asking it first", r"(?m)hv: vm 0 stopped for the unload -- on request\s*$", True),
        (RC_LOG, 0, "no unload left the extension on", r"hv: WARNING", False),
        (stop_all, 0, "hv stop all presses every button at once, and says what became of each press",
         r"hv: vm 0 stopped -- on request -- %s[\s\S]*hv: vm 1 stopped -- on request -- %s" % (all_note, all_note), True),
    ] + smp_checks + acpi_checks
    return lines, checks


def sections(txt):
    """What each /etc/rc line printed, in order: (line, output) pairs, the
    output being everything on the console up to the next line rc echoes --
    its own output as printed, then the same again in the log, and the
    kernel's own lines in between."""
    parts = re.split(r"(?m)^> (.*)$", txt)
    return [(parts[i].strip(), parts[i + 1]) for i in range(1, len(parts) - 1, 2)]


def output_of(secs, line, occurrence):
    runs = [out for (cmd, out) in secs if cmd == line]
    return runs[occurrence] if occurrence < len(runs) else None


def rootfs(tmp, bzimage, initrd, rc):
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"), exist_ok=True)
    shutil.copy(hvt.module("x86_64"), rootdir)
    shutil.copy(bzimage, os.path.join(rootdir, "bzImage"))
    if initrd:
        shutil.copy(initrd, os.path.join(rootdir, "initrd"))
    # An image for two guests' disks to claim at once (`vm_commands`).
    with open(os.path.join(rootdir, CLAIMED_IMAGE), "wb") as f:
        f.truncate(CLAIMED_BYTES)
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/hv-linux-test.py\n" + "\n".join(rc) + "\n")

    image = os.path.join(tmp, "root.img")
    # A roomy filesystem: the kernel and initrd have to fit on it.
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "128", rootdir],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)
    return image


# How many lines the flooding guest prints: more than nos's log takes in a
# boot's worth and the time the flood takes under TCG, by far.
FLOOD_LINES = 20000
# dmesg's count of the lines it has pushed out, read just before the flood
# and just after: the newest line alone, through a filter it does not match.
FLOOD_BEFORE = "dmesg 1 flood-before"
FLOOD_AFTER = "dmesg 1 flood-after"
# dmesg's slots (`Dmesg::MaxMsgs`, src/cpp/kernel/dmesg.h): how many more
# lines it can have taken than its count of lines pushed out grew by.
DMESG_SLOTS = 2048


def flood_vm(args):
    """The flooding guest's number: after the lost-input guest and the six
    starts of the claim checks, each of which takes a number."""
    return (6 if args.acpi else 4) + 1 + 6


def lost_lines(out):
    """dmesg's count of the lines it has pushed out, as a dmesg prints it:
    nothing printed is none yet."""
    m = re.search(r"dmesg: (\d+) earlier lines lost", out)
    return int(m.group(1)) if m else 0


def flood_checks(args, secs):
    """What nos's log took of the flooding guest's console: fewer than half
    its lines -- the growth of dmesg's count of the lines it pushed out over
    the flood, and its slots on top -- and of the lines left out, a notice
    once a second at most that says exactly how many: checked against the
    gaps in the numbers seq printed since the notice before, over what of
    the flood the log still holds."""
    fv = flood_vm(args)
    before, after = output_of(secs, FLOOD_BEFORE, 0), output_of(secs, FLOOD_AFTER, 0)
    if not pt.check("dmesg counted its lines before the flood and after", before is not None and after is not None,
                    "no output for %r or %r" % (FLOOD_BEFORE, FLOOD_AFTER)):
        return
    took = lost_lines(after) - lost_lines(before) + DMESG_SLOTS
    pt.check("nos's log took fewer than half of the flood's %d lines" % FLOOD_LINES,
             took < FLOOD_LINES // 2, "%d lines at most" % took)

    # The log's lines from the guest, oldest first, each with its time.
    log = output_of(secs, RC_LOG, 0) or ""
    lines = re.findall(r"(?m)^\d+:(\d+\.\d+):.*?hvvm%d\| (.*?)\s*$" % fv, log)
    checked, wrong, told_at, too_soon = 0, [], None, []
    left = last = said = said_at = None
    for (at, text) in lines:
        notice = re.fullmatch(r"\.\.\. (\d+) lines not logged", text)
        if notice:
            said, said_at = int(notice.group(1)), float(at)
        elif not text.isdigit():
            # The shell's lines, before the flood and after: a notice is
            # held to the gaps between two of seq's lines only -- and the
            # one the guest's stop gives, whatever the time, to neither.
            left = last = said = None
        else:
            n = int(text)
            if last is not None and left is not None:
                left += n - last - 1
            if said is not None:
                # The line the notice came before: the notice counts every
                # line left out since the one the notice before came with.
                if left is not None:
                    checked += 1
                    if left != said:
                        wrong.append((said, left, n))
                if told_at is not None and said_at - told_at < 1.0:
                    too_soon.append(said_at)
                told_at = said_at
                left, said = 0, None
            last = n
    pt.check("and said how many it left out, to the line",
             checked >= 3 and not wrong,
             "%d notices held to seq's numbers; said, left out, before: %s" % (checked, wrong[:8]))
    pt.check("once a second at most", checked >= 3 and not too_soon, "notices less than a second apart: %s" % too_soon[:8])


def check_boot(args, txt):
    """The one-shot `hv boot`: its report block, and the console in it."""
    if not pt.check("the guest ran and its console came back",
                    "--- end ttyS0 ---" in txt or "the guest printed nothing" in txt,
                    txt[-1500:]):
        return
    block = txt[txt.find("--- ttyS0 ---"):txt.find("--- end ttyS0 ---")] if "--- ttyS0 ---" in txt else ""
    for m in REQUIRED:
        pt.check("the guest printed: %s" % m, re.search(m, block) is not None, block[-1500:])
    if re.search(NICE, block):
        print("note: the guest reached %r" % NICE)
    if args.initrd:
        for m in SHELL_MARKERS:
            pt.check("the guest booted to a shell: %s" % m, re.search(m, block) is not None,
                     block[-1500:])
    for m in args.expect:
        pt.check("the console shows: %s" % m, re.search(re.escape(m), block) is not None,
                 block[-1500:])
    for m in NEVER:
        bad = re.search(r"(?m)^.*%s.*$" % m, block)
        pt.check("the console says nothing like: %s" % m, bad is None, bad.group(0) if bad else "")
    # What its kernel asked of MSRs nobody names, and was refused -- probes
    # it was ready for, else the check above fails: the report lists the
    # first few. On a machine of another vendor or family, other probes.
    refused = re.findall(r"(?m)^\s+#GP\s+((?:rd|wr)msr 0x[0-9a-f]+)", txt)
    if refused:
        print("note: the guest was refused %s" % ", ".join(refused))
    if args.acpi:
        for m in ACPI_BOOT:
            if args.ioapic:
                # Its interrupts routed through the IO-APIC, not the 8259.
                m = m.replace("Using PIC for", "Using IOAPIC for")
            pt.check("the guest took its ACPI: %s" % m, re.search(m, block) is not None, block[-1500:])
        if args.cpus > 1:
            # Its CPUs from the MADT -- and, given an IO-APIC there too, its
            # whole SMP configuration.
            config = (r"Using ACPI \(MADT\) for SMP configuration information" if args.ioapic
                      else r"Using ACPI for processor \(LAPIC\) configuration")
            pt.check("and found its CPUs in the MADT",
                     re.search(r"ACPI: APIC 0x[\s\S]*" + config, block) is not None, block[-1500:])
        for m in ACPI_NEVER:
            bad = re.search(r"(?m)^.*%s.*$" % m, block)
            pt.check("and said nothing like: %s" % m, bad is None, bad.group(0) if bad else "")
    if args.cpus > 1 and (args.xapic or "nox2apic" in args.cmdline.split()):
        # Its local APICs in xAPIC mode -- left there by firmware, or taken
        # there by the kernel -- and every access of theirs a nested fault
        # on the page, decoded and performed.
        pt.check("its local APICs were reached through the xAPIC page, each access decoded and performed",
                 re.search(r"mmio\s+[1-9]\d* accesses performed", txt) is not None, txt[-2500:])
    if args.ioapic:
        # Its IO-APIC found where the tables put it, version 0x20's 24 pins
        # its global interrupts 0 up, and interrupts sent through it.
        pt.check("the guest found its IO-APIC: version 32, at 0xfec00000, GSI 0-23",
                 re.search(r"IOAPIC\[0\]: apic_id \d+, version 32, address 0xfec00000, GSI 0-23", block)
                 is not None, block[-1500:])
        pt.check("and the report says interrupts went through it",
                 re.search(r"ioapic\s+[1-9]\d* interrupts sent", txt) is not None, txt[-2500:])
    if args.cpus > 1:
        # Linux's own count, once it has started every CPU it was told of.
        pt.check("the guest brought up all %d of its CPUs" % args.cpus,
                 re.search(r"smp: Brought up \d+ nodes?, %d CPUs" % args.cpus, block) is not None,
                 block[-1500:])
        pt.check("and the report says the guest started them",
                 re.search(r"cpus\s+%d, %d started by the guest" % (args.cpus, args.cpus - 1), txt) is not None,
                 txt[-2500:])


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def attach(args):
    """`hv attach` over a real SSH session: sshd and a guest started from
    /etc/rc, then a line typed through `ssh -tt ... hv attach 0` and its
    answer read back, ^] to detach -- and from /etc/rc, where nobody can
    type, a refusal."""
    tmp = tempfile.mkdtemp(prefix="nos-hvattach-")
    log = os.path.join(tmp, "serial.log")
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "hv-linux-test", "-f", key], check=True)
    port = free_port()

    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc", "ssh"))
    shutil.copy(hvt.module("x86_64"), rootdir)
    shutil.copy(os.path.join(ROOT, "out", "x86_64", "modules", "sshd.ko"), rootdir)
    shutil.copy(args.bzimage, os.path.join(rootdir, "bzImage"))
    shutil.copy(args.initrd, os.path.join(rootdir, "initrd"))
    with open(os.path.join(rootdir, "etc", "ssh", "authorized_keys"), "w") as f:
        f.write(open(key + ".pub").read())
    rc = ["insmod /sshd.ko", "sshd start", "insmod /hv.ko", "hv on",
          "hv start /bzImage mem=%d%s initrd=/initrd cmdline=%s" % (args.mem, cpus_opt(args), args.cmdline),
          "hv exec 0 secs=%d id" % args.vm_secs, "hv attach 0", RC_LAST]
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/hv-linux-test.py --attach\n" + "\n".join(rc) + "\n")
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "128", rootdir],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)

    argv = ["qemu-system-x86_64", "-display", "none", "-m", "2G", "-smp", NOS_SMP] + accel_args(args) + [
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
            "-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:%d-:22" % port]
    ssh = ["ssh", "-p", str(port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
           "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
           "-o", "ConnectTimeout=60", "-o", "LogLevel=ERROR"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        done = False
        t0 = time.time()
        txt = ""
        while time.time() - t0 < args.deadline and p.poll() is None:
            txt = open(log, errors="replace").read() if os.path.exists(log) else ""
            if "PANIC:" in txt:
                pt.check("nos did not panic", False, txt[-3000:])
                return
            if RC_DONE.search(txt):
                done = True
                break
            time.sleep(3)
        if not pt.check("sshd and a guest up, from /etc/rc", done,
                        txt[-2000:] or "qemu exited %s" % p.poll()):
            return
        secs = sections(open(log, errors="replace").read())
        pt.check("the guest's shell answered", re.search(r"uid=0 gid=0", output_of(secs, rc[5], 0) or "") is not None)
        pt.check("hv attach refuses where nobody can type",
                 "nobody can type here" in (output_of(secs, "hv attach 0", 0) or ""))

        # A person at a terminal: a line typed, its answer awaited, ^] typed.
        a = subprocess.Popen(ssh + ["-tt", "root@127.0.0.1", "hv attach 0"], stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        time.sleep(args.attach_wait)
        a.stdin.write(b"echo nos$((6*7))attach\r")
        a.stdin.flush()
        time.sleep(args.attach_wait)
        a.stdin.write(b"\x1d")
        a.stdin.flush()
        try:
            out, err = a.communicate(timeout=120)
        except subprocess.TimeoutExpired:
            a.kill()
            out, err = a.communicate()
        text = out.decode(errors="replace")
        pt.check("hv attach says it is attached", "attached to vm 0" in text, repr(text[-600:]))
        pt.check("what was typed ran in the guest, its answer came back", "nos42attach" in text, repr(text[-600:]))
        pt.check("^] detaches", "hv: detached -- vm 0" in text, repr(text[-600:]))
        pt.check("no terminal query reached the terminal", b"\x1b[6n" not in out, repr(text[-600:]))
        pt.check("the session ended cleanly", a.returncode == 0, "exit %s, %r" % (a.returncode, err[-300:]))

        c = subprocess.run(ssh + ["root@127.0.0.1", "hv list"], capture_output=True, timeout=120)
        pt.check("the guest runs on after the detach", b"vm 0  running" in c.stdout, repr(c.stdout[-300:]))

        # A command line past the 255 characters the dispatch took once -- as
        # long as a distribution's `hv start` -- runs whole.
        text = "nos-" + "x" * 300
        c = subprocess.run(ssh + ["root@127.0.0.1", "hv wait 0 secs=1 " + text], capture_output=True,
                           timeout=120)
        pt.check("a command line over 255 characters runs over ssh",
                 ('has not printed "%s"' % text).encode() in c.stdout, repr(c.stdout[-300:]))
    finally:
        pt.kill(p)
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def accel_args(args):
    """The QEMU accelerator flags for nos: KVM `-cpu host` where the host has
    an extension (AMD-V on AMD, Intel VT-x nested on Intel), else TCG
    `-cpu max` (AMD-V), which is what a guest is brought up under where there
    is no KVM. So the Linux path is exercised on whichever backend the host
    has, the two covered between an AMD host, an Intel host and TCG."""
    qemu_cpu = getattr(args, "qemu_cpu", None)
    kvm = os.path.exists("/dev/kvm") and not getattr(args, "tcg", False) and not qemu_cpu \
        and (hvt.host_has_svm() or hvt.host_has_vmx())
    return ["-cpu", "host", "-enable-kvm"] if kvm else ["-cpu", qemu_cpu or "max"]


def boot_rc(args, tmp, rc, extra=None, qemu=None):
    """nos with /etc/rc and the guest's files on its root, under TCG, run
    until rc's last line has printed. The QEMU process and the serial log;
    None for the process when rc did not get there."""
    log = os.path.join(tmp, "serial.log")
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"), exist_ok=True)
    shutil.copy(hvt.module("x86_64"), rootdir)
    shutil.copy(args.bzimage, os.path.join(rootdir, "bzImage"))
    if args.initrd:
        shutil.copy(args.initrd, os.path.join(rootdir, "initrd"))
    for name, src in (extra or {}).items():
        shutil.copy(src, os.path.join(rootdir, name))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# scripts/hv-linux-test.py\n" + "\n".join(rc) + "\n")
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, str(args.root_mib), rootdir],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL)

    # nos's root on virtio-blk, or with --nvme-root on NVMe: each driver has
    # its own way of taking a batch of blocks at once.
    if getattr(args, "nvme_root", False):
        root_device = ["-device", "nvme,serial=nosroot,drive=drive0"]
    else:
        root_device = ["-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off"]
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "2G", "-smp", NOS_SMP] + accel_args(args) + [
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image] + root_device + (qemu or [])
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0 = time.time()
    txt = ""
    while time.time() - t0 < args.deadline and p.poll() is None:
        txt = open(log, errors="replace").read() if os.path.exists(log) else ""
        if "PANIC:" in txt:
            pt.check("nos did not panic", False, txt[-3000:])
            return p, log, image
        if RC_DONE.search(txt):
            return p, log, image
        time.sleep(3)
    pt.check("/etc/rc ran to its end", False, txt[-2000:] or "qemu exited %s" % p.poll())
    return p, log, image


def disk(args):
    """A disk for the guest: an ext4 image on nos's root, `disk=` to the
    guest, which mounts it, reads what the host put there, writes, syncs and
    reads back -- and then nos's root is judged by e2fsck, and so is the
    image, taken back out of it, which must hold what the guest wrote."""
    tmp = tempfile.mkdtemp(prefix="nos-hvdisk-")
    content = os.path.join(tmp, "content")
    os.makedirs(content)
    with open(os.path.join(content, "hello.txt"), "w") as f:
        f.write("nos-disk-hello\n")
    img = os.path.join(tmp, "disk.img")
    subprocess.run(["mke2fs", "-q", "-F", "-t", "ext4", "-d", content, img, "%dM" % args.disk_mib],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    x = lambda line, secs=60: "hv exec 0 secs=%d %s" % (secs, line)
    rc = ["insmod /hv.ko", "hv on",
          "hv start /bzImage mem=%d%s initrd=/initrd disk=/disk.img cmdline=%s" % (args.mem, cpus_opt(args), args.cmdline),
          x("id", args.vm_secs),
          x("mount -t devtmpfs devtmpfs /dev"),
          x("cat /sys/block/vda/size"),
          x("mkdir -p /mnt"),
          x("mount /dev/vda /mnt"),
          x("cat /mnt/hello.txt"),
          x("dd if=/dev/zero of=/mnt/big bs=64k count=64", 120),
          x("echo nos-wrote-this > /mnt/written.txt"),
          x("sync", 120),
          x("umount /mnt", 120),
          x("mount /dev/vda /mnt"),
          x("cat /mnt/written.txt"),
          x("ls -l /mnt/big"),
          x("umount /mnt", 120),
          x("grep virtio /proc/interrupts"),
          "hv list",
          "hv stop 0", RC_LAST]
    p, log, image = boot_rc(args, tmp, rc, {"disk.img": img})
    try:
        if p.poll() is not None or not RC_DONE.search(open(log, errors="replace").read()):
            return
        secs = sections(open(log, errors="replace").read())
        out = lambda i: output_of(secs, rc[i], 0) or ""
        sectors = args.disk_mib * 1024 * 1024 // 512
        pt.check("the guest reached its shell", "uid=0 gid=0" in out(3), out(3)[-500:])
        pt.check("vda is the image's size: %d sectors" % sectors, re.search(r"(?m)^%d$" % sectors, out(5)) is not None, out(5))
        pt.check("it mounts", "hv: vm 0:" not in out(7) and "mount:" not in out(7), out(7))
        pt.check("what the host put there reads back", "nos-disk-hello" in out(8), out(8))
        pt.check("4 MiB written", re.search(r"4194304 bytes|64\+0 records out", out(9)) is not None, out(9))
        pt.check("sync returns", "hv: vm 0:" not in out(11), out(11))
        pt.check("remounted, what the guest wrote reads back", "nos-wrote-this" in out(14), out(14))
        pt.check("so does the size of the big one", "4194304" in out(15), out(15))
        pt.check(*interrupt_check(args, out(17), "req"))
        # Its disk's task on its own core -- the other thread of its vCPU's
        # -- or, its CPUs filling that core, on the core no guest has.
        own_disk = (r"vm 0  running  cpu (3  disks on cpu 2|2  disks on cpu 3|1  disks on cpu 0|0  disks on cpu 1)  "
                    if args.cpus == 1 else
                    r"vm 0  running  cpus ([23],[23]  disks on cpu [01]|[01],[01]  disks on cpu [23])  ")
        pt.check("its disk is served on a thread of a core of its own", re.search(own_disk, out(18)) is not None,
                 out(18)[-300:])
        stop = output_of(secs, "hv stop 0", 0) or ""
        m = re.search(r"vda +(\d+) MiB: (\d+) reads .*?, (\d+) writes .*?, (\d+) flushes, (\d+) errors", stop)
        pt.check("the report counts the disk's work, and no errors", m is not None and m.group(5) == "0"
                 and int(m.group(3)) > 0 and int(m.group(4)) > 0, stop[-800:])
    finally:
        pt.kill(p)

    # nos's own root, which every write of the guest's went through -- the
    # holes of the image filled, blocks of it written over -- as nos left it:
    # stopped, not unmounted, which e2fsck is to find clean all the same.
    fsck = subprocess.run(["e2fsck", "-fn", image], capture_output=True)
    pt.check("e2fsck finds nos's root clean after the guest's writes", fsck.returncode == 0,
             fsck.stdout.decode(errors="replace")[-800:])

    # The image as nos's root filesystem has it, after the guest's writes.
    back = os.path.join(tmp, "back.img")
    got = subprocess.run(["debugfs", "-R", "dump /disk.img " + back, image], capture_output=True)
    pt.check("the image comes back out of nos's root", got.returncode == 0 and os.path.exists(back),
             got.stderr.decode(errors="replace")[-300:])
    if os.path.exists(back):
        fsck = subprocess.run(["e2fsck", "-fn", back], capture_output=True)
        pt.check("e2fsck finds the guest's ext4 clean", fsck.returncode == 0,
                 fsck.stdout.decode(errors="replace")[-800:])
        cat = subprocess.run(["debugfs", "-R", "cat /written.txt", back], capture_output=True)
        pt.check("and the file the guest wrote is in it", b"nos-wrote-this" in cat.stdout,
                 repr(cat.stdout[-200:]) + cat.stderr.decode(errors="replace")[-200:])
    if args.keep or pt.failures:
        print("log at " + log)
    else:
        shutil.rmtree(tmp, ignore_errors=True)


def web_server(root):
    """A web server on the test machine, serving `root` on 127.0.0.1 --
    which QEMU's user network puts at 10.0.2.2 for nos, and NAT for the
    guests behind it: the server and its port."""
    import functools
    import http.server
    import threading

    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *a):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Quiet, directory=root))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def network(args):
    """The guests' network: two guests on the switch, each with its address
    from its port; each pings nos at 10.0.100.1 and the other guest, and
    nos pings a guest back through hv0. Then their way out, NAT from nos's
    own address: a page and a megabyte from a web server on the test
    machine -- at 10.0.2.2, where QEMU's user network puts it -- and a ping
    there; the DNS server nos was given, as ip= hands it to the guest; the
    switch's DHCP server, asked by udhcpc; and with --internet, a name
    looked up through that DNS server."""
    import hashlib
    tmp = tempfile.mkdtemp(prefix="nos-hvnet-")
    www = os.path.join(tmp, "www")
    os.makedirs(www)
    token = "nos-nat-%d" % os.getpid()
    with open(os.path.join(www, "nat.txt"), "w") as f:
        f.write(token + "\n")
    big = os.urandom(1 << 20)
    with open(os.path.join(www, "big"), "wb") as f:
        f.write(big)
    server, web = web_server(www)
    x = lambda vm, line, secs=60: "hv exec %d secs=%d %s" % (vm, secs, line)
    start = "hv start /bzImage mem=%d%s initrd=/initrd net cmdline=%s" % (args.mem, cpus_opt(args), args.cmdline)
    way_out = [x(0, "cat /proc/net/pnp"),
               x(0, "wget -q -O - http://10.0.2.2:%d/nat.txt" % web),
               x(0, "ping -c 3 10.0.2.2"),
               x(0, "wget -q -O - http://10.0.2.2:%d/big | md5sum" % web, 600),
               x(0, "echo 'echo lease $1 $ip $subnet $router $dns' > /dhcp.sh && chmod +x /dhcp.sh"),
               x(0, "udhcpc -i eth0 -f -q -n -t 5 -s /dhcp.sh"),
               x(0, "mkdir -p /etc && ln -sf /proc/net/pnp /etc/resolv.conf"),
               # One type asked for: BusyBox matches answers to questions by
               # their ID alone, which musl makes of the clock's nanoseconds --
               # the same for both, on a guest whose clock moves in ticks.
               x(0, "nslookup -type=a example.com" if args.internet else "true"),
               "nat", "hv list",
               # The megabyte again, into a guest that is busy: a loop has
               # each of its CPUs, and a frame that waits for a CPU's next
               # exit -- the host's tick -- overflows the port's inbox. The
               # switch kicks it out of its guest to take each one.
               x(0, "for i in %s; do sh -c 'while :; do :; done' > /dev/null 2>&1 & echo $! >> /busy.pid; done; echo busy"
                 % " ".join(str(i) for i in range(args.cpus))),
               x(0, "wget -q -O - http://10.0.2.2:%d/big | md5sum" % web, 600),
               x(0, "kill $(cat /busy.pid); echo idle"),
               "hv list",
               x(0, "grep virtio /proc/interrupts"),
               # A guest sending as another is stopped at its port: the first
               # claims the second's address -- ARP asked from it, a ping
               # from it -- which, let through, would teach nos's ARP table
               # the address is the first's, and hand it the second's
               # traffic. nos, which answers both, must hear neither. (Its
               # table is no steady witness: in a control run with the check
               # taken out of the switch, the second's own ARP put it right
               # again before `arp` could show it -- and in another did not,
               # and the fetch through `hv forward`, which the test makes
               # after all of this, reached the first guest instead of the
               # second's httpd.)
               x(0, "ip addr add 10.0.100.3/32 dev eth0 && arping -c 2 -w 3 -I eth0 -s 10.0.100.3 10.0.100.1; "
                    "ping -c 2 -W 2 -I 10.0.100.3 10.0.100.1; ip addr del 10.0.100.3/32 dev eth0; echo claimed"),
               "hv list",
               # A VM stopped takes its forwards with it, and NAT forgets
               # its address's flows -- the next VM to claim its port has its
               # MAC and address, and must be neither relayed to for it nor
               # answered for it. vm 0's flows above are in NAT's table still,
               # closing or not; vm 1's forward, which the fetch from outside
               # uses after all of this, stays.
               "hv forward add 8081 0 80",
               "hv stop 0",
               "hv forward",
               "nat",
               # nos's own services are not the guests': sshd listens on eth0,
               # and a guest's connection to it -- at nos's address on hv0, or
               # at eth0's through hv0 -- is refused. It once got the banner,
               # and four that never logged in held every slot for logging in.
               x(1, "echo | nc -w 5 10.0.100.1 22; echo nc-hv0-done"),
               x(1, "echo | nc -w 5 10.0.2.15 22; echo nc-eth0-done")]
    # Where NAT keeps the guests from, before the test machine is let
    # through: it is on nos's uplink's own subnet, 10.0.2.0/24 -- and
    # 169.254.169.254 is where a cloud's metadata server answers. A guest's
    # connect there is refused at once ("No route to host"), not left to
    # time out.
    kept = [x(0, "wget -q -T 5 -O - http://10.0.2.2:%d/nat.txt; echo nat-kept-done" % web),
            x(0, "wget -q -T 5 -O - http://169.254.169.254/; echo nat-metadata-done"),
            "nat allow 10.0.2.2/32"]
    rc = ["insmod /hv.ko", "hv on", start, start,
          x(0, "id", args.vm_secs), x(1, "id", args.vm_secs),
          "hv list",
          x(0, "ifconfig eth0"),
          x(0, "ping -c 3 10.0.100.1", 60),
          x(0, "ping -c 3 10.0.100.3", 60),
          x(1, "ping -c 3 10.0.100.2", 60),
          "ping 10.0.100.2",
          x(1, "mkdir -p /www"),
          x(1, "echo nos-http-hello > /www/index.html"),
          x(1, "httpd -p 80 -h /www"),
          "hv forward add 8080 1 80",
          "hv forward", "insmod /sshd.ko", "sshd start"] + kept + way_out + [RC_LAST]
    port = free_port()
    ssh_port = free_port()
    qemu = ["-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
            "-netdev", "user,id=net0,hostfwd=tcp:127.0.0.1:%d-:8080,hostfwd=tcp:127.0.0.1:%d-:22" % (port, ssh_port)]
    sshd = os.path.join(ROOT, "out", "x86_64", "modules", "sshd.ko")
    p, log, image = boot_rc(args, tmp, rc, extra={"sshd.ko": sshd}, qemu=qemu)
    try:
        if p.poll() is not None or not RC_DONE.search(open(log, errors="replace").read()):
            return
        secs = sections(open(log, errors="replace").read())
        out = lambda i: output_of(secs, rc[i], 0) or ""
        ok_ping = lambda t: re.search(r"3 packets transmitted, 3 packets received", t) is not None
        pt.check("both guests reach their shells", "uid=0 gid=0" in out(4) and "uid=0 gid=0" in out(5),
                 out(4)[-300:] + out(5)[-300:])
        pt.check("hv list gives each its address", "10.0.100.2" in out(6) and "10.0.100.3" in out(6), out(6))
        pt.check("the guest's eth0 has its port's address and MAC",
                 "10.0.100.2" in out(7) and re.search(r"(?i)02:00:00:00:64:02", out(7)) is not None, out(7))
        pt.check("a guest pings nos, at 10.0.100.1 on hv0", ok_ping(out(8)), out(8)[-600:])
        pt.check("a guest pings the other", ok_ping(out(9)), out(9)[-600:])
        pt.check("and the other pings it back", ok_ping(out(10)), out(10)[-600:])
        pt.check("nos pings a guest, out of hv0", "reply from 10.0.100.2" in out(11), out(11)[-600:])
        pt.check("hv forward add", "port 8080 forwarded to vm 1, 10.0.100.3:80" in out(15), out(15))
        # From outside: the host's port, QEMU's forward into nos's 8080, and
        # nos's into the guest's httpd.
        import urllib.request
        body = b""
        for attempt in range(10):
            try:
                body = urllib.request.urlopen("http://127.0.0.1:%d/index.html" % port, timeout=60).read()
                break
            except Exception as e:
                body = repr(e).encode()
                time.sleep(3)
        pt.check("a page from the guest's httpd, fetched from outside through hv forward",
                 b"nos-http-hello" in body, body[-300:])

        # Kept from the test machine and the metadata server, until let
        # through.
        kept_out = [output_of(secs, line, 0) or "" for line in kept]
        pt.check("NAT keeps a guest from the test machine, on nos's uplink's own subnet, at once",
                 "nat-kept-done" in kept_out[0] and token not in kept_out[0]
                 and "No route to host" in kept_out[0], kept_out[0][-400:])
        pt.check("and from 169.254.169.254, a cloud's metadata server",
                 "nat-metadata-done" in kept_out[1] and "No route to host" in kept_out[1], kept_out[1][-400:])
        pt.check("nat allow lets the test machine through", "nat: 10.0.2.2/32 let through" in kept_out[2],
                 kept_out[2])

        # The way-out lines, each by its own occurrence: "hv list" is run
        # before them, and among them twice.
        first = len(rc) - 1 - len(way_out)
        way = lambda i: output_of(secs, way_out[i], rc[:first + i].count(way_out[i])) or ""
        pt.check("ip= gave the guest nos's DNS server, 10.0.2.3", "nameserver 10.0.2.3" in way(0), way(0))
        pt.check("a guest fetches a page from the test machine through NAT", token in way(1), way(1)[-600:])
        pt.check("and pings it", ok_ping(way(2)), way(2)[-600:])
        pt.check("a megabyte through NAT arrives whole", hashlib.md5(big).hexdigest() in way(3), way(3)[-600:])
        pt.check("the switch's DHCP server gives the guest its port's address, nos as its router, "
                 "and nos's DNS server",
                 re.search(r"lease bound 10\.0\.100\.2 255\.255\.255\.0 10\.0\.100\.1 10\.0\.2\.3", way(5))
                 is not None, way(5)[-800:])
        if args.internet:
            pt.check("a name is looked up through that DNS server",
                     re.search(r"Name:\s+example\.com[\s\S]*Address", way(7)) is not None, way(7)[-800:])
        # The megabyte is 700-odd segments in; what goes out is the guest's
        # acknowledgements, far fewer since its virtio-net driver hands its
        # stack runs of segments at once.
        nat = way(8)
        counts = re.search(r"out (\d+)  back (\d+)  mapped (\d+)", nat)
        pt.check("nat: on, from hv0 out through eth0, and it carried the guests' packets both ways",
                 "nat: on, hv0 (10.0.100.1) out through eth0 (10.0.2.15)" in nat and counts is not None
                 and int(counts.group(1)) > 20 and int(counts.group(2)) > 700, nat[-800:])
        pt.check("nat says where the guests are kept from, what is let through, and what it denied",
                 "kept from 169.254.0.0/16" in nat and "and eth0's 10.0.2.0/24" in nat
                 and "let through: 10.0.2.2/32" in nat and re.search(r"denied [1-9]\d*", nat) is not None,
                 nat[-800:])
        pt.check("hv list says the guests go out through NAT, and counts the DHCP answers",
                 "the guests go out through NAT, from 10.0.2.15" in way(9)
                 and re.search(r"[1-9]\d* DHCP answers", way(9)) is not None, way(9)[-800:])
        pt.check("a busy guest takes the megabyte whole too", "busy" in way(10)
                 and hashlib.md5(big).hexdigest() in way(11), way(11)[-600:])
        vm0 = re.search(r"(?m)^vm 0 .*$", way(13))
        vm0 = vm0.group(0) if vm0 else ""
        kicks = re.search(r"kicks (\d+)", vm0)
        pt.check("kicked out of its guest to take the frames, its port dropped none",
                 kicks is not None and int(kicks.group(1)) > 0 and "frames dropped for it" not in vm0, vm0)
        # What the switch shares out -- hv0's backlog, a port's rate to
        # every port -- is for a guest that floods, not for this: DHCP, ARP,
        # pings and two megabytes through NAT, one into a busy guest.
        guests = re.findall(r"(?m)^vm [01] .*$", way(13))
        pt.check("neither guest's traffic went past its share of hv0's backlog or its rate to every port",
                 len(guests) == 2 and not any("past its share" in g or "past their rate" in g for g in guests),
                 "\n".join(guests))
        pt.check(*interrupt_check(args, way(14), "input"))
        claim = way(15)
        pt.check("a guest that claims another's address, ARP and IPv4, gets nothing back: nos heard none of it",
                 "claimed" in claim and re.search(r"Received 0 response", claim) is not None
                 and re.search(r"2 packets transmitted, 0 packets received", claim) is not None, claim[-800:])
        vm0 = re.search(r"(?m)^vm 0 .*$", way(16))
        vm1 = re.search(r"(?m)^vm 1 .*$", way(16))
        spoofed = re.search(r"(\d+) frames it sent as another guest", vm0.group(0) if vm0 else "")
        pt.check("hv list counts what the switch stopped against the guest that sent it, and only that guest",
                 spoofed is not None and int(spoofed.group(1)) >= 4
                 and vm1 is not None and "as another" not in vm1.group(0), way(16)[-800:])
        pt.check("a forward to vm 0 is added", "port 8081 forwarded to vm 0, 10.0.100.2:80" in way(17), way(17))
        pt.check("hv stop takes the stopped VM's forward away with it",
                 "its 1 forward(s) taken away with it" in way(18), way(18)[-600:])
        pt.check("hv forward no longer lists it, and still lists the other VM's",
                 "port 8081" not in way(19) and "port 8080 -> vm 1" in way(19), way(19))
        pt.check("NAT keeps no mapping of the stopped VM's address",
                 "nat: on" in way(20) and "10.0.100.2:" not in way(20), way(20)[-800:])
        # sshd answers from outside, by eth0 -- so a guest's refusal below is
        # the listener's doing, not a server that never started.
        banner = b""
        for attempt in range(10):
            try:
                with socket.create_connection(("127.0.0.1", ssh_port), timeout=30) as s:
                    banner = s.recv(64)
                break
            except Exception as e:
                banner = repr(e).encode()
                time.sleep(3)
        pt.check("nos's sshd answers from outside, by eth0", banner.startswith(b"SSH-2.0"), banner)
        for i, where in ((21, "at nos's address on hv0"), (22, "at eth0's address, through hv0")):
            pt.check("a guest's connection to nos's sshd %s is refused" % where,
                     "nc-" in way(i) and "SSH-2.0" not in way(i)
                     and re.search(r"(?i)connection refused", way(i)) is not None, way(i)[-400:])
    finally:
        server.shutdown()
        pt.kill(p)
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def run(args):
    tmp = tempfile.mkdtemp(prefix="nos-hvlinux-")
    log = os.path.join(tmp, "serial.log")

    boot_cmd = "hv boot /bzImage mem=%d secs=%d%s" % (args.mem, args.secs, cpus_opt(args))
    if args.initrd:
        boot_cmd += " initrd=/initrd"
    if args.input:
        # A single token, no spaces: \n stands for a newline. Kept before
        # cmdline=, which takes the rest of the line.
        boot_cmd += " input=" + args.input
    boot_cmd += " cmdline=" + args.cmdline
    rc = ["insmod /hv.ko", "hv on"]
    if not args.skip_boot:
        rc.append(boot_cmd)
    vm_lines, vm_checks = vm_commands(args) if args.initrd and not args.skip_vms else ([], [])
    rc += vm_lines + [RC_LAST]
    for line in rc:
        if len(line) > 255:
            sys.exit("an /etc/rc line is longer than rc takes (255): " + line)
    image = rootfs(tmp, args.bzimage, args.initrd, rc)

    accel = accel_args(args)
    print("accelerator: %s" % ("KVM, the host's %s (nested)" % ("Intel VT-x" if hvt.host_has_vmx() else "AMD-V")
                               if "-enable-kvm" in accel else "TCG, -cpu %s (AMD-V)" % accel[1]))
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "2G", "-smp", NOS_SMP] + accel + [
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off"]

    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("nos boots and runs /etc/rc",
                        pt.wait_log(log, "> hv on", args.deadline)):
            return
        # Every line of /etc/rc, to its end: the one-shot boot runs its whole
        # budget, and the VM phase waits on the guest as it goes.
        done = False
        t0 = time.time()
        while time.time() - t0 < args.deadline:
            txt = open(log, errors="replace").read()
            if "PANIC:" in txt:
                pt.check("nos did not panic", False, txt[-3000:])
                return
            if RC_DONE.search(txt):
                done = True
                break
            time.sleep(3)
        txt = open(log, errors="replace").read()
        if not pt.check("/etc/rc ran to its end", done, txt[-2000:]):
            return

        if not args.skip_boot:
            check_boot(args, txt)
        secs = sections(txt)
        for (line, occurrence, what, pattern, must) in vm_checks:
            out = output_of(secs, line, occurrence)
            if out is None:
                pt.check(what, False, "no output for %r" % line)
                continue
            found = re.search(pattern, out) is not None
            pt.check(what, found == must, out[-1500:])
        if vm_checks:
            flood_checks(args, secs)
    finally:
        pt.kill(p)
        if args.verbose and os.path.exists(log):
            print(open(log, errors="replace").read())
        if args.keep or pt.failures:
            print("log at " + log)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bzimage", required=True, help="a 64-bit Linux bzImage")
    ap.add_argument("--initrd", help="an initramfs image, optional")
    ap.add_argument("--cmdline", help="the guest kernel command line (default: %r, or with --cpus %r)"
                    % (DEFAULT_CMDLINE, DEFAULT_CMDLINE_SMP))
    ap.add_argument("--cpus", type=int, default=1,
                    help="the guest's CPUs: with more than one, that it brings every one up, and that "
                         "each takes its timer's interrupts and IPIs (needs a guest kernel with SMP and "
                         "x2APIC; under TCG, QEMU 9.2 or later)")
    ap.add_argument("--acpi", action="store_true",
                    help="the guest kernel has ACPI (with its power button and evdev): check that it takes "
                         "the machine's tables, reboots by the FADT's reset register, and shuts down when "
                         "hv stop presses its power button")
    ap.add_argument("--xapic", action="store_true",
                    help="the guest's local APICs out of reset in xAPIC mode (hv's `xapic`): every access of "
                         "theirs through the page, each instruction decoded and performed by the hypervisor")
    ap.add_argument("--ioapic", action="store_true",
                    help="the guest has an IO-APIC (hv's `ioapic`; needs --cpus 2 or more): the timer, the "
                         "serial port and the SCI routed through it -- and a virtio device's INTx line too, "
                         "given a --cmdline with pci=nomsi")
    ap.add_argument("--mem", type=int, default=256, help="guest RAM in MiB")
    ap.add_argument("--secs", type=int, default=120, help="guest run budget in seconds")
    ap.add_argument("--input", help="a single no-space token typed at the guest console once up; \\n = newline")
    ap.add_argument("--expect", action="append", default=[], help="extra text the console must contain (repeatable)")
    ap.add_argument("--deadline", type=int, default=900, help="seconds to wait for /etc/rc to finish")
    ap.add_argument("--vm-secs", type=int, default=400,
                    help="how long the first hv exec waits for the started guest's shell (at most 600)")
    ap.add_argument("--skip-boot", action="store_true", help="leave out the one-shot hv boot")
    ap.add_argument("--skip-vms", action="store_true", help="leave out the hv start phase")
    ap.add_argument("--attach", action="store_true",
                    help="instead: hv attach over a real ssh -tt session (needs ssh, ssh-keygen and --initrd)")
    ap.add_argument("--attach-wait", type=float, default=5.0,
                    help="seconds between what is typed through hv attach, for the guest to answer")
    ap.add_argument("--disk", action="store_true",
                    help="instead: a virtio disk for the guest (needs --initrd, and a guest kernel with PCI, "
                         "legacy virtio-pci, virtio-blk and ext4)")
    ap.add_argument("--disk-mib", type=int, default=32, help="the guest disk image's size")
    ap.add_argument("--nvme-root", action="store_true",
                    help="nos's root filesystem on an NVMe disk rather than virtio-blk (the AX41's)")
    ap.add_argument("--net", action="store_true",
                    help="instead: the guests' network (needs --initrd, and a guest kernel with PCI, "
                         "legacy virtio-pci, virtio-net, IP and ip= configuration)")
    ap.add_argument("--internet", action="store_true",
                    help="with --net: also look a name up through the DNS server the guest was given, "
                         "which needs the test machine to resolve names")
    ap.add_argument("--root-mib", type=int, default=128, help="nos's root filesystem image's size")
    ap.add_argument("--tcg", action="store_true", help="do not use KVM even if the host has AMD-V")
    ap.add_argument("--qemu-cpu", metavar="MODEL",
                    help="the CPU nos runs on, under TCG, in place of max: a named one -- EPYC-Rome, a Zen 2; "
                         "Skylake-Client,+svm, an Intel CPU with AMD-V -- whose family and model the guest's "
                         "kernel then takes the paths of, its MSRs above all")
    ap.add_argument("--keep", action="store_true", help="keep the serial log (it is kept on a failure anyway)")
    ap.add_argument("--verbose", action="store_true", help="print the whole serial log at the end")
    args = ap.parse_args()

    if not os.path.exists(args.bzimage):
        sys.exit("no such bzImage: " + args.bzimage)
    if args.cmdline is None:
        args.cmdline = DEFAULT_CMDLINE_SMP if args.cpus > 1 else DEFAULT_CMDLINE
    if args.ioapic and args.cpus < 2:
        sys.exit("--ioapic needs --cpus 2 or more: its interrupts go to local APICs")
    if args.cpus > 1 and "-enable-kvm" not in accel_args(args):
        version = hvt.qemu_version("qemu-system-x86_64")
        if version is None or version < hvt.TCG_REAL_MODE:
            sys.exit("--cpus under TCG needs QEMU %d.%d or later: a guest's other CPUs start in real mode, "
                     "which an older TCG does not put through the nested page table" % hvt.TCG_REAL_MODE)
    if not os.path.exists(os.path.join(ROOT, "nos.iso")):
        sys.exit("build nos.iso first (make)")

    if args.attach or args.disk or args.net:
        if not args.initrd:
            sys.exit("--attach, --disk and --net need --initrd: the guest's shell is what is typed at")
        attach(args) if args.attach else disk(args) if args.disk else network(args)
    else:
        run(args)

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("hv-linux-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
