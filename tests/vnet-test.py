#!/usr/bin/env python3
"""vnet-test: the guests' networks and their firewalls on a running nos,
with probes where guests would be -- ports of the switch with no VM behind
them, which answer ARP, echo and UDP to port 7 and send as a guest's NIC
does (`hv probe`, plans/vnet.md).

Its first phase, V0a's: the firewall on one host's networks. One nos
(x86-64, under KVM where the host has it, else TCG), hv.ko loaded and no
guest; its /etc/rc makes three probes -- a and b on the network lab, judged
by the firewall lab, and c alone on default -- and the checks read what
each line printed:

- closed by default: a reaches nos -- the host's own address, which no
  firewall judges -- and not b, of its own network: no echo and no
  datagram answered, while lab has no rule;
- a rule opens it: `allow in from net`, and a and b reach each other both
  ways, echo and UDP; c, alone, reaches neither -- the switch's isolation,
  not the firewall -- and does reach nos;
- a deny outwards is answered: with `deny out udp 9`, a's datagrams to b's
  9 are refused at once -- ICMP communication administratively prohibited,
  from its gateway -- where a deny inwards is silence;
- a change cuts a flow: a's datagrams to b's 7 from one source port are
  answered while `allow in udp 7` is lab's one way in, and that rule gone,
  the next from the same port is not -- the flow judged again by the rules
  left, and cut; `hv fw` says so;
- what was kept out is recorded: `trace show fw_deny` has the denials --
  which way, why, the rule;
- `rmmod hv` with the probes up: their tasks stopped, nothing panicked.

Its phase for V0b, NAT one to one and a public address on the host's own
network: one nos on tests/hub.py's LAN -- its uplink, the hub its router
-- with the world's host past the router (198.51.100.7), and two probes, p
with a public address (203.0.113.9) and its firewall letting in UDP to 7,
q with none:

- p pings the world, and the world sees p's public address; q pings it,
  and the world sees nos's own -- the masquerade's;
- the world calls the public address: asked for by ARP, nos answers for it
  as that address; a datagram to its 7 reaches p -- past its firewall --
  and comes back from the public address; an echo does not, until the
  firewall lets ICMP in, and then it does;
- `nat` lists the address one to one; with p gone, nothing answers for it
  any more.

Its phase for V1b, the group and its state: three nos on tests/hub.py's
LAN, each with ipcnet.ko, cluster.ko and vnet.ko -- the registry made among
them first -- on either architecture, no guest and no hv:

- vnet made on a, b added from a and c from b, a member that does not lead:
  three members, three voters, one leader, as every host says;
- networks: one made; one refused for holding a host's own address, which
  the proposer looks at as the state cannot; one of an MTU and no way out;
  every host listing the same; the provider's own made through the bus;
- ports, made by a word of the shell's as hv will make them: a VM's NIC's
  port on a, asked again from b for another network and host -- the one
  there is, as it is, what differs said; an attach on a answered with its
  address, gateway and MTU, on b refused, the port being a's;
- a public address added, put on the port and refused removed while on it,
  and one that is a host's own address refused;
- the cluster's firewalls: one made, a rule added, a port given it, its
  deletion refused while the port has it, and its rules as every host says;
- all three killed and booted again at once: the state back on each;
- the leader killed: the other two elect one of themselves and commit, and
  the killed host back has what they made;
- a cut off by the hub, its port moved to b by the two others; healed, an
  attach on a refused and on b answered;
- c's vnet.ko unloaded and loaded again: its member back, the same state;
- a port on c, and c removed from the registry: `vnet alerts` says the
  port is on a node the registry has not, and c's member removed;
- nothing panicked.

Its phase for V2, the cluster's networks on one host: one nos (x86-64,
where hv is) on tests/hub.py's LAN, the world past its router, with
ipcnet.ko, cluster.ko, vnet.ko and hv.ko -- the registry of it alone, vnet
of it alone, networks lan (10.10.0.0/24, MTU 1400) and dev (no way out):

- probes a and b on lan -- b at the address asked for -- and c on dev, each
  a port of vnet's as a VM's NIC is, the three listed by vnet;
- a pings its gateway, answered by the switch; hv0; and the world through
  NAT, which sees nos's address; c's echoes to the world go nowhere;
- inside lan, closed by the cluster's default until `vnet fw default add
  allow in from net`, which the keeper puts in its place from the table:
  then a and b reach each other, and neither reaches c on dev;
- b's port deleted in vnet: the keeper shuts its switch port, and nothing
  reaches it;
- `hv create` on lan at an address: its port made there, its vm.toml its
  network alone; another VM at that address refused; `hv delete` deletes
  the port with the folder; vm.networks offers the cluster's networks;
- a VM of a folder on lan -- hvboot, no disk -- started: its vm.toml given
  no port of the switch, whose port is vnet's (one kept there refused its
  every start once), and deleted with its port;
- vnet unloaded under the probes: they go on, a new one refused; vnet back;
- the probes deleted with their ports; `rmmod hv`; nothing panicked.

Its phase for V3, the tunnel: three nos (x86-64) on tests/hub.py's LAN, each
with ipcnet.ko, cluster.ko, vnet.ko and hv.ko -- its keys given anew every
eight seconds, the one before kept four (`rekey=8 keylife=4`) -- the
registry and vnet among them, networks lan and dev, and probes on each
host's switch: pa, pb, pc on lan on a, b and c, qa and qc on dev on a and c:

- every host's tunnel has a key to each other host, and its next hop;
- across hosts, through the tunnel: pa reaches pb, pb pc, pc pa, qa qc --
  and pa reaches none of dev's; `hv tunnel` counts what went and came, and
  nothing forged;
- pings every two seconds for half a minute, across four changes of keys:
  every one answered;
- a narrow link between a and b -- nothing past 1450 bytes: small echoes
  go on, and a says the way to b does not carry the networks' MTU;
- c killed: a's pings to it unanswered, and a says c answers no echo; c
  booted again, a new probe there reached -- the keys of c's new hv had;
- hv.ko unloaded and loaded again on b: a new probe there reached;
- b's probe's port moved to c in vnet: b's keeper shuts it, and nothing
  reaches it at b or at c;
- nothing panicked.

Its phase for V4, public addresses across hosts: the three again, hv
loaded as it ships, probes pa, pb, pc on lan, one on each host, pb's and
pc's ports judged by a firewall that lets everything in:

- a public address routed to a and put on pb's port, on b: a serves it --
  `hv tunnel` says so -- the world's echo and datagram to it answered, a's
  NAT and the tunnel taking them to b; pb's pings to the world seen from
  the address, out through a; pc's, with none, from c's own address;
- the address onto pc's port, on c: a serves it for pc, the world reaches
  pc at it, and pb goes out from b's own address again;
- routed to b (`vnet ip move`): b serves it and a no longer, the world --
  asking ARP again -- reaches pc through b, and pc goes out from it;
- taken off: b serves nothing, pc goes out from c's own address, nothing
  answers for the address; `rmmod hv` on all three; nothing panicked.

    tests/vnet-test.py [--phase v0a|v0b|v1b|v2|v3|v4|all] [--arch aarch64|x86_64] [--tcg] [--deadline SECS] [--iso ISO]

V0b, V2 and V3, and V1b on x86-64, need an ISO whose command line has the
UDP shell -- `make nos-test.iso TEST_ISO_ARGS=udpshell=9100`, which --iso may
name. V1b runs on --arch (arm64 unless said: HVF on a Mac), the others on
x86-64.

Exit code 0: every check held; 1: one did not.
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

# V0b: the hub's ports and the UDP shell's, nos-test.iso's
SHELL_PORT = 9100
HUB_BASE = 47900
PUBLIC = "203.0.113.9"

# The last line of /etc/rc, and what says rc has run to it.
RC_LAST = "version"
RC_DONE = re.compile(r"(?m)^> version\n.+\n")

A, B, C = "10.0.100.2", "10.0.100.3", "10.0.100.4"
NOS = "10.0.100.1"
# The source port a's flow to b's echo is sent from, run after run.
FLOW_PORT = 40999

# What /etc/rc runs, in order: each line's output is read by the line.
RC = [
    "insmod /hv.ko",
    "trace on fw",
    "hv fw create lab",
    "hv probe add a port=0 network=lab firewall=lab",
    "hv probe add b port=1 network=lab firewall=lab",
    "hv probe add c port=2",
    "hv probe",
    # Closed by default
    "hv probe ping a %s" % NOS,
    "hv probe ping a %s" % B,
    "hv probe udp a %s 7" % B,
    # A rule opens it
    "hv fw lab add allow in from net",
    "hv probe ping a %s" % B,
    "hv probe ping b %s" % A,
    "hv probe udp a %s 7" % B,
    "hv probe ping c %s" % A,
    "hv probe ping c %s" % NOS,
    # A deny outwards, answered
    "hv fw lab add deny out udp 9 at=1",
    "hv probe udp a %s 9" % B,
    # A change cuts a flow: udp 7 lab's one way in, a flow through it, the
    # rule gone
    "hv fw lab add allow in udp 7 at=1",
    "hv fw lab del 3",
    "hv probe ping a %s" % B,
    "hv probe udp a %s 7 count=1 sport=%d" % (B, FLOW_PORT),
    "hv fw lab del 1",
    "hv probe udp a %s 7 count=1 sport=%d" % (B, FLOW_PORT),
    "hv fw lab",
    "hv fw",
    "trace show 50 fw_deny",
    "hv probe del c",
    "hv probe",
    "rmmod hv",
    "lsmod",
    RC_LAST,
]


def sections(txt):
    """What each /etc/rc line printed, in order: (line, output) pairs."""
    parts = re.split(r"(?m)^> (.*)$", txt)
    return [(parts[i].strip(), parts[i + 1]) for i in range(1, len(parts) - 1, 2)]


def run_said(out):
    """A probe run's line: (sent, answered, refused, unreachable), or None."""
    m = re.search(r"(\d+) (?:echoes|datagrams) sent, (\d+) answered, (\d+) refused, (\d+) unreachable", out)
    return tuple(int(g) for g in m.groups()) if m else None


