# sshd: an SSH server, as a module

`sshd` is a loadable module that serves the kernel's shell over SSH: an
OpenSSH client logs in with an Ed25519 key and gets the same commands the
console and the [UDP shell](udp-shell.md) have -- encrypted, and only for the
keys it was told to let in -- and copies files in and out with `sftp` and
`scp`, over SFTP ([Copying files](#copying-files-sftp)).

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
and `rc run` runs it now. The file is on the root filesystem, so it takes a
root on disk -- ext4, or an ext2 made before it -- to outlive a reboot
([Filesystems](filesystems.md)); a root image can also be made with it
inside -- `scripts/mkrootfs.sh` copies a directory in.
When a line in it keeps the machine from coming up, `rc=off` on the kernel
command line skips the file.

On a machine whose only console is the network this is how the UDP shell,
which runs any command for anyone who can send it a datagram, stops being
needed: boot once with `udpshell=`, fetch the module from the release the
kernel came from ([Releases](modules.md#releases)), allow a key, put the two
lines in `/etc/rc`, and boot without it. A server set up from the system image
starts out that way: the image's `/etc/rc` is those two lines, the module at
`/lib/modules/sshd.ko`, and `nos-install --ssh-key` allows the key
([Install](install.md)).

## The command

    sshd [status]                   what it serves, the counters, who is logged in
    sshd start [port] [nic=eth0]    serve; port 22 unless told
    sshd stop
    sshd allow <ssh-ed25519 AAAA... [comment]>
    sshd deny <SHA256:...>
    sshd keys [reload]

- `start` loads the host key -- or makes one, the first time -- and the
  authorized keys, and listens on the port of one device -- `nic=`, eth0 by
  default -- at every address the machine has, so a DHCP renewal that moves
  the address does not leave it deaf. Only a connection that comes in by
  that device is its: a guest behind the hypervisor's switch (`hv0`) is
  refused, at nos's address there and at eth0's. It fails when the host key file
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
| cipher | `chacha20-poly1305@openssh.com`, an AEAD, so no MAC -- its ChaCha20 and Poly1305 CRYPTOGAMS' assembly in the kernel's SIMD sections ([tls.md](tls.md#the-assembly)) |
| compression | none |

Rekeying is the client's to start and is followed wherever it comes, in the
middle of a command's output included. A connection gets one session channel:
a shell, with or without a terminal, one command (`ssh host dmesg`), or the
`sftp` subsystem -- SFTP version 3, as OpenSSH speaks it, which is what
`sftp` and, since OpenSSH 9.0, `scp` copy files with. The rest is refused:
port forwarding, agent and X11 forwarding, `env`, and the old `scp -O`, rcp's
protocol run through a command on the server.

## The shell behind it

A command line goes to `Cmd::dispatch`, as the console's and the UDP shell's
do. What it prints waits in a 64 KiB buffer and goes out on the channel where
that may be done: a command that prints a line now and then is seen as it
prints, one that prints everything at once goes out when it returns, in
pieces of 64 KiB when there is more -- a `dmesg` comes out whole rather than
cut at 32 KiB. Some commands print holding a spinlock with interrupts off
(`ps`, `stacks`, `arp`), when sending -- waiting for the client, allocating --
is not allowed: their output waits for them to return, and what would pass
the buffer meanwhile is dropped, with a note saying how much. With a terminal, the client's side is raw and the line editing is the
server's: printable characters, Backspace, Enter, Up and Down (^P and ^N)
through the last 32 lines, ^R to search back through them as bash does, ^C to
drop the line, ^U to clear it, ^L to clear the screen, ^D on an empty line to
log out ([Line editing](shell-commands.md#line-editing)); line ends are made
CR LF. `exit` and `logout` end the
session. The exit status is always 0: the kernel's commands have none.

A command runs on a task of its own, beside its session, and cannot be
interrupted, as on the console: ^C typed during `ping` reaches the editor
once `ping` is done. The session tends the connection meanwhile: it waits on
the command -- whose output wakes it at once, to go out on the channel -- and
after every quiet 10 ms reads what the client has sent: a keepalive is
answered, a window taken, a rekey done, typing kept for the editor, a close
seen. A command that waits a minute in silence (`hv wait`, `top 60000`) is
not cut off by a client that asks whether the server is there
(`ServerAliveInterval`), as it was while the command ran on the session's
own task and nothing read the connection until it returned. A command may
also read what is typed while it runs: the session hands it the channel's
data raw, as the keys are pressed, and flushes what it has printed before
each wait, a prompt with no line end included. `hv attach` is one such command: the console of a
guest, typed at through `ssh -t`. What a command does not read is the line
editor's once it returns. A command may ask for the terminal too
(`Output::terminal`): the width and height the client gave in its pty-req and
in each window-change since -- taken in as the session tends the connection,
so a resize reaches the command while it runs -- and how many window-changes
there have been, which a command that draws on the terminal goes by to draw
it again whole: a terminal made smaller and then as large again between two
looks has the size it had, and not the screen. `edit` is that command
([edit](modules.md#edit)). Without a terminal -- `ssh host cmd`, no `-t` --
there is none to ask for. And there is one user: whatever name the client
gives, a key in the list logs in, and what it can do is what the console
can.

## Copying files: SFTP

`sftp` and `scp` copy files into the machine and out of it -- any client of
SFTP does: `sshfs`, `rclone`, FileZilla, Cyberduck, WinSCP, paramiko:

```
$ scp -P 2222 bzImage root@127.0.0.1:/vms/
$ scp -P 2222 -r root@127.0.0.1:/var/log ./nos-logs
$ sftp -P 2222 root@127.0.0.1
sftp> put guest.qcow2 /images/guest.qcow2
sftp> ls -l /images
-rw-r--r--   1 root     root     2147549184 Jan  1  1970 guest.qcow2
sftp> df -h /images
sftp> rename /images/guest.qcow2 /images/debian.qcow2
```

The server is the sftp crate's (`src/crates/common/net/sftp`): the
protocol, and nothing of the kernel, run on the session's task a request at
a time, each answered on the channel before the next is looked at. What it
serves is the kernel's files through `kcore::fs`, as a module reaches them,
and every path is the one the client names -- made absolute against `/`,
which is every client's home (there being one user), its `.` and `..` taken
out, and nothing else looked for: a module's configuration file may have its
next content in `<path>.new` while it is replaced ([Files](#files)), and the
calls that read those look there too, but a client's `a.new` is a file of its
own, never taken for `a` or taken away with it.

What nos's filesystems keep is names and sizes, files and directories, and
that is what a client is told:

- **No owners, no permissions, no times.** Every file is said to be
  `-rw-r--r--` and every directory `drwxr-xr-x`, owned by `root`, of the
  epoch -- the time OpenSSH shows for one it is not told. What a client asks
  to set of them -- `chmod`, `chown`, `put -p`, `scp -p`, a time after a copy
  -- is refused, `SSH_FX_OP_UNSUPPORTED`, and nothing is done, rather than
  taken and dropped: the data of `scp -p` is copied, and scp says what it
  could not keep (`remote fsetstat: Operation unsupported`) -- and, OpenSSH
  9.9's, still ends with 0. A mode in an OPEN or a MKDIR, which every
  client sends, is read past. `rclone` sets a file's time after a copy:
  `--sftp-set-modtime=false`, and compare by size (`--size-only`).
- **No symbolic links**: SYMLINK is refused, READLINK says a path is no
  link.
- **A name is at most 63 bytes, a path 255** -- what the VFS takes; a longer
  one is refused by the server before the VFS is asked, as a failure where
  the request would make something there and as no such file elsewhere. A
  name must be UTF-8 and have no NUL in it.
- **A remove takes one file, an rmdir one empty directory** -- never a tree,
  as the shell's `rm` takes one; `rm -r` is the client's, a file and a
  directory at a time. A file open, here or anywhere -- a disk's image held
  by a running guest -- is not removed or renamed, and a rename does not go
  over a name that is taken (SFTP version 3's RENAME), nor across mounts.
- **A directory is read a few entries at a time** -- a hundred to a READDIR,
  fewer where the answer would pass 63 KiB -- with nothing of it held between
  them: one that changes while it is listed may show an entry twice or not
  at all, and each READDIR walks the directory to its place, which a
  directory of tens of thousands of entries makes slow.
- **Requests**: 64 KiB the longest (a longer one ends the session -- what
  follows it cannot be found), 63 KiB the most a READ answers and a WRITE
  carries, 32 handles open at once, files and directories, as
  `limits@openssh.com` says, which OpenSSH's client sizes its requests and
  keeps its requests in flight by. The session's window is 512 KiB, eight
  requests' worth, from the subsystem's start -- 256 KiB where the heap has
  no block of 512 to hold what the client may send.
- **Extensions**: `limits@openssh.com`, `statvfs@openssh.com` (`df`: the
  filesystem's bytes, in 4 KiB blocks, and no inodes), `fsync@openssh.com`
  (`put -f`: the file on its disk's medium before the copy is said done) and
  `expand-path@openssh.com` (`~`, the home: `/`). Not
  `posix-rename@openssh.com`, as no filesystem here replaces a name
  atomically, nor `copy-data`, `hardlink@openssh.com` or
  `home-directory`.
- **What it runs at** is what the session's TCP connection carries. Every
  connection to the port starts on its slot's own 8 KiB each way -- what
  knocks on port 22 all day holds no more -- its window scaled at the
  handshake for more (`TcpListener::bind_growable`); a session that starts
  SFTP is given bulk buffers both ways, up to 512 KiB each, as the
  subsystem starts (`Link::bulk`, `TcpStream::grow`: what the connection
  holds and has in flight moved across). They come from the kernel's 32
  (`MAX_BULK`), which ipcnet's links and `hv forward` draw on too: a shell's
  session keeps the slot's own, and a copy with none to be had goes on on
  them, the log saying so. `tests/sftp-bench.py`, 64 MiB over QEMU's user
  network, arm64 under HVF (2026-11-18), before and after: 61 to 107 MB/s
  in, 63 to 91 out; with a round trip of some 5 ms added, 0.6 to 7.3 in,
  1.1 to 9.4 out, and of some 15 ms, 0.2 to 2.7 and 0.4 to 3.5 -- where
  QEMU's user network is what holds it now, its own TCP keeping some 64 KiB
  in flight, under the 512 KiB a round trip nos's buffers and the session's
  window carry.

A file a client writes is written where each WRITE says, the file grown to
reach past it (`kernel_file_pwrite_grow`) -- a hole between its end and a
write past it -- and is not synced unless the client asks (`fsync@`, `put
-f`): a copy is on the filesystem, in its journal's next commit, once it is
said done. As a session ends the log says what it did:

```
sshd: root from 10.0.2.2:51544: sftp, 3 files opened (1 to be written), 8400953 bytes read and 8400953 written, 0 of 61 requests refused
```

## Limits

- 8 connections at once, of which at most 4 still logging in -- a port 22 on
  the internet is knocked on all day, and what knocks must not crowd out the
  login that matters. A connection past either is reset at once, as is one
  that ends without a login: closed the usual way, the side that closes first
  keeps a TCP slot through a minute of TIME-WAIT, and the kernel has 64.
  sshd's listener holds at most 8 connections nobody has accepted yet (its
  backlog, which every listener now asks for -- 16 at most, 4 for an `hv
  forward`), so a flood of SYNs costs the pool those and no more.
- 30 seconds from the connection to a login, rekeys included, and 6 failed
  attempts.
- A client quiet for 60 seconds is asked whether it is still there
  (`keepalive@openssh.com`); three questions unanswered and it is dropped.
- A client that stops taking output -- a suspended ssh -- has 5 minutes to take
  more, with its SSH window or its TCP one shut, before its session is given
  up; a stop meanwhile is not held up by it.
- A command still running holds up `sshd stop`, `rmmod sshd` and `poweroff`
  until it returns, as a module's command does. `sshd stop` from a session --
  its own task, or its command's -- is refused: it would wait for itself.
- **What a transfer runs at, and what holds it.** `ssh nos cat` of 48 MiB
  over QEMU's user network, arm64 under HVF (2026-11-16): 27 MB/s, on
  RustCrypto's cipher and on CRYPTOGAMS' alike -- the cipher is a few per
  cent of one CPU at that rate. What holds it is a connection's TCP buffers,
  the slot's own 8 KiB each way, which a round trip can carry no more of --
  so over a WAN a fraction of that -- and the command's output moved to the
  session, and sent, 2 KiB at a time (`OUT_CHUNK`). As an experiment, not as
  built: bulk buffers 32 MB/s; those and 8 KiB a packet 40 MB/s, CRYPTOGAMS'
  cipher 2% ahead of RustCrypto's there. A shell's session or a command's
  is not given bulk buffers -- they come from the kernel's 32 (`MAX_BULK`),
  which ipcnet's links and `hv forward` draw on -- a session copying files
  over SFTP is ([Copying files](#copying-files-sftp)): `scp` is what to copy
  a file out with, not `ssh cat`.

## How it is put together

- `src/crates/common/net/ssh` -- the protocol, and nothing of the kernel: the transport
  (version exchange, packets, key exchange), authentication, the session
  channel and the line editor. What it needs of the machine comes in through
  three traits: `Link` (the connection, the clock, the random pool, whether
  the server is stopping), `Shell` (whose keys may log in, and running a
  line) and `Primitives` (ChaCha20 from any counter block, and Poly1305: what
  the cipher and a session's random stream are made of). The key exchange
  and the signatures are the vendored dalek crates and `sha2` the TLS client
  already uses; a module being a build of its own, `sha2` is asked for its
  software backend here as well as in the kernel crate.
- `src/crates/common/net/sftp` -- the SFTP server, and nothing of the kernel:
  the requests read with the ssh crate's wire module, the answers made, the
  handles, the paths made absolute; what it serves comes in through its
  `Files` trait, and it is an `ssh::Subsystem` -- what a session's channel is
  given over to by the `subsystem` request, the client's bytes handed to it
  as they come and its answers sent back, with no line editor and no CR LF,
  in a window of its own.
- `src/crates/modules/net/sshd` -- the kernel's side: the `sshd` command, the files,
  the sftp crate's `Files` over `kcore::fs` (`KernelFiles`), a growable
  listener and a session's connection grown as a copy starts (`Link::bulk`),
  the ssh crate's `Primitives` -- the crypto crate's, CRYPTOGAMS' assembly in
  the kernel's SIMD sections, its integer code where none can be had --
  a listener task, a task for each connection, and one for each command a
  connection runs, with 16 KiB of its output and 4 KiB of typing between the
  two (`run_beside`); with no memory for those, the command runs on the
  session's task as it used to, and nothing reads the connection until it
  returns.
- What the kernel exports for it: `kernel_tcp_listen`, `kernel_tcp_accept`
  (with a timeout, returning once the listener is closed), `kernel_tcp_close`,
  `kernel_tcp_abort`, `kernel_tcp_send_timeout`, `kernel_tcp_peer`,
  `kernel_cmd_dispatch` (a command's output handed to a callback, where it may
  be), `kernel_file_size/read/write/create` and `kernel_dir_create`, and
  `kernel_task_current` -- wrapped by `kcore::tcp` (`TcpListener`,
  `TcpStream`), `kcore::cmd::dispatch`, `kcore::fs` and
  `kcore::task::current_id`; and for SFTP the file ABI at exactly the path
  given (`layers/fs/src/files.rs`): `kernel_file_stat`,
  `kernel_file_open_flags` (made, cut, made by this open or refused, under
  the lock it is made under), `kernel_file_pwrite_grow`, `kernel_dir_make`,
  `kernel_file_unlink` and `kernel_dir_remove` (one file, one empty
  directory, never a tree: `Vfs::remove_one`), with `kernel_file_pread`,
  `kernel_dir_entry`, `kernel_file_rename` and `kernel_fs_space` -- wrapped
  by `kcore::fs::stat`, `File::open_with`, `File::write_growing`,
  `make_dir`, `remove_file`, `remove_dir` and `dir_entry`.

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
-- the client's TCP window is 64 KiB at most here, since nos scales no window
on a connection it did not open, far below OpenSSH's channel window -- was aborted as dead
within a minute.

## Testing

`tests/sshd-test.py` is its end-to-end test, against OpenSSH's own client.
On arm64 (`--arch aarch64`, the default; HVF on an Apple Silicon Mac) it loads
the module over the UDP shell and works it: commands, a shell with a terminal
and one without, 3 MiB of output through the client's window with rekeys in
the middle of it, a command that prints nothing for five seconds under a
client that asks every second whether the server is there and gives up after
two unanswered, a key it has to refuse, sessions at once, `sshd stop` with a
session open and `rmmod sshd` from inside one; files copied in and out by
OpenSSH's `sftp` and `scp` -- 8 MiB and some both ways, a tree with `scp -r`,
a listing, a rename, `a.new` kept beside the `a` removed -- and each refusal
said: a remove of nothing, an rmdir of a directory with something in it, a
mkdir where there is one, `chmod`, a name of 64 bytes, `scp -p`; then it puts
the module in `/etc/rc`, reboots, and checks it came back by itself with the
same host key. On x86-64 (`--arch x86_64`) the root image carries the
module, a key and the `/etc/rc` from the start, no shell is touched, and a
file goes in over SFTP and back out the same. `tests/net-fuzz.py`'s `sftp`
target holds the server to SFTP request by request over a model of the
files, and its `ssh` target copies files in and back out through the module
itself.

## Security

The server runs in the kernel, with no privilege separation: a bug in it is a
kernel bug. The parser is safe Rust with every read bounds-checked, and the
cryptography is not its own. A signature is checked strictly (no small-order
key, no second form of a signature), a packet's tag in constant time, and the
client's X25519 share for a point of small order.

What it guards is the shell, and whoever has the shell has the kernel -- they
can `insmod` anything. SFTP reaches no further: every file the shell can
`cat` or `rm`, a key in the list can copy out or remove over SFTP, and
nothing more. The host key file is readable from that shell too; on
nos that is nobody who does not already have everything. Release images carry
`sshd.ko` but never a host key: one is made on the machine the first time.
