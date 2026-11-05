#!/usr/bin/env python3
"""ipcnet between real kernels (plans/ipc.md, I4): two or three nos on a LAN
of scripts/hub.py's, each with ipcnet.ko, pinned to one another by keys the
test makes -- and what the channel between hosts promises, held to.

Each VM has one NIC, eth0, on the hub, which answers its DHCP with
10.9.0.<n> and carries everything between the hosts -- dropping, delaying
and cutting it when told -- and relays the test's UDP shell to it past all
of that. kernel.echo runs on every host, a `Cluster` name, and `ipc ping
kernel.echo@<host>` is what is sent between them.

    scripts/ipc-test.py                  arm64: HVF on an Apple Silicon Mac, TCG otherwise
    scripts/ipc-test.py --arch x86_64    KVM where the host has it
    scripts/ipc-test.py --keep           keep the logs and images

Needs a built tree: nos-arm64.img or nos.iso, and out/<arch>/modules/ipcnet.ko;
openssl, for the hosts' keys.
"""

import argparse
import importlib.util
import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, file))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


pt = load("pt", "parttest.py")
hubmod = load("hub", "hub.py")

HOSTS = ["a", "b", "c"]
CLUSTER = "0123456789abcdef0123456789abcdef"
SHELL_PORT = 9100
HUB_BASE = 47700
FAILED = []


def check(name, ok, detail=""):
    if not pt.check(name, ok, detail):
        FAILED.append(name)
    return ok


class Shell(pt.Shell):
    """The UDP shell of one VM: its own port."""

    def __init__(self, port, timeout=120):
        super().__init__(timeout)
        self.port = port

    def run(self, cmd):
        self.seq += 1
        body = cmd.encode()
        self.sock.sendto(pt.SHELL_HDR.pack(pt.SHELL_MAGIC, self.seq, 0, 0, len(body), 0) + body,
                         ("127.0.0.1", self.port))
        chunks, last = {}, None
        while True:
            data, _ = self.sock.recvfrom(4096)
            magic, seq, idx, flags, length, _ = pt.SHELL_HDR.unpack(data[:pt.SHELL_HDR.size])
            if magic != pt.SHELL_MAGIC or seq != self.seq:
                continue
            chunks[idx] = data[pt.SHELL_HDR.size:pt.SHELL_HDR.size + length]
            if flags & pt.SHELL_LAST:
                last = idx
            if last is not None and len(chunks) == last + 1:
                out = b"".join(chunks[i] for i in range(last + 1)).decode(errors="replace")
                print("  [%d] $ %s%s" % (self.port, cmd, "".join("\n      " + l for l in out.splitlines())),
                      flush=True)
                return out


def make_key(tmp, name):
    """An EC P-256 key and its certificate, PEM, and the key's fingerprint
    as tls::pinned takes it: the SHA-256 of its SubjectPublicKeyInfo."""
    d = os.path.join(tmp, "keys", name)
    os.makedirs(d)
    key, cert = os.path.join(d, "key.pem"), os.path.join(d, "cert.pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
                    "-nodes", "-keyout", key, "-out", cert, "-subj", "/CN=nos ipcnet " + name, "-days", "3650"],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    pub = subprocess.run(["openssl", "x509", "-in", cert, "-pubkey", "-noout"], check=True, capture_output=True).stdout
    der = subprocess.run(["openssl", "pkey", "-pubin", "-outform", "DER"], input=pub, check=True,
                         capture_output=True).stdout
    import hashlib
    return key, cert, hashlib.sha256(der).hexdigest()


