//! A running VM moved between two hosts' hv (plans/vm-migration.md): the
//! messages the old host (S) and the new one (D) send each other, over
//! ipcnet, to `hv.migrate` on D and then to the mailbox D answers from.
//!
//! ```text
//! S                                          D
//! Offer: the VM, its devices, its files -->  checks it can run it, builds it
//!                                       <--  Accept (or Refuse, saying why)
//! File, Pages, ... (a window unanswered) ->  written; Ack each
//! State: the VM's, in pieces             -->  put into the guest it built
//! Done: each volume's writer's generation ->  takes them from S's writer
//!                                       <--  Running (or Aborted, saying why)
//! ```
//!
//! Every message names its migration by the number S chose (`id`), and a
//! side takes nothing of another. Each is read whole and checked: a field
//! past its bound, a count that does not match what follows, bytes past
//! the end refuse the message. What the two decide from them -- the
//! handover's one compare and set, NBS's writer of the first volume, which
//! D's commit and S's giving up race for -- is in [`handover`].

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod handover;

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};

/// The name D's hv answers offers at, on every host: `hv.migrate@<host>`.
pub const MAILBOX: &str = "hv.migrate";
/// The one tag every message goes under: what it is, its first byte.
pub const TAG: u32 = 0x6D69_6772;
/// The most bytes a message may be: what a mailbox takes a message off in
/// one block of the heap's -- its largest, 512 KiB -- with room for the
/// allocator's own (`kcore::ipc::Mailbox::recv`, which drops a message it
/// has no block for); ipcnet would carry twice that.
pub const MESSAGE_MAX: usize = 496 * 1024;
/// A page.
pub const PAGE: u64 = 4096;
/// The most pages of data a `Pages` carries: 448 KiB of them -- with the
/// most numbers there may be in front of them, a message's most.
pub const PAGES_DATA_MAX: usize = 112;
/// The most pages a `Pages` names, zeros among them: a zero page is a mark
/// and no data.
pub const PAGES_MAX: usize = 4096;
/// The most of a file or of the state one message carries.
pub const PIECE_MAX: usize = 256 * 1024;
/// What a side has out unanswered at most: 2 MiB, as nbsclient keeps to --
/// ipcnet's queue never what bounces one -- and as many messages as half the
/// other's mailbox takes, however small they are.
pub const WINDOW_BYTES: usize = 2 << 20;
pub const WINDOW_MESSAGES: usize = 32;
/// The most of each a VM has.
pub const DISKS_MAX: usize = 8;
pub const NICS_MAX: usize = 8;
pub const VOLUMES_MAX: usize = DISKS_MAX;
pub const FILES_MAX: usize = 32;
/// A name's longest: a VM's, a file's, a disk's word, a host's.
pub const NAME_MAX: usize = 255;
/// The most bytes a VM's folder's file may be to go with it.
pub const FILE_MAX: u64 = 64 << 20;
/// A reason's longest, said back.
pub const WHY_MAX: usize = 255;
/// The most CPUs a VM has (`vmstate::CPUS_MAX`).
pub const CPUS_MAX: u32 = 64;
/// A vm.toml's longest (hv's `config::MAX_BYTES`).
pub const CONFIG_MAX: usize = 64 * 1024;
/// The most CPUID answers an offer says its CPU by (`Leaf`): a few hundred
/// leaves and subleaves a CPU has.
pub const LEAVES_MAX: usize = 512;
/// The most flows of its NIC's firewall a VM takes with it (`Flows`): a
/// port's most (hv's `net::FLOWS`), with room.
pub const FLOWS_MAX: usize = 4096;

/// A disk as its guest sees it: what D builds a stand-in of, before it has
/// the volume, and holds the volume to when it takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    /// The word its vm.toml names it by: `nbs:<volume>`.
    pub word: String,
    pub bytes: u64,
    pub block: u32,
    pub read_only: bool,
    pub discards: bool,
    pub writes_zeroes: bool,
    /// What its GET_ID says.
    pub serial: String,
}

