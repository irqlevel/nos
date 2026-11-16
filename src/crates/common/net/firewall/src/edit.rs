//! What a person asks of a firewall's rules -- at the shell, or through the
//! bus -- read once for the host's firewalls (`hv fw`) and the cluster's
//! (`vnet fw`): the words after its name, `add <rule> [at=N]`, `del <N>`,
//! `move <N> <M>`, `policy in=allow|deny out=allow|deny`, a rule's number
//! as a person says it -- 1 for the first -- and the change made to a list of
//! rules, as the cluster's state keeps them.

use alloc::string::String;
use alloc::vec::Vec;

use crate::rules::{Action, Dir, Rule, RULES_MAX};

/// A change asked of a firewall's rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// A rule in at place `at`, 0 for the first -- or last.
    Add { rule: Rule, at: Option<usize> },
    /// The rule of that number taken out, as a person numbers them.
    Del(usize),
    /// The rule numbered `from` moved to number `to`.
    Move { from: usize, to: usize },
    /// A policy each way given.
    Policy(Vec<(Dir, Action)>),
}

/// Why the words are no change: said, and the parameter of a provider's
/// call that gave what was wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bad {
    pub why: String,
    pub field: &'static str,
}

fn bad(why: String, field: &'static str) -> Bad {
    Bad { why, field }
}

/// The change `words` ask -- what follows a firewall's name -- or None for
/// words that are no change of its rules.
pub fn parse(words: &[&str]) -> Option<Result<Edit, Bad>> {
    Some(match words {
        ["add", rule @ ..] if !rule.is_empty() => {
            let (at, rule) = match rule.split_last() {
                Some((last, before)) if last.starts_with("at=") => (Some(&last[3..]), before),
                _ => (None, rule),
            };
            let text = rule.join(" ");
            let at = match at.map(|w| w.parse::<usize>()) {
                Some(Ok(n)) if n >= 1 => Ok(Some(n - 1)),
                Some(_) => Err(bad(alloc::format!("at={} is not a place -- 1 for the first", at.unwrap_or("")), "at")),
                None => Ok(None),
            };
            at.and_then(|at| {
                Rule::parse(&text).map(|rule| Edit::Add { rule, at })
                    .map_err(|why| bad(alloc::format!("\"{}\": {}", text, why), "rule"))
            })
        }
        ["del", n] => n.parse::<usize>().map(Edit::Del)
            .map_err(|_| bad(alloc::format!("\"{}\" is not a rule's number", n), "n")),
        ["move", from, to] => match (from.parse::<usize>(), to.parse::<usize>()) {
            (Ok(from), Ok(to)) => Ok(Edit::Move { from, to }),
            _ => Err(bad(alloc::format!("\"{} {}\" are not rules' numbers", from, to), "n")),
        },
        ["policy", rest @ ..] if !rest.is_empty() => {
            rest.iter().map(|w| policy_word(w)).collect::<Result<Vec<_>, _>>().map(Edit::Policy)
        }
        _ => return None,
    })
}

/// `in=allow`, `out=deny`, ...: a direction's policy.
pub fn policy_word(word: &str) -> Result<(Dir, Action), Bad> {
    let wrong = || bad(alloc::format!("\"{}\" is not a policy -- in=allow|deny or out=allow|deny", word), "in");
    let (dir, action) = word.split_once('=').ok_or_else(wrong)?;
    let dir = match dir {
        "in" => Dir::In,
        "out" => Dir::Out,
        _ => return Err(wrong()),
    };
    Ok((dir, Action::parse(action).ok_or_else(wrong)?))
}

/// A rule's number as a person says it -- 1 for the first -- of `count`, as
/// its place; `field` the provider's parameter that gave it.
pub fn number(n: usize, count: usize, field: &'static str) -> Result<usize, Bad> {
    if (1..=count).contains(&n) {
        Ok(n - 1)
    } else if count == 0 {
        Err(bad(String::from("the firewall has no rule"), field))
    } else {
        Err(bad(alloc::format!("{} is not a rule's number -- 1 to {}", n, count), field))
    }
}

/// `e` made to a firewall kept as a list of rules and its policies -- the
/// cluster's state keeps them so: what was done, said, or why not.
pub fn apply_list(e: &Edit, rules: &mut Vec<Rule>, inbound: &mut Action, outbound: &mut Action) -> Result<String, Bad> {
    match e {
        Edit::Add { rule, at } => {
            if rules.len() >= RULES_MAX {
                return Err(bad(String::from("a firewall has sixty-four rules at most"), "rule"));
            }
            let at = at.unwrap_or(rules.len());
            if at > rules.len() {
                return Err(bad(String::from("there is no such place among the rules"), "at"));
            }
            rules.insert(at, *rule);
            Ok(alloc::format!("rule {}: {}", at + 1, rule))
        }
        Edit::Del(n) => {
            let at = number(*n, rules.len(), "n")?;
            let rule = rules.remove(at);
            Ok(alloc::format!("rule {} taken out: {}", n, rule))
        }
        Edit::Move { from, to } => {
            let f = number(*from, rules.len(), "n")?;
            let t = number(*to, rules.len(), "to")?;
            let rule = rules.remove(f);
            rules.insert(t, rule);
            Ok(alloc::format!("rule {} is rule {} now", from, to))
        }
        Edit::Policy(list) => {
            for (dir, action) in list {
                match dir {
                    Dir::In => *inbound = *action,
                    Dir::Out => *outbound = *action,
                }
            }
            Ok(alloc::format!("in {}, out {}", inbound.name(), outbound.name()))
        }
    }
}
