#!/usr/bin/env python3
"""The management panel's bundle, run as its VM runs it.

    scripts/panel-appliance-test.py <nos-panel-VERSION.tar.gz> [--keep]

In Debian's image -- trixie, x86-64, the panel's VM's -- the bundle unpacked
at /opt/nos-panel as nos-panel-fetch unpacks it, and run as its systemd
units run it: its Node runs the panel (app/server.js) with the environment
nos-panel.service gives it and a seed as `api panel create` makes one, and
its Caddy serves the panel with the bundle's Caddyfile and a P-256
certificate for `nos-panel`. The mock host API (panel/mock) stands in for
nos, run by the bundle's own Node. From outside, as a browser through nos's
relay: TLS by the address alone, no server name, answered with that
certificate and HTTP/2 offered; root's login with the password the seed's
hash is of, a wrong one refused; and a call through the panel reaching the
host API by mTLS, its key pinned. What only the VM does -- cloud-init,
systemd, install.sh -- is not run: `sh -n` reads the scripts.

Needs Docker (linux/amd64: emulated on an arm64 Mac, and slow there) and
openssl.
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
import secrets
import shutil
import socket
import ssl
import subprocess
import sys
import tarfile
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IMAGE = "debian:trixie-slim"
API_PORT = 39234
START_TIMEOUT = 300

failures = []


def check(name, ok, detail=""):
    print("%s %s" % ("PASS" if ok else "FAIL", name), flush=True)
    if not ok:
        failures.append(name)
        if detail:
            print("    " + str(detail)[:3000].replace("\n", "\n    "), flush=True)
    return ok


def openssl_identity(state, name, san=None):
    """A P-256 key and a self-signed certificate, as the mock and nos make
    them: <name>.key, <name>.crt."""
    argv = ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
            "-keyout", os.path.join(state, name + ".key"), "-out", os.path.join(state, name + ".crt"),
            "-days", "825", "-subj", "/CN=" + name]
    if san:
        argv += ["-addext", "subjectAltName=" + san]
    subprocess.run(argv, check=True, capture_output=True)


def der_of(pem_path):
    return subprocess.run(["openssl", "x509", "-in", pem_path, "-outform", "DER"], check=True,
                          capture_output=True).stdout


def pin_of(pem_path):
    """The SHA-256 of a certificate's SubjectPublicKeyInfo, as nos pins it."""
    pub = subprocess.run(["openssl", "x509", "-in", pem_path, "-pubkey", "-noout"], check=True,
                         capture_output=True).stdout
    spki = subprocess.run(["openssl", "pkey", "-pubin", "-outform", "DER"], input=pub, check=True,
                          capture_output=True).stdout
    return hashlib.sha256(spki).hexdigest()


def scrypt_hash(password):
    """root's password hashed as nos-install hashes it."""
    salt = os.urandom(16)
    n, r, p = 1 << 15, 8, 1
    digest = hashlib.scrypt(password.encode(), salt=salt, n=n, r=r, p=p, maxmem=128 * n * r * 2, dklen=32)
    return "scrypt$%d$%d$%d$%s$%s" % (n, r, p, base64.b64encode(salt).decode(), base64.b64encode(digest).decode())


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


# The VM's two services, as their units run them, and the mock in nos's
# place; their logs where the test reads them.
RUN = r"""
set -eu
cd /opt/nos-panel
./node/bin/node /src/panel/mock/server.mjs --port %(api_port)d --state /state > /logs/mock.log 2>&1 &
(cd app && env NODE_ENV=production PORT=3000 HOSTNAME=127.0.0.1 PANEL_DATA=/var/lib/nos-panel \
    PANEL_SEED=/etc/nos-panel/seed.json ../node/bin/node server.js > /logs/panel.log 2>&1) &
env XDG_DATA_HOME=/var/lib/nos-panel/caddy XDG_CONFIG_HOME=/var/lib/nos-panel/caddy \
    ./caddy run --config Caddyfile --adapter caddyfile > /logs/caddy.log 2>&1 &
wait
"""


