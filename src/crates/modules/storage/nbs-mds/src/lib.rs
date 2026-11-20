#![no_std]

//! nbs-mds: NBS's metadata service on this host (plans/nbs.md, N5 and N6) --
//! the state every member holds alike, through raft over ipc, of which NBS's
//! hosts are its members: the NBS cluster's identity and its members, each
//! host's chunk server and its devices, the volumes and where each chunk of
//! them is (nbsmeta).
//!
//!     insmod /lib/modules/ipcnet.ko, then cluster.ko: the hosts that trust each other
//!     insmod /lib/modules/nbs-mds.ko [dir=/nbs/mds] [compact=<bytes>] [out_after=<seconds>] [scrub=<seconds>]
//!     nbs init <name>            on the first host: an NBS cluster of it alone
//!     nbs host add <host>        on a member: <host> -- a host of the registry -- a member
//!     nbs host remove <host>     on a member: <host>'s member out, for good
//!     nbs host replace <host>    on a member: a host that lost its state, back as a new member
//!     nbs hosts                  every member: its incarnation, its vote, the leader
//!     nbs recover [<name>]       on the last member standing: what it does, then -- the NBS
//!                                cluster's name given -- this host's member the one voter
//!     nbs                        this host's member: role, term, leader, what it applied
//!     nbs devices add <id|name> [wipe]   a device of this host's for its chunk server; wipe: whatever is
//!                                on it -- or a device its cs found blank where it had formatted it
//!     nbs devices drain <id>     given no new chunks
//!     nbs replica bad <set> <host>   <host>'s cs taken down in <set>, for its join back to copy it again
//!     nbs cs                     every host's chunk server: its devices and room
//!     nbs volume create <name> <size> [host=<h>], delete <name>, resize <name> <size>
//!     nbs volumes, nbs volume <name>
//!     nbs io attach|write|read|flush|close|held|detach <volume> ...   its bytes, through the cs's (io.rs)
//!
//! The same is on the management bus as the `nbs` provider (provider.rs). A
//! device is recorded here, and written by its host's cs (nbs-cs.ko), which
//! asks this host's mds -- this module's member -- what it is to take in, and
//! whether a request it has not checked is the metadata's (member.rs).
//! `nbs io` is a client of the cs's, for the shell and the tests.
//!
//! `compact=` is how many bytes of entries the log holds past its snapshot
//! before it is compacted into a new one -- or the state's size, should that
//! be more: raft's 1 MiB unless given. A test gives it a few, so that a
//! member started again reads its state back from a snapshot, and a member
//! added is sent one.
//!
//! `scrub=` is the leader's scrub's pace (N9e): a chunk every so many
//! seconds, ten unless given, 0 for none -- each look reads the chunk whole on
//! its set's every member it is closed on. What it finds it says, in the log
//! and in `nbs status`, and takes nothing down: no checksum of its own data
//! does a cs keep, and two members alike are no proof which one holds the
//! data. `nbs replica bad` is a person's.
//!
//! Who the members are is two things (plans/nbs.md, "Who is in the cluster"):
//! the hosts that may talk at all are the cluster registry's, ipcnet pinning
//! each by its key; the members are the NBS cluster's Raft configuration,
//! each a host and the incarnation its mds made when it was offered the
//! cluster -- 64 random bits, never made again. A host whose `dir` was lost
//! comes back with none and is offered the cluster as a new member, never as
//! the one it was: Raft never counts the vote of a member that forgot its
//! log.
//!
//! While this host is in no NBS cluster, a listener holds `nbs.mds`: it takes
//! an offer -- making the incarnation, the member file and an empty log, and
//! starting the member, which raft then brings the log -- and answers a raft
//! message for a member it does not have with `TAG_NO_MEMBER`, which is how a
//! leader learns that a host's state is gone. The listener, the offers, the
//! members' commands, the member's start and stop -- with the `begun` mark
//! that keeps a member whose log is gone from starting as the one it was --
//! and `recover` are raftmember's, as every group whose hosts are offered it
//! has them (`offered`); the member is raftmember's with NBS's hooks
//! (member.rs), a task of its own the shell hands its commands to.

extern crate alloc;

mod files;
mod history;
mod io;
mod member;
mod provider;
mod store;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::{Command as ShellCommand, Output};
use kcore::error::Error;
use kcore::mgmt::Provider;
use raft::Role;
use raftmember::offered::{self, MemberFile, Offered, Starter};
use raftmember::{Locked, Log, Recovered};

