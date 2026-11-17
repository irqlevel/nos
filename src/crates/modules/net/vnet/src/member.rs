//! vnet's member: raftmember's engine -- raft's replicated state machine
//! over vnet's state (`vnetmeta::State`), a task of its own (`vnet/raft`),
//! `vnet.raft` on every member -- and what is vnet's own:
//!
//! - **Its members**, as every group whose hosts are offered it keeps them
//!   (raftmember's `offered::Half`): the hosts its nodes are, an offer to a
//!   member already answered, a host's word that its member is gone kept and
//!   said, a probe -- what `vnet recover` asks every other voter -- answered.
//! - **The group's id** before every message of the engine's, held to on
//!   each that comes in: a member takes nothing of another vnet's.
//! - **The state's digest held to the leader's**: every member takes its
//!   state's digest every `vnetmeta::CHECK_EVERY` entries; the leader
//!   proposes its own (`Command::Digest`) as it takes it, and a member whose
//!   own differs stops -- a determinism bug said, not a silent fork.

use alloc::string::String;
use alloc::vec::Vec;

use kcore::ipc::Message;
use raft::{Role, Rsm};
use raftmember::offered::{self, Group, Half, Lost, MemberFile};
use raftmember::{App, Core, Outcome, Pending};
use vnetmeta::{Answer, Command, State};

/// The member's mailbox, the same on every host of the cluster.
pub const NAME: &str = "vnet.raft";

pub type Member = raftmember::Member<Vnet>;
pub type View = raftmember::View<Vnet>;

/// vnet's half of the member.
pub struct Vnet {
    /// This host's member file, and the hosts said to have lost their state.
    half: Half,
    /// The module's folder, for what a member that halts says to do.
    dir: String,
    /// The newest checkpoint whose digest this member proposed as leader.
    proposed: u64,
    /// The state's revision hv's keeper here was last told of
    /// (`vnetmeta::KEEPER`).
    told: u64,
}

/// What a command of vnet's own is waited for by.
pub enum Waiter {
    /// The leader's digest.
    Digest,
}

/// The member over its log in `dir`, its mailbox named.
pub fn new(rsm: Rsm<raftmember::Log, State>, me: MemberFile, lost: Lost, dir: &str) -> Result<Member, String> {
    let id = me.id;
    raftmember::Member::new(Vnet { half: Half { me, lost }, dir: String::from(dir), proposed: 0, told: 0 }, rsm, id)
}

impl App for Vnet {
    type State = State;
    type Command = Command;
    type Answer = Answer;
    type Waiter = Waiter;

    const NAME: &'static str = NAME;
    const LOG: &'static str = "vnet";
    const TRACED_AS: kcore::trace::RaftOf = kcore::trace::RaftOf::Vnet;

    fn encode_command(cmd: &Command) -> Vec<u8> {
        cmd.encode()
    }

    fn decode_command(bytes: &[u8]) -> Option<Command> {
        Command::decode(bytes)
    }

    fn encode_answer(answer: &Answer) -> Vec<u8> {
        answer.encode()
    }

    fn decode_answer(bytes: &[u8]) -> Option<Answer> {
        Answer::decode(bytes)
    }

    fn group(&self, _core: &Core<Vnet>) -> Option<[u8; 16]> {
        Some(self.half.me.group)
    }

    fn node_of(&self, core: &Core<Vnet>, name: &str) -> Option<u64> {
        self.half.node_of(core, name)
    }

    fn host_of(&self, core: &Core<Vnet>, node: u64) -> Option<String> {
        self.half.host_of(core, node)
    }

    fn wanted(&self, core: &Core<Vnet>) -> (Vec<u64>, Vec<u64>) {
        self.half.wanted(core)
    }

    fn on_message(&mut self, core: &mut Core<Vnet>, m: &Message) {
        if self.half.on_message(&*self, core, m) {
            return;
        }
        /* Nothing else of vnet's own comes to the member yet -- hv's asks
           come with V2 (plans/vnet.md, "hv and vnet"): counted */
        core.note_refused();
    }

    fn finish(&mut self, _core: &mut Core<Vnet>, w: Waiter, _outcome: Outcome<Vnet>, _index: u64) {
        match w {
            /* Applied or not, the next checkpoint's is proposed in its turn */
            Waiter::Digest => {}
        }
    }

    /// A host whose member is no longer the one that said it lost its state
    /// forgotten; and as the leader, the newest checkpoint's digest
    /// proposed, for every member to hold its own to.
    fn turn(&mut self, core: &mut Core<Vnet>) {
        self.half.turn(core);
        /* hv's keeper on this host told of each change applied, so that it
         * asks for its table at once and not at its next second: a port
         * moved -- a VM's migration -- reached at its new host sooner. No
         * keeper here -- no hv, or no VM on a network -- is no matter */
        let revision = core.state().revision();
        if revision != self.told {
            self.told = revision;
            let _ = core.mailbox().send_to(vnetmeta::KEEPER, vnetmeta::TAG_CHANGED, 0, &[]);
        }
        if core.role() != Role::Leader {
            return;
        }
        if let Some((at, digest)) = core.state().checks().latest() {
            if at > self.proposed {
                self.proposed = at;
                core.ask(self, Command::Digest { at, digest }, Pending::App(Waiter::Digest));
            }
        }
    }

    /// A state whose digest is not the leader's: this member serves nothing
    /// more -- what it would answer from is not what the others hold.
    fn halt(&self, core: &Core<Vnet>) -> Option<String> {
        let d = core.state().checks().diverged()?;
        Some(alloc::format!("this host's state at entry {} is not the leader's (digest {}..., the leader's {}..., seen \
                             at entry {}): a determinism bug, said rather than forked -- its member serves nothing; \
                             `rmmod vnet`, `rm {}` and `insmod` it, then `vnet host replace {}` on another member",
                            d.at, codec::hex(&d.mine[..6]), codec::hex(&d.theirs[..6]), d.seen, self.dir,
                            self.half.me.host))
    }
}

/// vnet as a group whose hosts are offered it (raftmember's `offered`): its
/// commands of its members.
impl Group for Vnet {
    const MODULE: &'static str = "vnet";
    const WHAT: &'static str = "vnet";
    const AN: &'static str = "a vnet";
    const KIND: &'static str = "vnet";
    const WORD: &'static str = "vnet";
    const MEMBER: &'static str = "vnet member";
    const DIR: &'static str = crate::DEFAULT_DIR;
    const TASK: &'static str = "vnet/raft";
    const IDLE_TASK: &'static str = "vnet/idle";

    fn members(state: &State) -> &members::Members {
        state.roll()
    }

    fn init(group: [u8; 16], name: &str, host: &str, member: u64) -> Command {
        Command::Init { group, name: String::from(name), host: String::from(host), member }
    }

    fn add(host: &str, member: u64) -> Command {
        Command::Add { host: String::from(host), member }
    }

    fn remove(host: &str) -> Command {
        Command::Remove { host: String::from(host) }
    }

    fn done(answer: Answer) -> Result<u64, String> {
        match answer {
            Answer::Done(id) => Ok(id),
            Answer::Refused(why) => Err(String::from(why.says())),
            other => Err(alloc::format!("answered {:?}", other)),
        }
    }
}

/* vnet's own tags, past the offered group's: none yet */
const _: () = assert!(offered::TAG_OWN > raftmember::TAG_APP);
