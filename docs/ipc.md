# The message channel

Modules cannot call one another: each is linked on its own against the
kernel's export table ([Loadable modules](modules.md)). The channel is where
services meet anyway: **mailboxes** with addresses, **names** for them in a
namespace public to the host, **messages** between them, **monitors** that
say when a peer is gone and **watches** that say when a name comes and goes.
The image's tasks reach it through the `ipc` crate (`src/crates/ipc`), a
module through `kcore::ipc` over the C ABI (`ffi/src/ipc.rs`). It is the
first step of [plans/ipc.md](../plans/ipc.md): this host only, for now; the
module that carries messages between hosts, `ipcnet`, comes next.

## What it is

- A **mailbox** is a bounded queue with one owner, who receives from it --
  one task at a time. Its **address** is its slot in the table and the slot's
  generation: an address is looked up on every call, never followed, so a
  mailbox that has closed is `Gone`, and its slot's next mailbox has another
  address. A mailbox is closed when its owner drops it.
- A **name** is one mailbox's, unique on the host: parts joined by dots, each
  a lowercase letter then `a-z 0-9 _ -`, 48 bytes in all. It goes when its
  mailbox does. Its first part is its **namespace**: a module registers names
  under its own -- its module name up to the first `-`, so `nbs-mds` and
  `nbs-cs` both under `nbs` -- and `kernel`, `ipc` and `sys` are the image's.
  A name's scope is `Host`, or `Cluster` once `ipcnet` carries messages
  between hosts.
- A **message** is who sent it, a tag the protocol defines (tags from
  `0xFFFF0000` up are the channel's own), a reference that says which request
  a reply answers, and up to 1 MiB of payload, held in pieces of a page: no
  allocation of the channel's is larger than a page. Copied in at a send and
  out at a receive.
- A **monitor** says once, with a `Down`, when the mailbox it watches goes:
  closed by its owner, closed because its module was unloaded, or gone
  already when the monitor was made. Its `Down` is made when the monitor is,
  and a place is kept for it in the watcher's queue, so a mailbox closing
  allocates nothing and finds room for every `Down` it owes -- even in a
  mailbox full of other messages.
- A **watch** says `NameUp` and `NameDown` for every name under a prefix that
  comes and goes. A watcher with no room for one is told, once, that its
  watch has ended (`TAG_WATCH_LOST`): watch again, and list what there is.

## Using it

In the image:

```rust
let mb = ipc::Mailbox::new(ipc::DEFAULT_LIMITS)?;
mb.register("kernel.thing", ipc::Scope::Host)?;
while let Some(msg) = mb.recv(Duration::from_secs(1)) {
    mb.reply(&msg, TAG_DONE, answer)?;          // to msg.from(), with its reference
}
let reply = ipc::call(&mine, ipc::To::Name("kernel.echo"), 7, payload, Duration::from_secs(5))?;
```

In a module (`kcore::ipc`):

```rust
let mb = Mailbox::new(Limits::DEFAULT)?;        // owned by this module: the kernel knows it by its code's address
mb.register("nbs.mds", Scope::Cluster)?;
let msg = mb.recv(Duration::from_secs(1));      // Message { from, tag, reference, data }
let big = mb.recv_pieces(Duration::from_secs(1), PIECE_MAX);   // Pieces { .., len, pieces: Vec<Vec<u8>> }
let answer = mb.call(lookup("kernel.echo").unwrap(), 7, b"ping", Duration::from_secs(5))?;
```

A `call` monitors what it calls, so it ends with the answer, a `Down` or its
timeout -- never by waiting for a peer that is gone.

A module's heap gives at most 512 KiB in one block, and a message may be a
megabyte: `recv` copies a message into one `Vec`, and leaves first one it
has no room for; `recv_pieces` takes any message, in pieces of at most 64
KiB, through `kernel_ipc_recv_part` -- a part of the first message at an
offset, a ticket from its first part held to at every part after so that
all of them are of one message, the message taken with its last byte. What
the router hands on from a peer (`deliver_remote`) goes in pieces too. A
module's `send` is still of one buffer: a module sends at most what its heap
holds in one block.

## One event to wait on

A task that waits for several things -- a mailbox's requests and a device's
completions, say: NBS's chunk server -- sleeps on one event they all signal
(plans/ipc.md, I2). The event is the task's; a mailbox, a device's queue and
an image's queue are made on it, and whatever comes to one of them signals
it:

```rust
let event = events::SharedEvent::new()?;                      // in the image: an Arc
let mb = ipc::Mailbox::new_on(ipc::DEFAULT_LIMITS, &event)?;
let q = block::queue::Queue::new(Some(disk), 8, 64, Some(event.clone()))?;

// in a module (kcore::events::SharedEvent, a handle the kernel looks up)
let event = SharedEvent::new().ok_or(Error::NoMemory)?;
let mb = Mailbox::new_on(Limits::DEFAULT, &event)?;
let q = disk.into_queue_on(&event, true, "nbs-cs", 8, 1 << 20)?;  // Image::into_queue_on too
loop {
    while let Some(msg) = mb.try_recv() { /* a request */ }
    q.reap(|buffer, ok| { /* a completion */ });
    event.wait_for(Duration::from_secs(1));
}
```

- **Counted, so never signalled once freed.** The event is held by its task
  and by every object made on it -- an `Arc` in the image; for a module, a
  handle of the `events` crate's table, which holds its own count until the
  handle is closed -- and freed with the last of them, whoever that is: the
  task may let go first, and the objects go on signalling an event that is
  still there. A queue's completions come in its device's interrupt, through
  an entry that lives for good and may be late: the entry keeps the task's
  event in a lock of its own, interrupt-safe, a completion signals it under
  that lock and the queue's close takes it out under it, so once the close
  has it no completion is still in its signal; it is let go of -- perhaps
  freed -- after, off the lock, in task context.
- **One waiter.** A wait holds the event (`SharedEvent::hold`) for as long
  as it sleeps: a second task waiting on it, or waiting in a receive from a
  mailbox made on it, is refused and said. A receive that finds its message,
  or does not wait, holds nothing -- so a task may look at its mailbox while
  another sleeps on the event.
- **Look first, then wait.** A queue made on an event signals both its own
  and the task's; its `wait`, `poll` and the waits inside its calls (a drain,
  its close) stay on its own. A mailbox's receive on a task's event ends at
  the event's first signal, with one more look: what woke it may have been
  another of the task's things. So a task on one event looks at every
  source before each wait -- a signal one of them took is never a wakeup
  lost.
