#!/usr/bin/env python3
"""NBS's metadata service between real kernels (plans/nbs.md, N5 and N6):
three nos on a LAN of scripts/hub.py's, each with ipcnet.ko, cluster.ko and
nbs-mds.ko and a pool disk of three partitions -- the cluster registry made
first, so that the hosts trust each other, then an NBS cluster of its
members, then what it stores.

- `nbs` on a host in no NBS cluster says so; `nbs init` on a makes one, a
  its leader and one voter; a second init refused;
- b added from a -- offered the cluster, its incarnation made there -- and c
  from b, a member that does not lead (the command handed to the leader):
  three members, three voters, one leader, as every host says; an add asked
  again answered with the same incarnation;
- a commit, through the majority: an entry's answer comes only once it is
  committed and applied, and `nbs host remove nosuch`'s refusal is one;
- what is stored (N6): no volume before three hosts have a device; a pool's
  partitions in `disks`, each its GUID its id; a device added by its id or
  by its name, the pool's disk itself refused for having no id, a device
  added again the same one, and c's copy of a's partition -- a disk moved
  between servers -- refused; every host saying the same of the chunk
  servers and their room. A volume made, made again the same, refused with
  another size, grown, refused smaller, refused past the room or with a
  name that is none, made from b for c; the room filled to its last slot
  and given back by a delete; a device drained, what it holds counted
  against the room its host has left; the `nbs` provider answering through
  the bus as the command does; the state's digest taken at the same entry
  on every host, the same, the leader's held to it; every host's log
  compacted into a snapshot (`compact=`);
- the leader killed: the other two elect one of themselves and commit; the
  killed host booted again, back from its snapshot and log -- three voters
  again, every host agreeing on the leader and on what is stored;
- the leader cut off from the other two by the hub: they elect one of
  themselves and commit, while the one cut off commits nothing; healed, it
  follows the new leader;
- a host's state lost -- its mds unloaded, its folder removed, loaded again:
  the leader says that host's mds has no member of its incarnation; an add
  of it refused, its old member being the cluster's; `nbs host replace`
  takes it back as a new member, the old incarnation removed for good, sent
  the state as a snapshot;
- a member removed: two members left; its old incarnation refused when it is
  added again, since it never lost its state; its chunk server kept, taking
  no new chunks, and so a volume refused;
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
import json
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


pt = load("pt", "parttest.py")
hubmod = load("hub", "hub.py")
ipct = load("ipct", "ipc-test.py")

HOSTS = ["a", "b", "c"]
SHELL_PORT = ipct.SHELL_PORT
HUB_BASE = 47900
FAILED = []
MODULES = ("ipcnet", "cluster", "nbs-mds")
# Each host's pool: a sparse GPT disk of three partitions of 300 MiB
POOL_MB = 1024
PART_SECTORS = 300 * 2048
POOL_PARTS = tuple((2048 + i * PART_SECTORS, 2048 + (i + 1) * PART_SECTORS - 1) for i in range(3))
# The log's bytes past its snapshot before nbs-mds compacts it (`compact=`):
# few, so that the log is compacted during the test -- a member started
# again reads its state back from a snapshot, and one added is sent it
COMPACT = 4096
CHUNK_MIB = 4


def part_slots(sectors):
    """The 4 MiB slots a device of `sectors` gives its chunk server, after
    what the cs keeps at its start (nbscs's format.rs): a slot for its
    superblock and fences, then its table of slots -- 64 bytes a slot of the
    device, each copy rounded up to pages of 64 -- twice, rounded up to
    slots."""
    slot = CHUNK_MIB << 20
    every = sectors * 512 // slot
    records = -(-every // 64) * 64
    return every - (1 + -(-2 * records * 64 // slot))


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
        f.write("# nbs-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /nbs-mds.ko compact=%d\n"
                % COMPACT)
    image = os.path.join(tmp, "root-%s.img" % name)
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def pool(tmp, name, guids):
    """Host `name`'s pool disk: three partitions, the i-th's unique GUID
    guids[i]."""
    path = os.path.join(tmp, "pool-%s.img" % name)
    pt.gpt_image(path, size_mb=POOL_MB, parts=POOL_PARTS, unique=lambda i: guids[i].bytes_le,
                 disk_guid=uuid.uuid4().bytes_le, marked=False)
    return path


def disks(sh):
    """`disks` as {id: name}, and the names of the devices with no id."""
    by_id, bare = {}, []
    for line in sh.run("disks").splitlines():
        w = line.split()
        if len(w) < 2 or "sectors" not in w:
            continue
        if w[-1].startswith(("part:", "disk:")):
            by_id[w[-1]] = w[0]
        else:
            bare.append(w[0])
    return by_id, bare


def same(shells, cmd):
    """`cmd`'s output if every host of `shells` gives the same, else None."""
    outs = [s.run(cmd).rstrip() for s in shells]
    return outs[0] if len(set(outs)) == 1 else None


def stored_alike(shells):
    """Every host of `shells` says the same of the volumes and of the chunk
    servers."""
    return same(shells, "nbs volumes") is not None and same(shells, "nbs cs") is not None


# How long a member that does not lead may take to apply what its leader
# has: a heartbeat, and the test's own round trips. The host a change is
# typed at is answered only once it has applied it too, and is read at once.
APPLIED_S = 10


def eventually(sh, cmd, ok):
    """`cmd`'s output on `sh` once `ok` holds of it, or the last one."""
    end = time.time() + APPLIED_S
    while True:
        out = sh.run(cmd)
        if ok(out) or time.time() >= end:
            return out
        time.sleep(0.3)


def alike(shells, cmd, ok):
    """`cmd`'s output once every host of `shells` gives the same and `ok`
    holds of it, or None."""
    end = time.time() + APPLIED_S
    while True:
        view = same(shells, cmd)
        if view is not None and ok(view):
            return view
        if time.time() >= end:
            return None
        time.sleep(0.3)


def digest(sh):
    """`nbs`'s newest digest of the state: (entry, its digest's start, the
    leader's held to it, too old to)."""
    m = re.search(r"digest at entry (\d+): ([0-9a-f]+)\.\.\. -- (\d+) of the leader's held to it, (\d+) too old to",
                  sh.run("nbs"))
    return (int(m.group(1)), m.group(2), int(m.group(3)), int(m.group(4))) if m else None


def same_digest(shells):
    """Every host of `shells` took its newest digest at the same entry, the
    same one, and has held a digest of the leader's to its own."""
    d = [digest(s) for s in shells]
    return all(x is not None and x[2] >= 1 for x in d) and len({(x[0], x[1]) for x in d}) == 1


def compacted(sh):
    """Whether the member's log on `sh` has a snapshot."""
    return re.search(r"\bsnap\.[0-9a-f]{16}(?!\.tmp)\b", sh.run("ls /nbs/mds/raft")) is not None


def json_of(out):
    m = re.search(r"(\{.*\})", out, re.S)
    try:
        return json.loads(m.group(1)) if m else None
    except ValueError:
        return None


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
    guids = {h: [uuid.uuid4() for _ in POOL_PARTS] for h in HOSTS}
    # c's third partition a copy of a's first, as if a's disk were moved to c
    guids["c"][2] = guids["a"][0]
    ids = {h: ["part:%s" % g for g in guids[h]] for h in HOSTS}
    pools = [pool(tmp, h, guids[h]) for h in HOSTS]
    n = len(HOSTS)
    hub = hubmod.Hub(n, HUB_BASE, shell_port=SHELL_PORT).start()
    procs, logs = [None] * n, []

    def boot(i):
        log = os.path.join(tmp, "serial-%s-%d.log" % (HOSTS[i], len(logs)))
        logs.append(log)
        procs[i] = subprocess.Popen(ipct.qemu(args.arch, accel, images[i], log, hub, i, args.iso, disks=[pools[i]]),
                                    cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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

        # -- what is stored (N6): each host's chunk server and its devices, the volumes --
        out = a.run("nbs cs")
        check("no chunk server has a device yet", "no chunk server has a device yet" in out, out)
        out = a.run("nbs volume create early 8")
        check("a volume before any device refused: fewer than three hosts take new chunks",
              "fewer than three hosts take new chunks" in out, out)
        found = [disks(x) for x in sh]
        for i, h in enumerate(HOSTS):
            check("%s's pool: its three partitions in disks, each its GUID its id" % h,
                  all(d in found[i][0] for d in ids[h]), found[i])
        need("every host's pool found", all(ids[h][0] in found[i][0] and ids[h][1] in found[i][0]
                                            for i, h in enumerate(HOSTS)))
        whole = found[0][0][ids["a"][0]][:-1]
        out = a.run("nbs devices add %s" % whole)
        check("a's pool disk itself refused: no id finds it again", whole in found[0][1] and "has no id" in out, out)
        room = part_slots(PART_SECTORS) * CHUNK_MIB
        dev = {}
        for i, h, word in ((0, "a", ids["a"][0]), (1, "b", found[1][0][ids["b"][0]]), (2, "c", ids["c"][0])):
            out = sh[i].run("nbs devices add %s" % word)
            m = re.search(r"device %s of this host's chunk server, (\d+) MiB -- device (\d+) in the metadata"
                          % re.escape(ids[h][0]), out)
            need("%s's first partition its chunk server's device, %d MiB%s" % (h, room, "" if i != 1 else ", by name"),
                 m is not None and int(m.group(1)) == room, out)
            dev[h] = int(m.group(2))
            if i == 1:
                out = a.run("nbs volume create early 8")
                check("with two hosts' devices, still too few", "fewer than three hosts take new chunks" in out, out)
        out = a.run("nbs devices add %s" % ids["a"][0])
        check("a's device added again: the same device", ("device %d in the metadata" % dev["a"]) in out, out)
        out = c.run("nbs devices add %s" % ids["c"][2])
        check("c's copy of a's partition -- a disk moved between servers -- refused", "another host's" in out, out)
        view = alike(sh, "nbs cs", lambda v: len(re.findall(r"^\s+[abc]\s+0 MiB of %d MiB reserved, %d MiB free, "
                                                              r"1 devices$" % (room, room), v, re.M)) == 3)
        check("every host says the same of the chunk servers: three of %d MiB each, nothing reserved" % room,
              view is not None)

        out = a.run("nbs volume create disk1 64")
        m = re.search(r"volume disk1 \((\d+)\), 64 MiB", out)
        need("a volume of 64 MiB made", m is not None, out)
        vol1 = int(m.group(1))
        out = b.run("nbs volume create disk1 64")
        check("made again with the same size: the same volume", ("volume disk1 (%d)" % vol1) in out, out)
        out = b.run("nbs volume create disk1 128")
        check("the same name with another size refused", "of another size" in out, out)
        out = eventually(c, "nbs volume disk1", lambda o: "16 chunks" in o)
        check("its 16 chunks, its own host a, detached", "64 MiB, 16 chunks of 4 MiB, its own host a, detached" in out,
              out)
        check("every chunk on all three hosts", "replicas, chunks a host: a 16, b 16, c 16" in out, out)
        out = c.run("nbs volume resize disk1 100")
        check("grown to 100 MiB", "is 100 MiB now" in out, out)
        out = c.run("nbs volume disk1")
        check("read at once where it was typed: 25 chunks now, on all three hosts",
              "25 chunks" in out and "a 25, b 25, c 25" in out, out)
        out = eventually(a, "nbs volume disk1", lambda o: "25 chunks" in o)
        check("and on a", "25 chunks" in out and "a 25, b 25, c 25" in out, out)
        out = a.run("nbs volume resize disk1 80")
        check("made smaller refused", "never made smaller" in out, out)
        out = a.run("nbs volume create huge 1T")
        check("a volume past the room refused", "not room for its chunks on three hosts" in out, out)
        out = a.run("nbs volume create Disk 4")
        check("a name that is no volume's refused", "not a volume's name" in out, out)
        out = a.run("nbs volume create disk3 4 host=nosuch")
        check("a host that is no member refused", "no member of the NBS cluster" in out, out)
        out = b.run("nbs volume create disk2 40 host=c")
        need("a volume of c's made from b", re.search(r"volume disk2 \(\d+\), 40 MiB", out) is not None, out)
        out = b.run("nbs volume disk2")
        check("read at once where it was typed: its own host c, its chunks on all three",
              "its own host c" in out and "a 10, b 10, c 10" in out, out)
        taken = 35 * CHUNK_MIB
        held = r"^\s+[abc]\s+%d MiB of %d MiB reserved, %d MiB free" % (taken, room, room - taken)
        out = b.run("nbs cs")
        check("and its room taken, as b says at once: %d MiB of each host's, %d MiB free" % (taken, room - taken),
              len(re.findall(held, out, re.M)) == 3, out)
        view = alike(sh, "nbs cs", lambda v: len(re.findall(held, v, re.M)) == 3)
        check("every host says %d MiB of each host's taken, %d MiB free" % (taken, room - taken), view is not None)
        out = a.run("nbs volume create fill %d" % (room - taken))
        check("a volume of all the room left made", "volume fill (" in out, out)
        out = a.run("nbs volume create more 4")
        check("and then not a chunk more: not room, three hosts full", "not room for its chunks on three hosts" in out,
              out)
        out = b.run("nbs volume delete fill")
        check("the volume deleted", "deleted" in out, out)
        out = b.run("nbs volume delete fill")
        check("deleted again: gone already", "gone already" in out, out)
        out = a.run("nbs volume create more 4")
        check("its room given back", "volume more (" in out, out)

        out = c.run("nbs devices add %s" % ids["c"][1])
        check("c's second partition a device too", "in the metadata" in out, out)
        out = c.run("nbs devices drain %s" % ids["c"][0])
        check("c's first drained", "takes no new chunks" in out, out)
        out = c.run("nbs cs")
        taken = 36 * CHUNK_MIB
        check("c's room its second device's, what its first holds counted against it",
              re.search(r"^\s+c\s+%d MiB of %d MiB reserved, %d MiB free, 2 devices$" % (taken, room, room - taken),
                        out, re.M) is not None
              and re.search(r"device %d\s+%s\s+\d+ MiB draining" % (dev["c"], re.escape(ids["c"][0])), out) is not None,
              out)

        js = json_of(b.run('mgmt call nbs volume {"name":"disk1"}'))
        check("the provider's volume, through the bus, as the command says it",
              js is not None and js.get("chunks") == 25 and js.get("host") == "a"
              and sorted((r["host"], r["chunks"]) for r in js.get("replicas", [])) == [("a", 25), ("b", 25), ("c", 25)],
              js)
        js = json_of(c.run('mgmt call nbs volume_create {"name":"api1","size_mib":8}'))
        check("a volume made through the bus", js is not None and isinstance(js.get("volume"), int), js)
        out = eventually(a, "nbs volume api1", lambda o: "2 chunks" in o)
        check("and there on a, c its own host", "8 MiB, 2 chunks of 4 MiB, its own host c" in out, out)
        out = c.run('mgmt call nbs volume_create {"name":"api1","size_mib":12}')
        check("made again through the bus with another size: a conflict", "mgmt: conflict" in out, out)

        # -- the digest: every 64 entries, every member's held to the leader's --
        for k in range(1, 61):
            a.run("nbs volume create t%d 4" % k)
            b.run("nbs volume delete t%d" % k)
            if k % 5 == 0 and same_digest(sh):
                break
        need("every host took its state's digest at the same entry, the same, and held the leader's to it",
             ipct.wait_for(lambda: same_digest(sh), 30, 1))
        check("every host says the same of the volumes and the chunk servers",
              ipct.wait_for(lambda: stored_alike(sh), APPLIED_S, 0.5))
        check("every host's log compacted into a snapshot", all(compacted(x) for x in sh))

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
        check("%s's state, back from its snapshot and its log, the others'" % name,
              ipct.wait_for(lambda: stored_alike(sh), 30, 1))
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
        out = sh[fi].run("insmod /nbs-mds.ko compact=%d" % COMPACT)
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
        check("%s's new member sent the state: the same volumes and chunk servers as the others'" % lost,
              ipct.wait_for(lambda: stored_alike(sh), 30, 1))
        check("as a snapshot: %s's new log has one" % lost, compacted(sh[fi]))

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
        out = sh[HOSTS.index(name)].run("nbs cs")
        check("%s's chunk server kept, taking no new chunks" % gone,
              re.search(r"^\s+%s\s.*\(no member: takes no new chunks\)$" % gone, out, re.M) is not None, out)
        out = sh[HOSTS.index(name)].run("nbs volume create late 4")
        check("and so a volume refused: fewer than three hosts take new chunks",
              "fewer than three hosts take new chunks" in out, out)
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
