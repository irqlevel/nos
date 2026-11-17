#!/usr/bin/env python3
"""vm-migration-test: a running Linux guest moved between hosts by nos's
hypervisor (plans/vm-migration.md, M1) -- three x86-64 nos VMs on
tests/hub.py's network, each with ipcnet.ko, cluster.ko, nbs-mds.ko,
nbs-cs.ko and hv.ko, a registry and an NBS cluster of the three, a pool
partition of each its chunk server's device. Then:

- the refusals, each leaving the VM as it was, running: a VM that is no
  name, a host that is this one, a VM whose disk is a file it writes, a VM
  whose disk is a volume but whose name the other host has a folder of;
- a guest on a, made by `hv create` on a volume of NBS's, counting in its
  memory and with 8 MiB of its own written to its disk, moved to b by
  `hv migrate`: it runs on b and not on a, its folder on b and not on a,
  still counting from where it was, its uptime gone on, what it wrote
  there, what it writes on b landing on the volume, and the volume's writer
  b's; its kernel saying nothing of a clock that went backwards, a lockup or
  a stall;
- moved back to a, the same, the two writes read back;
- moved to b once more, stopped and started there cold from the folder
  the move gave it: booted, what it wrote over the moves still there;
- nothing panicked on any host.

A manual gate, as hv-nbs-test.py is: a guest kernel and an initrd by hand
(docs/testing.md has the recipe -- a 6.x bzImage with virtio blk and pci,
devtmpfs; Ubuntu's static BusyBox, an /init that does `dmesg -n 1`).
x86-64 nos under KVM where the host has it -- the guests nested, AMD-V on
AMD, VT-x on Intel -- else TCG, slowly.

    make nos-test.iso TEST_ISO_ARGS=udpshell=9100
    tests/vm-migration-test.py --bzimage <bzImage> --initrd <initrd.cpio.gz>

Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
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
vnt = load("vnt", "vnet-test.py")

HOSTS = nbst.HOSTS
ARCH = "x86_64"
MODULES = ("ipcnet", "cluster", "nbs-mds", "nbs-cs", "vnet", "hv")
# The cluster's network the guest is on, its address there, and a probe's
# on host c that pings it as it moves
LAN = "10.10.0.0/24"
VM_AT = "10.10.0.50"
PROBE_AT = "10.10.0.13"
SHELL_PORT = nbst.SHELL_PORT
HUB_BASE = 48300
CMDLINE = "earlyprintk=serial,ttyS0,115200 console=ttyS0 nolapic no_timer_check"
GUEST_MIB = 256
VOLUME_MIB = 64
# What the guest writes on each host, at its own offset
WRITE_MIB = 8
# The guest's counter, kept in its memory: a file of tmpfs, written whole
# each time -- `mv` so that a read never finds it half written
COUNTER = "(i=0; while true; do i=$((i+1)); echo $i > /tmp/c; mv /tmp/c /tmp/count; done) &"
# The guest's writer, kept going while it moves: a MiB of random bytes and
# their sum, made whole in a file of tmpfs and renamed over one of four --
# its memory, the page cache, written at all the time; a file read back is
# one rename's, whole, its sum its own unless a write of it was lost
WRITER = ("(i=0;while :;do i=$((i+1));f=/tmp/w$((i%4));head -c 1048576 /dev/urandom>$f.t;md5sum<$f.t|"
          "cut -c1-32>$f.s;cat $f.t $f.s>$f.n;mv $f.n $f;echo $i>/tmp/wi;done)&")
# Each file checked: copied first -- one open, one rename's whole file,
# whatever the writer renames over it meanwhile -- its MiB summed against
# the sum after it
WRITTEN = ("for f in /tmp/w0 /tmp/w1 /tmp/w2 /tmp/w3; do cp $f /tmp/chk 2>/dev/null || continue; a=$(head -c 1048576 "
           "/tmp/chk|md5sum|cut -c1-32); b=$(tail -c 33 /tmp/chk|cut -c1-32); [ \"$a\" = \"$b\" ] && echo good || "
           "echo BAD $f; done")
# What a guest's kernel says of a clock or a CPU gone wrong under it
BAD_DMESG = "went backwards|unstable|stuck|lockup|oops|BUG:|watchdog|rcu.*stall"
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


def rootfs(tmp, name, cluster, bzimage, initrd):
    """Host `name`'s root: the modules, the guest's kernel and initrd, a raw
    image for a disk of a file's, and hosts.toml naming it alone, in a
    cluster of its own until it joins one."""
    rootdir = os.path.join(tmp, "root-" + name)
    os.makedirs(os.path.join(rootdir, "etc", "ipc"))
    for m in MODULES:
        ko = pt.module_path(ARCH, m)
        if not os.path.exists(ko):
            sys.exit("no %s -- build the tree first" % ko)
        shutil.copy(ko, os.path.join(rootdir, m + ".ko"))
    shutil.copy(bzimage, os.path.join(rootdir, "bzImage"))
    shutil.copy(initrd, os.path.join(rootdir, "initrd"))
    with open(os.path.join(rootdir, "file.raw"), "wb") as f:
        f.truncate(4 << 20)
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# vm-migration-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, cluster))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# vm-migration-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /nbs-mds.ko\n"
                "insmod /nbs-cs.ko\ninsmod /vnet.ko\ninsmod /hv.ko\nhv on\n")
    image = os.path.join(tmp, "root-%s.img" % name)
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "256", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def x(vm, line, secs=60):
    """`line` run in guest `vm`'s shell -- by its name or number -- its
    output back."""
    return "hv exec %s secs=%d %s" % (vm, secs, line)


def md5_of(out):
    m = re.search(r"\b([0-9a-f]{32})\b", out or "")
    return m.group(1) if m else None


def number_in(out, pattern):
    m = re.search(pattern, out or "")
    return int(m.group(1)) if m else None


def listed(sh, name):
    """Whether VM `name` is on `hv list` of `sh`'s host."""
    return re.search(r"\b%s\b" % re.escape(name), sh.run("hv list")) is not None


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
    tmp = tempfile.mkdtemp(prefix="nos-vmmig-")
    print("vm-migration-test: %s, scratch in %s" % (accel, tmp), flush=True)
    ip = hubmod.Hub.ip
    images = [rootfs(tmp, h, os.urandom(16).hex(), args.bzimage, args.initrd) for h in HOSTS]
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
        hosts = {"a": a, "b": b, "c": c}

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
        for name in ("mdisk", "other"):
            # Asked again while the leader has not yet seen every host's
            # device take chunks: a chunk server serving its device says so
            # before the metadata does
            said = []

            def made(name=name):
                said.append(a.run("nbs volume create %s %d" % (name, VOLUME_MIB)))
                return re.search(r"volume %s \((\d+)\)" % name, said[-1]) is not None

            need("volume %s made" % name, ipct.wait_for(made, 60, 2), said[-1] if said else "")

        # -- vnet of the three, the network lan, a probe on c --
        need("vnet made on a", "vnet: mig made" in a.run("vnet init mig"))
        for h in ("b", "c"):
            out = a.run("vnet host add %s" % h)
            need("%s added to vnet" % h, re.search(r"%s is member \d+" % h, out) is not None, out)
        need("vnet: three members, a leader", ipct.wait_for(lambda: vnt.vnet_agreed(sh, 3), 90, 1))
        need("lan made", "network lan" in a.run("vnet create lan %s mtu=1400" % LAN))
        out = a.run("vnet fw default add allow in from net")
        need("the cluster's default lets its network in", "allow in from net" in out, out)
        out = c.run("hv probe add pc network=vnet:lan address=%s" % PROBE_AT)
        need("a probe on c on lan", "%s (vnet:lan)" % PROBE_AT in out, out)

        def create(s, name, disk, more=""):
            return s.run("hv create %s /bzImage mem=%d initrd=/initrd disk=%s%s cmdline=%s" % (
                name, GUEST_MIB, disk, more, CMDLINE))

        def probe_answers(count=1, secs=2):
            run = vnt.probe_ping(c, "pc", VM_AT, count, secs)
            return run is not None and run[1] == count

        def flows_on(s, name):
            m = re.search(r"\b%s\b.*firewall: (\d+) flows" % re.escape(name), s.run("hv list"))
            return int(m.group(1)) if m else None

        def shell_up(s, vm):
            out = s.run(x(vm, "id", args.vm_secs))
            return "uid=0" in out and s.run(x(vm, "mount -t devtmpfs devtmpfs /dev")) is not None

        def volume_sum(s, vm, skip_mib, count_mib):
            out = s.run(x(vm, "echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=1M skip=%d count=%d 2>/dev/null "
                              "| md5sum" % (skip_mib, count_mib), 300))
            return md5_of(out), out

        def write(s, vm, seek_mib):
            """WRITE_MIB of random bytes written at `seek_mib` of its disk and
            synced: their sum, read back past the page cache."""
            out = s.run(x(vm, "dd if=/dev/urandom of=/dev/vda bs=1M seek=%d count=%d conv=fsync 2>&1; echo rc=$?" % (
                seek_mib, WRITE_MIB), 300))
            check("%d MiB written at %d MiB" % (WRITE_MIB, seek_mib), "rc=0" in out, out)
            got, _ = volume_sum(s, vm, seek_mib, WRITE_MIB)
            return got

        def reading(s, vm):
            """Its uptime and its count, as `cat /proc/uptime /tmp/count`
            says them: (None, None) where it says neither."""
            out = s.run(x(vm, "cat /proc/uptime /tmp/count"))
            m = re.search(r"(?m)^(\d+\.\d+) \d+\.\d+\s*\n\s*(\d+)\s*$", out or "")
            return (float(m.group(1)), int(m.group(2))) if m else (None, None)

        def bad_lines(s, vm):
            """What its kernel said of a clock or a CPU gone wrong, counted:
            None where the count did not come back."""
            got = re.findall(r"(?m)^(\d+)\s*$", s.run(x(vm, "dmesg | grep -ciE '%s'" % BAD_DMESG)) or "")
            return int(got[-1]) if got else None

        def migrate(s, vm, to):
            out = s.run("hv migrate %s %s" % (vm, to))
            print("vm-migration-test: hv migrate %s %s answered:\n%s" % (vm, to, out.rstrip()), flush=True)
            return out

        # -- the refusals --
        out = a.run("hv migrate nosuch b")
        check("a VM that is not there: refused", "no vm" in out, out)
        out = create(a, "filed", "/file.raw")
        need("a VM of a file's disk made", "written" in out, out)
        need("and started", "started" in a.run("hv start filed"))
        out = migrate(a, "filed", "b")
        check("its move refused: its disk is a file it writes", "not moved" in out and "file" in out, out)
        check("and it runs on", listed(a, "filed"), a.run("hv list"))
        out = migrate(a, "filed", "a")
        check("a move to this host refused", "is this host" in out, out)
        a.run("hv stop filed secs=0")

        # -- a guest on a volume, counting --
        out = create(a, "mig", "nbs:mdisk", " net network=vnet:lan address=%s" % VM_AT)
        need("a VM on nbs:mdisk and vnet:lan made on a", "written" in out, out)
        out = a.run("hv start mig")
        need("and started", "started" in out, out)
        need("its shell up", shell_up(a, "mig"))
        # Its address, as hv's DHCP would give it -- a kernel that asked at
        # boot has it already
        out = a.run(x("mig", "ip link set eth0 up && ip addr replace %s/24 dev eth0 && ip link set eth0 mtu 1400 && "
                             "ip route replace default via 10.10.0.1; ip -4 addr show eth0" % VM_AT))
        need("its address on lan", "inet %s/24" % VM_AT in out, out)
        out = a.run(x("mig", "ping -c 3 -W 5 %s; echo pinged" % PROBE_AT, 120))
        check("it reaches the probe on c, across the tunnel", re.search(r"3 packets received", out) is not None, out)
        check("the probe on c reaches it", ipct.wait_for(lambda: probe_answers(3, 3), 60, 2))
        a.run(x("mig", "dmesg -c > /dev/null; " + COUNTER))
        a.run(x("mig", WRITER))
        first = write(a, "mig", 0)
        need("what it wrote read back", first is not None)
        _, c1 = reading(a, "mig")
        need("it counts", c1 is not None and c1 > 0, str(c1))

        # -- the dirty log held to the guest where it runs, as it writes --
        out = a.run("hv logcheck mig rounds=2")
        print("vm-migration-test: hv logcheck mig answered:\n" + out.rstrip(), flush=True)
        check("hv logcheck: every page copied as it ran the same as its memory once paused",
              "dirty log held" in out, out)
        _, still = reading(a, "mig")
        check("and it runs on after the check", still is not None and still > c1, "%s then %s" % (c1, still))

        # -- the name the other host has a folder of: refused there --
        create(b, "mig", "nbs:other")
        out = migrate(a, "mig", "b")
        check("a move to a host with a folder of its name refused", "refused it" in out and "here already" in out, out)
        check("and it runs on, on a", listed(a, "mig"))
        b.run("hv delete mig")

        def moved(src, dst, label, cold=False):
            """`mig` moved from `src` to `dst` -- live, or `cold`: the checks of
            the move, its counter and uptime then, and its writer's files."""
            s, d = hosts[src], hosts[dst]
            up_before, before = reading(s, "mig")
            # Its connections: its pings of the probe's open flows on its port
            s.run(x("mig", "ping -c 2 -W 3 %s" % PROBE_AT, 60))
            # The probe on c pinging it all the while, a ping each 0.3 s:
            # c's shell is the pinger's alone until the move is done
            pings, stop = [], threading.Event()

            def pinger():
                while not stop.is_set():
                    run = vnt.probe_ping(c, "pc", VM_AT, 1, 1)
                    pings.append((time.time(), run is not None and run[1] == 1))
                    time.sleep(0.3)

            t = threading.Thread(target=pinger)
            t.start()
            out = migrate(s, "mig", dst + (" cold" if cold else ""))
            time.sleep(3)
            stop.set()
            t.join()
            lost = sum(1 for _, ok in pings if not ok)
            print("vm-migration-test: %s: %d of %d pings from the probe answered across the move" %
                  (label, len(pings) - lost, len(pings)), flush=True)
            check("%s: the probe's pings answered again once it runs there" % label,
                  len(pings) >= 3 and all(ok for _, ok in pings[-3:]), pings[-6:])
            check("%s: it reaches the probe from there" % label,
                  "3 packets received" in d.run(x("mig", "ping -c 3 -W 5 %s" % PROBE_AT, 120)))
            carried = number_in(out, r"(\d+) flows of its firewall")
            check("%s: its firewall's flows went with it" % label, carried is not None and carried > 0, out)
            dm = d.run("dmesg 4000 hv:")
            check("%s: its port moved there, on lan" % label, "without its network" not in dm, dm[-1500:])
            check("%s: and every flow had there" % label, "flows had here" not in dm, dm[-1500:])
            now_there = flows_on(d, "mig")
            check("%s: on its port there" % label, now_there is not None, d.run("hv list"))
            print("vm-migration-test: %s: %s flows carried, %s on its port there now" % (label, carried, now_there),
                  flush=True)
            relayed = re.search(r"(\d+) frames for ports moved to other hosts carried on", s.run("hv list"))
            print("vm-migration-test: %s: %s frames carried on by %s for the moved port" %
                  (label, relayed.group(1) if relayed else 0, src), flush=True)
            need("%s: hv migrate says it runs on %s" % (label, dst), "runs on %s" % dst in out, out)
            rounds = number_in(out, r"sent in (\d+) rounds?")
            if cold:
                check("%s: cold, one round, paused for it" % label, rounds == 1, out)
            else:
                check("%s: live, a round as it ran and the last paused" % label, rounds is not None and rounds >= 2,
                      out)
            paused = number_in(out, r"paused (\d+) ms;")
            print("vm-migration-test: %s paused %s ms over %s rounds" % (label, paused, rounds), flush=True)
            got = d.run(x("mig", WRITTEN))
            # Its own lines, not the command's echo, which says BAD too
            good = len(re.findall(r"(?m)^\s*good\s*$", got))
            bad = re.findall(r"(?m)^\s*BAD (/tmp/w\d)", got)
            check("%s: every file its writer made whole there, its sum its own" % label, good == 4 and not bad,
                  "%d good, bad: %s\n%s" % (good, bad, got))
            first = number_in(d.run(x("mig", "cat /tmp/wi")), r"(?m)^\s*(\d+)\s*$")
            time.sleep(3)
            then = number_in(d.run(x("mig", "cat /tmp/wi")), r"(?m)^\s*(\d+)\s*$")
            check("%s: and its writer writing on" % label, first is not None and then is not None and then > first,
                  "%s then %s" % (first, then))
            check("%s: not on %s's list" % (label, src), not listed(s, "mig"), s.run("hv list"))
            check("%s: its folder gone from %s" % (label, src), "mig" not in s.run("hv vms"))
            check("%s: on %s's list" % (label, dst), listed(d, "mig"), d.run("hv list"))
            check("%s: its folder on %s" % (label, dst), "mig" in d.run("hv vms"))
            up, after = reading(d, "mig")
            check("%s: its counter where it was, gone on" % label, after is not None and before is not None
                  and after >= before, "%s then %s" % (before, after))
            check("%s: its uptime gone on" % label, up is not None and up_before is not None and up >= up_before,
                  "%s then %s" % (up_before, up))
            time.sleep(2)
            _, later = reading(d, "mig")
            check("%s: and counting on %s" % (label, dst), later is not None and after is not None and later > after,
                  "%s then %s" % (after, later))
            check("%s: its kernel says nothing of a clock or a CPU gone wrong" % label, bad_lines(d, "mig") == 0,
                  d.run(x("mig", "dmesg | tail -20")))
            out = d.run("nbs volume mdisk")
            check("%s: the volume's writer %s" % (label, dst),
                  re.search(r"attached on %s \(generation \d+\)" % dst, out) is not None, out)

        moved("a", "b", "a to b")
        got, out = volume_sum(b, "mig", 0, WRITE_MIB)
        check("what it wrote on a there", got == first, out)
        second = write(b, "mig", WRITE_MIB)
        check("what it writes on b read back", second is not None)

        moved("b", "a", "b to a", cold=True)
        got, out = volume_sum(a, "mig", 0, WRITE_MIB)
        check("back on a: what it wrote on a there", got == first, out)
        got, out = volume_sum(a, "mig", WRITE_MIB, WRITE_MIB)
        check("back on a: what it wrote on b there", got == second, out)

        moved("a", "b", "a to b again")
        out = b.run("hv stop mig secs=0")
        check("stopped on b", not listed(b, "mig"), out)
        out = b.run("hv start mig")
        check("started on b cold, from the folder the move gave it", "started" in out, out)
        if check("its shell up", shell_up(b, "mig")):
            got, out = volume_sum(b, "mig", 0, WRITE_MIB)
            check("booted on b: what it wrote on a there", got == first, out)
            got, out = volume_sum(b, "mig", WRITE_MIB, WRITE_MIB)
            check("booted on b: what it wrote on b there", got == second, out)
        b.run("hv stop mig secs=0")

        for i in range(n):
            log = open(logs[i], errors="replace").read()
            check("%s: nothing panicked" % HOSTS[i], "PANIC" not in log, log[-2000:])
    except Stop as e:
        print("vm-migration-test: stopped at: %s" % e, flush=True)
    finally:
        for p in procs:
            if p:
                pt.kill(p)
        hub.close()
        if args.keep or FAILED:
            print("vm-migration-test: logs in %s" % tmp, flush=True)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("vm-migration-test: %s" % ("passed" if not FAILED else "FAILED: " + "; ".join(FAILED)), flush=True)
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
