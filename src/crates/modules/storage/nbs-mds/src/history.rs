//! NBS's metadata as the member keeps it (`Meta`): nbsmeta's state, each
//! command applied to it said in NBS's file log, `/logs/nbs.log`
//! (docs/logs.md) -- a volume made, attached, snapshotted or deleted, a
//! replica taken out and by whom, a cs down, up or out, a join begun, done
//! or given up, a device added -- when it was applied here. Every member
//! applies every command, so every host of the cluster has the same history
//! in its own file; a command refused is said too, through a limiter, since
//! a client may ask again and again.
//!
//! Not said: the digests the leader proposes, and a copy-up's two steps --
//! one a chunk a clone's writer first writes, as many as the volume has
//! chunks. Nor what the member applies again as it starts (`catch_up`),
//! which was said as it was first applied: `saying` is turned on once that
//! is done. An entry applied before a restart, but not yet known committed
//! when the log was last written, may be said a second time after it.

use alloc::vec::Vec;
use core::ops::Deref;
use core::sync::atomic::{AtomicBool, Ordering};

use nbsmeta::{Answer, Command, State};
use raft::rsm::{RestoreError, SnapshotReader, StateMachine};

/// NBS's file log: the metadata's history here, NBS's controller and scrub,
/// and what this host's cs and clients met (nbs-cs's and nbsdrive's lines,
/// in the same file).
pub static NBS: kcore::logs::Log = kcore::logs::Log::new("nbs");

/// Whether commands applied are said: from the end of the member's start,
/// its log applied as far as it had got.
static SAYING: AtomicBool = AtomicBool::new(false);

/// Commands applied said from now on, or not.
pub fn saying(on: bool) {
    SAYING.store(on, Ordering::Release);
}

/// NBS's metadata, the member's state machine: nbsmeta's state, read through
/// as it is (`Deref`), each command applied to it said (`say`).
#[derive(Clone, Default)]
pub struct Meta(State);

impl Deref for Meta {
    type Target = State;

    fn deref(&self) -> &State {
        &self.0
    }
}

impl StateMachine for Meta {
    type Frozen = <State as StateMachine>::Frozen;

    fn apply(&mut self, index: u64, command: &[u8]) -> Vec<u8> {
        let answer = self.0.apply(index, command);
        if SAYING.load(Ordering::Acquire) {
            if let (Some(c), Some(a)) = (Command::decode(command), Answer::decode(&answer)) {
                say(&self.0, index, &c, &a);
            }
        }
        answer
    }

    fn query(&self, query: &[u8]) -> Vec<u8> {
        self.0.query(query)
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn freeze(&self) -> Self::Frozen {
        self.0.freeze()
    }

    fn restore(&mut self, r: &mut SnapshotReader<'_>) -> Result<(), RestoreError> {
        let restored = self.0.restore(r);
        if restored.is_ok() && SAYING.load(Ordering::Acquire) {
            kcore::flog!(NBS, "metadata taken whole from the leader's snapshot: what changed meanwhile is not said \
                               here, line by line");
        }
        restored
    }
}

/// A cs as a line says it: its number, and its host's name where the state
/// has it.
struct CsOf<'a>(&'a State, u32);

impl core::fmt::Display for CsOf<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0.cs().get(self.1 as usize) {
            Some(c) => write!(f, "cs {} ({})", self.1, c.host),
            None => write!(f, "cs {}", self.1),
        }
    }
}

