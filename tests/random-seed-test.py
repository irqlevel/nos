#!/usr/bin/env python3
"""The random pool's seed kept on the root filesystem (/etc/random-seed,
src/crates/kernel/src/seed.rs, random::restore_seed): what seeds the pool on
a machine with no random instruction and a cycle counter too steady for the
jitter collector -- the ASUS RS500's Westmere, where without it sshd had
nothing to make a key with.

QEMU's TCG is such a machine: -cpu Westmere has no RDRAND, and TCG's cycle
counter gives the jitter collector no variance. The root is an IDE disk, as
the server's is. Four boots of the same ISO, each with a root of its own:

- a seed kept: the pool is seeded by it, says so, and the file is 64 new
  bytes when the machine is off -- never the seed it booted with, which the
  next boot would otherwise take as fresh a second time;
- none kept, nothing else: the pool stays unseeded, says why, `random`
  refuses, and no seed is made up for the next boot out of nothing;
- none kept, a virtio-rng: the pool is seeded by the device, and a seed is
  kept for the next boot;
- a file of the wrong size: not taken; with nothing else, left as it is.

    make nos-test.iso TEST_ISO_ARGS=udpshell=9100
    tests/random-seed-test.py [--iso nos-test.iso] [--keep]

x86-64 only. Exit code 0 = every check passed.
"""

import argparse
import importlib.util
import os
import re
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

spec = importlib.util.spec_from_file_location("pt", os.path.join(HERE, "parttest.py"))
pt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pt)

SEED_BYTES = 64
SEEDED = "Random: seeded from the seed the boot before kept, and a new one kept for the next"
NONE_UNSEEDED = "Random: no seed kept on the root filesystem, and nothing else seeded the pool"
NOT_TAKEN = "holds 10 bytes, not the 64 a seed is: not taken"


def make_root(tmp, name, seed):
    """A root of the label root=auto takes, its /etc/random-seed `seed`
    (none for None)."""
    stage = os.path.join(tmp, name + "-stage")
    os.makedirs(os.path.join(stage, "etc"), exist_ok=True)
    if seed is not None:
        with open(os.path.join(stage, "etc", "random-seed"), "wb") as f:
            f.write(seed)
    image = os.path.join(tmp, name + ".img")
    subprocess.run([os.path.join(ROOT, "scripts", "mkrootfs.sh"), image, "64", stage, "1024"],
                   cwd=ROOT, check=True, stdout=subprocess.DEVNULL, env=dict(os.environ, FSTYPE="ext4", LABEL="nos"))
    return image


def seed_of(tmp, image):
    """/etc/random-seed of a root image, as debugfs reads it: None when it
    has none."""
    out = os.path.join(tmp, "seed.out")
    if os.path.exists(out):
        os.remove(out)
    rel = os.path.relpath(image, tmp)
    subprocess.run(["docker", "run", "--platform", "linux/amd64", "--rm", "-v", tmp + ":/t", "nos-builder",
                    "debugfs", "-R", "dump /etc/random-seed /t/seed.out", "/t/" + rel],
                   capture_output=True)
    if not os.path.exists(out):
        return None
    with open(out, "rb") as f:
        return f.read()


