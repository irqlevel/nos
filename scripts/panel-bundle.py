#!/usr/bin/env python3
"""nos's management panel as a release has it, and as `api panel create`
has its VM install it: nos-panel-<version>.tar.gz.

    scripts/panel-bundle.py [--version VERSION] [--out DIR]

In it, for x86-64 (the hypervisor's guests'):

- node/bin/node and caddy: Node and Caddy at the versions pinned here, each
  held to the checksum its release publishes, pinned here too -- a download
  that is not what was pinned fails the build;
- app/: the panel (panel/), built standalone by Next.js in a container of
  Node's own image, from the files git has of panel/ alone and the packages
  its package-lock.json pins;
- install.sh, Caddyfile and systemd/: panel/appliance/, what the VM runs;
- VERSION, and licenses/.

Every member is owned by root, dated the commit's time and of a fixed mode,
in name order, so the same tree makes the same bundle. Prints its path and
its SHA-256: `make image PANEL_BUNDLE=<it> PANEL_BUNDLE_URL=<where the
release has it>` writes both into the system image's /lib/panel/bundle,
which `api panel create` fetches it by and holds it to.
"""

import argparse
import gzip
import hashlib
import io
import os
import subprocess
import sys
import tarfile
import tempfile

NODE_VERSION = "24.22.1"
NODE_URL = "https://nodejs.org/dist/v%s/node-v%s-linux-x64.tar.xz" % (NODE_VERSION, NODE_VERSION)
# SHASUMS256.txt of that release
NODE_SHA256 = "6c172a9ab6718f7b801fb5a7bd486af2ee5e32758086a04b84fcce2aa77095e6"
CADDY_VERSION = "2.11.7"
CADDY_URL = ("https://github.com/caddyserver/caddy/releases/download/v%s/caddy_%s_linux_amd64.tar.gz"
             % (CADDY_VERSION, CADDY_VERSION))
# caddy_<version>_checksums.txt of that release
CADDY_SHA512 = ("a7a433a1b133efc3c8d10eb0b99d52a24b5ef5c322dc77f5282182b1c0402139"
                "ab83f3a99f0c52409df77d20123fb0b523edad8a66d8f5e49136197bf61ef0e7")
# The image the panel is built in: the Node the bundle runs it with.
BUILDER = "node:%s-trixie-slim" % NODE_VERSION

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APPLIANCE = os.path.join(ROOT, "panel", "appliance")


def die(what):
    sys.exit("panel-bundle: " + what)


def fetch(url, algorithm, want):
    """`url`'s bytes, held to the pinned digest. By curl, which has the
    system's trust store where Python's ssl may have none (a python.org
    build on a Mac)."""
    print("panel-bundle: fetching " + url, flush=True)
    data = subprocess.run(["curl", "-fsSL", "--retry", "3", "--max-time", "600", url], check=True,
                          capture_output=True).stdout
    got = hashlib.new(algorithm, data).hexdigest()
    if got != want:
        die("%s is not what was pinned: %s %s, %s pinned" % (url, algorithm, got, want))
    return data


def git(*args):
    return subprocess.run(["git", "-C", ROOT] + list(args), check=True, capture_output=True).stdout


def build_app(work):
    """The panel built standalone in BUILDER: the files git has of panel/,
    copied in -- nothing of this checkout's node_modules or .next -- and
    the standalone server, its static files and public/ copied out to
    work/app."""
    files = [f for f in git("ls-files", "-z", "panel").decode().split("\0") if f]
    src = os.path.join(work, "src.tar")
    with tarfile.open(src, "w") as t:
        for f in files:
            t.add(os.path.join(ROOT, f), arcname=os.path.relpath(f, "panel"), recursive=False)
    script = """
        set -eu
        mkdir /build
        tar -C /build -xf /work/src.tar
        cd /build
        npm ci --no-audit --no-fund
        NEXT_TELEMETRY_DISABLED=1 npm run build
        mkdir -p /work/app/.next
        cp -a .next/standalone/. /work/app/
        cp -a .next/static /work/app/.next/static
        if [ -d public ]; then cp -a public /work/app/public; fi
        rm -rf /work/app/.next/cache
        chown -R "$HOST_UID:$HOST_GID" /work/app
    """
    subprocess.run(["docker", "run", "--rm", "--platform", "linux/amd64",
                    "-e", "HOST_UID=%d" % os.getuid(), "-e", "HOST_GID=%d" % os.getgid(),
                    "-v", "%s:/work" % work, BUILDER, "sh", "-c", script], check=True)
    if not os.path.isfile(os.path.join(work, "app", "server.js")):
        die("the build left no app/server.js")


