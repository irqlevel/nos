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
let answer = mb.call(lookup("kernel.echo").unwrap(), 7, b"ping", Duration::from_secs(5))?;
```

A `call` monitors what it calls, so it ends with the answer, a `Down` or its
timeout -- never by waiting for a peer that is gone.

From the shell:

```
ipc                     the host's counters
ipc names [prefix]      every name: its mailbox, scope and owner
ipc boxes               every mailbox: owner, name, what it holds and is kept
ipc echo [start|stop]   kernel.echo, which answers what it is sent with what it was sent
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
  `Down` -- the host's queues empty after.
- **`scripts/ipc-fuzz.py`** (host, in CI): the crate as it is over the
  fuzzers' common machine, against a model of its own -- every answer, every
  queue in order, every count, the `Down`s and the names; mailboxes of the
  image's and of made-up modules, receivers asleep until a message, a wake or
  a close, modules unloaded with mailboxes open, the halt. Ten mutants of the
  crate -- a `Down` lost, a byte count forgotten, a namespace not held, a
  generation not bumped, among them -- each fail its gate.
- `cpp-fuzz.py --target module` holds `ModuleTable::name_at` -- what the
  channel knows a module by -- to the module's name inside its image and
  nothing outside.

## Between hosts: the wire

`ipcwire` (`src/crates/ipcwire`, no kernel and no `unsafe`) is what ipcnet
is to say to another host inside their TLS session (plans/ipc.md, I4; the
module itself is not written yet): frames of an 8-byte head -- kind, flags,
two bytes kept at zero, a length -- and at most 16 KiB, a TLS record's
worth. `Hello` first, both ways (the version, the cluster's id, both
hosts' names); `Ping` and `Pong`; a message as a `Head` (its number on the
connection, where it goes -- an address or a name -- who sent it, its tag,
reference and length, up to 1 MiB) and `Body` frames, other messages'
frames between them; `Bounced`, `Monitor`, `Demonitor`, `Down`. `decode`
gives a frame once all of it has come, every length checked first -- a head
claiming more than a frame is refused at once, not waited on -- and a frame
it gives encodes to exactly the bytes it came from. `Assembler` puts
messages back together, bounded in how many are open and in the bytes their
heads promised, each body kept in pieces of a frame: no allocation larger
than a frame, whatever a message's size. `encode` grows its buffer by
exactly the frame. Its gate is `ipc-fuzz.py --target wire`.

## Limits

- This host only: a message to another host's address is `NoRoute` until
  `ipcnet` (plans/ipc.md, I3 and I4).
- Copies: a payload is copied in and out.
- One receiver a mailbox: a second task waiting on the same one is refused
  and said.
- Every send takes the table's lock once to find its mailbox: one lock for
  the whole host, held for a lookup.
