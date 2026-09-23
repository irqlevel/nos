# sshd: an SSH server, as a module

`sshd` is a loadable module that serves the kernel's shell over SSH: an
OpenSSH client logs in with an Ed25519 key and gets the same commands the
console and the [UDP shell](udp-shell.md) have -- encrypted, and only for the
keys it was told to let in.

```
$ insmod /sshd.ko
module: sshd loaded at 0xFFFF800031000000
$ sshd allow ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICExj27laz1eRnjp0PMUnGChfWMRtDJIHyWszKdj0on3 me@laptop
sshd: allowed SHA256:PNUlq9Yv5iOWSuGfuKTQ9ej3D3cnaTkdqY9m6hdkhNc me@laptop
$ sshd start
sshd: made a host key, /etc/ssh/ssh_host_ed25519_key
sshd: listening on port 22 -- eth0 10.0.2.15 -- host key SHA256:EXCYywFbF3JdnyDGKpFJfrZWUFB2xYH4/rodvZ+A9hg (ED25519), 1 authorized keys
```

and from anywhere else:

```
$ ssh root@10.0.2.15 uptime
205.613949
$ ssh root@10.0.2.15
nos -- `help` lists the commands, `exit` leaves
$ dmesg 3 sshd
...
$ exit
```

The first connection asks about the host key: compare its SHA256 fingerprint
with the one `sshd start` and `sshd` print. Under QEMU the run scripts forward
host port 2222 to the guest's 22, so it is `ssh -p 2222 root@127.0.0.1`.

## Starting it at boot

A module is loaded by `insmod`, and nothing runs `insmod` at boot unless told
to: that is what `/etc/rc` is for. Its lines are shell commands, run once by
the shell's task after DHCP has configured the network, their output going to
the kernel log (and to the screen of a machine that has one):

```
$ rc add insmod /sshd.ko
$ rc add sshd start
$ rc
1  insmod /sshd.ko
2  sshd start
```

`rc` alone shows the file, `rc del <n>` takes a line out, `rc clear` removes it
and `rc run` runs it now. The file is on the root filesystem, so it takes an
ext2 root to outlive a reboot ([Filesystems](filesystems.md)); a root image can
also be made with it inside -- `scripts/mkrootfs.sh` copies a directory in.
When a line in it keeps the machine from coming up, `rc=off` on the kernel
command line skips the file.

