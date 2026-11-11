#!/usr/bin/env python3
"""Power-off test: `poweroff` has to turn the machine off, and by ACPI.

On x86-64 the machine is turned off by a write of the S5 sleep type to the
PM1 control register, and the type is the chipset's own: the firmware's DSDT
names it in \\_S5 -- 7 on Intel's PCHs, 5 on AMD's FCH, 0 under QEMU. The
kernel used to write 5 whatever the machine: on an Intel one that is S3, a
suspend to RAM with nothing to wake to. Under QEMU nobody saw it, because the
write that did the work there was the next one -- to port 0x604, where QEMU's
machines have the register, with the type QEMU wants -- and after that the
isa-debug-exit device's. A power-off that asked for the wrong state and one
that asked for the right one ended the same way.

So this boots with the CPU's hypervisor bit off (-cpu qemu64,-hypervisor):
the kernel then takes the machine for a real one and tries neither of QEMU's
ports, and the only way it goes off is the one a real machine does -- the
\\_S5 the kernel read out of the DSDT, written to the register the FADT
names. The machine has to go off by itself, with nothing panicked on the way
down; twice, as QEMU's two machine types have two DSDTs.

    scripts/poweroff-test.py [--skip-build] [--keep]

x86-64. Exit code 0 = every check passed.
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

BOOT_TIMEOUT = 600
OFF_TIMEOUT = 180

failures = []


def check(name, ok, detail=""):
    print(("PASS: " if ok else "FAIL: ") + name +
          ("\n      " + str(detail).strip() if detail and not ok else ""), flush=True)
    if not ok:
        failures.append(name)
    return ok


def read(path):
    try:
        with open(path, errors="replace") as f:
            return f.read()
    except FileNotFoundError:
        return ""


def lines_with(text, key):
    return [line.strip() for line in text.splitlines() if key in line]


def power_off(tmp, machine):
    """Boot on that machine type, type poweroff, and see it go off."""
    log = os.path.join(tmp, "serial-%s.log" % machine)
    root = os.path.join(tmp, "root-%s.img" % machine)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), root, "64", "", "1024"], check=True,
                   stdout=subprocess.DEVNULL, cwd=ROOT)

    qemu = [
        os.environ.get("POWEROFF_QEMU", "qemu-system-x86_64"),
        "-machine", machine, "-cpu", "qemu64,-hypervisor",
        "-display", "none", "-m", "1G", "-smp", "2",
        "-cdrom", os.path.join(ROOT, "nos.iso"),
        "-serial", "stdio",
        "-drive", "file=%s,format=raw,id=drive0,if=none" % root,
        "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
        "-device", "virtio-rng-pci",
        "-no-reboot",
    ]
    with open(log, "wb") as out:
        proc = subprocess.Popen(qemu, stdin=subprocess.PIPE, stdout=out, stderr=subprocess.STDOUT)
        try:
            booted = False
            deadline = time.time() + BOOT_TIMEOUT
            while time.time() < deadline and proc.poll() is None:
                text = read(log)
                if "PANIC:" in text:
                    break
                if "boot: complete" in text:
                    booted = True
                    break
                time.sleep(1)
            if not check("%s: the kernel boots" % machine, booted, "\n".join(read(log).splitlines()[-15:])):
                return

            text = read(log)
            s5 = lines_with(text, "_S5 is SLP_TYP")
            check("%s: the DSDT's \\_S5 is read" % machine, len(s5) == 1,
                  "\n".join(lines_with(text, "Acpi:")[-8:]))

            time.sleep(2)
            proc.stdin.write(b"\npoweroff\n")
            proc.stdin.flush()

            deadline = time.time() + OFF_TIMEOUT
            while time.time() < deadline and proc.poll() is None:
                time.sleep(1)
            text = read(log)
            check("%s: the machine turns itself off" % machine, proc.poll() is not None,
                  "\n".join(text.splitlines()[-15:]))
            check("%s: by the sleep type \\_S5 names" % machine, len(lines_with(text, "ACPI shutdown: SLP_TYP")) == 1,
                  "\n".join(lines_with(text, "poweroff")))
            check("%s: nothing panicked on the way down" % machine, "PANIC:" not in text,
                  "\n".join(lines_with(text, "PANIC:")))
            check("%s: and it did not say it was still on" % machine, "the machine is still on" not in text)
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(5)
                except subprocess.TimeoutExpired:
                    proc.kill()


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--skip-build", action="store_true", help="use the nos.iso that is there")
    parser.add_argument("--keep", action="store_true", help="keep the logs")
    args = parser.parse_args()

    if not args.skip_build:
        print("poweroff-test: building (docker, incremental)...", flush=True)
        built = subprocess.run(
            ["docker", "run", "--platform", "linux/amd64", "--rm", "-v", ROOT + ":/src", "-w", "/src",
             "nos-builder", "bash", "-c", "make rust && make nocheck"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if built.returncode != 0:
            print("\n".join(built.stdout.splitlines()[-30:]))
            print("poweroff-test: build failed")
            return 1

    if not os.path.exists(os.path.join(ROOT, "nos.iso")):
        print("poweroff-test: nos.iso not found")
        return 1

    tmp = tempfile.mkdtemp(prefix="nos-poweroff-")
    try:
        for machine in ("pc", "q35"):
            power_off(tmp, machine)
    finally:
        if args.keep or failures:
            print("poweroff-test: logs in " + tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    if failures:
        print("poweroff-test: FAILED: " + "; ".join(failures))
        return 1
    print("poweroff-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
