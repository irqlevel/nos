//! The host's firewalls (plans/vnet.md, "The firewall"): a file each in
//! `/etc/hv/firewalls`, a place each in the guests' switch while a VM is
//! judged by it; `hv fw`, which makes them, shows them and changes them, and
//! the `fw` provider (`fw_provider.rs`), which does the same through the
//! same functions -- `create`, `delete`, `add`, `remove`, `reorder`,
//! `policy`, `assign` and `rows` -- each refused with a `Refused` that says
//! what kind of no it is.
//!
//! A VM names its firewall in its vm.toml (`firewall = "web"`), on `hv
//! start` (`firewall=web`) or with `hv fw set`; one that names none is
//! judged by `default` -- what comes in denied, what goes out allowed, no
//! rule -- made the first time it is wanted, changed as any other, never
//! deleted. A VM from a folder whose vm.toml was written before firewalls is
//! given one at its first start, written into the file: `default` -- or, one
//! with forwards, a firewall of its own name that lets in at each port
//! forwarded to it, so that what `hv forward` reached stays reached (`adopt`).
//!
//! A change is written to its file first, whole (`kcore::fs::write`), and
//! then put in its place in the switch: every VM on it judged by it from the
//! next frame on, and its open flows judged again (`Switch::install`). A
//! firewall whose file cannot be read is never stood in for: the VM that
//! names it does not start, and says why -- neither open nor with `default`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use conf::{error, put, quoted, Value};
use firewall::edit::{self, Edit};
use firewall::{valid_name, Action, Dir, Rule, Rules, RULES_MAX};
use kcore::sync::Mutex;

use crate::net::{Hits, Switch, FIREWALLS_MAX};
use crate::store::{self, Folder};
use crate::vms::Vms;
use crate::vnet::ClusterFirewall;

/// Where the host's firewalls are kept, a file each: `<name>.toml`.
pub const DIR: &str = "/etc/hv/firewalls";
/// The firewall a VM that names none is judged by.
pub const DEFAULT: &str = "default";
/// The most a firewall's file may be: sixty-four rules and some comments.
const MAX_BYTES: usize = 32 * 1024;
/// What a firewall's file is called in what is said of one.
const WHAT: &str = "a firewall's file";
const HEADER: &str = "# A firewall of the guests' (plans/vnet.md): `hv fw <name>` shows it and changes it.\n";
/// Words `hv fw` takes in a firewall's place, which no firewall is called.
const VERBS: [&str; 4] = ["create", "delete", "set", "help"];

/// What `hv fw` says it takes.
pub const USAGE: &str = "\
hv fw                                  the host's firewalls, and the VMs on each
hv fw create <name> [in=allow|deny] [out=allow|deny]
hv fw delete <name>                    one no VM names, default never
hv fw <name>                           its policies, its rules -- each with the flows it judged -- and its VMs
hv fw <name> add <rule> [at=N]         allow|deny in|out [tcp|udp|icmp] [ports] [from|to any|net|<a.b.c.d/n,...>]
hv fw <name> del <N> | move <N> <M> | policy [in=allow|deny] [out=allow|deny]
hv fw set <vm> <name>                  a VM onto another firewall: its vm.toml, and at once where it runs";

/// Why something asked of the firewalls was not done: what the shell prints,
/// and the kind of no the `fw` provider answers by.
#[derive(Debug)]
pub enum Refused {
    /// What was asked is not one -- a name, a rule, a rule's number; `field`
    /// the parameter of the provider's that said it.
    Invalid { why: String, field: &'static str },
    /// There is no such firewall, or VM.
    NotFound(String),
    /// What is there is in the way: a firewall of that name, a VM on it,
    /// `default` asked to go.
    Conflict(String),
    /// What should have been done was not: its file, memory, the switch.
    Failed(String),
}

impl Refused {
    pub fn text(&self) -> &str {
        match self {
            Refused::Invalid { why, .. } => why,
            Refused::NotFound(why) | Refused::Conflict(why) | Refused::Failed(why) => why,
        }
    }

    fn invalid(why: String, field: &'static str) -> Refused {
        Refused::Invalid { why, field }
    }
}

/// What an operation on a file or memory failed by: Failed.
impl From<String> for Refused {
    fn from(why: String) -> Refused {
        Refused::Failed(why)
    }
}

/// A firewall's rules on the heap. A `Rules` is seven kilobytes -- its
/// sixty-four rules inline, which the switch's places copy under their spin
/// lock with nothing allocated -- and the bus calls a provider on its
/// caller's stack: three or four of them on the stack of `api panel
/// create`'s shell, through `fw.add`, ran it out (a double fault, under
/// api-test). So nothing here keeps one on a stack; `STACK_LIGHT` below
/// holds what does to a size.
struct Held(Box<[Rules]>);

impl Held {
    /// A copy of `from`, in memory taken first: Failed without it.
    fn of(from: &Rules) -> Result<Held, Refused> {
        let mut v = Vec::new();
        v.try_reserve_exact(1).map_err(|_| String::from("out of memory"))?;
        v.push(Rules::DEFAULT);
        let mut held = Held(v.into_boxed_slice());
        held.get_mut().copy_from(from);
        Ok(held)
    }

