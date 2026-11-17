//! A migration's messages (`migwire`, plans/vm-migration.md), as D and S
//! read each other's:
//!
//! - **Hostile bytes**: refused or read, never a panic -- and a message read
//!   is written back as the same bytes: one reading of every message.
//! - **Every message the input makes**: written exactly where it is inside
//!   its bounds (`Msg::valid`), read back the same, and no bigger than a
//!   mailbox takes; its bytes then changed, cut short and grown, as hostile.
//! - **Each bound one past**: a message made inside its bounds, one field
//!   pushed past -- refused by `encode`, which writes no message `decode`
//!   would not read back; and the largest a message may be, written.

use migwire::{Disk, Flow, Leaf, Msg, Nic, Offer, Page, PAGE};

use crate::targets::Input;

pub fn migwire_target(r: &mut Input) {
    match r.op(4) {
        Some(0) => hostile(r.rest()),
        Some(1) | Some(2) => {
            let m = message(r);
            let bytes = written(&m);
            while let Some(op) = r.op(3) {
                let mut b = bytes.clone();
                match op {
                    0 if !b.is_empty() => {
                        let at = r.below(b.len() as u64) as usize;
                        b[at] ^= r.u8() | 1;
                    }
                    1 => b.truncate(r.below(b.len() as u64 + 1) as usize),
                    _ => b.extend_from_slice(&[r.u8(); 3]),
                }
                hostile(&b);
            }
        }
        _ => {
            largest();
            let mut m = message(r);
            let which = r.below(BOUNDS);
            if past(&mut m, which, r) {
                invariant!(!m.valid() && m.encode().is_none(), "a message past bound {} written: {:?}", which,
                           kind(&m));
            }
        }
    }
}

/// `bytes`, read: what reads is a message that writes back as `bytes`.
fn hostile(bytes: &[u8]) {
    if let Some(m) = Msg::decode(bytes) {
        invariant!(m.valid(), "a message read is not one: {:?}", kind(&m));
        let again = m.encode().expect("a message read is written");
        invariant!(again == bytes, "a message read and written is other bytes: {:?}", kind(&m));
    }
}

/// `m`, inside its bounds, written and read back.
fn written(m: &Msg) -> Vec<u8> {
    invariant!(m.valid(), "a message made inside its bounds is not one: {:?}", kind(m));
    let bytes = m.encode().expect("a message inside its bounds is written");
    invariant!(bytes.len() <= migwire::MESSAGE_MAX, "a message of {} bytes, past a mailbox's", bytes.len());
    let back = Msg::decode(&bytes);
    invariant!(back.as_ref() == Some(m), "a message read back is not the one written: {:?}", kind(m));
    bytes
}

fn kind(m: &Msg) -> &'static str {
    match m {
        Msg::Offer(_) => "offer",
        Msg::Accept { .. } => "accept",
        Msg::Refuse { .. } => "refuse",
        Msg::File { .. } => "file",
        Msg::Pages { .. } => "pages",
        Msg::Ack { .. } => "ack",
        Msg::State { .. } => "state",
        Msg::Done { .. } => "done",
        Msg::Running { .. } => "running",
        Msg::Aborted { .. } => "aborted",
        Msg::Cancel { .. } => "cancel",
        Msg::Flows { .. } => "flows",
    }
}

/// The largest message there is -- a page run of every number it may name
/// and every page of data -- written, and inside a mailbox's block.
fn largest() {
    let mut pages = Vec::new();
    for n in 0..migwire::PAGES_MAX as u64 {
        pages.push(Page { number: n, zero: n >= migwire::PAGES_DATA_MAX as u64 });
    }
    let data = vec![0xA5; migwire::PAGES_DATA_MAX * PAGE as usize];
    let m = Msg::Pages { id: u64::MAX, seq: u64::MAX, round: u32::MAX, pages, data };
    written(&m);
}

/* ---- messages of the input's, every field inside its bounds ---- */

const LETTERS: &[u8] = b"abcxyz019.-_";

/// A name: 1 to `max` of a few letters, now and then the longest.
fn name(r: &mut Input, max: usize) -> String {
    let n = if r.below(8) == 0 { max } else { 1 + r.below(max.min(24) as u64) as usize };
    (0..n).map(|_| char::from(r.pick(LETTERS))).collect()
}

/// A file's name: a name that is no path.
fn file(r: &mut Input) -> String {
    let n = name(r, migwire::NAME_MAX);
    if n == "." || n == ".." { String::from("f") } else { n }
}

fn why(r: &mut Input) -> String {
    let n = r.below(migwire::WHY_MAX as u64 + 1) as usize;
    (0..n).map(|i| if i % 7 == 6 { '\n' } else { 'w' }).collect()
}