/// A NIC as its guest sees it, and the port of vnet's it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nic {
    pub mac: [u8; 6],
    pub mtu: u16,
    /// Its port of vnet's: what moves to D at the handover.
    pub port: u64,
    /// The network it is on, as its vm.toml names it.
    pub network: String,
}

/// What S offers D.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub id: u64,
    /// The VM's name: its folder's, on S, and to be on D.
    pub name: String,
    /// S's host, as the registry names it.
    pub from: String,
    /// Its memory and CPUs, as its vm.toml says them: what D checks it has
    /// room for.
    pub mem_bytes: u64,
    pub cpus: u32,
    /// The extension it runs under (`vmstate::Vendor`'s code), its CPU
    /// model's digest, and the TSC rate it was told: what D refuses where
    /// its own differ.
    pub vendor: u8,
    pub model: [u8; 32],
    pub tsc_hz: u64,
    /// The shape its CPUs are told, beside their number: two threads a
    /// core, and a cache shared -- what the digest is of.
    pub smt: bool,
    pub llc: bool,
    /// Its vm.toml, as its folder has it: what D builds it as.
    pub config: String,
    /// The CPU it is shown, answer by answer: what `model` is the digest of,
    /// and D names a difference by.
    pub cpuid: Vec<Leaf>,
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
    /// Its folder's files that go with it, and their sizes.
    pub files: Vec<(String, u64)>,
    /// Its RAM's pages, for progress.
    pub pages: u64,
}

/// One answer of the CPU a guest is shown (hv's `policy::model_answers`): a
/// CPUID leaf and subleaf and its four registers -- or an MSR's, as the
/// policy says one. What the other host names the first difference by,
/// where the two CPUs' digests differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub leaf: u32,
    pub sub: u32,
    pub regs: [u32; 4],
}

/// A flow of the VM's NIC's stateful firewall (hv's `net::Carried`): what
/// lets the answers to its connections in on the other host as it did here.
/// The other end's address and port, the VM's own port, the protocol,
/// whether it began coming in, what its TCP connection showed, and how long
/// it has left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flow {
    pub proto: u8,
    pub local_port: u16,
    pub peer: u32,
    pub peer_port: u16,
    pub inbound: bool,
    pub seen: u8,
    pub left_ms: u32,
}

/// A page sent: its number, and whether it is all zeros -- no data then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub number: u64,
    pub zero: bool,
}

/// One message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    Offer(Offer),
    /// D has built the VM and takes the rest.
    Accept { id: u64 },
    /// D will not run it, and why.
    Refuse { id: u64, why: String },
    /// A piece of a file of its folder, at `offset`.
    File { id: u64, seq: u64, name: String, offset: u64, data: Vec<u8> },
    /// Pages of round `round`: each named, the data of those not zero after
    /// in their order.
    Pages { id: u64, seq: u64, round: u32, pages: Vec<Page>, data: Vec<u8> },
    /// Everything up to `seq` written.
    Ack { id: u64, seq: u64 },
    /// A piece of the VM's state (`vmstate`), at `offset` of `total`.
    State { id: u64, seq: u64, offset: u32, total: u32, data: Vec<u8> },
    /// The flows of its NIC's firewall, as it paused: after its state.
    Flows { id: u64, seq: u64, flows: Vec<Flow> },
    /// S has paused the VM and sent it all: each volume's writer's
    /// generation, the first the one that decides (`handover`).
    Done { id: u64, seq: u64, volumes: Vec<(String, u64)> },
    /// D runs it.
    Running { id: u64 },
    /// D gave it up before its commit, and why: S goes on with it.
    Aborted { id: u64, why: String },
    /// S gives it up before `Done`, and why: D lets go of what it built.
    Cancel { id: u64, why: String },
}

/* Kinds, the first byte. */
const OFFER: u8 = 1;
const ACCEPT: u8 = 2;
const REFUSE: u8 = 3;
const FILE: u8 = 4;
const PAGES: u8 = 5;
const ACK: u8 = 6;
const STATE: u8 = 7;
const DONE: u8 = 8;
const RUNNING: u8 = 9;
const ABORTED: u8 = 10;
const CANCEL: u8 = 11;
const FLOWS: u8 = 12;

