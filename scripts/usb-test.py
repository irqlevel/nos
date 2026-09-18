#!/usr/bin/env python3
"""USB test: type at the kernel through an emulated USB keyboard.

On the Dell Latitude this kernel boots -- UEFI, no serial port, no PS/2 --
the xHCI controller and the HID boot keyboard on it are the only way in.
Nothing else in the suite touches either: a smoke boot attaches no USB
controller at all, so the whole driver, from the command ring to the report
that becomes a keystroke, goes unexercised.

This is its gate, and it checks the thing that matters rather than the
counters: QEMU's monitor pushes keys into the emulated keyboard, the kernel's
shell answers on the serial console, and what comes back says whether the
path held. A driver that enumerates but delivers no report passes every
plausible self-test and leaves the laptop with no keyboard.

    scripts/usb-test.py [--keep]

x86-64, because that is where the driver is (`usb=off` turns it off).
Exit code 0 = every check passed.
"""

import argparse
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

failures = []


def check(name, ok, detail=""):
    print(("PASS: " if ok else "FAIL: ") + name +
          ("\n      " + str(detail).strip() if detail and not ok else ""), flush=True)
    if not ok:
        failures.append(name)
    return ok


# What `sendkey` calls the characters a command line is made of. The monitor
# takes key names, not text.
KEYS = {
    " ": "spc", "-": "minus", "/": "slash", ".": "dot", ",": "comma",
    "=": "equal", ";": "semicolon", "\n": "ret",
}
for c in "abcdefghijklmnopqrstuvwxyz0123456789":
    KEYS[c] = c


class Monitor:
    """QEMU's monitor over a unix socket: keys in, devices added and removed."""

    def __init__(self, path, timeout=20):
        deadline = time.time() + timeout
        while True:
            try:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.connect(path)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.time() > deadline:
                    raise
                time.sleep(0.2)
        self.sock.settimeout(2)
        self.drain()

    def drain(self):
        try:
            while self.sock.recv(65536):
                pass
        except socket.timeout:
            pass

    def command(self, line):
        self.sock.sendall((line + "\n").encode())
        time.sleep(0.4)
        self.drain()

    def type(self, text):
        """One key at a time, as someone at the keyboard would."""
        for ch in text:
            key = KEYS.get(ch)
            if key is None:
                raise ValueError("no key for %r" % ch)
            self.sock.sendall(("sendkey %s\n" % key).encode())
            time.sleep(0.08)
        self.drain()


class Console:
    """The serial log, read as the shell writes to it."""

    def __init__(self, path):
        self.path = path

    def text(self):
        if not os.path.exists(self.path):
            return ""
        return open(self.path, errors="replace").read()

    def wait(self, marker, timeout=60):
        start = time.time()
        while time.time() - start < timeout:
            text = self.text()
            if marker in text:
                return True
            if "PANIC" in text:
                print(text[-3000:])
                sys.exit("kernel panic")
            time.sleep(0.5)
        return False

    def run(self, mon, cmd, timeout=30):
        """Type a command and hand back what the shell printed for it."""
        before = len(self.text())
        mon.type(cmd + "\n")

        # The shell prints its answer and then a fresh prompt.
        start = time.time()
        while time.time() - start < timeout:
            out = self.text()[before:]
            if out.count("$") >= 1 and out.rstrip().endswith("$"):
                break
            time.sleep(0.5)

        out = self.text()[before:]
        # Drop the echo of the command itself and the trailing prompt.
        out = out.split("\n", 1)[1] if "\n" in out else out
        print("  $ " + cmd + "".join("\n    " + l for l in out.splitlines() if l.strip()),
              flush=True)
        return out


def ports_of(out):
    """The `usb` command's port lines: {port number: the rest of the line}."""
    ports = {}
    for line in out.splitlines():
        m = re.match(r"\s*port (\d+): (.*)", line)
        if m:
            ports[int(m.group(1))] = m.group(2)
    return ports


