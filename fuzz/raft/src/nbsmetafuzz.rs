//! NBS's metadata (src/crates/common/storage/nbsmeta, plans/nbs.md) held to a
//! model of its own, as the cluster's registry is (registryfuzz.rs):
//!
//! - **Alone**: the metadata and the model take the same commands -- the
//!   cluster made, members added and removed, bytes that are none -- from
//!   small pools of names and incarnations, so that they meet: a host added
//!   again as itself and as another incarnation, an incarnation another
//!   host's or removed before, the last member's removal, a remove asked
//!   again. After every command its answer and everything the metadata shows
//!   -- the cluster, its members in their order, the members removed, the
//!   voters and the learners -- must be the model's. Its snapshot must read
//!   back as itself and come out of a freeze in pieces as one; a snapshot
//!   changed -- a byte, cut short, more bytes, noise -- must be refused or
//!   read back as a state that holds to every rule; and a replica started
//!   from a snapshot part way must end as the metadata does.
//!
//! What raft's rsm does with a state machine -- sessions, a group, snapshots
//! between members -- is the rsm target's and the registry's to hold.

use std::collections::BTreeMap;

use nbsmeta::{Answer, Command, Refusal, State};
use raft::StateMachine;

use crate::rng::{mix, Rng};

/* ---- the model ---- */

#[derive(Clone, Debug, Default)]
struct Model {
    cluster: Option<([u8; 16], String)>,
    /// (host, incarnation, since), in the order they joined.
    members: Vec<(String, u64, u64)>,
    /// (host, incarnation, at), in the order they were removed.
    removed: Vec<(String, u64, u64)>,
}

const M_MEMBERS_MAX: usize = 16;
const M_VOTERS_MAX: usize = 5;
const M_REMOVED_MAX: usize = 1024;