def rootfs(tmp, arch, name, key, peers):
    """A root for host `name`: ipcnet.ko, the key and certificate `key` as
    its own, and hosts.toml naming `peers` -- (name, address, fingerprint)
    each."""
    rootdir = os.path.join(tmp, "root-" + name)
    os.makedirs(os.path.join(rootdir, "etc", "ipc", "tls"))
    ko = os.path.join(ROOT, "out", arch, "modules", "ipcnet.ko")
    if not os.path.exists(ko):
        sys.exit("no %s -- build the tree first" % ko)
    shutil.copy(ko, rootdir)
    shutil.copy(key[0], os.path.join(rootdir, "etc", "ipc", "tls", "key.pem"))
    shutil.copy(key[1], os.path.join(rootdir, "etc", "ipc", "tls", "cert.pem"))
    entries = ",\n  ".join('"%s %s %s"' % p for p in peers)
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# ipc-test.py\nname = "%s"\ncluster = "%s"\npeers = [\n  %s,\n]\n' % (name, CLUSTER, entries))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# ipc-test.py\ninsmod /ipcnet.ko\nipc echo start\n")
    image = os.path.join(tmp, "root-%s.img" % name)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def accel_of(arch):
    if arch == "aarch64":
        if platform.system() == "Darwin" and platform.machine() == "arm64":
            return "hvf"
        return "kvm" if os.path.exists("/dev/kvm") and platform.machine() == "aarch64" else "tcg"
    return "kvm" if os.path.exists("/dev/kvm") and platform.machine() in ("x86_64", "AMD64") else "tcg"


def qemu(arch, accel, image, log, hub, i):
    lan_mac = hub.mac(i)
    if arch == "aarch64":
        cpu = {"hvf": ["-accel", "hvf", "-cpu", "host"], "kvm": ["-accel", "kvm", "-cpu", "host"]}.get(
            accel, ["-accel", "tcg", "-cpu", "cortex-a72"])
        return ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "2", "-m", "1024"] + cpu + [
            "-kernel", os.path.join(ROOT, "nos-arm64.img"),
            "-append", "root=auto dhcp=auto udpshell=%d" % SHELL_PORT,
            "-global", "virtio-mmio.force-legacy=false",
            "-drive", "file=%s,format=raw,id=hd0,if=none" % image,
            "-device", "virtio-blk-device,drive=hd0",
            "-device", "virtio-net-device,netdev=lan,mac=%s" % lan_mac,
            "-netdev", hub.netdev(i),
            "-device", "virtio-rng-device",
            "-serial", "file:" + log, "-display", "none"]
    cpu = ["-enable-kvm", "-cpu", "host"] if accel == "kvm" else ["-cpu", "max"]
    return ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "2"] + cpu + [
        "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
        "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
        "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
        "-device", "virtio-net-pci,netdev=lan,mac=%s,disable-legacy=on,disable-modern=off" % lan_mac,
        "-netdev", hub.netdev(i),
        "-device", "virtio-rng-pci"]


PING = re.compile(r"ipc ping: to=\S+ sent=(\d+) answered=(\d+) wrong=(\d+) bounced=(\d+) timeouts=(\d+) "
                  r"errors=(\d+) down=(\w+) reordered=(\d+) min_us=(\d+) p50_us=(\d+) p99_us=(\d+) max_us=(\d+) "
                  r"mb_per_s=(\d+)")


def ping(sh, to, extra=""):
    out = sh.run("ipc ping %s %s" % (to, extra))
    m = PING.search(out)
    if not m:
        return None
    keys = ["sent", "answered", "wrong", "bounced", "timeouts", "errors", "down", "reordered", "min", "p50", "p99",
            "max", "mbs"]
    v = dict(zip(keys, m.groups()))
    return {k: (x == "true" if k == "down" else int(x)) for k, x in v.items()}


def peer_up(sh, name):
    return re.search(r"^\s+%s\s+\S+\s+\S+ -- up " % re.escape(name), sh.run("ipcnet"), re.M) is not None


