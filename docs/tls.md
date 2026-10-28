# HTTPS

`wget https://…` speaks TLS 1.3 (and 1.2), verifies the server's certificate
chain against the Mozilla root store, and streams the body to a file like any
other download. This page is how that works and what it costs.

## Why not hand-rolled

TLS is a certificate parser, a chain builder, a signature verifier, a key
exchange, an AEAD and a state machine. Writing that is weeks of work and the
kind of code where a mistake is a silent hole rather than a crash. The kernel
already had a Rust half with a global allocator, and rustls is I/O agnostic —
it never touches a socket itself — so it drops onto the existing TCP stack
without asking for anything the kernel does not have.

What the kernel supplies, and already had:

| TLS needs | comes from |
|---|---|
| a byte transport | the TCP of `src/crates/net` (`tcp.rs`) |
| randomness | the kernel's pool (`src/crates/random`, reached through `kcore::random`) — a ChaCha20 pool over RDSEED/RDRAND, RNDR, virtio-rng and timing jitter, see [Randomness](random.md) |
| a wall clock, for certificate validity | `get_wall_time_secs()` |
| a heap | the Rust global allocator over `mm::alloc` |

## The pieces

```
  wget                           src/crates/net/src/wget.rs
      |
  http::get_with_location        src/crates/net/src/http.rs
      |   Transport: send / recv
      |         |
    Plain      Tls
      |         |
      |     TlsStream<T>         src/crates/tls
      |         |  rustls, rustls-rustcrypto, webpki-roots
      |     T: tls::Transport    implemented by http.rs for its connection
      |         |
      +------  TCP               src/crates/net/src/tcp.rs
```

`Transport` is the seam: request framing, header parsing, chunked decoding,
redirects and the body sink all sit above it and neither know nor care
whether the bytes are encrypted. Adding TLS did not change any of them.

The connection belongs to the HTTP client -- `exchange` opens it and closes
it. `tls` is a crate of its own, *below* the network layer -- `net` depends
on it like on any other crate: `TlsStream::connect`, `send`, `recv`, and a
drop that says close_notify -- so it cannot name a connection of that layer.
It takes anything that implements its `Transport` trait (`send_all`, `recv`
with a timeout) instead, and `http.rs` hands it its connection as one. No C
ABI is involved: the session used to call back down through
`kernel_tcp_send` / `kernel_tcp_recv`, the names a loadable module reaches
TCP by, and those are now the modules' alone.

## The unbuffered API

There is no `std` in the kernel, so `read_tls`/`write_tls` — which take
`std::io` traits — are not available. The `tls` crate drives rustls'
unbuffered API instead: `process_tls_records` returns one state at a time
(`EncodeTlsData`, `TransmitTlsData`, `BlockedHandshake`, `ReadTraffic`,
`WriteTraffic`, `Closed`) and the loop in `src/crates/tls/src/lib.rs` does the
reading, the writing and the buffering that `std::io` would have done.

One rule matters more than the rest: **call `process_tls_records` once per
turn.** It consumes records as it goes and reports how many bytes to discard;
calling it a second time to ask a question (say, "are we blocked?") processes
the next record twice, and the connection dies a round trip later with
`InappropriateMessage { got_type: ChangeCipherSpec }`. The state machine
returns `NeedRead` instead, and the socket read happens after the borrow ends.

## Certificates

Trust anchors come from `webpki-roots` (the Mozilla set, compiled in), and
validity is checked against the RTC. A machine with a wrong clock rejects
every certificate it is shown, which is the safe direction to fail in. There
is no way to skip verification — no `--insecure` — and adding one would be a
poor trade for a kernel whose only network client is `wget`.

Rejections are reported with their reason in the kernel log:

```
$ wget https://expired.badssl.com
wget: TLS handshake refused -- bad certificate, or no protocol in common
$ dmesg 3
tls: InvalidCertificate(ExpiredContext { time: UnixTime(1788984857),
                                         not_after: UnixTime(1428883199) })
```

## The server, for the host API

