#!/usr/bin/env python3
"""NBS's metadata service between real kernels (plans/nbs.md, N5): three nos
on a LAN of scripts/hub.py's, each with ipcnet.ko, cluster.ko and
nbs-mds.ko -- the cluster registry made first, so that the hosts trust each
other, then an NBS cluster of its members.

- `nbs` on a host in no NBS cluster says so; `nbs init` on a makes one, a
  its leader and one voter; a second init refused;
- b added from a -- offered the cluster, its incarnation made there -- and c
  from b, a member that does not lead (the command handed to the leader):
  three members, three voters, one leader, as every host says; an add asked
  again answered with the same incarnation;
- a commit, through the majority: an entry's answer comes only once it is
  committed and applied, and `nbs host remove nosuch`'s refusal is one;
- the leader killed: the other two elect one of themselves and commit; the
  killed host booted again, back from its log -- three voters again, every
  host agreeing on the leader;
- the leader cut off from the other two by the hub: they elect one of
  themselves and commit, while the one cut off commits nothing; healed, it
  follows the new leader;
- a host's state lost -- its mds unloaded, its folder removed, loaded again:
  the leader says that host's mds has no member of its incarnation; an add
  of it refused, its old member being the cluster's; `nbs host replace`
  takes it back as a new member, the old incarnation removed for good;
- a member removed: two members left; its old incarnation refused when it is
  added again, since it never lost its state;
- nothing panicked.

    scripts/nbs-test.py                  arm64: HVF on an Apple Silicon Mac, TCG otherwise
    scripts/nbs-test.py --arch x86_64    KVM where the host has it, from nos-test.iso
    scripts/nbs-test.py --keep           keep the logs and images

Needs a built tree: nos-arm64.img, or for x86-64 an ISO whose command line
has the UDP shell (`make nos-test.iso TEST_ISO_ARGS=udpshell=9100`), and
out/<arch>/modules/{ipcnet,cluster,nbs-mds}.ko.
"""

import argparse
import importlib.util
import os
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
ipct = load("ipct", "ipc-test.py")

HOSTS = ["a", "b", "c"]
SHELL_PORT = ipct.SHELL_PORT
HUB_BASE = 47900
FAILED = []
MODULES = ("ipcnet", "cluster", "nbs-mds")


class Stop(Exception):
    """A step the rest of the test stands on failed."""


def check(name, ok, detail=""):
    if not pt.check(name, ok, detail):
        FAILED.append(name)
    return ok


def need(name, ok, detail=""):
    """`check`, and the test stopped there should it fail."""
    if not check(name, ok, detail):
        raise Stop(name)


def rootfs(tmp, arch, name, cluster):
    """A root for host `name`: the three modules, and hosts.toml naming this
    host alone, in a cluster of its own until it joins one."""
    rootdir = os.path.join(tmp, "root-" + name)
    os.makedirs(os.path.join(rootdir, "etc", "ipc"))
    for m in MODULES:
        ko = pt.module_path(arch, m)
        if not os.path.exists(ko):
            sys.exit("no %s -- build the tree first" % ko)
        shutil.copy(ko, os.path.join(rootdir, m + ".ko"))
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# nbs-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, cluster))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# nbs-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /nbs-mds.ko\n")
    image = os.path.join(tmp, "root-%s.img" % name)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def key_of(sh):
    m = re.search(r"\b([0-9a-f]{64})\b", sh.run("ipcnet id"))
    return m.group(1) if m else None


def token_of(out):
    m = re.search(r"cluster join (nos1-[0-9a-f]+)", out)
    return m.group(1) if m else None


def status(sh):
    """`nbs` as a dict: member, role, leader, members, voters -- None for a
    host with no member."""
    m = re.search(r"^nbs: \S+ \(\w+\) -- member (\d+), (\w+), term (\d+), leader (\S+), applied (\d+), (\d+) members, "
                  r"(\d+) voters, (\d+) learners", sh.run("nbs"), re.M)
    if not m:
        return None
    return {"member": int(m.group(1)), "role": m.group(2), "term": int(m.group(3)),
            "leader": None if m.group(4) == "none" else m.group(4), "applied": int(m.group(5)),
            "members": int(m.group(6)), "voters": int(m.group(7)), "learners": int(m.group(8))}


def members(sh):
    """`nbs hosts` as (host, incarnation, vote) each."""
    return re.findall(r"^\s+(\S+)\s+member (\d+)\s+(\w+)", sh.run("nbs hosts"), re.M)


def leader_of(sh):
    s = status(sh)
    return s["leader"] if s else None


