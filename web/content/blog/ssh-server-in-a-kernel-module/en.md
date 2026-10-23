---
title: "An SSH server in a kernel module"
description: "Two of nos's servers have no console but the network. So the kernel got an SSH server: a loadable Rust module, tested against OpenSSH's own client."
date: 2026-09-12
tags: [networking, rust, modules, safety, testing]
---

## No console but the network

Two of the three real machines nos boots on are Hetzner dedicated servers, an EX44 and an AX41. Neither has a serial port or IPMI, so the network is their only console ([Real hardware](/docs/real-hardware)). For a while we typed at them through the [UDP shell](/docs/udp-shell), which runs any command that arrives in a datagram with its magic number. With no encryption and no authentication, it is fine on a VM's private network and an open door on the internet.

On 12 September 2026 nos got an SSH server ([57544cd](https://github.com/irqlevel/nos/commit/57544cd)). An OpenSSH client logs in with an Ed25519 key and gets the same shell as the console. The connection is encrypted, and only keys the server was told to allow get in. nos has no user space yet, so the server runs inside the kernel, next to the shell it serves. A machine can run without it, so it is a loadable Rust module, `sshd.ko`.

## What it speaks

The server speaks one algorithm of each kind, the ones every OpenSSH of the last ten years offers first or nearly. The whole list is a few constants in [transport.rs](https://github.com/irqlevel/nos/blob/master/src/rust/ssh/src/transport.rs):

```rust
/* What the server offers: one of each kind */
const KEX: &[&str] = &["curve25519-sha256", "curve25519-sha256@libssh.org"];
/* OpenSSH's strict key exchange, the answer to Terrapin: offered and asked
   for in the first KEXINIT only (OpenSSH's PROTOCOL, 1.9) */
const KEX_STRICT_SERVER: &str = "kex-strict-s-v00@openssh.com";
const KEX_STRICT_CLIENT: &str = "kex-strict-c-v00@openssh.com";
const HOST_KEYS: &[&str] = &[ED25519];
const CIPHERS: &[&str] = &["chacha20-poly1305@openssh.com"];
/* Named because a KEXINIT has to name a MAC. With an AEAD for the cipher
   nobody uses it, and OpenSSH does not even negotiate it. */
const MACS: &[&str] = &["hmac-sha2-256"];
const COMPRESSION: &[&str] = &["none"];
```

The key exchange is X25519 with SHA-256 (RFC 8731), and host and user keys are Ed25519 (RFC 8709). The cipher is OpenSSH's ChaCha20-Poly1305 construction. It encrypts even the packet length, under a separate key. Because this is authenticated encryption, no separate MAC is needed.

Strict key exchange is OpenSSH's answer to Terrapin, an attack that deletes messages from the start of the encrypted stream. To keep the sequence numbers in line, the attacker slips an extra message into the handshake while it is still unencrypted. In strict mode nothing but the key exchange's own messages may arrive during the first exchange, not even an IGNORE. Both sides also reset their sequence numbers to zero at every NEWKEYS, the message that switches to the new keys.

Rekeying is a new key exchange in the middle of a session. The server never starts one, but it follows the client's wherever it comes, even mid-output. Flow control is the SSH channel window: a side sends only as many bytes as the other has said it will take, and the reader tops the window up as it reads. The server gives a client a 64 KiB window and waits for the client's window in turn.

Everything else is refused: passwords, keys other than Ed25519, compression, `sftp` and so modern `scp`, port forwarding, agent and X11 forwarding, and `env`. A connection gets one session, either a shell or one command (`ssh host dmesg`).

## A protocol with no kernel in it

The server is two crates. `src/rust/ssh` is the protocol: packets, key exchange, public key authentication, the session channel and a line editor with 32 lines of history. It knows nothing of the kernel and gets everything it needs from the machine through two traits ([lib.rs](https://github.com/irqlevel/nos/blob/master/src/rust/ssh/src/lib.rs), doc comments left out):

```rust
pub trait Link {
    fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> Recv;
    fn send(&mut self, data: &[u8]) -> bool;
    fn now_ms(&self) -> u64;
    fn stopping(&self) -> bool;
    fn random(&mut self, buf: &mut [u8]) -> bool;
}

pub trait Shell {
    fn authorized(&mut self, user: &str, key: &PublicKey) -> bool;
    fn logged_in(&mut self, _user: &str, _key: &PublicKey) {}
    fn run(&mut self, line: &str, io: &mut dyn Io);
}
```

`src/rust/modules/sshd` implements them with a listener task, a task for each connection, the `sshd` command and the server's two files. The protocol crate has no `unsafe` at all. The module has four `unsafe` blocks, and each turns a task's context pointer back into its `Box`.

The cryptography comes from the RustCrypto and dalek crates the TLS client had already vendored, so the server added no dependency. One build detail needed care. A module is a separate cargo build that the kernel's features do not reach, so the `ssh` crate itself asks `sha2` for its software backend. Otherwise `sha2` picks the CPU's SHA instructions at run time, and the kernel keeps SIMD off. Free of the kernel, the crate also runs on the host, where the network fuzzer drives it with a full SSH client ([Fuzzing the kernel on the host](/blog/fuzzing-the-kernel-on-the-host)).

## Keys in OpenSSH's formats

The host key lives in `/etc/ssh/ssh_host_ed25519_key`, in OpenSSH's own private key format. A key made by `ssh-keygen -t ed25519 -N ''` can go there, so a server can share its host key with the other OS it boots. The module makes a key on its first start, and only where there is no file. If the file is there but cannot be read, `sshd start` fails, because with a new key every client that knew the old one would refuse to connect.

`/etc/ssh/authorized_keys` takes `ssh-ed25519` lines as OpenSSH writes them. A line with options such as `from=` or `command=` is skipped with a warning, since the server would not honour them. `sshd allow` and `sshd deny` edit the file from the shell.

A review of the first version found three ways to empty that file: a full disk, two edits at once, and a `deny` that met a byte that is not UTF-8. When SSH is the only way in, that is a lockout. So a file is never written in place now. The new content goes to `<path>.new`, is synced, and only then replaces the old file, with one lock held from the read to the write.

## Started at boot by /etc/rc

Nothing runs `insmod` at boot by itself, so the same commit added `/etc/rc`: shell commands that the shell's task runs once, after DHCP has brought the network up.

```text
$ rc add insmod /sshd.ko
$ rc add sshd start
$ rc
1  insmod /sshd.ko
2  sshd start
```

Their output goes to the screen as it comes and to the kernel log once each command returns. Logging it as it is printed would make a `dmesg` in the script read its own output back. When a line keeps the machine from coming up, `rc=off` on the kernel command line skips the file ([sshd](/docs/sshd#starting-it-at-boot)).

In October this became the default. The system image for servers carries `sshd.ko` and an `/etc/rc` that loads and starts it, `nos-install --ssh-key` allows a key, and no shipped configuration starts the UDP shell any more ([2777692](https://github.com/irqlevel/nos/commit/2777692)). Logging in is covered in [Connect to a running nos](/howto/connect).

## The first user of TCP's listening side

Before sshd, nothing in nos had accepted a TCP connection. The stack's passive side changed as the server was built:

- `Accept` takes a timeout and returns once its listener is closed. The listener task accepts in 250 ms slices and checks in between whether the server is stopping.
- It also hands out a connection whose peer closed first. Such a connection used to sit in CLOSE-WAIT with nobody to close it and hold its slot for good.
- A listener holds at most 16 connections nobody has accepted, and closing it resets them.
- `Abort` ends a connection with a RST. The side that closes first keeps its slot through a minute of TIME-WAIT, and the kernel has only 64 slots.

The server needs that last one, because a port 22 on the internet is knocked on all day. It serves 8 connections at once, at most 4 of them still logging in, and resets the rest ([lib.rs](https://github.com/irqlevel/nos/blob/master/src/rust/modules/sshd/src/lib.rs)):

```rust
    if total >= MAX_SESSIONS || logging_in >= MAX_LOGGING_IN {
        shared.stats.refused.fetch_add(1, Ordering::Relaxed);
        kcore::trace!(1, "sshd: {} refused: {} sessions, {} of them logging in", peer, total, logging_in);
        /* Reset, not closed: closing first on everyone turned away would
           keep a TCP slot a minute each, and run the kernel out of them */
        stream.abort();
        return;
    }
```

The limit has a protocol side. A client gets 30 seconds from connecting to logging in, and in the first version rekeying stretched that wait, so four clients that kept rekeying could hold every login slot. Now the login grace is one deadline that every read checks. In October the hypervisor's review found that guests could reach those slots too, so a listener now belongs to one device ([e9e78d3](https://github.com/irqlevel/nos/commit/e9e78d3)).

One TCP bug hit slow readers. `Send` used to push a byte of data into a shut window to make the peer advertise it again. The duplicate ACKs that came back never acknowledged that byte, so the retransmit limit took the peer for dead. A suspended `ssh` client, which is only slow to read, was aborted within a minute. Now only the persist timer probes a shut window.

## The gate

`scripts/sshd-test.py` tests the server end to end against OpenSSH's own client ([sshd](/docs/sshd#testing)). On arm64 it boots nos in QEMU, loads the module over the UDP shell and works it: commands, shells with and without a terminal, 3 MiB of output through the client's 2 MiB window, a key it must refuse, four sessions at once, `sshd stop` with a session open, and `rmmod sshd` from inside one. Then it puts the module in `/etc/rc`, reboots and checks that the server came back by itself with the same host key. On x86-64 it boots a root image that already starts the server, and just logs in.

One check sends the 3 MiB again with `RekeyLimit=16K`. A server busy streaming sees a rekey request only when it next reads, which here is when the window runs out. So the test asks for rekeys mid-output, not for one every 16 KiB. Another opens 70 connections and closes them at once, then six that just sit there. Four of the six must get a session, two must be reset, and no TCP slot may be left in a closing state.

That second check once failed for the wrong reason. It took "nobody logging in" to mean the burst had drained, but that is also true before the first probe arrives. Stragglers then landed in the count: "refused 12, logging in 0". Now it also waits for the connection count to stand still for three seconds ([44579ed](https://github.com/irqlevel/nos/commit/44579ed)).

The gate paid for itself within a week. When TCP moved to Rust ([380308a](https://github.com/irqlevel/nos/commit/380308a); the port has [its own post](/blog/cpp-to-rust-layers)), it caught the one regression the port brought. Allocating a connection took every slot's lock, the listener's among them, and one path allocated while it held the listener's lock. The CPU spun on a lock it already held. Today 41 checks pass, and one more fails for reasons that have nothing to do with TCP.

## A command beside its session

The surprise from OpenSSH came through its keepalives. Until 24 September a command ran on its session's own task, and nothing read the connection until the command returned. On the AX41 a 30-second `hv wait` over ssh with `ServerAliveInterval=5` was cut off at 14 seconds with "Timeout, server not responding".

Now a command runs on a task of its own, with 16 KiB of its output and 4 KiB of typing buffered between it and the session ([50746e4](https://github.com/irqlevel/nos/commit/50746e4)). Output wakes the session at once. After every quiet 10 ms the session tends the connection for a millisecond: it answers a keepalive, takes a window update and keeps any typing for later. A first version waited on the socket instead, and 3 MiB of `cat` took 1.6 s rather than 0.7 s. Waiting on the command brought it back to 0.7 s.

The test got a check for this: `top 5000`, five seconds of silence, under `ServerAliveInterval=1` and `ServerAliveCountMax=2`. A command can also read what the client types while it runs, which is how `hv attach` shows a guest's console live over `ssh -t` ([3aab12c](https://github.com/irqlevel/nos/commit/3aab12c)).

## What it guards

The server runs in the kernel with no privilege separation, so a bug in it is a kernel bug, and whoever has its shell has the kernel. That is why the parser is safe Rust with every read bounds-checked, signatures are verified strictly, and packet tags are compared in constant time ([sshd](/docs/sshd#security)).
