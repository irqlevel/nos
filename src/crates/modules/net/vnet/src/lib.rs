#![no_std]

//! vnet: the cluster's networks for VMs on its hosts (plans/vnet.md) -- the
//! state every member holds alike, through raft over ipc, of which the
//! cluster's hosts are its members: the networks, their ports, the public
//! addresses and the cluster's firewalls (vnetmeta).
//!
//!     insmod /lib/modules/ipcnet.ko, then cluster.ko: the hosts that trust each other
//!     insmod /lib/modules/vnet.ko [dir=/etc/vnet] [compact=<bytes>]
//!     vnet init <name>                     on the first host: a vnet of it alone
//!     vnet host add|remove|replace <host>  its members, as NBS's are
//!     vnet recover [<name>]                on the last member standing
//!     vnet create lan 10.10.0.0/24 [mtu=1420] [out=no]   a network; .1 its gateway on every host
//!     vnet delete lan | vnet list | vnet ports [lan] | vnet port <id>
//!     vnet port create <network> <uuid> [nic=<n>] [address=<ip>] [firewall=<name>] [host=<host>]
//!     vnet port move <id> <host> | vnet port delete <id> | vnet port firewall <id> <firewall>
//!     vnet ip [add <address> <host> | set <address> <port|none> | move <address> <host> | remove <address>]
//!     vnet fw [create <name> [in=..] [out=..] | delete <name> | <name> [add <rule> [at=N] | del <N> |
//!             move <N> <M> | policy in=.. out=..]]
//!     vnet alerts
//!
//! A port is made by hv as a VM on a network of the cluster's is made (V2);
//! `vnet port create` makes one from the shell, as `nbs io` does a volume's
//! I/O: for the gates, and for an operator who looks. What applying cannot
//! see -- a subnet or a public address covering an address of a host of the
//! registry -- is looked at here, as the command is asked, by the
//! registry's hosts (registry.rs).
//!
//! The members, their offers, the listener of a host in no vnet, the
//! member's start and stop -- with the `begun` mark -- and `recover` are
//! raftmember's, as every group whose hosts are offered it has them
//! (`offered`); the member is raftmember's with vnet's hooks (member.rs).

extern crate alloc;

mod member;
mod ops;
mod provider;
mod registry;
mod shell;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;

use kcore::cmd::{Command as ShellCommand, Output};
use kcore::error::Error;
use raftmember::offered::{self, MemberFile, Offered, Starter};
use raftmember::Log;
use vnetmeta::State;

use member::Vnet;

/// The module's folder unless `dir=` names another.
pub const DEFAULT_DIR: &str = "/etc/vnet";
const HELP: &str = "vnet [init | host | create | list | ports | port | ip | fw | ...] - the cluster's VM networks";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");
pub(crate) const NO_MEMBER: &str = "this host is in no vnet -- `vnet init <name>`, or `vnet host add <this host>` on a \
                                    member";

/// The module's: its folder, the member while there is one, the listener
/// while there is not (raftmember's `Offered`).
pub(crate) struct ModState {
    group: Offered<Vnet>,
    /// The log's bytes past its snapshot before it is compacted: `compact=`.
    compact: u64,
    /// This load's own number: what hv tells a new load of the module by,
    /// whose table it takes whole and whose ports it confirms again.
    instance: u64,
}

/// What to do where this host's member's log is gone beside its `begun`
/// mark: it is never that member again.
fn lost_remedy(state: &ModState, host: &str) -> String {
    alloc::format!("on another member, `vnet host replace {}` once here `rmmod vnet`, `rm {}` and `insmod` it", host,
                   state.group.dir)
}

impl Starter<Vnet> for ModState {
    fn offered(&self) -> &Offered<Vnet> {
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
        let rsm = raftmember::member_rsm::<State>(log, me.id, fresh || campaign, Some(self.compact))?;
        let member = member::new(rsm, me, self.group.lost.clone(), &self.group.dir)?;
        self.group.runner.spawn(<Vnet as offered::Group>::TASK, member)
    }
}

/* Dropped in this order: the command and the provider -- the loader has
   waited out every call of them before this drop -- then the listener
   stopped and waited out, then the member's task, its log closed with it */
struct VnetModule {
    cmd: Option<ShellCommand>,
    provider: Option<kcore::mgmt::Provider>,
    state: Arc<ModState>,
}

impl kmod::Module for VnetModule {}

impl Drop for VnetModule {
    fn drop(&mut self) {
        drop(self.provider.take());
        drop(self.cmd.take());
        self.state.group.shut();
    }
}

fn init(params: &kmod::Params, out: &mut Output) -> kcore::error::Result<Box<dyn kmod::Module>> {
    let mut dir = None;
    let mut compact = None;
    for (key, value) in params.words() {
        match (key, value) {
            ("dir", Some(path)) if dir.is_none() && path.starts_with('/') && path.len() < 200 => dir = Some(path),
            ("compact", Some(bytes)) if compact.is_none() && bytes.parse::<u64>().is_ok() => {
                compact = bytes.parse::<u64>().ok();
            }
            _ => {
                let _ = writeln!(out, "vnet: \"{}\" is not a parameter of vnet's -- dir=<folder> and compact=<bytes> \
                                       are, each given once", key);
                return Err(Error::InvalidValue);
            }
        }
    }
    let state = Arc::new(ModState {
        group: Offered::new(dir.unwrap_or(DEFAULT_DIR)).ok_or(Error::NoMemory)?,
        compact: compact.unwrap_or(raft::rsm::SNAPSHOT_FLOOR),
        /* Within what a client's JSON numbers hold whole (2^53) */
        instance: kcore::random::random_u64().ok_or(Error::NoMemory)? & ((1 << 52) - 1),
    });
    /* A member already: started over its log. None: the listener, for an
       offer. A member file that cannot be read, or a member that does not
       start: neither -- the host is said to be broken rather than offered
       the vnet as somebody new */
    if let Err(why) = offered::load(&state) {
        let _ = writeln!(out, "vnet: {}", why);
    }
    let st = state.clone();
    let cmd = ShellCommand::register("vnet", HELP, move |args, out| {
        if let Err(problem) = shell::run(&st, args, out) {
            let _ = writeln!(out, "vnet: {}", problem);
        }
    })?;
    let provider = match provider::register(state.clone()) {
        Ok(p) => Some(p),
        Err(s) => {
            let _ = writeln!(out, "vnet: not on the management bus: {}", s.name());
            None
        }
    };
    Ok(Box::new(VnetModule { cmd: Some(cmd), provider, state }))
}

kmod::module!(name: "vnet", init_with_params: init);
