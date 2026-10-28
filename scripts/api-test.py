#!/usr/bin/env python3
"""api-test: the host API module (modules/api) and nosctl, end to end.

nos boots under QEMU with api.ko, modtest.ko and sshd.ko on its root, an
api.toml that listens on eth0 (hv0 is the default, and the panel's), and two
clients' keys in /etc/api/clients -- an admin's and a reader's, made by
`nosctl keygen` -- and a third key, a stranger's, that is in no file. Then,
through nosctl's own client:

  - the host's key, made on the first start, is the one `api status` and
    `api cert` print, and the one the connection shows: pinned, a host with
    another key is refused before a request is sent;
  - a stranger, and a client with no certificate, are refused in the TLS
    handshake: the API counts them, and its count of requests parsed does
    not move -- no byte of theirs reached the HTTP parser;
  - a reader calls reads, and is refused (403) an action, a job and a cancel,
    and is shown no method but reads; an admin calls all of them, and every
    one that changes something is a line of the audit log;
  - a job of modtest's runs to its end through the API, its log and result
    read back; another is cancelled, and says so;
  - a descriptor comes with an ETag, and a request with it is a 304; an
    unknown route is a 404, a method a route does not take a 405, parameters
    that are not JSON a 400, a body past the limit a 413;
  - the stream: metrics within a second, and an action's audit line in the
    log channel as it is logged;
  - the limits: a fifth connection of one key is let go after its handshake,
    a ninth connection reset at the door, and with two handshakes going the
    third reset at once -- the two let go when their time is up; a
    connection that says nothing for its idle time is let go;
  - sshd answers while the API holds all eight;
  - a client taken off the list is refused its next request on a connection
    it holds, and its next handshake; put back, it is let in again;
  - no slot of the API's port is left in TIME-WAIT: the clients close first;
  - rmmod api with a stream, a request's connection and a handshake under
    way unloads at once, and leaves no slot of the port;
  - what a handshake costs, and how deep the connection's task went in it,
    are printed.

On x86-64, where the guests are, hv.ko is loaded too, and its vm provider
taken through the API (vm_checks): a VM of hvboot -- which, given no disk,
says so and powers the guest off -- started as a job, found stopped, its
console read and streamed, a reader's typing at it refused, an admin's
refused for a VM that does not run, and stopped; a VM made of an image the
test serves, cloned, the clone deleted; a protected VM's delete refused
without force; and a create of an image served slowly cancelled midway,
nothing of it left behind.

    scripts/api-test.py [--arch aarch64|x86_64] [--tcg]

arm64 is the whole of it, over the UDP shell. `--arch x86_64` is the short
form: nos.iso, a root whose /etc/rc loads the modules and starts the API, the
host's key read off the serial log, and what needs no shell. Exit code 0:
every check held; 1: one did not.
"""
import argparse
import importlib.machinery
import importlib.util
import json
import os
import platform
import re
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)