use member::{Mds, View};

const HELP: &str = "nbs [init | host | hosts | recover | devices | cs | volume | volumes | io] - NBS's metadata";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: nbs                          this host's member: incarnation, role, leader, what it applied\n\
                     \x20      nbs init <name>            an NBS cluster of this host alone, named <name>\n\
                     \x20      nbs hosts                  every member: incarnation, vote, the leader\n\
                     \x20      nbs host add <host>        <host>, a host of the cluster registry, a member: a learner\n\
                     \x20                                 until it holds the log, then a voter while fewer than five vote\n\
                     \x20      nbs host remove <host>     <host>'s member out, for good\n\
                     \x20      nbs host replace <host>    <host>, which lost its state, back as a new member\n\
                     \x20      nbs recover [<name>]       on the last member standing: what it does, then -- the\n\
                     \x20                                 NBS cluster's name given -- this host's member the one voter\n\
                     \x20      nbs devices add <id|name> [wipe]   a device of this host's for its chunk server: \
                     with wipe,\n\
                     \x20                                 even one with something else on it, or one its cs\n\
                     \x20                                 found blank where it had formatted it\n\
                     \x20      nbs devices drain <id>     given no new chunks\n\
                     \x20      nbs replica bad <set> <host>   <host>'s cs taken down in <set>, to copy its chunks \
                     again\n\
                     \x20                                 -- the member the scrub found unlike the two others\n\
                     \x20      nbs cs                     every host's chunk server: its devices and room\n\
                     \x20      nbs volumes                every volume\n\
                     \x20      nbs volume <name>          one, and which hosts hold its replicas\n\
                     \x20      nbs volume create <name> <size> [host=<host>]   size in MiB, or with M, G, T\n\
                     \x20      nbs volume delete <name>\n\
                     \x20      nbs volume resize <name> <size>\n\
                     \x20      nbs volume snapshot <volume> <name>   on the host it is attached on, its writer paused\n\
                     \x20      nbs volume freeze <name>   an image: read-only for good, cloned from\n\
                     \x20      nbs volume clone <image|snapshot> <name> [host=<host>]   a volume of its, thin\n\
                     \x20      nbs io write <volume> <offset> <bytes> <byte>   through the cs's; read <volume> \
                     <offset> <bytes>,\n\
                     \x20                                 attach <volume> [force], flush, close, held and detach \
                     <volume>";
pub(crate) const NO_MEMBER: &str = "this host is in no NBS cluster -- `nbs init <name>`, or `nbs host add <this host>` \
                                    on a member";

/// The leader's scrub's pace unless `scrub=` is given: a chunk every ten
/// seconds, which reads it whole on each of its set's members (N9e).
const SCRUB_MS: u64 = 10_000;

/// The module's: its folder, the member while there is one, the listener
/// while there is not (raftmember's `Offered`).
struct ModState {
    group: Offered<Mds>,
    /// The log's bytes past its snapshot before it is compacted: `compact=`.
    compact: u64,
    /// How long a cs is silent before the leader's controller has it out
    /// and its places taken by others: `out_after=`, ten minutes unless
    /// given (N9c).
    out_after_ms: u64,
    /// The leader's scrub's pace, a chunk every so long: `scrub=`, ten
    /// seconds unless given, 0 for none (N9e); and what it found.
    scrub_ms: u64,
    scrubbed: member::Scrubbed,
    /// The volumes `nbs io` attached here for writing.
    io: Locked<io::Attachments>,
}

/// What to do where this host's member's log is gone beside its `begun`
/// mark: it is never that member again.
fn lost_remedy(state: &ModState, host: &str) -> String {
    alloc::format!("on another member, `nbs host replace {}` once here `rmmod nbs-mds`, `rm {}` and `insmod` it", host,
                   state.group.dir)
}

impl Starter<Mds> for ModState {
    fn offered(&self) -> &Offered<Mds> {
        &self.group
    }

    /// The member started over its log -- `log`, where one is given, or the
    /// folder's: made new, with this host its one voter, when `fresh` --
    /// standing for election at once when `campaign`. The shell sees it
    /// before this returns.
    fn start(&self, me: MemberFile, log: Option<Log>, fresh: bool, campaign: bool) -> Result<(), String> {
        if self.group.runner.running() {
            return Err(String::from("the member is running already"));
        }
        let log = match log {
            Some(log) => log,
            None => raftmember::open_log(&self.group.dir, me.id, fresh, &lost_remedy(self, &me.host))?,
        };
        /* What its log replays as it starts was said as it was first applied:
         * the history is said from the end of that on */
        history::saying(false);
        let rsm = raftmember::member_rsm::<history::Meta>(log, me.id, fresh || campaign, Some(self.compact))?;
        history::saying(true);
        let member = member::new(rsm, me, self.group.lost.clone(), &self.group.dir, self.out_after_ms, self.scrub_ms,
                                 self.scrubbed.clone())?;
        self.group.runner.spawn(<Mds as offered::Group>::TASK, member)
    }
}

