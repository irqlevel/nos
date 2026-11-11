#!/usr/bin/env python3
"""The cluster's registry between real kernels (plans/cluster.md, C1 and
C2): three nos on a LAN of scripts/hub.py's, each with ipcnet.ko and
cluster.ko -- a cluster made on one, the others invited and joined, and
every host's ipcnet pinning every other's key from the registry alone.

- `cluster init` on a: a cluster of a alone, a its leader;
- b invited from a with the key `ipcnet id` says on b, and joined with the
  token: a voter, and a and b up to each other;
- c invited from b, a member that does not lead (the command handed to the
  leader), a join with a secret not the token's refused; c, invited, b's
  peer and reaching b's member alone -- its pings to b's echo refused -- then
  joined with the token: a, b and c all up to one another, three voters, and
  c's pings to a's echo answered over the connection the registry made;
- the refusals a host makes itself: a second init, another host's token, a
  token used again;
- the provider: `mgmt call cluster hosts` answering as `cluster hosts` does;
- b's maintenance and c's public address, set and taken away, as every host
  says them;
- c moved to a new key while b pings c's echo across it: every ping
  answered, no connection lost on a or b; c's new key the registry's, and
  c's connections made again with it alone; the old key refused when
  invited again;
- all three VMs killed and booted again at once: back from their folders,
  a leader agreed on, three voters, every host up to every other;
- recover: refused while the majority answers; b and c killed for good, and
  on a -- once it follows no leader -- what it does said, a name not the
  cluster's refused, then a made the one voter and b and c removed; c's old
  key refused, and c installed anew and joined with a new key;
- the leader of the two left removed: the lead handed to the other first,
  which leads alone after, and the removed host's key refused when invited
  again;
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
import json
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
# An address the hub gives nobody: c's public one, as the registry has it
PUBLIC = "198.51.100.7"


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


def rootfs(tmp, arch, name, cluster, tag=""):
    """A root for host `name`: both modules, and hosts.toml naming this host
    alone, in a cluster of its own until it joins one -- `tag` telling one
    install of a host from another."""
    rootdir = os.path.join(tmp, "root-" + name + tag)
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
    image = os.path.join(tmp, "root-%s%s.img" % (name, tag))
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


def host_line(sh, name):
    """A host's line of `cluster hosts`."""
    m = re.search(r"^\s+%s\s+node .*$" % re.escape(name), sh.run("cluster hosts"), re.M)
    return m.group(0) if m else ""


def voters(sh):
    return sorted(h[0] for h in hosts_of(sh) if h[2] == "voter")


def leader_of(sh):
    """The leader a host's member follows, by name -- None while it has none."""
    m = re.search(r"^cluster: \S+ -- node \d+, \w+, term \d+, leader (\S+),", sh.run("cluster"), re.M)
    return m.group(1) if m and m.group(1) != "none" else None