def boot(args, tmp):
    """nos with hv.ko and /etc/rc on its root, run until rc's last line has
    printed: the QEMU process and the serial log."""
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc"))
    ko = pt.module_path("x86_64", "hv")
    if not os.path.exists(ko):
        sys.exit("no %s -- make first" % ko)
    shutil.copy(ko, rootdir)
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# tests/vnet-test.py\n" + "\n".join(RC) + "\n")
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", rootdir], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    log = os.path.join(tmp, "serial.log")
    kvm = os.path.exists("/dev/kvm") and not args.tcg
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "2",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", "virtio-rng-pci"]
    argv += ["-enable-kvm", "-cpu", "host"] if kvm else ["-cpu", "max"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0 = time.time()
    txt = ""
    while time.time() - t0 < args.deadline and p.poll() is None:
        txt = open(log, errors="replace").read() if os.path.exists(log) else ""
        if "PANIC:" in txt or RC_DONE.search(txt):
            break
        time.sleep(2)
    return p, log


def checks(txt):
    secs = sections(txt)
    seen = {}

    def out(line):
        """The output of `line`'s next run, in rc's order."""
        n = seen.get(line, 0)
        seen[line] = n + 1
        runs = [o for (cmd, o) in secs if cmd == line]
        return runs[n] if n < len(runs) else ""

    def run(line, what, want):
        o = out(line)
        got = run_said(o)
        pt.check(what, got is not None and want(got), (line, o.strip()[-400:]))

    added = [out("hv probe add a port=0 network=lab firewall=lab"), out("hv probe add b port=1 network=lab firewall=lab"),
             out("hv probe add c port=2")]
    pt.check("three probes made: a and b on lab, judged by lab, c alone on default",
             "on port 0, %s (lab), firewall lab" % A in added[0] and "on port 1, %s (lab), firewall lab" % B in added[1]
             and "on port 2, %s (alone), firewall default" % C in added[2], added)
    listed = out("hv probe")
    pt.check("hv probe lists them", all(n + ": port" in listed for n in "abc"), listed)

    run("hv probe ping a %s" % NOS, "a probe pings nos: the host's own address, judged by no firewall",
        lambda g: g == (3, 3, 0, 0))
    run("hv probe ping a %s" % B, "closed by default: a's echoes to b, of its own network, unanswered",
        lambda g: g[0] == 3 and g[1] == 0 and g[2] == 0)
    run("hv probe udp a %s 7" % B, "and its datagrams", lambda g: g[0] == 3 and g[1] == 0)

    opened = out("hv fw lab add allow in from net")
    pt.check("hv fw lab add allow in from net", "rule 1: allow in from net" in opened, opened)
    run("hv probe ping a %s" % B, "then a's echoes to b are answered", lambda g: g == (3, 3, 0, 0))
    run("hv probe ping b %s" % A, "and b's to a", lambda g: g == (3, 3, 0, 0))
    run("hv probe udp a %s 7" % B, "and a's datagrams echoed", lambda g: g == (3, 3, 0, 0))
    run("hv probe ping c %s" % A, "c, alone, does not reach a -- the switch's isolation, not the firewall",
        lambda g: g[0] == 3 and g[1] == 0)
    run("hv probe ping c %s" % NOS, "and does reach nos", lambda g: g == (3, 3, 0, 0))

    denied = out("hv fw lab add deny out udp 9 at=1")
    pt.check("hv fw lab add deny out udp 9 at=1: the first rule", "rule 1: deny out udp 9" in denied, denied)
    run("hv probe udp a %s 9" % B, "a deny outwards answered at once: communication administratively prohibited",
        lambda g: g[0] == 3 and g[1] == 0 and g[2] >= 1)

    pt.check("allow in udp 7 put first", "rule 1: allow in udp 7" in out("hv fw lab add allow in udp 7 at=1"))
    taken = out("hv fw lab del 3")
    pt.check("allow in from net taken out", "rule 3 taken out: allow in from net" in taken, taken)
    run("hv probe ping a %s" % B, "a's echoes to b unanswered again: udp 7 lab's one way in",
        lambda g: g[0] == 3 and g[1] == 0)
    flow = "hv probe udp a %s 7 count=1 sport=%d" % (B, FLOW_PORT)
    run(flow, "a flow from a to b's 7, its datagram echoed", lambda g: g == (1, 1, 0, 0))
    cut = out("hv fw lab del 1")
    pt.check("that rule taken out, and the flow it let through cut, said",
             "rule 1 taken out: allow in udp 7" in cut and re.search(r"\d+ open flows? of its VMs' cut", cut), cut)
    run(flow, "the next datagram of the same flow is not answered: the flow went with its rule",
        lambda g: g[0] == 1 and g[1] == 0)

    shown = out("hv fw lab")
    # Each rule's and policy's flows are counted since the firewall last
    # changed: after the last change, the datagram the cut flow no longer
    # carried, kept out by the inbound policy.
    pt.check("hv fw lab: its one rule left, and the flows its inbound policy kept out since",
             re.search(r"1\s+deny out udp 9\s+\d+ flows?", shown) is not None
             and re.search(r"policies: in deny -- [1-9]\d* flows?", shown) is not None, shown)
    every = out("hv fw")
    pt.check("hv fw lists default and lab", "default:" in every and "lab:" in every, every)
    trace = out("trace show 50 fw_deny")
    pt.check("each packet kept out recorded as fw_deny: in by the policy, out by a rule",
             re.search(r"fw_deny: port=1 place=\d+ dir=in why=policy", trace) is not None
             and re.search(r"fw_deny: port=0 place=\d+ dir=out why=rule rule=1 proto=17 peer=%s lport=\d+ rport=9"
                           % re.escape(B), trace) is not None, trace[-1500:])
    gone = out("hv probe del c")
    pt.check("hv probe del c: its port given back", "c gone, port 2 given back" in gone, gone)
    left = out("hv probe")
    pt.check("and the list has a and b alone", "a: port" in left and "b: port" in left and "c: port" not in left, left)
    unloaded = out("rmmod hv")
    listed = out("lsmod")
    pt.check("rmmod hv with probes up: their tasks stopped, the module unloaded",
             "module: hv unloaded" in unloaded + listed and "no modules loaded" in listed, unloaded + listed)


def phase_v0a(args, tmp):
    p = None
    log = os.path.join(tmp, "serial.log")
    try:
        p, log = boot(args, tmp)
        txt = open(log, errors="replace").read() if os.path.exists(log) else ""
        if pt.check("nos did not panic", "PANIC:" not in txt, txt[-3000:]) and \
                pt.check("/etc/rc ran to its end", RC_DONE.search(txt) is not None, txt[-2000:]):
            checks(txt)
    finally:
        if p is not None:
            pt.kill(p)
        keep_log(log, "v0a")


def keep_log(log, phase):
    """A failed phase's serial log, kept in out/."""
    if pt.failures and os.path.exists(log):
        kept = os.path.join(ROOT, "out", "vnet-test-%s-serial.log" % phase)
        os.makedirs(os.path.dirname(kept), exist_ok=True)
        shutil.copy(log, kept)
        print("vnet-test: the serial log is in %s" % kept)


def phase_v0b(args, tmp):
    """One nos on the hub, the world past its router: a public address one
    to one, and the masquerade beside it."""
    if not os.path.exists(args.iso):
        pt.check("an ISO with the UDP shell: make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % SHELL_PORT, False,
                 args.iso)
        return
    rootdir = os.path.join(tmp, "root-v0b")
    os.makedirs(os.path.join(rootdir, "etc"))
    shutil.copy(pt.module_path("x86_64", "hv"), rootdir)
    image = os.path.join(tmp, "root-v0b.img")
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", rootdir], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    log = os.path.join(tmp, "serial-v0b.log")
    hub = hubmod.Hub(1, HUB_BASE, shell_port=SHELL_PORT).start()
    accel = "kvm" if os.path.exists("/dev/kvm") and not args.tcg else "tcg"
    p = subprocess.Popen(ipct.qemu("x86_64", accel, image, log, hub, 0, args.iso), cwd=ROOT,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("V0b: nos boots on the hub", pt.wait_log(log, "boot: complete", 400)):
            return
        if not pt.check("and asks the hub for its address", ipct.wait_for(lambda: 0 in hub.acked, 90)):
            return
        time.sleep(1)
        sh = ipct.Shell(hub.shell(0))
        sh.run("insmod /hv.ko")
        sh.run("hv fw create pub")
        sh.run("hv fw pub add allow in udp 7")
        made = sh.run("hv probe add p port=0 firewall=pub public=%s" % PUBLIC)
        pt.check("p made with its public address", "public %s" % PUBLIC in made, made)
        made = sh.run("hv probe add q port=1")
        pt.check("q made with none", "q on port 1" in made, made)
        listed = sh.run("nat")
        pt.check("nat lists the address one to one", "%s one to one to 10.0.100.2" % PUBLIC in listed, listed)

        run = run_said(sh.run("hv probe ping p %s count=3 secs=5" % hubmod.WORLD_IP))
        pt.check("p pings the world", run is not None and run[1] == 3, run)
        with hub.lock:
            seen = list(hub.world_seen)
        pt.check("and the world sees p's public address", (PUBLIC, "icmp") in seen, seen[-6:])
        run = run_said(sh.run("hv probe ping q %s count=3 secs=5" % hubmod.WORLD_IP))
        pt.check("q pings the world", run is not None and run[1] == 3, run)
        with hub.lock:
            seen = list(hub.world_seen)
        pt.check("and the world sees nos's own address, the masquerade's",
                 (hubmod.Hub.ip(0), "icmp") in seen, seen[-6:])

        pt.check("the world calls the public address: a datagram to its 7 reaches p and comes back from it",
                 hub.world_call(PUBLIC, "udp", 15))
        pt.check("nos answered ARP for it as that address", PUBLIC in hub.arp_known, hub.arp_known)
        pt.check("an echo does not get through p's firewall", not hub.world_call(PUBLIC, "icmp", 4))
        sh.run("hv fw pub add allow in icmp")
        pt.check("until the firewall lets ICMP in", hub.world_call(PUBLIC, "icmp", 10))
        counts = sh.run("nat")
        m = re.search(r"one to one: out (\d+)  in (\d+)", counts)
        pt.check("nat counts what went one to one, both ways", m is not None and int(m.group(1)) >= 5
                 and int(m.group(2)) >= 5, counts)

        sh.run("hv probe del p")
        hub.forget_arp(PUBLIC)
        pt.check("p gone, nothing answers for its address", not hub.world_call(PUBLIC, "udp", 4))
        pt.check("nor ARP for it", PUBLIC not in hub.arp_known, hub.arp_known)
        listed = sh.run("nat")
        pt.check("and nat lists it no longer", "one to one to" not in listed, listed)
        sh.run("rmmod hv")
        txt = open(log, errors="replace").read()
        pt.check("V0b: nothing panicked", "PANIC" not in txt, txt[-2000:])
    finally:
        pt.kill(p)
        hub.close()
        keep_log(log, "v0b")


V1B_HOSTS = ["a", "b", "c"]
V1B_HUB = 48100
U1 = "11111111-2222-3333-4444-555555555555"
U2 = "66666666-7777-8888-9999-000000000000"
V1B_PUBLIC = "198.51.100.50"


def key_of(sh):
    m = re.search(r"\b([0-9a-f]{64})\b", sh.run("ipcnet id"))
    return m.group(1) if m else None


def json_of(out):
    m = re.search(r"(\{.*\})", out or "", re.S)
    try:
        return json.loads(m.group(1)) if m else None
    except ValueError:
        return None


def vnet_status(sh):
    """`vnet` as (members, voters, leader) -- None for a host with no member."""
    m = re.search(r"^vnet: \S+ \(\w+\) -- member \d+, \w+, term \d+, leader (\S+), applied \d+, (\d+) members, "
                  r"(\d+) voters", sh.run("vnet"), re.M)
    if not m:
        return None
    return int(m.group(2)), int(m.group(3)), None if m.group(1) == "none" else m.group(1)


def vnet_agreed(shells, count):
    st = [vnet_status(s) for s in shells]
    return all(x is not None and x[0] == count and x[1] == count and x[2] is not None for x in st) \
        and len({x[2] for x in st}) == 1


def networks_of(sh):
    return sorted(re.findall(r"^  (\S+)\s+\d+\s+\S+/\d+", sh.run("vnet list"), re.M))


def v1b_rootfs(tmp, arch, name):
    rootdir = os.path.join(tmp, "root-v1b-" + name)
    os.makedirs(os.path.join(rootdir, "etc", "ipc"))
    for m in ("ipcnet", "cluster", "vnet"):
        shutil.copy(pt.module_path(arch, m), os.path.join(rootdir, m + ".ko"))
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# vnet-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, os.urandom(16).hex()))
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# vnet-test.py\ninsmod /ipcnet.ko\nipcnet start\nipc echo start\ninsmod /cluster.ko\ninsmod /vnet.ko\n")
    image = os.path.join(tmp, "root-v1b-%s.img" % name)
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def phase_v1b(args, tmp):
    """Three nos on the hub with the registry among them: vnet's group and
    its state -- networks, ports, public addresses, firewalls -- through
    reboots, a leader killed, a host cut off, a module reloaded."""
    arch = args.arch
    if arch == "x86_64" and not os.path.exists(args.iso):
        pt.check("an ISO with the UDP shell: make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % SHELL_PORT, False,
                 args.iso)
        return
    accel = ipct.accel_of(arch)
    images = [v1b_rootfs(tmp, arch, h) for h in V1B_HOSTS]
    n = len(V1B_HOSTS)
    hub = hubmod.Hub(n, V1B_HUB, shell_port=SHELL_PORT).start()
    procs, logs = [None] * n, []
    ip = hubmod.Hub.ip

    def boot(i):
        log = os.path.join(tmp, "serial-v1b-%s-%d.log" % (V1B_HOSTS[i], len(logs)))
        logs.append(log)
        procs[i] = subprocess.Popen(ipct.qemu(arch, accel, images[i], log, hub, i, args.iso), cwd=ROOT,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return log

    def kill(i):
        if procs[i]:
            pt.kill(procs[i])
            procs[i] = None
        hub.acked.discard(i)

    def up(i, log):
        return pt.wait_log(log, "boot: complete", 300) and ipct.wait_for(lambda: i in hub.acked, 90)

    def need(name, ok, detail=""):
        if not pt.check(name, ok, detail):
            raise RuntimeError(name)

    try:
        started = [boot(i) for i in range(n)]
        for i in range(n):
            need("V1b: %s boots and is addressed" % V1B_HOSTS[i], up(i, started[i]))
        time.sleep(1)
        sh = [ipct.Shell(hub.shell(i)) for i in range(n)]
        a, b, c = sh
        # -- the registry, as cluster-test makes it --
        need("a makes the registry", "made" in a.run("cluster init test"))
        for i in (1, 2):
            out = a.run("cluster invite %s %s %s" % (V1B_HOSTS[i], ip(i), key_of(sh[i])))
            m = re.search(r"cluster join (nos1-[0-9a-f]+)", out)
            need("a invites %s" % V1B_HOSTS[i], m is not None, out)
            out = sh[i].run("cluster join %s" % m.group(1))
            need("%s joins the registry" % V1B_HOSTS[i], "joined as node" in out, out)
        need("every host up to every other",
             ipct.wait_for(lambda: all(ipct.peer_up(sh[x], V1B_HOSTS[y]) for x in range(n) for y in range(n) if x != y),
                           60, 1))

        # -- the group --
        out = a.run("vnet")
        pt.check("a, in no vnet yet, says so", "in no vnet" in out, out)
        out = a.run("vnet init lab")
        need("a makes vnet lab", "vnet: lab made" in out, out)
        out = a.run("vnet host add b")
        need("b added from a", re.search(r"b is member \d+", out) is not None, out)
        need("a and b agree", ipct.wait_for(lambda: vnet_agreed([a, b], 2), 30, 1))
        out = b.run("vnet host add c")
        need("c added from b, a member that does not lead", re.search(r"c is member \d+", out) is not None, out)
        need("three members, three voters, one leader, as every host says", ipct.wait_for(lambda: vnet_agreed(sh, 3),
                                                                                          60, 1))

        # -- networks --
        out = a.run("vnet create lan 10.10.0.0/24")
        need("lan made", "network lan (1), 10.10.0.0/24, gateway 10.10.0.1" in out, out)
        out = a.run("vnet create over 10.9.0.0/16")
        pt.check("a network holding a host's own address refused", "holds 10.9.0." in out and "own address" in out, out)
        out = b.run("vnet create dmz 10.20.0.0/24 mtu=1400 out=no")
        pt.check("dmz made, its MTU and no way out", "MTU 1400, no way out" in out, out)
        out = c.run('mgmt call vnet network_create {"name":"api","subnet":"10.30.0.0/24"}')
        js = json_of(out)
        pt.check("api made through the bus", js is not None and js.get("name") == "api" and js.get("vni") == 3, out)
        want = ["api", "dmz", "lan"]
        pt.check("every host lists the same networks",
                 ipct.wait_for(lambda: all(networks_of(x) == want for x in sh), 30, 1), [networks_of(x) for x in sh])
        js = json_of(b.run("mgmt call vnet networks"))
        pt.check("the provider says them as the command does",
                 js is not None and sorted(x["name"] for x in js.get("networks", [])) == want, js)

        # -- ports --
        out = a.run("vnet port create lan %s" % U1)
        need("U1's port made on a, at lan's first address", "port 1 made" in out and "10.10.0.2" in out, out)
        out = b.run("vnet port create dmz %s host=b" % U1)
        pt.check("asked again from b, for another network and host: the one there is, what differs said",
                 "there already, as it is" in out and "its network, its host" in out, out)
        out = a.run("vnet port 1")
        pt.check("an attach on a answered: address, gateway, MTU",
                 "attached here: 10.10.0.2/24 gateway 10.10.0.1 MTU 1420" in out, out)
        out = b.run("vnet port 1")
        pt.check("an attach on b refused: the port is a's", "refused: the port is a's" in out, out)

        # -- public addresses --
        out = a.run("vnet ip add %s a" % V1B_PUBLIC)
        pt.check("a public address, routed to a", "routed to a" in out, out)
        out = a.run("vnet ip set %s 1" % V1B_PUBLIC)
        pt.check("put on port 1", "on port 1" in out, out)
        out = a.run("vnet ip remove %s" % V1B_PUBLIC)
        pt.check("its removal refused while on the port", "on a port" in out, out)
        out = a.run("vnet ip add %s a" % ip(1))
        pt.check("b's own address refused as a public one", "b's own address" in out, out)

        # -- the cluster's firewalls --
        out = a.run("vnet fw create edge in=deny")
        pt.check("edge made", "edge made: in deny, out allow" in out, out)
        out = a.run("vnet fw edge add allow in tcp 22")
        pt.check("a rule added", "rule 1: allow in tcp 22" in out, out)
        out = a.run("vnet port firewall 1 edge")
        pt.check("port 1 given edge", "judged by edge" in out, out)
        out = a.run("vnet fw delete edge")
        pt.check("edge's deletion refused while port 1 has it", "ports have the firewall" in out, out)
        pt.check("edge's rule as c says it", ipct.wait_for(lambda: "1  allow in tcp 22" in c.run("vnet fw edge"), 20, 1))
        js = json_of(c.run("mgmt call vnet rules"))
        pt.check("the provider's rules: a row a rule, its firewall and number its id -- the panel's table",
                 js is not None and {"id": "edge/1", "firewall": "edge", "n": 1, "rule": "allow in tcp 22"}
                 in js.get("rules", []), js)

        # -- all three killed and booted again at once --
        for i in range(n):
            kill(i)
        relogs = [boot(i) for i in range(n)]
        for i in range(n):
            need("%s boots again" % V1B_HOSTS[i], up(i, relogs[i]))
        time.sleep(1)
        sh = [ipct.Shell(hub.shell(i)) for i in range(n)]
        a, b, c = sh
        need("back: three members, a leader agreed on", ipct.wait_for(lambda: vnet_agreed(sh, 3), 90, 1))
        pt.check("every host has its networks back",
                 ipct.wait_for(lambda: all(networks_of(x) == want for x in sh), 30, 1))
        pt.check("and port 1 on a, edge's, with its public address",
                 re.search(r"^\s+1\s+lan\s+10\.10\.0\.2\s+a\s+gen \d+\s+fw edge .* public %s" % re.escape(V1B_PUBLIC),
                           c.run("vnet ports"), re.M) is not None, c.run("vnet ports"))

        # -- the leader killed: the other two go on --
        st = vnet_status(a)
        leader = st[2] if st else None
        need("a leader known", leader in V1B_HOSTS, st)
        li = V1B_HOSTS.index(leader)
        kill(li)
        rest = [i for i in range(n) if i != li]
        need("the other two elect one of themselves",
             ipct.wait_for(lambda: all((vnet_status(sh[i]) or (0, 0, None))[2] in [V1B_HOSTS[j] for j in rest]
                                       for i in rest), 60, 1))
        out = sh[rest[0]].run("vnet create after 10.40.0.0/24")
        pt.check("and commit", "network after" in out, out)
        log = boot(li)
        need("%s, killed, boots again" % leader, up(li, log))
        time.sleep(1)
        sh[li] = ipct.Shell(hub.shell(li))
        a, b, c = sh
        pt.check("%s has what the others made meanwhile" % leader,
                 ipct.wait_for(lambda: "after" in networks_of(sh[li]), 60, 1), networks_of(sh[li]))
        need("three again, a leader agreed on", ipct.wait_for(lambda: vnet_agreed(sh, 3), 60, 1))

        # -- a cut off, its port moved to b --
        hub.cut(0, 1)
        hub.cut(0, 2)
        need("b and c agree on a leader of the two",
             ipct.wait_for(lambda: (vnet_status(b) or (0, 0, None))[2] in ("b", "c")
                           and (vnet_status(c) or (0, 0, None))[2] == (vnet_status(b) or (0, 0, None))[2], 60, 1))
        out = b.run("vnet port move 1 b")
        pt.check("port 1 moved to b while a is cut off", "port 1 is b's now" in out, out)
        hub.heal(0, 1)
        hub.heal(0, 2)
        pt.check("healed, an attach on a refused: the port is b's",
                 ipct.wait_for(lambda: "refused: the port is b's" in a.run("vnet port 1"), 60, 1), a.run("vnet port 1"))
        pt.check("and on b answered", "attached here: 10.10.0.2/24" in b.run("vnet port 1"), b.run("vnet port 1"))

        # -- c's vnet.ko unloaded and loaded again --
        c.run("rmmod vnet")
        out = c.run("vnet")
        pt.check("vnet gone from c: no member to say", "vnet: lab" not in out, out)
        c.run("insmod /vnet.ko")
        pt.check("c's member back, three members", ipct.wait_for(lambda: vnet_agreed(sh, 3), 60, 1))
        pt.check("with the same ports", "10.10.0.2" in c.run("vnet ports"), c.run("vnet ports"))

        # -- a port on c, and c removed from the registry --
        out = a.run("vnet port create lan %s host=c" % U2)
        pt.check("U2's port made on c", "made" in out and "10.10.0.3" in out, out)
        out = a.run("cluster remove c")
        pt.check("c removed from the registry", "removed, for good" in out, out)
        pt.check("vnet alerts says the port is on a node the registry has not",
                 ipct.wait_for(lambda: "which the registry has not" in a.run("vnet alerts"), 30, 1),
                 a.run("vnet alerts"))
        out = a.run("vnet host remove c")
        pt.check("c's member removed", "removed, for good" in out, out)
        pt.check("two members, two voters", ipct.wait_for(lambda: vnet_agreed([a, b], 2), 30, 1))
    except RuntimeError as e:
        print("vnet-test: V1b stopped at \"%s\": the rest stands on it" % e, flush=True)
    finally:
        for i in range(n):
            kill(i)
        hub.close()
        text = "".join(open(l, errors="replace").read() for l in logs if os.path.exists(l))
        pt.check("V1b: nothing panicked", "PANIC" not in text)
        for l in logs:
            keep_log(l, "v1b-" + os.path.basename(l)[:-4])


V2_HUB = 48300
V2_A, V2_B, V2_C = "10.10.0.2", "10.10.0.9", "10.20.0.2"
V2_VM_AT = "10.10.0.50"


def ports_of(sh):
    """`vnet ports` as (id, network, address) triples."""
    return re.findall(r"^\s+(\d+)\s+(\S+)\s+(\d+\.\d+\.\d+\.\d+)\s", sh.run("vnet ports"), re.M)


def phase_v2(args, tmp):
    """One nos (x86-64, where hv is) on the hub, the world past its router:
    the cluster's networks on one host -- probes on them as a VM's NIC is,
    the switch's rules, the keeper's table, a VM's port made and deleted
    with its folder."""
    if not os.path.exists(args.iso):
        pt.check("an ISO with the UDP shell: make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % SHELL_PORT, False,
                 args.iso)
        return
    rootdir = os.path.join(tmp, "root-v2")
    os.makedirs(os.path.join(rootdir, "etc", "ipc"))
    for m in ("ipcnet", "cluster", "vnet", "hv"):
        shutil.copy(pt.module_path("x86_64", m), os.path.join(rootdir, m + ".ko"))
    # hvboot, a kernel a VM of a folder boots -- the build's, beside its modules
    os.makedirs(os.path.join(rootdir, "lib"))
    shutil.copy(os.path.join(os.path.dirname(pt.module_path("x86_64", "hv")), os.pardir, "hvboot"),
                os.path.join(rootdir, "lib", "hvboot"))
    with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
        f.write('# vnet-test.py\nname = "a"\ncluster = "%s"\npeers = []\n' % os.urandom(16).hex())
    with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
        f.write("# vnet-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /vnet.ko\n"
                "insmod /hv.ko\n")
    image = os.path.join(tmp, "root-v2.img")
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    log = os.path.join(tmp, "serial-v2.log")
    hub = hubmod.Hub(1, V2_HUB, shell_port=SHELL_PORT).start()
    accel = "kvm" if os.path.exists("/dev/kvm") and not args.tcg else "tcg"
    p = subprocess.Popen(ipct.qemu("x86_64", accel, image, log, hub, 0, args.iso), cwd=ROOT,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def need(name, ok, detail=""):
        if not pt.check(name, ok, detail):
            raise RuntimeError(name)

    def ping(sh, probe, to, count=3, secs=5):
        return run_said(sh.run("hv probe ping %s %s count=%d secs=%d" % (probe, to, count, secs)))

    try:
        need("V2: nos boots on the hub", pt.wait_log(log, "boot: complete", 400))
        need("and asks the hub for its address", ipct.wait_for(lambda: 0 in hub.acked, 90))
        time.sleep(1)
        sh = ipct.Shell(hub.shell(0))
        need("the registry of a alone", "made" in sh.run("cluster init test"))
        need("vnet v2 made on a", "vnet: v2 made" in sh.run("vnet init v2"))
        need("a leads it", ipct.wait_for(lambda: (vnet_status(sh) or (0, 0, None))[2] == "a", 30, 1))
        out = sh.run("vnet create lan 10.10.0.0/24 mtu=1400")
        need("lan made", "network lan (1), 10.10.0.0/24" in out, out)
        out = sh.run("vnet create dev 10.20.0.0/24 out=no")
        need("dev made, no way out", "no way out" in out, out)

        # -- probes on the cluster's networks, ports of vnet's --
        out = sh.run("hv probe add a network=vnet:lan")
        need("a on lan at its first address, judged by the cluster's default",
             "%s (vnet:lan), firewall default" % V2_A in out, out)
        out = sh.run("hv probe add b network=vnet:lan address=%s" % V2_B)
        need("b on lan at the address asked for", "%s (vnet:lan)" % V2_B in out, out)
        out = sh.run("hv probe add c network=vnet:dev")
        need("c on dev", "%s (vnet:dev)" % V2_C in out, out)
        ports = ports_of(sh)
        pt.check("vnet has their three ports, on their networks, at their addresses",
                 sorted((n, a) for _, n, a in ports) == sorted([("lan", V2_A), ("lan", V2_B), ("dev", V2_C)]), ports)

        # -- the gateway, the host, the world --
        run = ping(sh, "a", "10.10.0.1")
        pt.check("a pings its gateway: answered by the switch", run == (3, 3, 0, 0), run)
        run = ping(sh, "a", NOS)
        pt.check("and hv0, the host's own address", run == (3, 3, 0, 0), run)
        run = ping(sh, "a", hubmod.WORLD_IP)
        pt.check("and the world, through NAT -- lan goes out", run is not None and run[1] == 3, run)
        with hub.lock:
            seen = list(hub.world_seen)
        pt.check("which sees nos's own address, the masquerade's", (hubmod.Hub.ip(0), "icmp") in seen, seen[-6:])
        run = ping(sh, "c", hubmod.WORLD_IP, secs=3)
        pt.check("c's echoes to the world go nowhere -- dev does not go out", run is not None and run[1] == 0, run)
        run = ping(sh, "c", "10.20.0.1")
        pt.check("c pings its own gateway", run == (3, 3, 0, 0), run)

        # -- inside a network: closed by the cluster's default, opened by a rule --
        run = ping(sh, "a", V2_B, secs=3)
        pt.check("a's echoes to b, of its own network, unanswered: default lets nothing in",
                 run is not None and run[0] == 3 and run[1] == 0, run)
        out = sh.run("vnet fw default add allow in from net")
        pt.check("the cluster's default lets its network in", "rule 1: allow in from net" in out, out)
        pt.check("which the keeper puts in its place: a's echoes to b answered",
                 ipct.wait_for(lambda: (ping(sh, "a", V2_B, count=1, secs=2) or (0, 0))[1] == 1, 20, 1))
        run = ping(sh, "b", V2_A)
        pt.check("and b's to a", run == (3, 3, 0, 0), run)
        run = ping(sh, "a", V2_C, secs=3)
        pt.check("a does not reach c, on dev -- the cluster's networks are apart", run is not None and run[1] == 0,
                 run)
        listed = sh.run("hv list")
        # A probe asks no ARP: it makes its peers' MACs of their addresses
        pt.check("hv list counts the switch's answers on the cluster's networks: its gateways' echoes",
                 re.search(r"the cluster's networks: \d+ ARP questions answered by the switch, [1-9]\d* echoes",
                           listed) is not None, listed)

        # -- a port revoked: shut by the keeper --
        b_port = next((i for i, _, a in ports if a == V2_B), None)
        out = sh.run("vnet port delete %s" % b_port)
        pt.check("b's port deleted in vnet", "port %s deleted" % b_port in out, out)
        pt.check("the keeper shuts b's switch port",
                 ipct.wait_for(lambda: "(shut:" in sh.run("hv probe"), 20, 1), sh.run("hv probe"))
        run = ping(sh, "a", V2_B, secs=3)
        pt.check("and nothing reaches b any more", run is not None and run[1] == 0, run)
        said = sh.run("dmesg 2000 hv:")
        pt.check("the keeper says so in the log", "shut -- revoked by vnet" in said, said[-1500:])

        # -- a VM's port made with its folder and deleted with it --
        out = sh.run("hv create vx /no/bzImage net network=vnet:lan address=%s firewall=default" % V2_VM_AT)
        pt.check("hv create on lan: its port made at the address asked for",
                 re.search(r"vx given port \d+ on vnet:lan at %s" % re.escape(V2_VM_AT), out) is not None, out)
        js = json_of(sh.run('mgmt call vm config_get {"name":"vx"}'))
        text = (js or {}).get("text", "")
        pt.check("its vm.toml says its network and no more: no address, no firewall",
                 'network = "vnet:lan"' in text and V2_VM_AT not in text and "firewall" not in text, text[:600])
        pt.check("vnet has its port", any(a == V2_VM_AT for _, _, a in ports_of(sh)), ports_of(sh))
        out = sh.run("hv create vy /no/bzImage net network=vnet:lan address=%s" % V2_VM_AT)
        pt.check("another at the same address refused", "given port" not in out, out)
        out = sh.run("hv delete vx")
        pt.check("hv delete: its port deleted with its folder", re.search(r"its port \d+ of vnet's deleted", out)
                 is not None, out)
        pt.check("vnet has it no more", not any(a == V2_VM_AT for _, _, a in ports_of(sh)), ports_of(sh))
        js = json_of(sh.run("mgmt call vm networks"))
        names = sorted(n["name"] for n in (js or {}).get("networks", []))
        pt.check("vm.networks offers the cluster's networks to make a VM on",
                 "vnet:lan" in names and "vnet:dev" in names, js)

        # -- a VM of a folder on lan started: no port of the switch kept --
        sh.run("hv on")
        out = sh.run("hv create vz /lib/hvboot mem=64 net network=vnet:lan address=%s" % V2_VM_AT)
        pt.check("hv create of a VM to start on lan",
                 re.search(r"vz given port \d+ on vnet:lan at %s" % re.escape(V2_VM_AT), out) is not None, out)
        out = sh.run("hv start vz")
        pt.check("a VM of a folder on lan starts", re.search(r"vm \d+ \(vz\) started", out) is not None, out)
        js = json_of(sh.run('mgmt call vm config_get {"name":"vz"}'))
        text = (js or {}).get("text", "")
        pt.check("and its vm.toml is given no port of the switch: its port is vnet's",
                 'network = "vnet:lan"' in text and re.search(r"^port\s*=", text, re.M) is None, text[:600])
        sh.run("hv stop vz secs=1")
        out = sh.run("hv delete vz force")
        pt.check("hv delete vz: its port deleted with its folder",
                 re.search(r"its port \d+ of vnet's deleted", out) is not None, out)

        # -- vnet gone and back: the probes go on --
        out = sh.run("rmmod vnet")
        pt.check("vnet unloaded under the probes", "unloaded" in out, out)
        run = ping(sh, "a", "10.10.0.1", count=1)
        pt.check("a still reaches its gateway: hv goes on by what it had", run == (1, 1, 0, 0), run)
        out = sh.run("hv probe add d network=vnet:lan")
        pt.check("a probe on a cluster network refused while vnet is gone", "no vnet" in out, out)
        sh.run("insmod /vnet.ko")
        pt.check("vnet back, its member the leader again",
                 ipct.wait_for(lambda: (vnet_status(sh) or (0, 0, None))[2] == "a", 30, 1))

        # -- probes gone: their ports deleted --
        for probe in "abc":
            sh.run("hv probe del %s" % probe)
        pt.check("the probes gone, their ports deleted -- b's was already",
                 ipct.wait_for(lambda: ports_of(sh) == [], 10, 1), ports_of(sh))
        out = sh.run("rmmod hv")
        pt.check("rmmod hv: the keeper's task stopped, the module unloaded", "unloaded" in out, out)
        txt = open(log, errors="replace").read()
        pt.check("V2: nothing panicked", "PANIC" not in txt, txt[-2000:])
    except RuntimeError:
        pass
    finally:
        pt.kill(p)
        hub.close()
        keep_log(log, "v2")


V3_HUB = 48500
V3_HOSTS = ["a", "b", "c"]
PA, PB, PC = "10.10.0.11", "10.10.0.12", "10.10.0.13"
QA, QC = "10.20.0.11", "10.20.0.13"
PB2, PC2 = "10.10.0.22", "10.10.0.23"


def tunnel_rows(sh):
    """`hv tunnel`'s hosts: name -> (hop known, key to it had). Each is said
    at its address and port -- the tunnel's own, 39241, where the registry
    gives it none."""
    rows = re.findall(r"^\s+(\S+)\s+\d+\.\d+\.\d+\.\d+:\d+\s+hop (\w+)\s+key to it (\w+)", sh.run("hv tunnel"),
                      re.M)
    return {n: (hop == "known", key == "had") for n, hop, key in rows}


def tunnel_counts(sh):
    """`hv tunnel`'s counts: sent, received, forged."""
    out = sh.run("hv tunnel")
    m = re.search(r"sent (\d+), received (\d+);.*forged (\d+)", out, re.S)
    return tuple(int(g) for g in m.groups()) if m else None


def keyed(sh, others):
    rows = tunnel_rows(sh)
    return all(rows.get(o, (False, False)) == (True, True) for o in others)


class Three:
    """Three x86-64 nos on the hub, each with ipcnet.ko, cluster.ko, vnet.ko
    and hv.ko loaded with `hv_args` -- the registry among them, vnet `name`
    of the three, networks lan (10.10.0.0/24, MTU 1400) and dev (no way
    out), and the cluster's default letting its network in: what V3's and
    V4's phases stand on. `need` raises where the rest cannot go on."""

    def __init__(self, args, tmp, phase, hub_base, hv_args=""):
        self.args, self.tmp, self.phase = args, tmp, phase
        self.n = len(V3_HOSTS)
        self.images = []
        for name in V3_HOSTS:
            rootdir = os.path.join(tmp, "root-%s-%s" % (phase, name))
            os.makedirs(os.path.join(rootdir, "etc", "ipc"))
            for m in ("ipcnet", "cluster", "vnet", "hv"):
                shutil.copy(pt.module_path("x86_64", m), os.path.join(rootdir, m + ".ko"))
            with open(os.path.join(rootdir, "etc", "ipc", "hosts.toml"), "w") as f:
                f.write('# vnet-test.py\nname = "%s"\ncluster = "%s"\npeers = []\n' % (name, os.urandom(16).hex()))
            with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
                f.write("# vnet-test.py\ninsmod /ipcnet.ko\nipcnet start\ninsmod /cluster.ko\ninsmod /vnet.ko\n"
                        "insmod /hv.ko %s\n" % hv_args)
            image = os.path.join(tmp, "root-%s-%s.img" % (phase, name))
            subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT,
                           check=True, stdout=subprocess.DEVNULL)
            self.images.append(image)
        self.hub = hubmod.Hub(self.n, hub_base, shell_port=SHELL_PORT).start()
        self.accel = "kvm" if os.path.exists("/dev/kvm") and not args.tcg else "tcg"
        self.procs, self.logs = [None] * self.n, []
        self.sh = []

    def boot(self, i):
        log = os.path.join(self.tmp, "serial-%s-%s-%d.log" % (self.phase, V3_HOSTS[i], len(self.logs)))
        self.logs.append(log)
        self.procs[i] = subprocess.Popen(ipct.qemu("x86_64", self.accel, self.images[i], log, self.hub, i,
                                                   self.args.iso), cwd=ROOT,
                                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return log

    def kill(self, i):
        if self.procs[i]:
            pt.kill(self.procs[i])
            self.procs[i] = None
        self.hub.acked.discard(i)

    def up(self, i, log):
        return pt.wait_log(log, "boot: complete", 400) and ipct.wait_for(lambda: i in self.hub.acked, 120)

    @staticmethod
    def need(name, ok, detail=""):
        if not pt.check(name, ok, detail):
            raise RuntimeError(name)

    def start(self, vnet):
        """The three booted, the registry and vnet `vnet` among them, its
        networks made: the shells, a, b and c."""
        need, ip, n = self.need, hubmod.Hub.ip, self.n
        started = [self.boot(i) for i in range(n)]
        for i in range(n):
            need("%s: %s boots and is addressed" % (self.phase.upper(), V3_HOSTS[i]), self.up(i, started[i]))
        time.sleep(1)
        self.sh = [ipct.Shell(self.hub.shell(i)) for i in range(n)]
        sh = self.sh
        a = sh[0]
        need("a makes the registry", "made" in a.run("cluster init test"))
        for i in (1, 2):
            out = a.run("cluster invite %s %s %s" % (V3_HOSTS[i], ip(i), key_of(sh[i])))
            m = re.search(r"cluster join (nos1-[0-9a-f]+)", out)
            need("a invites %s" % V3_HOSTS[i], m is not None, out)
            out = sh[i].run("cluster join %s" % m.group(1))
            need("%s joins the registry" % V3_HOSTS[i], "joined as node" in out, out)
        need("every host up to every other",
             ipct.wait_for(lambda: all(ipct.peer_up(sh[x], V3_HOSTS[y]) for x in range(n) for y in range(n) if x != y),
                           90, 1))
        need("vnet %s made on a" % vnet, "vnet: %s made" % vnet in a.run("vnet init %s" % vnet))
        for h in ("b", "c"):
            out = a.run("vnet host add %s" % h)
            need("%s added to vnet" % h, re.search(r"%s is member \d+" % h, out) is not None, out)
        need("three members, a leader", ipct.wait_for(lambda: vnet_agreed(sh, 3), 90, 1))
        need("lan made", "network lan" in a.run("vnet create lan 10.10.0.0/24 mtu=1400"))
        need("dev made", "network dev" in a.run("vnet create dev 10.20.0.0/24 mtu=1400 out=no"))
        out = a.run("vnet fw default add allow in from net")
        need("the cluster's default lets its network in", "allow in from net" in out, out)
        return sh

    def finish(self):
        for i in range(self.n):
            self.kill(i)
        self.hub.close()
        text = "".join(open(l, errors="replace").read() for l in self.logs if os.path.exists(l))
        pt.check("%s: nothing panicked" % self.phase.upper(), "PANIC" not in text)
        for l in self.logs:
            keep_log(l, "%s-%s" % (self.phase, os.path.basename(l)[:-4]))


def probe_ping(sh, probe, to, count=3, secs=5):
    return run_said(sh.run("hv probe ping %s %s count=%d secs=%d" % (probe, to, count, secs)))


def probe_answered(sh, probe, to, count=3, secs=5):
    run = probe_ping(sh, probe, to, count, secs)
    return run is not None and run[1] == count


def phase_v3(args, tmp):
    """Three nos on the hub, hv on each: the cluster's networks across
    hosts, through the tunnel -- keys given round and anew, a narrow link, a
    host killed, hv reloaded, a port moved."""
    if not os.path.exists(args.iso):
        pt.check("an ISO with the UDP shell: make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % SHELL_PORT, False,
                 args.iso)
        return
    t = Three(args, tmp, "v3", V3_HUB, "rekey=8 keylife=4")
    n, hub, procs, need = t.n, t.hub, t.procs, t.need
    kill, up, boot_host = t.kill, t.up, t.boot
    ping, answered = probe_ping, probe_answered

    try:
        sh = t.start("v3")
        a, b, c = sh

        # -- probes on each host --
        for host, probe, net, at in ((a, "pa", "lan", PA), (b, "pb", "lan", PB), (c, "pc", "lan", PC),
                                     (a, "qa", "dev", QA), (c, "qc", "dev", QC)):
            out = host.run("hv probe add %s network=vnet:%s address=%s" % (probe, net, at))
            need("%s on %s at %s" % (probe, net, at), "%s (vnet:%s)" % (at, net) in out, out)

        # -- the keys given round --
        for i in range(n):
            others = [V3_HOSTS[j] for j in range(n) if j != i]
            pt.check("%s's tunnel has a key to %s and the hop to each" % (V3_HOSTS[i], " and ".join(others)),
                     ipct.wait_for(lambda: keyed(sh[i], others), 90, 1), sh[i].run("hv tunnel"))

        # -- across hosts, through the tunnel --
        pt.check("pa on a reaches pb on b", ipct.wait_for(lambda: answered(a, "pa", PB, 1, 3), 30, 1))
        run = ping(a, "pa", PB)
        pt.check("three of three", run == (3, 3, 0, 0), run)
        run = ping(b, "pb", PC)
        pt.check("pb on b reaches pc on c", run == (3, 3, 0, 0), run)
        run = ping(c, "pc", PA)
        pt.check("pc on c reaches pa on a", run == (3, 3, 0, 0), run)
        run = ping(a, "qa", QC)
        pt.check("qa reaches qc, dev's, across a and c", run == (3, 3, 0, 0), run)
        run = ping(a, "pa", QC, secs=3)
        pt.check("pa does not reach qc on dev: the networks are apart across hosts too",
                 run is not None and run[1] == 0, run)
        counts = tunnel_counts(a)
        pt.check("hv tunnel on a counts datagrams sent and taken, none forged",
                 counts is not None and counts[0] > 0 and counts[1] > 0 and counts[2] == 0, a.run("hv tunnel"))

        # -- keys changed and run out under traffic --
        runs = []
        for _ in range(15):
            runs.append(ping(a, "pa", PC, count=1, secs=2))
            time.sleep(2)
        pt.check("a ping every two seconds for half a minute, across four changes of keys: every one answered",
                 all(r == (1, 1, 0, 0) for r in runs), runs)

        # -- a narrow link between a and b --
        hub.impair(0, 1, max_frame=1450)
        run = ping(a, "pa", PB)
        pt.check("over a link that takes nothing past 1450 bytes small echoes go on", run == (3, 3, 0, 0), run)
        pt.check("and a says the way to b does not carry the networks' MTU",
                 ipct.wait_for(lambda: "the way to b does not carry a packet of 1400 bytes" in a.run("dmesg 4000 hv:"),
                               120, 3), a.run("dmesg 4000 hv:")[-1500:])
        hub.impair(0, 1)

        # -- c killed, and booted again --
        kill(2)
        run = ping(a, "pa", PC, secs=3)
        pt.check("c killed: a's pings to pc unanswered", run is not None and run[1] == 0, run)
        pt.check("a says c answers no echo",
                 ipct.wait_for(lambda: "c answered no echo" in a.run("dmesg 4000 hv:"), 90, 3),
                 a.run("dmesg 4000 hv:")[-1500:])
        log = boot_host(2)
        need("c boots again", up(2, log))
        time.sleep(1)
        sh[2] = ipct.Shell(hub.shell(2))
        c = sh[2]
        need("c's member back", ipct.wait_for(lambda: vnet_agreed(sh, 3), 120, 1))
        out = c.run("hv probe add pc2 network=vnet:lan address=%s" % PC2)
        need("a new probe on c", "%s (vnet:lan)" % PC2 in out, out)
        pt.check("a reaches it: the keys of c's new hv had",
                 ipct.wait_for(lambda: answered(a, "pa", PC2, 1, 3), 90, 2), a.run("hv tunnel"))

        # -- hv reloaded on b --
        out = b.run("rmmod hv")
        pt.check("hv unloaded on b", "unloaded" in out, out)
        b.run("insmod /hv.ko rekey=8 keylife=4")
        out = b.run("hv probe add pb2 network=vnet:lan address=%s" % PB2)
        need("a new probe on b", "%s (vnet:lan)" % PB2 in out, out)
        pt.check("a reaches it: b's new hv keyed",
                 ipct.wait_for(lambda: answered(a, "pa", PB2, 1, 3), 90, 2), a.run("hv tunnel"))

        # -- a port moved: shut where it was, nothing past it --
        port = next((i for i, _, addr in ports_of(a) if addr == PB2), None)
        out = a.run("vnet port move %s c" % port)
        pt.check("pb2's port moved to c", "c's now" in out, out)
        pt.check("b's keeper shuts pb2", ipct.wait_for(lambda: "(shut:" in b.run("hv probe"), 30, 1), b.run("hv probe"))
        run = ping(a, "pa", PB2, secs=3)
        pt.check("and nothing reaches pb2's address, at b or at c", run is not None and run[1] == 0, run)
        for i in range(n):
            if procs[i]:
                out = sh[i].run("rmmod hv")
                pt.check("rmmod hv on %s" % V3_HOSTS[i], "unloaded" in out, out)
    except RuntimeError as e:
        print("vnet-test: V3 stopped at \"%s\": the rest stands on it" % e, flush=True)
    finally:
        t.finish()


V4_HUB = 48700
PB_AT, PC_AT = "10.10.0.12", "10.10.0.13"


def phase_v4(args, tmp):
    """Three nos on the hub, hv on each: a public address routed to one host
    and on a port of another's -- the world reaching it, and it the world,
    through the tunnel; moved onto a third host's port, routed to another
    host, and taken off."""
    if not os.path.exists(args.iso):
        pt.check("an ISO with the UDP shell: make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % SHELL_PORT, False,
                 args.iso)
        return
    t = Three(args, tmp, "v4", V4_HUB)
    hub, need = t.hub, t.need
    world = hubmod.WORLD_IP

    def seen_from(addr, before):
        with hub.lock:
            return (addr, "icmp") in hub.world_seen[before:]

    def called(kind, secs, what):
        """The world's call of the public address, and where it does not
        go through, what each host says of it: NAT, the tunnel, the switch,
        the log, the tunnel's records."""
        for s in sh:
            s.run("trace on tunnel")
        ok = hub.world_call(PUBLIC, kind, secs)
        if not ok:
            said = []
            for i, s in enumerate(sh):
                for cmd in ("nat", "hv tunnel", "hv list", "dmesg 4000 nat", "dmesg 6000 hv:", "trace show 40 tunnel"):
                    said.append("%s $ %s\n%s" % (V3_HOSTS[i], cmd, s.run(cmd)))
            with hub.lock:
                said.append("the hub's world heard %s, saw %s" % (hub.world_heard[-6:], hub.world_seen[-6:]))
            print("\n".join(said), flush=True)
        pt.check(what, ok)
        return ok

    def seen_count():
        with hub.lock:
            return len(hub.world_seen)

    try:
        sh = t.start("v4")
        a, b, c = sh
        # -- probes, and a firewall that lets the world in --
        for host, probe, at in ((a, "pa", "10.10.0.11"), (b, "pb", PB_AT), (c, "pc", PC_AT)):
            out = host.run("hv probe add %s network=vnet:lan address=%s" % (probe, at))
            need("%s on lan at %s" % (probe, at), "%s (vnet:lan)" % at in out, out)
        need("a firewall that lets everything in", "made" in a.run("vnet fw create pub"))
        need("its rule", "allow in" in a.run("vnet fw pub add allow in"))
        ports = {addr: pid for pid, _, addr in ports_of(a)}
        for at in (PB_AT, PC_AT):
            out = a.run("vnet port firewall %s pub" % ports.get(at))
            need("%s's port judged by pub" % at, "judged by pub" in out, out)

        # -- a public address routed to a, on b's port --
        out = a.run("vnet ip add %s a" % PUBLIC)
        need("%s routed to a" % PUBLIC, "routed to a" in out, out)
        out = a.run("vnet ip set %s %s" % (PUBLIC, ports.get(PB_AT)))
        need("put on pb's port, on b", "on port %s" % ports.get(PB_AT) in out, out)
        pt.check("a serves it for pb, on b",
                 ipct.wait_for(lambda: "serves %s for %s" % (PUBLIC, PB_AT) in a.run("hv tunnel"), 30, 1),
                 a.run("hv tunnel"))
        need("every host's tunnel has a key to each other's",
             ipct.wait_for(lambda: all(keyed(sh[i], [V3_HOSTS[j] for j in range(t.n) if j != i])
                                       for i in range(t.n)), 90, 2), [s.run("hv tunnel") for s in sh])
        called("icmp", 20, "the world's echo to it answered: a's NAT, the tunnel to b, pb")
        called("udp", 10, "and its datagram to port 7")
        before = seen_count()
        run = probe_ping(b, "pb", world)
        pt.check("pb, on b, pings the world", run == (3, 3, 0, 0), run)
        pt.check("which sees it from its public address: out through a", seen_from(PUBLIC, before),
                 hub.world_seen[before:])
        before = seen_count()
        run = probe_ping(c, "pc", world)
        pt.check("pc, with none, pings the world", run == (3, 3, 0, 0), run)
        pt.check("which sees it from c's own address: masqueraded on its own host",
                 seen_from(hubmod.Hub.ip(2), before), hub.world_seen[before:])

        # -- onto c's port, the address still a's --
        out = a.run("vnet ip set %s %s" % (PUBLIC, ports.get(PC_AT)))
        pt.check("moved onto pc's port, on c", "on port %s" % ports.get(PC_AT) in out, out)
        pt.check("a serves it for pc now",
                 ipct.wait_for(lambda: "serves %s for %s" % (PUBLIC, PC_AT) in a.run("hv tunnel"), 30, 1),
                 a.run("hv tunnel"))
        called("icmp", 20, "the world reaches pc at it, through a")
        # b's keeper takes the table up to a turn after the others: until it
        # has, pb's way to the world is still through a, which serves pc now
        before = seen_count()
        went = ipct.wait_for(lambda: probe_answered(b, "pb", world), 30, 1)
        run = probe_ping(b, "pb", world)
        pt.check("pb goes out from b's own address again",
                 went and run == (3, 3, 0, 0) and seen_from(hubmod.Hub.ip(1), before)
                 and not seen_from(PUBLIC, before), (run, hub.world_seen[before:]))

        # -- routed to b now --
        out = a.run("vnet ip move %s b" % PUBLIC)
        pt.check("the address routed to b", "routed to b" in out, out)
        pt.check("b serves it for pc, and a no longer",
                 ipct.wait_for(lambda: "serves %s" % PUBLIC in b.run("hv tunnel")
                               and "serves %s" % PUBLIC not in a.run("hv tunnel"), 30, 1),
                 (a.run("hv tunnel"), b.run("hv tunnel")))
        hub.forget_arp(PUBLIC)
        called("icmp", 20, "the world, asking again, reaches pc through b")
        before = seen_count()
        run = probe_ping(c, "pc", world)
        pt.check("pc goes out from it, through b", run == (3, 3, 0, 0) and seen_from(PUBLIC, before),
                 (run, hub.world_seen[before:]))

        # -- taken off --
        out = a.run("vnet ip set %s none" % PUBLIC)
        pt.check("taken off pc's port", "%s on no port" % PUBLIC in out, out)
        pt.check("b serves nothing", ipct.wait_for(lambda: "serves %s" % PUBLIC not in b.run("hv tunnel"), 30, 1),
                 b.run("hv tunnel"))
        # c's keeper takes the table up to a turn after b's: until it has,
        # pc's way to the world is still through b, which no longer serves
        # it -- a probe sends its echoes at once, so the run is asked again
        before = seen_count()
        went = ipct.wait_for(lambda: probe_answered(c, "pc", world), 30, 1)
        run = probe_ping(c, "pc", world)
        pt.check("pc goes out from c's own address",
                 went and run == (3, 3, 0, 0) and seen_from(hubmod.Hub.ip(2), before)
                 and not seen_from(PUBLIC, before),
                 (run, hub.world_seen[before:]))
        hub.forget_arp(PUBLIC)
        pt.check("and nothing answers for the address", not hub.world_call(PUBLIC, "icmp", 5))
        for i in range(t.n):
            out = sh[i].run("rmmod hv")
            pt.check("rmmod hv on %s" % V3_HOSTS[i], "unloaded" in out, out)
    except RuntimeError as e:
        print("vnet-test: V4 stopped at \"%s\": the rest stands on it" % e, flush=True)
    finally:
        t.finish()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--phase", choices=("v0a", "v0b", "v1b", "v2", "v3", "v4", "all"), default="all")
    ap.add_argument("--arch", choices=("aarch64", "x86_64"), default="aarch64", help="V1b's")
    ap.add_argument("--tcg", action="store_true", help="TCG even where KVM is")
    ap.add_argument("--deadline", type=int, default=900, help="seconds for /etc/rc to run to its end")
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="V0b: an ISO whose command line has udpshell=%d" % SHELL_PORT)
    args = ap.parse_args()
    tmp = tempfile.mkdtemp(prefix="nos-vnet-")
    try:
        if args.phase in ("v0a", "all"):
            phase_v0a(args, tmp)
        if args.phase in ("v0b", "all"):
            phase_v0b(args, tmp)
        if args.phase in ("v1b", "all"):
            phase_v1b(args, tmp)
        if args.phase in ("v2", "all"):
            phase_v2(args, tmp)
        if args.phase in ("v3", "all"):
            phase_v3(args, tmp)
        if args.phase in ("v4", "all"):
            phase_v4(args, tmp)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print()
    print("vnet-test: " + ("OK" if not pt.failures else "FAILED: " + ", ".join(pt.failures)))
    return 0 if not pt.failures else 1


if __name__ == "__main__":
    sys.exit(main())
