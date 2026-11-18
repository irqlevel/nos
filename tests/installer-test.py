#!/usr/bin/env python3
"""The installer ISO booted as a server boots one, installing nos by its answers.

What scripts/installer makes has to do on a server (docs/install.md, "From an
ISO"), and that no other test does. An ISO built with answers
(scripts/installer/build.sh --answers) boots from a CD under SeaBIOS; its
Linux names the blank disk the answers give by /dev/disk/by-id, as udev does,
and makes it nos's own with the system image's nos-install disk -- saying so
on the serial line as it goes, as an install nobody watches does -- then
powers off. The disk's table is then GPT's, as disk lays it out: BIOS boot,
EFI System, a root of the size answered, nos-data to the end. The disk alone
boots nos under OVMF -- made by BIOS, booted by UEFI -- whose sshd lets in
the key the answers gave, to the image's kernel, with the netconsole they
asked for on its command line. Then the same ISO under OVMF, the disk beside
it: the installer finds nos there as it would put it, and installs nothing --
the disk is as it was, byte for byte.

    tests/installer-test.py [--image nos-x86_64.squashfs] [--keep]

Builds the ISO with Docker (build.sh) from the image: make image, or
scripts/build-image.sh, first.
"""

import argparse
import hashlib
import os
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

MiB = 1024 * 1024
SECTOR = 512
DISK_SIZE = 3 * 1024 * MiB
ROOT_MIB = 1024
# What QEMU's virtio-blk calls the disk to the guest, and udev after it
SERIAL = "NOSINSTALLTEST"
BY_ID = "/dev/disk/by-id/virtio-" + SERIAL
NETCONSOLE = "10.0.2.2:6666"
GPT_BIOS_BOOT = "21686148-6449-6E6F-744E-656564454649"
GPT_ESP = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
GPT_LINUX = "0FC63DAF-8483-4772-8E79-3D69D8477DE4"
# Under TCG an install is minutes: the installer's Linux, udev, mkfs, GRUB
INSTALL_TIMEOUT = 1800
BOOT_TIMEOUT = 600

failures = []


def check(name, ok, detail=""):
    print(("PASS: " if ok else "FAIL: ") + name + (" -- " + str(detail) if detail and not ok else ""), flush=True)
    if not ok:
        failures.append(name)
    return ok


