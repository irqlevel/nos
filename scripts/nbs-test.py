#!/usr/bin/env python3
"""NBS between real kernels (plans/nbs.md, N5 to N7): three nos on a LAN of
scripts/hub.py's, each with ipcnet.ko, cluster.ko, nbs-mds.ko and nbs-cs.ko
and a pool disk of three partitions -- the cluster registry made first, so
that the hosts trust each other, then an NBS cluster of its members, then
what it stores, then a volume's bytes through the chunk servers.

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
- the chunk servers (N7): every host's cs takes its devices in, formatting
  them; a device with something else on it refused, and taken with `wipe`;
  a volume written to its three replicas and read back -- across a chunk's
  end, from the writer's host and read-only from the others -- flushed, its
  chunks open on every cs, then closed; a writer on another host refused,
  then forced: its fences refuse the old writer's writes and reads; a host
  booted again with its data, its fence and its open marks; a cs slow, not
  silent (N11) -- c's pool throttled by QEMU, a writer stopped that opened
  32 chunks, seconds of closes queued at c -- left in its set, the leader's
  look answered ahead of its queue; a snapshot of a
  volume its writer writes (N10c2) -- asked on another host, refused,
  naming the writer's; asked on the writer's host, a member that need not
  lead, its session (`nbs io`'s, driven by the snapshot's command while it
  pauses) paused for it and the snapshot taken; the writer writing on, its
  chunk copied up first; the volume as written since, the snapshot as the
  volume was; clones and images (N10d) -- a volume cloned refused, the
  snapshot cloned and read on a third host as it is, written there, its
  chunk copied up and the snapshot as it was; the clone made an image,
  written refused, cloned, deleted under its clone, whose write then takes
  the chunk in place; all of them deleted; the volumes deleted, and every cs gives its
  chunks -- the one a snapshot shared and the one copied up among them --
  and its fence back; a host's nbs-cs unloaded -- a request to it bounced
  and said at once -- and loaded again, serving;
- repair (N9): a volume written, c killed -- the leader's controller has its
  cs down -- the volume written past it, c booted again: its cs up, joined in
  its own place, every chunk at three replicas again, and c's own replica
  holding the write made while it was gone, which its join's copy took;
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
out/<arch>/modules/{ipcnet,cluster,nbs-mds,nbs-cs}.ko.
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
MODULES = ("ipcnet", "cluster", "nbs-mds", "nbs-cs", "nbsload")
# Each host's pool: a sparse GPT disk of three partitions of 300 MiB
POOL_MB = 1024
PART_SECTORS = 300 * 2048
POOL_PARTS = tuple((2048 + i * PART_SECTORS, 2048 + (i + 1) * PART_SECTORS - 1) for i in range(3))
# The log's bytes past its snapshot before nbs-mds compacts it (`compact=`):
# few, so that the log is compacted during the test -- a member started
# again reads its state back from a snapshot, and one added is sent it
COMPACT = 4096
# The leader's scrub's pace here (N9e): a chunk a second, so that it looks at
# every chunk of the test's volumes while the test runs.
SCRUB_S = 1
# How long the leader takes a silent cs for out, its places for others' to
# take (N9f): the least nbs-mds takes, so that a host lost for good is
# replaced while the test runs. With three hosts nothing can take a place
# -- a set's three are on three hosts -- and a host back comes back into its
# own.
OUT_S = 10
# A fourth host, booted late (N9f): one of four lost for good, its places
# are d's.
LATE = "d"
ALL = HOSTS + [LATE]
CHUNK_MIB = 4
# A slow cs (N11, N12c): c's pool held to this many operations a second
# while a writer opens this many chunks and stops -- fences, open marks and
# closes queued at c's device, each write slower there than the client's
# 0.5 s of silence.
BURST_IOPS = 20
BURST_CHUNKS = 16
SILENT_S = 0.5


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
        f.write("# nbs-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\n"
                "insmod /nbs-mds.ko compact=%d scrub=%d out_after=%d\ninsmod /nbs-cs.ko\n"
                % (COMPACT, SCRUB_S, OUT_S))
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


def scribble(path, part):
    """Something that is not NBS's on partition `part` of pool `path`: the
    magic of an ext filesystem's superblock, 1 KiB in."""
    with open(path, "r+b") as f:
        f.seek(POOL_PARTS[part][0] * 512 + 1024 + 56)
        f.write(b"\x53\xef")


def cs_of(sh):
    """`nbscs` on `sh`: (state, held, open, fenced) and its devices' lines,
    each (device, id, MiB, free MiB, lost, what) -- None for a cs with no
    number yet."""
    out = sh.run("nbscs")
    m = re.search(r"^nbscs: cs \d+ -- (.*?): (\d+) chunks held, (\d+) open, (\d+) volumes fenced", out, re.M)
    if not m:
        return None
    devs = re.findall(r"^\s+device (\d+)\s+(\S+)\s+(\d+) MiB, (\d+) MiB free, (\d+) slots lost, (\w+)$", out, re.M)
    return ((m.group(1), int(m.group(2)), int(m.group(3)), int(m.group(4))),
            [(int(d), i, int(mib), int(free), int(lost), what) for d, i, mib, free, lost, what in devs])


