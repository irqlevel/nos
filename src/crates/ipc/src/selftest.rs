//! The channel held to what it says, at boot, by one task: messages of every
//! size in order and each byte as sent; a full mailbox, a payload too large,
//! an address gone and never another mailbox's; names taken, refused and
//! looked up, a module's held to its namespace; a monitor's Down when its
//! mailbox goes, at once for one gone already, none once dropped; a watch's
//! names coming and going; and the host's queues back to what they held. A
//! failure panics, as the boot's selftests do. What takes a second task --
//! a call, a module's side, an unload under traffic -- is the boot's
//! test_module_ipc, through kernel.echo.

use crate::{counters, list, lookup, open, peers, register, Addr, Bounce, Error, Limits, Mailbox, Owner, Payload, Reason,
            Scope, To, BOUNCE_BYTES, MAX_PAYLOAD, PIECE, TAG_BOUNCED, TAG_DOWN, TAG_NAME_DOWN, TAG_NAME_UP, TAG_ROUTE,
            TAG_SYSTEM};
use alloc::vec::Vec;
use kcore::klog;

const SMALL: Limits = Limits { messages: 2, bytes: 64 };
const ROOMY: Limits = Limits { messages: 16, bytes: 4 << 20 };

fn check(ok: bool, what: &str) {
    if !ok {
        panic!("ipc selftest: {}", what);
    }
}

/// `n` bytes of a pattern of `seed`'s.
fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
}

pub fn selftest() {
    let before = counters().queued_bytes;
    order_and_sizes();
    limits();
    addresses();
    names();
    monitors();
    watches();
    router();
    check(counters().queued_bytes == before, "the host's queues hold more than before the test");
    klog!(0, "ipc: selftest passed");
}

fn order_and_sizes() {
    let a = Mailbox::new(ROOMY).unwrap_or_else(|e| panic!("ipc selftest: a mailbox: {}", e.name()));
    let sizes = [0, 1, PIECE - 1, PIECE, PIECE + 1, 3 * PIECE + 7, 300_000];
    for (i, &n) in sizes.iter().enumerate() {
        let p = Payload::from_slice(&pattern(n, i as u8)).unwrap_or_else(|e| panic!("ipc selftest: a payload: {}", e.name()));
        check(p.len() == n, "a payload's length");
        check(a.send(To::Addr(a.addr()), 7, i as u64, p).is_ok(), "a message to itself");
    }
    for (i, &n) in sizes.iter().enumerate() {
        let m = a.try_recv().unwrap_or_else(|| panic!("ipc selftest: message {} never came", i));
        check(m.reference() == i as u64 && m.tag() == 7 && m.from() == a.addr(), "a message out of order");
        check(m.payload().equals(&pattern(n, i as u8)), "a message's bytes");
        let mut tail = [0u8; 5];
        let got = m.payload().read(n.saturating_sub(3), &mut tail);
        check(got == n.min(3), "a read past a payload's end");
    }
    check(a.try_recv().is_none(), "a message no one sent");
    /* Past the most a payload holds, refused by its length before anything
     * is allocated -- a megabyte at once is past what the heap gives */
    let page = [0u8; PIECE];
    let parts = [&page[..]; MAX_PAYLOAD / PIECE + 1];
    check(Payload::gather(&parts).err() == Some(Error::TooLarge), "a payload past the most");
    let parts = [&b"ab"[..], &b""[..], &b"cde"[..]];
    check(Payload::gather(&parts).is_ok_and(|p| p.equals(b"abcde")), "a gathered payload");
}

fn limits() {
    let a = Mailbox::new(SMALL).unwrap_or_else(|e| panic!("ipc selftest: a small mailbox: {}", e.name()));
    let to = To::Addr(a.addr());
    check(a.send(to, 1, 0, Payload::empty()).is_ok() && a.send(to, 1, 1, Payload::empty()).is_ok(), "two messages");
    check(a.send(to, 1, 2, Payload::empty()).err().map(|e| e.0) == Some(Error::Full), "a third, past the limit");
    let big = Payload::from_slice(&[0u8; 65]).unwrap_or_else(|_| panic!("ipc selftest: 65 bytes"));
    check(a.send(to, 1, 3, big).err().map(|e| e.0) == Some(Error::TooLarge), "past what the mailbox holds at all");
    check(a.send(to, TAG_SYSTEM, 4, Payload::empty()).err().map(|e| e.0) == Some(Error::Invalid), "a system tag");
    check(Mailbox::new(Limits { messages: 0, bytes: 1 }).err() == Some(Error::Invalid), "no room at all");
    while a.try_recv().is_some() {}
    check(a.send(to, 1, 5, Payload::empty()).is_ok(), "room again once taken");
}