fn bytes(r: &mut Input, max: usize) -> Vec<u8> {
    let n = if r.below(16) == 0 { max } else { r.below(max.min(4096) as u64 + 1) as usize };
    let fill = r.u8();
    (0..n).map(|i| fill.wrapping_add(i as u8)).collect()
}

fn offer(r: &mut Input) -> Offer {
    let disks = (0..r.below(migwire::DISKS_MAX as u64 + 1)).map(|_| Disk {
        word: name(r, migwire::NAME_MAX),
        bytes: r.u64(),
        block: 512 << r.below(4),
        read_only: r.bool(),
        discards: r.bool(),
        writes_zeroes: r.bool(),
        serial: name(r, migwire::NAME_MAX),
    }).collect();
    let nics = (0..r.below(migwire::NICS_MAX as u64 + 1)).map(|_| Nic {
        mac: [r.u8(), r.u8(), r.u8(), r.u8(), r.u8(), r.u8()],
        mtu: r.u16(),
        port: r.u64(),
        network: name(r, migwire::NAME_MAX),
    }).collect();
    let files = (0..r.below(migwire::FILES_MAX as u64 + 1)).map(|_| (file(r), r.below(migwire::FILE_MAX + 1))).collect();
    let config_len = if r.below(16) == 0 { migwire::CONFIG_MAX } else { r.below(2048) as usize };
    Offer {
        id: r.u64(),
        name: name(r, migwire::NAME_MAX),
        from: name(r, migwire::NAME_MAX),
        mem_bytes: (1 + r.below(1 << 30)) * PAGE,
        cpus: 1 + r.below(u64::from(migwire::CPUS_MAX)) as u32,
        vendor: r.u8(),
        model: core::array::from_fn(|_| r.u8()),
        tsc_hz: 1 + r.below(u64::MAX - 1),
        smt: r.bool(),
        llc: r.bool(),
        config: (0..config_len).map(|i| if i % 40 == 39 { '\n' } else { 'k' }).collect(),
        cpuid: (0..if r.below(16) == 0 { migwire::LEAVES_MAX as u64 } else { r.below(160) }).map(|i| Leaf {
            leaf: i as u32, sub: r.u32(), regs: [r.u32(), r.u32(), r.u32(), r.u32()],
        }).collect(),
        disks,
        nics,
        files,
        pages: r.u64(),
    }
}

fn pages(r: &mut Input) -> (Vec<Page>, Vec<u8>) {
    let n = if r.below(16) == 0 { migwire::PAGES_MAX } else { r.below(256) as usize };
    let mut with_data = 0;
    let mut pages = Vec::new();
    for _ in 0..n {
        let zero = with_data == migwire::PAGES_DATA_MAX || r.bool();
        with_data += usize::from(!zero);
        pages.push(Page { number: r.below(1 << 40), zero });
    }
    let fill = r.u8();
    (pages, vec![fill; with_data * PAGE as usize])
}

fn message(r: &mut Input) -> Msg {
    let id = r.u64();
    match r.below(12) {
        0 => Msg::Offer(offer(r)),
        1 => Msg::Accept { id },
        2 => Msg::Refuse { id, why: why(r) },
        3 => {
            let data = bytes(r, migwire::PIECE_MAX);
            let offset = r.below(migwire::FILE_MAX - data.len() as u64 + 1);
            Msg::File { id, seq: r.u64(), name: file(r), offset, data }
        }
        4 => {
            let (pages, data) = pages(r);
            Msg::Pages { id, seq: r.u64(), round: r.u32(), pages, data }
        }
        5 => Msg::Ack { id, seq: r.u64() },
        6 => {
            let data = bytes(r, migwire::PIECE_MAX);
            let total = data.len() as u32 + r.below((migwire::STATE_MAX - data.len()) as u64 + 1) as u32;
            let offset = r.below(u64::from(total) - data.len() as u64 + 1) as u32;
            Msg::State { id, seq: r.u64(), offset, total, data }
        }
        7 => {
            let n = 1 + r.below(migwire::VOLUMES_MAX as u64) as usize;
            let volumes = (0..n).map(|_| (name(r, migwire::NAME_MAX), 1 + r.below(u64::MAX - 1))).collect();
            Msg::Done { id, seq: r.u64(), volumes }
        }
        8 => Msg::Running { id },
        9 => Msg::Aborted { id, why: why(r) },
        11 => {
            let n = if r.below(16) == 0 { migwire::FLOWS_MAX } else { r.below(64) as usize };
            let flows = (0..n).map(|_| Flow {
                proto: r.u8(), local_port: r.u16(), peer: r.u32(), peer_port: r.u16(), inbound: r.bool(), seen: r.u8(),
                left_ms: r.u32(),
            }).collect();
            Msg::Flows { id, seq: r.u64(), flows }
        }
        _ => Msg::Cancel { id, why: why(r) },
    }
}