/// Command `c`, applied at entry `index` of `s` and answered `a`, said in
/// the log: what it did, or why it was refused.
fn say(s: &State, index: u64, c: &Command, a: &Answer) {
    if let Answer::Refused(why) = a {
        match c {
            Command::Digest { .. } | Command::CopyUp { .. } | Command::CopyDone { .. } => {}
            _ => kcore::flog_limited!(NBS, "refused at {}: {} -- {}", index, Asked(c), why.says()),
        }
        return;
    }
    match (c, a) {
        (Command::Init { name, host, .. }, _) => {
            kcore::flog!(NBS, "the NBS cluster {} made, {}'s mds its first member", name, host);
        }
        (Command::Add { host, member }, _) => kcore::flog!(NBS, "{} a member, as {:x}", host, member),
        (Command::Remove { host }, _) => kcore::flog!(NBS, "{} a member no more", host),
        (Command::DeviceAdd { host, ident, slots }, Answer::Device { id }) => {
            kcore::flog!(NBS, "device {} ({} of {}) added: {} chunks of room", id, ident, host, slots);
        }
        (Command::DeviceDrain { host, ident }, Answer::Device { id }) => {
            kcore::flog!(NBS, "device {} ({} of {}) drained: no new chunks", id, ident, host);
        }
        (Command::DeviceFormatted { device, format: 0 }, _) => {
            kcore::flog!(NBS, "device {}'s format dropped: wiped, to be formatted anew", device);
        }
        (Command::DeviceFormatted { device, format }, _) => {
            kcore::flog!(NBS, "device {}'s format recorded: {:016x}", device, format);
        }
        (Command::VolumeCreate { name, size_mib, host }, Answer::Volume { id }) => {
            kcore::flog!(NBS, "volume {} ({}) made, {} MiB, for {}", name, id, size_mib, host);
        }
        (Command::VolumeDelete { name }, Answer::Volume { id: 0 }) => {
            kcore::flog!(NBS, "volume {} deleted: it was gone", name);
        }
        (Command::VolumeDelete { name }, Answer::Volume { id }) => {
            kcore::flog!(NBS, "volume {} ({}) deleted", name, id);
        }
        (Command::VolumeResize { name, size_mib }, _) => {
            kcore::flog!(NBS, "volume {} grown to {} MiB", name, size_mib);
        }
        (Command::Attach { name, host, writable: true, force, .. }, Answer::Attached { generation, .. }) => {
            kcore::flog!(NBS, "volume {} attached for writing on {}, generation {}{}", name, host, generation,
                         if *force { ", taken by force" } else { "" });
        }
        (Command::Attach { name, host, .. }, _) => kcore::flog!(NBS, "volume {} attached for reading on {}", name, host),
        (Command::Detach { name, host, generation: 0, .. }, _) => {
            kcore::flog!(NBS, "volume {} let go of by its reader on {}", name, host);
        }
        (Command::Detach { name, host, generation, .. }, _) => {
            kcore::flog!(NBS, "volume {} let go of by its writer on {}, generation {}", name, host, generation);
        }
        (Command::Reattach { name, host, generation, .. }, Answer::Attached { generation: now, .. }) => {
            kcore::flog!(NBS, "volume {}'s writer on {} attached anew: generation {} after {}", name, host, now,
                         generation);
        }
        (Command::ReplicaDown { set, cs, refused, name, generation, .. }, Answer::Set { version }) => {
            kcore::flog!(NBS, "set {}: {} taken out by volume {}'s writer, generation {} -- {}; the set's version {}",
                         set, CsOf(s, *cs), name, generation,
                         if *refused { "it refused as failed" } else { "it did not answer" }, version);
        }
        (Command::CsDown { cs }, Answer::Changed { sets }) => {
            kcore::flog!(NBS, "{} down: silent to the leader, {} set{} changed", CsOf(s, *cs), sets,
                         if *sets == 1 { "" } else { "s" });
        }
        (Command::CsUp { cs }, Answer::Changed { sets }) => {
            kcore::flog!(NBS, "{} up: answering the leader again, {} set{} changed", CsOf(s, *cs), sets,
                         if *sets == 1 { "" } else { "s" });
        }
        (Command::CsOut { cs }, Answer::Changed { sets }) => {
            kcore::flog!(NBS, "{} out: its places in {} set{} to be taken by others", CsOf(s, *cs), sets,
                         if *sets == 1 { "" } else { "s" });
        }
        (Command::Join { set, place, cs, .. }, Answer::Set { version }) => {
            kcore::flog!(NBS, "set {}: {} joins in place {}, the set's version {}", set, CsOf(s, *cs), place,
                         version);
        }
        (Command::Joined { set, cs, .. }, Answer::Set { version }) => {
            kcore::flog!(NBS, "set {}: {} joined -- its copy done, a member; the set's version {}", set,
                         CsOf(s, *cs), version);
        }
        (Command::Unjoin { set, cs, .. }, Answer::Set { version }) => {
            kcore::flog!(NBS, "set {}: the join of {} given up; the set's version {}", set, CsOf(s, *cs), version);
        }
        (Command::ReplicaBad { set, cs, .. }, Answer::Set { version }) => {
            kcore::flog!(NBS, "set {}: {} taken down, its copy found bad; the set's version {}", set, CsOf(s, *cs),
                         version);
        }
        (Command::Snapshot { volume, name, .. }, _) => {
            kcore::flog!(NBS, "snapshot {} of volume {} taken", name, volume);
        }
        (Command::Freeze { name }, _) => kcore::flog!(NBS, "volume {} made an image: read-only, to be cloned", name),
        (Command::Clone { from, name, host }, _) => {
            kcore::flog!(NBS, "volume {} made, a clone of {}, for {}", name, from, host);
        }
        /* The digests, the copy-ups: not history a person reads */
        _ => {}
    }
}

