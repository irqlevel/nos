---
title: "Connect to a running nos"
description: "Reach the kernel's shell over the serial console, the UDP shell or SSH, and stream the kernel log to your machine with netconsole."
order: 3
updated: 2026-10-22
---

There are four ways to reach a running nos. Three of them go over the network, and on a machine with no serial port they are all you have. In this guide you try each one on nos running in QEMU: you type at the console, run commands over the UDP shell, log in with SSH and receive the kernel log with netconsole.

| Channel | What it carries | How to turn it on | Who can use it |
|---|---|---|---|
| Serial console | the shell and the boot log | always on; QEMU decides where the port goes | whoever has the console |
| UDP shell | shell commands, one reply each | `udpshell=PORT` on the kernel command line | **anyone** who can send the machine a datagram |
| SSH | the shell, with or without a terminal, or a single command | the `sshd` module | holders of the Ed25519 keys you allow |
| Netconsole | the kernel log, one way | `netconsole=ip:port` on the kernel command line | anyone who receives the datagrams; nothing is encrypted |

You need:

- nos [built](/howto/build) and [booting in QEMU](/howto/run-in-qemu);
- Python 3 for the two clients, `scripts/udpsh.py` and `scripts/netconsole.py`;
- for SSH, an OpenSSH client and an Ed25519 key (`ssh-keygen -t ed25519` creates one). nos's sshd accepts no other key type.

## The serial console

Where the console is depends on the script:

| Script | Where you type | Where the serial port goes |
|---|---|---|
| `qemu.sh` | the QEMU window | `nos.log` |
| `qemu-disk.sh` | your terminal (`-nographic`) | your terminal |
| `run.sh` | a VNC viewer on display `:0` | `nos.log` |
| `qemu-arm64.sh` | nowhere; use the UDP shell or SSH | `nos-arm64.log` |

The serial port carries the boot log from its very first line. Once the shell starts, trace output stops going to the console, and only the shell's own output appears there. `dmesg` still has the rest. `console=serial` or `console=vga` keeps the shell on just one of the two consoles. The disk image boots with `console=serial`.

## The UDP shell

> The UDP shell runs any command for anyone who can send the machine a datagram, with no encryption and no authentication. That's why it's off by default in every configuration. Use it on a VM's private network, never on a port the internet can reach.

You turn it on with the `udpshell=PORT` kernel parameter. The arm64 script has a variable for it, and it forwards host UDP port 9000 to the guest:

```sh
NOS_UDPSHELL=9000 ./scripts/qemu-arm64.sh
```

Once the log shows `boot: complete`, connect from another terminal:

```sh
python3 scripts/udpsh.py 127.0.0.1
```

```text
nos udp shell -> 127.0.0.1:9000
$ uptime
```

On x86-64, only the disk boot's script, `qemu-disk.sh`, forwards UDP port 9000. So add the parameter to the disk's GRUB entry in `build/grub-disk.cfg`:

```diff
-	multiboot2 /boot/kernel64.elf console=serial dhcp=auto dns=on root=auto
+	multiboot2 /boot/kernel64.elf console=serial dhcp=auto dns=on root=auto udpshell=9000
```

Then rebuild the disk, boot it and connect:

```sh
./scripts/build-disk.sh
./scripts/qemu-disk.sh
python3 scripts/udpsh.py 127.0.0.1
```

The edits on this page change files tracked by git (`build/grub-disk.cfg`, `build/grub.cfg`, `scripts/qemu-arm64.sh`), so revert them when you're done.

Run the client as `udpsh.py <host> [port] [timeout]`. The port defaults to 9000 and the timeout to 30 seconds. Press Ctrl-D to quit.

- A reply holds at most 32 KB. Anything beyond that is cut off and marked `[output truncated]`. The kernel log holds 2048 messages, so ask for just the end of it: `dmesg 40`.
- A command that runs longer than the timeout, such as a `blkload` run or a long `ping`, needs a longer timeout: `python3 scripts/udpsh.py 127.0.0.1 9000 90`.
- On a timeout the client prints `[timeout, reconnecting]` and starts over. If some datagrams of a reply were lost, it prints what arrived and names the missing chunks.
- `loglevel N` raises the trace level of the running kernel. That way you can see a subsystem that traces above the default level, without rebuilding.

On a server set up from the system image, `nos-install --udpshell[=PORT]` turns the UDP shell on (see [Install nos on a dedicated server](/howto/install-on-a-server)).

## SSH

