#!/usr/bin/env python3
"""hv-nbs-test: Linux guests of nos's hypervisor on volumes of NBS's
(plans/nbs.md, N8) -- three x86-64 nos VMs on scripts/hub.py's network, each
with ipcnet.ko, cluster.ko, nbs-mds.ko, nbs-cs.ko and hv.ko, a registry and an
NBS cluster of the three, a pool partition of each its chunk server's
device. Then:

- an image poured onto a volume on a (`hv convert <raw> nbs:<volume>`) and
  read by a guest on b: what its /dev/vda holds is what was poured;
- a guest on a with `disk=nbs:gdisk`: its disk the volume's size, its serial
  `nbs-<number>`; written with dd, synced, read back past the page cache the
  same; stopped -- the volume let go of, detached;
- a guest on b with it: what a's wrote is there;
- a's start refused while b's guest has the volume, and with `force` taken:
  b's guest's next write fails, fenced, and a's guest reads what was there;
- host c killed while a's guest writes: its replicas taken out, the write
  done all the same, read back the same;
- a's guest stopped with c gone: the volume let go of all the same.

A manual gate, as hv-linux-test.py is: a guest kernel and an initrd by hand
(docs/testing.md has the recipe -- a 6.x bzImage with virtio blk and pci,
devtmpfs; Ubuntu's static BusyBox, an /init that does `dmesg -n 1`).
x86-64 nos under KVM where the host has it -- the guests nested, AMD-V on
AMD, VT-x on Intel -- else TCG, slowly.

    make nos-test.iso TEST_ISO_ARGS=udpshell=9100
    scripts/hv-nbs-test.py --bzimage <bzImage> --initrd <initrd.cpio.gz>

Exit code 0 = every check passed.
"""

import argparse
import hashlib
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, file))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


nbst = load("nbst", "nbs-test.py")
pt, hubmod, ipct = nbst.pt, nbst.hubmod, nbst.ipct

HOSTS = nbst.HOSTS
ARCH = "x86_64"
MODULES = ("ipcnet", "cluster", "nbs-mds", "nbs-cs", "hv")
SHELL_PORT = nbst.SHELL_PORT
HUB_BASE = 48100
CMDLINE = "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"
GUEST_MIB = 256
VOLUME_MIB = 64
# What the pour's image and the guest's first write are: 8 MiB each
WRITE_MIB = 8
# The write a host is killed under: 16 MiB, after the first
LONG_MIB = 16
FAILED = []


def check(name, ok, detail=""):
    if not pt.check(name, ok, detail):
        FAILED.append(name)
    return ok


class Stop(Exception):
    pass


def need(name, ok, detail=""):
    if not check(name, ok, detail):
        raise Stop(name)


def rootfs(tmp, name, cluster, bzimage, initrd, raw):
    """Host `name`'s root: the modules, the guest's kernel and initrd, the
    image to pour, and hosts.toml naming it alone, in a cluster of its own
    until it joins one."""
    rootdir = os.path.join(tmp, "root-" + name)
    os.makedirs(os.path.join(rootdir, "etc", "ipc"))
    for m in MODULES:
        ko = pt.module_path(ARCH, m)
        if not os.path.exists(ko):
            sys.exit("no %s -- build the tree first" % ko)
        shutil.copy(ko, os.path.join(rootdir, m + ".ko"))
    shutil.copy(bzimage, os.path.join(rootdir, "bzImage"))
    shutil.copy(initrd, os.path.join(rootdir, "initrd"))
    shutil.copy(raw, os.path.join(rootdir, "img.raw"))
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# hv-nbs-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, cluster))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# hv-nbs-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /nbs-mds.ko\n"
                "insmod /nbs-cs.ko\ninsmod /hv.ko\nhv on\n")
    image = os.path.join(tmp, "root-%s.img" % name)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "160", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def x(vm, line, secs=60):
    """`line` run in guest `vm`'s shell, its output back."""
    return "hv exec %d secs=%d %s" % (vm, secs, line)


def started(out):
    """The number of the VM `hv start` started, or None."""
    m = re.search(r"hv: vm (\d+) started", out)
    return int(m.group(1)) if m else None