impl Msg {
    /// The migration it is of.
    pub fn id(&self) -> u64 {
        match self {
            Msg::Offer(o) => o.id,
            Msg::Accept { id } | Msg::Refuse { id, .. } | Msg::File { id, .. } | Msg::Pages { id, .. }
            | Msg::Ack { id, .. } | Msg::State { id, .. } | Msg::Done { id, .. } | Msg::Running { id }
            | Msg::Aborted { id, .. } | Msg::Cancel { id, .. } | Msg::Flows { id, .. } => *id,
        }
    }

    /// Whether `decode` would read it back: every field inside its bound,
    /// every count what follows it -- `encode` makes no message that is not,
    /// rather than one cut short or read as another.
    pub fn valid(&self) -> bool {
        let name = |s: &str| !s.is_empty() && s.len() <= NAME_MAX && !s.chars().any(|c| c.is_control());
        match self {
            Msg::Offer(o) => {
                name(&o.name) && name(&o.from) && o.mem_bytes != 0 && o.mem_bytes % PAGE == 0 && o.cpus != 0
                    && o.cpus <= CPUS_MAX && o.tsc_hz != 0 && o.config.len() <= CONFIG_MAX
                    && o.disks.len() <= DISKS_MAX
                    && o.disks.iter().all(|d| {
                        name(&d.word) && name(&d.serial) && d.block.is_power_of_two() && (512..=4096).contains(&d.block)
                    })
                    && o.nics.len() <= NICS_MAX && o.nics.iter().all(|n| name(&n.network))
                    && o.files.len() <= FILES_MAX && o.files.iter().all(|(f, size)| name(f) && file_name(f) && *size <= FILE_MAX)
                    && o.cpuid.len() <= LEAVES_MAX
            }
            Msg::Accept { .. } | Msg::Ack { .. } | Msg::Running { .. } => true,
            Msg::Refuse { why, .. } | Msg::Aborted { why, .. } | Msg::Cancel { why, .. } => why.len() <= WHY_MAX,
            Msg::File { name: n, offset, data, .. } => {
                name(n) && file_name(n) && data.len() <= PIECE_MAX
                    && offset.checked_add(data.len() as u64).is_some_and(|end| end <= FILE_MAX)
            }
            Msg::Pages { pages, data, .. } => {
                let with_data = pages.iter().filter(|p| !p.zero).count();
                pages.len() <= PAGES_MAX && with_data <= PAGES_DATA_MAX
                    && pages.iter().all(|p| p.number & !NUMBER_MASK == 0) && data.len() == with_data * PAGE as usize
            }
            Msg::State { offset, total, data, .. } => {
                data.len() <= PIECE_MAX && *total as usize <= STATE_MAX
                    && u32::try_from(data.len()).ok().and_then(|n| offset.checked_add(n)).is_some_and(|end| end <= *total)
            }
            Msg::Done { volumes, .. } => {
                !volumes.is_empty() && volumes.len() <= VOLUMES_MAX && volumes.iter().all(|(v, g)| name(v) && *g != 0)
            }
            Msg::Flows { flows, .. } => flows.len() <= FLOWS_MAX,
        }
    }

