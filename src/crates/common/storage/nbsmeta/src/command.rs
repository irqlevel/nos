//! What changes the metadata, and what it is answered: each as bytes, read
//! whole and every count bounded.

use alloc::string::String;
use alloc::vec::Vec;

use codec::{Reader, Writer};

/// A change to the metadata, as a log entry carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// The NBS cluster made, `host`'s mds -- as member `member` -- its first.
    Init { cluster: [u8; 16], name: String, host: String, member: u64 },
    /// `host` a member, as `member`: the incarnation its mds made when it was
    /// offered the cluster. Taken again as done where it is that member
    /// already.
    Add { host: String, member: u64 },
    /// `host`'s member out, for good: its incarnation never taken again. Done
    /// again where it is out already. Its cs, kept by the host's name, stays:
    /// a member replaced keeps its data.
    Remove { host: String },
    /// Device `ident` (`part:...`, `disk:...`, as nos finds it) of `host`'s cs,
    /// `slots` chunks of room: done again where it is that already.
    DeviceAdd { host: String, ident: String, slots: u32 },
    /// Device `ident` of `host`'s cs given no new chunks; moving what it holds
    /// comes with repair (N9). Done again where it drains already.
    DeviceDrain { host: String, ident: String },
    /// Device `device`'s format as its cs found it: `format` the 64 random
    /// bits its superblock carries -- recorded where none is, done again where
    /// it is that one, refused (`OtherFormat`) where another is. With 0, the
    /// record dropped: a person's wipe of a device whose contents are lost
    /// (`nbs devices add ... wipe`), for its cs to format it anew. A cs never
    /// formats a device the metadata has a format of: one it reads blank is
    /// wiped or rotten, its chunks lost, and said so -- formatted, its
    /// replicas read as zeros where they held data (the review of 2026-11).
    DeviceFormatted { device: u32, format: u64 },
    /// Volume `name`, `size_mib` MiB, placed with one replica on `host` --
    /// its own host, where it is made and attached from -- where that has
    /// room. Made again with the same size, it is answered as it is.
    VolumeCreate { name: String, size_mib: u64, host: String },
    /// Volume `name` gone, its chunks counted down. Done where it is gone.
    VolumeDelete { name: String },
    /// Volume `name` grown to `size_mib` MiB: never made smaller.
    VolumeResize { name: String, size_mib: u64 },
    /// Volume `name` attached on `host`: for writing -- a new generation, the
    /// fence of every older one -- or read-only. `force` takes it from whoever
    /// has it. `token` is the client's own, made for its session: an attach
    /// for writing asked again by the writer it made -- its answer late or
    /// lost -- is answered with the generation it gave, not another.
    Attach { name: String, host: String, writable: bool, force: bool, token: u64 },
    /// Volume `name` let go by `host`: the writer of `generation`, or -- with
    /// generation 0 -- a reader. Done where it is not so attached. `token`:
    /// the session's that let go, kept with the volume so that an attach of
    /// the same session's, applied late, attaches it to nobody (the review
    /// of 2026-11); 0 in an entry of before it.
    Detach { name: String, host: String, generation: u64, token: u64 },
    /// Volume `name`'s writer on `host` at `generation` attached anew, at a
    /// new generation, `token` its session's now (N10c2): a session paused
    /// for a snapshot that heard no word, so that a snapshot of its pause
    /// applied late is refused. Refused (`NotTheWriter`) where the volume's
    /// writer is no longer so -- taken over since, here or elsewhere. Asked
    /// again by that token, answered with the generation it gave.
    Reattach { name: String, host: String, generation: u64, token: u64 },
    /// The leader's digest of the state at entry `at`, for every member to
    /// hold its own to.
    Digest { at: u64, digest: [u8; 32] },
    /// Replica `cs` of set `set` taken out (N8): written and read by nobody
    /// until repair brings it back (N9), the set's version up -- and with it
    /// the epoch of every chunk in the set. Asked by a client that a replica
    /// did not answer, and proposed by the leader once the cs does not answer
    /// it either -- or, `refused`, that the cs itself refused as failed,
    /// which the leader takes as it is. Done again where it is out already;
    /// never the set's last active replica, which writes go on to. Asked by
    /// volume `name`'s writer at `generation`, of a set of its chunks: a
    /// writer fenced since takes nothing out -- else its write would be done
    /// by taking out the replicas that refuse it. `version`: the set's as the
    /// writer's map had it -- refused as stale once the set has moved on, a
    /// member asked out late taken out of nothing it has joined since (the
    /// review of 2026-11); none in an entry of before it, taken as it is.
    ReplicaDown { set: u32, cs: u32, refused: bool, name: String, generation: u64, version: Option<u32> },
    /// cs `cs` silent past the controller's 5 s, as the leader sees it
    /// (N9): down in every set it is a member of -- never the last active
    /// member of one -- dropped as the joiner of any, and given no new
    /// chunks. Done again where it is down or out already.
    CsDown { cs: u32 },
    /// cs `cs` answering the leader again: up, taking new chunks. Back in a
    /// set it is down in only by joining it again.
    CsUp { cs: u32 },
    /// cs `cs` silent past `out_after`, or its host no member: down in its
    /// sets as `CsDown` has it, its places there to be taken by others.
    CsOut { cs: u32 },
    /// cs `cs` joins set `set` -- planned at the set's `version` -- to take
    /// the place of member `place` once its copy is done: the member itself,
    /// down and back, or another cs in the place of a member down and out,
    /// or drained.
    Join { set: u32, place: u8, cs: u32, version: u32 },
    /// The join of set `set` by cs `cs` that raised it to `since` done: the
    /// joiner a member in its place, active.
    Joined { set: u32, cs: u32, since: u32 },
    /// The join of set `set` by cs `cs` that raised it to `since` given up.
    Unjoin { set: u32, cs: u32, since: u32 },
    /// A person's: member `cs` of set `set` taken down at the set's
    /// `version` as its copy is found bad -- the leader's scrub said it unlike
    /// the set's two others (N9e). Down, as a member taken out is, for the
    /// controller to have it join its own place again: its copy holds every
    /// chunk to a source's by sums. Never the set's last active member, a
    /// member down already, nor a set with a joiner; refused as stale once
    /// the set has moved on.
    ReplicaBad { set: u32, cs: u32, version: u32 },
    /// A snapshot of volume `volume`, named `name` (N10): its map as it is,
    /// every chunk's count up -- what the volume held, for good. At the
    /// generation of the writer that paused for it, or 0 for a volume with
    /// none: a writer not paused could have a write on its way into a chunk
    /// the snapshot now shares. Of a volume only -- an image or a snapshot
    /// is read-only, and cloned rather than snapshotted. Made again of the
    /// same volume, it is answered as it is.
    Snapshot { volume: String, name: String, generation: u64 },
    /// Volume `name` made an image (N10): read-only for good, to be cloned;
    /// refused while attached, its copy-ups pending given up. Done where it
    /// is an image already.
    Freeze { name: String },
    /// Volume `name`, `host`'s own, a clone of image or snapshot `from`
    /// (N10): its map `from`'s, every chunk's count up, room reserved for
    /// none of them -- a write to one copies it up. Made again of the same
    /// origin, it is answered as it is.
    Clone { from: String, name: String, host: String },
    /// A copy-up's first step (N10): chunk `place` of volume `name`'s map --
    /// shared, and to be written by its writer at `generation` -- given a
    /// new chunk in the same set, its room taken on the set's cs's, refused
    /// where one has none left; pending, the map naming the old one still.
    /// Asked again, answered the pending one; asked of a chunk shared no
    /// longer, the chunk itself.
    CopyUp { name: String, place: u32, generation: u64 },
    /// A copy-up's second step: pending chunk `chunk` of place `place`, at
    /// `incarnation`, copied onto its set's active members -- volume `name`'s
    /// map names it, and the old one's count goes down, at none freed. Done
    /// where the map names it already. Refused (`Stale`) where the pending
    /// chunk is that number at another incarnation: a copy-up given up -- a
    /// join of its set begun -- and asked anew, by an ask of the writer's
    /// asked again and late, is given a new chunk, which can be the number
    /// freed; an end of the old one's then would have the map name a chunk
    /// nobody copied into, zeros where the volume had data (found by
    /// fuzz/nbs's cluster target, N13c).
    CopyDone { name: String, place: u32, generation: u64, chunk: u32, incarnation: u32 },
}