def free_port():
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def ovmf():
    """UEFI for QEMU's two flash chips, as this host has it (image-test's
    list): None where there is none."""
    qemu = shutil.which("qemu-system-x86_64")
    share = os.path.join(os.path.dirname(os.path.dirname(os.path.realpath(qemu))), "share", "qemu")
    for code, variables in ((os.path.join(share, "edk2-x86_64-code.fd"), os.path.join(share, "edk2-i386-vars.fd")),
                            ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
                            ("/usr/share/OVMF/OVMF_CODE.fd", "/usr/share/OVMF/OVMF_VARS.fd"),
                            ("/usr/share/qemu/edk2-x86_64-code.fd", "/usr/share/qemu/edk2-i386-vars.fd"),
                            ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd")):
        if os.path.exists(code) and os.path.exists(variables):
            return code, variables
    return None


class Machine:
    def __init__(self, tmp, key):
        self.tmp = tmp
        self.key = key
        self.ssh_port = free_port()
        self.qemu = None
        self.log = None

    def boot(self, name, iso=None, firmware=None):
        """The disk, with the installer's ISO on a CD in front of it when
        given; UEFI's code and variables, or SeaBIOS."""
        self.log = os.path.join(self.tmp, "serial-%s.log" % name)
        argv = ["qemu-system-x86_64", "-display", "none", "-no-reboot", "-m", "2G", "-smp", "2"]
        if firmware is not None:
            code, variables = firmware
            mine = os.path.join(self.tmp, "vars-%s.fd" % name)
            shutil.copyfile(variables, mine)
            argv += ["-drive", "if=pflash,format=raw,unit=0,readonly=on,file=%s" % code,
                     "-drive", "if=pflash,format=raw,unit=1,file=%s" % mine]
        if iso is not None:
            argv += ["-drive", "file=%s,if=none,id=cd,media=cdrom,readonly=on" % iso,
                     "-device", "ide-cd,drive=cd,bootindex=0"]
        argv += ["-drive", "file=%s,format=raw,if=none,id=d0" % os.path.join(self.tmp, "disk.raw"),
                 "-device", "virtio-blk-pci,drive=d0,serial=%s,bootindex=1,disable-legacy=on,disable-modern=off"
                 % SERIAL,
                 "-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:%d-:22" % self.ssh_port,
                 "-device", "virtio-net-pci,netdev=n0,disable-legacy=on,disable-modern=off",
                 "-device", "virtio-rng-pci", "-serial", "file:" + self.log]
        if os.path.exists("/dev/kvm") and os.access("/dev/kvm", os.W_OK):
            argv += ["-enable-kvm", "-cpu", "host"]
        else:
            argv += ["-cpu", "max"]
        self.qemu = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def text(self):
        return open(self.log, errors="replace").read() if self.log and os.path.exists(self.log) else ""

    def wait_log(self, marker, timeout, stop_at=("PANIC", "nos-setup: FAILED")):
        start = time.time()
        while time.time() - start < timeout:
            text = self.text()
            if marker in text:
                return True
            if any(s in text for s in stop_at):
                print(text[-4000:], flush=True)
                return False
            if self.qemu.poll() is not None:
                return marker in self.text()
            time.sleep(1)
        return False

    def wait_exit(self, timeout):
        try:
            self.qemu.wait(timeout)
            return True
        except subprocess.TimeoutExpired:
            return False

    def stop(self):
        if self.qemu is not None and self.qemu.poll() is None:
            self.qemu.send_signal(signal.SIGTERM)
            try:
                self.qemu.wait(10)
            except subprocess.TimeoutExpired:
                self.qemu.kill()
        self.qemu = None

    def ssh(self, args, timeout=120):
        cmd = ["ssh", "-p", str(self.ssh_port), "-i", self.key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
               "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=20",
               "-o", "LogLevel=ERROR", "root@127.0.0.1"] + args
        try:
            p = subprocess.run(cmd, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired:
            return None, ""
        out = p.stdout.decode(errors="replace")
        print("  ssh %s -> %s%s" % (" ".join(args), p.returncode,
                                   "".join("\n    " + line for line in out.splitlines()[:12])), flush=True)
        return p.returncode, out

    def wait_ssh(self, timeout):
        start = time.time()
        while time.time() - start < timeout:
            rc, out = self.ssh(["version"], timeout=60)
            if rc == 0 and out.startswith("nos "):
                return out.strip()
            if self.qemu.poll() is not None:
                return None
            time.sleep(3)
        return None


def gpt_partitions(path):
    """The disk's GPT entries in use, in order: type, name, first and last
    sector -- read here, as nothing on the host need know GPT for it."""
    with open(path, "rb") as f:
        f.seek(SECTOR)
        header = f.read(92)
        if header[:8] != b"EFI PART":
            return None
        entries_lba, count, size = struct.unpack_from("<QII", header, 72)
        f.seek(entries_lba * SECTOR)
        raw = f.read(count * size)
    parts = []
    for i in range(count):
        entry = raw[i * size:(i + 1) * size]
        if entry[:16] == bytes(16):
            continue
        first, last = struct.unpack_from("<QQ", entry, 32)
        parts.append({"type": str(uuid.UUID(bytes_le=entry[:16])).upper(), "first": first, "last": last,
                      "name": entry[56:128].decode("utf-16-le").rstrip("\0")})
    return parts


def layout_ok(parts):
    per_mib = MiB // SECTOR
    want = [(GPT_BIOS_BOOT, "BIOS boot", per_mib), (GPT_ESP, "EFI System", 512 * per_mib),
            (GPT_LINUX, "nos", ROOT_MIB * per_mib), (GPT_LINUX, "nos-data", None)]
    if parts is None or len(parts) != len(want):
        return False
    at = per_mib
    for part, (kind, name, size) in zip(parts, want):
        if part["type"] != kind or part["name"] != name or part["first"] != at:
            return False
        if size is not None and part["last"] - part["first"] + 1 != size:
            return False
        at = part["last"] + 1
    return at > DISK_SIZE // SECTOR - 2 * per_mib


def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(MiB), b""):
            h.update(block)
    return h.hexdigest()


def said_version(text):
    """The image's version as the installer says it installs it: what nos's
    `version` prints, from the same VERSION file."""
    m = re.search(r"installing (nos .+?) as the answers say", text)
    return m.group(1) if m else None


def run(tmp, image):
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "installer-test", "-f", key], check=True)
    answers = os.path.join(tmp, "answers")
    with open(answers, "w") as f:
        f.write("# installer-test's\ndisk=%s\nwipe=yes\nroot_size=%dM\nkey=%s\nnetconsole=%s\nafter=poweroff\n"
                % (BY_ID, ROOT_MIB, open(key + ".pub").read().strip(), NETCONSOLE))
    iso = os.path.join(tmp, "installer.iso")
    print("--- build.sh --answers", flush=True)
    p = subprocess.run([os.path.join(ROOT, "scripts", "installer", "build.sh"), "--image", image,
                        "--answers", answers, "--out", iso], capture_output=True, text=True)
    print("".join("  " + line + "\n" for line in (p.stdout + p.stderr).splitlines()[-10:]), end="", flush=True)
    if not check("build.sh makes the ISO, with the answers", p.returncode == 0 and os.path.exists(iso)):
        return
    with open(os.path.join(tmp, "disk.raw"), "wb") as f:
        f.truncate(DISK_SIZE)
    m = Machine(tmp, key)
    firmware = ovmf()
    want = None

    # The ISO under SeaBIOS, installing by its answers
    m.boot("install-bios", iso=iso)
    try:
        check("the ISO boots its Linux under SeaBIOS, and nos-setup takes the answers",
              m.wait_log("as the answers say, asking nothing", INSTALL_TIMEOUT), m.text()[-3000:])
        check("it installs onto the disk the answers name by /dev/disk/by-id",
              m.wait_log("nos-setup: done: nos is on " + BY_ID, INSTALL_TIMEOUT), m.text()[-4000:])
        text = m.text()
        want = said_version(text)
        check("what nos-install does is on the serial line as it goes",
              "a GPT of nos's own" in text and "GRUB for BIOS" in text and "GRUB for UEFI" in text, text[-3000:])
        check("and it powers off, as answered", m.wait_exit(300))
    finally:
        m.stop()
    disk = os.path.join(tmp, "disk.raw")
    parts = gpt_partitions(disk)
    check("the disk's table is disk's: BIOS boot, EFI System, the root of the size answered, nos-data to the end",
          layout_ok(parts), parts)

    if firmware is None:
        check("UEFI for QEMU here: QEMU's edk2-x86_64-code.fd, or the ovmf package", False)
        return
    # The disk alone, under UEFI
    m.boot("disk-uefi", firmware=firmware)
    try:
        check("the disk boots nos alone under OVMF", m.wait_log("boot: complete", BOOT_TIMEOUT), m.text()[-3000:])
        version = m.wait_ssh(300)
        check("sshd lets in the key the answers gave, to the image's kernel", version is not None and
              version == want, "%r, the installer said it installs %r" % (version, want))
        if version is not None:
            rc, out = m.ssh(["cat", "/proc/cmdline"])
            check("with the netconsole the answers asked for", rc == 0 and ("netconsole=" + NETCONSOLE) in out, out)
        m.ssh(["poweroff"], timeout=60)
        check("nos powers off", m.wait_exit(120))
    finally:
        m.stop()

    # The ISO again, under UEFI, the installed disk beside it
    before = digest(disk)
    m.boot("install-uefi", iso=iso, firmware=firmware)
    try:
        check("the ISO boots its Linux under OVMF too", m.wait_log("as the answers say, asking nothing",
                                                                  INSTALL_TIMEOUT), m.text()[-3000:])
        check("and finds nos on the disk as it would put it there, and installs nothing",
              m.wait_log("not installed again", INSTALL_TIMEOUT) and "a GPT of nos's own" not in m.text(),
              m.text()[-3000:])
    finally:
        m.stop()
    check("the disk is as it was, byte for byte", digest(disk) == before)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--image", default=os.path.join(ROOT, "nos-x86_64.squashfs"))
    p.add_argument("--keep", action="store_true", help="keep the scratch directory: the ISO, the disk, the logs")
    args = p.parse_args()
    if not os.path.exists(args.image):
        sys.exit("no %s: make image, or scripts/build-image.sh" % args.image)
    for tool in ("qemu-system-x86_64", "ssh", "ssh-keygen", "docker"):
        if shutil.which(tool) is None:
            sys.exit("installer-test needs %s" % tool)

    tmp = tempfile.mkdtemp(prefix="installer-test.")
    print("installer-test: %s, scratch in %s" % (args.image, tmp), flush=True)
    try:
        run(tmp, os.path.abspath(args.image))
    finally:
        if args.keep or failures:
            print("installer-test: kept %s" % tmp, flush=True)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    if failures:
        print("installer-test: %d FAILED: %s" % (len(failures), ", ".join(failures)), flush=True)
        sys.exit(1)
    print("installer-test: all passed", flush=True)


if __name__ == "__main__":
    main()
