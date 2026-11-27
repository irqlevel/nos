# File logs

What a part of the kernel keeps for a person to read later, in files of its
own under `/logs`, rather than in the kernel log. The kernel log -- dmesg's
ring, the consoles, netconsole -- is for what the machine itself does and
what goes wrong with it; a line for every connection a machine on the
internet takes all day is not that, and at level 0 those lines crowded out
everything else there. Each log is a file, `/logs/<name>.log`, moved along
to `<name>.log.1`, `.2`, ... as it fills, held to a size for the file and
one for all its files together.

`/logs/net.log` is the network's: TCP's connections, made, accepted and
reset (`layers/net/src/tcp.rs`, `trace_event`); sshd's sessions -- who
came, who logged in and with which key, how each ended, what an SFTP copy
did, who was turned away; every command the UDP shell runs and who sent
it, and the ones it would not run (`layers/net/src/udp_shell.rs`); a TLS
handshake ipcnet's listener refused, and from where; and a connection `hv
forward` gave up for a side that took nothing, from whom and to which
guest. What a server itself does -- sshd started and stopped, its keys
read, the UDP shell started, ipcnet's own dials refused -- stays in the
kernel log.

```
2026-11-20T09:13:20.214790Z --- a boot: nos 0.1.33 (9fc9bbe9) ---
2026-11-20T10:20:40.771558Z tcp: accepted 22 <- 45811
2026-11-20T10:20:41.002319Z sshd: 198.51.100.80:45811 gone before a login: the client disconnected
2026-11-20T10:30:01.389199Z tcp: accepted 22 <- 49731
2026-11-20T10:30:01.913478Z sshd: root logged in from 203.0.113.7:49731 with SHA256:Xq3V... me@laptop
```

`/logs/hv.log` is the hypervisor's (`modules/hypervisor/hv`): what became of
each VM -- made (`hv create`, from a cloud's image by URL, a local file in
`/images` or an image of NBS's),
cloned, deleted, started (on which CPUs, with what kernel, memory and
command line), started again, stopped and after how long, and why; a
start, a make or a clone that failed and why; a move to another host and
one come from another, and what went wrong on either side; a split lock a
guest made (one a VM every five seconds at most, with how many came
between); a disk of NBS's attached and let go of; a cluster network's
tunnel to a host that stopped answering, a port vnet revoked. A VM is `vm <number> (<name>)`,
or `vm <number>` for one started from a kernel's path. The module itself --
loaded, unloaded, its extension on and off, a warning of its own -- and a
host's device given to a guest stay in the kernel log, as do the lines of
a guest's console mirrored there (`log=`).

```
2026-11-20T11:02:13.008211Z vm web made of https://cloud-images.example/noble.img -- /vms/web/vm.toml, a 10240 MiB disk
2026-11-20T11:02:41.517300Z vm 0 (web) started on cpus 2,3 -- /lib/hvboot, 2048 MiB, 2 cpus, cmdline "console=ttyS0"
2026-11-20T13:40:02.100023Z vm db not started -- input= is 5000 bytes, more than the 4096 a vm queues
2026-11-20T14:15:30.412871Z vm 0 (web) stopped after 9168895 ms -- on request
```

`/logs/nbs.log` is NBS's. Every host's member applies every change to the
metadata, and says each in its own file as it applies it: a volume made,
grown, attached and let go of and by whom, snapshotted, made an image,
cloned, deleted; a replica taken out of its set, by which volume's writer
and why -- it did not answer, or refused as failed; a cs down, up or out
and how many sets that changed; a join begun, done or given up; a device
added, drained, its format recorded or dropped; the members. So every host
has the cluster's history, the same lines in the same order, each stamped
when that host applied it. A change refused is said too (`refused at
<entry>: ... -- <why>`), through a limiter (below); a digest and a
copy-up's steps -- one a chunk a clone's writer writes first -- are not.
Beside them the host's own: its leader's controller giving up a join
(`controller:`), the scrub's findings (`scrub:`), its cs's devices and its
side of each join (`cs:`), and what each client session on the host met --
a replica asked out, a request refused, an operation failed at its
deadline with the queue state and request ages for one that never began
(`client:`), these through limiters too. The member replays its log as it starts; that is not said again -- but an entry applied before a
restart and not yet known committed may be said a second time after it.

```
2026-11-20T12:00:04.229401Z volume db (7) made, 8192 MiB, for host-a
2026-11-20T12:00:04.731122Z volume db attached for writing on host-a, generation 1
2026-11-20T12:47:19.003845Z set 12: cs 2 (host-c) taken out by volume db's writer, generation 1 -- it did not answer; the set's version 5
2026-11-20T12:47:19.004210Z client: cs 2 of set 12 taken out of volume db's chunks there (version 5)
2026-11-20T12:47:21.552301Z cs 2 (host-c) down: silent to the leader, 41 sets changed
2026-11-20T12:58:40.917745Z set 12: cs 2 (host-c) joins in place 2, the set's version 6
```

## A line

A line is its stamp, a space and its text. The stamp is the wall clock when
the line was written, UTC to the microsecond -- `2026-11-20T10:20:40.771558Z`
-- or, on a machine with no wall clock (no RTC read at boot), the boot
clock's seconds, `+4000.214790`. The text is what was written, cut at 240
bytes (`LOG_LINE_MAX`) and said to be cut (` [cut]` after it; a formatted
line is cut where a character begins), every ASCII control character but
the tab made `?` -- a line is one line, whatever its writer put in it, and
says nothing to the terminal it is read on.

The file says two things of its own. The first line of each boot written to
a log says the boot -- `--- a boot: nos 0.1.33 (9fc9bbe9) ---`, stamped with
the boot's start -- and lines lost are said about where they would have
been: `--- 12 lines lost here: the queue was full ---`.

## How a line gets there

`flog!(LOG, "fmt {}", arg)` formats the line on the stack where it is
written, stamps it with the boot clock and puts it on one queue every log
shares (`kcore::static_ring::Mailbox`, 512 lines): nothing is locked,
allocated or waited for, so a line may be written from anywhere -- the
receive path's soft IRQ, under a spin lock, an interrupt handler. The
writer, a task of its own (`logs`), is woken by the first line, lets lines
gather for a second (`GATHER_MS`), takes the queue and appends each log's
lines to its file with one write a batch. So a line is in its file within
a second of being written, and whoever wrote it never waits for a disk --
the TCP receive path least of all. `logs sync` cuts the gather short and
waits for the write.

A write is a transaction of the filesystem `/logs` is on -- ext4 flushes
the disk's cache for each -- and it holds that mount's lock while it runs,
so every other call on the mount waits for it: the second's gather makes a
trickle of lines one such write a second a log rather than one a line.
A burst is not left to wait that long: once the queue is half full
(`FILLING_AT`, 256 lines) the gather is cut short, to no less than 200 ms
(`GATHER_MIN_MS`) -- a flood is written in batches no more often than five
times a second, and lines are lost only where more than the queue holds
come within 200 ms.

A burst bigger than the queue before the writer gets to it loses lines:
counted, said in the file, shown by `logs`. One queue serves every log, so a
flood on one can cost another lines; each log's losses are its own count.

## A place that may say it a thousand times

A line about a failure can come as often as what fails: a replica that
refuses every request, at every request; a client asking again every
second. `flog_limited!(LOG, "fmt {}", arg)` writes such a line through the
limiter of its place -- each use of the macro has one of its own
(`kcore::logs::Limiter`, a `static` the macro makes):

- a place's window begins at its first line after its last window ended,
  and lasts a minute (`LIMIT_WINDOW_MS`); 30 of its lines go in it
  (`LIMIT_PLACE`), and once they have, every line until the window ends is
  held;
- lines alike -- the same but for their digits: a volume's number, a count,
  an address -- have windows of their own, begun the same way, 5 of a kind
  going in each (`LIMIT_ALIKE`). A line of another kind still goes while one
  kind floods. A place tells 8 kinds apart at once (`LIMIT_SHAPES`): a ninth
  takes the slot of the kind whose window began longest ago, whose count
  starts again -- the place's 30 hold them all;
- every line held is counted, and the next line of the place that goes says
  how many before it: `[held back: 1834] client: volume db: ...`. A flood
  that stops leaves its count to the place's next line, whenever that comes.

A failure a thousand times a second is then a few lines a minute and a
count. What is history rather than failure is not limited -- a change to
NBS's metadata, a VM started or stopped: each happened once, and a test
that counts them counts what happened.

## What it costs

Measured on arm64 under HVF (Apple Silicon, four CPUs, the host busy with
other guests: the quietest of four boots, and the range over all four), a
typical line of 71 bytes and four arguments:

| | a line |
|---|---|
| formatted alone | 58 ns (58-129) |
| `flog!`: formatted and put on the queue | 104 ns (104-394) |
| `klog!` at level 0, as these lines were written before: the ring and the consoles | 513 ns (513-2936) |
| `flog_limited!`, held, its place closed: the clock read and an atomic add, not formatted | 19 ns (19-49) |
| `flog_limited!`, held by its kind: formatted, its kind's sum taken, the place's lock | 169 ns (169-639) |
| the same, four CPUs at one place at once | 427 ns a CPU (427-861) |
| held, the place closed, four CPUs at once | 143 ns a CPU (143-300) |
| the writer's task, per line written: the VFS, ext4, the moves along | 6-10 µs |

The writer runs on its own task, never the caller's: a line costs its
writer a tenth of a microsecond or so where it is written, a fifth of what
the kernel log cost the same line, and whatever comes of the disk is the
`logs` task's. The most it can be made to do is bounded by the queue -- 512
lines a burst, a burst every 200 ms at most -- some 2,500 lines a second,
2-3% of a CPU; what is past that is counted lost, not written. What it
costs the others on its filesystem is the time of its writes under the
mount's lock: a transaction and a flush of the disk a log a second for a
trickle, five a second in a flood -- each as long as the disk takes to
flush, which is longest on a spinning disk; `/logs` on a filesystem of its
own (a mount of its own, its lock and journal its own) keeps them off the
root's. A flood through a
limiter is the cheapest of all: 30 lines a minute reach the writer, and the
rest cost their place tens of nanoseconds each once it is closed, a few
hundred before. No file log is written on a path per packet, per disk
request or per guest exit: a line is a connection, a session, a VM's or a
volume's change, or a failure behind a limiter.

## Rotation

A log is held to two limits (`fs::logs::Limits`): its file's size, 1 MiB
unless whoever makes the log says otherwise, and the size of all its files
together, 8 MiB. A batch that would take `<name>.log` past the first moves
it along first: `<name>.log` becomes `<name>.log.1`, `.1` becomes `.2`, and
so on, and the oldest go -- as many as it takes for those left to fit the
second limit with room for a whole file beside them, and to be no more than
99. A file always moves whole lines: a line is never split across two.

The old files are renumbered from 1 whenever the log moves along, whatever
an earlier boot or a person left -- gaps, a `.150` -- and only names of that
form are the log's: `net.log.01`, `net.log.1.bak` and anything else in
`/logs` are left as they are. Something that is not a file by one of the
log's names -- a directory called `net.log.2` -- is in the way, and the log
cannot be written (below).

A file someone has open -- `tail` reading it, an SFTP client copying it --
cannot be renamed (`Vfs::is_open`), so a log that would move along while one
of its files is open goes on into its file instead, to twice its limit at
most, and moves along once it can. Such a file, and one left past the limit
by a boot that had a bigger one, goes old whole: until the next move along
the log's files may take more than their total, by what that file is over.

## When a log cannot be written

The root read-only, no filesystem holding `/logs`, `/logs` a file, the
filesystem full, a disk failing, a file of the log's name that is not one:
the log says so in the kernel log once -- `logs: /logs/net.log cannot be
written -- its filesystem is read-only: its lines go to the kernel log until
it can` -- and its lines go there instead, `net.log: <line>`, so that a
line kept out of the kernel log is never a line lost. It is tried again
every ten seconds; when it can be written it says so, with how many lines
went to the kernel log meanwhile. A write that failed part way may have left
the start of a line in the file; the next write ends that line first, as it
does for one a power cut left.

## Boot and halt

The writer starts once the root is mounted (`rust_logs_start`, after
`rust_mount_root_fs` in both `main`s); a line written before that waits on
the queue. On a machine with no root on disk `/` is a ramfs, and the logs are
kept there -- in memory, to their limits. At the halt the writer is told to
finish just before the filesystems are unmounted (`rust_logs_stop`), after
the modules, whose last lines -- sshd's last sessions -- it writes first;
from there on a line goes to the kernel log. A panic writes nothing: what is
on the queue is lost with the machine (netconsole and the disk log carry
the panic itself).

## Reading them

```
tail /logs/net.log 50   the newest lines, from the last 64 KiB of the file
cat /logs/net.log       all of it
logs                    every log: its file, its old files, its limits, the lines it has
                        written, lost and sent to dmesg since the boot, whether it is written