def chunks_of(sh):
    """`nbscs chunks` on `sh`: (chunk, incarnation, state, volume,
    generation) each."""
    return [(int(c), int(i), s, int(v), int(g)) for c, i, s, v, g in
            re.findall(r"^\s+chunk (\d+)\.(\d+) (\w+) volume (\d+) generation (\d+) device \d+$",
                       sh.run("nbscs chunks"), re.M)]


def fences_of(sh):
    """`nbscs fences` on `sh`: {volume: generation}."""
    return {int(v): int(g) for v, g in re.findall(r"^\s+volume (\d+) fenced at generation (\d+)$",
                                                  sh.run("nbscs fences"), re.M)}


# How long a cs may take to take a device in, or to give back what the
# metadata no longer has: it asks its mds every 5 s, and every 30 s
CS_S = 45


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


def said_by_one(shells, cmd, ok):
    """The first of `shells` whose `cmd` output `ok` holds of, and that
    output -- polled until one does, or the first's last one."""
    end = time.time() + APPLIED_S
    while True:
        outs = [(s, s.run(cmd)) for s in shells]
        for s, out in outs:
            if ok(out):
                return s, out
        if time.time() >= end:
            return outs[0]
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


def health(sh):
    """`nbs status`'s counts of how far from three the chunks are (N9): at
    three replicas, at two, at one, being copied, the joins, the cs's down
    and out."""
    m = re.search(r"chunks at three replicas (\d+), at two (\d+), at one (\d+) -- (\d+) being copied, by "
                  r"(\d+) joins; cs's down (\d+), out (\d+)", sh.run("nbs status"))
    return tuple(int(g) for g in m.groups()) if m else None


