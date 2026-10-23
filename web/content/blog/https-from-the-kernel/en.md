---
title: "HTTPS from inside the kernel"
description: "What it took for wget https:// to work in nos: rustls without std, a transport trait, a root store and the RTC, an entropy pool, and no SSE."
date: 2026-09-09
tags: [networking, rust, safety, x86-64, deployment]
---

On 9 September 2026, `wget https://example.com` worked from the nos shell for the first time. The kernel resolved the name, opened a TCP connection, made a TLS 1.3 handshake and verified the server's certificate chain. Today this is how a running nos fetches its own releases from GitHub. A server whose only console is the network downloads its next kernel, checks it and boots it once to try it.

We did not write the protocol, and that was the easy part. The work was in what rustls and its cryptography expect from an operating system: the standard library, a clock, random bytes and SSE registers. Here is each gap, how it showed up, and what HTTPS costs.

## Why not plain HTTP

nos already had an HTTP client and a `wget` command. But half the http:// links on the internet redirect to https://, as the commit that added TLS put it, and the client refused every such redirect with "redirect to non-HTTP" ([ca8ff9c](https://github.com/irqlevel/nos/commit/ca8ff9c)).

Writing TLS ourselves was never the plan. It is a certificate parser, a chain builder, a signature verifier, a key exchange, authenticated encryption and a state machine. That is weeks of work, and a mistake there is a silent hole rather than a crash. rustls, a TLS library in Rust, never touches a socket: it takes bytes and hands bytes back, so it could sit on the kernel's own TCP stack.

## rustls without std

The kernel's Rust is `no_std`. It links `core` and `alloc` but not the standard library, so there are no files, sockets, clocks or OS randomness to call. rustls works in this mode, but its usual `read_tls` and `write_tls` take `std::io` traits. So the [`tls` crate](https://github.com/irqlevel/nos/blob/master/src/rust/tls/src/lib.rs) drives rustls's unbuffered API instead. `process_tls_records` returns one state at a time, such as bytes to send or a handshake waiting for the peer. Our loop does the reading, writing and buffering that `std::io` would have done.