    fn get(&self) -> &Rules {
        &self.0[0]
    }

    fn get_mut(&mut self) -> &mut Rules {
        &mut self.0[0]
    }
}

/// A firewall hv has read: its rules, and its place in the switch once a
/// VM is judged by it -- one of the host's, from its file, or one of the
/// cluster's, `vnet:<name>`, from vnet's state at `revision`.
struct Known {
    name: String,
    rules: Held,
    place: Option<usize>,
    revision: Option<u64>,
}

/// What lives on a stack here, held to a size: a `Rules` in one of them is
/// a stack run out on a deep caller's.
const STACK_LIGHT: usize = 256;
const _: () = assert!(core::mem::size_of::<Known>() <= STACK_LIGHT, "a Known's rules belong on the heap");
const _: () = assert!(core::mem::size_of::<Reader>() <= STACK_LIGHT, "a Reader's rules belong on the heap");

/// The host's firewalls hv has read, and their places.
pub struct Firewalls {
    known: Mutex<Vec<Known>>,
}

/// A firewall as a listing says it, copied out from under the lock.
pub struct Row {
    pub name: String,
    /// What its file says -- or why it cannot be read.
    pub read: Result<Said, String>,
    /// Its place in the switch, while it has one: where its VMs and what it
    /// has judged are read.
    pub place: Option<usize>,
    /// Whether it has a file: `default` is listed before its first use.
    pub made: bool,
}

/// A firewall's policies and its rules, each as a person writes it.
pub struct Said {
    pub inbound: Action,
    pub outbound: Action,
    pub rules: Vec<String>,
}

/// A VM put on a firewall (`assign`).
pub struct Assigned {
    /// Its number, where it runs.
    pub running: Option<u32>,
    /// Whether the switch judges it by the firewall now: one that runs with
    /// no NIC has no port to judge.
    pub live: bool,
    /// Its open flows the firewall does not let through, cut.
    pub cut: usize,
    /// The VM whose vm.toml says it now, by its folder's name.
    pub kept: Option<String>,
}

/// A firewall's file, as `conf` reads it: `in` and `out`, each `allow` or
/// `deny`, and `rules`, a list of rules as a person writes them.
struct Reader {
    rules: Held,
}

impl conf::Keys for Reader {
    fn known(&mut self, line: usize, key: &str) -> Result<(), String> {
        match key {
            "in" | "out" | "rules" => Ok(()),
            _ => Err(error(line, &alloc::format!("\"{}\" is not a key {} has", key, WHAT))),
        }
    }