def wait_for(what, timeout, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        if what():
            return True
        time.sleep(step)
    return False


def ipcnet_line(sh, name):
    """The `ipcnet` line of the peer `name`, as host `sh` has it."""
    m = re.search(r"^\s+%s\s+.*$" % re.escape(name), sh.run("ipcnet"), re.M)
    return m.group(0) if m else ""


def counter(text, name):
    m = re.search(r"%s[ =](\d+)" % re.escape(name), text)
    return int(m.group(1)) if m else -1


def in_thread(f):
    """`f` run beside the test: its answer, once joined."""
    import threading
    box = {}
    t = threading.Thread(target=lambda: box.setdefault("r", f()), daemon=True)
    t.start()
    return t, box


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--arch", choices=["x86_64", "aarch64"], default="aarch64")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    accel = accel_of(args.arch)
    tmp = tempfile.mkdtemp(prefix="nos-ipc-")
    print("ipc-test: %s, %s, scratch in %s" % (args.arch, accel, tmp), flush=True)
    keys = {h: make_key(tmp, h) for h in ("a", "b", "c", "c-pinned")}
    ip = hubmod.Hub.ip
    fp = lambda h: keys[h][2]
    # a pins b, and pins c by a key c does not have; c holds b's key and
    # pins a; b pins a alone, so c is a stranger to it
    hosts = [("a", keys["a"], [("b", ip(1), fp("b")), ("c", ip(2), fp("c-pinned"))]),
             ("b", keys["b"], [("a", ip(0), fp("a"))]),
             ("c", keys["b"], [("a", ip(0), fp("a"))])]
    n = len(hosts)
    images = [rootfs(tmp, args.arch, name, key, peers) for name, key, peers in hosts]
    hub = hubmod.Hub(n, HUB_BASE, shell_port=SHELL_PORT).start()
    procs, logs = [], []
    try:
        for i in range(n):
            log = os.path.join(tmp, "serial-%s.log" % HOSTS[i])
            logs.append(log)
            procs.append(subprocess.Popen(qemu(args.arch, accel, images[i], log, hub, i), cwd=ROOT,
                                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        for i in range(n):
            if not check("%s boots" % HOSTS[i], pt.wait_log(logs[i], "boot: complete", 300)):
                return 1
        for i in range(n):
            check("%s asks the hub for its address" % HOSTS[i], wait_for(lambda: i in hub.acked, 60))
        time.sleep(1)
        a, b, c = [Shell(hub.shell(i)) for i in range(n)]
        for i, sh in enumerate((a, b, c)):
            check("%s has %s from the hub's DHCP" % (HOSTS[i], ip(i)), "ip:%s " % ip(i) in sh.run("net"))
        for sh in (a, b, c):
            sh.run("ipcnet start")

        # -- up, pinned both ways --
        check("a has b up", wait_for(lambda: peer_up(a, "b"), 30, 1))
        check("b has a up", wait_for(lambda: peer_up(b, "a"), 30, 1))
        check("a says so in its log", "ipcnet: b (%s) up" % ip(1) in a.run("dmesg"))
        r = ping(a, "kernel.echo@b", "count=200 bytes=64")
        check("a's 200 small pings to b's echo all answered, every byte as sent, in order",
              r is not None and r["answered"] == 200 and r["wrong"] == 0 and r["reordered"] == 0, r)
        r = ping(b, "kernel.echo@a", "count=200 bytes=1000 window=8")
        check("and b's to a's, eight in flight", r is not None and r["answered"] == 200 and r["wrong"] == 0
              and r["reordered"] == 0, r)
        r = ping(a, "kernel.echo@b", "count=16 bytes=1048576 window=4 bulk timeout=10000")
        check("a megabyte at a time by the bulk connection, every byte back",
              r is not None and r["answered"] == 16 and r["wrong"] == 0 and r["reordered"] == 0, r)
        out = a.run("tcpstat")
        check("a's links to b have their 20 ms floor", "floor=20" in out, out)

        # -- bounces --
        r = ping(a, "nobody@b", "count=3")
        check("a message to a name nobody has there bounced back, Gone",
              r is not None and r["bounced"] == 3 and r["answered"] == 0, r)
        r = ping(a, "kernel.echo@b", "count=200 bytes=262144 window=64 bulk timeout=20000")
        check("messages past b's echo's mailbox bounced back, Full, the rest answered whole",
              r is not None and r["bounced"] > 0 and r["answered"] + r["bounced"] == 200 and r["wrong"] == 0, r)
        out = a.run("ipc")
        check("a counts the bounces", re.search(r"bounced [1-9]", out) is not None, out)

        # -- the wire lossy and late --
        hub.impair(0, 1, loss=13, delay_ms=2, jitter_ms=4)
        r = ping(a, "kernel.echo@b", "count=300 bytes=2000 window=4 timeout=20000")
        check("under 5% loss and a jittered delay, every message answered, whole and in order",
              r is not None and r["answered"] == 300 and r["wrong"] == 0 and r["reordered"] == 0, r)
        r = ping(b, "kernel.echo@a", "count=8 bytes=1048576 window=2 bulk timeout=30000")
        check("and every megabyte too", r is not None and r["answered"] == 8 and r["wrong"] == 0, r)
        hub.impair(0, 1)
        check("the links' retransmissions counted", counter(a.run("tcpstat"), "retx") > 0)

        # -- a cut: lost within 3 s either way, a monitor told, back up after --
        t, box = in_thread(lambda: ping(b, "kernel.echo@a", "count=5000 bytes=64 timeout=20000"))
        time.sleep(2)
        hub.cut(0, 1)
        cut_at = time.time()
        lost = wait_for(lambda: "down:" in ipcnet_line(a, "b") and "down:" in ipcnet_line(b, "a"), 15, 0.25)
        took = time.time() - cut_at
        check("a cut link: both hosts lose each other within 3 s and a margin", lost and took < 5.5,
              "%.1f s" % took)
        t.join(60)
        r = box.get("r")
        check("and the ping running across it ends with its monitor's Down, HostLost",
              r is not None and r["down"] and r["wrong"] == 0, r)
        hub.heal(0, 1)
        check("healed, the two are up again", wait_for(lambda: peer_up(a, "b") and peer_up(b, "a"), 30, 1))
        r = ping(a, "kernel.echo@b", "count=50")
        check("and talk again", r is not None and r["answered"] == 50, r)

        # -- strangers --
        line = ipcnet_line(a, "c")
        check("a host whose key is not the one pinned for it is never up", "down:" in line, line)
        out = a.run("ipcnet")
        check("its handshake refused by a, which dials it", counter(out, "handshakes refused") >= 0
              and re.search(r"handshakes refused \d+ in / [1-9]\d* out", out) is not None, out)
        before = counter(b.run("tcpstat"), "refused")
        c.run("wget http://%s:39240/ /x" % ip(1))
        out = b.run("tcpstat")
        check("a stranger's SYNs to b's port dropped there, counted, with no slot taken",
              counter(out, "refused") > before and ":39240 -> %s" % ip(2) not in out, out)
        r = ping(a, "kernel.echo@b", "count=20")
        check("and a and b talk on", r is not None and r["answered"] == 20, r)

        # -- unloaded under traffic, and back --
        t, box = in_thread(lambda: ping(b, "kernel.echo@a", "count=5000 bytes=1000 window=4 timeout=20000"))
        time.sleep(2)
        started = time.time()
        out = a.run("rmmod ipcnet")
        check("rmmod ipcnet under traffic returns promptly", time.time() - started < 15, out)
        t.join(60)
        r = box.get("r")
        check("and the ping across it ends, its monitor told", r is not None and (r["down"] or r["timeouts"] > 0)
              and r["wrong"] == 0, r)
        check("b has a down", wait_for(lambda: "down:" in ipcnet_line(b, "a"), 10, 0.5))
        out = a.run("ipc peers")
        check("a has no router left, and b not up there", "no router" in out
              and re.search(r"^\s+\d+\s+b\s+up", out, re.M) is None, out)
        a.run("insmod /ipcnet.ko")
        a.run("ipcnet start")
        check("loaded again, a and b are up", wait_for(lambda: peer_up(a, "b") and peer_up(b, "a"), 30, 1))
        r = ping(b, "kernel.echo@a", "count=50")
        check("and talk", r is not None and r["answered"] == 50 and r["wrong"] == 0, r)
    finally:
        for p in procs:
            pt.kill(p)
        hub.close()
        text = "".join(open(l, errors="replace").read() for l in logs if os.path.exists(l))
        check("nothing panicked", "PANIC" not in text)
        if args.keep or FAILED:
            print("ipc-test: logs in %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("ipc-test: %s" % ("FAILED: " + "; ".join(FAILED) if FAILED else "passed"))
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