const INIT: u8 = 1;
const ADD: u8 = 2;
const REMOVE: u8 = 3;
const DEVICE_ADD: u8 = 4;
const DEVICE_DRAIN: u8 = 5;
const VOLUME_CREATE: u8 = 6;
const VOLUME_DELETE: u8 = 7;
const VOLUME_RESIZE: u8 = 8;
const ATTACH: u8 = 9;
const DETACH: u8 = 10;
const DIGEST: u8 = 11;
const REPLICA_DOWN: u8 = 12;
const CS_DOWN: u8 = 13;
const CS_UP: u8 = 14;
const CS_OUT: u8 = 15;
const JOIN: u8 = 16;
const JOINED: u8 = 17;
const UNJOIN: u8 = 18;
const REPLICA_BAD: u8 = 19;
const SNAPSHOT: u8 = 20;
const FREEZE: u8 = 21;
const CLONE: u8 = 22;
const COPY_UP: u8 = 23;
const COPY_DONE: u8 = 24;
const REATTACH: u8 = 25;
const REPLICA_DOWN_AT: u8 = 26;
const DETACH_TOKEN: u8 = 27;
const DEVICE_FORMATTED: u8 = 28;

const WRITABLE: u8 = 1;
const FORCE: u8 = 2;

impl Command {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Command::Init { cluster, name, host, member } => {
                w.u8(INIT);
                w.fixed(cluster);
                w.str(name);
                w.str(host);
                w.u64(*member);
            }
            Command::Add { host, member } => {
                w.u8(ADD);
                w.str(host);
                w.u64(*member);
            }
            Command::Remove { host } => {
                w.u8(REMOVE);
                w.str(host);
            }
            Command::DeviceAdd { host, ident, slots } => {
                w.u8(DEVICE_ADD);
                w.str(host);
                w.str(ident);
                w.u32(*slots);
            }
            Command::DeviceDrain { host, ident } => {
                w.u8(DEVICE_DRAIN);
                w.str(host);
                w.str(ident);
            }
            Command::DeviceFormatted { device, format } => {
                w.u8(DEVICE_FORMATTED);
                w.u32(*device);
                w.u64(*format);
            }
            Command::VolumeCreate { name, size_mib, host } => {
                w.u8(VOLUME_CREATE);
                w.str(name);
                w.u64(*size_mib);
                w.str(host);
            }
            Command::VolumeDelete { name } => {
                w.u8(VOLUME_DELETE);
                w.str(name);
            }
            Command::VolumeResize { name, size_mib } => {
                w.u8(VOLUME_RESIZE);
                w.str(name);
                w.u64(*size_mib);
            }
            Command::Attach { name, host, writable, force, token } => {
                w.u8(ATTACH);
                w.str(name);
                w.str(host);
                w.u8(if *writable { WRITABLE } else { 0 } | if *force { FORCE } else { 0 });
                w.u64(*token);
            }
            Command::Detach { name, host, generation, token } => {
                w.u8(if *token != 0 { DETACH_TOKEN } else { DETACH });
                w.str(name);
                w.str(host);
                w.u64(*generation);
                if *token != 0 {
                    w.u64(*token);
                }
            }
            Command::Digest { at, digest } => {
                w.u8(DIGEST);
                w.u64(*at);
                w.fixed(digest);
            }
            Command::ReplicaDown { set, cs, refused, name, generation, version } => {
                w.u8(if version.is_some() { REPLICA_DOWN_AT } else { REPLICA_DOWN });
                w.u32(*set);
                w.u32(*cs);
                w.u8(u8::from(*refused));
                w.str(name);
                w.u64(*generation);
                if let Some(v) = version {
                    w.u32(*v);
                }
            }
            Command::CsDown { cs } => {
                w.u8(CS_DOWN);
                w.u32(*cs);
            }
            Command::CsUp { cs } => {
                w.u8(CS_UP);
                w.u32(*cs);
            }
            Command::CsOut { cs } => {
                w.u8(CS_OUT);
                w.u32(*cs);
            }
            Command::Join { set, place, cs, version } => {
                w.u8(JOIN);
                w.u32(*set);
                w.u8(*place);
                w.u32(*cs);
                w.u32(*version);
            }
            Command::Joined { set, cs, since } => {
                w.u8(JOINED);
                w.u32(*set);
                w.u32(*cs);
                w.u32(*since);
            }
            Command::Unjoin { set, cs, since } => {
                w.u8(UNJOIN);
                w.u32(*set);
                w.u32(*cs);
                w.u32(*since);
            }
            Command::ReplicaBad { set, cs, version } => {
                w.u8(REPLICA_BAD);
                w.u32(*set);
                w.u32(*cs);
                w.u32(*version);
            }
            Command::Snapshot { volume, name, generation } => {
                w.u8(SNAPSHOT);
                w.str(volume);
                w.str(name);
                w.u64(*generation);
            }
            Command::Freeze { name } => {
                w.u8(FREEZE);
                w.str(name);
            }
            Command::Clone { from, name, host } => {
                w.u8(CLONE);
                w.str(from);
                w.str(name);
                w.str(host);
            }
            Command::CopyUp { name, place, generation } => {
                w.u8(COPY_UP);
                w.str(name);
                w.u32(*place);
                w.u64(*generation);
            }
            Command::Reattach { name, host, generation, token } => {
                w.u8(REATTACH);
                w.str(name);
                w.str(host);
                w.u64(*generation);
                w.u64(*token);
            }
            Command::CopyDone { name, place, generation, chunk, incarnation } => {
                w.u8(COPY_DONE);
                w.str(name);
                w.u32(*place);
                w.u64(*generation);
                w.u32(*chunk);
                w.u32(*incarnation);
            }
        }
        w.buf
    }

    /// The command `bytes` are, read whole: None for anything else.
    pub fn decode(bytes: &[u8]) -> Option<Command> {
        let mut r = Reader::new(bytes);
        let host = |r: &mut Reader| r.str(ipcwire::HOST_MAX).map(String::from);
        let volume = |r: &mut Reader| r.str(crate::VOLUME_NAME_MAX).map(String::from);
        let cmd = match r.u8()? {
            INIT => {
                let cluster = r.fixed()?;
                let name = String::from(r.str(crate::NAME_MAX)?);
                let host = String::from(r.str(ipcwire::HOST_MAX)?);
                let member = r.u64()?;
                Command::Init { cluster, name, host, member }
            }
            ADD => {
                let host = String::from(r.str(ipcwire::HOST_MAX)?);
                let member = r.u64()?;
                Command::Add { host, member }
            }
            REMOVE => Command::Remove { host: String::from(r.str(ipcwire::HOST_MAX)?) },
            DEVICE_ADD => {
                let host = host(&mut r)?;
                let ident = String::from(r.str(crate::IDENT_MAX)?);
                Command::DeviceAdd { host, ident, slots: r.u32()? }
            }
            DEVICE_DRAIN => {
                let host = host(&mut r)?;
                Command::DeviceDrain { host, ident: String::from(r.str(crate::IDENT_MAX)?) }
            }
            DEVICE_FORMATTED => {
                let device = r.u32()?;
                Command::DeviceFormatted { device, format: r.u64()? }
            }
            VOLUME_CREATE => {
                let name = volume(&mut r)?;
                let size_mib = r.u64()?;
                Command::VolumeCreate { name, size_mib, host: host(&mut r)? }
            }
            VOLUME_DELETE => Command::VolumeDelete { name: volume(&mut r)? },
            VOLUME_RESIZE => {
                let name = volume(&mut r)?;
                Command::VolumeResize { name, size_mib: r.u64()? }
            }
            ATTACH => {
                let name = volume(&mut r)?;
                let host = host(&mut r)?;
                let flags = r.u8()?;
                if flags & !(WRITABLE | FORCE) != 0 {
                    return None;
                }
                Command::Attach { name, host, writable: flags & WRITABLE != 0, force: flags & FORCE != 0,
                                  token: r.u64()? }
            }
            code @ (DETACH | DETACH_TOKEN) => {
                let name = volume(&mut r)?;
                let host = host(&mut r)?;
                let generation = r.u64()?;
                let token = if code == DETACH_TOKEN { r.u64().filter(|&t| t != 0)? } else { 0 };
                Command::Detach { name, host, generation, token }
            }
            DIGEST => {
                let at = r.u64()?;
                Command::Digest { at, digest: r.fixed()? }
            }
            code @ (REPLICA_DOWN | REPLICA_DOWN_AT) => {
                let (set, cs) = (r.u32()?, r.u32()?);
                let refused = match r.u8()? {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let name = volume(&mut r)?;
                let generation = r.u64()?;
                let version = if code == REPLICA_DOWN_AT { Some(r.u32()?) } else { None };
                Command::ReplicaDown { set, cs, refused, name, generation, version }
            }
            CS_DOWN => Command::CsDown { cs: r.u32()? },
            CS_UP => Command::CsUp { cs: r.u32()? },
            CS_OUT => Command::CsOut { cs: r.u32()? },
            JOIN => {
                let (set, place) = (r.u32()?, r.u8()?);
                let cs = r.u32()?;
                Command::Join { set, place, cs, version: r.u32()? }
            }
            JOINED => {
                let (set, cs) = (r.u32()?, r.u32()?);
                Command::Joined { set, cs, since: r.u32()? }
            }
            UNJOIN => {
                let (set, cs) = (r.u32()?, r.u32()?);
                Command::Unjoin { set, cs, since: r.u32()? }
            }
            REPLICA_BAD => {
                let (set, cs) = (r.u32()?, r.u32()?);
                Command::ReplicaBad { set, cs, version: r.u32()? }
            }
            SNAPSHOT => {
                let volume = volume(&mut r)?;
                let name = String::from(r.str(crate::VOLUME_NAME_MAX)?);
                Command::Snapshot { volume, name, generation: r.u64()? }
            }
            FREEZE => Command::Freeze { name: volume(&mut r)? },
            CLONE => {
                let from = volume(&mut r)?;
                let name = String::from(r.str(crate::VOLUME_NAME_MAX)?);
                Command::Clone { from, name, host: host(&mut r)? }
            }
            COPY_UP => {
                let name = volume(&mut r)?;
                let place = r.u32()?;
                Command::CopyUp { name, place, generation: r.u64()? }
            }
            COPY_DONE => {
                let name = volume(&mut r)?;
                let place = r.u32()?;
                let (generation, chunk) = (r.u64()?, r.u32()?);
                Command::CopyDone { name, place, generation, chunk, incarnation: r.u32()? }
            }
            REATTACH => {
                let (name, host) = (volume(&mut r)?, host(&mut r)?);
                Command::Reattach { name, host, generation: r.u64()?, token: r.u64()? }
            }
            _ => return None,
        };
        r.done().then_some(cmd)
    }
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Bytes that are no command.
    Malformed,
    /// No NBS cluster yet: `Init` alone is taken.
    NotInitialized,
    /// `Init` once there is a cluster.
    Initialized,
    /// A cluster's or a host's name that is none.
    BadName,
    /// Member 0, which names none.
    BadMember,
    /// The host is a member already, as another incarnation: one whose state
    /// was lost, if it says it is a new one -- `nbs host replace`.
    HostIsMember,
    /// An incarnation another host's member is.
    MemberInUse,
    /// An incarnation removed: never taken again, as a member that forgot
    /// its log must never vote as the one that had it.
    MemberRemoved,
    /// As many members as a cluster takes, or as many removed as it keeps.
    Full,
    /// No member of that host, now or before.
    NoSuchHost,
    /// The cluster's last member.
    LastMember,
    /// A device of a host that is no member.
    NotAMember,
    /// Not a device's id, or a device of no slots or of more than are kept.
    BadDevice,
    /// The device is another host's -- a disk moved between servers.
    DeviceElsewhere,
    /// The device is this host's already, of another size.
    DeviceChanged,
    /// No such device on the host.
    NoSuchDevice,
    /// Not a volume's name, or a size of none or past the most.
    BadVolume,
    /// A volume of that name is there already, of another size or kind.
    NameTaken,
    /// No volume of that name.
    NoSuchVolume,
    /// Not room for its chunks on three hosts.
    NoRoom,
    /// Fewer than three hosts take new chunks: members with an active device.
    TooFewHosts,
    /// It would leave too little room to rebuild the biggest host.
    NoRoomToRebuild,
    /// A volume is never made smaller.
    Shrink,
    /// The volume is attached: detached first.
    Attached,
    /// Attached for writing on another host: `force` takes it.
    AttachedElsewhere,
    /// Attached read-only somewhere: `force` takes it.
    ReadersAttached,
    /// Attached for writing: no reader with it.
    Writing,
    /// An image or a snapshot: read-only.
    ReadOnly,
    /// As many as the metadata keeps -- volumes, places of maps, chunks,
    /// devices, chunk servers, readers -- or a number at its end.
    TooMany,
    /// No such replica set, or the cs is no replica of it.
    NoSuchSet,
    /// The set's last active replica: writes go on to it alone.
    LastReplica,
    /// Not the volume's writer: attached elsewhere since, or never.
    NotTheWriter,
    /// No chunk server of that number.
    NoSuchCs,
    /// Asked of what has moved on since: the set's version, its join, a
    /// copy-up's pending chunk, or a session that let the volume go.
    Stale,
    /// A join of the set is under way already.
    Joining,
    /// The member is active: it needs no join of its own.
    NotDown,
    /// The cs is a member of the set already.
    AlreadyMember,
    /// The cs is not up: it joins nothing.
    NotUp,
    /// The member in that place stays: active, or down with its cs neither
    /// out nor gone -- its place is another's only from a cs out, gone or
    /// drained.
    InPlace,
    /// The member is down already: the controller has it join its place
    /// again.
    AlreadyDown,
    /// A clone is of an image or a snapshot, not of a volume (N10).
    NotFrozen,
    /// A copy-up into a set with a member drained, whose place is to be
    /// joined by another cs first (N10).
    Draining,
    /// As many copy-ups pending as the metadata keeps: asked again in a
    /// moment, as they end (the review of 2026-11: said as the bound of
    /// volumes, chunks, devices, cs's and readers, it failed the write).
    CopiesPending,
    /// Another format of the device's is recorded: formatted again since,
    /// which only a wipe lets it be (`DeviceFormatted`).
    OtherFormat,
}

