#!/usr/bin/env python3
"""memsites: Rust's heap in nos by where it was allocated, to the file and
the line (docs/memsites.md).

`memsites dump` in nos lists every allocation site that holds blocks on the
heap -- the stack that asked for them, as return addresses and the names the
kernel's symbol table gives them -- and this reads it: the sites merged by
the frame that is the code's own, past Rust's allocator shims and alloc's
and core's code, as the kernel's `memsites` merges them, and each frame
named to a file and a line by llvm-symbolizer over the kernel's ELF -- the
line in the code's own file, where alloc's code was inlined into it.

    ssh <host> memsites dump | scripts/memsites.py -     # through sshd
    scripts/memsites.py --udp 127.0.0.1:9000             # through the UDP shell
    scripts/memsites.py dump.txt --elf bin/kernel64.elf --top 30

The ELF is the one the kernel was built as -- bin/kernel64.elf for x86-64,
kernel-arm64.elf for arm64, told apart by the addresses unless --elf names
it -- with the lines of its Rust (the Makefile builds it so). A module's
frame is named by the module, not to a line. llvm-symbolizer is the host's,
or the nos-builder image's. Exit code 0 = read and named.
"""

import argparse
import importlib.util
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

# What the kernel's memsites::is_machinery takes for the allocation's own
# machinery rather than the code that asked: the same, prefix for prefix.
MACHINERY = ("__rustc::", "alloc::", "core::", "<alloc::", "<core::", "<[", "<str>", "<str as ",
             "<ffi::alloc::KernelAllocator")


def is_std(path):
    """A file of Rust's own library, as the line tables name it -- under the
    toolchain's sources, which the kernel's core and alloc are built from, or
    rustc's own: never the line of the code that asked."""
    return path.startswith("/rustc/") or "/rustlib/src/rust/library/" in path


def machinery(name):
    return name.startswith(MACHINERY) or " as alloc::" in name


def parse(text):
    """The dump's sites, each with its frames, and the Rust heap's totals."""
    sites, rust, cur = [], None, None
    for line in text.splitlines():
        words = line.split()
        if not words:
            continue
        if words[0] == "site" and len(words) == 5:
            cur = {"blocks": int(words[2]), "asked": int(words[3]), "taken": int(words[4]), "frames": []}
            sites.append(cur)
        elif words[0] == "frame" and cur is not None and len(words) >= 2:
            name = line.split(None, 2)[2] if len(words) > 2 else ""
            cur["frames"].append((int(words[1], 16), name.strip()))
        elif words[0] == "rust" and len(words) == 4:
            rust = {"blocks": int(words[1]), "asked": int(words[2]), "taken": int(words[3])}
    return sites, rust


def own(frames):
    """The position of the first frame that is the code's own."""
    for i, (_, name) in enumerate(frames):
        if not machinery(name):
            return i
    return max(len(frames) - 1, 0)


def merged(sites):
    """The sites merged by their own frame: totals summed, the frames of the
    one holding the most kept, and how many were merged."""
    groups = {}
    for s in sites:
        if not s["frames"]:
            continue
        at = own(s["frames"])
        key = s["frames"][at][0]
        g = groups.get(key)
        if g is None:
            groups[key] = g = {"blocks": 0, "asked": 0, "taken": 0, "paths": 0, "best": 0, "frames": s["frames"],
                               "at": at}
        g["blocks"] += s["blocks"]
        g["asked"] += s["asked"]
        g["taken"] += s["taken"]
        g["paths"] += 1
        if s["taken"] > g["best"]:
            g["best"], g["frames"], g["at"] = s["taken"], s["frames"], at
    return sorted(groups.values(), key=lambda g: -g["taken"])


