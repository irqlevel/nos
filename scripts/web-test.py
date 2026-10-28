#!/usr/bin/env python3
"""The host API's wire crates on the host: src/crates/web and
src/crates/mgmtwire, as the API module links them (fuzz/web).

1. Known answers: SHA-1's FIPS 180 vectors, RFC 6455's accept key and
   frames, requests, frames, JSON texts, parameters, networks and
   certificates, each with what it must make or why it must be refused.
2. Every parser fed what a generator makes and then breaks: no panic, what
   its writer wrote taken back, a prefix of anything whole "not yet", the
   limits it was given kept.
3. The certificates web::x509 makes, judged by others: OpenSSL reads each,
   its extensions and dates as they were asked for, its fingerprint and its
   key's as web::x509 says; Python's ssl serves TLS with the server's
   certificate and key -- which it refuses unless the key is the
   certificate's -- to a client that trusts it and checks its name, at the
   address and at `nos`; and a client's certificate is taken by a server
   that asks for one.
4. The providers' descriptors in the tree (src/crates/**/*.descriptor.json):
   each as the bus checks it at registration, for the methods it says it
   has -- the kernel then holds those to what the provider registers -- and
   each text read by Python too: no key twice, every title, help, confirm
   and enum value in English and Russian, none of them empty, and no
   sentence left untranslated.

Usage: python3 scripts/web-test.py [--seed N] [--iters N]
Needs cargo and openssl on the host; the crates' dependencies are the
kernel's vendored ones, so it builds offline. Exit code 0 = every check
held; 1 = one did not.
"""
import glob
import hashlib
import json
import os
import re
import socket
import ssl
import subprocess
import sys
import tempfile
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, 'out', 'web-test')
VENDOR = os.path.join(ROOT, 'src', 'crates', 'vendor')

failures = []


def check(what, ok, detail=''):
    print('%s: %s%s' % ('PASS' if ok else 'FAIL', what, '' if ok else ' -- %s' % detail))
    if not ok:
        failures.append(what)


def build():
    # The kernel's vendored crates, as net-fuzz takes them: the manifest is
    # outside src/crates, whose .cargo/config.toml would make any build under
    # it a kernel build.
    return subprocess.run(['cargo', 'build', '--release', '--offline', '--quiet',
                           '--manifest-path', os.path.join(ROOT, 'fuzz', 'web', 'Cargo.toml'),
                           '--target-dir', OUT,
                           '--config', 'source.crates-io.replace-with="vendored-sources"',
                           '--config', 'source.vendored-sources.directory="%s"' % VENDOR],
                          cwd=ROOT).returncode


def openssl(*args, data=None):
    p = subprocess.run(['openssl'] + list(args), input=data, capture_output=True)
    return p.returncode, p.stdout, p.stderr


def handshake(server_ctx, client_ctx, name):
    """A TLS handshake over a socket pair; None, or why it failed."""
    a, b = socket.socketpair()
    result = {}

    def serve():
        try:
            with server_ctx.wrap_socket(a, server_side=True) as s:
                s.sendall(s.recv(16))
        except Exception as e:  # noqa: BLE001 -- the reason is the finding
            result['server'] = repr(e)

    t = threading.Thread(target=serve)
    t.start()
    try:
        with client_ctx.wrap_socket(b, server_hostname=name) as c:
            c.sendall(b'ping')
            got = c.recv(16)
            if got != b'ping':
                result['client'] = 'echo %r' % got
    except Exception as e:  # noqa: BLE001
        result['client'] = repr(e)
    t.join(10)
    return result or None