`sshd` is a loadable module that serves the same shell over SSH. It uses `curve25519-sha256` for key exchange, `ssh-ed25519` for host and user keys, and the `chacha20-poly1305@openssh.com` cipher. Each connection gets one session: a shell (with or without a terminal) or a single command. Everything else is refused: `sftp` (and therefore modern `scp`), port forwarding, and agent and X11 forwarding. With an allowed key you can log in under any user name. `qemu.sh`, `qemu-disk.sh` and `qemu-arm64.sh` forward host port 2222 to the guest's port 22.

### Start sshd at boot

At boot, nothing runs `insmod` unless `/etc/rc` says so. Each line of that file is a shell command. The shell's task runs them once, after DHCP has set up the network. So a root filesystem that holds the module, your key and that file comes up with SSH already running. For x86-64:

```sh
mkdir -p rootdir/etc/ssh rootdir/lib/modules
cp out/x86_64/modules/sshd.ko rootdir/lib/modules/
cp ~/.ssh/id_ed25519.pub rootdir/etc/ssh/authorized_keys
printf 'insmod /lib/modules/sshd.ko\nsshd start\n' > rootdir/etc/rc
./scripts/mkrootfs.sh root.img 64 rootdir
```

Boot the ISO with that image as its root disk. This is the machine the sshd test boots:

```sh
qemu-system-x86_64 -display none -m 1G -smp 4 -cdrom nos.iso -serial file:nos.log \
    -drive file=root.img,format=raw,id=drive0,if=none \
    -device virtio-blk-pci,drive=drive0,disable-legacy=on,disable-modern=off \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 -device virtio-rng-pci
```

When `nos.log` shows `sshd: listening on port 22`, log in:

```sh
ssh -p 2222 root@127.0.0.1
```

```text
nos -- `help` lists the commands, `exit` leaves
```

To run a single command instead: `ssh -p 2222 root@127.0.0.1 uptime`.

The same directory works for the disk image: run `ROOTFS=rootdir ./scripts/build-disk.sh`, then `./scripts/qemu-disk.sh`. For arm64, build the directory with the arm64 module, then turn it into the disk that the arm64 script boots:

```sh
mkdir -p rootdir-arm64/etc/ssh rootdir-arm64/lib/modules
cp out/aarch64/modules/sshd.ko rootdir-arm64/lib/modules/
cp ~/.ssh/id_ed25519.pub rootdir-arm64/etc/ssh/authorized_keys
printf 'insmod /lib/modules/sshd.ko\nsshd start\n' > rootdir-arm64/etc/rc
./scripts/mkrootfs.sh root-arm64.img 64 rootdir-arm64
qemu-img convert -f raw -O qcow2 root-arm64.img nos-arm64.qcow2
./scripts/qemu-arm64.sh
```

