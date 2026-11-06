#!/usr/bin/env python3
"""The cluster's registry between real kernels (plans/cluster.md, C1): three
nos on a LAN of scripts/hub.py's, each with ipcnet.ko and cluster.ko -- a
cluster made on one, the others invited and joined, and every host's ipcnet
pinning every other's key from the registry alone.

- `cluster init` on a: a cluster of a alone, a its leader;
- b invited from a with the key `ipcnet id` says on b, and joined with the
  token: a voter, and a and b up to each other;
- c invited from b, a member that does not lead (the command handed to the
  leader), a join with a secret not the token's refused; c, invited, b's
  peer and reaching b's member alone -- its pings to b's echo refused -- then
  joined with the token: a, b and c all up to one another, three voters, and c's pings to
  a's echo answered over the connection the registry made;
- the refusals a host makes itself: a second init, another host's token, a
  token used again;
- c removed: dropped by a and b, its connections gone within seconds;
- b's VM restarted: back from its member file, its log and its hosts.toml,
  following the leader again;
- the leader of the two left -- whichever was elected after b's restart --
  removed: the lead handed to the other first, which leads alone after, and
  the removed host's key refused even invited again;
- nothing panicked.

    scripts/cluster-test.py                  arm64: HVF on an Apple Silicon Mac, TCG otherwise
    scripts/cluster-test.py --arch x86_64    KVM where the host has it, from nos-test.iso
    scripts/cluster-test.py --keep           keep the logs and images

Needs a built tree: nos-arm64.img, or for x86-64 an ISO whose command line
has the UDP shell (`make nos-test.iso TEST_ISO_ARGS=udpshell=9100`), and
out/<arch>/modules/{ipcnet,cluster}.ko.
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
HUB_BASE = 47800
FAILED = []


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
    """A root for host `name`: both modules, and hosts.toml naming this host
    alone, in a cluster of its own until it joins one."""
    rootdir = os.path.join(tmp, "root-" + name)
    os.makedirs(os.path.join(rootdir, "etc", "ipc"))
    for m in ("ipcnet", "cluster"):
        ko = pt.module_path(arch, m)
        if not os.path.exists(ko):
            sys.exit("no %s -- build the tree first" % ko)
        shutil.copy(ko, os.path.join(rootdir, m + ".ko"))
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# cluster-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, cluster))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# cluster-test.py\ninsmod /ipcnet.ko\nipcnet start\nipc echo start\ninsmod /cluster.ko\n")
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


def hosts_of(sh):
    """`cluster hosts` as (name, node, vote) each."""
    return re.findall(r"^\s+(\S+)\s+node (\d+)\s+(\w+)", sh.run("cluster hosts"), re.M)


def voters(sh):
    return sorted(h[0] for h in hosts_of(sh) if h[2] == "voter")


def leader_of(sh):
    """The leader a host's member follows, by name -- None while it has none."""
    m = re.search(r"^cluster: \S+ -- node \d+, \w+, term \d+, leader (\S+),", sh.run("cluster"), re.M)
    return m.group(1) if m and m.group(1) != "none" else None


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
    tmp = tempfile.mkdtemp(prefix="nos-cluster-")
    print("cluster-test: %s, %s, scratch in %s" % (args.arch, accel, tmp), flush=True)
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

    try:
        boot_logs = [boot(i) for i in range(n)]
        for i in range(n):
            if not check("%s boots" % HOSTS[i], pt.wait_log(boot_logs[i], "boot: complete", 300)):
                return 1
        for i in range(n):
            check("%s asks the hub for its address" % HOSTS[i], ipct.wait_for(lambda: i in hub.acked, 60))
        time.sleep(1)
        a, b, c = [ipct.Shell(hub.shell(i)) for i in range(n)]
        for i, sh in enumerate((a, b, c)):
            out = sh.run("ipcnet")
            check("%s serves, started by /etc/rc, with no peers yet" % HOSTS[i],
                  "-- serving" in out and not re.search(r"^  \S", out, re.M), out)

        # -- a cluster of a alone --
        out = a.run("cluster init test")
        need("a makes the cluster", "made" in out, out)
        need("a leads it, its one voter", ipct.wait_for(lambda: voters(a) == ["a"], 10, 0.5))
        out = a.run("cluster init again")
        check("a second init refused", "a member of a cluster already" in out, out)

        # -- b invited from a, with the key b's ipcnet has, and joined --
        kb = key_of(b)
        out = a.run("cluster invite b %s %s" % (ip(1), kb))
        tb = token_of(out)
        need("a invites b and prints a token", tb is not None, out)
        out = c.run("cluster join %s" % tb)
        check("c refuses b's token, the invitation not its", "the invitation is for b" in out, out)
        out = b.run("cluster join %s" % tb)
        need("b joins with it", "joined as node 2" in out, out)
        need("b is a voter, as a and b both say",
             ipct.wait_for(lambda: voters(a) == ["a", "b"] and voters(b) == ["a", "b"], 30, 1))
        need("a and b are up to each other",
             ipct.wait_for(lambda: ipct.peer_up(a, "b") and ipct.peer_up(b, "a"), 30, 1))
        out = b.run("cluster join %s" % tb)
        check("b's token used again refused: b is a member", "a member of a cluster already" in out, out)

        # -- c invited from b, which hands it to the leader; a wrong secret refused --
        kc = key_of(c)
        out = b.run("cluster invite c %s %s" % (ip(2), kc))
        tc = token_of(out)
        need("b invites c, a member that does not lead", tc is not None, out)
        # The token's bytes in hex after "nos1-": the cluster (16), the
        # member's address (4) and port (2), its key (32), the invitation
        # (8), then the secret -- its first byte changed
        at = len("nos1-") + 2 * (16 + 4 + 2 + 32 + 8)
        wrong = tc[:at] + ("00" if tc[at:at + 2] != "00" else "11") + tc[at + 2:]
        out = c.run("cluster join %s" % wrong)
        check("a join with a secret not the invitation's refused by b", "b refused the join" in out, out)
        # c, invited and not joined, is b's peer -- pinned by the key the
        # invitation named -- and reaches b's member alone, by its name
        need("c and b up to each other, c invited", ipct.wait_for(lambda: ipct.peer_up(c, "b"), 30, 1))
        line = ipct.ipcnet_line(b, "c")
        check("b says c is a host invited, which reaches cluster.raft alone", "(invited: cluster.raft alone)" in line,
              line)
        before = ipct.counter(line, "refused")
        r = ipct.ping(c, "kernel.echo@b", "count=3 timeout=1000")
        line = ipct.ipcnet_line(b, "c")
        check("c's pings to b's echo never answered, each refused by b",
              r is not None and r["answered"] == 0 and ipct.counter(line, "refused") >= before + 3, (r, line))
        out = c.run("cluster join %s" % tc)
        need("c joins with the token, after a join refused", "joined as node 3" in out, out)
        need("three voters, as every host says",
             ipct.wait_for(lambda: all(voters(s) == ["a", "b", "c"] for s in (a, b, c)), 40, 1))
        need("every host up to every other",
             ipct.wait_for(lambda: all(ipct.peer_up(s, o) for s, o in
                                       ((a, "b"), (a, "c"), (b, "a"), (b, "c"), (c, "a"), (c, "b"))), 40, 1))
        r = ipct.ping(c, "kernel.echo@a", "count=50 bytes=1000")
        check("c's pings to a's echo, over the connection the registry made, all answered",
              r is not None and r["answered"] == 50 and r["wrong"] == 0, r)
        out = a.run("cluster")
        check("a's member says three hosts, three voters", "3 hosts, 3 voters" in out, out)

        # -- c removed: dropped by a and b --
        out = a.run("cluster remove c")
        need("a removes c", "removed" in out, out)
        check("a and b take c off their peers within seconds",
              ipct.wait_for(lambda: "c" not in [h[0] for h in hosts_of(b)]
                            and not re.search(r"^\s+c\s", a.run("ipcnet"), re.M)
                            and not re.search(r"^\s+c\s", b.run("ipcnet"), re.M), 20, 1))
        check("two voters left", ipct.wait_for(lambda: voters(a) == ["a", "b"], 20, 1))

        # -- b restarted: from its member file, its log and its hosts.toml --
        pt.kill(procs[1])
        log = boot(1)
        need("b boots again", pt.wait_log(log, "boot: complete", 300))
        b = ipct.Shell(hub.shell(1))
        need("and a and b are up to each other", ipct.wait_for(lambda: ipct.peer_up(a, "b"), 30, 1))
        # a stepped down while b was away -- it heard from no majority of
        # the two -- and one of them is elected now that both vote: either
        need("a and b agree on a leader, one of them, b's registry the cluster's",
             ipct.wait_for(lambda: leader_of(a) in ("a", "b") and leader_of(b) == leader_of(a)
                           and voters(b) == ["a", "b"], 30, 1))

        # -- the leader removed: the lead handed on first, in a group of two --
        name = leader_of(a)
        lead, other = (a, b) if name == "a" else (b, a)
        rest = "b" if name == "a" else "a"
        out = lead.run("cluster remove %s" % name)
        need("%s, the leader of two, removed" % name, "removed" in out, out)
        log = lead.run("dmesg 2000 cluster:")
        check("%s handed the lead to %s before its own removal, as its log says" % (name, rest),
              re.search(r"asked to remove this host, the leader: the lead handed to node %d first"
                        % (HOSTS.index(rest) + 1), log) is not None, log)
        check("%s leads alone, one host and one voter" % rest,
              ipct.wait_for(lambda: re.search(r"leader %s, applied \d+, 1 hosts, 1 voters" % rest, other.run("cluster")),
                            20, 1))
        check("%s drops %s" % (rest, name),
              ipct.wait_for(lambda: not re.search(r"^\s+%s\s" % name, other.run("ipcnet"), re.M), 20, 1))
        out = other.run("cluster invite %s %s %s" % (name, ip(HOSTS.index(name)), key_of(lead)))
        check("%s's key refused for good, even invited again" % name, "removed" in out and "nos1-" not in out, out)
    except Stop as e:
        print("cluster-test: stopped at \"%s\": the rest stands on it" % e, flush=True)
    finally:
        for p in procs:
            if p:
                pt.kill(p)
        hub.close()
        text = "".join(open(l, errors="replace").read() for l in logs if os.path.exists(l))
        check("nothing panicked", "PANIC" not in text)
        if args.keep or FAILED:
            print("cluster-test: logs in %s" % tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("cluster-test: %s" % ("FAILED: " + "; ".join(FAILED) if FAILED else "passed"))
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