class Panel:
    def __init__(self, port):
        self.port = port

    def tls(self, alpn):
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        ctx.set_alpn_protocols(alpn)
        return ctx

    def handshake(self):
        """By the address alone, as a browser reaches the relay: no server
        name. The certificate shown, and the protocol chosen."""
        raw = socket.create_connection(("127.0.0.1", self.port), timeout=20)
        with self.tls(["h2", "http/1.1"]).wrap_socket(raw, server_hostname=None) as s:
            return s.getpeercert(binary_form=True), s.selected_alpn_protocol()

    def request(self, method, path, body=None, cookie=None, origin=True):
        c = http.client.HTTPSConnection("127.0.0.1", self.port, timeout=60, context=self.tls(["http/1.1"]))
        headers = {"Host": "127.0.0.1:%d" % self.port}
        if origin:
            headers["Origin"] = "https://127.0.0.1:%d" % self.port
        if cookie:
            headers["Cookie"] = cookie
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        try:
            c.request(method, path, body=data, headers=headers)
            r = c.getresponse()
            return r.status, r.getheader("Set-Cookie") or "", r.read().decode(errors="replace")
        finally:
            c.close()


def main():
    ap = argparse.ArgumentParser(description="The panel's bundle, run as its VM runs it")
    ap.add_argument("bundle")
    ap.add_argument("--keep", action="store_true", help="leave the work folder")
    args = ap.parse_args()
    tmp = tempfile.mkdtemp(prefix="panel-appliance-")
    name = "panel-appliance-%d" % os.getpid()
    try:
        opt = os.path.join(tmp, "opt", "nos-panel")
        os.makedirs(opt)
        with tarfile.open(args.bundle) as t:
            if hasattr(tarfile, "tar_filter"):
                t.extractall(opt, filter="tar")
            else:
                t.extractall(opt)
        for f in ("install.sh",):
            r = subprocess.run(["sh", "-n", os.path.join(opt, f)], capture_output=True, text=True)
            check("the bundle's %s reads as a script" % f, r.returncode == 0, r.stderr)
        check("the bundle has what its units run", all(os.path.exists(os.path.join(opt, p)) for p in
              ("node/bin/node", "caddy", "Caddyfile", "app/server.js", "app/.next/static", "VERSION",
               "systemd/nos-panel.service", "systemd/nos-panel-caddy.service")), sorted(os.listdir(opt)))
        # What install.sh does to the app: its cache the data's
        os.symlink("/var/lib/nos-panel/cache", os.path.join(opt, "app", ".next", "cache"))

        state = os.path.join(tmp, "state")
        etc = os.path.join(tmp, "etc")
        data = os.path.join(tmp, "data")
        logs = os.path.join(tmp, "logs")
        for d in (state, os.path.join(etc, "tls"), os.path.join(data, "cache"), os.path.join(data, "caddy"), logs):
            os.makedirs(d)
        for who in ("host", "admin", "reader"):
            openssl_identity(state, who)
        openssl_identity(os.path.join(etc, "tls"), "panel", "DNS:nos-panel")
        os.rename(os.path.join(etc, "tls", "panel.crt"), os.path.join(etc, "tls", "cert.pem"))
        os.rename(os.path.join(etc, "tls", "panel.key"), os.path.join(etc, "tls", "key.pem"))
        password = secrets.token_urlsafe(18)
        seed = {
            "host": "nos",
            "host_url": "127.0.0.1:%d" % API_PORT,
            "host_pin": pin_of(os.path.join(state, "host.crt")),
            "client_cert": open(os.path.join(state, "admin.crt")).read(),
            "client_key": open(os.path.join(state, "admin.key")).read(),
            "password_hash": scrypt_hash(password),
        }
        with open(os.path.join(etc, "seed.json"), "w") as f:
            json.dump(seed, f)

        port = free_port()
        argv = ["docker", "run", "-d", "--rm", "--name", name, "--platform", "linux/amd64",
                "-p", "127.0.0.1:%d:443" % port,
                "-v", "%s:/opt/nos-panel:ro" % opt, "-v", "%s:/src:ro" % ROOT, "-v", "%s:/etc/nos-panel:ro" % etc,
                "-v", "%s:/state" % state, "-v", "%s:/var/lib/nos-panel" % data, "-v", "%s:/logs" % logs,
                IMAGE, "sh", "-c", RUN % {"api_port": API_PORT}]
        subprocess.run(argv, check=True, capture_output=True)

        panel = Panel(port)
        start = time.time()
        up = False
        while time.time() - start < START_TIMEOUT:
            try:
                status, _, _ = panel.request("GET", "/en/login")
                if status == 200:
                    up = True
                    break
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(2)
        check("the panel answers through Caddy, by HTTPS on 443 (%d s)" % (time.time() - start), up,
              "\n".join(open(os.path.join(logs, f), errors="replace").read()[-1500:]
                        for f in sorted(os.listdir(logs))))
        if not up:
            return

        cert, alpn = panel.handshake()
        want = der_of(os.path.join(etc, "tls", "cert.pem"))
        check("TLS by the address alone, no server name, is answered with the certificate made for the panel",
              cert == want, hashlib.sha256(cert or b"").hexdigest())
        check("and HTTP/2 is offered and taken", alpn == "h2", alpn)

        status, cookie, body = panel.request("POST", "/api/login", {"user": "root", "password": password + "x"})
        check("a wrong password is refused", status == 401 and not cookie, (status, body))
        status, cookie, body = panel.request("POST", "/api/login", {"user": "root", "password": password},
                                             origin=False)
        check("a login from no page of the panel's is refused", status == 403 and not cookie, (status, body))
        status, cookie, body = panel.request("POST", "/api/login", {"user": "root", "password": password})
        check("root logs in with the password the seed's hash is of: a session, in a __Host- cookie, Secure, "
              "HttpOnly", status == 200 and cookie.startswith("__Host-nos-panel=") and "Secure" in cookie and
              "HttpOnly" in cookie, (status, cookie, body))
        session = cookie.split(";", 1)[0]

        # The host's API may come up after the panel, as nos's may: the
        # panel's own stream to it says when it is reached
        start = time.time()
        while True:
            status, _, body = panel.request("GET", "/api/hosts", cookie=session)
            hosts = json.loads(body).get("hosts", []) if status == 200 else []
            if (hosts and hosts[0].get("online")) or time.time() - start > START_TIMEOUT:
                break
            time.sleep(2)
        check("the panel manages the host its seed names, its key pinned as the seed has it",
              len(hosts) == 1 and hosts[0]["pin"] == seed["host_pin"] and hosts[0]["url"] == seed["host_url"],
              (status, body[:500]))
        check("and reaches it: its stream of the host's numbers is up (%d s)" % (time.time() - start),
              bool(hosts) and hosts[0].get("online") is True, (status, body[:500]))
        if hosts:
            status, _, body = panel.request("GET", "/api/h/%s/providers" % hosts[0]["id"], cookie=session)
            providers = json.loads(body).get("providers", []) if status == 200 else []
            names = [p.get("name") for p in providers]
            check("a call through the panel reaches the host API, by mTLS with the seed's identity",
                  status == 200 and "vm" in names and "system" in names, (status, body[:500]))
        status, _, body = panel.request("GET", "/api/hosts")
        check("and nothing of the hosts without a session", status == 401, (status, body[:200]))
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)
        if args.keep:
            print("panel-appliance-test: kept " + tmp)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    print("panel-appliance-test: %s" % ("PASS" if not failures else "%d FAILED" % len(failures)))


if __name__ == "__main__":
    main()
    sys.exit(1 if failures else 0)