impl Refusal {
    pub fn says(self) -> &'static str {
        match self {
            Refusal::Malformed => "bytes that are no command",
            Refusal::NotInitialized => "no NBS cluster yet -- `nbs init` makes one",
            Refusal::Initialized => "the NBS cluster is made already",
            Refusal::BadName => "not a name: lowercase letters, digits and -, up to 63",
            Refusal::BadMember => "member 0 is none",
            Refusal::HostIsMember => "the host is a member already, as another incarnation -- if its state was lost, \
                                      `nbs host replace` it",
            Refusal::MemberInUse => "another host's member is that incarnation",
            Refusal::MemberRemoved => "that incarnation was removed: never taken again",
            Refusal::Full => "as many members as an NBS cluster takes, or as many removed as it keeps",
            Refusal::NoSuchHost => "no member of that host",
            Refusal::LastMember => "the NBS cluster's last member",
            Refusal::NotAMember => "the host -- the one named, or the chunk server's -- is no member of the NBS \
                                    cluster",
            Refusal::BadDevice => "not a device's id (part:..., disk:...), or a device of no slots",
            Refusal::DeviceElsewhere => "the device is another host's in the NBS cluster -- a disk moved between servers",
            Refusal::DeviceChanged => "the device is this host's already, of another size",
            Refusal::NoSuchDevice => "no such device on the host",
            Refusal::BadVolume => "not a volume's name (lowercase letters, digits, . _ -, up to 63) or size (1 MiB up to \
                                   16 TiB)",
            Refusal::NameTaken => "a volume of that name is there already, of another size or kind",
            Refusal::NoSuchVolume => "no volume of that name",
            Refusal::NoRoom => "not room for its chunks on three hosts",
            Refusal::TooFewHosts => "fewer than three hosts take new chunks -- `nbs devices add` on three",
            Refusal::NoRoomToRebuild => "it would leave too little room to rebuild the biggest host",
            Refusal::Shrink => "a volume is never made smaller",
            Refusal::Attached => "the volume is attached",
            Refusal::AttachedElsewhere => "the volume is attached for writing on another host -- `force` takes it",
            Refusal::ReadersAttached => "the volume is attached read-only -- `force` takes it",
            Refusal::Writing => "the volume is attached for writing",
            Refusal::ReadOnly => "an image or a snapshot is read-only",
            Refusal::TooMany => "as many as the metadata keeps -- volumes, places of maps, chunks, devices, chunk \
                                 servers, readers -- or a number at its end",
            Refusal::NoSuchSet => "no such replica set, or the chunk server is no replica of it",
            Refusal::LastReplica => "the set's last active replica: writes go on to it alone",
            Refusal::NotTheWriter => "not the volume's writer: attached elsewhere since",
            Refusal::NoSuchCs => "no chunk server of that number",
            Refusal::Stale => "asked of what has moved on since: the set's version, its join, a copy-up's pending \
                               chunk, or a session that let the volume go",
            Refusal::Joining => "a join of the set is under way already",
            Refusal::NotDown => "the member is active: it needs no join",
            Refusal::AlreadyMember => "the chunk server is a member of the set already",
            Refusal::NotUp => "the chunk server is not up",
            Refusal::InPlace => "the member in that place stays: its place is another's only from a chunk server \
                                 out, gone or drained",
            Refusal::AlreadyDown => "the member is down already: it joins its place again",
            Refusal::NotFrozen => "a clone is of an image or a snapshot -- freeze or snapshot the volume first",
            Refusal::Draining => "a member of the set is drained: its place is to be joined by another cs first",
            Refusal::CopiesPending => "as many copy-ups pending as the metadata keeps -- asked again as they end",
            Refusal::OtherFormat => "another format of the device's is recorded -- `nbs devices add ... wipe` takes \
                                     it anew",
        }
    }

    fn code(self) -> u8 {
        match self {
            Refusal::Malformed => 1,
            Refusal::NotInitialized => 2,
            Refusal::Initialized => 3,
            Refusal::BadName => 4,
            Refusal::BadMember => 5,
            Refusal::HostIsMember => 6,
            Refusal::MemberInUse => 7,
            Refusal::MemberRemoved => 8,
            Refusal::Full => 9,
            Refusal::NoSuchHost => 10,
            Refusal::LastMember => 11,
            Refusal::NotAMember => 12,
            Refusal::BadDevice => 13,
            Refusal::DeviceElsewhere => 14,
            Refusal::DeviceChanged => 15,
            Refusal::NoSuchDevice => 16,
            Refusal::BadVolume => 17,
            Refusal::NameTaken => 18,
            Refusal::NoSuchVolume => 19,
            Refusal::NoRoom => 20,
            Refusal::TooFewHosts => 21,
            Refusal::NoRoomToRebuild => 22,
            Refusal::Shrink => 23,
            Refusal::Attached => 24,
            Refusal::AttachedElsewhere => 25,
            Refusal::ReadersAttached => 26,
            Refusal::Writing => 27,
            Refusal::ReadOnly => 28,
            Refusal::TooMany => 29,
            Refusal::NoSuchSet => 30,
            Refusal::LastReplica => 31,
            Refusal::NotTheWriter => 32,
            Refusal::NoSuchCs => 33,
            Refusal::Stale => 34,
            Refusal::Joining => 35,
            Refusal::NotDown => 36,
            Refusal::AlreadyMember => 37,
            Refusal::NotUp => 38,
            Refusal::InPlace => 39,
            Refusal::AlreadyDown => 40,
            Refusal::NotFrozen => 41,
            Refusal::Draining => 42,
            Refusal::CopiesPending => 43,
            Refusal::OtherFormat => 44,
        }
    }

    fn from_code(c: u8) -> Option<Refusal> {
        Some(match c {
            1 => Refusal::Malformed,
            2 => Refusal::NotInitialized,
            3 => Refusal::Initialized,
            4 => Refusal::BadName,
            5 => Refusal::BadMember,
            6 => Refusal::HostIsMember,
            7 => Refusal::MemberInUse,
            8 => Refusal::MemberRemoved,
            9 => Refusal::Full,
            10 => Refusal::NoSuchHost,
            11 => Refusal::LastMember,
            12 => Refusal::NotAMember,
            13 => Refusal::BadDevice,
            14 => Refusal::DeviceElsewhere,
            15 => Refusal::DeviceChanged,
            16 => Refusal::NoSuchDevice,
            17 => Refusal::BadVolume,
            18 => Refusal::NameTaken,
            19 => Refusal::NoSuchVolume,
            20 => Refusal::NoRoom,
            21 => Refusal::TooFewHosts,
            22 => Refusal::NoRoomToRebuild,
            23 => Refusal::Shrink,
            24 => Refusal::Attached,
            25 => Refusal::AttachedElsewhere,
            26 => Refusal::ReadersAttached,
            27 => Refusal::Writing,
            28 => Refusal::ReadOnly,
            29 => Refusal::TooMany,
            30 => Refusal::NoSuchSet,
            31 => Refusal::LastReplica,
            32 => Refusal::NotTheWriter,
            33 => Refusal::NoSuchCs,
            34 => Refusal::Stale,
            35 => Refusal::Joining,
            36 => Refusal::NotDown,
            37 => Refusal::AlreadyMember,
            38 => Refusal::NotUp,
            39 => Refusal::InPlace,
            40 => Refusal::AlreadyDown,
            41 => Refusal::NotFrozen,
            42 => Refusal::Draining,
            43 => Refusal::CopiesPending,
            44 => Refusal::OtherFormat,
            _ => return None,
        })
    }
}