def load(name, path):
    loader = importlib.machinery.SourceFileLoader(name, path)
    spec = importlib.util.spec_from_loader(name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


pt = load("pt", os.path.join(HERE, "parttest.py"))
nc = load("nosctl", os.path.join(HERE, "nosctl"))

PORT = 39234
SSH_HOST_PORT = 2222
IDLE_SECS = 10
HANDSHAKE_S = 10
MAX_CONNECTIONS = 8
MAX_PER_KEY = 4


def keys(tmp):
    """The admin's, the reader's and the stranger's keys, and their fingerprints."""
    out = {}
    for name in ("admin", "reader", "stranger"):
        nc.keygen(name, tmp)
        crt, key = os.path.join(tmp, name + ".crt"), os.path.join(tmp, name + ".key")
        out[name] = (crt, key, nc.fingerprint(nc.pem_cert(crt)))
    return out


# The images the vm provider's create downloads (x86-64): a raw disk -- a
# partition table's signature and random bytes -- and one served slowly, a
# chunk at a time, for a create to be cancelled while it downloads.
RAW_SIZE = 2 * 1024 * 1024
SLOW_SIZE = 64 * 1024 * 1024
SLOW_CHUNK = 64 * 1024
SLOW_PAUSE = 0.05
# hvboot, as `make hvboot` builds it: the kernel of the VM the test starts.
HVBOOT = os.path.join(ROOT, "out", "x86_64", "hvboot")
# What /etc/rc does for the vm checks: hv loaded and on, and a VM of hvboot.
VM_RC = "insmod /hv.ko\nhv on\nhv create t1 /lib/hvboot mem=64\n"


def rootfs(tmp, arch, k, rc):
    rootdir = os.path.join(tmp, "rootdir")
    os.makedirs(os.path.join(rootdir, "etc", "api"))
    for m in ("api", "modtest", "sshd"):
        shutil.copy(pt.module_path(arch, m), rootdir)
    if arch == "x86_64":
        shutil.copy(pt.module_path(arch, "hv"), rootdir)
        os.makedirs(os.path.join(rootdir, "lib"))
        shutil.copy(HVBOOT, os.path.join(rootdir, "lib", "hvboot"))
    with open(os.path.join(rootdir, "etc", "api", "api.toml"), "w") as f:
        f.write("# api-test.py\nport = %d\nlisten = [\"eth0\"]\nidle_secs = %d\nmax_connections = %d\n"
                "max_per_key = %d\nmax_handshakes = 2\n" % (PORT, IDLE_SECS, MAX_CONNECTIONS, MAX_PER_KEY))
    with open(os.path.join(rootdir, "etc", "api", "clients"), "w") as f:
        f.write("%s admin admin\n%s read reader\n" % (k["admin"][2], k["reader"][2]))
    if rc:
        with open(os.path.join(rootdir, "etc", "rc"), "w") as f:
            f.write("# api-test.py\ninsmod /modtest.ko\ninsmod /api.ko\napi start\n"
                    + (VM_RC if arch == "x86_64" else ""))
    image = os.path.join(tmp, "root.img")
    subprocess.run([os.path.join(HERE, "mkrootfs.sh"), image, "64", rootdir, "1024"], cwd=ROOT, check=True,
                   stdout=subprocess.DEVNULL)
    return image


def client(k, who, pin):
    crt, key, _ = k[who]
    return nc.Api("127.0.0.1", PORT, crt, key, pin, timeout=20)


def status(k, pin):
    """api.status, through the API itself, as the admin."""
    a = client(k, "admin", pin)
    try:
        s, _, body = a.request("POST", "/v1/call/api/status")
        return body if s == 200 else {}
    finally:
        a.close()


def tls_refused(crt, key):
    """A handshake with this certificate -- or none -- that the host refuses:
    the error, or None if it let the client in."""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.minimum_version = ssl.TLSVersion.TLSv1_3
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    if crt:
        ctx.load_cert_chain(crt, key)
    raw = socket.create_connection(("127.0.0.1", PORT), timeout=20)
    try:
        s = ctx.wrap_socket(raw, server_hostname="nos")
        # TLS 1.3: the refusal of the client's certificate comes after the
        # client's Finished, with the first read
        s.sendall(b"GET /v1/host HTTP/1.1\r\nHost: nos\r\n\r\n")
        data = s.recv(1024)
        s.close()
        return None if data else "closed"
    except (ssl.SSLError, OSError) as e:
        return str(e)
    finally:
        raw.close()


def tcp_state(k, pin):
    """The API's port's TCP slots, through the API: net.tcp, a read."""
    a = client(k, "reader", pin)
    try:
        s, _, body = a.request("POST", "/v1/call/net/tcp")
        return [c for c in body.get("connections", []) if c.get("local_port") == PORT] if s == 200 else None
    finally:
        a.close()


def common(k, pin, arch, sh=None):
    """What both forms check."""
    st0 = status(k, pin)
    pt.check("the API says what it serves through its own provider", st0.get("serving") is True and
             st0.get("key") == pin, st0)

    a = client(k, "admin", pin)
    s, _, host = a.request("GET", "/v1/host")
    pt.check("GET /v1/host: the host, and who asks", s == 200 and host.get("arch") == arch and
             host.get("client") == {"name": "admin", "role": "admin"} and host.get("key") == pin, (s, host))
    a.close()

    # Refused before HTTP: the request count moves by the status calls alone
    before = status(k, pin)
    why = tls_refused(k["stranger"][0], k["stranger"][1])
    why_none = tls_refused(None, None)
    after = status(k, pin)
    pt.check("a stranger's key is refused in the handshake", why is not None, why)
    pt.check("and so is a client with no certificate", why_none is not None, why_none)
    pt.check("both counted refused, one as an unknown key",
             after.get("refused", 0) - before.get("refused", 0) == 2 and
             after.get("unknown_key", 0) - before.get("unknown_key", 0) == 1, (before, after))
    pt.check("and no request of theirs was parsed: the count moved by one, the status call's",
             after.get("requests", 0) - before.get("requests", 0) == 1, (before.get("requests"), after.get("requests")))

    bad = nc.Api("127.0.0.1", PORT, k["admin"][0], k["admin"][1], "00" * 32, timeout=20)
    try:
        bad.request("GET", "/v1/host")
        pinned = False
    except ssl.SSLError:
        pinned = True
    bad.close()
    pt.check("a host showing a key other than the pinned one is refused by nosctl", pinned)

    r = client(k, "reader", pin)
    s, _, _ = r.request("POST", "/v1/call/system/memory")
    pt.check("a reader calls a read", s == 200, s)
    s, _, body = r.request("POST", "/v1/call/system/set_log_level", {"level": 1})
    pt.check("and is refused an action: 403", s == 403 and body.get("error", {}).get("code") == "forbidden", (s, body))
    s, _, body = r.request("POST", "/v1/jobs/modtest/nap")
    pt.check("and a job: 403", s == 403, (s, body))
    s, _, body = r.request("POST", "/v1/jobs/1/cancel")
    pt.check("and a cancel: 403", s == 403, (s, body))
    s, _, body = r.request("GET", "/v1/providers")
    methods = [m["name"] for p in body.get("providers", []) for m in p["methods"]]
    pt.check("and is shown the reads alone", s == 200 and "memory" in methods and "power_off" not in methods and
             "set_nice" not in methods, methods)
    r.close()

    a = client(k, "admin", pin)
    s, _, body = a.request("POST", "/v1/call/system/set_log_level", {"level": 1})
    pt.check("an admin calls an action", s == 200 and body == {"log_level": 1}, (s, body))
    s, _, body = a.request("GET", "/v1/providers")
    methods = [m["name"] for p in body.get("providers", []) for m in p["methods"]]
    pt.check("and is shown every method", "power_off" in methods and "nap" in methods, methods)

    # A job, to its end
    s, _, body = a.request("POST", "/v1/jobs/modtest/nap")
    job = body.get("id") if s == 202 else None
    pt.check("a job started: 202 and its number", job is not None, (s, body))
    rec = {}
    for _ in range(100):
        s, _, rec = a.request("GET", "/v1/jobs/%d" % job)
        if rec.get("state") == "done":
            break
        time.sleep(0.1)
    pt.check("run to its end, its result and log read back", rec.get("status") == "ok" and
             rec.get("result") == {"napped": True} and any("napping" in n["text"] for n in rec.get("notes", [])), rec)
    s, _, body = a.request("POST", "/v1/jobs/modtest/wait")
    wait = body.get("id")
    for _ in range(50):
        s, _, rec = a.request("GET", "/v1/jobs/%d" % wait)
        if rec.get("state") == "running":
            break
        time.sleep(0.1)
    s, _, _ = a.request("POST", "/v1/jobs/%d/cancel" % wait)
    for _ in range(50):
        _, _, rec = a.request("GET", "/v1/jobs/%d" % wait)
        if rec.get("state") == "done":
            break
        time.sleep(0.1)
    pt.check("a job told to stop says it was", s == 200 and rec.get("status") == "cancelled", (s, rec))
    s, _, body = a.request("POST", "/v1/call/modtest/nap")
    pt.check("a job called as a call: 409", s == 409 and body.get("error", {}).get("code") == "kind", (s, body))

    # Descriptors and the HTTP edges
    s, h, _ = a.request("GET", "/v1/providers/system")
    tag = h.get("etag")
    s2, _, _ = a.request("GET", "/v1/providers/system", headers={"If-None-Match": tag})
    pt.check("a descriptor with its ETag, and a 304 for it", s == 200 and tag and s2 == 304, (s, tag, s2))
    s, _, _ = a.request("GET", "/v1/nothing")
    pt.check("an unknown route: 404", s == 404, s)
    s, h, _ = a.request("PUT", "/v1/host")
    pt.check("a method a route does not take: 405, and what it does", s == 405 and h.get("allow") == "GET", (s, h))
    s, _, body = a.request("POST", "/v1/call/system/set_log_level", b"{level: 1")
    pt.check("parameters that are not JSON: 400", s == 400 and body.get("error", {}).get("code") == "invalid", (s, body))
    s, h, body = a.request("POST", "/v1/call/system/info", b"x" * (70 * 1024))
    pt.check("a body past the limit: 413 too_large, and the connection closed",
             s == 413 and body.get("error", {}).get("code") == "too_large" and h.get("connection") == "close", (s, h, body))
    a.close()

    # The stream
    st = client(k, "reader", pin)
    st.stream("?channels=metrics,log")
    first = st.message()
    pt.check("the stream's metrics come", first and first.get("channel") == "metrics" and
             first.get("cpus") and first.get("memory") and first.get("interfaces"), first)
    act = client(k, "admin", pin)
    act.request("POST", "/v1/call/system/set_log_level", {"level": 1})
    act.close()
    seen = False
    end = time.time() + 10
    while time.time() < end and not seen:
        m = st.message()
        if m and m.get("channel") == "log":
            seen = any("call system.set_log_level" in l["text"] for l in m["data"]["lines"])
    pt.check("and the log: an action's audit line, as it is logged", seen)
    st.say_goodbye()
    st.close()

    # The limits
    held = []
    for _ in range(MAX_PER_KEY):
        c = client(k, "admin", pin)
        c.request("GET", "/v1/host")
        held.append(c)
    fifth = client(k, "admin", pin)
    b0 = status_via(held[0])
    try:
        fifth.request("GET", "/v1/host")
        let_in = True
    except (OSError, ssl.SSLError, ConnectionError, ValueError):
        let_in = False
    fifth.close()
    b1 = status_via(held[0])
    pt.check("a fifth connection of one key is let go after its handshake", not let_in and
             b1.get("busy", 0) > b0.get("busy", 0), (b0.get("busy"), b1.get("busy")))
    for _ in range(MAX_CONNECTIONS - MAX_PER_KEY):
        c = client(k, "reader", pin)
        c.request("GET", "/v1/host")
        held.append(c)
    ninth = tls_refused(k["reader"][0], k["reader"][1])
    b2 = status_via(held[0])
    pt.check("a ninth connection is reset at the door", ninth is not None and b2.get("busy", 0) > b1.get("busy", 0),
             (ninth, b1.get("busy"), b2.get("busy")))
    if sh is not None:
        banner = b""
        try:
            s = socket.create_connection(("127.0.0.1", SSH_HOST_PORT), timeout=15)
            banner = s.recv(64)
            s.close()
        except OSError as e:
            banner = str(e).encode()
        pt.check("sshd answers while the API holds all eight", banner.startswith(b"SSH-2.0-"), banner)
    for c in held:
        c.close()
    time.sleep(1)

    # Two handshakes going: a third is reset at once; the two let go in time
    silent = [socket.create_connection(("127.0.0.1", PORT), timeout=30) for _ in range(2)]
    time.sleep(0.5)
    third = socket.create_connection(("127.0.0.1", PORT), timeout=5)
    try:
        third.settimeout(5)
        got = third.recv(16)
        reset = got == b""
    except (ConnectionResetError, OSError):
        reset = True
    third.close()
    pt.check("with two handshakes going, a third is reset at once", reset)
    t0 = time.time()
    ended = 0
    for s in silent:
        try:
            s.settimeout(HANDSHAKE_S + 10)
            s.recv(16)
        except OSError:
            pass
        ended += 1
        s.close()
    pt.check("and the two silent ones are let go when the handshake's time is up",
             ended == 2 and time.time() - t0 <= HANDSHAKE_S + 8, round(time.time() - t0, 1))

    # Idle
    idle = client(k, "reader", pin)
    idle.request("GET", "/v1/host")
    t0 = time.time()
    try:
        idle.sock.settimeout(IDLE_SECS + 15)
        got = idle.sock.recv(16)
        went = got == b""
    except (OSError, ssl.SSLError):
        went = True
    pt.check("a connection that says nothing for its idle time is let go", went and
             IDLE_SECS - 1 <= time.time() - t0 <= IDLE_SECS + 10, round(time.time() - t0, 1))
    idle.close()

    time.sleep(2)
    left = tcp_state(k, pin)
    waits = [c for c in (left or []) if c.get("state") == "TIME_WAIT"]
    pt.check("no slot of the API's port is in TIME-WAIT: the clients closed first", left is not None and not waits,
             left)

    st = status(k, pin)
    hs = st.get("handshakes", {})
    print("  (handshakes: %d, mean %d us, slowest %d us)" % (hs.get("count", 0), hs.get("mean_us", 0), hs.get("max_us", 0)))
    pt.check("the API took, refused and let go what it should, and counted it", st.get("accepted", 0) > 10, st)


def status_via(c):
    s, _, body = c.request("POST", "/v1/call/api/status")
    return body if s == 200 else {}


def qemu_arm64(tcg, image, log):
    hvf = platform.system() == "Darwin" and platform.machine() == "arm64" and not tcg
    accel = ["-accel", "hvf", "-cpu", "host"] if hvf else ["-accel", "tcg", "-cpu", "cortex-a72"]
    return ["qemu-system-aarch64", "-M", "virt,gic-version=3", "-smp", "4", "-m", "1024"] + accel + [
        "-kernel", os.path.join(ROOT, "nos-arm64.img"),
        "-append", "root=auto dhcp=auto udpshell=%d" % pt.SHELL_PORT,
        "-global", "virtio-mmio.force-legacy=false",
        "-drive", "file=%s,format=raw,id=hd0,if=none" % image, "-device", "virtio-blk-device,drive=hd0",
        "-device", "virtio-net-device,netdev=n0",
        "-netdev", "user,id=n0,hostfwd=udp::%d-:%d,hostfwd=tcp::%d-:%d,hostfwd=tcp::%d-:22" % (
            pt.SHELL_PORT, pt.SHELL_PORT, PORT, PORT, SSH_HOST_PORT),
        "-serial", "file:" + log, "-display", "none"]


def stop(p):
    if p.poll() is None:
        p.terminate()
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()


def arm64(args, tmp, k):
    image = rootfs(tmp, "aarch64", k, rc=False)
    log = os.path.join(tmp, "serial.log")
    p = subprocess.Popen(qemu_arm64(args.tcg, image, log), cwd=ROOT, stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 300)):
            return
        time.sleep(10)
        sh = pt.Shell()
        sh.sock.settimeout(60)
        out = sh.run("insmod /api.ko")
        pt.check("api.ko loads", "loaded" in out, out)
        out = sh.run("api start")
        found = re.search(r"host key ([0-9a-f]{64})", out)
        if not pt.check("api start serves, its host key made", "serving port %d on eth0" % PORT in out and found, out):
            return
        pin = found.group(1)
        out = sh.run("api cert")
        pem = re.search(r"-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----", out, re.S)
        der = ssl.PEM_cert_to_DER_cert(pem.group(0)) if pem else b""
        pt.check("api cert prints the certificate whose key it says", pem and nc.fingerprint(der) == pin and pin in out,
                 out[-300:])
        out = sh.run("api start")
        pt.check("a second start is refused", "already" in out, out)
        out = sh.run("insmod /modtest.ko")
        out = sh.run("insmod /sshd.ko") + sh.run("sshd start")
        pt.check("sshd starts beside it", "listening on port 22" in out, out)
        # The listener binds within its second's look for the interface
        for _ in range(20):
            if "eth0: listening" in sh.run("api"):
                break
            time.sleep(0.5)

        common(k, pin, "aarch64", sh)

        out = sh.run("cat /etc/api/audit.log")
        pt.check("the audit log has the admin's action, the job started and the cancel",
                 "admin 10.0.2.2 call system.set_log_level ok" in out and "start modtest.nap ok" in out and
                 "cancel job" in out, out[-600:])

        # A client taken off the list, and put back
        r = client(k, "reader", pin)
        s, _, _ = r.request("GET", "/v1/host")
        out = sh.run("api client del reader")
        pt.check("api client del takes a key off the list", "let in no more" in out, out)
        s2, _, body = r.request("GET", "/v1/host")
        pt.check("its next request on a connection it holds: 403 forbidden, and closed",
                 s == 200 and s2 == 403 and body.get("error", {}).get("code") == "forbidden", (s, s2, body))
        r.close()
        pt.check("and its next handshake is refused", tls_refused(k["reader"][0], k["reader"][1]) is not None)
        out = sh.run("api client add reader read %s" % k["reader"][2])
        r = client(k, "reader", pin)
        s, _, _ = r.request("GET", "/v1/host")
        r.close()
        pt.check("put back, it is let in again", s == 200, (s, out))

        # The stack, and the unload under open connections
        st = client(k, "reader", pin)
        st.stream("?channels=metrics")
        st.message()
        out = sh.run("stacks")
        depth = re.search(r"task \d+ (\d+) (\d+) api/conn", out)
        if depth:
            print("  (a connection's task: %s of its %s stack bytes, handshake and stream)" % depth.groups())
        pt.check("the connection's task's stack is read", depth is not None, out[-400:])
        held = client(k, "admin", pin)
        held.request("GET", "/v1/host")
        silent = socket.create_connection(("127.0.0.1", PORT), timeout=30)
        time.sleep(0.5)
        t0 = time.time()
        out = sh.run("rmmod api")
        took = time.time() - t0
        silent.close()
        pt.check("rmmod api, with a stream, a connection and a handshake under way, unloads", "unloaded" in out, out)
        pt.check("at once: the handshake's ten seconds are not waited out", took < 4, round(took, 1))
        try:
            gone = st.message() is None
        except (OSError, ssl.SSLError, ConnectionError):
            gone = True
        st.close()
        held.close()
        pt.check("and the stream's client sees it end", gone)
        time.sleep(2)
        out = sh.run("mgmt call net tcp")
        pt.check("and no slot of the API's port is left", ('"local_port":%d' % PORT) not in out, out)
        pt.check("nothing panicked", "PANIC" not in open(log, errors="replace").read())
    finally:
        stop(p)