/* Dropped in this order: the command and the provider -- the loader has
   waited out every call of them before this drop, the member's task they
   hand their changes to among what such a call may have been waiting on --
   then the listener stopped and waited out, then the member's task, its log
   closed with it */
struct Nbs {
    cmd: Option<ShellCommand>,
    provider: Option<Provider>,
    state: Arc<ModState>,
}

impl kmod::Module for Nbs {}

impl Drop for Nbs {
    fn drop(&mut self) {
        drop(self.provider.take());
        drop(self.cmd.take());
        self.state.group.shut();
    }
}

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    let mut dir = None;
    let mut compact = None;
    let mut out_after = None;
    let mut scrub = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("dir", Some(path)) if dir.is_none() && path.starts_with('/') && path.len() < 200 => dir = Some(path),
            ("compact", Some(bytes)) if compact.is_none() && bytes.parse::<u64>().is_ok() => {
                compact = bytes.parse::<u64>().ok();
            }
            ("out_after", Some(secs)) if out_after.is_none() && secs.parse::<u64>().is_ok_and(|s| (10..=86_400).contains(&s)) => {
                out_after = secs.parse::<u64>().ok();
            }
            ("scrub", Some(secs)) if scrub.is_none() && secs.parse::<u64>().is_ok_and(|s| s <= 86_400) => {
                scrub = secs.parse::<u64>().ok();
            }
            _ => {
                let _ = writeln!(out, "nbs: \"{}\" is not a parameter of nbs-mds's -- dir=<folder>, \
                                       compact=<bytes>, out_after=<seconds, 10 to 86400> and scrub=<seconds a chunk, \
                                       0 for none, to 86400> are, each given once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    let state = Arc::new(ModState {
        group: Offered::new(dir.unwrap_or(files::DEFAULT_DIR)).ok_or(Error::NoMemory)?,
        compact: compact.unwrap_or(raft::rsm::SNAPSHOT_FLOOR),
        out_after_ms: out_after.map_or(nbsmeta::repair::OUT_AFTER_MS, |s| s * 1000),
        scrub_ms: scrub.map_or(SCRUB_MS, |s| s * 1000),
        scrubbed: Arc::new(Locked::new(member::Scrubs::default()).ok_or(Error::NoMemory)?),
        io: Locked::new(BTreeMap::new()).ok_or(Error::NoMemory)?,
    });
    /* A member already: started over its log. None: the listener, for an
       offer. A member file that cannot be read, or a member that does not
       start: neither -- the host is said to be broken rather than offered
       the cluster as somebody new */
    if let Err(why) = offered::load(&state) {
        let _ = writeln!(out, "nbs: {}", why);
    }
    let st = state.clone();
    let cmd = ShellCommand::register("nbs", HELP, move |args, out| {
        if let Err(problem) = run(&st, args, out) {
            let _ = writeln!(out, "nbs: {}", problem);
        }
    })?;
    let provider = provider::register(state.clone()).map_err(|_| Error::Busy)?;
    Ok(Box::new(Nbs { cmd: Some(cmd), provider: Some(provider), state }))
}

kmod::module!(name: "nbs-mds", init_with_params: init);

/* ---- the shell ---- */

pub(crate) fn view(state: &ModState) -> Option<View> {
    state.group.view()
}

pub(crate) fn member_view(state: &ModState) -> Result<View, String> {
    state.group.member_view(NO_MEMBER)
}

/// This member's view once its log has the cluster: a member just added
/// holds none until raft has brought it the log, which it is given a few
/// seconds for.
pub(crate) fn caught_up(state: &ModState) -> Result<View, String> {
    state.group.caught_up(NO_MEMBER)
}

/// `host` made a member: offered the cluster, then added -- `nbs host add`
/// and the provider's `host_add`. Its incarnation.
pub(crate) fn op_add(state: &ModState, host: &str, stop: &dyn Fn() -> bool) -> Result<u64, String> {
    state.group.add(host, stop, NO_MEMBER)
}