/// A command as a refusal says what was asked.
struct Asked<'a>(&'a Command);

impl core::fmt::Display for Asked<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Command::Init { name, .. } => write!(f, "the NBS cluster {} made", name),
            Command::Add { host, .. } => write!(f, "{} a member", host),
            Command::Remove { host } => write!(f, "{} out of the members", host),
            Command::DeviceAdd { host, ident, .. } => write!(f, "device {} of {} added", ident, host),
            Command::DeviceDrain { host, ident } => write!(f, "device {} of {} drained", ident, host),
            Command::DeviceFormatted { device, .. } => write!(f, "device {}'s format recorded", device),
            Command::VolumeCreate { name, .. } => write!(f, "volume {} made", name),
            Command::VolumeDelete { name } => write!(f, "volume {} deleted", name),
            Command::VolumeResize { name, .. } => write!(f, "volume {} grown", name),
            Command::Attach { name, host, .. } => write!(f, "volume {} attached on {}", name, host),
            Command::Detach { name, host, .. } => write!(f, "volume {} let go of on {}", name, host),
            Command::Reattach { name, host, .. } => write!(f, "volume {} attached anew on {}", name, host),
            Command::ReplicaDown { set, cs, name, .. } => {
                write!(f, "cs {} taken out of set {} by volume {}'s writer", cs, set, name)
            }
            Command::CsDown { cs } => write!(f, "cs {} down", cs),
            Command::CsUp { cs } => write!(f, "cs {} up", cs),
            Command::CsOut { cs } => write!(f, "cs {} out", cs),
            Command::Join { set, cs, .. } => write!(f, "cs {} joining set {}", cs, set),
            Command::Joined { set, cs, .. } => write!(f, "cs {}'s join of set {} done", cs, set),
            Command::Unjoin { set, cs, .. } => write!(f, "cs {}'s join of set {} given up", cs, set),
            Command::ReplicaBad { set, cs, .. } => write!(f, "cs {} of set {} taken down as bad", cs, set),
            Command::Snapshot { volume, name, .. } => write!(f, "snapshot {} of volume {}", name, volume),
            Command::Freeze { name } => write!(f, "volume {} made an image", name),
            Command::Clone { from, name, .. } => write!(f, "volume {} cloned from {}", name, from),
            Command::Digest { .. } => write!(f, "a digest"),
            Command::CopyUp { name, place, .. } | Command::CopyDone { name, place, .. } => {
                write!(f, "volume {}'s place {} copied up", name, place)
            }
        }
    }
}
