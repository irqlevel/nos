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
asked for on its command line, on the static address they gave: eth0's
settings in /etc/net.toml, as a network with no DHCP server needs them, and
not on the command line. Then the same ISO under OVMF, the disk beside
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
# QEMU's user network, given as a static address, gateway and DNS server
STATIC_IP = "10.0.2.15/24"
STATIC_GW = "10.0.2.2"
STATIC_DNS = "10.0.2.3"
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


# Roots of the systems a server may come with, each with its network as its
# own tools keep it, and what the installer is to read off each: "address
# gateway dns kind:file", or NONE. The first is the Supermicro X11SSD-F's
# Ubuntu's, as Selectel set it up: an address on a network with no DHCP
NETWORK_ROOTS = {
    "supermicro": ({"etc/netplan/50-cloud-init.yaml": """network:
  version: 2
  renderer: networkd
  ethernets:
    id0:
      match:
        macaddress: "ac:1f:6b:20:53:52"
      optional: true
      addresses:
      - "46.148.233.123/24"
      nameservers:
        addresses:
        - 188.93.17.19
        - 188.93.16.19
      routes:
      - to: "0.0.0.0/0"
        via: "46.148.233.1"
      - scope: "link"
        to: "46.148.233.1"
    id1:
      match:
        macaddress: "ac:1f:6b:20:53:53"
      optional: true
"""}, "46.148.233.123/24 46.148.233.1 188.93.17.19 netplan:/etc/netplan/50-cloud-init.yaml"),
    # A route elsewhere before the addresses, and before the default one
    "netplan-routes": ({"etc/netplan/00-installer-config.yaml": """network:
  ethernets:
    eno1:
      routes:
        - to: 10.0.0.0/8
          via: 192.168.5.254
        - to: default
          via: 192.168.5.1
      addresses:
        - 192.168.5.20/24
      nameservers:
        addresses: [192.168.5.53, 8.8.8.8]
"""}, "192.168.5.20/24 192.168.5.1 192.168.5.53 netplan:/etc/netplan/00-installer-config.yaml"),
    "netplan-gateway4": ({"etc/netplan/01-netcfg.yaml": """network:
  ethernets:
    eth0:
      addresses: [172.16.0.9/20]
      gateway4: 172.16.0.1
      nameservers:
        addresses: [1.1.1.1]
"""}, "172.16.0.9/20 172.16.0.1 1.1.1.1 netplan:/etc/netplan/01-netcfg.yaml"),
    "netplan-dhcp": ({"etc/netplan/50-cloud-init.yaml": """network:
  ethernets:
    eth0:
      dhcp4: true
"""}, "NONE"),
    "ifupdown": ({"etc/network/interfaces": """auto lo
iface lo inet loopback

auto eth0
iface eth0 inet static
    address 203.0.113.5
    netmask 255.255.255.0
    gateway 203.0.113.1
    dns-nameservers 203.0.113.53 8.8.8.8
"""}, "203.0.113.5/24 203.0.113.1 203.0.113.53 ifupdown:/etc/network/interfaces"),
    "networkmanager": ({"etc/NetworkManager/system-connections/eth0.nmconnection": """[connection]
id=eth0
[ipv4]
address1=198.51.100.7/25,198.51.100.1
dns=198.51.100.53;9.9.9.9;
method=manual
"""}, "198.51.100.7/25 198.51.100.1 198.51.100.53 "
          "NetworkManager:/etc/NetworkManager/system-connections/eth0.nmconnection"),
    "networkd": ({"etc/systemd/network/10-eth0.network": """[Match]
Name=eth0
[Network]
Address=192.0.2.10/26
Gateway=192.0.2.1
DNS=192.0.2.53
"""}, "192.0.2.10/26 192.0.2.1 192.0.2.53 systemd-networkd:/etc/systemd/network/10-eth0.network"),
    "ifcfg": ({"etc/sysconfig/network-scripts/ifcfg-eth0": """DEVICE=eth0
BOOTPROTO=none
IPADDR="10.1.2.3"
NETMASK='255.255.0.0'
GATEWAY=10.1.0.1
DNS1=10.1.0.53
"""}, "10.1.2.3/16 10.1.0.1 10.1.0.53 ifcfg:/etc/sysconfig/network-scripts/ifcfg-eth0"),
}
NETWORK_FUNCTIONS = ("valid_cidr", "valid_ip", "mask_bits", "first_host", "root_network")
# Alpine's, as the installer's: its awk is busybox's
INSTALLER_ALPINE = "alpine:3.24"