    fn assign(&mut self, line: usize, key: &str, value: Value) -> Result<(), String> {
        match (key, value) {
            ("in" | "out", Value::Str(s)) => {
                let action = Action::parse(&s)
                    .ok_or_else(|| error(line, &alloc::format!("{} is allow or deny, not \"{}\"", key, s)))?;
                self.rules.get_mut().set_policy(if key == "in" { Dir::In } else { Dir::Out }, action);
                Ok(())
            }
            ("rules", Value::List(list)) => {
                for (i, text) in list.iter().enumerate() {
                    let rule = Rule::parse(text)
                        .map_err(|why| error(line, &alloc::format!("rule {}, \"{}\": {}", i + 1, text, why)))?;
                    self.rules.get_mut().insert(None, rule).map_err(|why| error(line, why))?;
                }
                Ok(())
            }
            (key, value) => {
                let wants = if key == "rules" { "a list of rules" } else { "allow or deny" };
                Err(error(line, &alloc::format!("{} wants {}, not {}", key, wants, value.kind())))
            }
        }
    }
}

/// A firewall's file read as the rules it says, or what is wrong with it --
/// the first thing, with its line.
fn parse(text: &str) -> Result<Held, String> {
    let mut reader = Reader { rules: Held::of(&Rules::DEFAULT).map_err(|r| String::from(r.text()))? };
    conf::parse(text, WHAT, &mut reader)?;
    Ok(reader.rules)
}

/// The rules as their file says them: what `parse` reads back as the same.
/// None out of memory.
pub fn render(rules: &Rules) -> Option<String> {
    let mut s = String::new();
    s.try_reserve(HEADER.len() + 64 + rules.rules().len() * 128).ok()?;
    s.push_str(HEADER);
    put(&mut s, "in", rules.inbound.name());
    put(&mut s, "out", rules.outbound.name());
    s.push_str("rules = [");
    for r in rules.rules() {
        let text = rule_text(r)?;
        s.push_str("\n    ");
        quoted(&mut s, &text);
        s.push(',');
    }
    s.push_str(if rules.rules().is_empty() { "]\n" } else { "\n]\n" });
    Some(s)
}

/// A rule as a person writes it: None out of memory.
fn rule_text(r: &Rule) -> Option<String> {
    let mut text = String::new();
    text.try_reserve(96).ok()?;
    let _ = write!(text, "{}", r);
    Some(text)
}

/// The file of firewall `name`.
fn path(name: &str) -> Result<String, String> {
    let mut file = String::new();
    file.try_reserve(name.len() + 5).map_err(|_| String::from("out of memory"))?;
    let _ = write!(file, "{}.toml", name);
    store::join(DIR, &file).ok_or_else(|| String::from("out of memory"))
}

/// Firewall `name`'s rules, from its file -- None where it has none -- or
/// why not: a file that is not one.
fn read(name: &str) -> Result<Option<Held>, String> {
    let file = path(name)?;
    let bytes = match kcore::fs::read(&file, MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(kcore::error::Error::NotFound) => return Ok(None),
        Err(kcore::error::Error::InvalidValue) => {
            return Err(alloc::format!("{} is more than the {} bytes {} may be", file, MAX_BYTES, WHAT));
        }
        Err(e) => return Err(alloc::format!("{}: {}", file, e)),
    };
    let text = core::str::from_utf8(&bytes).map_err(|_| alloc::format!("{} is not UTF-8 text", file))?;
    parse(text).map(Some).map_err(|e| alloc::format!("{}: {}", file, e))
}

/// What is said of a firewall there is no file of.
fn none_called(name: &str) -> Refused {
    Refused::NotFound(alloc::format!("no firewall {} on this host -- hv fw create {}", name, name))
}

/// Firewall `name`'s file made `rules`: a new file where `new` -- refused
/// where there is one -- and otherwise replaced whole, a full disk or a
/// crash midway leaving it as it was.
fn write(name: &str, rules: &Rules, new: bool) -> Result<(), Refused> {
    let file = path(name)?;
    store::make_dirs(DIR).map_err(|e| alloc::format!("{} could not be made -- {}", DIR, e))?;
    let text = render(rules).ok_or_else(|| String::from("out of memory"))?;
    let done = if new { kcore::fs::create(&file, text.as_bytes()) } else { kcore::fs::write(&file, text.as_bytes()) };
    match done {
        Ok(()) => Ok(()),
        Err(kcore::error::Error::Busy) => Err(Refused::Conflict(alloc::format!("there is a firewall {} already", name))),
        Err(e) => Err(Refused::Failed(alloc::format!("{} could not be written -- {}", file, e))),
    }
}

/// The place of firewall `name` in `known`: read from its file -- `default`
/// made the first time it is wanted -- the first time it is asked for.
fn index(known: &mut Vec<Known>, name: &str) -> Result<usize, Refused> {
    if name.starts_with(crate::vnet::PREFIX) {
        return Err(Refused::Conflict(alloc::format!("{} is the cluster's: `vnet fw` shows it and changes it", name)));
    }
    if let Some(i) = known.iter().position(|k| k.name == name) {
        return Ok(i);
    }
    if !valid_name(name) {
        return Err(Refused::invalid(alloc::format!("\"{}\" is not a firewall's name", name), "name"));
    }
    let rules = match read(name)? {
        Some(rules) => rules,
        /* `default`, made the first time it is wanted -- and a file of it
         * that is not one is said, never made again over it. */
        None if name == DEFAULT => {
            write(name, &Rules::DEFAULT, true)?;
            read(name)?.ok_or_else(|| none_called(name))?
        }
        None => return Err(none_called(name)),
    };
    let mut owned = String::new();
    owned.try_reserve_exact(name.len()).map_err(|_| String::from("out of memory"))?;
    owned.push_str(name);
    known.try_reserve(1).map_err(|_| String::from("out of memory"))?;
    known.push(Known { name: owned, rules, place: None, revision: None });
    Ok(known.len() - 1)
}

/// Firewall `known[i]`'s place in `switch`: given one the first time -- a
/// free place, or one whose firewall no VM is judged by now -- and its rules
/// put in it.
fn placed(known: &mut [Known], i: usize, switch: &Switch) -> Result<usize, Refused> {
    if let Some(place) = known[i].place {
        return Ok(place);
    }
    let taken = |place: usize| known.iter().any(|k| k.place == Some(place));
    let place = match (0..FIREWALLS_MAX).find(|&p| !taken(p)) {
        Some(p) => p,
        None => {
            /* Every place has a firewall: one no VM is judged by now gives
             * its place up, and is put in one again when one is. */
            let j = known.iter().position(|k| k.place.is_some_and(|p| switch.uninstall(p)))
                .ok_or_else(|| Refused::Conflict(alloc::format!("{} firewalls have VMs on them, the most the \
                                                                 switch holds", FIREWALLS_MAX)))?;
            known[j].place.take().unwrap_or(0)
        }
    };
    switch.install(place, known[i].rules.get()).map_err(|why| Refused::Failed(String::from(why)))?;
    known[i].place = Some(place);
    Ok(place)
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// A rule's number as a person says it -- 1 for the first -- of `count`, as
/// its place; `field` the provider's parameter that gave it (`firewall`'s,
/// as vnet's firewalls have it).
fn number(n: usize, count: usize, field: &'static str) -> Result<usize, Refused> {
    edit::number(n, count, field).map_err(refused_as)
}

/// `in=allow`, `out=deny`, ...: a direction's policy.
fn policy_word(word: &str) -> Result<(Dir, Action), Refused> {
    edit::policy_word(word).map_err(refused_as)
}

/// What the firewall crate's reading of a person's words refused, as hv's.
fn refused_as(b: edit::Bad) -> Refused {
    Refused::invalid(b.why, b.field)
}

/// A cluster firewall's rules, read from vnet's text into hv's own: a rule
/// hv cannot read is a vnet of another build's, and refused rather than
/// left out -- a firewall missing a rule judges what it should not.
fn cluster_rules(f: &ClusterFirewall) -> Result<Held, String> {
    let mut held = Held::of(&Rules::DEFAULT).map_err(|r| String::from(r.text()))?;
    let rules = held.get_mut();
    rules.inbound = f.inbound;
    rules.outbound = f.outbound;
    for text in f.rules.iter() {
        let rule = Rule::parse(text).map_err(|why| alloc::format!("vnet's rule \"{}\": {}", text, why))?;
        rules.insert(None, rule).map_err(String::from)?;
    }
    Ok(held)
}

/// What a listing says of a firewall's rules: None out of memory.
fn said(rules: &Rules) -> Option<Said> {
    let mut texts = Vec::new();
    texts.try_reserve_exact(rules.rules().len()).ok()?;
    for r in rules.rules() {
        texts.push(rule_text(r)?);
    }
    Some(Said { inbound: rules.inbound, outbound: rules.outbound, rules: texts })
}

/// A firewall's name owned: Failed out of memory.
fn owned(name: &str) -> Result<String, Refused> {
    let mut n = String::new();
    n.try_reserve_exact(name.len()).map_err(|_| String::from("out of memory"))?;
    n.push_str(name);
    Ok(n)
}

impl Firewalls {
    pub fn new() -> Option<Firewalls> {
        Some(Firewalls { known: Mutex::new(Vec::new())? })
    }

    /// The place of firewall `name` in `switch`, for a VM to be judged by
    /// it: its file read, and its rules put in a place, the first time --
    /// `default` made, the first time it is wanted. Why not: no such
    /// firewall, a file that is not one, every place taken.
    pub fn place(&self, switch: &Switch, name: &str) -> Result<usize, String> {
        let mut known = self.known.lock();
        let got = index(&mut known, name).and_then(|i| placed(&mut known, i, switch));
        got.map_err(|r| String::from(r.text()))
    }

    /// The place of the cluster's firewall `f` in `switch` -- `vnet:<name>`
    /// among the host's -- for a VM on a network of the cluster's to be
    /// judged by: its rules read from vnet's text, put in a place the first
    /// time, and again where vnet's revision of them has moved since.
    pub fn place_cluster(&self, switch: &Switch, f: &ClusterFirewall) -> Result<usize, String> {
        let name = alloc::format!("{}{}", crate::vnet::PREFIX, f.name);
        let rules = cluster_rules(f)?;
        let mut known = self.known.lock();
        let i = match known.iter().position(|k| k.name == name) {
            Some(i) => {
                if known[i].revision != Some(f.revision) {
                    known[i].rules = rules;
                    known[i].revision = Some(f.revision);
                    if let Some(place) = known[i].place {
                        switch.install(place, known[i].rules.get()).map_err(String::from)?;
                    }
                }
                i
            }
            None => {
                known.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                known.push(Known { name, rules, place: None, revision: Some(f.revision) });
                known.len() - 1
            }
        };
        placed(&mut known, i, switch).map_err(|r| String::from(r.text()))
    }

    /// The cluster's firewalls hv has, made what vnet's table says of them
    /// now -- `firewalls`, those its ports here are judged by -- each put in
    /// its place again where its revision moved: what could not be, said.
    pub fn refresh_cluster(&self, switch: &Switch, firewalls: &[ClusterFirewall]) -> Vec<String> {
        let mut said = Vec::new();
        for f in firewalls {
            let name = alloc::format!("{}{}", crate::vnet::PREFIX, f.name);
            let stale = self.known.lock().iter().any(|k| k.name == name && k.revision != Some(f.revision));
            if stale {
                if let Err(why) = self.place_cluster(switch, f) {
                    said.push(alloc::format!("{}: {}", name, why));
                }
            }
        }
        said
    }

    /// The firewall VM `folder`'s first start since VMs had them gives it,
    /// written into its vm.toml as every change is: `default` -- or, a VM
    /// with forwards, a firewall of its own name letting in at each port
    /// forwarded to it, made unless there is one of that name. Its name,
    /// and what was done, to be said; where the file could not be changed,
    /// that too, and the VM is judged by it this start all the same.
    pub fn adopt(&self, vms: &Vms, folder: &Folder, forwards: &[(u16, u16)]) -> (String, String) {
        let mut name = String::new();
        let mut said = String::new();
        let own = !forwards.is_empty() && valid_name(&folder.name);
        if own && name.try_reserve_exact(folder.name.len()).is_ok() {
            name.push_str(&folder.name);
            let mut known = self.known.lock();
            match index(&mut known, &name) {
                Ok(_) => {
                    let _ = write!(said, "{} is judged by the firewall {}, which there was already", folder.name, name);
                }
                Err(_) => {
                    let made = Held::of(&Rules::DEFAULT).and_then(|mut held| {
                        let mut ports = String::new();
                        let rules = held.get_mut();
                        for &(_, guest) in forwards {
                            let text = alloc::format!("allow in tcp {}", guest);
                            if let Ok(rule) = Rule::parse(&text) {
                                if !rules.rules().contains(&rule) && rules.insert(None, rule).is_ok() {
                                    let _ = write!(ports, "{}{}", if ports.is_empty() { "" } else { "," }, guest);
                                }
                            }
                        }
                        write(&name, held.get(), true).map(|()| ports)
                    });
                    match made {
                        Ok(ports) => {
                            let _ = write!(said, "{} is judged by a firewall of its own, {}, letting in at the \
                                                  port{} forwarded to it -- {} -- and nothing else (hv fw {})",
                                           folder.name, name, if forwards.len() == 1 { "" } else { "s" }, ports, name);
                        }
                        Err(e) => {
                            let _ = write!(said, "{} -- its firewall {} could not be made: {}", folder.name, name,
                                           e.text());
                            name.clear();
                        }
                    }
                }
            }
        }
        if name.is_empty() {
            name.push_str(DEFAULT);
            if said.is_empty() {
                let _ = write!(said, "{} is judged by the firewall {}: what comes in denied, what goes out allowed",
                               folder.name, DEFAULT);
            }
        }
        let kept = store::update(vms, folder, |c| {
            if c.options.firewall.is_none() {
                let mut n = String::new();
                n.try_reserve_exact(name.len()).map_err(|_| String::from("out of memory"))?;
                n.push_str(&name);
                c.options.firewall = Some(n);
            }
            Ok(())
        });
        if let Err(e) = kept {
            let _ = write!(said, " -- not written into its vm.toml ({}): again at its next start", e);
        }
        (name, said)
    }

    /// Firewall `name` changed by `change` -- written to its file whole
    /// first, then, where it has a place, put in it, every VM on it judged
    /// by it from the next frame on: what `change` gave back, and how many
    /// open flows its VMs lost to it.
    fn change<T>(&self, switch: Option<&Switch>, name: &str, change: impl FnOnce(&mut Rules) -> Result<T, Refused>)
        -> Result<(T, usize), Refused>
    {
        let mut known = self.known.lock();
        let i = index(&mut known, name)?;
        let mut rules = Held::of(known[i].rules.get())?;
        let done = change(rules.get_mut())?;
        write(name, rules.get(), false)?;
        known[i].rules = rules;
        let mut cut = 0;
        if let (Some(place), Some(switch)) = (known[i].place, switch) {
            cut = switch.install(place, known[i].rules.get()).map_err(|why| Refused::Failed(String::from(why)))?;
        }
        Ok((done, cut))
    }

    /// Rule `rule` added to firewall `name` at place `at` (from 0), or
    /// last: its place, and the open flows cut.
    pub fn add(&self, switch: Option<&Switch>, name: &str, rule: Rule, at: Option<usize>)
        -> Result<(usize, usize), Refused>
    {
        self.change(switch, name, |rules| {
            if let Some(at) = at {
                number(at + 1, rules.rules().len() + 1, "at")?;
            }
            rules.insert(at, rule).map_err(|why| Refused::invalid(String::from(why), "rule"))
        })
    }

    /// Firewall `name`'s rule number `n` (from 1) taken out: the rule, and
    /// the open flows cut.
    pub fn remove(&self, switch: Option<&Switch>, name: &str, n: usize) -> Result<(Rule, usize), Refused> {
        self.change(switch, name, |rules| {
            let at = number(n, rules.rules().len(), "n")?;
            rules.remove(at).ok_or_else(|| Refused::invalid(String::from("there is no such rule"), "n"))
        })
    }

    /// Firewall `name`'s rule number `n` made number `to` (both from 1): the
    /// open flows cut, the order deciding.
    pub fn reorder(&self, switch: Option<&Switch>, name: &str, n: usize, to: usize) -> Result<usize, Refused> {
        self.change(switch, name, |rules| {
            let count = rules.rules().len();
            let (from, to) = (number(n, count, "n")?, number(to, count, "to")?);
            if rules.reorder(from, to) {
                Ok(())
            } else {
                Err(Refused::invalid(String::from("there is no such rule"), "n"))
            }
        }).map(|((), cut)| cut)
    }

    /// Firewall `name`'s policies, each of `policies` set: both as they are
    /// now, and the open flows cut.
    pub fn policy(&self, switch: Option<&Switch>, name: &str, policies: &[(Dir, Action)])
        -> Result<((Action, Action), usize), Refused>
    {
        self.change(switch, name, |rules| {
            for &(dir, action) in policies {
                rules.set_policy(dir, action);
            }
            Ok((rules.inbound, rules.outbound))
        })
    }

    /// Firewall `name` made, with no rule and `policies` -- `in` denied and
    /// `out` allowed unless they say: its policies.
    pub fn create(&self, name: &str, policies: &[(Dir, Action)]) -> Result<(Action, Action), Refused> {
        if !valid_name(name) || VERBS.contains(&name) {
            return Err(Refused::invalid(alloc::format!("\"{}\" is not a firewall's name -- a letter or a digit, then \
                                                        letters, digits, . _ and -, at most {}, and none of {}", name,
                                                       firewall::NAME_MAX, VERBS.join(", ")), "name"));
        }
        let mut held = Held::of(&Rules::DEFAULT)?;
        let rules = held.get_mut();
        for &(dir, action) in policies {
            rules.set_policy(dir, action);
        }
        /* Under the lock, so that two makes of one name are one and a
         * refusal. */
        let known = self.known.lock();
        if known.iter().any(|k| k.name == name) {
            return Err(Refused::Conflict(alloc::format!("there is a firewall {} already", name)));
        }
        write(name, held.get(), true)?;
        Ok((held.get().inbound, held.get().outbound))
    }

    /// Firewall `name` deleted: not `default`, and not one a VM names -- one
    /// running, or one whose vm.toml in the VMs' folder `root` says it.
    pub fn delete(&self, root: &str, switch: Option<&Switch>, name: &str) -> Result<(), Refused> {
        if name == DEFAULT {
            return Err(Refused::Conflict(alloc::format!("{} is what a VM that names no firewall is judged by: it is \
                                                         changed, never deleted", DEFAULT)));
        }
        let mut known = self.known.lock();
        let i = index(&mut known, name)?;
        if let (Some(place), Some(switch)) = (known[i].place, switch) {
            if switch.in_use(place) {
                return Err(Refused::Conflict(alloc::format!("a running VM is judged by {} -- hv fw set <vm> \
                                                             <another> first", name)));
            }
        }
        let naming = named_by(root, name);
        if !naming.is_empty() {
            return Err(Refused::Conflict(alloc::format!("{} name{} {} in their vm.toml -- hv fw set <vm> <another> \
                                                         first", naming.join(", "),
                                                        if naming.len() == 1 { "s" } else { "" }, name)));
        }
        if let (Some(place), Some(switch)) = (known[i].place, switch) {
            if !switch.uninstall(place) {
                return Err(Refused::Conflict(alloc::format!("a VM was started on {} meanwhile", name)));
            }
        }
        kcore::fs::remove(&path(name)?).map_err(|e| alloc::format!("{} could not be removed -- {}", name, e))?;
        known.remove(i);
        Ok(())
    }

    /// VM `vm` put on firewall `name`: a running one judged by it at once --
    /// its open flows judged again -- and the vm.toml of one with a folder
    /// in the VMs' folder `root` made to say it.
    pub fn assign(&self, root: &str, vms: &Vms, vm: &str, name: &str) -> Result<Assigned, Refused> {
        let running = vms.id_of(vm).ok();
        let mut done = Assigned { running, live: false, cut: 0, kept: None };
        if let Some(id) = running {
            if let (Some(port), Some(switch)) = (vms.port_of(id), vms.switch_now()) {
                let place = {
                    let mut known = self.known.lock();
                    let i = index(&mut known, name)?;
                    placed(&mut known, i, &switch)?
                };
                done.cut = switch.assign(port, place).map_err(|why| Refused::Failed(String::from(why)))?;
                done.live = true;
            } else {
                let mut known = self.known.lock();
                index(&mut known, name)?;
            }
        } else {
            let mut known = self.known.lock();
            index(&mut known, name)?;
        }
        let folder = match running.and_then(|id| vms.name_of(id)) {
            Some(n) => vms.folder_listed(&n),
            None if running.is_none() => {
                Some(store::folder_of(root, vm).map_err(|e| Refused::NotFound(alloc::format!("no vm {}: {}", vm, e)))?)
            }
            None => None,
        };
        if let Some(folder) = folder {
            /* A firewall judges a NIC: a vm.toml with none says no firewall
             * (`config`), and is told why here rather than refused as a
             * file that does not hold. */
            if store::load(&folder).is_ok_and(|c| !c.options.net) {
                return Err(Refused::Conflict(alloc::format!("{} has no NIC -- net in its vm.toml -- for a firewall \
                                                             to judge", folder.name)));
            }
            store::update(vms, &folder, |c| {
                let mut n = String::new();
                n.try_reserve_exact(name.len()).map_err(|_| String::from("out of memory"))?;
                n.push_str(name);
                c.options.firewall = Some(n);
                Ok(())
            })?;
            done.kept = Some(folder.name);
        }
        Ok(done)
    }

    /// The host's firewalls as a listing says them -- every file in `DIR`,
    /// and `default` before its first use -- or, `only`, the one of that
    /// name.
    pub fn rows(&self, only: Option<&str>) -> Result<Vec<Row>, Refused> {
        let mut names = Vec::new();
        match only {
            Some(name) => {
                names.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                names.push(owned(name)?);
            }
            None => {
                if let Ok(entries) = kcore::fs::list(DIR) {
                    for e in entries.iter().filter(|e| !e.dir) {
                        if let Some(n) = core::str::from_utf8(&e.name).ok().and_then(|n| n.strip_suffix(".toml")) {
                            if valid_name(n) {
                                names.try_reserve(1).map_err(|_| String::from("out of memory"))?;
                                names.push(owned(n)?);
                            }
                        }
                    }
                }
                names.sort();
            }
        }
        let mut rows = Vec::new();
        rows.try_reserve_exact(names.len() + 1).map_err(|_| String::from("out of memory"))?;
        if only.is_none() && !names.iter().any(|n| n == DEFAULT) {
            let read = said(&Rules::DEFAULT).ok_or_else(|| String::from("out of memory"))?;
            rows.push(Row { name: owned(DEFAULT)?, read: Ok(read), place: None, made: false });
        }
        let mut known = self.known.lock();
        for name in names {
            /* `default` asked for alone before it has a file: listed as it
             * will be made, and not made by a look. */
            if only.is_some() && name == DEFAULT && !known.iter().any(|k| k.name == DEFAULT) && read(DEFAULT)?.is_none() {
                let read = said(&Rules::DEFAULT).ok_or_else(|| String::from("out of memory"))?;
                rows.push(Row { name, read: Ok(read), place: None, made: false });
                continue;
            }
            match index(&mut known, &name) {
                Ok(i) => {
                    let k = &known[i];
                    let read = said(k.rules.get()).ok_or_else(|| String::from("out of memory"))?;
                    rows.push(Row { name, read: Ok(read), place: k.place, made: true });
                }
                Err(e @ (Refused::NotFound(_) | Refused::Invalid { .. })) if only.is_some() => return Err(e),
                Err(e) => rows.push(Row { name, read: Err(String::from(e.text())), place: None, made: true }),
            }
        }
        Ok(rows)
    }

    /// The firewall each port of `switch` with a VM on it is judged by, by
    /// the port's number: what a running VM's listing says it is on.
    pub fn on_ports(&self, switch: &Switch) -> Vec<(usize, String)> {
        let mut all = Vec::new();
        let known = self.known.lock();
        for k in known.iter() {
            let Some(place) = k.place else { continue };
            for port in switch.ports_on(place) {
                match owned(&k.name) {
                    Ok(n) if all.try_reserve(1).is_ok() => all.push((port, n)),
                    _ => return all,
                }
            }
        }
        all
    }

    /// `hv fw ...` (`USAGE`).
    pub fn command(&self, root: &str, vms: &Vms, args: &str, out: &mut dyn Write) {
        let words: Vec<&str> = args.split_ascii_whitespace().collect();
        let switch = vms.switch_now();
        let switch = switch.as_deref();
        let done = match words.as_slice() {
            [] => self.list(switch, vms, out),
            ["help"] => {
                let _ = writeln!(out, "{}", USAGE);
                Ok(())
            }
            ["create", name, rest @ ..] => rest.iter().map(|w| policy_word(w)).collect::<Result<Vec<_>, _>>()
                .and_then(|policies| self.create(name, &policies))
                .and_then(|(inbound, outbound)| {
                    let _ = writeln!(out, "hv fw: {} made: in {}, out {}, no rule -- {}", name, inbound.name(),
                                     outbound.name(), path(name)?);
                    Ok(())
                }),
            ["delete", name] => self.delete(root, switch, name).map(|()| {
                let _ = writeln!(out, "hv fw: {} deleted", name);
            }),
            ["set", vm, name] => self.assign(root, vms, vm, name).map(|a| said_assigned(&a, name, out)),
            [name] => self.show(switch, vms, name, out),
            /* A change of its rules: the words read by the firewall crate, as
               vnet's are */
            [name, rest @ ..] if edit::parse(rest).is_some() => match edit::parse(rest) {
                Some(Ok(e)) => self.edit(switch, name, e, out),
                Some(Err(b)) => Err(refused_as(b)),
                None => Ok(()),
            },
            _ => {
                let _ = writeln!(out, "{}", USAGE);
                Ok(())
            }
        };
        if let Err(e) = done {
            let _ = writeln!(out, "hv fw: {}", e.text());
        }
    }

    /// A change of firewall `name`'s rules, as `hv fw <name> ...` asks it:
    /// made, and said with the open flows it cut.
    fn edit(&self, switch: Option<&Switch>, name: &str, e: Edit, out: &mut dyn Write) -> Result<(), Refused> {
        match e {
            Edit::Add { rule, at } => {
                let (place, cut) = self.add(switch, name, rule, at)?;
                let _ = writeln!(out, "hv fw: {}: rule {}: {}{}", name, place + 1, rule, cut_said(cut));
            }
            Edit::Del(n) => {
                let (rule, cut) = self.remove(switch, name, n)?;
                let _ = writeln!(out, "hv fw: {}: rule {} taken out: {}{}", name, n, rule, cut_said(cut));
            }
            Edit::Move { from, to } => {
                let cut = self.reorder(switch, name, from, to)?;
                let _ = writeln!(out, "hv fw: {}: rule {} is rule {} now{}", name, from, to, cut_said(cut));
            }
            Edit::Policy(policies) => {
                let ((inbound, outbound), cut) = self.policy(switch, name, &policies)?;
                let _ = writeln!(out, "hv fw: {}: in {}, out {}{}", name, inbound.name(), outbound.name(),
                                 cut_said(cut));
            }
        }
        Ok(())
    }

    /// `hv fw`: each firewall of the host's, its policies, its rules and
    /// the VMs on it.
    fn list(&self, switch: Option<&Switch>, vms: &Vms, out: &mut dyn Write) -> Result<(), Refused> {
        for row in self.rows(None)? {
            match &row.read {
                Ok(s) if !row.made => {
                    let _ = writeln!(out, "{}: in {}, out {}, no rule -- made the first time a VM names none", row.name,
                                     s.inbound.name(), s.outbound.name());
                }
                Ok(s) => {
                    let _ = write!(out, "{}: in {}, out {}, {} rule{}", row.name, s.inbound.name(), s.outbound.name(),
                                   s.rules.len(), plural(s.rules.len()));
                    on_it(switch, vms, row.place, out);
                    let _ = writeln!(out);
                }
                Err(e) => {
                    let _ = writeln!(out, "{}: {}", row.name, e);
                }
            }
        }
        Ok(())
    }

    /// `hv fw <name>`: its policies, its rules -- each with the flows it
    /// let through or kept out -- and the VMs on it.
    fn show(&self, switch: Option<&Switch>, vms: &Vms, name: &str, out: &mut dyn Write) -> Result<(), Refused> {
        let rows = self.rows(Some(name))?;
        let Some(row) = rows.first() else { return Err(none_called(name)) };
        let s = row.read.as_ref().map_err(|e| Refused::Failed(e.clone()))?;
        let hits = match (row.place, switch) {
            (Some(place), Some(sw)) => sw.hits(place).unwrap_or(Hits::NONE),
            _ => Hits::NONE,
        };
        let _ = writeln!(out, "firewall {}: in {}, out {} -- {}{}", name, s.inbound.name(), s.outbound.name(),
                         path(name)?, if row.made { "" } else { ", made the first time a VM names none" });
        for (j, text) in s.rules.iter().enumerate() {
            let _ = writeln!(out, "  {:2}  {:56} {} flow{}", j + 1, text, hits.rules[j], plural(hits.rules[j] as usize));
        }
        let _ = write!(out, "  policies: in {} -- {} flow{}, out {} -- {} flow{}", s.inbound.name(), hits.policy_in,
                       plural(hits.policy_in as usize), s.outbound.name(), hits.policy_out,
                       plural(hits.policy_out as usize));
        let _ = writeln!(out);
        let _ = write!(out, "  ");
        on_it(switch, vms, row.place, out);
        let _ = writeln!(out);
        if s.rules.len() == RULES_MAX {
            let _ = writeln!(out, "  (the most rules a firewall has)");
        }
        Ok(())
    }
}

/// What the shell says of open flows a change cut.
fn cut_said(cut: usize) -> String {
    if cut == 0 {
        String::new()
    } else {
        alloc::format!(" -- {} open flow{} of its VMs' cut", cut, plural(cut))
    }
}

/// What `hv fw set` says it did.
fn said_assigned(a: &Assigned, name: &str, out: &mut dyn Write) {
    let mut said = String::new();
    match (a.running, a.live) {
        (Some(id), true) => {
            let _ = write!(said, "vm {} judged by {} from now on", id, name);
            if a.cut != 0 {
                let _ = write!(said, " -- {} open flow{} it no longer lets through cut", a.cut, plural(a.cut));
            }
        }
        (Some(id), false) => {
            let _ = write!(said, "vm {} has no NIC: its firewall is for when it has", id);
        }
        (None, _) => {}
    }
    if let Some(kept) = &a.kept {
        let _ = write!(said, "{}{}'s vm.toml says firewall = \"{}\"", if said.is_empty() { "" } else { "; " }, kept, name);
    }
    let _ = writeln!(out, "hv fw: {}", said);
}

/// The VMs running on the firewall in place `place`, said after what came
/// before on the line.
fn on_it(switch: Option<&Switch>, vms: &Vms, place: Option<usize>, out: &mut dyn Write) {
    let ports = match (place, switch) {
        (Some(place), Some(s)) => s.ports_on(place),
        _ => Vec::new(),
    };
    if ports.is_empty() {
        let _ = write!(out, "; no VM running on it");
        return;
    }
    let _ = write!(out, "; running on it:");
    for port in ports {
        match vms.on_port(port) {
            Some(who) => {
                let _ = write!(out, " {}", who);
            }
            None => {
                let _ = write!(out, " port {}", port);
            }
        }
    }
}

/// The VMs in the VMs' folder `root` whose vm.toml names firewall `name`.
fn named_by(root: &str, name: &str) -> Vec<String> {
    let mut naming = Vec::new();
    let Ok(entries) = kcore::fs::list(root) else { return naming };
    for e in entries.iter().filter(|e| e.dir) {
        let Some(vm) = core::str::from_utf8(&e.name).ok().filter(|n| store::valid_name(n)) else { continue };
        let Ok(folder) = store::folder_of(root, vm) else { continue };
        if store::load(&folder).is_ok_and(|c| c.options.firewall.as_deref() == Some(name))
            && naming.try_reserve(1).is_ok()
        {
            naming.push(String::from(vm));
        }
    }
    naming
}