pub(crate) fn op_remove(state: &ModState, host: &str) -> Result<u64, String> {
    state.group.remove(host, NO_MEMBER)
}

/// A host that lost its state taken back: its old member removed -- for
/// good, its incarnation never taken again -- and the host offered the
/// cluster as a new one.
pub(crate) fn op_replace(state: &ModState, host: &str, stop: &dyn Fn() -> bool) -> Result<(u64, u64), String> {
    state.group.replace(host, stop, NO_MEMBER)
}

/// The last member standing made the NBS cluster's one voter: refused while
/// it follows a leader or any other voter answers; said first, done once
/// the cluster's name is given (raftmember's `recover`, as every group's).
/// Unsafe by design -- what the lost members had committed and this one had
/// not is gone: a volume made, an attach, a snapshot -- so they are removed
/// for good, each back only as a new member once its folder is gone.
fn recover(state: &Arc<ModState>, confirm: Option<&str>, out: &mut Output) -> Result<(), String> {
    let host = raftmember::ipcnet::identity().map(|id| id.name).unwrap_or_else(|_| String::from("<this host>"));
    let (r, done) = offered::recover(state, confirm, NO_MEMBER, &lost_remedy(state, &host))?;
    let names: Vec<&str> = r.others.iter().map(|(n, _)| n.as_str()).collect();
    let none_or = |list: &[&str]| if list.is_empty() { String::from("none") } else { list.join(", ") };
    match done {
        Recovered::Asked => {
            let _ = writeln!(out, "nbs: recover makes {}'s member ({}) the one voter of NBS cluster {}, and removes \
                                   every other member for good -- each back only as a new member, by `nbs host add` \
                                   once its folder is gone; whatever they had committed that {} had not is gone: a \
                                   volume made, an attach, a snapshot. No other voter answers. To go ahead: nbs \
                                   recover {}", r.me.0, r.me.1, r.group, r.me.0, r.group);
            let _ = writeln!(out, "nbs: the members removed: {}", none_or(&names));
        }
        Recovered::Done { removed, kept } => {
            for (host, why) in &kept {
                let _ = writeln!(out, "nbs: {}'s member not removed: {} -- `nbs host remove {}` again", host, why, host);
            }
            let removed: Vec<&str> = removed.iter().map(|n| n.as_str()).collect();
            let _ = writeln!(out, "nbs: {}'s member is NBS cluster {}'s one voter now; removed for good: {}", r.me.0,
                             r.group, none_or(&removed));
        }
    }
    Ok(())
}

