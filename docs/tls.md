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
| a byte transport | `Tcp::Send` / `Tcp::Recv` (`net/tcp.cpp`) |
| randomness | `EntropySourceTable` — virtio-rng, RDRAND |
| a wall clock, for certificate validity | `GetWallTimeSecs()` |
| a heap | the Rust global allocator over `Mm::Alloc` |

## The pieces

```
  cmd.cpp  wget
      |
  HttpClient                     net/http.cpp
      |   HttpTransport: Send / Recv
      |         |
  TcpTransport  TlsTransport     net/http.cpp
                    |
                TlsConn          net/tls.h, net/tls.cpp
                    |  tls_connect / tls_send / tls_recv / tls_close
                TlsStream        src/rust/tls
                    |  rustls, rustls-rustcrypto, webpki-roots
                TcpSocket -> kernel_tcp_send / kernel_tcp_recv
                    |
                  Tcp            net/tcp.cpp
```

`HttpTransport` is the seam: request framing, header parsing, chunked
decoding, redirects and the body sink all sit above it and neither know nor
care whether the bytes are encrypted. Adding TLS did not change any of them.

The connection belongs to C++ — `HttpClient::DoGet` opens it and closes it.
Rust borrows it for the session and calls back down through
`kernel_tcp_send` / `kernel_tcp_recv` (`kernel/rust_ffi.cpp`).

## The unbuffered API

There is no `std` in the kernel, so `read_tls`/`write_tls` — which take
`std::io` traits — are not available. The `tls` crate drives rustls'
unbuffered API instead: `process_tls_records` returns one state at a time
(`EncodeTlsData`, `TransmitTlsData`, `BlockedHandshake`, `ReadTraffic`,
`WriteTraffic`, `Closed`) and the loop in `src/rust/tls/src/lib.rs` does the
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

## Building: two things to know

**External crates.** These are the first dependencies in the tree that are not
local paths: `rustls`, `rustls-rustcrypto`, `webpki-roots`, `getrandom` and
what they pull in. A clean build fetches them from crates.io, so the build
host (or the Docker container) needs network access the first time.

**Software crypto only.** The RustCrypto crates dispatch to SSE2/AVX2 at
runtime and compile those paths unconditionally, which LLVM cannot legalize
for a target with SSE off — it fails with `Do not know how to split the result
of this operator`. The kernel keeps SSE off on purpose: it saves no XMM state
on a context switch, so SSE in kernel code would corrupt userland-visible
state the moment there is any. `src/rust/.cargo/config.toml` therefore sets
the crates' own switches to the portable backends:

```
--cfg aes_force_soft --cfg polyval_force_soft --cfg poly1305_force_soft
--cfg chacha20_force_soft --cfg curve25519_dalek_backend="serial"
```

`getrandom` has no backend for a bare-metal target either; the `tls` crate
registers the kernel's entropy pool as its custom one.

## What it costs

- **Kernel image**: about 4 MB, most of it the root store and the crypto.
- **Task stacks**: 64 KB, up from 32 KB. Verifying a chain walks candidate
  issuers recursively and goes deeper than anything else a task does — the
  shell task peaks at ~29 KB during a handshake, against ~4 KB before. A task
  stack that overflows takes the machine down with a `GetCurrentTask` BUG, so
  the headroom is not optional. `stacks` prints the high-water marks.
- **Throughput**: software ChaCha20-Poly1305, several MB/s under QEMU —
  bounded by the 8 KB TCP receive window long before the cipher.
