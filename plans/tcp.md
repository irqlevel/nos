# tcp: what nos's TCP needs before a cluster runs on it

Asked 2026-11-05, while planning NBS. `ipcnet` carries everything between a
cluster's hosts -- Raft, the registry, NBS's data -- over TCP with TLS
([ipc.md](ipc.md), "TCP, not UDP"). This is what that TCP has to get first,
measured before and after. The host API, sshd, wget and `hv forward` gain
from the same changes.

## Where it starts

`src/crates/net/src/tcp.rs`:

- **64 connections for the whole kernel** (`MAX_CONNECTIONS`), each with 8
  KiB of its own each way.
- **Bulk buffers**, 512 KiB from the heap:
  - A listener's accepted connections get them both ways (`bind_bulk`).
  - An active open gets them on the receive side alone (`Receive::Bulk`,
    which wget's downloads use). Its send buffer stays the slot's 8 KiB.
  - A module's active open gets neither: `kernel_tcp_connect(dev, ip, port)`
    takes no buffers. A sender with 8 KiB in flight moves 8 KiB a round
    trip, about 27 MB/s at the 0.3 ms between two servers in a data centre.
- **Congestion control is in** (RFC 5681, NewReno RFC 6582, an initial
  window of ten segments), and so is window scaling. Recovery after a
  timeout is go-back-N. There is no SACK.
- **The retransmit timeout is not taken from the round trip.** It starts at
  1 s (`INITIAL_RTO_MS`), doubles to at most 8 s, and goes back to 1 s on
  progress. One kernel timer, every 200 ms (`TIMER_PERIOD_MS`), looks at
  every connection's deadline. A lost segment that no three later ones
  follow -- the last of a request -- stalls its connection for a second.
  Fast retransmit, on the third duplicate ACK, covers the rest.
- **A connection is given up after 8 retransmissions** (`MAX_RETRANSMITS`):
  some 47 s of doubling timeouts.
- **Strangers**:
  - A SYN for a listener takes a slot as soon as the backlog has room. A
    server that refuses an address does it after `accept`, as the API's
    `admit` does, with a reset.
  - Every segment for a port nobody listens on is answered with an RST, one
    each, with no limit.
- **Neither Nagle nor delayed ACKs**: a segment goes out at once and is
  acknowledged at once. That is right for request and response, and stays.

## What changes

**T1, bulk both ways for an active open, and for modules** -- done on
branch `ipc-raft`: `Receive::Both`, `kernel_tcp_connect_bulk`,
`kcore::tcp::TcpStream::connect_bulk`, and net-fuzz's `tcp` target opening
with it; the throughput between the AX41 and the EX44 is still to measure.
`TCP.connect`'s
`Receive::Bulk` becomes 512 KiB each way from the heap, the window scaled,
as `bind_bulk`'s connections have. kcore gets `TcpStream::connect_bulk`
over a `kernel_tcp_connect` that says which buffers it wants. Measured
between the AX41 and the EX44: one connection's throughput, which should
reach the 1 GbE line rate (117 MB/s) where it is 27 MB/s now.

**T2, a timeout from the round trip.** Done on branch `ipc-raft` but for
the measurements: the estimate, a floor of 200 ms and giving up by time;
a connection's own floor, down to 20 ms, for the links between nos hosts
(`Tcp::set_rto_floor`, a module's `TcpStream::set_rto_floor`); and the timer a task (`tcp/timer`, on CPU 0) asleep until the
earliest retransmission any connection has or the next 200 ms sweep -- for
TIME-WAIT, the persist timer and slots given back -- where it was a soft
IRQ raised every 200 ms whatever was due. A deadline is set only through a
type that tells the timer, and net-fuzz holds every connection's to being
acted on within a tick of the machine's; its tcp target gives
most connections a floor of their own -- mostly the 20 ms -- and holds each
one's timeout to its floor, the floor to what may be given, and a new
connection to 200 whatever its slot's last had. Of the timer's five
mutants it catches all five: the fifth -- a deadline set that does not
wake the task -- shows only under a floor below the sweep's period, where
a deadline the task is not told of waits for the sweep; and four mutants
of the floor besides.
- The round trip is measured on every acknowledged segment that was not
  sent again (Karn's rule). SRTT and RTTVAR follow RFC 6298, in integers.
- The RTO is SRTT plus four RTTVARs, no less than 200 ms -- Linux's floor,
  not the 20 ms first planned. A peer that delays its acknowledgements
  holds the one for a lone segment 40 ms and up to 200 (Linux's delayed
  ACK, longer in its interactive mode), hoping to send it with an answer: a
  floor under that sends every last segment of a request again, against
  the Linux guests a forward relays to over the switch first of all, and
  Karn's rule keeps the delay out of the estimate, so it never learns. nos
  acknowledges every segment at once, so a link both of whose ends are nos
  -- ipcnet's -- is given a floor of its own, as Linux's `ip route ...
  rto_min` gives a route one: 20 ms, set where the link is made (I4) and
  measured there. It takes effect from the next round trip measured: a
  timeout of the moment is never lowered by it, since it may be backed off
  from a loss that only the next sample says is over (RFC 6298, 5.7). A path of 50 ms, the Mac to Hetzner, gets an RTO of a few
  hundred ms from its own measurements either way.
- The timer is armed for the earliest deadline any connection has, to the
  millisecond, instead of a look every 200 ms. The 200 ms sweep stays, for
  what is not a retransmission; an idle machine still has it, as it has its
  tick a hundred times a second -- a sweep only where something needs one is
  for when that is worth its bookkeeping.
- A connection is given up by time, not by count: after 100 s with nothing
  acknowledged (RFC 1122's R2 for data: at least 100 s), whatever its RTO.
  From a floor of 200 ms eight doublings are over in half a minute, from
  20 ms in five seconds, either of which would cut off an ssh session on a
  flaky link -- the user's Wi-Fi and VPN to the servers are one. A
  handshake is still given up after eight tries.
- Measured on `scripts/hub.py` (I4's harness, [ipc.md](ipc.md)), which
  drops the last segment of a burst on purpose: the stall, a second today,
  should become the RTO -- 200 ms, and tens of ms on a link between nos
  hosts.
  Then on the boxes: request and response p50 and p99 under load, and how
  many retransmissions turn out spurious.

**A receive that waits on an event.** Done on branch `ipc-raft`, for
ipcnet (I4): `recv` slept a millisecond and then up to ten between looks,
which is most of what a round trip between two hosts cost (7 ms at the
median, on the QEMU LAN). Each connection now has an event, made at
`init`, that `recv` sleeps on -- a hundred milliseconds at most, a missed
signal's whole cost -- and that every place marking data ready signals: data,
an end, a reset. And `Tcp::wake` (`kernel_tcp_wake`, kcore's `TcpWaker`)
ends a receive under way, or the next, as a timeout, for a task that waits
on its connection and on a queue of its own; it is held to the
connection's generation, bumped under the connection's lock as its slot is
handed out, so a wake of a connection gone reaches nobody, and the first
receive back after it -- with data or not -- has it. net-fuzz's tcp target
wakes connections by their own generation and by stale ones, and holds a
receive with no timeout to coming back as one only when its connection was
woken; three mutants (the generation not held to, a wake kept across a
slot's handout, a wake not had by a receive with data) fail it. A round
trip of ipcnet went from 7 ms to 0.4 ms at the median.

**T3, strangers kept off before they cost anything.** Done on branch
`ipc-raft` but for the module's way to it and the measurement, which come
with ipcnet (I4), its first user: `Tcp::listen_allowed` and `set_allowed`
in the image, with no C ABI yet -- an export nobody calls is not added. Any
segment from a stranger to an allowlisted port is dropped, not only a SYN:
an ACK's probe answered with a reset would say the port is there. nos sends
no ICMP error, and its echo replies are not held to a rate, as Linux's are
not: a ping is answered one for one, nothing amplified. `tcpstat` counts
all three (`refused=`, `halfopen=`, `rstheld=`), and net-fuzz's tcp target
holds each to a model -- a stranger, a list changed under it, floods at a
closed port -- with eight mutants caught.
- A listener can be given the addresses it answers (`bind_allowed`). A SYN
  from any other is dropped where the listener is found, before the backlog
  and before a slot: no SYN-ACK, no RST, no slot.
- A half-open connection times out in 3 s, not the full connect timeout.
- The RSTs and ICMPs the stack sends a stranger are held to a rate, a
  token bucket over the whole stack: a flood at closed ports cannot turn
  this host into a sender of as many resets.
- The API can take the same allowlist into the listen path instead of its
  `admit`.
- Measured: a SYN flood from QEMU's host at `ipcnet`'s port, with no slot
  taken and sshd still answering on another; the resets counted against the
  limit.

**T4, recovery from loss and reordering.** Asked for 2026-11-06 and done
on branch `ipc-raft`, measured with `scripts/tcp-loss-bench.py`: two nos on
hub.py's LAN, ipcnet's bulk connection between them, megabytes echoed under
each impairment on links made fresh for it.

- **The sender woken by acknowledgements.** A send waiting for room slept
  1 ms and then up to 10; it waits on its connection's event, which every
  segment taken in signals. Alone, loss1 went from 6.1 to 7.9 MB/s and loss5
  from 1.7 to 2.6.
- **ipcnet's floor at both ends.** The end that accepted a link kept the
  200 ms every connection starts with, and waited out each of its losses
  ten times as long as the end that dialed; ipc-test now looks at both.
- **SACK both ways** (RFC 2018), SACK permitted in every SYN of nos's and
  in a SYN-ACK to one that offers it; the receiver's blocks newest first,
  its kept stretches up from 8 to 32, and D-SACK (RFC 2883) for what came
  twice -- an old duplicate answered at once, not at the 2 a second a
  challenge goes at.
- **A table of the segments in flight** (`Sent`): each segment's end,
  fate (in flight, lost, sent again, held by the peer) and when it last
  went; 64 in the slot, and a bulk send buffer's one for every 512 bytes
  from the heap with it. The table covers exactly what the send buffer
  holds; a full one holds a send back as a full buffer does.
- **RACK-TLP** (RFC 8985) finds what is lost by time: a segment sent before
  one the peer is known to have, and longer ago than a round trip and a
  reordering window -- none until the path is seen to reorder, then a
  quarter of the least round trip, a quarter more for each round with a
  D-SACK, never past the smoothed round trip; and a loss probe two round
  trips into a silence, the newest segment again.
- **PRR** (RFC 6937) paces the recovery that starts: every hole sent again
  the round trip it is found in, the window brought to half over it.
- **Undo** (RFC 3708): a recovery or a timeout all of whose copies the peer
  says came twice gives the window back -- its D-SACKs counted until the
  next recovery, as Linux keeps its undo_marker; and a timeout after which
  an acknowledgement newly delivers an original and no copy was spurious
  (F-RTO's SACK-enhanced test, RFC 5682, as Linux has it).
- **A timeout** counts lost what the peer does not hold and sends that
  again as slow start lets it; **a peer that reneges** (RFC 2018 8) is
  found out when its acknowledgement stops at what it SACKed. A block that
  reaches SND.UNA is refused, as Linux's tcp_is_sackblock_valid refuses it.
- **Karn's rule for the segment timed alone.** Any copy of anything stopped
  the timing, so a path whose round trip grew never had it measured --
  each probe went after two of the old round trips -- and the timeout
  stayed under the new one.
- A peer that does not take SACK has the sender NewReno, as before.
- **The window offered never past the receive buffer.** A scaled window
  held its old edge rounded up to a unit (16 bytes on a bulk connection),
  and so crept up to a unit further with every acknowledgement while the
  reader was behind -- past the buffer's end within a few: the peer's data
  there was lost (`unkept=`), and its acknowledgements, sent from past
  RCV.NXT, were dropped as outside a shut window, which starved the other
  direction's sender of them (`outside=`, a couple of hundred a clean run,
  found with the `tcp_recv` tracepoint's `verdict=outside`). The window is
  now what is free, rounded down -- the retracted window RFC 7323 2.4 lets
  a receiver offer rather than one past its buffer, by less than a unit --
  with the furthest edge offered kept and honoured (rule 1), and a segment
  of no length taken at the window's right edge, as Linux and BSD take it.
- **Silly window avoidance, both halves** (RFC 1122 4.2.3.3 and 4.2.3.4):
  the edge moves on only by a segment or half the buffer, and a sender does
  not send a segment the peer's window cuts short while anything is in
  flight -- unless that window is half the largest the peer offered. A
  rounded-down edge moves a few bytes either way as RCV.NXT does, and a
  sender filling every opening sent a thousand segments of 4 to 12 bytes a
  direction in a 24 MB run; 70 now.
- **ipcnet's link reads what has come.** A turn of its loop read one TLS
  record, then flushed for up to a millisecond into the peer's window: with
  both directions busy and every buffer on the way full, each host's flush
  waited on the other's read, and the two went in step, a record each a
  turn -- one message at a time was faster than four. A turn now reads on
  while the session holds more of what came (`Session::buffered`), up to a
  receive buffer's worth.

net-fuzz's tcp target holds it: a peer that takes SACK or not, says four
blocks, fewer or none, and reneges now and then; every SACK block of the
machine's of what reached it, every D-SACK of what reached it twice, and
nothing sent again that an acknowledgement the machine had taken in said
was held; every table audited after every step. What it found, of the
kernel's: a block reaching SND.UNA, cut to it, made the oldest segment
look held and the next acknowledgement a reneging; the timeout's undo read
a copy filling a hole past what a peer that sends no blocks held as
originals arriving late, and undid every timeout's recovery; Karn above;
and the receive queue dropping a burst's tail with no count (now `rxfull:`
in `net`). The rest were the model's own: an acknowledgement is taken in
when the machine's pass after its arrival has run, not at its arrival; one
past RCV.NXT is outside the window; a later segment can overtake it. It
also holds the receiver to its window: the furthest edge offered within the
receive buffer at every audit, and no byte inside a window offered ever
finding no room (`overrun=`, kept by the kernel and held to 0) -- the old
edge rounded up, put back as a control, fails it at the second input; and
an edge that falls back does so by less than a unit, which 55% of its
inputs see. A campaign of it then found what the window's change had
brought: in TIME-WAIT, whose bulk buffer goes back, the acknowledgement of
a FIN sent again offered the slot's own 8 KiB in place of the 512 KiB
before, once nothing held the old edge; there the window now stays the
last one offered.

Measured on arm64 under HVF, MB/s each way, two runs a scenario of 8 MB
echoed with four 1 MB messages in flight, before T4 (c5b6f405) and with it:

| scenario | before | T4 |
|---|---|---|
| clean | 10.46, 10.01 | 15.45, 15.65 |
| 1% loss | 2.32, 3.82 | 8.16, 8.45 |
| 5% loss | 0.35, 0.41 | 5.71, 5.44 |
| 10 ms delay | 1.02, 6.35 | 2.65, 6.53 |
| 4 ms reordering | 0.74, 0.74 | 1.90, 4.96 |
| 5% loss and reordering | 0.16, 0.17 | 0.43, 0.55 |

A run of 24 MB clean: 10.0 before, 19.3 with T4. The clean gain is
ipcnet's drain: the tree before with that alone does 18.5-18.8 (24 MB) and
15.7-16.0 (8 MB), so on a clean link T4's TCP is even with the old. The
first run after the links come up is the slower with delay and reordering:
slow start under 10 ms, and RACK's reordering window learnt from D-SACKs.
The frames a clean 24 MB run cost a direction: 36.9k before, 39.6k with T4
before silly window avoidance, 35.3k after.

What is left:
- **Pacing.** Asked for to take off the clean LAN's loss, which turned out
  to be the three things above, none of them bursts: hub.py drops nothing
  (`send_failed` stays empty), a clean run retransmits nothing, and its
  round trip stays at 0.3 to 0.6 ms under load -- no queue for pacing to
  keep short. A bottleneck with a shallow queue between the boxes may want
  it; that is a measurement on the AX41 and the EX44 first.
- **F-RTO's other half**: after a timeout the copies slow start sends can
  outrun the originals' acknowledgements, so the test above rarely sees a
  pause for what it is, and the D-SACKs undo it a round trip later -- RFC
  5682's step 2, new data and not copies after the first acknowledgement,
  would see it at once.
- The numbers between the boxes, with the rest of I4's ([ipc.md](ipc.md)).

## What does not change

- The 64 slots: `ipcnet` takes two a peer, and the rest of the kernel's
  users are few.
- Keepalives: `ipcnet` has heartbeats of its own, and sshd its own
  timeouts.
- IPv4 alone.

## Tested

- **net-fuzz's `tcp` target**, extended:
  - the RTT estimate held to RFC 6298's arithmetic, done again by a model;
  - every retransmission at its deadline, and none later than the RTO plus
    a millisecond;
  - a connection given up at 60 s, not before;
  - bulk buffers both ways on an active open;
  - a stranger's SYN never taking a slot or getting an answer;
  - the reset limiter never letting more through than its rate.
- `tcp-test.py` (arm64), `sshd-test.py` (the backlog, and the ffi digest
  that T1 changes), `api-test.py`, and `hv-linux-test.py --net` (`hv forward`
  runs on bulk listeners).
- **Numbers**, on the AX41 and the EX44, each change against the tree before
  it: throughput, latency, the stall after a loss, spurious retransmissions.

## Steps

T1, T2 and T3 in that order, each a round of its own with its gates. They
are I3 of [ipc.md](ipc.md), together with `tls`'s pinned client, and come
before `ipcnet` carries anything. T4 was to wait for NBS's numbers
([nbs.md](nbs.md), N11), and came first, asked for on its own.

## Limits

- The floor is a guess until it is measured. Too low, and a peer's pause --
  a long softirq, a CPU held by a guest -- becomes spurious retransmissions.
- Go-back-N stays for a peer that does not take SACK: on a lossy path, a
  big window costs its whole self again for one lost segment.
- One kernel timer still serves every connection, armed for the earliest of
  them. That is cheap for 64 connections, and would want a wheel for
  thousands.
