# The host API

What nos is managed through, by programs: the panel, `nosctl`, other hosts'
panels. It is a loadable module, `api.ko` (`src/crates/modules/api`), and
what it serves is the [management bus](mgmt.md): every provider registered
there -- the kernel's own, a module's -- with its methods, its descriptor and
its jobs. It runs no shell command, and no client can make it run one.

```
insmod /lib/modules/api.ko [conf=/etc/api]
api client add panel admin 3fe5cf7a...570d
api start
api                     what it serves, who is connected, what it refused
api stop
```

## Who is let in

HTTP/1.1 and a WebSocket, JSON, over **TLS 1.3 with a client's certificate
required** (`tls::server`, [HTTPS](tls.md)). A client is let in by its key
alone: the SHA-256 of the SubjectPublicKeyInfo of the certificate it shows,
which has to be in `clients`. The certificate's dates and issuer are not
looked at -- the list names the key, as `authorized_keys` does -- but the
handshake's signature is: the client has to hold the key it shows.

That is decided in the handshake, before a byte of what the client sends is
decrypted. A stranger, a client with no certificate, one that speaks TLS 1.2
or offers no protocol the API speaks by ALPN (`nos-api/1`, `http/1.1`) gets
TLS's alert and a reset; its bytes never reach the HTTP parser, and the
API's own count of requests does not move (`api-test.py` holds it to that).

The client pins the host's key the same way: the fingerprint `api start`,
`api status` and `api cert` print. There are no CAs on either side.

Before even the handshake, the interface's allowlist (`allow_<nic>`) and the
limits decide whether a connection is taken at all; past either, it is reset.

A client's **role** is in `clients` beside its key:

| Role | May |
|---|---|
| `read` | call the read methods, list the providers -- shown their reads alone -- read jobs, open the stream |
| `admin` | everything: actions, jobs, cancels |

What a method is, is what its provider registered it as on the bus, never
what a request says. The list is read again by `api reload`, and changed by
`api client add` and `api client del`: from the next handshake on, and a
connection whose key was taken off is a stranger's -- nothing it sends is
parsed, its next request is refused (403), and a stream of it is closed
(1008) within a quarter of a second.

## The folder

`/etc/api` unless `insmod api.ko conf=<folder>` names another; every file is
the machine's own:

| File | |
|---|---|
| `api.toml` | where it listens, and whom it takes (below) |
| `tls/key.pem`, `tls/cert.pem` | the host's key and certificate: P-256, made with `web::x509` on the first start -- refused while the wall clock is not set, since the certificate would start in 1970 -- for the addresses of eth0 and hv0 there are and the name `nos`, good for 825 days; never written over, since every client pins its key |
| `host` | the host's id, a UUID made with them: what a cluster's panel knows the host by, whatever its addresses |
| `clients` | the keys let in, a line each: `<fingerprint> <admin\|read> <name>`; written whole through the file ABI's replace, so a write cut short leaves the old list or the new |
| `audit.log` | a line for every call that changes something -- an action, a job started, a cancel: the wall clock, the client, its address, what, how it ended -- turned over to `audit.log.1` at a megabyte. Each is in the kernel's log too |

## api.toml

The strict TOML of `src/crates/conf`; every key optional, a key it does not
have or one given twice refused with its line:

```toml
port = 39234                 # the API's port; the panel's relay is the host's 39233
listen = ["hv0"]             # the interfaces: the guests' switch alone unless told
allow_eth0 = ["203.0.113.0/24", "198.51.100.7"]   # whom an interface takes; none said, any
max_connections = 8          # at once, in all
max_per_key = 4              # of one client's key
max_handshakes = 2           # in their handshake
idle_secs = 60               # a connection that says nothing this long is let go
```

`hv0` is made the first time hv loads, so its listener looks for it once a
second; a net device is never taken back, so the listener stays good through
hv's unloads and loads after. eth0 is for an external panel, `nosctl` from
another machine and a cluster's other hosts: on it the allowlist is the
first thing to set.

## The routes

| Route | |
|---|---|
| `GET /v1/host` | the host's id, version, revision, arch, module ABI, the API's version and capabilities, the wall clock, the uptime, its key -- and who is asking |
| `GET /v1/providers` | the bus's providers and their methods, with kinds; a reader's has the reads alone |
| `GET /v1/providers/<name>` | its descriptor, with an `ETag`; `If-None-Match` with it is a 304 |
| `POST /v1/call/<provider>/<method>` | a read or an action, the body its parameters (JSON); the answer as the provider gave it |
| `POST /v1/jobs/<provider>/<method>` | a job started: 202 and `{"id": N}` |
| `GET /v1/jobs` | every job's record |
| `GET /v1/jobs/<id>?notes_since=N` | one's record, its log from line N, its result once done |
| `POST /v1/jobs/<id>/cancel` | one told to stop |
| `GET /v1/stream` | the WebSocket, below |