One rule matters more than the rest: call `process_tls_records` once per turn. It consumes records as it goes. Calling it again just to ask "are we blocked?" processes the next record twice, and the connection dies a round trip later with `InappropriateMessage { got_type: ChangeCipherSpec }` ([HTTPS](/docs/tls#the-unbuffered-api)). The primitives come from rustls-rustcrypto, a provider built on the RustCrypto crates, which implement them in plain Rust.

## A seam between TLS and TCP

From the first day, the HTTP client has not known whether its bytes are encrypted. Request framing, header parsing, chunked decoding, redirects and the body sink sit above a transport that is plain TCP or TLS, and adding TLS changed none of them.

The seam took two more steps. At first the HTTP client was C++, and the Rust TLS session reached its C++ TCP connection through two C calls, `kernel_tcp_send` and `kernel_tcp_recv`. On 18 September the HTTP client followed TCP into Rust ([30e70e7](https://github.com/irqlevel/nos/commit/30e70e7)), but TLS still reached TCP the way a loadable module does, across the C ABI. Between two pieces of Rust in one image, that was a C ABI standing in for a trait.

The network layer depends on `tls`, so `tls` cannot name the layer's connections. It takes a trait instead ([d6dac83](https://github.com/irqlevel/nos/commit/d6dac83)):

```rust
pub trait Transport {
    /// Sends the whole buffer; false if the connection failed part way.
    fn send_all(&mut self, buf: &[u8]) -> bool;

    /// Bytes read, 0 at the end of the stream, negative on an error or when
    /// `timeout_ms` went by with nothing received.
    fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> isize;
}
```

[`http.rs`](https://github.com/irqlevel/nos/blob/master/src/rust/net/src/http.rs) hands `TlsStream` a `TcpPipe`, a wrapper around its connection that calls TCP directly. Dropping the TLS stream only sends close_notify, the message that ends a TLS session cleanly, and the HTTP client closes the connection itself.

## Certificates and the clock

The trust anchors come from webpki-roots, a compiled-in copy of Mozilla's root store. A root store is the set of certificate authorities that a chain must end at to be trusted, and our copy has 121 of them. To check expiry, rustls needs the time, which it cannot get without std. So the kernel hands it a clock: the real-time clock (RTC) read at boot, plus the time since.

```rust
impl TimeProvider for KernelTime {
    fn current_time(&self) -> Option<UnixTime> {
        let secs = kcore::time::wall_clock_secs();
        if secs == 0 {
            return None;
        }
        Some(UnixTime::since_unix_epoch(Duration::from_secs(secs)))
    }
}
```

A machine with a wrong clock rejects every certificate, which is the safe way to fail. There is no `--insecure` switch. badssl.com serves broken certificates on purpose, and its expired, self-signed, untrusted-root and wrong-host ones were all refused, each with its reason in the kernel log ([HTTPS](/docs/tls#certificates)):

```text
tls: InvalidCertificate(ExpiredContext { time: UnixTime(1788984857),
                                         not_after: UnixTime(1428883199) })
```

The first number is 9 September 2026, the evening it first worked. The second is 12 April 2015.

The wrong-host test also crashed the machine, and the bug behind it was the stack size, not TLS. That certificate overflowed the shell task's 32 KB stack, and the kernel stopped with a BUG in `GetCurrentTask`, which checks the stack each time it finds the running task. Verifying a chain walks candidate issuers recursively. The shell task used to peak at about 4 KB, and during a handshake it reaches about 29 KB, so task stacks went to 64 KB.

## Random bytes

The RustCrypto crates get their random bytes through getrandom, a crate that asks the operating system for them. It has no backend for bare metal but accepts a custom one, so the `tls` crate registers the kernel's:

```rust
fn kernel_getrandom(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    if kcore::random::fill_random(buf) {
        Ok(())
    } else {
        Err(getrandom::Error::UNSUPPORTED)
    }
}

getrandom::register_custom_getrandom!(kernel_getrandom);
```

That worked under QEMU, which gives the kernel virtio-rng, a virtual device that passes the host's entropy to the guest. On a Hetzner server, the first `wget https://` died at the very start of the handshake:

```text
tls: connection: FailedToGetRandomBytes
TlsConn: handshake with github.com failed
```

The kernel had no random generator, only a table of entropy sources. The only source ever written was virtio-rng, which a hypervisor provides, so on bare metal the table was empty.

The same evening the kernel got a ChaCha20 pool built on the same two ideas as Linux's ([72432cf](https://github.com/irqlevel/nos/commit/72432cf)). Seed material is XORed into the key, so a worthless source cannot make the pool worse. Each answer replaces the key that produced it, so no output leads back to an earlier one. The pool is seeded from RDSEED and RDRAND on x86-64, RNDR on arm64, virtio-rng and, as a last resort, timing jitter. An unseeded pool refuses, because a handshake keyed from zeros is worse than none.

With the pool, `wget https://github.com/robots.txt` returned 200. With `hwrng=off` and no virtio-rng, the pool was seeded from jitter alone, and an HTTPS fetch still completed. The pool moved to Rust in October ([cf87887](https://github.com/irqlevel/nos/commit/cf87887)), and [Randomness](/docs/random) describes it.

## No SSE, no x87

The kernel keeps SSE and the x87 FPU off, because its context switch saves neither's registers. Two tasks using them would share one set and silently corrupt each other's values. So the kernel's Rust is built for soft-float targets, `x86_64-unknown-none` and `aarch64-unknown-none-softfloat`. On a soft-float target, the compiler turns floating-point math into calls to helpers such as `__muldf3`, which work in integer registers. The helpers come from `compiler_builtins`, Rust's library of compiler intrinsics, and since 22 September the C++ calls them too, built with `-mno-80387` as well as `-mno-sse` ([bec20c2](https://github.com/irqlevel/nos/commit/bec20c2)).

The RustCrypto crates choose SSE2 or AVX2 at run time, and they compile those paths unconditionally. LLVM cannot compile them for a target with SSE off, so the build stopped with `Do not know how to split the result of this operator`. Each crate has a documented switch to a portable, constant-time software backend, and [`.cargo/config.toml`](https://github.com/irqlevel/nos/blob/master/src/rust/.cargo/config.toml) sets the switches for both targets:

```toml
[target.x86_64-unknown-none]
rustflags = ["-Ccode-model=large", "-Cforce-frame-pointers=yes", "--cfg", "aes_force_soft", "--cfg", "polyval_force_soft", "--cfg", "poly1305_force_soft", "--cfg", "chacha20_force_soft", "--cfg", "curve25519_dalek_backend=\"serial\""]
```

A build error is the friendly form of this trap. The next day, while adding a `sha256` shell command, we found the quiet form ([6d3ffd5](https://github.com/irqlevel/nos/commit/6d3ffd5)). sha2 has no such cfg. It picks SHA-NI, the CPU's SHA instructions, by CPUID at run time, and it builds without complaint. But SHA-NI instructions are SSE instructions, and the CPU allows them only when CR4.OSFXSR is set.

An application processor (AP) is any CPU other than the one the machine booted on. It starts from INIT, which clears CR4, and the boot code set only PAE and MCE on it: the AX41 logged `cr4=0x60` on every AP. So the first block hashed on an AP would have been an invalid opcode, in the TLS client too. Now [`kernel/Cargo.toml`](https://github.com/irqlevel/nos/blob/master/src/rust/kernel/Cargo.toml) asks for sha2's `force-soft` feature, which covers TLS as well, because a crate is built once for the whole graph.

## Three details in the HTTP client

The body never exists in one piece. Until half an hour before TLS landed, the client read each response into one 32 KB heap buffer. No bigger buffer would have helped, since the page allocator could not serve a single allocation over 512 KB ([ec93382](https://github.com/irqlevel/nos/commit/ec93382)).

Now the body goes to a sink as it arrives. The client keeps one 16 KiB buffer for the headers and then for the body, one receive at a time, and `wget` writes the file in 64 KiB blocks. A 20 MB download, the cap, costs no more memory than a 200-byte one.

Redirects follow both schemes, five at most, so an http:// link that answers with a 301 to https:// downloads intact. A redirect's body is never read.

URLs are never cut short. The day after HTTPS worked, `wget` of a GitHub release asset returned a CDN error page instead of the file ([5de70df](https://github.com/irqlevel/nos/commit/5de70df)):

```text
$ wget https://github.com/irqlevel/nos/releases/download/v0.0.18/nos.iso
HTTP 618, 457 bytes
... <title>618 jwt:jwt-not-provided</title> ... Varnish cache server
```

github.com answers a release URL with a 302 to a signed link on release-assets.githubusercontent.com. The link is 891 characters long, and its query carries a signature and a JWT (a signed token). The client clipped it in three places, to 255, 383 and 255 characters. When the file was in the CDN's edge cache, the clipped link was served anyway, so the command worked or failed depending on who had fetched the file last. Now a URL may be up to 2,047 characters, and a longer one fails the request: a truncated URL is a different URL.

With all of this in place, a running kernel can update itself. On the Hetzner servers set up by hand, a new kernel goes in from the [UDP shell](/docs/udp-shell) ([Real hardware](/docs/real-hardware#updating-the-kernel-from-inside-nos)):

```sh
wget https://github.com/irqlevel/nos/releases/latest/download/kernel-x86_64.elf /nos-kernel64.elf.next
sha256 /nos-kernel64.elf.next        # against the release's SHA256SUMS
grubenv /grubenv nos_next=nos-next   # one boot of the candidate
reboot
```

GRUB boots the candidate once, and only the new kernel, once it is up, promotes itself. So a kernel that never reached a shell cannot replace one that did.

## What it costs

- About 4 MB of kernel image, most of it the root store and the crypto.
- Task stacks of 64 KB instead of 32 KB. After the URL change, the shell's command task peaked at 31,640 bytes of its 65,536.
- Two 18 KiB record buffers per TLS session, one for each direction.
- Software ChaCha20-Poly1305 runs at several MB/s under QEMU, and TCP's 8 KB receive window limits a download long before the cipher does.
- A heavier build. These were the tree's first external dependencies, so they are vendored and cargo runs with `--offline` ([4927a77](https://github.com/irqlevel/nos/commit/4927a77)). `scripts/vendor.sh` empties the packages no target compiles, which leaves 31 MB instead of 143.
- A nightly pinned to a date. A newer one broke the offline build over `hermit-abi`, a crate nos never compiles ([6a05f66](https://github.com/irqlevel/nos/commit/6a05f66)).

## Three weeks later

The [network fuzzer](/blog/fuzzing-the-kernel-on-the-host) ran the client against a rustls server with a certificate authority of its own. It caught the TLS client taking a stream cut inside a record, or a receive that timed out, for a finished one. A stalled body with no length came back looking whole. Both are errors now ([c68452d](https://github.com/irqlevel/nos/commit/c68452d)), and certificate verification still cannot be turned off.
