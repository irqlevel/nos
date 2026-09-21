#!/usr/bin/env python3
"""A synchronous wait on a CPU with nothing else runnable must not cost a tick.

Why this exists. A task that waits for another CPU's interrupt -- a block
driver's synchronous read, a mutex, the TLB shootdown's acks -- polls. If it
polls with Schedule(), the scheduler is free to hand the CPU to the idle task,
which halts it; the thing it waits for is a counter or a store in an interrupt
handler, with no waiter to unblock and no IPI to send, so nothing brings the
CPU back before its own next tick. The wait costs 10 ms, every time, and the
work was done microseconds in.

Why no other gate sees it. The precondition is a CPU with *nothing else
runnable*, and the smoke boots have more polling tasks (shell, udpsh, dhcp,
netconsole, usb) than CPUs -- every CPU always has something to run, so
Schedule() never reaches the idle task and every latency looks right. It takes
more CPUs than busy tasks to show at all: at -smp 4 the unfixed kernel reads
at 45k IOPS, at -smp 16 the same kernel reads at 98. It was found on a 12-CPU
Hetzner AX41 and nowhere else, three times over (a171a86, ce3087c, and the
wait primitives themselves).

What it asserts: blkload at qd=1 -- one worker, one I/O in flight, every other
CPU idle -- comes back in well under a tick. There is no middle ground to tune
against: the bug puts p50 at exactly one tick (10223 us at 100 Hz), the fix
puts it at ~20 us under KVM.
"""

import argparse
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# The boot/shell plumbing lives in netblk-test.py; this gate differs only in
# how many CPUs it asks for and what it asserts.
_spec = importlib.util.spec_from_file_location("netblk_test", os.path.join(HERE, "netblk-test.py"))
nb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(nb)

# One tick is 10 ms on both architectures (Hpet::DesiredHz, Pit::DesiredHz).
# A wait that halted the CPU lands on it exactly; a wait that did not is three
# orders of magnitude below. Anywhere near it is the bug.
TICK_US = 10_000.0
MAX_P50_US = 1_000.0

# Enough CPUs that some are genuinely idle while the one worker waits. Below
# this the kernel's own polling tasks cover every CPU and the bug cannot show,
# so a small host must not report a pass.
MIN_SMP = 12


def boot(tmp, smp):
    log = os.path.join(tmp, "serial.log")
    fwd = f"hostfwd=udp:127.0.0.1:{nb.SHELL_PORT}-:{nb.SHELL_PORT}"
    cmd = [
        "qemu-system-x86_64", "-enable-kvm", "-cpu", "host",
        "-cdrom", os.path.join(tmp, "test.iso"), "-boot", "d",
        "-device", "virtio-blk-pci,drive=root,disable-legacy=on,disable-modern=off",
        "-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
        "-device", "virtio-rng-pci",
        "-drive", f"file={os.path.join(tmp, 'root.img')},format=raw,id=root,if=none",
        "-drive", f"file={os.path.join(tmp, 'nvme.img')},format=raw,id=nvme0,if=none",
        "-device", "nvme,serial=idlewait0,drive=nvme0",
        "-netdev", f"user,id=net0,{fwd}",
        "-serial", f"file:{log}", "-display", "none", "-m", "2G", "-smp", str(smp),
    ]
    return subprocess.Popen(cmd), log


def latency_us(reply, field):
    """blkload's report: 'latency us: min 11.3, avg 49.3, p50 15.1, ...'"""
    match = re.search(rf"\b{field}\s+([0-9.]+)", reply)
    if match is None:
        raise nb.TestFailure(f"no {field} in blkload's report: {reply!r}")
    return float(match.group(1))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--smp", type=int, default=16,
                        help=f"vCPUs to boot with (at least {MIN_SMP} for the bug to be visible)")
    args = parser.parse_args()

    if args.smp < MIN_SMP:
        print(f"idle-wait-test: --smp {args.smp} is too few: with fewer CPUs than the "
              f"kernel's polling tasks none is ever idle and the test cannot fail")
        return 1
    if os.cpu_count() < MIN_SMP:
        print(f"idle-wait-test: this host has {os.cpu_count()} CPUs, and the test needs "
              f"{MIN_SMP} to mean anything -- run it on a bigger one")
        return 1
    if not os.path.exists("/dev/kvm"):
        print("idle-wait-test: needs /dev/kvm (TCG's latencies say nothing)")
        return 1

    # blkload.ko, not netblk.ko, on the root image
    nb.module_path = lambda arch: os.path.join(ROOT, "out", "x86_64", "modules", "blkload.ko")

    tmp = tempfile.mkdtemp(prefix="idle-wait-")
    try:
        nb.build_images(tmp, "x86_64")
        qemu, log = boot(tmp, args.smp)
        try:
            nb.wait_for_boot(qemu, log, 120)
            shell = nb.Shell(nb.SHELL_PORT)
            shell.run("insmod /blkload.ko")

            # qd=1: one worker, one I/O in flight, every other CPU with
            # nothing to run -- the wait has only the idle task to be given to
            reply = shell.run("blkload nvme0 randread qd=1 secs=3", timeout=60)
            p50 = latency_us(reply, "p50")
            # Worded to read as the truth either way: check() prints it after
            # "ok:" when it holds and after "FAILED:" when it does not
            nb.check(p50 < MAX_P50_US,
                     f"a qd=1 read waits {p50:.1f} us against a {MAX_P50_US:.0f} us bar "
                     f"(parked behind the idle task it costs a whole {TICK_US:.0f} us tick)")
            worst = latency_us(reply, "max")
            nb.check(worst < TICK_US,
                     f"the slowest of them waited {worst:.1f} us against a tick's {TICK_US:.0f} us")
        finally:
            qemu.terminate()
            try:
                qemu.wait(timeout=10)
            except subprocess.TimeoutExpired:
                qemu.kill()
            text = open(log, errors="replace").read() if os.path.exists(log) else ""
            if "PANIC:" in text:
                print("idle-wait-test: PANIC in the serial log")
                print(text[-2000:])
                return 1
    except nb.TestFailure as failure:
        print(f"idle-wait-test: FAILED: {failure}")
        return 1
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("idle-wait-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