/// A name, as the model has the rule: a DNS label, lowercase.
fn m_name(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

impl Model {
    /// What the metadata answers `cmd`: None for a command of what is stored
    /// (N6), which spoiled bytes can read as and this model does not keep --
    /// fuzz/nbs's meta target does -- and which must leave the members as
    /// they were.
    fn apply(&mut self, index: u64, cmd: Option<&Command>) -> Option<Answer> {
        let Some(cmd) = cmd else { return Some(Answer::Refused(Refusal::Malformed)) };
        if !matches!(cmd, Command::Init { .. } | Command::Add { .. } | Command::Remove { .. }) {
            return None;
        }
        Some(self.apply_member(index, cmd))
    }

    fn apply_member(&mut self, index: u64, cmd: &Command) -> Answer {
        let refuse = Answer::Refused;
        match cmd {
            Command::Init { cluster, name, host, member } => {
                if self.cluster.is_some() {
                    return refuse(Refusal::Initialized);
                }
                if !m_name(name) || !m_name(host) {
                    return refuse(Refusal::BadName);
                }
                if *member == 0 {
                    return refuse(Refusal::BadMember);
                }
                self.cluster = Some((*cluster, name.clone()));
                self.members.push((host.clone(), *member, index));
                Answer::Done { member: *member }
            }
            Command::Add { host, member } => {
                if self.cluster.is_none() {
                    return refuse(Refusal::NotInitialized);
                }
                if !m_name(host) {
                    return refuse(Refusal::BadName);
                }
                if *member == 0 {
                    return refuse(Refusal::BadMember);
                }
                if let Some(m) = self.members.iter().find(|m| m.0 == *host) {
                    return if m.1 == *member { Answer::Done { member: m.1 } } else { refuse(Refusal::HostIsMember) };
                }
                if self.members.iter().any(|m| m.1 == *member) {
                    return refuse(Refusal::MemberInUse);
                }
                if self.removed.iter().any(|r| r.1 == *member) {
                    return refuse(Refusal::MemberRemoved);
                }
                if self.members.len() >= M_MEMBERS_MAX {
                    return refuse(Refusal::Full);
                }
                self.members.push((host.clone(), *member, index));
                Answer::Done { member: *member }
            }
            Command::Remove { host } => {
                if self.cluster.is_none() {
                    return refuse(Refusal::NotInitialized);
                }
                let Some(at) = self.members.iter().position(|m| m.0 == *host) else {
                    return match self.removed.iter().rev().find(|r| r.0 == *host) {
                        Some(r) => Answer::Done { member: r.1 },
                        None => refuse(Refusal::NoSuchHost),
                    };
                };
                if self.members.len() == 1 {
                    return refuse(Refusal::LastMember);
                }
                if self.removed.len() >= M_REMOVED_MAX {
                    return refuse(Refusal::Full);
                }
                let (h, id, _) = self.members.remove(at);
                self.removed.push((h, id, index));
                Answer::Done { member: id }
            }
            _ => Answer::Refused(Refusal::Malformed),
        }
    }
}

/// Where the metadata differs from the model, said.
fn differs(s: &State, m: &Model) -> Option<String> {
    let cluster = s.cluster().map(|c| (c.id, c.name.clone()));
    if cluster != m.cluster {
        return Some(format!("cluster {:?}, the model's {:?}", cluster, m.cluster));
    }
    let members: Vec<(String, u64, u64)> = s.members().iter().map(|x| (x.host.clone(), x.id, x.since)).collect();
    if members != m.members {
        return Some(format!("members {:?}, the model's {:?}", members, m.members));
    }
    let removed: Vec<(String, u64, u64)> = s.removed().iter().map(|x| (x.host.clone(), x.id, x.at)).collect();
    if removed != m.removed {
        return Some(format!("removed {:?}, the model's {:?}", removed, m.removed));
    }
    let voters: Vec<u64> = m.members.iter().take(M_VOTERS_MAX).map(|x| x.1).collect();
    let learners: Vec<u64> = m.members.iter().skip(M_VOTERS_MAX).map(|x| x.1).collect();
    if s.voters() != voters || s.learners() != learners {
        return Some(format!("voters {:?} and learners {:?}, the model's {:?} and {:?}", s.voters(), s.learners(),
                            voters, learners));
    }
    for (host, id, _) in &m.members {
        if s.member(host).map(|x| x.id) != Some(*id) || s.member_of_id(*id).map(|x| x.host.as_str()) != Some(host) {
            return Some(format!("member {} ({}) not found as itself", host, id));
        }
    }
    None
}

/* ---- the commands ---- */

const NAMES: [&str; 8] = ["ex44", "ax41", "box3", "node-4", "n5", "a", "b", "c"];
const BAD_NAMES: [&str; 5] = ["", "Ex44", "a b", "ex44.lan", "x23456789012345678901234567890123456789012345678901234567890123"];
const IDS: [u64; 8] = [1, 2, 3, 4, 0x7fff_ffff_ffff_ffff, u64::MAX, 0xdead_beef, 0];

/// A command, from pools small enough that its parts meet; now and then one
/// of a host or an incarnation never seen, so that the members reach the
/// most a cluster takes.
fn command(r: &mut Rng, wide: bool, fresh: &mut u64) -> Command {
    let name = |r: &mut Rng| -> String {
        if r.chance(3) {
            String::from(r.pick(&BAD_NAMES).unwrap_or(""))
        } else if wide && r.chance(30) {
            format!("h{}", r.below(40))
        } else {
            String::from(r.pick(&NAMES).unwrap_or("a"))
        }
    };
    let mut id = |r: &mut Rng| -> u64 {
        if wide && r.chance(30) {
            *fresh += 1;
            *fresh
        } else {
            r.pick(&IDS).unwrap_or(1)
        }
    };
    match r.below(10) {
        0 => {
            let mut cluster = [0u8; 16];
            for b in cluster.iter_mut() {
                *b = r.below(4) as u8;
            }
            Command::Init { cluster, name: name(r), host: name(r), member: id(r) }
        }
        1..=6 => Command::Add { host: name(r), member: id(r) },
        _ => Command::Remove { host: name(r) },
    }
}

/// A command's bytes, now and then made into others: cut, a bit flipped,
/// noise.
fn entry(r: &mut Rng, cmd: &Command) -> Vec<u8> {
    let mut b = cmd.encode();
    if r.chance(8) {
        match r.below(3) {
            0 => b.truncate(r.below(b.len() as u64) as usize),
            1 => {
                let at = r.below(b.len() as u64) as usize;
                b[at] ^= 1 << r.below(8);
            }
            _ => b = (0..r.below(40)).map(|_| r.next() as u8).collect(),
        }
    }
    b
}

/// A snapshot's bytes changed: a byte, cut short, more bytes, noise.
fn spoil(r: &mut Rng, mut b: Vec<u8>) -> Vec<u8> {
    match r.below(4) {
        0 if !b.is_empty() => {
            let at = r.below(b.len() as u64) as usize;
            b[at] = b[at].wrapping_add(1 + r.below(255) as u8);
        }
        1 => b.truncate(r.below(b.len() as u64 + 1) as usize),
        2 => b.extend((0..r.range(1, 16)).map(|_| r.next() as u8)),
        _ => b = (0..r.below(200)).map(|_| r.next() as u8).collect(),
    }
    b
}

/// `RAFT_NBSMETA_STATS=1`: what each run's commands were answered, and how
/// far its metadata got.
fn stats(seed: u64, answers: &BTreeMap<String, u64>, members_max: usize, removed: usize) {
    if std::env::var_os("RAFT_NBSMETA_STATS").is_some() {
        eprintln!("nbsmeta seed {}: members at most {}, removed {}, answers {:?}", seed, members_max, removed, answers);
    }
}

fn answer_name(a: &Answer) -> String {
    match a {
        Answer::Done { .. } => String::from("Done"),
        Answer::Refused(why) => format!("{:?}", why),
        other => format!("{:?}", other),
    }
}

pub fn run(seed: u64, steps: u64) -> Result<(), String> {
    let mut r = Rng::new(mix(seed, 0x6e62_736d));
    let wide = seed % 3 == 0;
    let mut fresh = 0x1000;
    let mut answers: BTreeMap<String, u64> = BTreeMap::new();
    let mut members_max = 0;
    let mut s = State::new();
    let mut m = Model::default();
    let mut replica: Option<State> = None;
    for step in 1..=steps {
        let cmd = command(&mut r, wide, &mut fresh);
        let bytes = entry(&mut r, &cmd);
        let decoded = Command::decode(&bytes);
        if bytes == cmd.encode() && decoded.as_ref() != Some(&cmd) && m_name_fits(&cmd) {
            return Err(format!("step {}: {:?} does not read back from its bytes", step, cmd));
        }
        let got = Answer::decode(&s.apply(step, &bytes))
            .ok_or_else(|| format!("step {}: an answer that does not read back", step))?;
        let want = m.apply(step, decoded.as_ref());
        if want.is_some_and(|w| w != got) {
            return Err(format!("step {}: {:?} answered {:?}, the model {:?}", step, decoded, got, want));
        }
        *answers.entry(answer_name(&got)).or_default() += 1;
        members_max = members_max.max(s.members().len());
        if let Some(rep) = replica.as_mut() {
            rep.apply(step, &bytes);
        }
        if let Some(d) = differs(&s, &m) {
            return Err(format!("step {} after {:?}: {}", step, decoded, d));
        }
        /* Its snapshot read back as itself, and in pieces as one */
        if r.chance(10) {
            let bytes = s.encode();
            let back = State::decode(&bytes).map_err(|e| format!("step {}: its own snapshot refused: {:?}", step, e))?;
            if back != s {
                return Err(format!("step {}: its snapshot read back as another state", step));
            }
            let mut frozen = s.freeze();
            let mut pieces = Vec::new();
            loop {
                let before = pieces.len();
                raft::Frozen::write(&mut frozen, r.range(1, 64) as usize, &mut pieces);
                if pieces.len() == before {
                    break;
                }
            }
            if pieces != bytes {
                return Err(format!("step {}: the frozen state's pieces are not its snapshot", step));
            }
            if replica.is_none() && r.chance(30) {
                replica = Some(back);
            }
        }
        /* A snapshot changed: refused, or a state that holds to the rules
           -- which `decode` checks -- and reads back as itself */
        if r.chance(5) {
            let spoiled = spoil(&mut r, s.encode());
            if let Ok(t) = State::decode(&spoiled) {
                let again = State::decode(&t.encode()).map_err(|e| {
                    format!("step {}: a state read from a changed snapshot refused as itself: {:?}", step, e)
                })?;
                if again != t {
                    return Err(format!("step {}: a state read from a changed snapshot does not read back", step));
                }
            }
        }
    }
    if let Some(rep) = replica {
        if rep != s {
            return Err(String::from("a replica started from a snapshot part way ends another state"));
        }
    }
    stats(seed, &answers, members_max, s.removed().len());
    Ok(())
}

/// Whether a command's strings fit what its bytes carry: a name past 255
/// bytes is cut by the writer, and so reads back as another.
fn m_name_fits(cmd: &Command) -> bool {
    match cmd {
        Command::Init { name, host, .. } => name.len() <= 63 && host.len() <= 63,
        Command::Add { host, .. } | Command::Remove { host } => host.len() <= 63,
        _ => true,
    }
}

/* ---- the scenarios: what each rule is there for ---- */

const CLUSTER: [u8; 16] = [7; 16];

fn made() -> State {
    let mut s = State::new();
    s.apply_command(1, &Command::Init { cluster: CLUSTER, name: String::from("nbs"), host: String::from("a"), member: 11 });
    s
}

fn add(s: &mut State, index: u64, host: &str, member: u64) -> Answer {
    s.apply_command(index, &Command::Add { host: String::from(host), member })
}

fn remove(s: &mut State, index: u64, host: &str) -> Answer {
    s.apply_command(index, &Command::Remove { host: String::from(host) })
}

fn expect(what: &str, got: Answer, want: Answer) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("{}: {:?}, not {:?}", what, got, want)) }
}

