#!/usr/bin/env python3
"""Put a nos system image on a server and boot it once, from here.

    scripts/nos-deploy.py HOST install PARTITION [options]   the first time
    scripts/nos-deploy.py HOST [update] [options]            every time after

HOST is how ssh reaches the server's Linux as root (root@1.2.3.4, or a name
from ~/.ssh/config). The image -- nos-x86_64.squashfs here, from make image
or scripts/build-image.sh, unless --image names another -- is copied there,
checked, mounted, and the nos-install in it is run: install onto PARTITION,
or update the partition installed before (docs/install.md). It arms one boot
of nos and reboots the server, then waits for nos's own sshd to answer on
port 22 and, with a key it lets in, checks that the kernel running is the
image's. The boot after that is the server's default again: `reboot` in nos
gets it back.

Options:
  --image FILE          the image (default: nos-x86_64.squashfs here)
  --no-boot             put the image there, and do not reboot into it
  --wait SECONDS        how long nos has to come up (600)
  --ssh-key FILE|LINE   a key nos's sshd lets in -- a .pub file here, or the
                        line itself; may be given more than once
  --udpshell[=PORT], --netconsole IP:PORT, --cmdline ARGS, --format
                        nos-install's own (scripts/nos-install --help)
"""

import argparse
import hashlib
import os
import shlex
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
REMOTE_IMAGE = "/tmp/nos-x86_64.squashfs"
NOS_BANNER = "SSH-2.0-nos_sshd"
SSH_OPTS = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "-o", "ServerAliveInterval=15"]


def die(msg):
    sys.exit("nos-deploy: " + msg)


def say(msg):
    print("nos-deploy: " + msg, flush=True)


def resolve(host):
    """The address and port ssh would connect to for HOST."""
    p = subprocess.run(["ssh", "-G", host], capture_output=True, text=True)
    name, port = host.rsplit("@", 1)[-1], 22
    for line in p.stdout.splitlines():
        key, _, value = line.partition(" ")
        if key == "hostname":
            name = value
        elif key == "port":
            port = int(value)
    return name, port


def banner(addr):
    """What answers on the SSH port first, or None when nothing does."""
    try:
        with socket.create_connection(addr, timeout=5) as s:
            s.settimeout(5)
            return s.recv(255).decode(errors="replace").strip()
    except OSError:
        return None


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def key_lines(arg):
    if os.path.isfile(os.path.expanduser(arg)):
        text = open(os.path.expanduser(arg)).read()
        lines = [line.strip() for line in text.splitlines() if line.strip() and not line.startswith("#")]
        if not lines:
            die("--ssh-key: no key in %s" % arg)
        return lines
    if arg.startswith("ssh-"):
        return [arg]
    die("--ssh-key: no file %s here, and it is not a key" % arg)