    /// Its bytes, a message's at most; None where it is not one `decode`
    /// would read back (`valid`), or there was no memory.
    pub fn encode(&self) -> Option<Vec<u8>> {
        if !self.valid() {
            return None;
        }
        let mut w = Writer::new();
        w.buf.try_reserve(self.size_hint()).ok()?;
        match self {
            Msg::Offer(o) => {
                w.u8(OFFER);
                w.u64(o.id);
                put_str(&mut w, &o.name);
                put_str(&mut w, &o.from);
                w.u64(o.mem_bytes);
                w.u32(o.cpus);
                w.u8(o.vendor);
                w.fixed(&o.model);
                w.u64(o.tsc_hz);
                w.u8(u8::from(o.smt) | u8::from(o.llc) << 1);
                w.u32(o.config.len() as u32);
                w.fixed(o.config.as_bytes());
                w.u16(o.cpuid.len() as u16);
                for l in &o.cpuid {
                    for v in [l.leaf, l.sub, l.regs[0], l.regs[1], l.regs[2], l.regs[3]] {
                        w.u32(v);
                    }
                }
                w.u8(o.disks.len() as u8);
                for d in &o.disks {
                    put_str(&mut w, &d.word);
                    w.u64(d.bytes);
                    w.u32(d.block);
                    w.u8(u8::from(d.read_only) | u8::from(d.discards) << 1 | u8::from(d.writes_zeroes) << 2);
                    put_str(&mut w, &d.serial);
                }
                w.u8(o.nics.len() as u8);
                for n in &o.nics {
                    w.fixed(&n.mac);
                    w.u16(n.mtu);
                    w.u64(n.port);
                    put_str(&mut w, &n.network);
                }
                w.u8(o.files.len() as u8);
                for (name, size) in &o.files {
                    put_str(&mut w, name);
                    w.u64(*size);
                }
                w.u64(o.pages);
            }
            Msg::Accept { id } => {
                w.u8(ACCEPT);
                w.u64(*id);
            }
            Msg::Refuse { id, why } => {
                w.u8(REFUSE);
                w.u64(*id);
                put_str(&mut w, why);
            }
            Msg::File { id, seq, name, offset, data } => {
                w.u8(FILE);
                w.u64(*id);
                w.u64(*seq);
                put_str(&mut w, name);
                w.u64(*offset);
                w.u32(data.len() as u32);
                w.fixed(data);
            }
            Msg::Pages { id, seq, round, pages, data } => {
                w.u8(PAGES);
                w.u64(*id);
                w.u64(*seq);
                w.u32(*round);
                w.u16(pages.len() as u16);
                for p in pages {
                    /* A page's number in 52 bits, its zero mark above */
                    w.u64(p.number | if p.zero { ZERO } else { 0 });
                }
                w.fixed(data);
            }
            Msg::Ack { id, seq } => {
                w.u8(ACK);
                w.u64(*id);
                w.u64(*seq);
            }
            Msg::State { id, seq, offset, total, data } => {
                w.u8(STATE);
                w.u64(*id);
                w.u64(*seq);
                w.u32(*offset);
                w.u32(*total);
                w.u32(data.len() as u32);
                w.fixed(data);
            }
            Msg::Done { id, seq, volumes } => {
                w.u8(DONE);
                w.u64(*id);
                w.u64(*seq);
                w.u8(volumes.len() as u8);
                for (name, generation) in volumes {
                    put_str(&mut w, name);
                    w.u64(*generation);
                }
            }
            Msg::Running { id } => {
                w.u8(RUNNING);
                w.u64(*id);
            }
            Msg::Aborted { id, why } => {
                w.u8(ABORTED);
                w.u64(*id);
                put_str(&mut w, why);
            }
            Msg::Cancel { id, why } => {
                w.u8(CANCEL);
                w.u64(*id);
                put_str(&mut w, why);
            }
            Msg::Flows { id, seq, flows } => {
                w.u8(FLOWS);
                w.u64(*id);
                w.u64(*seq);
                w.u16(flows.len() as u16);
                for f in flows {
                    w.u8(f.proto);
                    w.u16(f.local_port);
                    w.u32(f.peer);
                    w.u16(f.peer_port);
                    w.u8(u8::from(f.inbound));
                    w.u8(f.seen);
                    w.u32(f.left_ms);
                }
            }
        }
        (w.buf.len() <= MESSAGE_MAX).then_some(w.buf)
    }

    fn size_hint(&self) -> usize {
        match self {
            Msg::File { data, .. } | Msg::State { data, .. } => 64 + data.len(),
            Msg::Pages { pages, data, .. } => 32 + 8 * pages.len() + data.len(),
            Msg::Flows { flows, .. } => 32 + 15 * flows.len(),
            Msg::Offer(o) => 4096 + o.config.len() + 24 * o.cpuid.len(),
            _ => 1024,
        }
    }