def member(name, mode, mtime, size=0, kind=tarfile.REGTYPE, link=""):
    info = tarfile.TarInfo(name)
    info.mode = mode
    info.mtime = mtime
    info.uid = info.gid = 0
    info.uname = info.gname = "root"
    info.size = size
    info.type = kind
    info.linkname = link
    return info


def one_of(archive, opener, name):
    """The bytes of the member `name` of a tarball given whole."""
    with tarfile.open(fileobj=io.BytesIO(archive), mode=opener) as t:
        f = t.extractfile(name)
        if f is None:
            die("no %s in it" % name)
        return f.read()


def main():
    ap = argparse.ArgumentParser(description="Build nos-panel-<version>.tar.gz")
    ap.add_argument("--version", default=None, help="the release's name (git describe, by default)")
    ap.add_argument("--out", default=os.path.join(ROOT, "out", "panel"), help="where the bundle goes")
    args = ap.parse_args()
    version = args.version or git("describe", "--tags", "--always", "--dirty").decode().strip()
    if not version or "/" in version or any(c.isspace() for c in version):
        die("a version is a word")
    mtime = int(git("log", "-1", "--format=%ct").decode().strip())

    node = fetch(NODE_URL, "sha256", NODE_SHA256)
    caddy = fetch(CADDY_URL, "sha512", CADDY_SHA512)
    prefix = "node-v%s-linux-x64/" % NODE_VERSION
    # name: (bytes, mode), or (None, the link's target) for a link
    files = {
        "node/bin/node": (one_of(node, "r:xz", prefix + "bin/node"), 0o755),
        "licenses/node": (one_of(node, "r:xz", prefix + "LICENSE"), 0o644),
        "caddy": (one_of(caddy, "r:gz", "caddy"), 0o755),
        "licenses/caddy": (one_of(caddy, "r:gz", "LICENSE"), 0o644),
        "VERSION": (("nos-panel %s\nnode %s\ncaddy %s\n" % (version, NODE_VERSION, CADDY_VERSION)).encode(), 0o644),
    }
    for name, mode in (("install.sh", 0o755), ("Caddyfile", 0o644), ("nos-panel.service", 0o644),
                       ("nos-panel-caddy.service", 0o644)):
        with open(os.path.join(APPLIANCE, name), "rb") as f:
            files[("systemd/" if name.endswith(".service") else "") + name] = (f.read(), mode)

    with tempfile.TemporaryDirectory() as work:
        build_app(work)
        app = os.path.join(work, "app")
        for top, subdirs, names in os.walk(app):
            subdirs.sort()
            for n in names + [d for d in subdirs if os.path.islink(os.path.join(top, d))]:
                path = os.path.join(top, n)
                name = "app/" + os.path.relpath(path, app)
                if os.path.islink(path):
                    target = os.readlink(path)
                    if os.path.isabs(target) or not os.path.realpath(path).startswith(app + os.sep):
                        die("a link out of the app: %s -> %s" % (name, target))
                    files[name] = (None, target)
                    continue
                with open(path, "rb") as f:
                    mode = 0o755 if os.stat(path).st_mode & 0o111 else 0o644
                    files[name] = (f.read(), mode)

    os.makedirs(args.out, exist_ok=True)
    out = os.path.join(args.out, "nos-panel-%s.tar.gz" % version)
    dirs = set()
    for name in files:
        parts = name.split("/")[:-1]
        dirs.update("/".join(parts[:i]) for i in range(1, len(parts) + 1))
    with open(out + ".tmp", "wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=mtime, compresslevel=9) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as t:
                for d in sorted(dirs):
                    t.addfile(member(d, 0o755, mtime, kind=tarfile.DIRTYPE))
                for name in sorted(files):
                    data, how = files[name]
                    if data is None:
                        t.addfile(member(name, 0o777, mtime, kind=tarfile.SYMTYPE, link=how))
                    else:
                        t.addfile(member(name, how, mtime, len(data)), io.BytesIO(data))
    os.replace(out + ".tmp", out)
    with open(out, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    print("panel-bundle: %s, %d MB, sha256 %s" % (out, os.path.getsize(out) // (1 << 20), digest))


if __name__ == "__main__":
    main()