def whole(sh):
    """Every chunk at three replicas, none being copied, no cs down or out."""
    h = health(sh)
    return h is not None and h[0] > 0 and h[1:] == (0, 0, 0, 0, 0, 0)


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
    images = [rootfs(tmp, args.arch, h, os.urandom(16).hex()) for h in ALL]
    guids = {h: [uuid.uuid4() for _ in POOL_PARTS] for h in ALL}
    # c's third partition a copy of a's first, as if a's disk were moved to c
    guids["c"][2] = guids["a"][0]
    ids = {h: ["part:%s" % g for g in guids[h]] for h in ALL}
    pools = [pool(tmp, h, guids[h]) for h in ALL]
    # a's second partition with a filesystem's magic on it: refused as a
    # device until wiped
    scribble(pools[0], 1)
    n = len(HOSTS)
    hub = hubmod.Hub(len(ALL), HUB_BASE, shell_port=SHELL_PORT).start()
    procs, logs = [None] * len(ALL), []

    def qmp(i):
        return os.path.join(tmp, "qmp-%s.sock" % ALL[i])

    def boot(i):
        log = os.path.join(tmp, "serial-%s-%d.log" % (ALL[i], len(logs)))
        logs.append(log)
        if os.path.exists(qmp(i)):
            os.remove(qmp(i))
        procs[i] = subprocess.Popen(ipct.qemu(args.arch, accel, images[i], log, hub, i, args.iso, disks=[pools[i]],
                                              monitor=qmp(i)),
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
        pool_disk = found[0][0][ids["a"][0]][:-1]
        out = a.run("nbs devices add %s" % pool_disk)
        check("a's pool disk itself refused: no id finds it again", pool_disk in found[0][1] and "has no id" in out,
              out)
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

        # -- the chunk servers (N7): the devices taken in, a volume's bytes through them --
        def devices_of(x):
            return (cs_of(x) or (None, []))[1]

        def serving(i, want):
            got = cs_of(sh[i])
            return got is not None and got[0][0] == "serving" and len(got[1]) == want and \
                all(d[2] == room and d[4] == 0 for d in got[1])
        need("every host's cs takes its devices in -- formatted, %d MiB each, nothing lost -- and serves" % room,
             ipct.wait_for(lambda: all(serving(i, 2 if HOSTS[i] == "c" else 1) for i in range(n)), CS_S, 1),
             [x.run("nbscs") for x in sh])
        out = c.run("nbscs")
        check("c's first device draining, its second active",
              re.search(r"device %d\s+%s\s+%d MiB, %d MiB free, 0 slots lost, draining$"
                        % (dev["c"], re.escape(ids["c"][0]), room, room), out, re.M) is not None
              and re.search(r"%s\s+%d MiB, %d MiB free, 0 slots lost, active$" % (re.escape(ids["c"][1]), room, room),
                            out, re.M) is not None, out)
        out = a.run("nbs devices add %s" % ids["a"][1])
        check("a's second partition, a filesystem's magic on it, refused", "not NBS's" in out and "wipe" in out, out)
        out = a.run("nbs devices add %s wipe" % ids["a"][1])
        check("and taken with wipe", "in the metadata, its first pages wiped" in out, out)
        out = a.run("nbs devices add %s" % ids["a"][0])
        check("a device its cs holds, added again: the same device", ("device %d in the metadata" % dev["a"]) in out,
              out)
        check("a's cs takes the wiped device in too, formatting it",
              ipct.wait_for(lambda: serving(0, 2) and all(d[5] == "active" for d in devices_of(a)), CS_S, 1),
              a.run("nbscs"))

        # The cluster settled first: a cs the leader's controller took for
        # down as it took its devices in has its places joined again, and a
        # write under way meanwhile reaches it as a joiner
        need("every chunk at three replicas, none being copied, no cs down: the cluster settled",
             ipct.wait_for(lambda: whole(a), 60, 1), a.run("nbs status"))
        out = a.run("nbs io write disk1 0 65536 0x5a")
        m = re.search(r"65536 bytes of 0x5a written at 0 of disk1, generation (\d+): 1 chunks, to every replica in",
                      out)
        need("64 KiB written to disk1's first chunk, to its replicas: a session attached for writing on a",
             m is not None, out)
        gen1 = int(m.group(1))
        # What `nbscs` shows is what the cs's task last made of it: a second old at most
        check("every cs fences disk1 at a's generation",
              ipct.wait_for(lambda: all(fences_of(x).get(vol1) == gen1 for x in sh), 5, 0.5),
              [x.run("nbscs") + x.run("dmesg 8192 nbs") for x in sh])
        end = CHUNK_MIB << 20
        out = a.run("nbs io write disk1 %d 8192 0x33" % (end - 4096))
        check("8 KiB across a chunk's end: two chunks", "8192 bytes of 0x33" in out and ": 2 chunks" in out, out)
        out = a.run("nbs io read disk1 0 65536")
        check("read back on a, from its own replica", "65536 bytes at 0 of disk1, from a: 0x5ax65536" in out, out)
        out = a.run("nbs io read disk1 %d 16384" % (end - 8192))
        check("and across the chunk's end", "from a: 0x00x4096 0x33x8192 0x00x4096" in out, out)
        out = b.run("nbs io read disk1 0 65536")
        check("a read on b -- read-only, a session of its own -- refused while a writes",
              "attached for writing" in out, out)
        out = a.run("nbs io flush disk1")
        check("flushed", "nbs: disk1 flushed" in out, out)
        out = a.run("nbs io held disk1")
        check("its two chunks open on every cs, at a's generation",
              all(re.search(r"^\s+%s\s+2 chunks of disk1 open:.*@%d" % (h, gen1), out, re.M) for h in HOSTS), out)
        out = a.run("nbs io close disk1")
        check("closed", "every chunk written here closed on its replicas" in out, out)
        out = a.run("nbs io held disk1")
        check("and none is now", all(re.search(r"^\s+%s\s+0 chunks of disk1 open:" % h, out, re.M) for h in HOSTS),
              out)

        out = b.run("nbs io write disk1 0 4096 0x77")
        check("a write on b refused while a has disk1 attached", "attached for writing on another host" in out, out)
        out = b.run("nbs io attach disk1 force")
        m = re.search(r"disk1 attached for writing here, generation (\d+)", out)
        need("b takes disk1 with force: a newer generation", m is not None and int(m.group(1)) > gen1, out)
        gen2 = int(m.group(1))
        out = b.run("nbs io write disk1 0 4096 0x77")
        check("b writes, fencing every cs at its generation", ("generation %d: 1 chunks" % gen2) in out, out)
        out = a.run("nbs io write disk1 0 4096 0x11")
        check("a's write at its older generation refused by the fences: its session failed",
              "attached elsewhere: this attach is fenced" in out, out)
        out = a.run("nbs io read disk1 0 4096")
        check("and a's read, read-only now, refused while b writes", "attached for writing" in out, out)
        out = b.run("nbs io read disk1 0 8192")
        check("b reads its write, and a's after it", "from b: 0x77x4096 0x5ax4096" in out, out)
        check("every cs fences disk1 at b's generation",
              ipct.wait_for(lambda: all(fences_of(x).get(vol1) == gen2 for x in sh), 5, 0.5),
              [x.run("nbscs") for x in sh])
        # b's chunks closed before c goes: one a writer has open on a host that
        # restarts is taken out of its set at the writer's next flush (N8)
        out = b.run("nbs io close disk1")
        check("b closes what it wrote", "every chunk written here closed on its replicas" in out, out)

        # c booted again: its cs takes its devices back as they were
        kill(2)
        need("c boots again", pt.wait_log(boot(2), "boot: complete", 300) and addressed(2))
        sh[2] = c = ipct.Shell(hub.shell(2))
        need("c back: three voters, one leader", ipct.wait_for(lambda: agreed(sh, 3), 90, 1))
        need("c's cs serving again", ipct.wait_for(lambda: serving(2, 2), CS_S, 1), c.run("nbscs"))
        check("c's fence kept: disk1 at b's generation", fences_of(c).get(vol1) == gen2, fences_of(c))
        held = [x for x in chunks_of(c) if x[3] == vol1]
        check("c's chunks of disk1 kept: the first closed at b's generation, the second at a's",
              sorted((x[2], x[4]) for x in held) == sorted([("closed", gen2), ("closed", gen1)]), held)

        # the volume deleted: every cs gives its chunks and its fence back
        out = a.run("nbs volume delete disk1")
        check("disk1 not deleted while b has it attached", "attached" in out and "deleted" not in out, out)
        out = b.run("nbs io detach disk1")
        check("b detaches it", ("disk1 detached (generation %d)" % gen2) in out, out)
        # A boot of c's past DOWN_AFTER_MS -- under TCG -- has the leader's
        # controller (N9c) take its cs down and its replicas out until its
        # joins are done, and a read takes an active replica alone
        check("c's sets whole again, as the leader's controller keeps them: every chunk at three replicas",
              ipct.wait_for(lambda: whole(a), 300, 3), [x.run("nbs status") for x in sh])
        out = c.run("nbs io read disk1 0 8192")
        check("c's data kept: read on c, read-only now nobody writes, from its own replica",
              "from c: 0x77x4096 0x5ax4096" in out, out)
        # nbsload: the client driven by a module of its own
        a.run("insmod /nbsload.ko")
        out = a.run("nbsload disk1 verify secs=3 qd=8 span=8")
        check("nbsload verify through the client: every block read back as last written",
              re.search(r"verify: \d+ reads held to their blocks' last writes as it went, \d+ blocks read back", out)
              is not None and "WRONG" not in out and "nbsload:" not in out, out)
        # Its session named (nbsdrive's), its writes left a once, along their
        # chains (N13): a's cs, the head, and the next passed them on, and
        # none was left for the client to send directly
        chains = [re.search(r"(\d+) writes passed on along chains, (\d+) not, (\d+) answers lost", x.run("nbscs"))
                  for x in sh]
        check("nbsload's writes passed along their chains, cs to cs, none left to the client",
              all(chains) and sum(int(m.group(1)) for m in chains) > 0
              and all(int(m.group(2)) == 0 for m in chains),
              [m.group(0) if m else None for m in chains])
        # Every answer of the cs's taken by the channel: one refused -- the
        # session's mailbox full, as a stop's closes once filled it (N13b) --
        # is the cs's silence to its client
        check("no cs's answer lost: the sessions' mailboxes had room for them all",
              all(chains) and all(int(m.group(3)) == 0 for m in chains), [m.group(0) if m else None for m in chains])
        # At depth 1 a write is alone, and goes to each replica directly
        # (N13c): no cs passes one on
        before = [int(m.group(1)) for m in chains if m]
        out = a.run("nbsload disk1 write secs=2 qd=1 span=8")
        written = re.search(r"(\d+) writes: \d+ IOPS", out)
        after = [re.search(r"(\d+) writes passed on along chains", x.run("nbscs")) for x in sh]
        check("nbsload at depth 1: each write alone, sent to each replica directly -- none passed on",
              written is not None and int(written.group(1)) > 0 and "nbsload:" not in out and all(after)
              and [int(m.group(1)) for m in after] == before, (out, before, [m.group(0) if m else None for m in after]))
        out = b.run("insmod /nbsload.ko")
        out = b.run("nbsload disk1 randread secs=2 qd=8 span=8")
        check("and reads on b, read-only", re.search(r"\d+ reads: \d+ IOPS", out) is not None and "nbsload:" not in out,
              out)

        # -- a cs slow, not silent (N11, N12c): c's pool throttled by QEMU to
        # BURST_IOPS operations a second from the writer's first write on --
        # its fence, each chunk's open mark, each close -- and a writer's
        # stop, which closes every chunk it opened on every replica at once:
        # seconds of queue at c's device. The client asks c out of the set
        # after 0.5 s; the leader's look (`Op::Ping`, 250 ms) is answered
        # whatever c's device is doing -- the cs's task never waits on it --
        # so c stays. Before N11 the look waited behind the closes, and every
        # stop of a writer that had written many chunks took the slower hosts
        # out of its sets, to be copied anew (found on the hardware); before
        # N12c it waited behind whatever request the cs was in --
        out = a.run("nbs volume create burst %d" % (BURST_CHUNKS * CHUNK_MIB))
        need("a volume of %d chunks" % BURST_CHUNKS, "nbs: volume burst" in out, out)
        taken = len(re.findall(r"taken out of volume burst", a.run("dmesg 65536 nbs")))
        ipct.hmp(qmp(2), "block_set_io_throttle more0 0 0 0 %d 0 0" % BURST_IOPS)
        try:
            t0 = time.time()
            outs = [a.run("nbs io write burst %d 4096 0x5b" % (k * (CHUNK_MIB << 20))) for k in range(BURST_CHUNKS)]
            wrote = time.time() - t0
            t0 = time.time()
            out = a.run("nbs io detach burst")
            took = time.time() - t0
        finally:
            ipct.hmp(qmp(2), "block_set_io_throttle more0 0 0 0 0 0 0")
        check("a writes a block into each of burst's chunks, each open on every replica", all("written at" in o for o in outs),
              [o for o in outs if "written at" not in o][:3])
        # Each write slower at c than the client waits before it asks c out:
        # c stays only where the leader's look is answered meanwhile
        check("each write slower than the client's %.1f s of silence, c's device queued: %.2f s a write"
              % (SILENT_S, wrote / BURST_CHUNKS), wrote / BURST_CHUNKS > SILENT_S)
        check("a detaches burst, every chunk closed -- their records together: %.1f s" % took, "burst detached" in out,
              out)
        log = a.run("dmesg 65536 nbs")
        check("c not taken out of burst's sets, slow as it was: the leader's look answered whatever its device did",
              len(re.findall(r"taken out of volume burst", log)) == taken,
              [l for l in log.split("\n") if "burst" in l][-6:])
        check("burst's chunks at three replicas still", ipct.wait_for(lambda: whole(a), 30, 1),
              [x.run("nbs status") for x in sh])
        out = a.run("nbs volume delete burst")
        check("burst deleted", "deleted" in out, out)

        # -- a snapshot of a volume its writer writes (N10c2): asked on the
        # writer's host -- b's, a member that need not lead -- its session
        # paused for it; asked elsewhere, refused naming the host; the writer
        # goes on, copying its chunk up first. The snapshot and the volume
        # deleted then: the chunk they shared and the one copied up are given
        # back with disk1's, below --
        out = a.run("nbs volume create snapsrc 4")
        need("a volume to take a snapshot of", "nbs: volume snapsrc" in out, out)
        out = b.run("nbs io write snapsrc 0 8192 0x41")
        need("written on b: a session there attached for writing", "8192 bytes of 0x41 written" in out, out)
        out = a.run("nbs volume snapshot snapsrc snap1")
        check("its snapshot asked on another host refused, naming the writer's", "attached on host b" in out, out)
        out = b.run("nbs volume snapshot snapsrc snap1")
        check("asked on the writer's host: its session paused for it, and the snapshot taken",
              re.search(r"nbs: snapshot snap1 \(\d+\) of volume snapsrc", out) is not None, out)
        out = b.run("nbs io write snapsrc 4096 4096 0x42")
        check("the writer writes on: its chunk, the snapshot's too, copied up first", "4096 bytes of 0x42 written" in out,
              out)
        out = b.run("nbs io read snapsrc 0 8192")
        check("the volume as written since", "0x41x4096 0x42x4096" in out, out)
        out = b.run("nbs io detach snapsrc")
        need("and let go", "snapsrc detached" in out, out)
        out = c.run("nbs io read snap1 0 8192")
        check("the snapshot as the volume was when it was taken, read on another host", "0x41x8192" in out, out)

        # -- clones and images (N10d): a volume cloned once it is an image or
        # a snapshot; a clone written copies its chunk up, its origin as it
        # was; an image read-only for good; an image deleted under a clone,
        # whose write then takes the chunk in place, another volume's on its
        # cs until then --
        out = a.run("nbs volume clone snapsrc fromvol")
        check("a volume cloned refused: a clone is of an image or a snapshot", "freeze or snapshot" in out, out)
        out = a.run("nbs volume clone snap1 sclone host=c")
        need("the snapshot cloned, for c", "a clone of snap1" in out, out)
        out = a.run("nbs volumes")
        check("each said as what it is: a snapshot of its volume, a clone of its snapshot",
              re.search(r"^\s+snap1\s.*a snapshot of snapsrc", out, re.M) is not None
              and re.search(r"^\s+sclone\s.*a clone of snap1", out, re.M) is not None, out)
        out = c.run("nbs io read sclone 0 8192")
        check("the clone read on c as its snapshot: its chunks the volume's on every cs, vouched for by the mds",
              "0x41x8192" in out, out)
        out = c.run("nbs io write sclone 0 4096 0x43")
        check("the clone written on c: its chunk, shared, copied up first", "4096 bytes of 0x43 written" in out, out)
        out = c.run("nbs io read sclone 0 8192")
        check("the clone as written", "0x43x4096 0x41x4096" in out, out)
        out = b.run("nbs io read snap1 0 8192")
        check("and its snapshot as it was", "0x41x8192" in out, out)
        out = c.run("nbs io detach sclone")
        need("the clone let go", "sclone detached" in out, out)
        out = a.run("nbs volume freeze sclone")
        need("the clone made an image", "is an image now" in out, out)
        out = b.run("nbs io write sclone 0 4096 0x44")
        check("an image written refused: read-only for good", "read-only" in out, out)
        out = a.run("nbs volume clone sclone iclone")
        need("the image cloned, for a", "a clone of sclone" in out, out)
        out = a.run("nbs volume delete sclone")
        check("the image deleted under its clone", "sclone" in out and "deleted" in out, out)
        out = a.run("nbs io read iclone 0 8192")
        check("the clone reads as the image did", "0x43x4096 0x41x4096" in out, out)
        out = a.run("nbs io write iclone 0 4096 0x45")
        check("the clone written where the image's chunk is its alone now: in place",
              "4096 bytes of 0x45 written" in out, out)
        out = a.run("nbs io read iclone 0 8192")
        check("and read as written", "0x45x4096 0x41x4096" in out, out)
        out = a.run("nbs io detach iclone")
        need("the clone let go", "iclone detached" in out, out)
        for name in ("iclone", "snap1", "snapsrc"):
            out = a.run("nbs volume delete %s" % name)
            check("%s deleted" % name, name in out and "deleted" in out, out)

        out = a.run("nbs volume delete disk1")
        check("disk1 deleted", "deleted" in out, out)
        check("every cs gives its chunks and its fence back",
              ipct.wait_for(lambda: all((cs_of(x) or ((None, 1, 1, 1), []))[0][1:] == (0, 0, 0) for x in sh), CS_S, 2),
              [x.run("nbscs") for x in sh])
        check("their slots free again", all(d[3] == d[2] for x in sh for d in devices_of(x)),
              [x.run("nbscs") for x in sh])

        # c's cs unloaded: a client told at once that it is not there; loaded again, it takes its devices back
        c.run("rmmod nbs-cs")
        start = time.time()
        out = a.run("nbs io held disk2")
        took = time.time() - start
        check("c's cs unloaded: a request to it bounced and said at once (%.1f s)" % took,
              re.search(r"^\s+c\s+nbs\.cs@c is not there", out, re.M) is not None and took < 10, out)
        check("and the other hosts' answered all the same",
              all(re.search(r"^\s+%s\s+0 chunks of disk2 open:" % h, out, re.M) for h in ("a", "b")), out)
        c.run("insmod /nbs-cs.ko")
        check("c's cs loaded again: its devices opened as they were, serving",
              ipct.wait_for(lambda: serving(2, 2), CS_S, 1), c.run("nbscs"))

        # -- repair (N9): c lost and written past, then back -- the leader's
        # controller has its cs down, then up, joins it in its own place, and
        # its copy takes what it missed --
        out = a.run("nbs volume create rep 8")
        need("a volume to repair made", re.search(r"volume rep \(\d+\), 8 MiB", out) is not None, out)
        out = a.run("nbs io write rep 0 65536 0x31")
        need("written on a, to its three replicas", "65536 bytes of 0x31 written" in out and
             "to every replica in" in out, out)
        out = a.run("nbs io write rep %d 65536 0x33" % (CHUNK_MIB << 20))
        need("and its second chunk", "65536 bytes of 0x33 written" in out, out)
        # the sets moved on past those writes -- b's cs taken away and back --
        # so that c, lost next, was last active at a version after them: a
        # chunk written at the version a member was last active at may be one
        # it missed, a write on its way as it went, and is pulled whole; one
        # written before is one it held through, held to its source's by sums
        b.run("rmmod nbs-cs")
        need("b's cs gone: the leader's controller has it down",
             ipct.wait_for(lambda: sum((health(a) or (0,) * 7)[5:]) == 1, 90, 2), a.run("nbs status"))
        b.run("insmod /nbs-cs.ko")
        # c's cs, unloaded a while just now, may be joining again yet
        check("every chunk at three replicas", ipct.wait_for(lambda: whole(a), 240, 3), a.run("nbs status"))
        kill(2)
        others = [sh[0], sh[1]]
        check("c gone: the leader's controller has its cs down",
              ipct.wait_for(lambda: any(sum((health(x) or (0,) * 7)[5:]) == 1 for x in others), 90, 2),
              [x.run("nbs status") for x in others])
        check("and out, %d s on (out_after=): its places for others' to take -- none can, a set's three on three "
              "hosts" % OUT_S, ipct.wait_for(lambda: any((health(x) or (0,) * 7)[6] == 1 for x in others), 60, 2),
              [x.run("nbs status") for x in others])
        out = a.run("nbs io write rep 65536 65536 0x32")
        check("written past c, to a's and b's replicas", "65536 bytes of 0x32 written" in out, out)
        out = a.run("nbs io close rep")
        check("and closed there", "closed on its replicas" in out, out)
        need("c boots again", pt.wait_log(boot(2), "boot: complete", 300) and addressed(2))
        sh[2] = c = ipct.Shell(hub.shell(2))
        need("c back: three voters, one leader", ipct.wait_for(lambda: agreed(sh, 3), 90, 1))
        check("c's cs up, joined in its own place, its copy done: every chunk at three replicas, none being "
              "copied, no cs down", ipct.wait_for(lambda: whole(a), 240, 3), [x.run("nbs status") for x in sh])
        out = a.run("nbs io detach rep")
        check("a lets rep go", "rep detached" in out, out)
        out = c.run("nbs io read rep 0 131072")
        check("read on c, from its own replica: what was written while it was gone, which its join's copy took",
              "131072 bytes at 0 of rep, from c: 0x31x65536 0x32x65536" in out, out)
        out = c.run("nbs io read rep %d 65536" % (CHUNK_MIB << 20))
        check("and its second chunk, not written meanwhile, as it was",
              ("65536 bytes at %d of rep, from c: 0x33x65536" % (CHUNK_MIB << 20)) in out, out)
        # what c's copies did, each join's end in its log
        joined = re.findall(r"set \d+ joined: this cs is its member now -- (\d+) chunks copied, (\d+) of them found "
                            r"as the source's, (\d+) bytes pulled", c.run("dmesg 5000 joined:"))
        copied, kept, pulled = (sum(int(j[k]) for j in joined) for k in range(3))
        check("c's copies took only what it missed: the chunk written meanwhile pulled whole, the other found as "
              "its source's (%d chunks, %d found, %d bytes pulled, by %d joins)" % (copied, kept, pulled, len(joined)),
              copied == 2 and kept == 1 and pulled == CHUNK_MIB << 20, joined)
        out = a.run("nbs volume delete rep")
        check("rep deleted", "deleted" in out, out)

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
        out = sh[fi].run("insmod /nbs-mds.ko compact=%d scrub=%d out_after=%d" % (COMPACT, SCRUB_S, OUT_S))
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

        # -- a fourth host, and one of four lost for good (N9f): the leader's
        # controller has the lost host's cs down, then out, and d joins
        # every set it was in, in its place --
        di = len(HOSTS)
        need("d boots", pt.wait_log(boot(di), "boot: complete", 300) and addressed(di))
        sh.append(ipct.Shell(hub.shell(di)))
        d = sh[di]
        out = a.run("cluster invite %s %s %s" % (LATE, ip(di), key_of(d)))
        token = token_of(out)
        need("a invites d", token is not None, out)
        out = d.run("cluster join %s" % token)
        need("d joins the registry", "joined as node" in out, out)
        need("every host up to d, and d to every host",
             ipct.wait_for(lambda: all(ipct.peer_up(x, LATE) for x in sh[:di])
                           and all(ipct.peer_up(d, h) for h in HOSTS), 60, 1))
        name = leader_of(a)
        need("a leader agreed on", name in HOSTS, name)
        li = ALL.index(name)
        out = sh[li].run("nbs host add %s" % LATE)
        need("d added", re.search(r"d is member \d+", out) is not None, out)
        need("four members, four voters, one leader, as every host says", ipct.wait_for(lambda: agreed(sh, 4), 60, 1))
        out = d.run("nbs devices add %s" % ids[LATE][0])
        need("d's device in the metadata", "in the metadata" in out, out)
        need("d's cs takes its device in and serves", ipct.wait_for(lambda: serving(di, 1), CS_S, 1), d.run("nbscs"))
        out = sh[li].run("nbs io write api1 0 65536 0x61")
        need("api1 written, on a, b and c -- made before d", "65536 bytes of 0x61 written" in out, out)
        out = sh[li].run("nbs io write api1 %d 65536 0x62" % (CHUNK_MIB << 20))
        need("and its second chunk", "65536 bytes of 0x62 written" in out, out)
        out = sh[li].run("nbs io close api1")
        need("and closed", "closed on its replicas" in out, out)
        out = sh[li].run("nbs io detach api1")
        need("and let go", "api1 detached" in out, out)
        check("every chunk at three replicas", ipct.wait_for(lambda: whole(sh[li]), 240, 3), sh[li].run("nbs status"))
        xi = next(i for i in range(n) if i != li)
        gone_for_good = HOSTS[xi]
        kill(xi)
        live = [i for i in range(len(ALL)) if i != xi]

        def at_three(x):
            h = health(x)
            return h is not None and h[0] > 0 and h[1:5] == (0, 0, 0, 0) and h[5] == 0

        check("%s lost for good: its cs out, %d s on" % (gone_for_good, OUT_S),
              ipct.wait_for(lambda: (health(sh[li]) or (0,) * 7)[6] == 1, 90, 2), sh[li].run("nbs status"))
        check("d joined every set %s was in, in its place, its copies done: every chunk at three replicas, none "
              "being copied" % gone_for_good, ipct.wait_for(lambda: at_three(sh[li]), 300, 3),
              [sh[i].run("nbs status") for i in live])
        out = d.run("nbs io read api1 0 65536")
        check("api1 read on d, from its own replica", "65536 bytes at 0 of api1, from d: 0x61x65536" in out, out)
        out = d.run("nbs io read api1 %d 65536" % (CHUNK_MIB << 20))
        check("and its second chunk", ("65536 bytes at %d of api1, from d: 0x62x65536" % (CHUNK_MIB << 20)) in out,
              out)
        out = sh[li].run("nbs host remove %s" % gone_for_good)
        check("%s removed, for good" % gone_for_good, "removed, for good" in out, out)
        need("three members again, d among them, as every host alive says",
             ipct.wait_for(lambda: agreed([sh[i] for i in live], 3), 60, 1))

        # -- rot (N9e): a sector of a closed chunk spoiled under a running
        # host's cs, through QEMU's monitor -- spoiled while the host was off,
        # its member back's copy would mend it before the scrub looked; the
        # leader's scrub says it, and a person has the member copied again --
        name = leader_of(d)
        need("a leader agreed on", name in ALL, name)
        ld = sh[ALL.index(name)]
        rot = b"\x4e" * 65536
        out = ld.run("nbs io write more 0 65536 0x4e")
        need("more written, its chunk", "65536 bytes of 0x4e written" in out, out)
        out = ld.run("nbs io close more")
        need("and closed", "closed on its replicas" in out, out)
        out = ld.run("nbs io detach more")
        need("and let go", "more detached" in out, out)

        def found_at(i):
            """Where in host i's pool the chunk's bytes are, if once."""
            import mmap
            with open(pools[i], "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as data:
                at = data.find(rot)
                return at if at >= 0 and data.find(rot, at + 1) < 0 else None

        held = {i: found_at(i) for i in live}
        holders = [i for i in live if held[i] is not None]
        need("more's chunk on three hosts' pools, once on each", len(holders) == 3,
             {ALL[i]: held[i] for i in live})
        yi = next(i for i in holders if ALL[i] != name)
        spoiled = held[yi] + 8192
        ipct.hmp(qmp(yi), 'qemu-io more0 "write -P 0xb1 %d 4096"' % spoiled)
        with open(pools[yi], "rb") as f:
            f.seek(spoiled)
            need("a sector of it spoiled on %s, under its cs" % ALL[yi], f.read(4096) == b"\xb1" * 4096)
        finding = re.compile(r"nbs: scrub: chunk (\d+) of set (\d+) \(version (\d+)\): (\w+)'s copy is unlike the two "
                             r"others' -- `nbs replica bad (\d+) (\w+)` has it copied again")
        # The leader's scrub: whichever member leads by then -- a leader
        # may change under a loaded host, the finding then the new one's
        sld, out = said_by_one([sh[i] for i in live], "nbs status", lambda o: finding.search(o) is not None)
        m = finding.search(out)
        need("the leader's scrub says %s's copy unlike the two others', with the command" % ALL[yi],
             m is not None and m.group(4) == ALL[yi] and m.group(6) == ALL[yi], out)
        out = ld.run("nbs replica bad %s %s" % (m.group(5), ALL[yi]))
        check("%s's cs taken down in set %s, to join its place again" % (ALL[yi], m.group(5)),
              "taken down in set %s" % m.group(5) in out and "joins its own place again" in out, out)
        check("its copy done: every chunk at three replicas, none being copied",
              ipct.wait_for(lambda: at_three(ld), 240, 3), [sh[i].run("nbs status") for i in live])
        out = sh[yi].run("nbs io read more 0 65536")
        check("more read on %s, from its own replica, as written" % ALL[yi],
              ("65536 bytes at 0 of more, from %s: 0x4ex65536" % ALL[yi]) in out, out)
        with open(pools[yi], "rb") as f:
            f.seek(held[yi])
            check("and the sector spoiled on its pool as it was", f.read(65536) == rot)
        out = eventually(sld, "nbs status", lambda o: "copy was unlike the two others' -- the set is at version" in o)
        check("the finding shown as history: the set moved on", finding.search(out) is None
              and "copy was unlike the two others' -- the set is at version" in out, out)

        # -- a member removed, and refused when added again as it was --
        name = leader_of(d)
        gi = next(i for i in live if i < n and HOSTS[i] != name)
        gone = HOSTS[gi]
        out = sh[ALL.index(name)].run("nbs host remove %s" % gone)
        need("%s removed" % gone, "removed, for good" in out, out)
        others = [sh[i] for i in live if i != gi]
        need("two members, two voters, as the others say", ipct.wait_for(lambda: agreed(others, 2), 30, 1))
        out = sh[ALL.index(name)].run("nbs host add %s" % gone)
        check("%s, still its old member, refused: an incarnation removed is never taken again" % gone,
              "never taken again" in out, out)
        out = sh[ALL.index(name)].run("nbs cs")
        check("%s's chunk server kept, taking no new chunks" % gone,
              re.search(r"^\s+%s\s.*\(no member: takes no new chunks\)$" % gone, out, re.M) is not None, out)
        out = sh[ALL.index(name)].run("nbs volume create late 4")
        check("and so a volume refused: fewer than three hosts take new chunks",
              "fewer than three hosts take new chunks" in out, out)

        # -- the scrub (N9e): the leader has looked, and found nothing unlike --
        # the volumes left -- disk2, more, api1 -- were never written: more
        # written now, so that a chunk is held, on the two members of its
        # set left
        ld = sh[ALL.index(name)]
        out = ld.run("nbs io write more 0 65536 0x6b")
        need("more written, its chunk", "65536 bytes of 0x6b written" in out, out)
        out = ld.run("nbs io close more")
        need("and closed", "closed on its replicas" in out, out)
        out = ld.run("nbs io detach more")
        need("and let go", "more detached" in out, out)
        scrubbed = re.compile(r"the scrub, a chunk every \d+ s as this host's member leads: (\d+) alike, (\d+) held by "
                              r"none, \d+ let be, (\d+) unlike")
        out = eventually(ld, "nbs status",
                         lambda o: (lambda m: m is not None and int(m.group(1)) > 0 and int(m.group(2)) > 0)(
                             scrubbed.search(o)))
        m = scrubbed.search(out)
        check("the leader's scrub found the chunk written alike on its members, the others held by none, and no "
              "finding of now -- the rot's, shown as history, its one",
              m is not None and int(m.group(1)) > 0 and int(m.group(2)) > 0 and finding.search(out) is None, out)
    except Stop as e:
        print("nbs-test: stopped at \"%s\": the rest stands on it" % e, flush=True)
    finally:
        for i in range(len(ALL)):
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