    /// The message `bytes` hold, every field checked; None for anything
    /// that is not one.
    pub fn decode(bytes: &[u8]) -> Option<Msg> {
        if bytes.len() > MESSAGE_MAX {
            return None;
        }
        let mut r = Reader::new(bytes);
        let msg = match r.u8()? {
            OFFER => {
                let id = r.u64()?;
                let name = read_name(&mut r)?;
                let from = read_name(&mut r)?;
                let mem_bytes = r.u64()?;
                let cpus = r.u32()?;
                let vendor = r.u8()?;
                let model = r.fixed::<32>()?;
                let tsc_hz = r.u64()?;
                if mem_bytes == 0 || mem_bytes % PAGE != 0 || cpus == 0 || cpus > CPUS_MAX || tsc_hz == 0 {
                    return None;
                }
                let shape = r.u8()?;
                if shape & !3 != 0 {
                    return None;
                }
                let n = r.u32()? as usize;
                if n > CONFIG_MAX {
                    return None;
                }
                let text = core::str::from_utf8(r.bytes(n)?).ok()?;
                let mut config = String::new();
                config.try_reserve_exact(n).ok()?;
                config.push_str(text);
                let n = usize::from(r.u16()?);
                if n > LEAVES_MAX {
                    return None;
                }
                let mut cpuid = Vec::new();
                cpuid.try_reserve_exact(n).ok()?;
                for _ in 0..n {
                    let (leaf, sub) = (r.u32()?, r.u32()?);
                    cpuid.push(Leaf { leaf, sub, regs: [r.u32()?, r.u32()?, r.u32()?, r.u32()?] });
                }
                let n = usize::from(r.u8()?);
                if n > DISKS_MAX {
                    return None;
                }
                let mut disks = Vec::new();
                disks.try_reserve_exact(n).ok()?;
                for _ in 0..n {
                    let word = read_name(&mut r)?;
                    let bytes = r.u64()?;
                    let block = r.u32()?;
                    let flags = r.u8()?;
                    if flags & !7 != 0 || !block.is_power_of_two() || !(512..=4096).contains(&block) {
                        return None;
                    }
                    let serial = read_name(&mut r)?;
                    disks.push(Disk { word, bytes, block, read_only: flags & 1 != 0, discards: flags & 2 != 0,
                                      writes_zeroes: flags & 4 != 0, serial });
                }
                let n = usize::from(r.u8()?);
                if n > NICS_MAX {
                    return None;
                }
                let mut nics = Vec::new();
                nics.try_reserve_exact(n).ok()?;
                for _ in 0..n {
                    nics.push(Nic { mac: r.fixed::<6>()?, mtu: r.u16()?, port: r.u64()?, network: read_name(&mut r)? });
                }
                let n = usize::from(r.u8()?);
                if n > FILES_MAX {
                    return None;
                }
                let mut files = Vec::new();
                files.try_reserve_exact(n).ok()?;
                for _ in 0..n {
                    let file = read_name(&mut r)?;
                    let size = r.u64()?;
                    if !file_name(&file) || size > FILE_MAX {
                        return None;
                    }
                    files.push((file, size));
                }
                let pages = r.u64()?;
                Msg::Offer(Offer { id, name, from, mem_bytes, cpus, vendor, model, tsc_hz, smt: shape & 1 != 0,
                                   llc: shape & 2 != 0, config, cpuid, disks, nics, files, pages })
            }
            ACCEPT => Msg::Accept { id: r.u64()? },
            REFUSE => Msg::Refuse { id: r.u64()?, why: read_why(&mut r)? },
            FILE => {
                let (id, seq) = (r.u64()?, r.u64()?);
                let name = read_name(&mut r)?;
                let offset = r.u64()?;
                let n = r.u32()? as usize;
                if !file_name(&name) || n > PIECE_MAX || offset.checked_add(n as u64)? > FILE_MAX {
                    return None;
                }
                Msg::File { id, seq, name, offset, data: bytes_of(&mut r, n)? }
            }
            PAGES => {
                let (id, seq, round) = (r.u64()?, r.u64()?, r.u32()?);
                let n = usize::from(r.u16()?);
                if n > PAGES_MAX {
                    return None;
                }
                let mut pages = Vec::new();
                pages.try_reserve_exact(n).ok()?;
                let mut data_pages = 0usize;
                for _ in 0..n {
                    let word = r.u64()?;
                    if word & !(NUMBER_MASK | ZERO) != 0 {
                        return None;
                    }
                    let zero = word & ZERO != 0;
                    data_pages += usize::from(!zero);
                    pages.push(Page { number: word & NUMBER_MASK, zero });
                }
                if data_pages > PAGES_DATA_MAX {
                    return None;
                }
                let data = bytes_of(&mut r, data_pages * PAGE as usize)?;
                Msg::Pages { id, seq, round, pages, data }
            }
            ACK => Msg::Ack { id: r.u64()?, seq: r.u64()? },
            STATE => {
                let (id, seq) = (r.u64()?, r.u64()?);
                let (offset, total) = (r.u32()?, r.u32()?);
                let n = r.u32()? as usize;
                if n > PIECE_MAX || total as usize > STATE_MAX || offset.checked_add(n as u32)? > total {
                    return None;
                }
                Msg::State { id, seq, offset, total, data: bytes_of(&mut r, n)? }
            }
            DONE => {
                let (id, seq) = (r.u64()?, r.u64()?);
                let n = usize::from(r.u8()?);
                if n == 0 || n > VOLUMES_MAX {
                    return None;
                }
                let mut volumes = Vec::new();
                volumes.try_reserve_exact(n).ok()?;
                for _ in 0..n {
                    let name = read_name(&mut r)?;
                    let generation = r.u64()?;
                    if generation == 0 {
                        return None;
                    }
                    volumes.push((name, generation));
                }
                Msg::Done { id, seq, volumes }
            }
            RUNNING => Msg::Running { id: r.u64()? },
            ABORTED => Msg::Aborted { id: r.u64()?, why: read_why(&mut r)? },
            CANCEL => Msg::Cancel { id: r.u64()?, why: read_why(&mut r)? },
            FLOWS => {
                let (id, seq) = (r.u64()?, r.u64()?);
                let n = usize::from(r.u16()?);
                if n > FLOWS_MAX {
                    return None;
                }
                let mut flows = Vec::new();
                flows.try_reserve_exact(n).ok()?;
                for _ in 0..n {
                    let (proto, local_port, peer, peer_port) = (r.u8()?, r.u16()?, r.u32()?, r.u16()?);
                    let flags = r.u8()?;
                    if flags & !1 != 0 {
                        return None;
                    }
                    flows.push(Flow { proto, local_port, peer, peer_port, inbound: flags & 1 != 0, seen: r.u8()?,
                                      left_ms: r.u32()? });
                }
                Msg::Flows { id, seq, flows }
            }
            _ => return None,
        };
        r.done().then_some(msg)
    }
}