logs sync               what is queued, written now
```

The files are plain text: `scp` and `sftp` copy them off the machine like
any other (docs/sshd.md).

## Writing to one

In the image, a log is a `static` of `fs::logs::Log`, by its name -- a
lowercase letter, then letters, digits, `-` and `_`, 15 bytes at most, which
a name that is not does not build -- and a line is `kcore::flog!`:

```rust
pub static LOG: fs::logs::Log = fs::logs::Log::new("net");
// or with limits of its own:
// fs::logs::Log::with_limits("audit", fs::logs::Limits { file: 4 << 20, total: 64 << 20 })

kcore::flog!(crate::LOG, "tcp: accepted {} <- {}", local_port, remote_port);
```

A module has `kcore::logs::Log`, the same shape, over the C ABI
(`kernel_log_open`, `kernel_log_write` in `base/ffi/src/logs.rs`):

```rust
static NET: kcore::logs::Log = kcore::logs::Log::new("net");

kcore::flog!(NET, "sshd: {} logged in from {}", user, peer);
```

A line that may come as often as a failure does goes through its place's
limiter (above):

```rust
kcore::flog_limited!(NBS, "client: {}", line);
```

A log is made the first time a line is written to it, by whichever `Log` of
its name writes first, with that `Log`'s limits; every `Log` of one name --
the image's and any module's -- writes the same files. A name or limits the
kernel refuses (a file under 4 KiB or over 64 MiB, all the files under twice
a file or over a hundred files' worth or 1 GiB), or a table with no room
for another log (16), is said once in the kernel log, and that `Log`'s lines
go to the kernel log. A log, once made, is there until the machine stops: a
module unloaded and loaded again writes to the same one. Making a log takes
an interrupt-safe lock for a few dozen compares and allocates nothing, so
even a log's first line may be written from anywhere.

## Tested by

`fs-fuzz.py`'s `logs` target (host, in CI): lines from tasks, interrupt
handlers and the world, through the image's `Log`, a module's and the C ABI,
before the writer starts, while it runs and after it stops, in floods past
the queue, to logs with small limits over a `/logs` that already has old
files, others' files, things in the way, a file held open -- or is a file --
on a root writable, read-only or not there; and every line held to exactly
one place -- its files, the kernel log, or counted lost -- in its file in
the order written, stamped right (read back by the judge's own calendar),
its text as written; the boot said; the files within their limits as the
rules above say, the old ones renumbered, the newest kept, others' files
untouched; a failure said once a reason; `tail`'s lines the file's; and the
writer's timing on the machine's clock -- `logs sync` answered at once, a
trickle written a second after it came and not before, a burst of half the
queue 200 ms after and not before. Its controls -- twelve mistakes put in
the engine, from the budget's sum to the boot left unsaid, and seven in the
gather, from the half-full queue waking nobody to a gather of 200 ms -- each
fail it within the gate's inputs. `sshd-test.py`
(arm64, and `--arch x86_64`) then reads `/logs/net.log` on a booted machine:
the logins, TCP's accepts and an SFTP session's line there -- on arm64 the
UDP shell's commands too, each with who sent it -- and none of them in dmesg.

Its `limiter` target holds the limiter to a model of the rules above, its
own: kinds of line with digits that change, some of characters of more
than a byte and some longer than a line, at times on either side of a
window's end -- the place's and a kind's -- in floods that close the place,
and now and then a line that passed the closed place's look as its last
line went on another CPU. Each line's fate is the model's where the input
has no more kinds than the limiter tells apart; whatever it has, every held
line is said exactly once by the next that goes, no window of the place
lets more than its 30 through, and the count before a line fits the line's
room, whole characters, a cut said. Its controls -- ten mistakes put in the
limiter, off by one at each bound, the closed count or the held one
dropped, digits counted in a kind, the count said wrong -- each fail it
within the gate's inputs. `nbs-test.py` (arm64, and `--arch x86_64`) reads
`/logs/nbs.log`: no replica of a slow cs taken out, counted in the
metadata's history, and what its joins copied, from the cs's own lines;
`hv-linux-test.py` (x86-64, **manual**) reads `/logs/hv.log`: a VM's start
and its stop with how long it ran, and neither in dmesg.
