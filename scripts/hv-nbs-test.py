#!/usr/bin/env python3
"""hv-nbs-test: Linux guests of nos's hypervisor on volumes of NBS's
(plans/nbs.md, N8) -- three x86-64 nos VMs on scripts/hub.py's network, each
with ipcnet.ko, cluster.ko, nbs-mds.ko, nbs-cs.ko and hv.ko, a registry and an
NBS cluster of the three, a pool partition of each its chunk server's
device. Then:

- an image poured onto a volume on a (`hv convert <raw> nbs:<volume>`) and
  read by a guest on b: what its /dev/vda holds is what was poured;
- a disk hvboot boots -- ext4, a grub.cfg naming the guest's kernel and
  initrd -- made an image of NBS's by `hv image` (N10d3), and two VMs made
  of it by `hv create <vm> image=nbs:<image>`, one on a and one on b, each
  booted by its name, by hvboot, off a clone of its own: a's grown to
  size=, zeros past the image, and written over a chunk the image shares --
  b's, started after, still the image's; the refusals: the image's name
  again, a VM of a plain volume, a size= under the image's, a clone's name
  taken -- each leaving nothing;
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
# N10d3: the disk hvboot boots, made an image; a's clone of it grown to
# CLONE_MIB, and written at COPY_AT MiB -- inside the image, so over a
# chunk the image shares -- with the image's first MiB
BOOT_MIB = 32
CLONE_MIB = 64
COPY_AT = 29
MIB = 1 << 20
HVBOOT = os.path.join(ROOT, "out", "x86_64", "hvboot")
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


def boot_disk(tmp, bzimage, initrd):
    """A disk hvboot boots (N10d3): ext4 on the whole of it, /boot/grub/grub.cfg
    naming /boot/bzImage and /boot/initrd -- found by hvboot on the
    filesystem the configuration is on."""
    content = os.path.join(tmp, "bootdisk")
    os.makedirs(os.path.join(content, "boot", "grub"))
    shutil.copy(bzimage, os.path.join(content, "boot", "bzImage"))
    shutil.copy(initrd, os.path.join(content, "boot", "initrd"))
    with open(os.path.join(content, "boot", "grub", "grub.cfg"), "w") as f:
        f.write("set default=0\nset timeout=0\nmenuentry 'hv-nbs-test' {\n\tlinux /boot/bzImage %s\n"
                "\tinitrd /boot/initrd\n}\n" % CMDLINE)
    disk = os.path.join(tmp, "boot.raw")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), disk, str(BOOT_MIB), content], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL, env=dict(os.environ, LABEL="hvnbsboot"))
    return disk


def rootfs(tmp, name, cluster, bzimage, initrd, raw, boot):
    """Host `name`'s root: the modules, the guest's kernel and initrd, the
    image to pour, hvboot and the disk it boots, and hosts.toml naming it
    alone, in a cluster of its own until it joins one."""
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
    shutil.copy(boot, os.path.join(rootdir, "boot.raw"))
    os.makedirs(os.path.join(rootdir, "lib"))
    shutil.copy(HVBOOT, os.path.join(rootdir, "lib", "hvboot"))
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# hv-nbs-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, cluster))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# hv-nbs-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /nbs-mds.ko\n"
                "insmod /nbs-cs.ko\ninsmod /hv.ko\nhv on\n")
    image = os.path.join(tmp, "root-%s.img" % name)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "256", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def x(vm, line, secs=60):
    """`line` run in guest `vm`'s shell, its output back."""
    return "hv exec %d secs=%d %s" % (vm, secs, line)


