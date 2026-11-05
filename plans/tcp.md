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

**T2, a timeout from the round trip.** Its first half is done on branch
`ipc-raft`: the estimate, a floor of 200 ms and giving up by time, with the
200 ms look kept; the timer armed for the earliest deadline (a task that
sleeps until it), the lower floor for links between nos hosts and the
measurements are still to do.
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
  measured there. A path of 50 ms, the Mac to Hetzner, gets an RTO of a few
  hundred ms from its own measurements either way.
- The timer is armed for the earliest deadline any connection has, to the
  millisecond, instead of a look every 200 ms. With nothing in flight
  anywhere, it does not run at all: no wakeups on an idle machine.
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

**T3, strangers kept off before they cost anything.**
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

**T4, only if the numbers ask for it.**
- SACK (RFC 2018, with RFC 6675's recovery), if retransmitted bytes on
  Hetzner's network turn out to matter. Go-back-N sends a whole window
  again after a loss: up to 512 KiB for one lost segment.
- RACK-TLP (RFC 8985), if tail losses still cost after T2: a lost last
  segment recovered in two round trips without waiting for the timeout.

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
before `ipcnet` carries anything. T4 only after NBS's numbers ([nbs.md](nbs.md),
N11).

## Limits

- The floor is a guess until it is measured. Too low, and a peer's pause --
  a long softirq, a CPU held by a guest -- becomes spurious retransmissions.
- Go-back-N stays until T4: on a lossy path, a big window costs its whole
  self again for one lost segment.
- One kernel timer still serves every connection, armed for the earliest of
  them. That is cheap for 64 connections, and would want a wheel for
  thousands.
