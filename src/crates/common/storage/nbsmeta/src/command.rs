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
    /// has it.
    Attach { name: String, host: String, writable: bool, force: bool },
    /// Volume `name` let go by `host`: the writer of `generation`, or -- with
    /// generation 0 -- a reader. Done where it is not so attached.
    Detach { name: String, host: String, generation: u64 },
    /// The leader's digest of the state at entry `at`, for every member to
    /// hold its own to.
    Digest { at: u64, digest: [u8; 32] },
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
            Command::Attach { name, host, writable, force } => {
                w.u8(ATTACH);
                w.str(name);
                w.str(host);
                w.u8(if *writable { WRITABLE } else { 0 } | if *force { FORCE } else { 0 });
            }
            Command::Detach { name, host, generation } => {
                w.u8(DETACH);
                w.str(name);
                w.str(host);
                w.u64(*generation);
            }
            Command::Digest { at, digest } => {
                w.u8(DIGEST);
                w.u64(*at);
                w.fixed(digest);
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
                Command::Attach { name, host, writable: flags & WRITABLE != 0, force: flags & FORCE != 0 }
            }
            DETACH => {
                let name = volume(&mut r)?;
                let host = host(&mut r)?;
                Command::Detach { name, host, generation: r.u64()? }
            }
            DIGEST => {
                let at = r.u64()?;
                Command::Digest { at, digest: r.fixed()? }
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
    /// As many volumes, chunks, devices, chunk servers or readers as the
    /// metadata keeps.
    TooMany,
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
            Refusal::NotAMember => "the host is no member of the NBS cluster",
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
            Refusal::TooMany => "as many volumes, chunks, devices, chunk servers or readers as the metadata keeps",
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
    /// and 0 for a reader's either way.
    Attached { generation: u64 },
    /// A digest held to.
    Checked,
    Refused(Refusal),
}

const DONE: u8 = 0;
const REFUSED: u8 = 1;
const DEVICE: u8 = 2;
const VOLUME: u8 = 3;
const ATTACHED: u8 = 4;
const CHECKED: u8 = 5;

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
            Answer::Attached { generation } => {
                w.u8(ATTACHED);
                w.u64(*generation);
            }
            Answer::Checked => w.u8(CHECKED),
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
            ATTACHED => Answer::Attached { generation: r.u64()? },
            CHECKED => Answer::Checked,
            _ => return None,
        };
        r.done().then_some(a)
    }
}
