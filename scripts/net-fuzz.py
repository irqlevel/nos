#!/usr/bin/env python3
"""net fuzz: the network layer, fuzzed on the host.

Everything the network hands the kernel is somebody else's to choose --
every frame on the wire, every answer a server gives the HTTP client, the
DHCP client and the resolver, every byte an SSH client sends the server --
so a panic, an overflow, a lock broken or a loop that never ends anywhere on
those paths is one somebody else can cause. fuzz/net is a host
program built from the kernel's own crates -- `net` (Ethernet, ARP, IPv4,
ICMP, UDP, TCP, DHCP, DNS, the HTTP client, the UDP shell, netconsole),
`tls`, `fs`, `ssh` and the sshd module's source, over `kcore` and `ffi` as
they are -- linked with the rest of a kernel written for the purpose: its
C++ half (the locks, tasks, soft IRQs, timers, the clock, the entropy pool,
the command table), a NIC whose wire is the fuzzer's, and the network around
the machine, whose hosts speak each protocol well and badly: a gateway, a
LAN of hosts that answer ARP or do not, a TCP peer that keeps every byte of
both streams, a DHCP server and a rogue one, a DNS server, an HTTP server
that speaks TLS on its TLS ports with the certificate the input picks (the
fuzzer's own CA, certs/make.sh), a collector of the kernel log, clients of
the UDP shell, and SSH clients. Overflow checks are on, as a RUSTUB=1
kernel has them. Each input runs in a process of its own, forked from one
booted machine.

The targets: stack (frames of every kind), tcp, http, https, dns, dhcp,
icmp (ping and ARP), udpshell, netconsole and ssh. Each holds what it drives
to what the protocol, and this stack of itself, says: a frame no stack
should send, a byte of a stream delivered that was never sent, a lease kept
past its end, an ARP cache holding what nobody said, a log line twice, a
login for a key the server does not know -- and, everywhere, a slot, a frame
or a task kept for good, a lock taken in both orders, a spin lock held
across a sleep, memory allocated with interrupts off. Those, a panic, a
spin, a crash and a hang are each reported with the seed and iteration that
make them again, and the input in a file to replay.

By default every target runs its own number of inputs from a fixed seed, the
same every time: a gate, a couple of minutes. A campaign is `--seconds` a
target and seeds of its own:

    python3 scripts/net-fuzz.py                          # the gate
    python3 scripts/net-fuzz.py --seed 7 --seconds 300   # five minutes a target
    python3 scripts/net-fuzz.py --target tcp --target https
    python3 scripts/net-fuzz.py --replay tcp out/net-fuzz/findings/X.hex [--trace]

NET_FUZZ_STATS=1 adds each target's slowest input. Needs cargo on the host;
the program's dependencies are the kernel's vendored ones, so it builds
offline. Exit code 0 = no finding; 1 = a finding; 3 = a hang.
"""

import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, 'out', 'net-fuzz')
VENDOR = os.path.join(ROOT, 'src', 'rust', 'vendor')

# The gate's seed. How many inputs of each target it runs is the target's
# own (targets/mod.rs): an input of ssh is a hundred of icmp.
GATE_SEED = '1'

# The RustCrypto crates on the software backends the kernel builds them with
# (src/rust/.cargo/config.toml): the code fuzzed is the code the kernel runs
# -- and on x86-64, curve25519-dalek's SIMD backend would want proc macros
# whose vendored build scripts are empty.
RUSTFLAGS = ['--cfg', 'aes_force_soft', '--cfg', 'polyval_force_soft', '--cfg', 'poly1305_force_soft',
             '--cfg', 'chacha20_force_soft', '--cfg', 'curve25519_dalek_backend="serial"']


def main():
    args = sys.argv[1:]
    # The kernel's vendored crates, as the kernel's own build takes them: the
    # fuzzer's manifest is outside src/rust, whose .cargo/config.toml says so
    # for the kernel -- and makes any build under it a kernel build -- so it
    # is said here, and cargo is run from the root.
    build = subprocess.run(['cargo', 'build', '--release', '--offline', '--quiet',
                            '--manifest-path', os.path.join(ROOT, 'fuzz', 'net', 'Cargo.toml'),
                            '--target-dir', OUT,
                            '--config', 'source.crates-io.replace-with="vendored-sources"',
                            '--config', 'source.vendored-sources.directory="%s"' % VENDOR,
                            '--config', 'build.rustflags=[%s]' % ','.join('"%s"' % f.replace('"', '\\"')
                                                                         for f in RUSTFLAGS)],
                           cwd=ROOT)
    if build.returncode != 0:
        print('net-fuzz: the build failed')
        return build.returncode
    findings = os.path.join(OUT, 'findings')
    os.makedirs(findings, exist_ok=True)
    if '--replay' in args:
        # The input's file as given, from where the caller is.
        i = args.index('--replay')
        if i + 2 < len(args):
            args[i + 2] = os.path.abspath(args[i + 2])
    elif '--seed' not in args:
        args = ['--seed', GATE_SEED] + args
    run = subprocess.run([os.path.join(OUT, 'release', 'net-fuzz')] + args, cwd=findings)
    if run.returncode not in (0, 2):
        print('net-fuzz: the inputs are in %s' % findings)
    return run.returncode


if __name__ == '__main__':
    sys.exit(main())