def reports_of(out):
    """How many HID reports each keyboard slot has delivered."""
    return {int(m.group(1)): int(m.group(2))
            for m in re.finditer(r"keyboard slot (\d+): .*reports (\d+)", out)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--keep", action="store_true", help="leave the log behind")
    args = ap.parse_args()

    iso = os.path.join(ROOT, "nos.iso")
    if not os.path.exists(iso):
        sys.exit("%s not found -- build it first" % iso)

    tmp = tempfile.mkdtemp(prefix="nos-usbtest-")
    log = os.path.join(tmp, "serial.log")
    monitor_path = os.path.join(tmp, "monitor.sock")

    # A keyboard on a root port, a hub with a second keyboard behind it --
    # the tiered path, which nothing else reaches -- and a device that is not
    # a keyboard, whose slot the driver takes and gives back.
    argv = ["qemu-system-x86_64", "-m", "1024", "-smp", "2", "-cdrom", iso,
            "-device", "qemu-xhci,id=xhci",
            "-device", "usb-kbd,bus=xhci.0,port=1,id=kbd0",
            "-device", "usb-hub,bus=xhci.0,port=2,id=hub0",
            "-device", "usb-kbd,bus=xhci.0,port=2.1,id=kbd1",
            "-device", "usb-mouse,bus=xhci.0,port=3,id=mouse0",
            "-serial", "file:" + log, "-display", "none",
            "-monitor", "unix:%s,server,nowait" % monitor_path]

    qemu = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    con = Console(log)
    try:
        if not check("reaches the shell", con.wait("boot: complete", 300)):
            return 1

        boot = con.text()
        check("the controller came up", "Xhci: initialized 1 controllers" in boot,
              boot[-1500:])
        check("and found a boot keyboard on a root port",
              re.search(r"Xhci: keyboard ready on root port \d+ slot \d+ route 0x0", boot)
              is not None, boot[-1500:])
        check("a hub came up on a root port",
              re.search(r"Xhci: hub slot \d+: \d+ ports", boot) is not None,
              "\n".join(l for l in boot.splitlines() if "Xhci:" in l))
        # The tiered path: a device a route string away, which only a hub
        # reaches and which nothing else in the suite exercises.
        check("and a keyboard behind it, a tier down with a route",
              re.search(r"Xhci: keyboard ready on root port \d+ slot \d+ route 0x[1-9a-fA-F]",
                        boot) is not None,
              "\n".join(l for l in boot.splitlines() if "Xhci:" in l))
        check("a device that is neither is enumerated and let go",
              "is neither keyboard nor hub, releasing" in boot,
              "\n".join(l for l in boot.splitlines() if "Xhci:" in l))

        mon = Monitor(monitor_path)

        # The point of the whole driver: a command typed at the keyboard.
        out = con.run(mon, "usb")
        if not check("a command typed on the usb keyboard reaches the shell",
                     "xhci" in out, out or "(nothing)"):
            return 1

        ports = ports_of(out)
        check("usb lists what is on each root port", len(ports) >= 3,
              out or "(nothing)")
        check("the keyboard on a root port among them",
              any("boot keyboard" in line for line in ports.values()), ports)
        check("and the one that is neither, its slot given back",
              any("boot keyboard" not in line and "class 0" in line
                  for line in ports.values()), ports)

        reports = reports_of(out)
        # Two: the one on a root port and the one behind the hub, which has
        # no port line of its own because port records are root ports.
        check("both keyboards are being serviced", len(reports) >= 2, out)
        check("and one has delivered the reports that were typed",
              any(n > 0 for n in reports.values()), out)

        # A second command, to show the reports climb -- a driver that
        # enumerated but stopped pumping would pass everything above.
        before = reports
        out = con.run(mon, "uptime")
        check("a second command works too", out.strip() != "",
              out or "(nothing)")
        out = con.run(mon, "usb")
        after = reports_of(out)
        check("and the report count climbed",
              any(after.get(slot, 0) > before.get(slot, 0) for slot in after),
              "before %s after %s" % (before, after))
        check("with no transfer errors",
              re.search(r"errors [1-9]", out) is None, out)

        # Hot-plug. The kernel's own trace does not reach the console once
        # the shell has taken it, so the shell is what is asked.
        mon.command("device_add usb-kbd,bus=xhci.0,port=4,id=kbd2")
        start = time.time()
        grown = {}
        while time.time() - start < 30:
            grown = ports_of(con.run(mon, "usb"))
            if len(grown) > len(ports):
                break
            time.sleep(1)
        check("a keyboard plugged in after boot is picked up",
              len(grown) > len(ports), grown)
        check("and is claimed as a keyboard",
              sum(1 for line in grown.values() if "boot keyboard" in line) >= 2, grown)

    finally:
        if qemu.poll() is None:
            qemu.send_signal(signal.SIGTERM)
            try:
                qemu.wait(10)
            except subprocess.TimeoutExpired:
                qemu.kill()

    if args.keep:
        print("log left in " + log)
    ok = not failures
    print("usb-test: " + ("PASSED" if ok else "FAILED: " + ", ".join(failures)))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