def agreed(shells, count):
    """Every host of `shells` follows one leader, and has `count` members,
    every one a voter."""
    st = [status(s) for s in shells]
    return all(x is not None and x["leader"] is not None and x["members"] == count and x["voters"] == count
               for x in st) and len({x["leader"] for x in st}) == 1


def commits(sh):
    """Whether an entry asked of `sh` is committed and applied: a refusal is
    the metadata's answer, given only once its entry is."""
    return "no member of that host" in sh.run("nbs host remove nosuch")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--arch", choices=["x86_64", "aarch64"], default="aarch64")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="x86-64: an ISO whose command line has udpshell=%d" % SHELL_PORT)
    args = ap.parse_args()
    accel = ipct.accel_of(args.arch)
    if args.arch == "x86_64" and not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, SHELL_PORT))
    tmp = tempfile.mkdtemp(prefix="nos-nbs-")
    print("nbs-test: %s, %s, scratch in %s" % (args.arch, accel, tmp), flush=True)
    ip = hubmod.Hub.ip
    images = [rootfs(tmp, args.arch, h, os.urandom(16).hex()) for h in HOSTS]
    n = len(HOSTS)
    hub = hubmod.Hub(n, HUB_BASE, shell_port=SHELL_PORT).start()
    procs, logs = [None] * n, []

    def boot(i):
        log = os.path.join(tmp, "serial-%s-%d.log" % (HOSTS[i], len(logs)))
        logs.append(log)
        procs[i] = subprocess.Popen(ipct.qemu(args.arch, accel, images[i], log, hub, i, args.iso), cwd=ROOT,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return log

    def kill(i):
        if procs[i]:
            pt.kill(procs[i])
            procs[i] = None
        hub.acked.discard(i)

    def addressed(i):
        ok = ipct.wait_for(lambda: i in hub.acked, 60)
        time.sleep(1)
        return ok

    try:
        boot_logs = [boot(i) for i in range(n)]
        for i in range(n):
            if not check("%s boots" % HOSTS[i], pt.wait_log(boot_logs[i], "boot: complete", 300)):
                return 1
        for i in range(n):
            check("%s asks the hub for its address" % HOSTS[i], ipct.wait_for(lambda: i in hub.acked, 60))
        time.sleep(1)
        sh = [ipct.Shell(hub.shell(i)) for i in range(n)]
        a, b, c = sh

        # -- the registry: the hosts that trust each other --
        out = a.run("cluster init test")
        need("the registry made on a", "made" in out, out)
        for i, host in ((1, "b"), (2, "c")):
            out = a.run("cluster invite %s %s %s" % (host, ip(i), key_of(sh[i])))
            token = token_of(out)
            need("a invites %s" % host, token is not None, out)
            out = sh[i].run("cluster join %s" % token)
            need("%s joins the registry" % host, "joined as node" in out, out)
        everyone = [(x, y) for x in sh for y in HOSTS if y != HOSTS[sh.index(x)]]
        need("every host up to every other", ipct.wait_for(lambda: all(ipct.peer_up(x, y) for x, y in everyone), 60, 1))

        # -- an NBS cluster of a, then b and c added --
        out = a.run("nbs")
        check("a, in no NBS cluster yet, says so", "in no NBS cluster" in out, out)
        out = a.run("nbs init store")
        need("a makes the NBS cluster", "NBS cluster store made" in out, out)
        need("a leads it, its one member and voter",
             ipct.wait_for(lambda: (status(a) or {}).get("leader") == "a" and status(a)["voters"] == 1, 10, 0.5))
        out = a.run("nbs init again")
        check("a second init refused", "member of an NBS cluster already" in out, out)
        out = a.run("nbs host add b")
        m = re.search(r"b is member (\d+)", out)
        need("b added from a, offered the cluster", m is not None, out)
        b_member = int(m.group(1))
        need("b holds the log: two members, as b itself says",
             ipct.wait_for(lambda: (status(b) or {}).get("members") == 2, 30, 1))
        out = b.run("nbs host add c")
        need("c added from b, a member that does not lead", re.search(r"c is member \d+", out) is not None, out)
        need("three members, three voters, one leader, as every host says", ipct.wait_for(lambda: agreed(sh, 3), 60, 1))
        out = a.run("nbs host add b")
        check("b added again: the same incarnation", ("b is member %d" % b_member) in out, out)
        out = b.run("nbs init other")
        check("an init on a member refused", "member of an NBS cluster already" in out, out)
        check("a commit through the majority", commits(c))

        # -- the leader killed, and booted again --
        name = leader_of(a)
        need("a leader agreed on", name in HOSTS, name)
        li = HOSTS.index(name)
        rest = [i for i in range(n) if i != li]
        kill(li)
        need("the other two elect one of themselves",
             ipct.wait_for(lambda: all(leader_of(sh[i]) in [HOSTS[j] for j in rest] for i in rest)
                           and len({leader_of(sh[i]) for i in rest}) == 1, 30, 1))
        check("and commit, two of three", commits(sh[rest[0]]))
        need("%s boots again" % name, pt.wait_log(boot(li), "boot: complete", 300) and addressed(li))
        sh[li] = ipct.Shell(hub.shell(li))
        need("%s back from its log: three voters, one leader, as every host says" % name,
             ipct.wait_for(lambda: agreed(sh, 3), 90, 1))
        check("%s's member the same incarnation" % name,
              any(h == name for h, _, _ in members(sh[li])) and (status(sh[li]) or {}).get("member") in
              [int(x) for h, x, _ in members(sh[rest[0]]) if h == name])

        # -- the leader cut off from the other two --
        name = leader_of(sh[rest[0]])
        need("a leader agreed on again", name in HOSTS, name)
        li = HOSTS.index(name)
        rest = [i for i in range(n) if i != li]
        for i in rest:
            hub.cut(li, i)
        need("the two cut off from %s elect one of themselves" % name,
             ipct.wait_for(lambda: all(leader_of(sh[i]) in [HOSTS[j] for j in rest] for i in rest)
                           and len({leader_of(sh[i]) for i in rest}) == 1, 40, 1))
        check("and commit", commits(sh[rest[0]]))
        check("%s, cut off, commits nothing" % name, not commits(sh[li]))
        for i in rest:
            hub.heal(li, i)
        need("healed: every host follows one leader, three voters", ipct.wait_for(lambda: agreed(sh, 3), 60, 1))

        # -- a host's state lost: its mds unloaded, its folder removed, loaded again --
        name = leader_of(a)
        fi = next(i for i in range(n) if HOSTS[i] != name)
        li = HOSTS.index(name)
        lost = HOSTS[fi]
        old = [int(x) for h, x, _ in members(sh[li]) if h == lost]
        need("%s's incarnation known" % lost, len(old) == 1, old)
        old = old[0]
        sh[fi].run("rmmod nbs-mds")
        sh[fi].run("rm /nbs/mds")
        out = sh[fi].run("insmod /nbs-mds.ko")
        out = sh[fi].run("nbs")
        need("%s's mds, its folder gone, is in no NBS cluster" % lost, "in no NBS cluster" in out, out)
        check("the leader says %s's mds has no member %d" % (lost, old),
              ipct.wait_for(lambda: ("%s says its mds has no member %d" % (lost, old)) in sh[li].run("nbs"), 30, 1))
        out = sh[li].run("nbs host add %s" % lost)
        check("an add of %s refused, the cluster having it as its old member" % lost,
              "has it as member %d" % old in out and "replace" in out, out)
        out = sh[li].run("nbs host replace %s" % lost)
        m = re.search(r"member %d removed, and %s is member (\d+) now" % (old, lost), out)
        need("%s replaced: its old member removed, and back as a new one" % lost, m is not None, out)
        new = int(m.group(1))
        need("three voters again, %s's new member among them" % lost,
             ipct.wait_for(lambda: agreed(sh, 3) and (status(sh[fi]) or {}).get("member") == new, 60, 1))
        out = sh[li].run("nbs host add %s" % lost)
        check("%s added again: its new incarnation" % lost, ("%s is member %d" % (lost, new)) in out, out)

        # -- a member removed, and refused when added again as it was --
        name = leader_of(a)
        gi = next(i for i in range(n) if HOSTS[i] != name)
        gone = HOSTS[gi]
        out = sh[HOSTS.index(name)].run("nbs host remove %s" % gone)
        need("%s removed" % gone, "removed, for good" in out, out)
        others = [sh[i] for i in range(n) if i != gi]
        need("two members, two voters, as the others say", ipct.wait_for(lambda: agreed(others, 2), 30, 1))
        out = sh[HOSTS.index(name)].run("nbs host add %s" % gone)
        check("%s, still its old member, refused: an incarnation removed is never taken again" % gone,
              "never taken again" in out, out)
    except Stop as e:
        print("nbs-test: stopped at \"%s\": the rest stands on it" % e, flush=True)
    finally:
        for i in range(n):
            kill(i)
        hub.close()
        text = "".join(open(l, errors="replace").read() for l in logs if os.path.exists(l))
        check("nothing panicked", "PANIC" not in text)
        if args.keep or FAILED:
            print("nbs-test: logs in %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("nbs-test: %s" % ("FAILED: " + "; ".join(FAILED) if FAILED else "passed"))
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
