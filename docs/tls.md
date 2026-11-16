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
| a byte transport | the TCP of `src/crates/layers/net` (`tcp.rs`) |
| randomness | the kernel's pool (`src/crates/base/random`, reached through `kcore::random`) — a ChaCha20 pool over RDSEED/RDRAND, RNDR, virtio-rng and timing jitter, see [Randomness](random.md) |
| a wall clock, for certificate validity | `get_wall_time_secs()` |
| a heap | the Rust global allocator over `mm::alloc` |

## The pieces

```
  wget                           src/crates/layers/net/src/wget.rs
      |
  http::get_with_location        src/crates/layers/net/src/http.rs
      |   Transport: send / recv
      |         |
    Plain      Tls
      |         |
      |     TlsStream<T>         src/crates/layers/tls
      |         |  rustls, rustls-rustcrypto, webpki-roots
      |     T: tls::Transport    implemented by http.rs for its connection
      |         |
      +------  TCP               src/crates/layers/net/src/tcp.rs
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
`WriteTraffic`, `Closed`) and the loop in `src/crates/layers/tls/src/lib.rs` does the
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

## Both ends pinned: the host API's server, a host's client

`tls::pinned` (the `pinned` feature, which the API module's build turns on
and the kernel's own does not) is TLS between ends that know each other's
keys. Its server is the [host API](mgmt.md)'s, whose clients are programs --
the panel, `nosctl`, the panels of other hosts -- each holding a key the
host was given; its client is how one host is to reach another's (ipcnet,
plans/ipc.md I4), the server known by its key. The two share one session
(`Session`, a `ServerStream` or a `ClientStream`), and everything below of
the server holds of the client turned around, but where the client is said
apart.

- **TLS 1.3 alone, and a client's certificate required.** A client is known
  by its key: the SHA-256 of the SubjectPublicKeyInfo of the certificate it
  shows (`tls::pinned::fingerprint`), which has to be on the list the
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
- **Every wait bounded.** The session runs over a `tls::pinned::Socket`,
  whose reads and sends each wait at most what they are given. `read` with
  nothing in its time answers `Idle`, which is no error; `write` encrypts
  into the session's own queue, bounded by what the caller allows, and
  `flush` sends from it for as long as it is given -- so a client that stops
  reading holds up its own connection's task and nobody else.
- **A failure says why.** `read`'s `Failed`, a `write` that is refused and
  `flush`'s `Failed` all end the session, and `failure` says what ended it,
  the first cause kept (`tls::pinned::Failure`): the connection under it
  (reset, or given up on), the stream cut inside a record, a record that
  does not decrypt -- its bytes changed on the way -- or the other end's
  alert that one of this end's did not, another alert, the protocol
  broken, no memory. A reset and a corrupted record end a session alike,
  and only this tells them apart; ipcnet logs it as the reason a peer was
  lost.
- **The way out.** `close` sends a close_notify and hands the socket back to
  be closed, after the client's own goodbye, so the server is never the
  side left in TIME-WAIT; `into_socket` hands it back for a reset. A
  client's close_notify closes its side alone (TLS 1.3, RFC 8446 6.1):
  `read` answers `End` once what it said before is read, and `write` still
  takes the answer to it -- a client that says goodbye with its last
  request, before the answer, is answered, and then told goodbye.
- **The client.** `Client::new` takes the client's certificate and key, the
  keys of the servers it lets serve it and the protocols it offers;
  `connect` runs the handshake over a socket somebody connected, given the
  server's address -- the name rustls is told, which sends no SNI for an
  address and whose certificate is judged by its key alone. A server whose
  key is not on the list is `UnknownKey`, one whose certificate is the
  right one but whose handshake is signed by another key `BadCertificate`,
  one that gives up on the client -- its alert -- `ServerAlert`. TLS 1.3
  ends the client's handshake with its own Finished, before the server has
  judged the client's certificate: a server that refuses the client's key
  does so with an alert the session's first read then meets, and the
  client has said nothing yet that anyone read. The client closes first --
  its close_notify, and its FIN without waiting for the server's -- so it
  is the end left in TIME-WAIT, as the server expects of its clients; two
  ends each waiting for the other's FIN would both give up and reset.

Its gates are `net-fuzz.py`'s `tls_server` and `tls_client` targets
(docs/testing.md), which run each end as a module will -- over the kernel's
TCP -- against the world's rustls clients and servers; and a build of
`tls --features pinned` for both kernel targets, since nothing in the
kernel's own build compiles it (the kernel's build of either arch makes one,
building api.ko). `pub mod pinned` is the last thing in
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

**RustCrypto's portable code, and CRYPTOGAMS' assembly in a SIMD
section.** The RustCrypto crates dispatch to SSE2/AVX2 at runtime and
compile those paths unconditionally, which LLVM cannot legalize for a target
with SSE off — it fails with `Do not know how to split the result of this
operator`. The kernel keeps its compilers off SSE on purpose: it saves no
vector state on a context switch, and lends the registers only to a SIMD
section, in which ChaCha20-Poly1305 runs on CRYPTOGAMS' assembly ([The
assembly](#the-assembly)); SSE anywhere else would work on registers that
may hold a guest's. `src/crates/.cargo/config.toml` therefore sets the
crates' own switches to the portable backends:

```
--cfg aes_force_soft --cfg polyval_force_soft --cfg poly1305_force_soft
--cfg chacha20_force_soft --cfg curve25519_dalek_backend="serial"
```

`sha2` has no such `--cfg`: left alone it picks SHA-NI by CPUID at run time,
SSE instructions outside a section -- which no longer fault, `CR4.OSFXSR`
being set at boot for the sections, and which the link refuses
(`scripts/check-simd.py`). It takes a feature instead, so
`kernel/Cargo.toml` asks for `force-soft` -- which reaches the TLS client
too, since a crate is built once for the whole graph -- and so does
`common/net/ssh/Cargo.toml`, because a module is a build of its own that the kernel's
features do not reach.

`getrandom` has no backend for a bare-metal target either; the `tls` crate
registers the kernel's entropy pool as its custom one. That pool is what makes
a handshake work on a machine with no virtio-rng: `FailedToGetRandomBytes` from
rustls means `kernel_get_random` returned zero, i.e. nothing had seeded the
pool -- `entropy` in the shell says which sources exist.

## The assembly

ChaCha20-Poly1305 -- TLS 1.3's suite and both of TLS 1.2's -- runs on
CRYPTOGAMS' assembly: the ChaCha20 and Poly1305 Andy Polyakov wrote for
OpenSSL, from his own distribution of them (`src/crates/base/crypto`).
`tls::provider` keeps everything else of rustls-rustcrypto's suites -- the
hash, HKDF and PRF, the key exchange, the signatures -- and AES-GCM stays
on RustCrypto's portable code.

**Where it comes from, and under what license.**
`src/crates/base/crypto/cryptogams/` holds the perlasm as it came -- the
two algorithms for x86-64 and ARMv8 and the two translators -- at the
commit its README names, each file's SHA-256 beside it, and the
repository's `LICENSE`: BSD-style, or the GPL at the taker's choice. This
tree takes the BSD-style one, which an MIT kernel can carry, and keeps its
three conditions: the notice with every copy of the source (the files
themselves, and at the top of each one `scripts/cryptogams.sh` makes), the
notice with every binary handed out (a release's `LICENSE-CRYPTOGAMS`, the
system image's `/lib/licenses/CRYPTOGAMS`), and no endorsement. Not Linux's
copies of the same code, which are GPL-2.0, nor OpenSSL's, which are its
author's work under the project's license rather than his own.
`scripts/cryptogams.sh` makes `src/asm/` of them -- x86-64 ELF, arm64 ELF
and Mach-O (a Mac's fuzzers) -- in perlasm's kernel mode, AVX2 in and
AVX-512 out; `make check` runs it with `--check`, so what is built is what
the sources say.

**The SIMD section.** The kernel saves no vector state on a context switch,
and its code is built to touch none: its C++ with `-mno-sse -mno-80387` or
`-mgeneral-regs-only`, its Rust soft-float. The unit is on all the same
(`hal::simd_setup_cpu`, every CPU at its bring-up: x86-64's CR4.OSFXSR and
OSXMMEXCPT, and OSXSAVE with XCR0 the x87, SSE and AVX where the CPU has
AVX; arm64's CPACR_EL1.FPEN, and CPTR_EL2.TFP clear when booted at EL2),
for one thing: a SIMD section (`kernel/simd.h`, `kcore::simd::Section`),
in which the vector registers are the holder's -- preemption off while it
is open, one at a time on a CPU, so an interrupt handler that asks for one
then is refused and runs the integer code; every register zeroed as it
closes; and a guest's registers on that CPU marked gone, so that hv puts
them back at the guest's next entry rather than skip the load
([The hypervisor](hypervisor.md)). The crypto crate opens one for each
16 KiB of a message, so preemption is off for some microseconds at a time.

**What keeps the rest of the kernel off the registers.** Nothing at run
time any more -- `CR4.OSFXSR`, which once made an SSE instruction fault, is
set at boot -- so the build: the compilers are told to use none, the
RustCrypto crates are forced onto their portable code (above), and
`scripts/check-simd.py` holds every kernel and module as it is linked --
the link fails on a vector, x87, MMX or mask register named outside
CRYPTOGAMS' functions and `hal::simd_leave`.

**The paths.** x86-64: AVX2's ChaCha20 for more than 128 bytes and
Poly1305; SSSE3's ChaCha20 and AVX's Poly1305; SSSE3's alone; arm64:
NEON's ChaCha20 for 192 bytes and more and Poly1305 for 128; each
architecture's integer code where no section can be had -- the same choice
OpenSSL's own dispatch makes on the same CPU, from what every CPU's
sections may use. A boot says which: `crypto: ChaCha20-Poly1305 on
CRYPTOGAMS' AVX2 code`.