/// What applying a command gave, or why it was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A member made, taken or removed: its incarnation.
    Done { member: u64 },
    /// A device added or drained: its number in the metadata.
    Device { id: u32 },
    /// A volume made, grown or deleted: its number -- 0 for a delete of one
    /// that was gone.
    Volume { id: u32 },
    /// An attach's generation; a detach's, or 0 where it let go of nothing --
    /// and 0 for a reader's either way. `first`: the attach is the first for
    /// writing of a volume made empty -- no clone -- so that everything of it
    /// reads as zeros, which a pour leaves out.
    Attached { generation: u64, first: bool },
    /// A digest held to.
    Checked,
    /// A replica taken out, a join begun, done or given up: its set's
    /// version now.
    Set { version: u32 },
    /// A cs down, up or out: how many sets that changed.
    Changed { sets: u32 },
    /// A copy-up's chunk (N10): the pending one, or -- done, or the chunk
    /// shared no longer -- the one the map names; and its incarnation.
    Chunk { chunk: u32, incarnation: u32 },
    Refused(Refusal),
}

const DONE: u8 = 0;
const REFUSED: u8 = 1;
const DEVICE: u8 = 2;
const VOLUME: u8 = 3;
const ATTACHED: u8 = 4;
const CHECKED: u8 = 5;
const SET: u8 = 6;
const CHANGED: u8 = 7;
const CHUNK: u8 = 8;
const ATTACHED_FIRST: u8 = 9;