/// The most bytes a VM's state is: a few KiB a CPU.
pub const STATE_MAX: usize = 4 << 20;
/// A page's number: what 52 bits of address hold, in pages.
const NUMBER_MASK: u64 = (1 << 40) - 1;
const ZERO: u64 = 1 << 63;

fn put_str(w: &mut Writer, s: &str) {
    w.str(s);
}

/// A name: UTF-8, not empty, `NAME_MAX` at most, no NUL and no control.
fn read_name(r: &mut Reader) -> Option<String> {
    let s = r.str(NAME_MAX)?;
    if s.is_empty() || s.chars().any(|c| c.is_control()) {
        return None;
    }
    let mut out = String::new();
    out.try_reserve_exact(s.len()).ok()?;
    out.push_str(s);
    Some(out)
}

/// A reason said back: UTF-8, `WHY_MAX` at most.
fn read_why(r: &mut Reader) -> Option<String> {
    let s = r.str(WHY_MAX)?;
    let mut out = String::new();
    out.try_reserve_exact(s.len()).ok()?;
    out.push_str(s);
    Some(out)
}

/// A file of a VM's folder, as the folder names one: one name, no path --
/// not `.` or `..`, no `/`.
pub fn file_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.chars().any(|c| c.is_control())
}

fn bytes_of(r: &mut Reader, n: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).ok()?;
    v.extend_from_slice(r.bytes(n)?);
    Some(v)
}
