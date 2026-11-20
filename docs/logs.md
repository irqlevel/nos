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
reset (`layers/net/src/tcp.rs`, `trace_event`), and sshd's sessions -- who
came, who logged in and with which key, how each ended, what an SFTP copy
did, who was turned away. What a server itself does -- sshd started and
stopped, its keys read -- stays in the kernel log.

```
2026-11-20T09:13:20.214790Z --- a boot: nos 0.1.33 (9fc9bbe9) ---
2026-11-20T10:20:40.771558Z tcp: accepted 22 <- 45811
2026-11-20T10:20:41.002319Z sshd: 198.51.100.80:45811 gone before a login: the client disconnected
2026-11-20T10:30:01.389199Z tcp: accepted 22 <- 49731
2026-11-20T10:30:01.913478Z sshd: root logged in from 203.0.113.7:49731 with SHA256:Xq3V... me@laptop
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
writer, a task of its own (`logs`), is woken by the first line of a burst,
lets the burst gather for 200 ms, takes the queue and appends each log's
lines to its file with one write a batch. So a line is in its file a moment
after it is written, and whoever wrote it never waits for a disk -- the TCP
receive path least of all. `logs sync` waits for the moment to pass.

A burst bigger than the queue before the writer gets to it loses lines:
counted, said in the file, shown by `logs`. One queue serves every log, so a
flood on one can cost another lines; each log's losses are its own count.

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
untouched; a failure said once a reason; `tail`'s lines the file's. Its
controls -- twelve mistakes put in the engine, from the budget's sum to the
boot left unsaid -- each fail it within the gate's inputs. `sshd-test.py`
(arm64, and `--arch x86_64`) then reads `/logs/net.log` on a booted machine:
the logins, TCP's accepts and an SFTP session's line there, and none of them
in dmesg.