/// A host's member added twice as itself is done twice; as another
/// incarnation -- a host whose state was lost -- refused, until the old one
/// is removed; and the old incarnation is never taken again.
fn nbsmeta_replace() -> Result<(), String> {
    let mut s = made();
    expect("b added", add(&mut s, 2, "b", 22), Answer::Done { member: 22 })?;
    expect("b added again, the same", add(&mut s, 3, "b", 22), Answer::Done { member: 22 })?;
    expect("b as a new incarnation", add(&mut s, 4, "b", 23), Answer::Refused(Refusal::HostIsMember))?;
    expect("b removed", remove(&mut s, 5, "b"), Answer::Done { member: 22 })?;
    expect("b's remove asked again", remove(&mut s, 6, "b"), Answer::Done { member: 22 })?;
    expect("b's old incarnation back", add(&mut s, 7, "b", 22), Answer::Refused(Refusal::MemberRemoved))?;
    expect("b's old incarnation on c", add(&mut s, 8, "c", 22), Answer::Refused(Refusal::MemberRemoved))?;
    expect("b as its new one", add(&mut s, 9, "b", 23), Answer::Done { member: 23 })?;
    expect("c as b's", add(&mut s, 10, "c", 23), Answer::Refused(Refusal::MemberInUse))?;
    Ok(())
}