fn run(state: &Arc<ModState>, args: &str, out: &mut Output) -> Result<(), String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => status(state, out),
        ["init", name] => init_cluster(state, name, out),
        ["hosts"] => hosts(state, out),
        ["host", "add", host] => {
            let id = op_add(state, host, &|| false)?;
            let _ = writeln!(out, "nbs: {} is member {} -- a learner until it holds the log, then a voter while fewer \
                                   than five vote", host, id);
            Ok(())
        }
        ["host", "remove", host] => {
            let id = op_remove(state, host)?;
            let _ = writeln!(out, "nbs: {}'s member {} removed, for good", host, id);
            Ok(())
        }
        ["host", "replace", host] => {
            let (old, new) = op_replace(state, host, &|| false)?;
            let _ = writeln!(out, "nbs: {}'s member {} removed, and {} is member {} now", host, old, host, new);
            Ok(())
        }
        ["recover"] => recover(state, None, out),
        ["recover", name] => recover(state, Some(name), out),
        ["devices", "add", word, rest @ ..] => {
            let wipe = match rest {
                [] => false,
                ["wipe"] => true,
                _ => return Err(String::from(USAGE)),
            };
            let (ident, slots, id, wiped) = store::device_add(state, word, wipe).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: device {} of this host's chunk server, {} -- device {} in the metadata{}; its \
                                   cs takes it in", ident, store::Mib(u64::from(slots) * u64::from(nbsmeta::CHUNK_MIB)),
                             id, if wiped { ", its first pages wiped" } else { "" });
            Ok(())
        }
        ["replica", "bad", set, host] => {
            let set = set.parse::<u32>().map_err(|_| alloc::format!("\"{}\" is no set's number", set))?;
            let version = store::replica_bad(state, set, host).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: {}'s cs taken down in set {} (now at version {}): it joins its own place \
                                   again, its copy held to another member's by sums", host, set, version);
            Ok(())
        }
        ["devices", "drain", ident] => {
            let id = store::device_drain(state, None, ident).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: device {} ({}) takes no new chunks", ident, id);
            Ok(())
        }
        ["devices"] | ["cs"] => {
            store::show_cs(&member_view(state)?, out);
            Ok(())
        }
        ["volumes"] => {
            store::show_volumes(&member_view(state)?, out);
            Ok(())
        }
        ["volume", "create", name, size, rest @ ..] => {
            let size_mib = store::parse_size(size).ok_or_else(|| alloc::format!("\"{}\" is not a size: MiB, or with \
                                                                                 M, G or T after it", size))?;
            let host = match rest {
                [] => None,
                [h] => Some(h.strip_prefix("host=").ok_or_else(|| String::from(USAGE))?),
                _ => return Err(String::from(USAGE)),
            };
            let id = store::volume_create(state, name, size_mib, host).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: volume {} ({}), {}", name, id, store::Mib(size_mib));
            Ok(())
        }
        ["volume", "delete", name] => {
            match store::volume_delete(state, name).map_err(|f| f.says())? {
                0 => {
                    let _ = writeln!(out, "nbs: no volume {}: gone already", name);
                }
                id => {
                    let _ = writeln!(out, "nbs: volume {} ({}) deleted", name, id);
                }
            }
            Ok(())
        }
        ["volume", "resize", name, size] => {
            let size_mib = store::parse_size(size).ok_or_else(|| alloc::format!("\"{}\" is not a size", size))?;
            let id = store::volume_resize(state, name, size_mib).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: volume {} ({}) is {} now", name, id, store::Mib(size_mib));
            Ok(())
        }
        ["volume", "snapshot", volume, name] => {
            let id = io::snapshot(state, volume, name).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: snapshot {} ({}) of volume {}", name, id, volume);
            Ok(())
        }
        ["volume", "freeze", name] => {
            let id = store::volume_freeze(state, name).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: volume {} ({}) is an image now: read-only for good, cloned from", name, id);
            Ok(())
        }
        ["volume", "clone", from, name, rest @ ..] => {
            let host = match rest {
                [] => None,
                [h] => Some(h.strip_prefix("host=").ok_or_else(|| String::from(USAGE))?),
                _ => return Err(String::from(USAGE)),
            };
            let id = store::volume_clone(state, from, name, host).map_err(|f| f.says())?;
            let _ = writeln!(out, "nbs: volume {} ({}), a clone of {}", name, id, from);
            Ok(())
        }
        ["volume", name] => store::show_volume(&member_view(state)?, name, out),
        ["io", rest @ ..] => io::run(state, rest, out),
        _ => Err(String::from(USAGE)),
    }
}

fn init_cluster(state: &Arc<ModState>, name: &str, out: &mut Output) -> Result<(), String> {
    let made = op_init(state, name)?;
    let _ = writeln!(out, "nbs: NBS cluster {} made ({}), {} its first member ({}) -- `nbs host add` the next", name,
                     codec::hex(&made.cluster), made.host, made.member);
    Ok(())
}

/// An NBS cluster made of this host alone: its id, this host's name and
/// its member's incarnation.
pub(crate) struct Made {
    pub cluster: [u8; 16],
    pub host: String,
    pub member: u64,
}

/// An NBS cluster named `name` made of this host alone -- `nbs init` and the
/// provider's `init`: undone where it does not finish, for an init again.
pub(crate) fn op_init(state: &Arc<ModState>, name: &str) -> Result<Made, String> {
    let made = offered::init(state, name, NO_MEMBER)?;
    Ok(Made { cluster: made.group, host: made.host, member: made.member })
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Leader => "leader",
        Role::Follower => "follower",
        Role::Candidate | Role::PreCandidate => "candidate",
    }
}