def image_server(tmp):
    """A web server on the test machine, which QEMU's user network puts at
    10.0.2.2 for nos: the raw disk as a file, and /slow.img written a chunk
    at a time. The server, and its port."""
    import functools
    import http.server
    import random
    import threading

    www = os.path.join(tmp, "www")
    os.makedirs(www)
    raw = bytearray(random.Random(7).randbytes(RAW_SIZE))
    raw[510:512] = b"\x55\xaa"
    with open(os.path.join(www, "raw.img"), "wb") as f:
        f.write(raw)

    class Handler(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            if self.path != "/slow.img":
                return super().do_GET()
            self.send_response(200)
            self.send_header("Content-Length", str(SLOW_SIZE))
            self.end_headers()
            chunk = bytes(SLOW_CHUNK)
            try:
                for _ in range(SLOW_SIZE // SLOW_CHUNK):
                    self.wfile.write(chunk)
                    time.sleep(SLOW_PAUSE)
            except OSError:
                pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(Handler, directory=www))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def vm_job(a, method, params, timeout=300):
    """A job of the vm provider's started and followed to its end: its
    starting answer, and its record once done (None if it never was)."""
    s, _, body = a.request("POST", "/v1/jobs/vm/%s" % method, params)
    if s != 202:
        return (s, body), None
    end = time.time() + timeout
    while time.time() < end:
        _, _, rec = a.request("GET", "/v1/jobs/%d" % body["id"])
        if rec.get("state") == "done":
            return (s, body), rec
        time.sleep(0.5)
    return (s, body), None


def vm_get(a, name):
    s, _, body = a.request("POST", "/v1/call/vm/get", {"name": name})
    return s, body


def vm_checks(k, pin, port):
    """The vm provider through the API (x86-64): what the module's docstring
    says of it."""
    a = client(k, "admin", pin)
    r = client(k, "reader", pin)
    url = "http://10.0.2.2:%d/" % port

    s, _, d = a.request("GET", "/v1/providers/vm")
    pt.check("the vm provider is on the bus, its descriptor served", s == 200 and d.get("provider") == "vm", (s, d))
    s, _, body = r.request("GET", "/v1/providers")
    vm = [p for p in body.get("providers", []) if p.get("name") == "vm"] if s == 200 else []
    methods = {m["name"]: m.get("kind") for m in vm[0]["methods"]} if vm else {}
    pt.check("a reader is shown its reads alone", "list" in methods and "console_read" in methods
             and all(kind == "read" for kind in methods.values()), (s, methods))
    s, _, listed = r.request("POST", "/v1/call/vm/list")
    t1 = [v for v in listed.get("vms", []) if v.get("name") == "t1"] if s == 200 else []
    pt.check("the reader lists the vms: t1, off", t1 and t1[0].get("state") == "off" and t1[0].get("folder") is True,
             (s, listed))

    started, rec = vm_job(a, "start", {"name": "t1"})
    pt.check("t1 started as a job, which ends ok with its number", rec is not None and rec.get("status") == "ok"
             and isinstance(rec.get("result", {}).get("id"), int), (started, rec))
    pt.check("the job's log says it started", rec is not None
             and any("started on" in n.get("text", "") for n in rec.get("notes", [])), rec)
    stopped = None
    end = time.time() + 120
    while time.time() < end:
        s, got = vm_get(a, "t1")
        if s == 200 and got.get("vm", {}).get("state") == "stopped":
            stopped = got
            break
        time.sleep(1)
    pt.check("hvboot, given no disk, turns the guest off: t1 is found stopped", stopped is not None,
             vm_get(a, "t1"))

    s, _, page = r.request("POST", "/v1/call/vm/console_read", {"name": "t1"})
    pt.check("its console read by a reader: hvboot's lines, from where the ring begins",
             s == 200 and "hvboot" in page.get("text", "") and page.get("from") == 0
             and page.get("next") == page.get("total"), (s, page))
    s, _, body = r.request("POST", "/v1/call/vm/console_write", {"name": "t1", "text": "x"})
    pt.check("a reader may not type at it: 403", s == 403, (s, body))

    r.close()
    r.stream("?channels=console&vm=t1")
    msg = r.message()
    pt.check("the reader's stream of t1's console: what it printed, hvboot's lines among it",
             msg is not None and msg.get("channel") == "console" and "hvboot" in msg.get("data", {}).get("text", ""),
             msg)
    r.type("x")
    msg = r.message()
    pt.check("and what the reader types is refused", msg is not None and msg.get("refused") == "forbidden", msg)
    r.say_goodbye()
    r.close()
    a.close()
    a.stream("?channels=console&vm=t1")
    first = a.message()
    a.type("x")
    msg = a.message()
    pt.check("an admin's typing at a vm that does not run is refused, saying why",
             first is not None and msg is not None and msg.get("refused") == "conflict", (first, msg))
    a.say_goodbye()
    a.close()

    started, rec = vm_job(a, "stop", {"name": "t1"})
    pt.check("t1 stopped -- taken off the list -- as a job", rec is not None and rec.get("status") == "ok", (started, rec))
    s, got = vm_get(a, "t1")
    pt.check("and off", s == 200 and got.get("vm", {}).get("state") == "off", got)

    started, rec = vm_job(a, "create", {"name": "c1", "url": url + "raw.img", "memory_mib": 256})
    pt.check("c1 made of a raw image the test serves", rec is not None and rec.get("status") == "ok"
             and rec.get("result", {}).get("disk_bytes") == RAW_SIZE, (started, rec))
    pt.check("its download in the job's log", rec is not None
             and any(n.get("text", "").startswith("download:") for n in rec.get("notes", [])), rec)
    s, got = vm_get(a, "c1")
    disks = {d.get("file"): d for d in got.get("disks", [])} if s == 200 else {}
    pt.check("its disks: the image and the seed", disks.get("disk.raw", {}).get("size_bytes") == RAW_SIZE
             and disks.get("seed.iso", {}).get("read_only") is True, got)

    started, rec = vm_job(a, "clone", {"name": "c1", "to": "c2"})
    copied = (rec or {}).get("result", {}).get("copied", [])
    pt.check("c1 cloned to c2, its disk copied and its seed made anew", rec is not None and rec.get("status") == "ok"
             and rec["result"].get("seed") is True and len(copied) == 1
             and copied[0].get("size_bytes") == RAW_SIZE and copied[0].get("written_bytes") <= RAW_SIZE,
             (started, rec))
    started, rec = vm_job(a, "delete", {"name": "c2"})
    pt.check("c2 deleted", rec is not None and rec.get("status") == "ok", (started, rec))
    s, got = vm_get(a, "c2")
    pt.check("and gone", s == 404, (s, got))

    s, _, body = a.request("POST", "/v1/call/vm/config_set",
                           {"name": "c1", "text": 'kernel = "/lib/hvboot"\nnet = true\nprotected = true\n'})
    pt.check("c1 made protected through its vm.toml", s == 200 and body.get("changed") is True, (s, body))
    started, rec = vm_job(a, "delete", {"name": "c1"})
    pt.check("a protected vm's delete refused without force", rec is not None and rec.get("status") == "conflict",
             (started, rec))
    started, rec = vm_job(a, "delete", {"name": "c1", "force": True})
    pt.check("and done with it", rec is not None and rec.get("status") == "ok", (started, rec))

    s, _, body = a.request("POST", "/v1/jobs/vm/create", {"name": "c3", "url": url + "slow.img"})
    job = body.get("id") if s == 202 else None
    going = None
    end = time.time() + 60
    while job is not None and time.time() < end:
        _, _, going = a.request("GET", "/v1/jobs/%d" % job)
        if going.get("done", 0) > 0 or going.get("state") == "done":
            break
        time.sleep(0.2)
    pt.check("a create of an image served slowly is under way", going is not None and going.get("state") == "running"
             and going.get("done", 0) > 0, going)
    if job is not None:
        s, _, body = a.request("POST", "/v1/jobs/%d/cancel" % job)
        rec = None
        end = time.time() + 60
        while time.time() < end:
            _, _, rec = a.request("GET", "/v1/jobs/%d" % job)
            if rec.get("state") == "done":
                break
            time.sleep(0.5)
        pt.check("cancelled, it stops as the next of the image comes", rec is not None and rec.get("state") == "done"
                 and rec.get("status") == "cancelled", rec)
    s, got = vm_get(a, "c3")
    s2, _, listed = a.request("POST", "/v1/call/vm/list")
    pt.check("and leaves nothing behind: no c3, nor a folder being made",
             s == 404 and not any(v.get("name") == "c3" for v in listed.get("vms", [])), (s, got, listed))
    a.close()


def x86(args, tmp, k):
    image = rootfs(tmp, "x86_64", k, rc=True)
    server, port = image_server(tmp)
    log = os.path.join(tmp, "serial.log")
    kvm = os.path.exists("/dev/kvm") and not args.tcg
    argv = ["qemu-system-x86_64", "-display", "none", "-m", "1G", "-smp", "2",
            "-cdrom", os.path.join(ROOT, "nos.iso"), "-serial", "file:" + log,
            "-drive", "file=%s,format=raw,id=drive0,if=none" % image,
            "-device", "virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off",
            "-device", "virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off",
            "-netdev", "user,id=net0,hostfwd=tcp::%d-:%d" % (PORT, PORT),
            # TCG's CPU has no RDRAND: the pool's source, without which no
            # key is made and no handshake done
            "-device", "virtio-rng-pci"]
    # Under TCG, -cpu max: the default CPU has no AMD-V for hv's guests.
    argv += ["-enable-kvm", "-cpu", "host"] if kvm else ["-cpu", "max"]
    p = subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        if not pt.check("boots", pt.wait_log(log, "boot: complete", 400)):
            return
        if not pt.check("/etc/rc loads the module and starts the API", pt.wait_log(log, "api: serving port", 120)):
            shutil.copy(log, os.path.join(ROOT, "out", "api-test-x86-serial.log"))
            return
        found = re.search(r"api: serving port \d+ .*?host key ([0-9a-f]{64})", open(log, errors="replace").read())
        if not pt.check("the host's key is in the log", found):
            shutil.copy(log, os.path.join(ROOT, "out", "api-test-x86-serial.log"))
            return
        pin = found.group(1)
        time.sleep(3)
        common(k, pin, "x86_64")
        vm_checks(k, pin, port)
        pt.check("nothing panicked", "PANIC" not in open(log, errors="replace").read())
    finally:
        server.shutdown()
        stop(p)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", choices=("aarch64", "x86_64"), default="aarch64")
    ap.add_argument("--tcg", action="store_true")
    args = ap.parse_args()
    for m in ("api", "modtest", "sshd") + (("hv",) if args.arch == "x86_64" else ()):
        if not os.path.exists(pt.module_path(args.arch, m)):
            sys.exit("no %s -- make modules%s first" % (pt.module_path(args.arch, m),
                                                       " ARCH=aarch64" if args.arch == "aarch64" else ""))
    tmp = tempfile.mkdtemp(prefix="nos-api-")
    try:
        k = keys(os.path.join(tmp, "keys"))
        (arm64 if args.arch == "aarch64" else x86)(args, tmp, k)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print()
    print("api-test: " + ("OK (%s)" % args.arch if not pt.failures else "FAILED: " + ", ".join(pt.failures)))
    return 0 if not pt.failures else 1


if __name__ == "__main__":
    sys.exit(main())