def judge(d):
    said = {}
    for line in open(os.path.join(d, 'said.txt')):
        name, _, fp, _, spki = line.split()
        said[name] = (fp, spki)

    for name in ('server', 'client', 'late'):
        crt = os.path.join(d, name + '.crt')
        code, text, err = openssl('x509', '-in', crt, '-noout', '-text')
        text = text.decode(errors='replace')
        check('OpenSSL reads the %s certificate' % name, code == 0, err.decode(errors='replace'))
        check('%s: version 3, ECDSA with SHA-256 over P-256' % name,
              'Version: 3' in text and 'ecdsa-with-SHA256' in text and ('prime256v1' in text or 'P-256' in text), text)
        check('%s: no CA, signing only, critical' % name,
              re.search(r'Basic Constraints: critical\s+CA:FALSE', text) is not None
              and re.search(r'Key Usage: critical\s+Digital Signature', text) is not None, text)
        usage = 'TLS Web Client Authentication' if name == 'client' else 'TLS Web Server Authentication'
        check('%s: for %s' % (name, usage), usage in text, text)
        if name != 'client':
            check('%s: for 10.0.100.1, 65.108.38.184 and nos' % name,
                  'IP Address:10.0.100.1' in text and 'IP Address:65.108.38.184' in text and 'DNS:nos' in text, text)
        code, out, _ = openssl('x509', '-in', crt, '-noout', '-fingerprint', '-sha256')
        fp = out.decode().strip().split('=', 1)[-1]
        check('%s: the fingerprint OpenSSL gives is the one web::x509 gives' % name,
              code == 0 and fp.upper() == said[name][0].upper(), (fp, said[name][0]))
        code, pub, _ = openssl('x509', '-in', crt, '-noout', '-pubkey')
        code2, der, _ = openssl('pkey', '-pubin', '-outform', 'DER', data=pub)
        check('%s: its key\'s SubjectPublicKeyInfo hashes as web::x509 says' % name,
              code == 0 and code2 == 0 and hashlib.sha256(der).hexdigest() == said[name][1],
              (hashlib.sha256(der).hexdigest(), said[name][1]))
        code, _, err = openssl('pkey', '-in', os.path.join(d, name + '.key'), '-noout')
        check('%s: OpenSSL reads its PKCS#8 key' % name, code == 0, err.decode(errors='replace'))

    code, text, _ = openssl('x509', '-in', os.path.join(d, 'late.crt'), '-noout', '-enddate')
    check('a certificate valid into 2050 ends then (a GeneralizedTime)',
          code == 0 and 'Dec 31 23:59:59 2050 GMT' in text.decode(), text)

    server = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    try:
        server.load_cert_chain(os.path.join(d, 'server.crt'), os.path.join(d, 'server.key'))
        check('Python\'s ssl takes the server\'s key as its certificate\'s', True)
    except ssl.SSLError as e:
        check('Python\'s ssl takes the server\'s key as its certificate\'s', False, e)
        return
    for name in ('nos', '10.0.100.1'):
        client = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        client.load_verify_locations(os.path.join(d, 'server.crt'))
        if hasattr(ssl, 'VERIFY_X509_PARTIAL_CHAIN'):
            client.verify_flags |= ssl.VERIFY_X509_PARTIAL_CHAIN
        failed = handshake(server, client, name)
        check('TLS to the server\'s certificate, trusted, its name %s checked' % name, failed is None, failed)
    client = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    client.load_verify_locations(os.path.join(d, 'server.crt'))
    failed = handshake(server, client, 'other')
    check('a name the certificate is not for is refused', failed is not None, failed)

    mutual = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    mutual.load_cert_chain(os.path.join(d, 'server.crt'), os.path.join(d, 'server.key'))
    mutual.verify_mode = ssl.CERT_REQUIRED
    mutual.load_verify_locations(os.path.join(d, 'client.crt'))
    if hasattr(ssl, 'VERIFY_X509_PARTIAL_CHAIN'):
        mutual.verify_flags |= ssl.VERIFY_X509_PARTIAL_CHAIN
    client = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    client.load_verify_locations(os.path.join(d, 'server.crt'))
    if hasattr(ssl, 'VERIFY_X509_PARTIAL_CHAIN'):
        client.verify_flags |= ssl.VERIFY_X509_PARTIAL_CHAIN
    client.load_cert_chain(os.path.join(d, 'client.crt'), os.path.join(d, 'client.key'))
    failed = handshake(mutual, client, 'nos')
    check('a client\'s certificate taken by a server that asks for one', failed is None, failed)


# A text the descriptor format has: the member names whose values are texts,
# and how long a Russian text may be and still be the English one (an
# abbreviation: PID, MAC, Nice).
TEXT_MEMBERS = ('title', 'help', 'confirm')
SAME_MAX = 6


def judge_texts(path):
    def no_twice(pairs):
        keys = [k for k, _ in pairs]
        if len(keys) != len(set(keys)):
            raise ValueError('a key twice: %s' % keys)
        return dict(pairs)

    name = os.path.relpath(path, ROOT)
    try:
        with open(path, encoding='utf-8') as f:
            doc = json.load(f, object_pairs_hook=no_twice)
    except (ValueError, UnicodeDecodeError) as e:
        check('%s read by Python' % name, False, e)
        return
    bad = []

    def text(t, where):
        if not isinstance(t, dict) or set(t) != {'en', 'ru'} or not all(isinstance(t[l], str) and t[l] for l in t):
            bad.append('%s is not a text in en and ru' % where)
        elif t['en'] == t['ru'] and len(t['en']) > SAME_MAX:
            bad.append('%s is not translated: %r' % (where, t['en']))

    def walk(v, where):
        if isinstance(v, dict):
            for k, child in v.items():
                if k in TEXT_MEMBERS:
                    text(child, '%s.%s' % (where, k))
                elif k == 'values' and isinstance(child, dict):
                    for value, t in child.items():
                        text(t, '%s.values.%s' % (where, value))
                else:
                    walk(child, '%s.%s' % (where, k))
        elif isinstance(v, list):
            for i, child in enumerate(v):
                walk(child, '%s[%d]' % (where, i))

    walk(doc, doc.get('provider', '?') if isinstance(doc, dict) else '?')
    check('%s: every text in English and Russian' % name, not bad, '; '.join(bad[:5]))


def main():
    seed, iters = '1', '20000'
    args = sys.argv[1:]
    if '--seed' in args:
        seed = args[args.index('--seed') + 1]
    if '--iters' in args:
        iters = args[args.index('--iters') + 1]
    if build() != 0:
        print('web-test: the build failed')
        return 1
    exe = os.path.join(OUT, 'release', 'web-test')
    check('the known answers', subprocess.run([exe, 'vectors']).returncode == 0)
    check('the parsers fuzzed, seed %s' % seed,
          subprocess.run([exe, 'fuzz', '--seed', seed, '--iters', iters]).returncode == 0)
    descriptors = sorted(glob.glob(os.path.join(ROOT, 'src', 'crates', '**', '*.descriptor.json'), recursive=True))
    check('the providers\' descriptors in the tree found', len(descriptors) >= 4, descriptors)
    check('each descriptor as the bus checks it', subprocess.run([exe, 'descriptor'] + descriptors).returncode == 0)
    for path in descriptors:
        judge_texts(path)
    with tempfile.TemporaryDirectory() as d:
        made = subprocess.run([exe, 'cert', d]).returncode == 0
        check('the certificates made', made)
        if made:
            judge(d)
    print('web-test: %s' % ('every check held' if not failures else '%d failed' % len(failures)))
    return 0 if not failures else 1


if __name__ == '__main__':
    sys.exit(main())