fn addresses() {
    let a = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a mailbox"));
    let b = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a second mailbox"));
    let gone = b.addr();
    drop(b);
    check(a.send(To::Addr(gone), 1, 0, Payload::empty()).err().map(|e| e.0) == Some(Error::Gone), "to one closed");
    let c = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a third mailbox"));
    check(c.addr() != gone, "a closed mailbox's address given again");
    check(a.send(To::Addr(gone), 1, 0, Payload::empty()).err().map(|e| e.0) == Some(Error::Gone),
          "the old address reaching the new mailbox");
    check(a.send(To::Addr(Addr { host: 3, slot: 1, generation: 1 }), 1, 0, Payload::empty()).err().map(|e| e.0)
          == Some(Error::NoRoute), "another host's, with nothing between hosts");
    check(Addr::from_u64(c.addr().to_u64()) == c.addr(), "an address through the ABI's word");
}

fn names() {
    let a = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a mailbox"));
    let b = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a mailbox"));
    check(a.register("kernel.selftest.a", Scope::Host).is_ok(), "a name");
    check(lookup("kernel.selftest.a") == Some(a.addr()), "a name looked up");
    check(b.register("kernel.selftest.a", Scope::Host) == Err(Error::Taken), "a name taken");
    check(a.register("kernel.selftest.b", Scope::Host) == Err(Error::Taken), "a second name");
    for bad in ["", "Kernel.x", "kernel..x", "kernel.x.", "9.x", "kernel x", "kernel.x/y"] {
        check(b.register(bad, Scope::Host) == Err(Error::Invalid), "a name that is none");
    }
    check(a.send(To::Name("kernel.selftest.a"), 9, 1, Payload::empty()).is_ok(), "a message by name");
    check(a.try_recv().is_some_and(|m| m.tag() == 9), "a message by name received");
    check(list("kernel.selftest.").len() == 1, "the names under a prefix");
    /* A module's names: under its namespace, never the image's */
    let owner = Owner::module(b"nbs-test").unwrap_or_else(|| panic!("ipc selftest: an owner"));
    let m = open(owner, ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a module's mailbox"));
    check(register(&m, "kernel.nbs", Scope::Host) == Err(Error::NotYours), "a module under the kernel's namespace");
    check(register(&m, "other.x", Scope::Host) == Err(Error::NotYours), "a module under another's");
    check(register(&m, "nbs.test", Scope::Cluster) == Ok(()), "a module under its own");
    crate::close(&m, Reason::Closed);
    check(lookup("nbs.test").is_none(), "a name that outlived its mailbox");
    drop(a);
    check(lookup("kernel.selftest.a").is_none(), "a name that outlived its mailbox");
}

fn monitors() {
    let w = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a watcher"));
    let t = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a target"));
    let target = t.addr();
    let id = w.monitor(target).unwrap_or_else(|e| panic!("ipc selftest: a monitor: {}", e.name()));
    check(w.try_recv().is_none(), "a Down before its mailbox went");
    drop(t);
    let down = w.try_recv().unwrap_or_else(|| panic!("ipc selftest: no Down"));
    check(down.tag() == TAG_DOWN && down.reference() == id && down.from() == target, "the Down's who and which");
    check(down.down() == Some(Reason::Closed), "the Down's reason");
    /* Of one gone already: at once */
    let id = w.monitor(target).unwrap_or_else(|_| panic!("ipc selftest: a monitor of one gone"));
    check(w.try_recv().is_some_and(|m| m.reference() == id && m.down() == Some(Reason::NoSuch)), "a Down at once");
    /* Dropped: none */
    let t = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a target"));
    let id = w.monitor(t.addr()).unwrap_or_else(|_| panic!("ipc selftest: a monitor"));
    w.demonitor(id);
    drop(t);
    check(w.try_recv().is_none(), "a Down for a monitor dropped");
    /* A Down finds room in a full mailbox: the place kept for it */
    let small = Mailbox::new(SMALL).unwrap_or_else(|_| panic!("ipc selftest: a small watcher"));
    let t = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a target"));
    let id = small.monitor(t.addr()).unwrap_or_else(|_| panic!("ipc selftest: a monitor"));
    let to = To::Addr(small.addr());
    check(small.send(to, 1, 0, Payload::empty()).is_ok() && small.send(to, 1, 1, Payload::empty()).is_ok(), "full");
    drop(t);
    let mut got = 0;
    let mut downs = 0;
    while let Some(m) = small.try_recv() {
        got += 1;
        if m.reference() == id && m.tag() == TAG_DOWN {
            downs += 1;
        }
    }
    check(got == 3 && downs == 1, "a Down lost to a full mailbox");
}

fn watches() {
    let w = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a watcher"));
    let id = w.watch("kernel.selftest.w").unwrap_or_else(|e| panic!("ipc selftest: a watch: {}", e.name()));
    let n = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a named mailbox"));
    check(n.register("kernel.selftest.w1", Scope::Host).is_ok(), "a watched name");
    let up = w.try_recv().unwrap_or_else(|| panic!("ipc selftest: no NameUp"));
    check(up.tag() == TAG_NAME_UP && up.reference() == id && up.from() == n.addr()
          && up.payload().equals(b"kernel.selftest.w1"), "the NameUp");
    let addr = n.addr();
    drop(n);
    let gone = w.try_recv().unwrap_or_else(|| panic!("ipc selftest: no NameDown"));
    check(gone.tag() == TAG_NAME_DOWN && gone.from() == addr && gone.payload().equals(b"kernel.selftest.w1"),
          "the NameDown");
    w.unwatch(id);
    let n = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a named mailbox"));
    check(n.register("kernel.selftest.w2", Scope::Host).is_ok(), "a name");
    check(w.try_recv().is_none(), "an event for a watch ended");
    check(w.watch("Bad prefix").err() == Some(Error::Invalid), "a prefix that is none");
}

/// The router's side, by the image's own mailboxes: a message for a peer's
/// address handed to the router with its head; an answer from the peer
/// delivered, from the peer's number, to a mailbox that sent there and to
/// no other; a Cluster name reached and a Host name not; a monitor of the
/// peer's mailbox asked of the router, ended by the peer's Down and by the
/// peer lost; the peer's old addresses refused once it is up again under a
/// new number; and the peers lost with the router's close. The peer stays
/// in the table, down, so that no address from before is ever another's.
fn router() {
    let router = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a router"));
    check(router.route().is_ok(), "a router refused");
    let peer = router.peer_up("selftest").unwrap_or_else(|e| panic!("ipc selftest: a peer up: {}", e.name()));
    check(router.peer_up("selftest") == Ok(peer), "a peer up twice, two numbers");
    let a = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a sender"));
    let there = Addr { host: peer, slot: 7, generation: 1 };
    check(a.send(To::Addr(there), 5, 77, Payload::from_slice(b"to the peer").unwrap_or_else(|_| panic!("ipc selftest")))
              .is_ok(), "a send to a peer refused");
    let m = router.try_recv().unwrap_or_else(|| panic!("ipc selftest: nothing at the router"));
    let mut bytes = [0u8; ipcwire::route::HEAD + 11];
    check(m.tag() == TAG_ROUTE && m.from() == a.addr() && m.payload().len() == bytes.len()
              && m.payload().read(0, &mut bytes) == bytes.len(), "the routed message's tag, sender or length");
    let head = ipcwire::route::decode(&bytes).unwrap_or_else(|_| panic!("ipc selftest: the route head"));
    check(head.kind == ipcwire::route::SEND && head.peer == peer && head.tag == 5 && head.reference == 77
              && head.to == ipcwire::To::Addr(Addr { host: 0, ..there }.to_u64())
              && &bytes[ipcwire::route::HEAD..] == b"to the peer", "the route head");
    /* The peer's answer: to a mailbox that sent there, from the peer's number */
    let from = Addr { host: 0, slot: 9, generation: 2 }.to_u64();
    check(router.deliver_remote(peer, from, To::Addr(a.addr()), 6, 77, Payload::empty()).is_ok(), "an answer refused");
    check(a.try_recv().is_some_and(|m| m.from() == Addr { host: peer, slot: 9, generation: 2 } && m.tag() == 6
                                       && m.reference() == 77), "the answer's sender or tag");
    let b = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a mailbox"));
    check(router.deliver_remote(peer, from, To::Addr(b.addr()), 6, 0, Payload::empty()) == Err(Error::NotYours),
          "a mailbox that never sent there reached from a peer");
    check(b.register("kernel.selftest.cluster", Scope::Cluster).is_ok(), "a Cluster name refused");
    check(router.deliver_remote(peer, from, To::Name("kernel.selftest.cluster"), 6, 0, Payload::empty()).is_ok()
              && b.try_recv().is_some(), "a Cluster name not reached from a peer");
    let c = Mailbox::new(ROOMY).unwrap_or_else(|_| panic!("ipc selftest: a mailbox"));
    check(c.register("kernel.selftest.host", Scope::Host).is_ok(), "a Host name refused");
    check(router.deliver_remote(peer, from, To::Name("kernel.selftest.host"), 6, 0, Payload::empty())
              == Err(Error::NotYours), "a Host name reached from a peer");
    /* A message of a's that did not get there, told back: from the mailbox
     * it was for, as this host knows it, why and its tag in the payload --
     * and none to a mailbox that never sent there */
    check(router.bounced(peer, a.addr(), To::Addr(Addr { host: 0, ..there }), 5, 78, Bounce::Full).is_ok(),
          "a bounce refused");
    let mut why = [0u8; BOUNCE_BYTES];
    check(a.try_recv().is_some_and(|m| m.tag() == TAG_BOUNCED && m.from() == there && m.reference() == 78
                                       && m.payload().read(0, &mut why) == BOUNCE_BYTES
                                       && why[..4] == Bounce::Full.code().to_le_bytes() && why[4..] == 5u32.to_le_bytes()),
          "the bounce's sender, reference or reason");
    check(router.bounced(peer, a.addr(), To::Name("nbs.x"), 5, 79, Bounce::Gone).is_ok()
              && a.try_recv().is_some_and(|m| m.tag() == TAG_BOUNCED && m.from() == Addr { host: peer, slot: 0,
                                                                                            generation: 0 }
                                          && m.payload().len() == BOUNCE_BYTES + 5),
          "a bounce of a message sent by name");
    check(router.bounced(peer, b.addr(), To::Addr(Addr { host: 0, ..there }), 5, 0, Bounce::Full)
              == Err(Error::NotYours), "a bounce told to a mailbox that never sent there");
    /* A monitor of the peer's mailbox: the router asked, the peer's Down */
    let id = a.monitor(there).unwrap_or_else(|e| panic!("ipc selftest: a remote monitor: {}", e.name()));
    check(router.try_recv().is_some_and(|m| m.tag() == TAG_ROUTE), "the router not asked to watch");
    check(router.remote_down(peer, id, Reason::Closed).is_ok(), "the peer's Down refused");
    check(a.try_recv().is_some_and(|m| m.reference() == id && m.down() == Some(Reason::Closed)), "the peer's Down");
    /* Another, ended by the peer lost */
    let id = a.monitor(there).unwrap_or_else(|_| panic!("ipc selftest: a remote monitor"));
    let _ = router.try_recv();
    check(router.peer_lost(peer).is_ok(), "the peer lost refused");
    check(a.try_recv().is_some_and(|m| m.reference() == id && m.down() == Some(Reason::HostLost)), "HostLost");
    check(a.send(To::Addr(there), 5, 0, Payload::empty()).is_err_and(|e| e.0 == Error::NoRoute),
          "a lost peer's mailbox reached");
    /* Up again: another number, and the old addresses still nobody's */
    let again = router.peer_up("selftest").unwrap_or_else(|_| panic!("ipc selftest: the peer up again"));
    check(again != peer, "a peer back up under its old number");
    check(a.send(To::Addr(there), 5, 0, Payload::empty()).is_err_and(|e| e.0 == Error::NoRoute),
          "an address from before the peer was lost reached");
    /* The router closed: the peer lost with it */
    drop(router);
    check(peers().iter().any(|p| p.1 == "selftest" && !p.2), "the peer up past its router's close");
}