impl Answer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Answer::Done { member } => {
                w.u8(DONE);
                w.u64(*member);
            }
            Answer::Refused(why) => {
                w.u8(REFUSED);
                w.u8(why.code());
            }
            Answer::Device { id } => {
                w.u8(DEVICE);
                w.u32(*id);
            }
            Answer::Volume { id } => {
                w.u8(VOLUME);
                w.u32(*id);
            }
            Answer::Attached { generation, first } => {
                w.u8(if *first { ATTACHED_FIRST } else { ATTACHED });
                w.u64(*generation);
            }
            Answer::Checked => w.u8(CHECKED),
            Answer::Set { version } => {
                w.u8(SET);
                w.u32(*version);
            }
            Answer::Changed { sets } => {
                w.u8(CHANGED);
                w.u32(*sets);
            }
            Answer::Chunk { chunk, incarnation } => {
                w.u8(CHUNK);
                w.u32(*chunk);
                w.u32(*incarnation);
            }
        }
        w.buf
    }

    pub fn decode(bytes: &[u8]) -> Option<Answer> {
        let mut r = Reader::new(bytes);
        let a = match r.u8()? {
            DONE => Answer::Done { member: r.u64()? },
            REFUSED => Answer::Refused(Refusal::from_code(r.u8()?)?),
            DEVICE => Answer::Device { id: r.u32()? },
            VOLUME => Answer::Volume { id: r.u32()? },
            ATTACHED => Answer::Attached { generation: r.u64()?, first: false },
            ATTACHED_FIRST => Answer::Attached { generation: r.u64()?, first: true },
            CHECKED => Answer::Checked,
            SET => Answer::Set { version: r.u32()? },
            CHANGED => Answer::Changed { sets: r.u32()? },
            CHUNK => {
                let chunk = r.u32()?;
                Answer::Chunk { chunk, incarnation: r.u32()? }
            }
            _ => return None,
        };
        r.done().then_some(a)
    }
}