A server set up from the system image starts sshd the same way. Its `/etc/rc` is [these two lines](https://github.com/irqlevel/nos/blob/master/build/image/rc), and `nos-install --ssh-key` adds the key.

### Load sshd by hand

On a running nos, `insmod` loads a `.ko` from any mounted filesystem, and `wget` can download the file there from a web server on your machine. On QEMU's user network the host is `10.0.2.2`. Turn on the UDP shell (this example uses arm64) and serve the modules from the host:

```sh
python3 -m http.server 8000 --bind 127.0.0.1 --directory out/aarch64/modules
```

Then, in `udpsh.py`:

```text
$ wget http://10.0.2.2:8000/sshd.ko /sshd.ko
$ insmod /sshd.ko
$ sshd allow ssh-ed25519 AAAA... you@laptop
$ sshd start
```

`insmod` replies `module: sshd loaded at 0xFFFF8000...`. `sshd allow` takes a whole line from a `.pub` file and appends the key to `/etc/ssh/authorized_keys`. The first `sshd start` creates a host key at `/etc/ssh/ssh_host_ed25519_key`, then prints where it listens:

```text
sshd: made a host key, /etc/ssh/ssh_host_ed25519_key
sshd: listening on port 22 -- eth0 10.0.2.15 -- host key SHA256:... (ED25519), 1 authorized keys
```

Check that this fingerprint matches the one `ssh` shows on the first connection. To have sshd start again after a reboot, run `rc add insmod /sshd.ko` and `rc add sshd start`. That only helps if the root filesystem survives the reboot (see [Starting it at boot](/docs/sshd#starting-it-at-boot)).

### The `sshd` command

| Command | What it does |
|---|---|
| `sshd` | shows what it serves, its counters and who is logged in |
| `sshd start [port] [nic=eth0]` | starts listening, on port 22 unless you name another |
| `sshd stop` | closes the port and ends every session |
| `sshd allow <ssh-ed25519 AAAA... [comment]>` | lets a key in |
| `sshd deny <SHA256:...>` | removes a key, by fingerprint |
| `sshd keys [reload]` | lists the keys; `reload` re-reads the file |

The host key and the allowed keys live only as long as the root filesystem. On a ramfs root they are gone at the next boot, along with `/etc/rc`. The host key then changes, and `ssh` refuses to connect because the key changed. For a throwaway VM you can skip the check, as the sshd test does:

```sh
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p 2222 root@127.0.0.1
```

The module must come from the same tree, or the same release, as the kernel. The loader refuses a module built against a different kernel interface (see [What a module may call](/docs/modules#what-a-module-may-call)).

## Netconsole

The `netconsole=ip:port` kernel parameter makes the kernel send its log to a UDP collector, one line at a time, as the lines are written. The stream starts with the very first line of the boot, because the lines wait in a buffer until the network has an address. Netconsole needs that address, so use the parameter together with `dhcp=auto`. Start the collector:

```sh
python3 scripts/netconsole.py
```

It reports `netconsole: listening on 0.0.0.0:6666`. On QEMU's user network the host is `10.0.2.2`, and outgoing UDP needs no port forwarding, so the parameter is `netconsole=10.0.2.2:6666`. For arm64, add it in `scripts/qemu-arm64.sh`:

```diff
-APPEND="dhcp=auto dns=on root=auto"
+APPEND="dhcp=auto dns=on root=auto netconsole=10.0.2.2:6666"
```

For the x86-64 ISO, add it in `build/grub.cfg` and rebuild `nos.iso`:

```diff
-	multiboot2 /boot/kernel64.elf dhcp=auto dns=on root=auto fstest=on
+	multiboot2 /boot/kernel64.elf dhcp=auto dns=on root=auto fstest=on netconsole=10.0.2.2:6666
```

The collector puts the arrival time in front of each line (seconds since the collector started). The datagrams carry sequence numbers, and the collector reports a gap in them as `--- netconsole: N datagram(s) lost, seq A..B ---`. It also reports a sender that restarted. That way you can tell a log that the network cut short from a machine that stopped. The collector's options:

| Option | What it does |
|---|---|
| `-p PORT` | listens on PORT (default 6666) |
| `-b ADDR` | binds to ADDR (default 0.0.0.0) |
| `-o FILE` | also appends every line to FILE |
| `--from IP` | shows lines from this sender only |
| `--no-stamp` | drops the time prefix |
| `--show-source` | always prefixes the sender's address |

- `nctail=N` sends only the newest N KiB of the lines buffered before the link came up. If a machine dies soon after its network appears, it spends its little remaining time on the lines closest to the failure.
- The stream is the kernel log, the same lines `dmesg` holds, not the shell's echo. The `netconsole` shell command shows the target, how many bytes are buffered, and the drop and send counters.
- A panic report is flushed to the collector too, as a best effort. It goes out only if the collector's MAC address is already in the ARP cache.
- On a server set up from the system image, use `nos-install --netconsole IP:PORT`.

> The log goes out unencrypted, to whoever holds that address. Use netconsole only on a network you trust.

## Troubleshooting

- **`udpsh.py` keeps printing `[timeout, reconnecting]`.** The kernel was booted without `udpshell=`, or the port isn't forwarded (`qemu.sh` forwards no UDP port), or the command takes longer than the client's timeout. For a slow command, pass a longer timeout as the third argument.
- **`ssh` connects, but you never get a session.** Check from the console or the UDP shell. `lsmod` shows whether `sshd` is loaded, `sshd` shows whether it is listening and how many keys it has, and `dmesg 60 rc:` shows what `/etc/rc` did. With no allowed key, `sshd start` says `sshd: nobody can log in yet`. A key that isn't `ssh-ed25519` is never accepted.
- **`insmod` refuses the module as built against another kernel interface.** The `.ko` and the kernel come from different trees. Rebuild both, or take both from the same release.
- **The collector receives nothing.** Either the kernel has no address, because it was booted without `dhcp=auto`, or the collector listens on a different port. Nothing is sent before DHCP finishes.
- **The collector prints `--- netconsole: ... lost ---` lines.** UDP dropped datagrams on the way. The machine didn't stop at that point.

## Next steps

- [Debug nos with GDB](/howto/debug-with-gdb): panics, trace levels, a machine with no serial port
- [Install nos on a dedicated server](/howto/install-on-a-server), where the network is the only console
- [Write a loadable kernel module in Rust](/howto/write-a-module)
- [Boot a Linux guest under the nos hypervisor](/howto/linux-guest): `hv attach` drives a guest's console through `ssh -t`
- The full references: [UDP remote shell](/docs/udp-shell), [sshd](/docs/sshd), [Netconsole](/docs/netconsole)