**What holds it.** Every boot, `crypto::selftest`: RFC 8439's known answers
on every path the CPU has, each path against the integer code over the
lengths where the assembly changes how it works -- a block, the 128, 192,
256 and 512 bytes its wider paths start at, a 16 KiB piece -- at three
alignments, in place, and a forged tag refused with the message left as it
was; and `test_simd_section`. `tests/net-fuzz.py`: its `aead` target holds
the crypto crate to RustCrypto's portable code on every path the host has,
and its TLS targets the kernel's suites to RustCrypto's at the world's end
of the connection. `tests/hv-test.py`'s `avx` guest: a section of the
host's before each of its CPUs' entries, and its registers its own at
every one of its turns. And every link (`scripts/check-simd.py`).

## What it costs

- **Kernel image**: about 4 MB, most of it the root store and the crypto.
- **Task stacks**: 64 KB, up from 32 KB. Verifying a chain walks candidate
  issuers recursively and goes deeper than anything else a task does — the
  shell task peaks at ~29 KB during a handshake, against ~4 KB before. A task
  stack that overflows takes the machine down with a `get_current_task` BUG, so
  the headroom is not optional. `stacks` prints the high-water marks.
- **Throughput**: software ChaCha20-Poly1305, several MB/s under QEMU —
  bounded by the 8 KB TCP receive window long before the cipher.