def shell_functions(path, names):
    text = open(path).read()
    found = []
    for name in names:
        m = re.search(r"^%s\(\) \{\n.*?^\}\n" % re.escape(name), text, re.S | re.M)
        if m is None:
            return None
        found.append(m.group(0))
    return "".join(found)


def network_found(tmp):
    """nos-setup's reading of the network a Linux on the server's disks has,
    run as the installer runs it: its own functions, in its Alpine."""
    print("--- the network a Linux on the disks has", flush=True)
    where = os.path.join(tmp, "network")
    fns = shell_functions(os.path.join(ROOT, "scripts", "installer", "nos-setup"), NETWORK_FUNCTIONS)
    if not check("nos-setup has " + ", ".join(NETWORK_FUNCTIONS), fns is not None):
        return
    for name, (files, _) in NETWORK_ROOTS.items():
        for rel, text in files.items():
            path = os.path.join(where, "roots", name, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                f.write(text)
    with open(os.path.join(where, "fns.sh"), "w") as f:
        f.write(fns)
    with open(os.path.join(where, "run.sh"), "w") as f:
        f.write("set -u\n. /n/fns.sh\n"
                "for r in /n/roots/*; do echo \"${r##*/}=$(root_network \"$r\" || echo NONE)\"; done\n"
                "echo \"first_host=$(first_host 46.148.233.123/24) $(first_host 10.1.2.3/16) "
                "$(first_host 198.51.100.7/25) [$(first_host 192.0.2.1/32)]\"\n"
                "echo \"mask_bits=$(mask_bits 255.255.255.192) $(mask_bits 255.255.0.0)\"\n")
    p = subprocess.run(["docker", "run", "--rm", "--platform", "linux/amd64", "-v", where + ":/n",
                        INSTALLER_ALPINE, "sh", "-c", "apk add -q bash >/dev/null && bash /n/run.sh"],
                       capture_output=True, text=True, timeout=600)
    got = dict(line.split("=", 1) for line in p.stdout.splitlines() if "=" in line)
    for name, (files, want) in NETWORK_ROOTS.items():
        check("%s: %s" % (name, want), got.get(name) == want, "read %r%s" % (got.get(name), p.stderr[-500:]))
    check("a gateway offered is the network's first address, none for a /32",
          got.get("first_host") == "46.148.233.1 10.1.0.1 198.51.100.1 []", got.get("first_host"))
    check("a netmask is its prefix's length", got.get("mask_bits") == "26 16", got.get("mask_bits"))


NETWORK_HERE_FUNCTIONS = ("network_here_is_multi", "network_here_summary", "network_here_toml", "put_ssh_nic")


def network_here(tmp):
    """The two-link server's records become nos's per-interface net.toml."""
    print("--- two links in --network-here", flush=True)
    where = os.path.join(tmp, "network-here")
    os.makedirs(where)
    fns = shell_functions(os.path.join(ROOT, "scripts", "nos-install"), NETWORK_HERE_FUNCTIONS)
    if not check("nos-install has " + ", ".join(NETWORK_HERE_FUNCTIONS), fns is not None):
        return
    data = """default|eth1
dns|77.88.8.8
nic|eth0|dhcp|10.0.0.4/24|10.0.0.1|etx1
nic|eth1|static|94.126.205.237/31|94.126.205.236|etx2
route|eth0|10.0.0.0/8|10.0.0.1
route|eth0|172.16.0.0/12|10.0.0.1
route|eth0|192.168.0.0/16|10.0.0.1
"""
    with open(os.path.join(where, "fns.sh"), "w") as f:
        f.write(fns)
    with open(os.path.join(where, "data"), "w") as f:
        f.write(data)
    with open(os.path.join(where, "run.sh"), "w") as f:
        f.write("set -u\n. /n/fns.sh\ndata=$(cat /n/data)\n"
                "network_here_summary \"$data\"\n"
                "network_here_is_multi \"$data\"\n"
                "network_here_toml \"$data\"\n"
                "mkdir -p /n/etc\n"
                "printf 'sshd start\\n' > /n/etc/rc\n"
                "NET_MODE=multi NET_DEFAULT=eth1\n"
                "say() { printf '%s\\n' \"$*\"; }\n"
                "put_ssh_nic /n\n"
                "cat /n/etc/rc\n")
    p = subprocess.run(["docker", "run", "--rm", "--platform", "linux/amd64", "-v", where + ":/n",
                        INSTALLER_ALPINE, "sh", "-c", "apk add -q bash >/dev/null && bash /n/run.sh"],
                       capture_output=True, text=True, timeout=600)
    want = """ip=94.126.205.237/31 gw=94.126.205.236 dns=77.88.8.8
dhcp_eth0 = true
ip_eth1 = \"94.126.205.237/31\"
gw_eth1 = \"94.126.205.236\"
dns = \"77.88.8.8\"
default = \"eth1\"
route_eth0 = [\"10.0.0.0/8 via 10.0.0.1\", \"172.16.0.0/12 via 10.0.0.1\", \"192.168.0.0/16 via 10.0.0.1\"]
/etc/rc: sshd start 22 nic=eth1, the default route's interface
sshd start 22 nic=eth1
"""
    check("two links keep DHCP inside, static /31 outside, private routes and external sshd",
          p.returncode == 0 and p.stdout == want, "rc %d, got %r, want %r%s" % (p.returncode, p.stdout, want, p.stderr[-500:]))


PANEL_FUNCTIONS = ("add_answer", "run_install", "unattended", "ask_panel", "ask_nbs", "nbs_host_default", "interactive")


def panel_choices(tmp):
    """The dialog and the answers, with nos-install and everything outside
    the panel choice stood in for: what command each choice runs."""
    print("--- the management panel choice", flush=True)
    where = os.path.join(tmp, "panel")
    os.makedirs(where)
    fns = shell_functions(os.path.join(ROOT, "scripts", "installer", "nos-setup"), PANEL_FUNCTIONS)
    lacks = shell_functions(os.path.join(ROOT, "scripts", "installer", "install.sh"), ("nos_lacks",))
    if not check("nos-setup has " + ", ".join(PANEL_FUNCTIONS), fns is not None):
        return
    if not check("install.sh has nos_lacks", lacks is not None):
        return
    with open(os.path.join(where, "fns.sh"), "w") as f:
        f.write(fns)
    with open(os.path.join(where, "lacks.sh"), "w") as f:
        f.write(lacks)
    with open(os.path.join(where, "run.sh"), "w") as f:
        f.write(r"""set -uo pipefail
mode=$1
choice=$2
nbs=${3:-no}
. /n/fns.sh
TITLE="nos installer"
ON_ISO=0
KMSG_LEVEL=6
LOG=/tmp/nos-setup.log
DONE=/tmp/nos-setup.done
KEYS_FILE=/tmp/nos-keys
ROOT_SIZE_DEFAULT=10G
DISK=/dev/test
ROOT_SIZE=10G
WIPE=0
NOS_NET=(--dhcp)
NOS_CMDLINE=
NETCONSOLE=
PANEL=0
NBS=0
NBS_HOST=
RESCUE_LINE=
declare -A ANS=([disk]=/dev/test [wipe]=yes [after]=stay)
KEYS=()
KEY_URLS=()
say() { printf '%s\n' "$*"; }
kmsg() { :; }
hostname() { echo Rescue_Node; }
dialog_() { echo node1; }
image_version() { echo test; }
wait_carrier() { :; }
net_dhcp_chosen() { NOS_NET=(--dhcp); }
net_dhcp() { :; }
gather_keys() { : > "$KEYS_FILE"; }
keys_count() { echo 0; }
in_vm() { return 1; }
nos_install() {
    printf 'INSTALL'
    printf ' %s' "$@"
    printf '\n'
}
clear() { :; }
ask_disk() { :; }
ask_root_size() { :; }
ask_network() { :; }
ask_keys() { :; }
ask_netconsole() { :; }
confirm() { :; }
done_text() { echo done; }
the_end() { exit 0; }
whiptail() {
    case " $* " in
        *" Enable NBS "*) [ "$nbs" = yes ] ;;
        *) [ "$choice" = yes ] ;;
    esac
}
if [ "$mode" = dialog ]; then
    interactive
else
    [ "$choice" = default ] || add_answer panel "$choice"
    if [ "$nbs" != no ]; then
        add_answer nbs yes
        [ "$nbs" = default ] || add_answer nbs_host node1
    fi
    unattended
fi
""")
    p = subprocess.run(["docker", "run", "--rm", "--platform", "linux/amd64", "-v", where + ":/n",
                        INSTALLER_ALPINE, "sh", "-c",
                        "apk add -q bash >/dev/null && "
                        "bash /n/run.sh dialog yes no && bash /n/run.sh dialog yes yes && bash /n/run.sh dialog no no && "
                        "bash /n/run.sh answers yes no && bash /n/run.sh answers yes yes && "
                        "bash /n/run.sh answers yes default && bash /n/run.sh answers no no && "
                        "bash /n/run.sh answers default no && (bash /n/run.sh answers no yes || true) && "
                        "sh -c '. /n/lacks.sh; echo DIALOG=$(nos_lacks dialog 0); "
                        "echo PANEL=$(nos_lacks disk 1); echo PLAIN=$(nos_lacks disk 0)'"],
                       capture_output=True, text=True, timeout=600)
    got = [line for line in p.stdout.splitlines() if line.startswith("INSTALL ")]
    plain = "INSTALL disk /dev/test --root-size 10G --wipe --dhcp"
    enabled = plain + " --panel"
    with_nbs = enabled + " --nbs --nbs-host node1"
    check("the dialog's panel Yes and panel=yes pass --panel",
          p.returncode == 0 and len(got) == 8 and got[0] == enabled and got[3] == enabled,
          "rc %d, %r%s" % (p.returncode, got, p.stderr[-500:]))
    check("NBS Yes in the dialog and nbs=yes in answers add --nbs with the host's name",
          p.returncode == 0 and len(got) == 8 and got[1] == with_nbs and got[4] == with_nbs,
          "rc %d, %r%s" % (p.returncode, got, p.stderr[-500:]))
    check("nbs=yes without nbs_host uses the Linux hostname, sanitized",
          p.returncode == 0 and len(got) == 8 and got[5] == enabled + " --nbs --nbs-host rescue-node",
          "rc %d, %r%s" % (p.returncode, got, p.stderr[-500:]))
    check("the dialog's No, panel=no and no answer leave both options out",
          p.returncode == 0 and len(got) == 8 and got[2] == plain and got[6] == plain and got[7] == plain,
          "rc %d, %r%s" % (p.returncode, got, p.stderr[-500:]))
    check("nbs=yes without panel=yes is refused before an install runs",
          p.returncode == 0 and "FAILED: nbs=yes needs panel=yes" in p.stdout and len(got) == 8,
          "rc %d, %r%s" % (p.returncode, p.stdout[-1000:], p.stderr[-500:]))

    def lacked(kind):
        prefix = kind + "="
        for line in p.stdout.splitlines():
            if line.startswith(prefix):
                return line[len(prefix):].split()
        return []

    check("the rescue installer requires python3 for the dialog and --panel",
          p.returncode == 0 and "python3" in lacked("DIALOG") and "python3" in lacked("PANEL"),
          "rc %d, %r%s" % (p.returncode, p.stdout[-1000:], p.stderr[-500:]))
    check("a rescue disk install without --panel does not require python3",
          p.returncode == 0 and "python3" not in lacked("PLAIN"),
          "rc %d, %r%s" % (p.returncode, p.stdout[-1000:], p.stderr[-500:]))


def arch_rescue(tmp):
    """A rescue Arch gets the same tools as a rescue Debian, by pacman."""
    print("--- the rescue installer on Arch", flush=True)
    where = os.path.join(tmp, "arch")
    os.makedirs(where)
    fns = shell_functions(os.path.join(ROOT, "scripts", "installer", "install.sh"),
                          ("nos_say", "nos_lacks", "nos_arch_packages", "nos_packages"))
    if not check("install.sh has its Arch package path", fns is not None):
        return
    with open(os.path.join(where, "fns.sh"), "w") as f:
        f.write(fns)
    with open(os.path.join(where, "run.sh"), "w") as f:
        f.write(r"""set -eu
. /n/fns.sh
# Pacman's downloader sandbox cannot run under Docker Desktop's emulation.
pacman() { command pacman --disable-sandbox "$@"; }
echo "ARCH_NAMES=$(nos_arch_packages 'grub2-common grub-pc-bin grub-efi-amd64-bin whiptail openssh-client python3 fdisk udev util-linux')"
nos_packages dialog 0
missing=0
for t in whiptail curl ssh-keygen python3 grub-install grub-editenv unsquashfs lsblk blkid sfdisk mkfs.ext4 mkfs.vfat ip udevadm; do
    command -v "$t" >/dev/null 2>&1 || { echo "MISSING $t"; missing=1; }
done
for d in i386-pc x86_64-efi; do
    [ -f "/usr/lib/grub/$d/modinfo.sh" ] || { echo "MISSING grub $d"; missing=1; }
done
[ "$missing" = 0 ] && echo ARCH_RESCUE_OK
""")
    p = subprocess.run(["docker", "run", "--rm", "--platform", "linux/amd64", "-v", where + ":/n",
                        "quay.io/archlinux/archlinux:latest", "sh", "/n/run.sh"],
                       capture_output=True, text=True, timeout=900)
    check("Arch package names cover GRUB, whiptail, SSH, Python, fdisk and udev",
          "ARCH_NAMES=grub libnewt openssh python systemd util-linux" in p.stdout,
          "rc %d, stdout %r, stderr %r" % (p.returncode, p.stdout[-1000:], p.stderr[-500:]))
    check("Arch rescue installs every dialog and disk tool with pacman",
          p.returncode == 0 and "ARCH_RESCUE_OK" in p.stdout,
          "rc %d, stdout %r, stderr %r" % (p.returncode, p.stdout[-2000:], p.stderr[-1000:]))


def download_without_squashfs(tmp):
    release = os.path.join(tmp, "release")
    root = os.path.join(release, "root")
    os.makedirs(root)
    with open(os.path.join(root, "nos-install"), "w") as f:
        f.write("#!/bin/sh\necho NOS_INSTALL_OK \"$@\"\n")
    os.chmod(os.path.join(root, "nos-install"), 0o755)
    with open(os.path.join(root, "VERSION"), "w") as f:
        f.write("version=test\nrevision=test\n")
    script = (
        "set -eu; "
        "mksquashfs /tmp/release/root /tmp/release/nos-x86_64.squashfs -no-progress -no-xattrs >/dev/null; "
        "cd /tmp/release; sha256sum nos-x86_64.squashfs > SHA256SUMS; "
        "mkdir -p /tmp/fakebin; "
        "printf '#!/bin/sh\\nexit 1\\n' > /tmp/fakebin/mount; "
        "cp /tmp/fakebin/mount /tmp/fakebin/modprobe; "
        "chmod +x /tmp/fakebin/mount /tmp/fakebin/modprobe; "
        "PATH=/tmp/fakebin:$PATH NOS_RELEASE_URL=file:///tmp/release /src/scripts/installer/install.sh other")
    p = subprocess.run(["docker", "run", "--rm", "--platform", "linux/amd64", "-v", release + ":/tmp/release",
                        "-v", ROOT + ":/src", "nos-builder", "sh", "-c", script],
                       capture_output=True, text=True, timeout=120)
    return check("rescue install.sh unpacks the image when squashfs cannot be mounted",
                 p.returncode == 0 and "unpacking the image with unsquashfs" in p.stdout
                 and "nos test (test)" in p.stdout and "NOS_INSTALL_OK other" in p.stdout,
                 "rc %d, stdout %r, stderr %r" % (p.returncode, p.stdout[-2000:], p.stderr[-1000:]))


def run(tmp, image):
    if not download_without_squashfs(tmp):
        return
    arch_rescue(tmp)
    network_found(tmp)
    network_here(tmp)
    panel_choices(tmp)
    key = os.path.join(tmp, "id_test")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "installer-test", "-f", key], check=True)
    answers = os.path.join(tmp, "answers")
    with open(answers, "w") as f:
        f.write("# installer-test's\ndisk=%s\nwipe=yes\nroot_size=%dM\nkey=%s\nnetconsole=%s\nafter=poweroff\n"
                % (BY_ID, ROOT_MIB, open(key + ".pub").read().strip(), NETCONSOLE))
        # QEMU's user network as a static address: what a network with no
        # DHCP server needs
        f.write("ip=%s\ngw=%s\ndns=%s\n" % (STATIC_IP, STATIC_GW, STATIC_DNS))
    iso = os.path.join(tmp, "installer.iso")
    print("--- build.sh --answers", flush=True)
    p = subprocess.run([os.path.join(ROOT, "scripts", "installer", "build.sh"), "--image", image,
                        "--answers", answers, "--out", iso], capture_output=True, text=True)
    print("".join("  " + line + "\n" for line in (p.stdout + p.stderr).splitlines()[-10:]), end="", flush=True)
    if not check("build.sh makes the ISO, with the answers", p.returncode == 0 and os.path.exists(iso)):
        return
    packages = []
    packages_path = os.path.join(ROOT, "out", "installer", "packages.txt")
    if os.path.exists(packages_path):
        with open(packages_path) as f:
            packages = f.read().splitlines()
    check("the installer ISO carries python3 for the panel's password",
          "python3" in packages, packages)
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
            check("and none of the network's words", rc == 0 and " ip=" not in out and " gw=" not in out, out)
            rc, out = m.ssh(["net"])
            check("on the static address the answers gave, from /etc/net.toml",
                  rc == 0 and "eth0 settings: %s, gateway %s, resolver %s (/etc/net.toml)"
                  % (STATIC_IP, STATIC_GW, STATIC_DNS) in out, out[-400:])
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