/* ---- one field past its bound ---- */

const BOUNDS: u64 = 26;

/// `m` with field `which` one past its bound, where `m` has that field:
/// false where it has not.
fn past(m: &mut Msg, which: u64, r: &mut Input) -> bool {
    let long = |n: usize| -> String { "n".repeat(n) };
    match (m, which) {
        (Msg::Offer(o), 0) => o.name = String::new(),
        (Msg::Offer(o), 1) => o.from = long(migwire::NAME_MAX + 1),
        (Msg::Offer(o), 2) => o.name = String::from("a\u{1}b"),
        (Msg::Offer(o), 3) => o.mem_bytes = 0,
        (Msg::Offer(o), 4) => o.mem_bytes = PAGE * 3 + 1 + r.below(PAGE - 1),
        (Msg::Offer(o), 5) => o.cpus = if r.bool() { 0 } else { migwire::CPUS_MAX + 1 },
        (Msg::Offer(o), 6) => o.tsc_hz = 0,
        (Msg::Offer(o), 7) => o.config = long(migwire::CONFIG_MAX + 1),
        (Msg::Offer(o), 8) => {
            let d = Disk { word: String::from("nbs:v"), bytes: 1, block: 512, read_only: false, discards: false,
                           writes_zeroes: false, serial: String::from("s") };
            o.disks = vec![d; migwire::DISKS_MAX + 1];
        }
        (Msg::Offer(o), 9) => {
            let block = [0, 256, 513, 1000, 8192, u32::MAX][r.below(6) as usize];
            o.disks = vec![Disk { word: String::from("w"), bytes: 1, block, read_only: false, discards: false,
                                  writes_zeroes: false, serial: String::from("s") }];
        }
        (Msg::Offer(o), 10) => {
            o.nics = vec![Nic { mac: [0; 6], mtu: 1500, port: 1, network: String::from("n") }; migwire::NICS_MAX + 1];
        }
        (Msg::Offer(o), 11) => o.files = vec![(String::from("f"), 0); migwire::FILES_MAX + 1],
        (Msg::Offer(o), 12) => {
            let bad = [".", "..", "a/b", "/", ""][r.below(5) as usize];
            o.files = vec![(String::from(bad), 0)];
        }
        (Msg::Offer(o), 13) => o.files = vec![(String::from("f"), migwire::FILE_MAX + 1)],
        (Msg::Refuse { why, .. } | Msg::Aborted { why, .. } | Msg::Cancel { why, .. }, 14) => {
            *why = long(migwire::WHY_MAX + 1);
        }
        (Msg::File { data, .. }, 15) => *data = vec![0; migwire::PIECE_MAX + 1],
        (Msg::File { offset, data, .. }, 16) => *offset = migwire::FILE_MAX - data.len() as u64 + 1,
        (Msg::File { name, .. }, 17) => *name = String::from(if r.bool() { "a/b" } else { ".." }),
        (Msg::Pages { pages, data, .. }, 18) => {
            *pages = vec![Page { number: 1, zero: true }; migwire::PAGES_MAX + 1];
            data.clear();
        }
        (Msg::Pages { pages, data, .. }, 19) => {
            *pages = vec![Page { number: 1, zero: false }; migwire::PAGES_DATA_MAX + 1];
            *data = vec![0; pages.len() * PAGE as usize];
        }
        (Msg::Pages { pages, data, .. }, 20) => {
            /* Data that is not its pages': a page short or one over */
            let with_data = pages.iter().filter(|p| !p.zero).count();
            let n = if r.bool() || with_data == 0 { with_data + 1 } else { with_data - 1 };
            *data = vec![0; n * PAGE as usize];
        }
        (Msg::Pages { pages, .. }, 21) => {
            pages.push(Page { number: 1 << 40 | r.below(1 << 20), zero: true });
        }
        (Msg::State { offset, total, data, .. }, 22) => {
            if r.bool() {
                *total = migwire::STATE_MAX as u32 + 1;
            } else {
                *offset = *total - data.len() as u32 + 1;
            }
        }
        (Msg::Offer(o), 24) => o.cpuid = vec![Leaf { leaf: 0, sub: 0, regs: [0; 4] }; migwire::LEAVES_MAX + 1],
        (Msg::Flows { flows, .. }, 25) => {
            *flows = vec![Flow { proto: 6, local_port: 1, peer: 1, peer_port: 1, inbound: false, seen: 0, left_ms: 1 };
                          migwire::FLOWS_MAX + 1];
        }
        (Msg::Done { volumes, .. }, 23) => match r.below(3) {
            0 => volumes.clear(),
            1 => *volumes = vec![(String::from("v"), 1); migwire::VOLUMES_MAX + 1],
            _ => volumes[0].1 = 0,
        },
        _ => return false,
    }
    true
}