- **The cipher, measured** (`tlsbench`, below): both pinned ends in one
  kernel over memory, 64 MB each way, nos under KVM on the AX41's Zen 2
  (2026-11-05, before the assembly below: every suite then ran on its
  portable backend, as AES-GCM still does, with no AES-NI):

  | Suite | sender and receiver on two CPUs | both on one | handshake |
  |---|---|---|---|
  | AES-128-GCM | 92 MB/s | 49 MB/s | 2.4 ms |
  | AES-256-GCM | 82 MB/s | 42 MB/s | 1.5–2.4 ms |
  | ChaCha20-Poly1305 | 203 MB/s | 119 MB/s | 1.2 ms |

  Only ChaCha20-Poly1305 carries a 1 GbE link (117 MB/s) with room to
  spare, so every configuration the crate makes asks for it first
  (`tls::provider`): the HTTP client offers it before AES-GCM -- in TLS
  1.3 and 1.2 alike -- and a server that follows its client's order, as
  most do, agrees on it; the kernel's own server (`pinned`: the host
  API's, ipcnet's) holds its clients to its order rather than theirs, so
  a client with AES-NI, which puts AES-GCM first -- the panel's Node,
  nosctl's OpenSSL -- gets ChaCha20-Poly1305 too. The net fuzzer holds
  both: the world's server, following its client, agrees on
  ChaCha20-Poly1305 with the kernel's HTTP client, and the kernel's server
  on it with a client that asks for AES-GCM first. On an Apple M-series
  core under HVF, arm64's: AES-GCM 160–195 MB/s, ChaCha20 440.
- **ChaCha20-Poly1305 on CRYPTOGAMS' assembly** ([The
  assembly](#the-assembly)), measured against the tree before it on the
  same machine, the two taking turns twice (`tlsbench mb=64`, arm64 on an
  Apple M-series under HVF, 2 CPUs, 2026-11-16):

  | | RustCrypto's portable code | CRYPTOGAMS' NEON |
  |---|---|---|
  | a TLS 1.3 session, client->server | 406, 399 MB/s | 1012, 967 MB/s |
  | a TLS 1.3 session, server->client | 404, 398 MB/s | 1014, 965 MB/s |
  | the cipher alone, one CPU, 16 KB records | seal 547, 542 MB/s | seal 2020, 1988; open 2036, 2045 MB/s |

  The session two and a half times as fast, the cipher alone 3.7 times:
  what is left is the session's own work -- its records copied in and out,
  the two ends' tasks -- which the last line of `tlsbench` shows. AES-GCM's,
  on code that did not change, came out a tenth lower client->server and the
  same server->client in both rounds. x86-64's under KVM on the servers is
  still to be measured.
  `tests/tlsbench.py` boots nos with the module and runs it: under KVM
  on a server -- nos a guest, nothing rebooted -- it says what the cores
  give; under TCG, nothing worth quoting.