def size_text(n):
    if n < 10 * 1024:
        return str(n)
    if n < 10 * 1024 * 1024:
        return "%d.%dK" % (n // 1024, n % 1024 * 10 // 1024)
    return "%d.%dM" % (n // (1 << 20), n % (1 << 20) * 10 // (1 << 20))


def default_elf(sites):
    """The ELF the addresses are of: arm64's kernel lives at 0xffff8000...,
    x86-64's at 0xffffffff8..."""
    for s in sites:
        for addr, _ in s["frames"]:
            if addr >> 40 == 0xffffff:
                return os.path.join(ROOT, "bin", "kernel64.elf")
            if addr >> 44 == 0xffff8:
                return os.path.join(ROOT, "kernel-arm64.elf")
    return None


def symbolizer(elf, addresses):
    """llvm-symbolizer's answer for each address: the inlined chain, the
    innermost first, as (function, file, line). The host's, or the
    nos-builder image's with the tree mounted."""
    query = "".join("0x%x\n" % a for a in addresses)
    local = shutil.which("llvm-symbolizer") or next(
        (p for p in ("/opt/homebrew/opt/llvm/bin/llvm-symbolizer", "/usr/local/opt/llvm/bin/llvm-symbolizer")
         if os.path.exists(p)), None)
    if local:
        argv = [local, "--obj=" + elf]
    elif shutil.which("docker"):
        rel = os.path.relpath(os.path.abspath(elf), ROOT)
        if rel.startswith(".."):
            sys.exit("memsites: %s is not under %s, which the nos-builder image is given" % (elf, ROOT))
        argv = ["docker", "run", "--platform", "linux/amd64", "--rm", "-i", "-v", "%s:/src" % ROOT, "-w", "/src",
                "nos-builder", "llvm-symbolizer", "--obj=/src/" + rel]
    else:
        sys.exit("memsites: no llvm-symbolizer here, and no docker for the nos-builder image's")
    r = subprocess.run(argv, input=query, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("memsites: llvm-symbolizer failed: %s" % r.stderr[-500:])
    answers, chain, lines = [], [], r.stdout.split("\n")
    i = 0
    while i < len(lines):
        if lines[i] == "":
            if chain:
                answers.append(chain)
                chain = []
            i += 1
            continue
        func = lines[i]
        where = lines[i + 1] if i + 1 < len(lines) else "??:0"
        path, _, rest = where.partition(":")
        line = rest.split(":")[0]
        chain.append((func, path, int(line) if line.isdigit() else 0))
        i += 2
    if chain:
        answers.append(chain)
    return answers


def tree_path(path):
    """A path the line tables have, as one in the tree: built in the
    nos-builder image at /src, or here."""
    for prefix in ("/src/", ROOT + "/"):
        if path.startswith(prefix):
            return path[len(prefix):]
    return path


def place(chain):
    """Where in the code's own files a frame is: the innermost of its inlined
    chain that is not Rust's library -- the line of the code that asked,
    where alloc's code was inlined into it -- and, when that line is in a
    function inlined into the frame's, where the frame's own function has it."""
    ours = [(path, line) for _, path, line in chain if path and line and not is_std(path)]
    if not ours:
        return None
    path, line = ours[0]
    text = "%s:%d" % (tree_path(path), line)
    if len(ours) > 1 and ours[-1] != ours[0]:
        outer_path, outer_line = ours[-1]
        text += " (inlined at %s:%d)" % (os.path.basename(outer_path), outer_line)
    return text


def fetch_udp(target):
    """`memsites dump` through the UDP shell: scripts/udpsh.py's client, its
    reply kept rather than printed."""
    spec = importlib.util.spec_from_file_location("udpsh", os.path.join(HERE, "udpsh.py"))
    udpsh = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(udpsh)
    host, _, port = target.partition(":")

    class Client(udpsh.UdpShellClient):
        reply = ""

        def _emit(self, chunks, last_idx):
            end = (last_idx if last_idx is not None else max(chunks)) + 1
            if any(i not in chunks for i in range(end)):
                sys.exit("memsites: the dump lost a datagram on its way: run it again")
            self.reply = b"".join(chunks[i] for i in range(end)).decode(errors="replace")

    c = Client(host, int(port) if port else 9000, timeout=30.0)
    c.connect()
    c.send("memsites dump")
    if not c.recv():
        sys.exit("memsites: no whole answer from %s" % target)
    return c.reply


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("dump", nargs="?", help="a file of `memsites dump`'s output, or - for stdin")
    ap.add_argument("--udp", metavar="HOST[:PORT]", help="fetch the dump through nos's UDP shell")
    ap.add_argument("--elf", help="the kernel's ELF the addresses are of")
    ap.add_argument("--top", type=int, default=20, help="how many sites to show (20)")
    ap.add_argument("--callers", type=int, default=2, help="the frames above each site to show (2)")
    args = ap.parse_args()

    if args.udp:
        text = fetch_udp(args.udp)
    elif args.dump == "-":
        text = sys.stdin.read()
    elif args.dump:
        text = open(args.dump, errors="replace").read()
    else:
        ap.error("a dump, - for stdin, or --udp")
    if "memsites-dump 1" not in text:
        sys.exit("memsites: no `memsites dump` output in what was read")
    sites, rust = parse(text)
    groups = merged(sites)
    elf = args.elf or default_elf(sites)
    if elf is None or not os.path.exists(elf):
        sys.exit("memsites: no ELF to name the frames by (--elf): %s" % elf)

    shown = groups[:args.top]
    wanted = []
    for g in shown:
        for addr, name in g["frames"][g["at"]:g["at"] + 1 + args.callers]:
            # A return address is named by its call, the byte before it; a
            # module's is not the ELF's.
            if " [" not in name:
                wanted.append(addr - 1)
    named = dict(zip(wanted, symbolizer(elf, wanted))) if wanted else {}

    blocks = sum(g["blocks"] for g in groups)
    taken = sum(g["taken"] for g in groups)
    print("memsites: %d sites hold %s in %d blocks" % (len(groups), size_text(taken), blocks), end="")
    if rust:
        print("; the rest of Rust's heap, about %s in %d blocks, has none" %
              (size_text(max(rust["taken"] - taken, 0)), max(rust["blocks"] - blocks, 0)), end="")
    print()
    print("     taken      asked  blocks paths  where")
    for g in shown:
        frames, at = g["frames"], g["at"]
        for k, (addr, name) in enumerate(frames[at:at + 1 + args.callers]):
            line = place(named.get(addr - 1, [])) if " [" not in name else None
            text = name + ("  " + line if line else "")
            if k == 0:
                print("%10s %10s %7d %5d  %s" % (size_text(g["taken"]), size_text(g["asked"]), g["blocks"],
                                                  g["paths"], text))
            else:
                print("%37s<- %s" % ("", text))
    return 0


if __name__ == "__main__":
    sys.exit(main())