def started(out):
    """The number of the VM `hv start` started, or None."""
    m = re.search(r"hv: vm (\d+)(?: \([^)]*\))? started", out)
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
    ap.add_argument("--mem", action="store_true",
                    help="say a's and b's memusage at each step, and a's every 10 s while c is killed")
    args = ap.parse_args()
    accel = ipct.accel_of(ARCH)
    if not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, SHELL_PORT))
    if not os.path.exists(HVBOOT):
        sys.exit("no %s -- build the tree first" % HVBOOT)
    tmp = tempfile.mkdtemp(prefix="nos-hvnbs-")
    print("hv-nbs-test: %s, scratch in %s" % (accel, tmp), flush=True)
    raw = os.path.join(tmp, "img.raw")
    with open(raw, "wb") as f:
        f.write(os.urandom(WRITE_MIB << 20))
    poured_sum = hashlib.md5(open(raw, "rb").read()).hexdigest()
    boot = boot_disk(tmp, args.bzimage, args.initrd)
    boot_bytes = open(boot, "rb").read()
    boot_sum = hashlib.md5(boot_bytes).hexdigest()
    # a's clone once its guest copied the image's first MiB over COPY_AT
    copied_sum = hashlib.md5(boot_bytes[:COPY_AT * MIB] + boot_bytes[:MIB]
                             + boot_bytes[(COPY_AT + 1) * MIB:]).hexdigest()
    zeros_sum = hashlib.md5(bytes((CLONE_MIB - BOOT_MIB) * MIB)).hexdigest()
    ip = hubmod.Hub.ip
    images = [rootfs(tmp, h, os.urandom(16).hex(), args.bzimage, args.initrd, raw, boot) for h in HOSTS]
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

        def mem(when):
            """With --mem: a's and b's free pages and Rust heap, said."""
            if not args.mem:
                return
            for s_, h in ((a, "a"), (b, "b")):
                got = s_.run("memusage")
                said = " ".join(ln.strip() for ln in got.splitlines()
                                if ln.strip().startswith(("free_pages", "rust_heap:", "rust_heap_peak")))
                print("hv-nbs-test: memory of %s %s: %s" % (h, when, said), flush=True)

        def shell_up(s, vm):
            out = s.run(x(vm, "id", args.vm_secs))
            return "uid=0" in out and s.run(x(vm, "mount -t devtmpfs devtmpfs /dev")) is not None

        def volume_sum(s, vm, skip_mib, count_mib):
            out = s.run(x(vm, "echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=1M skip=%d count=%d 2>/dev/null "
                              "| md5sum" % (skip_mib, count_mib), 300))
            return md5_of(out), out

        mem("with the cluster made")
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

        # -- N10d3: an image of NBS's made by hv, and VMs of it, each a clone of its own --
        out = a.run("hv image img /boot.raw")
        print("hv-nbs-test: hv image img /boot.raw answered:\n" + out.rstrip(), flush=True)
        check("hv image: a disk hvboot boots made image nbs:img", "nbs:img is an image now" in out, out)
        out = a.run("nbs volume img")
        check("nbs:img an image, of the disk's size", "an image" in out and ("%d MiB" % BOOT_MIB) in out, out)
        out = a.run("hv image img /boot.raw")
        check("hv image of the name again refused", "nbs:img is there already" in out, out)
        out = a.run("hv create v3 image=nbs:gdisk")
        check("a VM of a plain volume refused", "nbs:gdisk is a volume" in out, out)
        out = a.run("hv create v4 image=nbs:img size=1M")
        check("a size= under the image's refused", "is less than the image's disk" in out, out)
        out = a.run("hv create v1 image=nbs:img size=%dM mem=%d" % (CLONE_MIB, GUEST_MIB))
        print("hv-nbs-test: hv create v1 answered:\n" + out.rstrip(), flush=True)
        check("hv create v1 image=nbs:img: made, a clone its disk",
              "nbs:v1 cloned from nbs:img" in out and "/vms/v1/vm.toml written" in out, out)
        out = a.run("nbs volume v1")
        check("nbs:v1 a clone of img, grown to size=", "a clone of img" in out and ("%d MiB" % CLONE_MIB) in out, out)
        out = a.run("hv create v5 image=nbs:img onto=nbs:v1")
        check("a clone's name NBS has refused", "nbs:v1 is there already" in out, out)
        out = a.run("hv vms") + a.run("nbs volumes")
        check("the refused left nothing: no folder, no volume",
              not any(n in out for n in ("v3", "v4", "v5")) and "v1" in out, out)
        out = b.run("hv create v2 image=nbs:img mem=%d" % GUEST_MIB)
        check("hv create v2 image=nbs:img on b: made", "/vms/v2/vm.toml written" in out, out)

        out = a.run("hv start v1")
        v1 = started(out)
        need("v1 started by its name", v1 is not None, out)
        need("its shell up: hvboot booted the kernel the image's grub.cfg names, off the clone", shell_up(a, v1))
        out = a.run(x(v1, "cat /sys/block/vda/size"))
        check("its disk grown to size=", ("%d" % (CLONE_MIB << 11)) in out, out)
        got, out = volume_sum(a, v1, 0, BOOT_MIB)
        check("its disk holds the image", got == boot_sum, out)
        got, out = volume_sum(a, v1, BOOT_MIB, CLONE_MIB - BOOT_MIB)
        check("and zeros past it", got == zeros_sum, out)
        out = a.run(x(v1, "dd if=/dev/vda of=/dev/vda bs=1M count=1 seek=%d conv=fsync 2>/dev/null; echo nos-rc=$?"
                          % COPY_AT, 300))
        check("its first MiB copied over a chunk the image shares, synced", "nos-rc=0" in out, out)
        got, out = volume_sum(a, v1, 0, BOOT_MIB)
        check("read back past the page cache: the write, and the rest of the chunk the image's", got == copied_sum,
              out)

        out = b.run("hv start v2")
        v2 = started(out)
        need("v2 started by its name on b", v2 is not None, out)
        need("its shell up, off a clone of its own", shell_up(b, v2))
        out = b.run(x(v2, "cat /sys/block/vda/size"))
        check("its disk the image's size", ("%d" % (BOOT_MIB << 11)) in out, out)
        got, out = volume_sum(b, v2, 0, BOOT_MIB)
        check("v1's write is v1's alone: v2's clone holds the image", got == boot_sum, out)
        b.run("hv stop %d" % v2)
        a.run("hv stop %d" % v1)
        check("both stopped, their clones let go of",
              ipct.wait_for(lambda: all("detached" in s_.run("nbs volume %s" % v) for s_, v in ((a, "v1"), (b, "v2"))),
                            120, 1), a.run("nbs volumes"))
        mem("with v1 and v2 stopped")

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
        mem("with a's guest off gdisk stopped")

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
        mem("before c is killed, a's guest writing")
        time.sleep(3)
        pt.kill(procs[2])
        procs[2] = None
        if args.mem:
            for _ in range(30):
                if "nos-long-rc=" in a.run("hv wait %d secs=10 nos-long-rc=" % (va,)):
                    break
                got = a.run("memusage")
                said = " ".join(ln.strip() for ln in got.splitlines()
                                if ln.strip().startswith(("free_pages:", "rust_heap:")))
                print("hv-nbs-test: memory of a, c killed: %s" % said, flush=True)
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