- A handle that is no event's -- closed, or never one -- is refused by
  everything that takes one: a mailbox's, a queue's open, a wait.

From the shell:

```
ipc                     the host's counters
ipc names [prefix]      every name: its mailbox, scope and owner
ipc boxes               every mailbox: owner, name, what it holds and is kept
ipc echo [start|stop]   kernel.echo, which answers what it is sent with what it was sent
ipc ping <to> [count=N] [bytes=B] [window=W] [bulk] [timeout=MS]
                        messages to a name, here or name@host, each answer held to what was sent
```

## What keeps it sound

- **Pull, never push.** Nothing in the channel calls anybody's code: a
  receiver's own task takes its messages. No message lands in a module that
  has gone, and a module's unload is its mailboxes' drops.
- **What a module leaves open is closed, loudly.** After a module's exit the
  loader tells the channel (`rust_ipc_module_gone`), which closes every
  mailbox that module still had and says each in the log as the module's
  bug -- its monitors told, its names gone.
- **Bounded**: a mailbox's messages and bytes, by its owner's limits (4096
  messages and 16 MiB at most); one module's mailboxes, by their limits'
  bytes summed (64 MiB); the host's queues at once (128 MiB); 1024 mailboxes,
  256 names, 512 monitors a mailbox, 16 watches a mailbox and 256 in all.
  `Full` and `NoMemory` are answers, and counted.
- **Locks**: the table's, then one mailbox's -- never the other way round,
  never two mailboxes' at once -- both mutexes, task context only. What a
  closing mailbox held is dropped after every lock is let go of. No send from
  interrupt context.
- **The halt** (`rust_ipc_stop`, after the modules): nothing is made or sent
  from then on, and a module's mailbox still open is said.

## Tested