def all_up(pairs):
    return all(ipct.peer_up(s, o) for s, o in pairs)


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

    def kill(i):
        if procs[i]:
            pt.kill(procs[i])
            procs[i] = None
        # Its address asked again as it boots: the shell's datagrams before
        # it has one are lost, and the shell's client does not send again
        hub.acked.discard(i)

    def addressed(i):
        """VM i booted again and its address given: its shell answers."""
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
        everyone = ((a, "b"), (a, "c"), (b, "a"), (b, "c"), (c, "a"), (c, "b"))
        need("every host up to every other", ipct.wait_for(lambda: all_up(everyone), 40, 1))
        r = ipct.ping(c, "kernel.echo@a", "count=50 bytes=1000")
        check("c's pings to a's echo, over the connection the registry made, all answered",
              r is not None and r["answered"] == 50 and r["wrong"] == 0, r)
        out = a.run("cluster")
        check("a's member says three hosts, three voters", "3 hosts, 3 voters" in out, out)

        # -- the provider: the bus answering as the command does --
        out = a.run("mgmt call cluster hosts")
        m = re.search(r"(\{.*\})", out, re.S)
        try:
            names = sorted(h["name"] for h in json.loads(m.group(1))["hosts"]) if m else []
        except (ValueError, KeyError):
            names = []
        check("the cluster provider's hosts, through the bus, the three", names == ["a", "b", "c"], out)

        # -- maintenance and an address, as every host says them --
        out = a.run("cluster maintenance b on")
        check("b's maintenance on, from a", "being worked on" in out, out)
        check("every host says b is in maintenance",
              ipct.wait_for(lambda: all("maintenance" in host_line(s, "b") for s in (a, b, c)), 20, 1))
        b.run("cluster maintenance b off")
        check("and every host says it is back",
              ipct.wait_for(lambda: all("maintenance" not in host_line(s, "b") for s in (a, b, c)), 20, 1))
        out = b.run("cluster address c public %s" % PUBLIC)
        check("c's public address set, from b", PUBLIC in out, out)
        check("every host has c's public address",
              ipct.wait_for(lambda: all("public %s" % PUBLIC in host_line(s, "c") for s in (a, b, c)), 20, 1))
        out = c.run("cluster address c public none")
        check("and taken away", ipct.wait_for(lambda: all(PUBLIC not in host_line(s, "c") for s in (a, b, c)), 20, 1),
              out)
        out = a.run("cluster address c ipc none")
        check("c's ipc address never taken away", "not a host's address" in out, out)

        # -- c moved to a new key, b's pings to c's echo running across it --
        old_c = key_of(c)
        ups_before = [ipct.counter(ipct.ipcnet_line(s, "c"), "ups") for s in (a, b)]
        # As many as take some seconds -- a shell's answer is waited for two
        # minutes, and an emulated CPU's round trip is ten times a real one's
        pings = 2000 if accel == "tcg" else 6000
        thread, box = ipct.in_thread(lambda: ipct.ping(b, "kernel.echo@c", "count=%d bytes=200" % pings))
        time.sleep(0.5)
        out = c.run("cluster rekey")
        need("c moves to a new key", "this host's key is" in out and "retired" in out, out)
        thread.join(300)
        r = box.get("r")
        check("b's %d pings to c's echo, across the rekey, every one answered as sent" % pings,
              r is not None and r["answered"] == pings and r["wrong"] == 0 and not r["down"], r)
        ups_after = [ipct.counter(ipct.ipcnet_line(s, "c"), "ups") for s in (a, b)]
        check("no connection with c lost on a or b", ups_after == ups_before and min(ups_after) >= 1,
              (ups_before, ups_after))
        new_c = key_of(c)
        check("c's ipcnet has a new key", new_c is not None and new_c != old_c, (old_c, new_c))
        check("every host's registry has c's new key, and the old one alone gone",
              ipct.wait_for(lambda: all(("key %s..." % new_c[:16]) in host_line(s, "c") and "moving to" not in
                                        host_line(s, "c") for s in (a, b, c)), 20, 1))
        # The connections made again: with the new key alone
        c.run("ipcnet stop")
        c.run("ipcnet start")
        check("c's connections made again with its new key",
              ipct.wait_for(lambda: ipct.peer_up(a, "c") and ipct.peer_up(b, "c"), 40, 1))
        out = a.run("cluster invite x 10.9.0.99 %s" % old_c)
        check("c's old key refused when invited again", "dropped" in out and "nos1-" not in out, out)

        # -- all three killed and booted again at once --
        for i in range(n):
            kill(i)
        relogs = [boot(i) for i in range(n)]
        for i in range(n):
            need("%s boots again" % HOSTS[i], pt.wait_log(relogs[i], "boot: complete", 300) and addressed(i))
        a, b, c = [ipct.Shell(hub.shell(i)) for i in range(n)]
        need("back: a leader agreed on, three voters",
             ipct.wait_for(lambda: leader_of(a) is not None and leader_of(a) == leader_of(b) == leader_of(c)
                           and voters(a) == ["a", "b", "c"], 90, 1))
        need("and every host up to every other", ipct.wait_for(lambda: all_up(everyone), 60, 1))

        # -- recover: refused while the majority answers; then b and c lost for good --
        out = a.run("cluster recover")
        check("recover refused while the majority answers", "recover is for a cluster whose majority" in out
              or "answers" in out, out)
        kill(1)
        kill(2)
        check("a, its majority gone, follows no leader", ipct.wait_for(lambda: leader_of(a) is None, 30, 1))
        out = a.run("cluster recover")
        check("recover says what it does first", "To go ahead: cluster recover test" in out
              and "the hosts removed: b, c" in out, out)
        out = a.run("cluster recover tset")
        check("a name not the cluster's refused", "not this cluster's name" in out, out)
        out = a.run("cluster recover test")
        need("recover: a the one voter, b and c removed", "one voter now; removed for good: b, c" in out, out)
        need("a leads alone, one host and one voter",
             ipct.wait_for(lambda: re.search(r"leader a, applied \d+, 1 hosts, 1 voters", a.run("cluster")), 20, 1))
        out = a.run("cluster invite c %s %s" % (ip(2), new_c))
        check("c's key refused: c removed for good", "removed" in out and "nos1-" not in out, out)
        # c installed anew: a new key, invited and joined
        images[2] = rootfs(tmp, args.arch, "c", os.urandom(16).hex(), tag="-new")
        need("c, installed anew, boots", pt.wait_log(boot(2), "boot: complete", 300) and addressed(2))
        c = ipct.Shell(hub.shell(2))
        kc = key_of(c)
        out = a.run("cluster invite c %s %s" % (ip(2), kc))
        tc = token_of(out)
        need("a invites the new c", tc is not None, out)
        out = c.run("cluster join %s" % tc)
        need("the new c joins as a new node", "joined as node 4" in out, out)
        need("a and the new c, two voters, up to each other",
             ipct.wait_for(lambda: voters(a) == ["a", "c"] and ipct.peer_up(a, "c") and ipct.peer_up(c, "a"), 40, 1))

        # -- the leader of the two removed: the lead handed on first --
        name = leader_of(a)
        need("a and c agree on a leader", ipct.wait_for(lambda: leader_of(a) in ("a", "c")
                                                         and leader_of(c) == leader_of(a), 30, 1))
        name = leader_of(a)
        lead, other = (a, c) if name == "a" else (c, a)
        rest = "c" if name == "a" else "a"
        out = lead.run("cluster remove %s" % name)
        if "removed" not in out:
            # What each host says of it, before the stop: whether the removal
            # was made after all, its answer lost on the way to the host removed
            for x, who in ((lead, name), (other, rest)):
                for cmd in ("cluster", "cluster hosts", "ipcnet", "dmesg 2000 cluster:"):
                    print("    [%s] %s:\n%s" % (who, cmd, x.run(cmd)), flush=True)
        need("%s, the leader of two, removed" % name, "removed" in out, out)
        log = lead.run("dmesg 2000 cluster:")
        check("%s handed the lead to %s before its own removal, as its log says" % (name, rest),
              re.search(r"asked to remove this host, the leader: the lead handed to node \d+ first", log) is not None,
              log)
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
        for i in range(n):
            kill(i)
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