def boot(args, tmp, name, image, rng):
    """The ISO on the CD, the root on the primary master, the shell over UDP:
    to the shell and back off. The serial log's text, and the shell's answers
    to `entropy` and `random`."""
    log = os.path.join(tmp, name + ".log")
    argv = ["qemu-system-x86_64", "-machine", "pc", "-display", "none",
            "-cpu", "Westmere,-x2apic,-tsc-deadline", "-smp", "2", "-m", "1G",
            "-serial", "file:" + log,
            "-drive", "if=none,id=root,format=raw,file=" + image,
            "-device", "ide-hd,drive=root,bus=ide.0,unit=0",
            "-drive", "if=none,id=cd,format=raw,media=cdrom,readonly=on,file=" + args.iso,
            "-device", "ide-cd,drive=cd,bus=ide.1,unit=0,bootindex=0",
            "-device", "virtio-net-pci,netdev=n0",
            "-netdev", "user,id=n0,hostfwd=udp::%d-:%d" % (pt.SHELL_PORT, pt.SHELL_PORT)]
    if rng:
        argv += ["-device", "virtio-rng-pci"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    entropy = random = ""
    try:
        if not pt.check("%s: boots to the shell" % name, pt.wait_log(log, "boot: complete", 600)):
            return open(log, errors="replace").read(), "", ""
        sh = pt.Shell(timeout=60)
        entropy = sh.run("entropy")
        random = sh.run("random 16")
        try:
            sh.sock.settimeout(5)
            sh.run("poweroff")
        except Exception:
            pass
        for _ in range(120):
            if p.poll() is not None:
                break
            time.sleep(1)
        pt.check("%s: powers itself off" % name, p.poll() is not None)
    finally:
        pt.kill(p)
    return open(log, errors="replace").read(), entropy, random


def jitter_seeded(text):
    """Whether the jitter collector seeded the pool on this host after all --
    then a boot with nothing else is seeded too, and what it says of the
    seed file is another case. The only source such a boot has is the
    jitter collector, so any reseed that took something from one is it."""
    if "Random: seeded from timing jitter" in text:
        return True
    return any(int(n) > 0 for n in re.findall(r"Random: reseeded from (\d+) of", text))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", default=os.path.join(ROOT, "nos-test.iso"),
                    help="an ISO whose command line has udpshell=%d" % pt.SHELL_PORT)
    ap.add_argument("--keep", action="store_true", help="leave the images and the logs behind")
    args = ap.parse_args()
    if not os.path.exists(args.iso):
        sys.exit("no %s -- make nos-test.iso TEST_ISO_ARGS=udpshell=%d" % (args.iso, pt.SHELL_PORT))

    tmp = tempfile.mkdtemp(prefix="nos-seedtest-")

    # A seed kept by the boot before
    kept = os.urandom(SEED_BYTES)
    image = make_root(tmp, "kept", kept)
    text, entropy, random = boot(args, tmp, "kept", image, rng=False)
    pt.check("kept: the pool is seeded by the seed", SEEDED in text,
             "\n".join(l for l in text.splitlines() if "Random:" in l))
    pt.check("kept: entropy says so", "seeded" in entropy and "UNSEEDED" not in entropy, entropy)
    pt.check("kept: random answers", len(random.strip()) == 32, random)
    after = seed_of(tmp, image)
    pt.check("kept: the file holds a seed of 64 bytes for the next boot", after is not None and len(after) == SEED_BYTES,
             repr(after)[:80])
    pt.check("kept: and not the one it booted with", after != kept)

    # None kept, and nothing else to seed the pool
    image = make_root(tmp, "none", None)
    text, entropy, random = boot(args, tmp, "none", image, rng=False)
    if jitter_seeded(text):
        print("  (the jitter collector seeded the pool on this host: the unseeded case is not this host's)")
    else:
        pt.check("none: says the pool has nothing", NONE_UNSEEDED in text,
                 "\n".join(l for l in text.splitlines() if "Random:" in l))
        pt.check("none: entropy says it is unseeded", "UNSEEDED" in entropy, entropy)
        pt.check("none: random refuses", "not seeded" in random, random)
        pt.check("none: no seed is made up for the next boot", seed_of(tmp, image) is None)

    # None kept, a virtio-rng
    image = make_root(tmp, "rng", None)
    text, entropy, random = boot(args, tmp, "rng", image, rng=True)
    pt.check("rng: the device seeds the pool", "seeded" in entropy and "UNSEEDED" not in entropy, entropy)
    made = seed_of(tmp, image)
    pt.check("rng: a seed is kept for the next boot", made is not None and len(made) == SEED_BYTES, repr(made)[:80])

    # A file of the wrong size, nothing else
    image = make_root(tmp, "short", b"0123456789")
    text, entropy, random = boot(args, tmp, "short", image, rng=False)
    pt.check("short: the file is not taken", NOT_TAKEN in text,
             "\n".join(l for l in text.splitlines() if "Random:" in l))
    if not jitter_seeded(text):
        pt.check("short: the pool stays unseeded", "UNSEEDED" in entropy, entropy)
        pt.check("short: and the file is left as it was", seed_of(tmp, image) == b"0123456789")

    if args.keep:
        print("images and logs in " + tmp)
    else:
        subprocess.run(["rm", "-rf", tmp])

    print()
    if pt.failures:
        print("FAILED: " + ", ".join(pt.failures))
        return 1
    print("random-seed-test: OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