fn status(state: &ModState, out: &mut Output) -> Result<(), String> {
    let Some(v) = view(state) else {
        let _ = writeln!(out, "nbs: {}", state.group.runner.no_member(NO_MEMBER));
        return Ok(());
    };
    if let Some(why) = &v.stopped {
        let _ = writeln!(out, "nbs: the member stopped: {}", why);
    }
    if v.state.cluster().is_some() && v.state.member_of_id(v.node).is_none() {
        let _ = writeln!(out, "nbs: this host's member ({}) is removed from the NBS cluster -- `rmmod nbs-mds`, \
                               `rm {}` and `insmod` it, then `nbs host add` it again", v.node, state.group.dir);
    }
    let (name, id) = v.state.cluster().map_or((String::from("(no state yet)"), String::new()),
                                              |c| (c.name.clone(), alloc::format!(" ({})", codec::hex(&c.id))));
    let leader = v.state.member_of_id(v.leader).map_or("none", |m| m.host.as_str());
    let _ = writeln!(out, "nbs: {}{} -- member {}, {}, term {}, leader {}, applied {}, {} members, {} voters, {} \
                           learners, {} messages refused", name, id, v.node, role_name(v.role), v.term, leader,
                     v.applied, v.state.members().len(), v.membership.voters.len(), v.membership.learners.len(),
                     v.refused);
    /* How far from three the chunks are (N9) */
    let h = nbsmeta::repair::health(&v.state);
    let joins = v.state.sets().iter().filter(|s| s.joiner.is_some()).count();
    let _ = writeln!(out, "nbs: chunks at three replicas {}, at two {}, at one {} -- {} being copied, by {} joins; \
                           cs's down {}, out {}", h.by_active[3], h.by_active[2], h.by_active[1], h.joining, joins,
                     h.down, h.out);
    /* The scrub's, as this host's member led it (N9e) */
    let (stats, found) = state.scrubbed.with(|x| (x.stats, x.found.clone()));
    if state.scrub_ms == 0 {
        let _ = writeln!(out, "nbs: no scrub here (scrub=0)");
    } else {
        let _ = writeln!(out, "nbs: the scrub, a chunk every {} s as this host's member leads: {} alike, {} held by none, \
                               {} let be, {} unlike -- {} with a member unlike the two others", state.scrub_ms / 1000,
                         stats.alike, stats.empty, stats.skipped, stats.odd + stats.differ, stats.odd);
    }
    for (chunk, set, version, odd) in found {
        let who = odd.and_then(|cs| v.state.cs().get(cs as usize)).map(|c| c.host.as_str());
        /* A finding of a set that has moved on since -- the member taken
           down, joined back -- is history, and says so */
        let now = v.state.sets().get(set as usize).map(|s| s.version).filter(|&now| now != version);
        match (who, now) {
            (Some(host), None) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): {}'s copy is unlike the two others' \
                                       -- `nbs replica bad {} {}` has it copied again", chunk, set, version, host, set,
                                 host);
            }
            (Some(host), Some(now)) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): {}'s copy was unlike the two \
                                       others' -- the set is at version {} now", chunk, set, version, host, now);
            }
            (None, None) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): its members' copies differ, no two \
                                       alike", chunk, set, version);
            }
            (None, Some(now)) => {
                let _ = writeln!(out, "nbs: scrub: chunk {} of set {} (version {}): its members' copies differed, no \
                                       two alike -- the set is at version {} now", chunk, set, version, now);
            }
        }
    }
    match v.state.checks().latest() {
        Some((at, d)) => {
            let _ = writeln!(out, "nbs: the state's digest at entry {}: {}... -- {} of the leader's held to it, {} \
                                   too old to", at, codec::hex(&d[..8]), v.state.checks().held,
                             v.state.checks().unheld);
        }
        None => {
            let _ = writeln!(out, "nbs: no digest of the state yet -- one every {} entries", nbsmeta::CHECK_EVERY);
        }
    }
    let lost: Vec<(String, u64)> = state.group.lost.with(|l| l.iter().map(|(h, &(i, _))| (h.clone(), i)).collect());
    for (host, id) in lost {
        let _ = writeln!(out, "nbs: {} says its mds has no member {}: its state is gone -- `nbs host replace {}`",
                         host, id, host);
    }
    Ok(())
}

fn hosts(state: &ModState, out: &mut Output) -> Result<(), String> {
    let v = member_view(state)?;
    let lost: Vec<String> = state.group.lost.with(|l| l.keys().cloned().collect());
    for m in v.state.members() {
        let vote = if v.membership.voters.contains(&m.id) {
            "voter"
        } else if v.membership.learners.contains(&m.id) {
            "learner"
        } else {
            "joining"
        };
        let mut more = String::new();
        if m.id == v.leader {
            more.push_str(" (leader)");
        }
        if m.id == v.node {
            more.push_str(" (this host)");
        }
        if lost.contains(&m.host) {
            more.push_str(" lost its state");
        }
        let _ = writeln!(out, "  {:<16} member {:<20} {:<8}{}", m.host, m.id, vote, more);
    }
    if !v.state.removed().is_empty() {
        let _ = writeln!(out, "  {} members removed", v.state.removed().len());
    }
    Ok(())
}