def md5_of(out):
    """The first md5 sum in `out`."""
    m = re.search(r"\b([0-9a-f]{32})\b", out)
    return m.group(1) if m else None


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--bzimage", required=True)
    ap.add_argument("--initrd", required=True)
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="an ISO whose command line has udpshell=%d" % SHELL_PORT)
    ap.add_argument("--vm-secs", type=int, default=300, help="how long a guest is given to reach its shell")
    args = ap.parse_args()
    accel = ipct.accel_of(ARCH)
    if not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, SHELL_PORT))
    tmp = tempfile.mkdtemp(prefix="nos-hvnbs-")
    print("hv-nbs-test: %s, scratch in %s" % (accel, tmp), flush=True)
    raw = os.path.join(tmp, "img.raw")
    with open(raw, "wb") as f:
        f.write(os.urandom(WRITE_MIB << 20))
    poured_sum = hashlib.md5(open(raw, "rb").read()).hexdigest()
    ip = hubmod.Hub.ip
    images = [rootfs(tmp, h, os.urandom(16).hex(), args.bzimage, args.initrd, raw) for h in HOSTS]
    guids = {h: [uuid.uuid4() for _ in nbst.POOL_PARTS] for h in HOSTS}
    ids = {h: ["part:%s" % g for g in guids[h]] for h in HOSTS}
    pools = [nbst.pool(tmp, h, guids[h]) for h in HOSTS]
    n = len(HOSTS)
    hub = hubmod.Hub(n, HUB_BASE, shell_port=SHELL_PORT).start()
    procs = [None] * n

    def boot(i):
        log = os.path.join(tmp, "serial-%s.log" % HOSTS[i])
        procs[i] = subprocess.Popen(ipct.qemu(ARCH, accel, images[i], log, hub, i, args.iso, disks=[pools[i]]),
                                    cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return log

    try:
        logs = [boot(i) for i in range(n)]
        for i in range(n):
            need("%s boots" % HOSTS[i], pt.wait_log(logs[i], "boot: complete", 600))
        for i in range(n):
            need("%s asks the hub for its address" % HOSTS[i], ipct.wait_for(lambda: i in hub.acked, 60))
        time.sleep(1)
        sh = [ipct.Shell(hub.shell(i), timeout=900) for i in range(n)]
        a, b, c = sh

        # -- the registry, an NBS cluster of the three, a device each --
        need("the registry made on a", "made" in a.run("cluster init test"))
        for i, host in ((1, "b"), (2, "c")):
            token = nbst.token_of(a.run("cluster invite %s %s %s" % (host, ip(i), nbst.key_of(sh[i]))))
            need("a invites %s" % host, token is not None)
            need("%s joins the registry" % host, "joined as node" in sh[i].run("cluster join %s" % token))
        everyone = [(p, q) for p in sh for q in HOSTS if q != HOSTS[sh.index(p)]]
        need("every host up to every other", ipct.wait_for(lambda: all(ipct.peer_up(p, q) for p, q in everyone), 60, 1))
        need("a makes the NBS cluster", "made" in a.run("nbs init store"))
        need("b added", re.search(r"b is member \d+", a.run("nbs host add b")) is not None)
        need("c added", re.search(r"c is member \d+", a.run("nbs host add c")) is not None)
        need("three members, three voters, one leader", ipct.wait_for(lambda: nbst.agreed(sh, 3), 90, 1))
        for i, h in enumerate(HOSTS):
            out = sh[i].run("nbs devices add %s" % ids[h][0])
            need("%s's first partition its chunk server's device" % h, "in the metadata" in out, out)
        need("every chunk server serving its device",
             ipct.wait_for(lambda: all("serving" in x_.run("nbscs") for x_ in sh), 60, 1))
        for name in ("poured", "gdisk"):
            out = a.run("nbs volume create %s %d" % (name, VOLUME_MIB))
            need("volume %s made" % name, re.search(r"volume %s \((\d+)\)" % name, out) is not None, out)
        gdisk = int(re.search(r"volume gdisk \((\d+)\)", a.run("nbs volume gdisk")).group(1))

        start = lambda vol, more="": "hv start /bzImage mem=%d initrd=/initrd disk=nbs:%s%s cmdline=%s" % (
            GUEST_MIB, vol, more, CMDLINE)

        def shell_up(s, vm):
            out = s.run(x(vm, "id", args.vm_secs))
            return "uid=0" in out and s.run(x(vm, "mount -t devtmpfs devtmpfs /dev")) is not None

        def volume_sum(s, vm, skip_mib, count_mib):
            out = s.run(x(vm, "echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=1M skip=%d count=%d 2>/dev/null "
                              "| md5sum" % (skip_mib, count_mib), 300))
            return md5_of(out), out

        # -- an image poured onto a volume, read by a guest elsewhere --
        out = a.run("hv convert /img.raw nbs:poured")
        check("a raw image poured onto nbs:poured, flushed and let go of", "flushed and let go of" in out, out)
        out = b.run(start("poured"))
        vb = started(out)
        need("a guest on b off nbs:poured", vb is not None, out)
        need("its shell up", shell_up(b, vb))
        got, out = volume_sum(b, vb, 0, WRITE_MIB)
        check("its /dev/vda holds what was poured", got == poured_sum, out)
        b.run("hv stop %d" % vb)

        # -- a guest on a, off gdisk --
        out = a.run(start("gdisk"))
        va = started(out)
        need("a guest on a off nbs:gdisk", va is not None, out)
        need("its shell up", shell_up(a, va))
        out = a.run(x(va, "cat /sys/block/vda/size"))
        check("its disk the volume's size", ("%d" % (VOLUME_MIB << 11)) in out, out)
        out = a.run(x(va, "cat /sys/block/vda/serial"))
        check("its serial the volume's number", ("nbs-%d" % gdisk) in out, out)
        out = a.run(x(va, "dd if=/dev/urandom of=/w bs=1M count=%d 2>/dev/null; md5sum /w" % WRITE_MIB, 120))
        written = md5_of(out)
        need("8 MiB of noise made in the guest", written is not None, out)
        out = a.run(x(va, "dd if=/w of=/dev/vda bs=1M conv=fsync 2>/dev/null; echo nos-rc=$?", 300))
        check("written to its disk, synced", "nos-rc=0" in out, out)
        got, out = volume_sum(a, va, 0, WRITE_MIB)
        check("read back past the page cache, the same", got == written, out)
        a.run("hv stop %d" % va)
        check("stopped, the volume let go of",
              ipct.wait_for(lambda: "detached" in a.run("nbs volume gdisk"), 60, 1), a.run("nbs volume gdisk"))

        # -- on b: what a wrote is there --
        out = b.run(start("gdisk"))
        vb = started(out)
        need("a guest on b off nbs:gdisk", vb is not None, out)
        need("its shell up", shell_up(b, vb))
        got, out = volume_sum(b, vb, 0, WRITE_MIB)
        check("what a's guest wrote is there", got == written, out)

        # -- taken by force from b --
        out = a.run(start("gdisk"))
        check("a's start refused while b's guest has it", started(out) is None and "another host" in out, out)
        out = a.run(start("gdisk", " force"))
        va = started(out)
        need("with force, a's guest takes it", va is not None, out)
        need("its shell up", shell_up(a, va))
        out = b.run(x(vb, "dd if=/dev/zero of=/dev/vda bs=4k count=1 conv=fsync 2>&1; echo nos-rc=$?", 120))
        check("b's guest's next write fails: fenced", "nos-rc=0" not in out and "nos-rc=" in out, out)
        got, out = volume_sum(a, va, 0, WRITE_MIB)
        check("a's guest reads what was there, b's write never landed", got == written, out)
        b.run("hv stop %d" % vb)

        # -- c killed while a's guest writes --
        out = a.run(x(va, "dd if=/dev/urandom of=/w2 bs=1M count=%d 2>/dev/null; md5sum /w2" % LONG_MIB, 120))
        long_sum = md5_of(out)
        need("16 MiB more of noise made", long_sum is not None, out)
        a.run("hv send %d dd if=/w2 of=/dev/vda bs=1M seek=%d conv=fsync 2>/dev/null; echo nos-long-rc=$?\\n"
              % (va, WRITE_MIB))
        time.sleep(3)
        pt.kill(procs[2])
        procs[2] = None
        out = a.run("hv wait %d secs=300 nos-long-rc=" % (va,))
        out = a.run("hv console %d" % va)
        check("c killed under the write, the write done all the same", "nos-long-rc=0" in out, out[-600:])
        got, out = volume_sum(a, va, WRITE_MIB, LONG_MIB)
        check("and read back the same", got == long_sum, out)
        got, out = volume_sum(a, va, 0, WRITE_MIB)
        check("what was there before kept", got == written, out)
        a.run("hv stop %d" % va)
        check("a's guest stopped with c gone: the volume let go of all the same",
              ipct.wait_for(lambda: "detached" in a.run("nbs volume gdisk"), 120, 1), a.run("nbs volume gdisk"))
        for i in (0, 1):
            log = open(logs[i], errors="replace").read()
            check("%s: nothing panicked" % HOSTS[i], "PANIC" not in log, log[-2000:])
    except Stop as e:
        print("hv-nbs-test: stopped at: %s" % e, flush=True)
    finally:
        for p in procs:
            if p:
                pt.kill(p)
        hub.close()
        if args.keep or FAILED:
            print("hv-nbs-test: logs in %s" % tmp, flush=True)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("hv-nbs-test: %s" % ("passed" if not FAILED else "FAILED: " + "; ".join(FAILED)), flush=True)
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