/// The first five to join vote, the rest learn; a voter removed makes the
/// first learner a voter.
fn nbsmeta_voters() -> Result<(), String> {
    let mut s = made();
    for (i, h) in ["b", "c", "d", "e", "f", "g"].iter().enumerate() {
        add(&mut s, 2 + i as u64, h, 20 + i as u64);
    }
    if s.voters() != [11, 20, 21, 22, 23] || s.learners() != [24, 25] {
        return Err(format!("voters {:?} and learners {:?}", s.voters(), s.learners()));
    }
    remove(&mut s, 20, "c");
    if s.voters() != [11, 20, 22, 23, 24] || s.learners() != [25] {
        return Err(format!("after a voter's removal: voters {:?} and learners {:?}", s.voters(), s.learners()));
    }
    Ok(())
}

/// No cluster: Add and Remove refused; Init once; the last member stays.
fn nbsmeta_bounds() -> Result<(), String> {
    let mut s = State::new();
    expect("an add before init", add(&mut s, 1, "a", 1), Answer::Refused(Refusal::NotInitialized))?;
    expect("a remove before init", remove(&mut s, 1, "a"), Answer::Refused(Refusal::NotInitialized))?;
    let mut s = made();
    let again = s.apply_command(2, &Command::Init { cluster: CLUSTER, name: String::from("nbs"), host: String::from("b"),
                                                   member: 2 });
    expect("a second init", again, Answer::Refused(Refusal::Initialized))?;
    expect("the last member's removal", remove(&mut s, 3, "a"), Answer::Refused(Refusal::LastMember))?;
    expect("member 0", add(&mut s, 4, "b", 0), Answer::Refused(Refusal::BadMember))?;
    expect("a remove of a host never seen", remove(&mut s, 5, "z"), Answer::Refused(Refusal::NoSuchHost))?;
    for i in 0..15 {
        add(&mut s, 10 + i, &format!("h{}", i), 100 + i);
    }
    expect("a seventeenth member", add(&mut s, 30, "z", 999), Answer::Refused(Refusal::Full))?;
    Ok(())
}

/// A snapshot that breaks a rule -- a member twice, a member removed, a
/// member of no cluster -- refused, though its bytes are well formed.
fn nbsmeta_snapshot_rules() -> Result<(), String> {
    let mut s = made();
    add(&mut s, 2, "b", 22);
    remove(&mut s, 3, "b");
    let good = s.encode();
    State::decode(&good).map_err(|e| format!("a good snapshot refused: {:?}", e))?;
    /* The removed incarnation, 22, written in for a's: a member removed */
    let mut bad = good.clone();
    let at = bad.windows(8).position(|w| w == 11u64.to_le_bytes()).ok_or("a's incarnation not in the snapshot")?;
    bad[at..at + 8].copy_from_slice(&22u64.to_le_bytes());
    if State::decode(&bad).is_ok() {
        return Err(String::from("a snapshot with a removed member taken"));
    }
    /* The cluster's flag taken away: members of no cluster */
    let mut bad = good;
    bad[4] = 0;
    if State::decode(&bad).is_ok() {
        return Err(String::from("a snapshot of members with no cluster taken"));
    }
    Ok(())
}

pub const SCENARIOS: &[(&str, fn() -> Result<(), String>)] = &[
    ("nbsmeta_replace", nbsmeta_replace),
    ("nbsmeta_voters", nbsmeta_voters),
    ("nbsmeta_bounds", nbsmeta_bounds),
    ("nbsmeta_snapshot_rules", nbsmeta_snapshot_rules),
];