On a machine whose only console is the network this is how the UDP shell,
which runs any command for anyone who can send it a datagram, stops being
needed: boot once with `udpshell=`, fetch the module from the release the
kernel came from ([Releases](modules.md#releases)), allow a key, put the two
lines in `/etc/rc`, and boot without it.

## The command

    sshd [status]                   what it serves, the counters, who is logged in
    sshd start [port] [nic=eth0]    serve; port 22 unless told
    sshd stop
    sshd allow <ssh-ed25519 AAAA... [comment]>
    sshd deny <SHA256:...>
    sshd keys [reload]

- `start` loads the host key -- or makes one, the first time -- and the
  authorized keys, and listens on the port at every address the machine has,
  so a DHCP renewal that moves the address does not leave it deaf (`nic=`
  names the device whose address it shows). It fails when the host key file
  is there and cannot be read: a new key in its place would make every client
  that knew the old one refuse to connect, so it has to be moved out of the
  way on purpose -- and a key is only ever made where there is no file, never
  over one that a failed read did not see.
- `stop` closes the port, ends every session and waits for them. From one of
  its own sessions it refuses, since it would be waiting for the task it runs
  in; `rmmod sshd` works from one, and the session ends with it.
- `allow` takes the line of a `.pub` file, adds the key and appends it to
  `/etc/ssh/authorized_keys`; `deny` takes a fingerprint, as `sshd keys` shows
  them, and removes the key from both -- from the file whatever the module
  has read, before a start too. The file is read when the module is loaded,
  and `keys reload` reads it again, for a file changed some other way (`wget`,
  say). Every change to it is made under one lock, read to write, so two
  shells at once do not each write the other's key away.

## Files

| Path | What |
|---|---|
| `/etc/ssh/ssh_host_ed25519_key` | The host key, in OpenSSH's own format: `ssh-keygen -l -f` shows its fingerprint, and a key made by `ssh-keygen -t ed25519 -N ''` can be put here -- the one the machine's other OS uses, so clients see the same host whichever of the two is booted |
| `/etc/ssh/authorized_keys` | One key a line, as OpenSSH writes them. `ssh-ed25519` keys only; a line with options in front of its key is skipped with a warning rather than taken without them, since the server would not honour `from=` or `command=` |

A file is never written over in place: the new content goes to `<path>.new`,
is synced, and only then takes the old one's place (`kernel_file_write`), so a
full disk or a machine stopping midway leaves the old content rather than an
empty file -- a machine whose only way in is SSH is not locked out by an
`allow` at the wrong moment. Cut short between the two steps, the content is
in `<path>.new`, and read from there. `/etc/rc` is written the same way.

They last as long as the root does. Without a root disk `/` is a ramfs (the
fallback layout in [Filesystems](filesystems.md)): the host key is made and
kept there, and is gone at the next boot -- as is any `/etc/rc`, so there the
module is loaded by hand. A root that cannot be written at all keeps nothing,
and `sshd start` says so: the key then changes at every start.

## What it speaks

One of each, what every OpenSSH of the last ten years offers first or nearly:

| | |
|---|---|
| key exchange | `curve25519-sha256` (RFC 8731), with OpenSSH's strict key exchange (`kex-strict-s-v00@openssh.com`) -- the fix for Terrapin |
| host key | `ssh-ed25519` (RFC 8709) |
| user authentication | `publickey` with `ssh-ed25519` keys; no passwords |
| cipher | `chacha20-poly1305@openssh.com`, an AEAD, so no MAC |
| compression | none |

Rekeying is the client's to start and is followed wherever it comes, in the
middle of a command's output included. A connection gets one session channel:
a shell, with or without a terminal, or one command (`ssh host dmesg`). The
rest is refused -- `sftp` and so modern `scp`, port forwarding, agent and X11
forwarding, `env`.

## The shell behind it

A command line goes to `Cmd::Dispatch`, as the console's and the UDP shell's
do. What it prints waits in a 64 KiB buffer and goes out on the channel where
that may be done: a command that prints a line now and then is seen as it
prints, one that prints everything at once goes out when it returns, in
pieces of 64 KiB when there is more -- a `dmesg` comes out whole rather than
cut at 32 KiB. Some commands print holding a spinlock with interrupts off
(`ps`, `stacks`, `arp`), when sending -- waiting for the client, allocating --
is not allowed: their output waits for them to return, and what would pass
the buffer meanwhile is dropped, with a note saying how much. With a terminal, the client's side is raw and the line editing is the
server's: printable characters, Backspace, Enter, Up and Down through the last
32 lines, ^C to drop the line, ^U to clear it, ^L to clear the screen, ^D on an
empty line to log out; line ends are made CR LF. `exit` and `logout` end the
session. The exit status is always 0: the kernel's commands have none.

A command runs on the session's own task and cannot be interrupted, as on the
console: ^C typed during `ping` reaches the editor once `ping` is done -- unless
the command reads what is typed while it runs, which a command may: the
session hands it the channel's data raw, as the keys are pressed, reading
more of the connection while it waits (a rekey and a close are dealt with on
the way), and flushes what the command has printed before each wait, a prompt
with no line end included. `hv attach` is one such command: the console of a
guest, typed at through `ssh -t`. What a command does not read is the line
editor's once it returns. And there is one user: whatever name the client
gives, a key in the list logs in, and what it can do is what the console
can.

## Limits

- 8 connections at once, of which at most 4 still logging in -- a port 22 on
  the internet is knocked on all day, and what knocks must not crowd out the
  login that matters. A connection past either is reset at once, as is one
  that ends without a login: closed the usual way, the side that closes first
  keeps a TCP slot through a minute of TIME-WAIT, and the kernel has 64. A
  listener holds at most 16 connections nobody has accepted yet, so a flood
  of SYNs costs the pool those and no more.
- 30 seconds from the connection to a login, rekeys included, and 6 failed
  attempts.
- A client quiet for 60 seconds is asked whether it is still there
  (`keepalive@openssh.com`); three questions unanswered and it is dropped.
- A client that stops taking output -- a suspended ssh -- has 5 minutes to take
  more, with its SSH window or its TCP one shut, before its session is given
  up; a stop meanwhile is not held up by it.
- A command still running holds up `sshd stop`, `rmmod sshd` and `poweroff`
  until it returns, as a module's command does.

## How it is put together

- `src/rust/ssh` -- the protocol, and nothing of the kernel: the transport
  (version exchange, packets, key exchange), authentication, the session
  channel and the line editor. What it needs of the machine comes in through
  two traits: `Link` (the connection, the clock, the random pool, whether the
  server is stopping) and `Shell` (whose keys may log in, and running a line).
  The cryptography is the vendored RustCrypto and dalek crates the TLS client
  already uses; a module being a build of its own, `sha2` is asked for its
  software backend here as well as in the kernel crate.
- `src/rust/modules/sshd` -- the kernel's side: the `sshd` command, the files,
  a listener task and a task for each connection, whose stack the commands it
  runs share.
- What the kernel exports for it: `kernel_tcp_listen`, `kernel_tcp_accept`
  (with a timeout, returning once the listener is closed), `kernel_tcp_close`,
  `kernel_tcp_abort`, `kernel_tcp_send_timeout`, `kernel_tcp_peer`,
  `kernel_cmd_dispatch` (a command's output handed to a callback, where it may
  be), `kernel_file_size/read/write/create` and `kernel_dir_create`, and
  `kernel_task_current` -- wrapped by `kcore::tcp` (`TcpListener`,
  `TcpStream`), `kcore::cmd::dispatch`, `kcore::fs` and
  `kcore::task::current_id`.

The server is the first user the TCP stack's passive side has had, and it
changed on the way. `Accept` takes a timeout and notices its listener being
closed; it hands out a connection whose peer closed before it was taken --
nothing else would ever close that CLOSE-WAIT, whose slot was gone for good --
and never one of the kernel's own connections that happens to use the port.
Closing a listener resets what finished its handshake on the port and was
never accepted, a listener's backlog is capped, and `Abort` resets a
connection. And a shut window is probed by the persist timer alone: `Send`
used to push a byte of data into it, whose retransmits the peer's duplicate
ACKs never answered, so a connection to a client that was only slow to read
-- the TCP window here is 64 KiB, far below OpenSSH's channel window -- was
aborted as dead within a minute.

## Testing

`scripts/sshd-test.py` is its end-to-end test, against OpenSSH's own client.
On arm64 (`--arch aarch64`, the default; HVF on an Apple Silicon Mac) it loads
the module over the UDP shell and works it: commands, a shell with a terminal
and one without, 3 MiB of output through the client's window with rekeys in
the middle of it, a key it has to refuse, sessions at once, `sshd stop` with a
session open and `rmmod sshd` from inside one; then it puts the module in
`/etc/rc`, reboots, and checks it came back by itself with the same host key.
On x86-64 (`--arch x86_64`) the root image carries the module, a key and the
`/etc/rc` from the start, and no shell is touched.

## Security

The server runs in the kernel, with no privilege separation: a bug in it is a
kernel bug. The parser is safe Rust with every read bounds-checked, and the
cryptography is not its own. A signature is checked strictly (no small-order
key, no second form of a signature), a packet's tag in constant time, and the
client's X25519 share for a point of small order.

What it guards is the shell, and whoever has the shell has the kernel -- they
can `insmod` anything. The host key file is readable from that shell too; on
nos that is nobody who does not already have everything. Release images carry
`sshd.ko` but never a host key: one is made on the machine the first time.