A failure answers `{"error": {"code", "message", "field"}}` -- the provider's
own when it gave one -- with the HTTP status of its bus status: `invalid` 400,
`forbidden` 403, `no_provider`, `no_method` and `not_found` 404, `kind` (a job
called as a call, or the other way), `conflict` and `cancelled` 409,
`too_large` 413, `busy` 429 with `Retry-After`, `failed` 500, `unsupported`
501, `gone` 503. An unknown route is a 404, a method a route does not take a
405 with `Allow`, a request the parser refuses its own status (`web::http`),
a request larger than 74 KiB a 413, a request on a connection whose key was
taken off the list a 403; each of those last closes the connection. Their
code is the one the bus answers that status with -- `forbidden`,
`too_large`, `unsupported` -- or `invalid` where there is none (405, 414,
426, 431, 505): a client can go by the code alone.

Connections are kept alive. One task serves a connection in both directions,
with no poll or select: a read that waits a second at most, the answer put
into the session's queue in pieces, and the queue sent with waits of its own.
A client that stops reading holds up its own connection, and nothing else;
one that has taken nothing for its idle time is let go. The client closes
first: on its goodbye the API says its own and waits for the client's FIN
before it closes, so the slot left in TIME-WAIT is the client's. Anything else
-- an error, the idle time, the stop -- is a reset.

## The stream

`GET /v1/stream[?channels=metrics,log,jobs][&log_since=N]`, upgraded to a
WebSocket. Each message is a text frame of JSON, `{"channel": ...}`:

- `metrics`, once a second: `cpus` (each CPU's idle time), `memory` and `interfaces` (each one's counters), as `system` and `net` answer them -- a client makes its rates from two, and keeps its own history;
- `log`, every half second when there is something new: the `system.log` answer, from `log_since` or from now -- a gap in the messages' numbers is what the log lost;
- `jobs`, every job's record, whenever one has changed.

The client says nothing but its pongs and its close. The server pings it
every half of its idle time, and lets go of one that answers nothing for the
whole of it. Every channel is a read: either role may open it, while its key
is on the list. A guest's
console will be a channel of the `vm` provider's.

## The api provider

The module registers a provider of its own: `api.status` -- where it
listens, every connection and whose it is, what it has taken, refused and
reset, what its handshakes cost -- and `api.clients`. Both are reads, so the
panel shows them, and a test reads the counters through the API itself.

## nosctl

`scripts/nosctl`: the API from the command line, Python's standard library
alone.

```
nosctl keygen admin ~/.config/nos         # a key and a certificate (openssl); prints the fingerprint
nosctl fingerprint admin.crt              # a certificate's key, as the host pins a client by
export NOSCTL_HOST=203.0.113.5 NOSCTL_CERT=~/.config/nos/admin.crt \
       NOSCTL_KEY=~/.config/nos/admin.key NOSCTL_PIN=<the host's key>
nosctl host
nosctl call system info
nosctl start modtest nap; nosctl wait 5
nosctl stream metrics,log 30
```

A host showing a key other than the pinned one is refused before a request
is sent.

## What it costs

Measured by `api-test.py`, which prints them:

| | arm64, HVF (Apple M) | x86-64, TCG |
|---|---|---|
| a handshake, mean | 2.3 ms | 17 ms |
| the slowest | 6.8 ms | 85 ms |
| a connection task's stack, handshake and stream | 22.5 KiB of 64 KiB | |

`api.ko` is about 1.8 MB: rustls and the RustCrypto provider, which brings
RSA and P-384 the API never uses (a provider of TLS 1.3 with P-256 and
Ed25519 alone would take most of that away), and a quarter of a megabyte of
the symbol table backtraces name functions by.

## Tested

`scripts/api-test.py` (arm64, and `--arch x86_64` its short form):
docs/testing.md has what it checks. `net-fuzz.py`'s `api` target runs the
module's own source on the host, against clients of each role and of none
that ask for every route and break HTTP, the stream and TLS, while the
console changes the keys and stops the server: it holds every answer to a
model of the routes, the roles and the limits, and what ran to what an
admin asked for. Every boot's `test_module_bus` has
modtest ask the bus as a module does (`modtest bus`): the caller's side of
the bus's C ABI, which the API is built on.