def main():
    if any(a in ("-h", "--help") for a in sys.argv[1:]):
        print(__doc__)
        return
    p = argparse.ArgumentParser(prog="nos-deploy.py", add_help=False,
                                usage="%(prog)s HOST [install PARTITION | update] [options] (--help)")
    p.add_argument("host")
    p.add_argument("command", nargs="?", default="update", choices=["install", "update"])
    p.add_argument("partition", nargs="?")
    p.add_argument("--image", default=os.path.join(ROOT, "nos-x86_64.squashfs"))
    p.add_argument("--no-boot", action="store_true")
    p.add_argument("--wait", type=int, default=600)
    p.add_argument("--ssh-key", action="append", default=[])
    p.add_argument("--udpshell", nargs="?", const="", default=None)
    p.add_argument("--netconsole")
    p.add_argument("--cmdline")
    p.add_argument("--format", action="store_true")
    args = p.parse_args()
    if args.command == "install" and not args.partition:
        die("install onto which partition of %s?" % args.host)
    if args.command == "update" and args.partition is not None and args.partition.startswith("-"):
        die("unknown option %s" % args.partition)
    if not os.path.isfile(args.image):
        die("no image at %s: make image, or scripts/build-image.sh" % args.image)

    install = [args.command] + ([args.partition] if args.partition else [])
    for arg in args.ssh_key:
        for line in key_lines(arg):
            install += ["--ssh-key", line]
    if args.udpshell is not None:
        install.append("--udpshell" + ("=" + args.udpshell if args.udpshell else ""))
    if args.netconsole:
        install += ["--netconsole", args.netconsole]
    if args.cmdline is not None:
        install += ["--cmdline", args.cmdline]
    if args.format:
        install.append("--format")
    if not args.no_boot:
        install.append("--boot-once")

    digest = sha256(args.image)
    say("%s (sha256 %s) to %s:%s" % (os.path.basename(args.image), digest[:16], args.host, REMOTE_IMAGE))
    if subprocess.run(["scp", "-q"] + SSH_OPTS + [args.image, "%s:%s" % (args.host, REMOTE_IMAGE)]).returncode != 0:
        die("scp to %s failed" % args.host)

    # Checked there before it is used, mounted, its VERSION said, and its own
    # nos-install run -- the one that came with the kernel it installs
    script = """set -euo pipefail
SUDO=; [ "$(id -u)" = 0 ] || SUDO=sudo
echo "%(digest)s  %(image)s" | sha256sum -c --quiet - || { echo "the image was damaged on the way" >&2; exit 1; }
D=$(mktemp -d /tmp/nos-deploy.XXXXXX)
$SUDO mount -t squashfs -o loop,ro %(image)s "$D"
trap '$SUDO umount "$D"; rmdir "$D"' EXIT
echo "image: $(sed -n 's/^version=//p' "$D/VERSION") ($(sed -n 's/^revision=//p' "$D/VERSION"))"
$SUDO "$D/nos-install" %(install)s
""" % {"digest": digest, "image": REMOTE_IMAGE, "install": " ".join(shlex.quote(a) for a in install)}
    p = subprocess.run(["ssh"] + SSH_OPTS + [args.host, "bash", "-s"], input=script, text=True,
                       capture_output=True)
    sys.stdout.write(p.stdout)
    sys.stderr.write(p.stderr)
    if p.returncode != 0:
        die("nos-install on %s failed" % args.host)
    want = None
    for line in p.stdout.splitlines():
        if line.startswith("image: "):
            want = "nos " + line[len("image: "):]
    if args.no_boot:
        return

    addr = resolve(args.host)
    say("rebooting %s" % args.host)
    # Left running there with nothing of this ssh's open, so the ssh returns
    subprocess.run(["ssh", "-n"] + SSH_OPTS + [args.host, "(setsid nohup sh -c 'sleep 2; systemctl reboot || reboot' "
                                                           "> /dev/null 2>&1 < /dev/null &)"], timeout=60)

    # Its Linux goes away, then nos's sshd answers in its place
    start = time.time()
    gone = False
    while time.time() - start < args.wait:
        seen = banner(addr)
        if seen is not None and seen.startswith(NOS_BANNER):
            break
        if seen is None or not seen.startswith("SSH-"):
            gone = True
        elif gone:
            say("%s came back with %s, not nos -- GRUB did not boot it (docs/install.md)" % (args.host, seen))
            sys.exit(1)
        time.sleep(5)
    else:
        die("no nos on %s:%d after %d s -- a hardware reset brings its Linux back; netconsole= or "
            "disklog=on say why (docs/install.md)" % (addr[0], addr[1], args.wait))
    say("nos answers on %s:%d after %d s" % (addr[0], addr[1], time.time() - start))

    # Its own host key, not the Linux one known_hosts has: not checked here
    p = subprocess.run(["ssh"] + SSH_OPTS + ["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                                             "-o", "LogLevel=ERROR", args.host, "version"],
                       capture_output=True, text=True, timeout=120)
    if p.returncode != 0:
        say("logging in to check its version did not work: does its /etc/ssh/authorized_keys have your key? "
            "(--ssh-key)")
        return
    running = p.stdout.strip()
    if want is not None and running != want:
        die("running %s, but the image is %s" % (running, want))
    say("running %s -- reboot in it gets the server's default back" % running)


if __name__ == "__main__":
    main()
