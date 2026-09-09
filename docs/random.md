# Randomness

Everything in the kernel that needs random bytes asks `Kernel::Random`
(`kernel/random.h`): the TLS client for its handshake keys, the shell's
`random` command, anything later that needs a token or a nonce. It is one
ChaCha20 generator, seeded from every entropy source the machine turns out to
have. This page is what those sources are, what the generator does with them,
and what is and is not being claimed.

## What this replaced

There used to be no generator, only the source registry: `EntropySource`
implementations registered with `EntropySourceTable`, and a caller took
`GetDefault()` — whichever had registered first — and read it directly. Only
one source was ever implemented, virtio-rng, and that is a device a hypervisor
provides. So on real hardware the table was empty, `GetDefault()` returned
null, and the first `wget https://` on the Hetzner box died at the first thing
a TLS handshake does:

```
tls: connection: FailedToGetRandomBytes
TlsConn: handshake with github.com failed
```

Reading a source directly does not work even where there is one. Supply is
nothing like uniform: virtio-rng is a device round trip, RDSEED runs dry when
several cores draw at once, timing jitter yields a bit at a time. A handshake
wants hundreds of bytes at once and cannot be told to come back later. That is
what a generator is for — sources contribute, the generator answers.

## The sources

| source | where | what it is |
|---|---|---|
| `rdseed` / `rdrand` | x86-64, `arch/x86_64/hal_random.cpp` | the CPU's own DRBG. RDSEED taps the conditioned output of the physical noise source and is what another generator wants to be keyed from; RDRAND is the AES-CTR DRBG downstream of it — faster, never dry, one more deterministic step from the noise |
| `rndr` | arm64, `arch/arm64/hal_random.cpp` | FEAT_RNG, the same idea (RNDRRS reseeds before answering, RNDR does not). Optional from Armv8.5 and **not implemented by Apple's M-series**, so no arm64 machine nos runs on today has it |
| `rng0`… | `drivers/virtio_rng.cpp` | virtio-rng, i.e. the host's entropy. Present under QEMU, absent on bare metal — exactly the wrong way round from the CPU instruction, which is why both exist |
| `jitter` | `kernel/random.cpp` | timing jitter, the fallback (below) |

`hal/random.h` is the seam for the CPU instruction, so common code never asks
which architecture it is on. `Hal::ProbeHwRandom()` runs once on the BSP and is
what decides whether the instruction exists: reading RNDR on a core without
FEAT_RNG is an undefined-instruction trap, and CPUID has to be checked before
RDRAND for the same reason. It also honours `hwrng=off`.

The probe answering "yes" is not enough. `HwRandomSource::SelfTest()` draws
eight values and refuses the instruction unless at least half of them arrive
and at least two differ — which is what a broken RDRAND looks like (some AMD
parts answer `0xFFFF'FFFF'FFFF'FFFF` forever after a resume) and what a
hypervisor that stubs the instruction out looks like too. In both cases the
capability bit still says the instruction is there.

## Timing jitter, and what it is worth

On a machine with no random instruction and no virtio-rng, the only thing left
that differs between two boots is how long things take. `JitterSource` times a
dependent read-modify-write walk over a 4 KiB buffer — the trip count and the
starting offset depend on the state the last measurement left, so the walk
cannot be prefetched or unrolled into something of fixed cost — and keeps the
low bit of the duration. Pairs of those bits go through von Neumann extraction:
a pair that agrees is discarded, a pair that differs contributes one bit, which
removes whatever fixed bias the low bit has at the cost of most of the samples.

What varies between two measurements is cache and store-buffer state, the
branch predictor, the CPU's own frequency, an SMT sibling, and any interrupt
that lands in the middle of one. That is real but it is not a hardware entropy
source, the bits are not independent, and no attempt is made here to put a
number on it. It is mixed in as material of unknown worth, which the absorb
below makes safe.

It can also fail outright, and says so rather than handing back biased bytes:
if 256 pairs go by without a single measurement differing from the one before
it, the counter is too coarse to see this much work, or is not running.

## The generator

Both halves are Linux's construction, in miniature. ChaCha20's block function
(`lib/chacha20.cpp`, RFC 8439, checked at boot against the RFC's own test
vector in `TestChaCha20`) is the only primitive.

```
        sources                   pool                     callers
  rdseed ------\                                    /--- kernel_get_random -> rustls
  rng0 --------->  AddEntropy  ->  Key[32]  ->  GetBytes ---- random [len]
  jitter ------/     absorb          |          fast key
  boot marks --/                     \---- erasure ---/
```

**Output — fast key erasure.** A request generates one ChaCha20 block over the
32-byte key, keeps the first half of the block as the next key and hands the
second half out. The state that produced a value is gone by the time the caller
has it, so no output leads back to an earlier one. Where the CPU has a random
instruction, one draw from it is also XORed into the key on every request: a
few hundred cycles, no lock and no device, and it means the stream keeps taking
in fresh entropy with no timer to drive it.

**Seeding — absorb.** Seed material is XORed into the key 32 bytes at a time
and the block function run over the result. XOR cannot take entropy out of the
key, and ChaCha20's feed-forward addition is what stops the new key leading
back to the old one. Together those two properties are what let material of
unknown quality — jitter, boot-time constants — be mixed in without an entropy
estimate to argue about: **a source that turns out to be worthless cannot make
the pool worse.**

## When it happens

| when | what |
|---|---|
| `Main2` / `MainArm64`, before the self-tests | `Random::Setup()` — probe the CPU instruction, register the sources that need no device, seed from them. No heap, no device |
| `BpStartup` / `BpStartupArm`, after the virtio probe | `Random::Reseed()` — fold in every registered source, virtio-rng now included |
| `entropy reseed` | the same, by hand |

Setup has to precede the self-tests because they ask the pool for bytes. On
arm64 nothing better than jitter exists at that point — no FEAT_RNG on any
core nos runs on, and the virtio-mmio slots have not been probed yet — so the
pool is seeded from jitter there and the reseed after device bring-up is what
puts hardware entropy into it. `Setup()` returning false, which takes a machine
with no random instruction *and* a cycle counter too coarse for the jitter
collector, is not fatal: boot says so plainly and HTTPS is what stops working.

There is no periodic reseed. A CSPRNG needs one good seed, and the rest of what
Linux reseeds against — recovery from a state compromise across a
privilege boundary — is not a threat this kernel has a boundary for. The
per-request draw from the CPU instruction covers freshness where such an
instruction exists.

## Using it

```
random [len]        len random bytes as hex, 1..1024, default 16
entropy             pool state: seeded, whether hardware entropy reached it,
                    reseed count, bytes generated, and the registered sources
entropy reseed      draw from every source again, then print the same
```

`hwrng=off` on the kernel command line makes the CPU instruction invisible to
the probe. That is how the fallback path gets exercised on a machine that does
have RDRAND — `entropy` then reports `hardware entropy no` unless a virtio-rng
is attached.

Reading `entropy` is the first thing to do when a handshake fails on a new
machine. `hardware entropy no` with `pool: chacha20 seeded` means everything
works but is resting on timing jitter alone; `pool: chacha20 UNSEEDED` means
nothing could seed it and HTTPS will refuse to start rather than key itself
from a zero pool (`kernel_get_random` returns 0, which rustls reports as
`FailedToGetRandomBytes`).