`tls::server` (the `server` feature, which the API module's build turns on and
the kernel's own does not) is the other side: a TLS server for the
[host API](mgmt.md), whose clients are programs -- the panel, `nosctl`, the
panels of other hosts -- each holding a key the host was given.

- **TLS 1.3 alone, and a client's certificate required.** A client is known
  by its key: the SHA-256 of the SubjectPublicKeyInfo of the certificate it
  shows (`tls::server::fingerprint`), which has to be on the list the
  server was made with. The certificate's dates, issuer and chain are not
  looked at -- the list names the key, as `authorized_keys` does -- but the
  handshake's signature is, with the provider's algorithms
  (`verify_tls13_signature`): the client has to hold the key it showed.
- **Decided in the handshake.** A client whose key is not on the list, that
  shows none, speaks TLS 1.2, offers no protocol the server speaks by ALPN,
  or breaks the protocol, is refused there: rustls' alert goes out within a
  second, and `accept` hands its socket back with why (`Refused`) for the
  caller to reset. With a client's certificate asked for, rustls sends no
  data before the client's Finished, so nothing of what the server says
  reaches a client it refuses -- and `accept` returns no session, so nothing
  the client says reaches whoever reads one. A handshake that has not
  finished in 10 s (`HANDSHAKE_MS`) is let go.
- **No resumption.** No tickets and no session cache: every connection
  shows its key afresh, and a list that has changed holds from the next
  handshake on. The list is the server's, fixed when it is made
  (`Server::new`); the API module makes the server again when it changes,
  and a connection keeps the server it was accepted by.
- **Every wait bounded.** The session runs over a `tls::server::Socket`,
  whose reads and sends each wait at most what they are given. `read` with
  nothing in its time answers `Idle`, which is no error; `write` encrypts
  into the session's own queue, bounded by what the caller allows, and
  `flush` sends from it for as long as it is given -- so a client that stops
  reading holds up its own connection's task and nobody else.
- **The way out.** `close` sends a close_notify and hands the socket back to
  be closed, after the client's own goodbye, so the server is never the
  side left in TIME-WAIT; `into_socket` hands it back for a reset. A
  client's close_notify closes its side alone (TLS 1.3, RFC 8446 6.1):
  `read` answers `End` once what it said before is read, and `write` still
  takes the answer to it -- a client that says goodbye with its last
  request, before the answer, is answered, and then told goodbye.

Its gate is `net-fuzz.py`'s `tls_server` target (docs/testing.md), which
runs it as the module will -- a listener and a task a connection, over the
kernel's TCP -- against the world's rustls clients; and a build of
`tls --features server` for both kernel targets, since nothing in the
kernel's own build compiles it. `pub mod server` is the last thing in
`lib.rs`: with it anywhere above the client, the client's line numbers
move -- in its debug info and its panics' locations, compiled into the
network layer that uses it -- and the kernel's image, which has the feature
off, would no longer come out the same to the byte.

## Building: two things to know

**External crates.** These are the first dependencies in the tree that are not
local paths: `rustls`, `rustls-rustcrypto`, `webpki-roots`, `getrandom` and
what they pull in. Their sources are vendored in `src/crates/vendor` and the
build runs `cargo --offline`, so nothing is ever fetched during a build. After
changing a dependency, run `scripts/vendor.sh` (which does need the network,
once) and commit the result with `Cargo.lock`.

The vendor directory is 31 MB rather than the 143 MB `cargo vendor` produces
on its own: the lock file holds packages no target compiles — `ring`, an
optional rustls backend we do not use, and through it the Windows import
libraries, `wasi` and `libc` — and cargo needs their manifests to resolve but
never their code, so the script empties them. Emptied, not deleted: cargo
still looks for the source files the manifest names.

**Software crypto only.** The RustCrypto crates dispatch to SSE2/AVX2 at
runtime and compile those paths unconditionally, which LLVM cannot legalize
for a target with SSE off — it fails with `Do not know how to split the result
of this operator`. The kernel keeps SSE off on purpose: it saves no XMM state
on a context switch, so SSE in kernel code would corrupt userland-visible
state the moment there is any. `src/crates/.cargo/config.toml` therefore sets
the crates' own switches to the portable backends:

```
--cfg aes_force_soft --cfg polyval_force_soft --cfg poly1305_force_soft
--cfg chacha20_force_soft --cfg curve25519_dalek_backend="serial"
```

`sha2` has no such `--cfg`: left alone it picks SHA-NI by CPUID at run time,
and on an application processor, which runs with `CR4.OSFXSR` clear, the
first block hashed is an invalid opcode. It takes a feature instead, so
`kernel/Cargo.toml` asks for `force-soft` -- which reaches the TLS client
too, since a crate is built once for the whole graph -- and so does
`ssh/Cargo.toml`, because a module is a build of its own that the kernel's
features do not reach.

`getrandom` has no backend for a bare-metal target either; the `tls` crate
registers the kernel's entropy pool as its custom one. That pool is what makes
a handshake work on a machine with no virtio-rng: `FailedToGetRandomBytes` from
rustls means `kernel_get_random` returned zero, i.e. nothing had seeded the
pool -- `entropy` in the shell says which sources exist.

## What it costs

- **Kernel image**: about 4 MB, most of it the root store and the crypto.
- **Task stacks**: 64 KB, up from 32 KB. Verifying a chain walks candidate
  issuers recursively and goes deeper than anything else a task does — the
  shell task peaks at ~29 KB during a handshake, against ~4 KB before. A task
  stack that overflows takes the machine down with a `get_current_task` BUG, so
  the headroom is not optional. `stacks` prints the high-water marks.
- **Throughput**: software ChaCha20-Poly1305, several MB/s under QEMU —
  bounded by the 8 KB TCP receive window long before the cipher.