- **At boot**, every boot, both architectures: `ipc::selftest` in
  `rust_test` (messages of every size in order, every byte as sent; a full
  mailbox, a payload too large; an address gone and never another mailbox's;
  names taken, refused, looked up and held to a module's namespace; a
  `Down` when a mailbox goes, at once for one gone already, none once
  dropped, and one into a full mailbox; a watch's names coming and going),
  then `test_module_ipc`: kernel.echo started, modtest's `modtest ipc`
  calling it with payloads up to 200 KB, its names, a `Down`, a full mailbox;
  one mailbox left open on purpose, which the sweep after the unload must
  close; and the unload done under the echo's traffic, which must see its
  `Down` -- the host's queues empty after. And one event to wait on:
  `one_event::selftest` in `rust_test` (a mailbox and a block queue made on
  one event, a task asleep on it woken by a message, by a completion made
  in another CPU's interrupt through the queue's own completion path, and
  by a wake; a second waiter refused; the task and the event's owner
  letting go first, the two keeping it; the event freed with the last of
  them), and modtest's side of it first in `modtest ipc` (a mailbox and a
  device's queue on a module's event, through the C ABI).
- **`scripts/ipc-fuzz.py`** (host, in CI): the crate as it is over the
  fuzzers' common machine, against a model of its own -- every answer, every
  queue in order, every count, the `Down`s and the names; mailboxes of the
  image's and of made-up modules, receivers asleep until a message, a wake or
  a close, modules unloaded with mailboxes open, the halt. Ten mutants of the
  crate -- a `Down` lost, a byte count forgotten, a namespace not held, a
  generation not bumped, among them -- each fail its gate. And events:
  mailboxes made on the image's and on modules' (by handle, live and stale),
  a task asleep on one woken by a message to a mailbox on it, a wake, a
  close, a signal, and its holder letting go first -- by nothing else -- a
  second waiter refused at once, and every event there exactly while its
  holder or a mailbox on it is (`events::live` held to the model).
- `cpp-fuzz.py --target module` holds `ModuleTable::name_at` -- what the
  channel knows a module by -- to the module's name inside its image and
  nothing outside.

## Between hosts: the router

What the channel does of another host's, built before what carries it
(ipcnet, plans/ipc.md I4): one mailbox may be the **router** (`route`), and
the channel hands it whatever is for a peer -- the router carries it, and
hands back what peers say.

- **A peer's number** is this host's own: the router says a peer is up
  (`peer_up`) and is given its number, the host part of the addresses of
  its mailboxes here -- an index in the low six bits, an epoch above. Lost
  (`peer_lost`) and up again, it has the next epoch: an address held from
  before the loss is `NoRoute`, never another mailbox's. On the wire an
  address is always the owning host's, and the router rewrites it.
- **To a peer**: a send to a peer's address, or to `name@host`, goes into
  the router's mailbox -- its limits the host's cap on what waits to go out,
  `Full` at the send -- as a message tagged `TAG_ROUTE` whose payload begins
  with ipcwire's route head (where it goes, its tag and reference), its own
  bytes after. The sender is kept, first, as a mailbox the peer may answer.
- **From a peer** (`deliver_remote`, the router's call): delivered to a
  `Cluster` name, or to a mailbox that has sent to that peer -- the newest
  256 a peer, forgotten when the mailbox closes or the peer is lost -- and
  refused (`NotYours`, counted) for anything else: a `Host` name is
  unreachable from outside. Its `from` is given the peer's number.
- **Monitors of a peer's mailbox** are kept here, on the watcher's host, as
  every monitor is, their Downs made and their places kept; the router is
  asked to have the peer watch (`TAG_ROUTE` again), and its `remote_down`
  hands the Down over. A peer lost ends them all with `HostLost`, needing
  nothing from the host that is gone; a router's close loses every peer.
- **A message that did not get there** is told back to its sender
  (`bounced`, the router's call): its mailbox there full, gone or no
  mailbox of its name (`Full`, `Gone`), or never sent -- the way to that host
  full, or the host lost before it went (`Unsent`). It comes as a message of
  the channel's own, `TAG_BOUNCED`: from the mailbox it was for as this host
  knows it (the peer's alone, slot 0, for a name there), the reference the
  message's, the payload why, its tag and, for a name, the name
  (`Message::bounced` in kcore reads it). Only to a mailbox that has sent to
  that peer, as only one may be answered from there; and dropped, and
  counted, when the sender has no room: a bounce keeps no place, as a Down
  does. A send to another host is `Ok` once it is in the router's mailbox,
  so a bounce is the only word there is of one that went nowhere.
- **Two classes**: a tag with the bit `TAG_BULK` (0x4000_0000) goes between
  hosts by the bulk connection, any other by the control one, so that a
  megabyte of data never stands in front of a heartbeat. Order between two
  mailboxes holds within a class, not across them -- nor between a bulk
  message and the Down of its sender, which comes by the control one.
- **Hosts coming and going** are told to the watchers of `host.`
  (`NameUp`/`NameDown` of `host.<name>`, from the peer's number); nobody may
  name a mailbox under `host.`. A router's own close loses every peer, and
  is told to every watcher but itself: its watches and remote monitors go
  first, so the task waiting on it finds it closed, not a NameDown of its
  own making.
- **Which host sent it**: a message from another host has that peer's
  number in its `from`, and `kcore::ipc::peer_name(from)` (the C ABI's
  `kernel_ipc_peer_name`) says the peer's name -- the one the router came up
  with it under, so with ipcnet the name the key its connection showed is
  pinned under. It is what a service takes as the word of who is asking: the
  cluster's member takes Raft's messages only from the host whose node they
  claim, and a join only from the host invited (docs/cluster.md). None for
  this host's own mailboxes and for a number whose peer is gone.
- `ipc peers` lists the router and the peers; `ipc` counts what went to
  peers and came from them, and what was refused.

The fuzz model holds all of it to its own -- routed heads byte for byte,
the answer rule, the epochs, HostLost in its kept place, a router's close,
every bounce and every refusal of one -- and sixteen mutants of it each
fail the gate.

## Between hosts: the wire

`ipcwire` (`src/crates/ipcwire`, no kernel and no `unsafe`) is what ipcnet
says to another host inside their TLS session (plans/ipc.md, I4): frames of
an 8-byte head -- kind, flags, two bytes kept at zero, a length -- and at
most 16 KiB, a TLS record's worth. `Hello` first, both ways (the version,
the cluster's id, the session -- the dialing host's number for the pair of
connections, which the other pairs them by -- both hosts' names); `Ping`
and `Pong`; a message as a `Head` (its number on the connection, where it
goes -- an address or a name -- who sent it, its tag, reference and length,
up to 1 MiB) and `Body` frames, other messages' frames between them;
`Bounced`, `Monitor`, `Demonitor`, `Down`. `decode` gives a frame once all
of it has come, every length checked first -- a head claiming more than a
frame is refused at once, not waited on -- and a frame it gives encodes to
exactly the bytes it came from. `Assembler` puts messages back together,
bounded in how many are open, in the bytes their heads promised, and in the
pieces -- a body each -- they are kept in, so that a sender of a byte a
frame is refused before its pieces cost more than their bytes: no
allocation larger than a frame, whatever a message's size. `encode` grows
its buffer by exactly the frame. Its gate is `ipc-fuzz.py --target wire`.

## Between hosts: ipcnet

`ipcnet.ko` (`src/crates/modules/ipcnet`, docs/modules.md has its commands)
is the router: what carries the channel's messages to the cluster's other
hosts and back.

- **Its hosts** are in /etc/ipc/hosts.toml: this one's name and the
  cluster's id, and every peer's name, address and key -- one, or two while
  it moves to a new one -- pinned. The host's own key and certificate are
  made at its first start, in one file, /etc/ipc/tls/identity.pem (a pair
  openssl made, key.pem and cert.pem, is read where there is none). A peer
  is its name and its address: its keys change in place, a connection
  keeping the key it was made with and the next made with a key it has
  then. A peer's line may end
  `only=<name>`: a host invited to the cluster, which reaches that one
  `Cluster` name of this host's, by the name, and nothing else -- no
  mailbox by its address, no monitor; what it sends anywhere else is
  dropped and counted as refused. A new list that keeps the peer's name,
  address and keys changes that in place, its connections kept.
- **Its control mailbox**, `ipcnet.ctl` (`ipcwire::control`), a `Host`
  name, which nothing from another host reaches: this host's modules ask it
  who this host is -- its name, its cluster, its key, the interface and its
  address, serving or not -- and the cluster's registry tells it the peers
  this host is to have, in place of those it has, and a cluster to join
  while it has none. It writes hosts.toml as `ipcnet add` and `remove` do (a
  new file, synced, renamed over the old) and gives the running server the
  list: one writer of the file. And it stages a new key for this host and
  switches to it when asked (`TAG_KEY_STAGE`, `TAG_KEY_USE`: `cluster
  rekey`), the server and every peer's clients made anew with it and the
  connections up kept. It answers while the module is loaded, serving or
  not (docs/cluster.md).
- **Two TCP connections a peer**, control and bulk, on one port (39240), on
  the interface hosts.toml names (eth0 unless told; never hv0, so that no
  guest reaches it). The listener answers the peers' addresses alone: to
  anyone else the port is not there, no SYN-ACK and no reset, and no slot
  taken (TCP's allowlist, plans/tcp.md T3). At most two handshakes at once,
  one a source, each given 3 s. Each connection has a link's 20 ms floor
  for its retransmissions (T2).
- **TLS 1.3, both keys pinned**: the dialer is a `tls::pinned::Client` of
  the peer's keys, the listener a `Server` of every peer's, ChaCha20-
  Poly1305 asked for first (measured: docs/tls.md). The connection's class is
  its ALPN, `nos-ipc/1` or `nos-ipc-bulk/1`. Then the `Hello` each way: the
  cluster's id, the session, both names -- the name the dialer says has to
  be the one its key is pinned under, its address that peer's.
- **Who dials**: of two names, the one that sorts first, a session at a
  time: the control connection, and once it has said its Hello the bulk one;
  the other end pairs them by the session's number, a new session ending its
  old. The peer is up -- the channel's `peer_up` -- once both have said
  their Hello, and lost the moment either ends, a frame the wire refuses
  comes, or nothing is heard for 3 s (a `Ping` goes every 500 ms each way):
  what was queued for it and never went is bounced (`Unsent`), the channel
  told (HostLost to every monitor of an address there), and the monitors it
  had this host hold taken back. The dialer tries again after a wait that
  doubles from 250 ms to 8 s while no session comes up.
- **Out**: the router's task takes each message from its mailbox into the
  peer's queue of its class, at most a megabyte (control) or four
  (bulk) waiting -- past that, bounced. A connection's task sends small
  frames first, then messages a frame at a time, up to eight begun at once
  and in turn -- but never two of one pair, sender and destination, since
  the other end hands a message over as its last frame comes and a pair is
  promised its order. In: frames decoded as they come, a message handed to
  the channel the moment it is whole (`deliver_remote`), a full mailbox a
  `Bounced` back, never a connection that stops reading; what a peer may not
  reach here is dropped and counted.
- **One wait for both ways**: a connection's task sleeps in its socket's
  receive, which TCP wakes the moment data comes -- its receive waits on an
  event of the connection's, which the receive path signals -- and which
  whoever queues something for the connection ends early (`TcpWaker`, held
  to the connection's generation, so that a wake of one gone reaches
  nobody). A small message's round trip between two hosts is some 0.4 ms at
  the median on `scripts/hub.py`'s LAN under HVF, where it was 7 ms while
  the task polled; what is left of polling is a send waiting for room in
  TCP's buffer, a millisecond at a time.

`ipcnet` says each peer's state -- up and as which number, or down and why
-- and what went between them; `ipc ping kernel.echo@<host>` sends to a
peer's echo. Its gates are net-fuzz's `ipcnet` target (the module's source
on the host against a world that is the other host, docs/testing.md) and
`scripts/ipc-test.py` (both arches), three nos on
`scripts/hub.py`'s LAN: keys pinned both ways, pings small and of a
megabyte held to every byte and to their order, bounces of a name nobody
has and of a full mailbox, the wire lossy and late, a cut seen within 3 s
on both sides and its monitors told, a host with the wrong key never let
in, a stranger's SYNs dropped, and `rmmod ipcnet` under traffic.

## Limits

- Between hosts, a message lost with its connection -- sent, or part way
  -- is lost: a monitor's HostLost is the word of it, and a bounce
  (`Unsent`) only of what had not begun to go.
- Copies: a payload is copied in and out.
- A module sends a message of one buffer, so of at most what one block of
  its heap holds (512 KiB): a send in pieces comes when a module needs one
  (NBS's data).
- One receiver a mailbox: a second task waiting on the same one -- or on the
  event it was made on -- is refused and said.
- A module's events are not swept at its unload, as its mailboxes are: one
  it leaves open keeps its slot of the table, 256 of them, until the reboot.
- Every send takes the table's lock once to find its mailbox: one lock for
  the whole host, held for a lookup.
