//! A VM's `vm.toml` (`modules/hv/src/config.rs`): text an operator writes
//! by hand, so whatever it is, `parse` must not panic -- and what it reads,
//! `render` must write back so that `parse` reads the same again. A
//! configuration made up the input's way -- its strings full of quotes,
//! backslashes, control characters and characters past ASCII -- must come
//! back from its own rendering whole; and the rendering cut, spliced and
//! scribbled on must still be read without a panic, and what it reads still
//! hold to the round trip.
//!
//! And what hv writes into one it did not write (`rewrite`, through
//! `conf::edit`): a vm.toml with an operator's comments in it -- lines of
//! their own, and after values -- made to say another configuration, as a
//! clone, a forward or the host API's edits do, must be read back as that
//! configuration, every comment of a line of its own still there and in its
//! order. A command line made a clone's (`without_ip`) keeps every word but
//! `ip=`'s, in order.

use crate::targets::Input;
use crate::vmconfig::{parse, render, rewrite, without_ip, Config, Options};

/// What strings are made of: the plain, and what quoting gets wrong.
const CHARS: [char; 18] = ['a', 'z', '0', '9', '/', '.', '-', ' ', '"', '\\', '\'', '\n', '\t', '\r', '\u{0}',
                           '\u{7f}', 'é', '日'];

fn text(r: &mut Input, max: u64) -> String {
    (0..r.below(max)).map(|_| r.pick(&CHARS)).collect()
}

/// A path: never empty, and never one ending in `:ro` -- which a disk's
/// word reads as the disk being read-only, so it cannot stand for itself.
fn path(r: &mut Input) -> String {
    let mut p = text(r, 12);
    if p.is_empty() {
        p.push('a');
    }
    if p.ends_with(":ro") {
        p.push('x');
    }
    p
}

fn made_up(r: &mut Input) -> Config {
    let mut o = Options {
        kernel: path(r),
        ..Options::default()
    };
    if r.bool() {
        o.initrd = Some(path(r));
    }
    for _ in 0..r.below(5) {
        let p = path(r);
        o.disks.push((p, r.bool()));
    }
    if r.bool() {
        o.mem_mib = Some(r.value64());
    }
    if r.bool() {
        o.cmdline = Some(text(r, 40));
    }
    o.input = text(r, 24);
    if r.bool() {
        o.cpu = Some(r.value32());
    }
    if r.bool() {
        o.cpus = Some(r.value32());
    }
    o.xapic = r.bool();
    o.ioapic = r.bool();
    o.apicv = match r.u8() % 3 {
        0 => None,
        1 => Some(true),
        _ => Some(false),
    };
    if r.bool() {
        o.rxmod_us = Some(r.value64());
    }
    o.nooffload = r.bool();
    o.log = r.bool();
    o.restart = r.bool();
    o.net = r.bool();
    if r.bool() {
        o.port = Some(r.value32());
    }
    if r.bool() {
        /* A network's name, as a name may be. */
        let n = 1 + r.below(crate::vmconfig::NETWORK_NAME_MAX as u64) as usize;
        let name: String = (0..n).map(|i| {
            let c = r.pick(&['a', 'z', '0', '9', '.', '_', '-', 'Q']);
            if i == 0 && matches!(c, '.' | '_' | '-') { 'n' } else { c }
        }).collect();
        o.network = Some(name);
    }
    for _ in 0..r.below(4) {
        let (host, guest) = (r.u16().max(1), r.u16().max(1));
        o.forwards.push((host, guest));
    }
    if r.bool() {
        let mut u = [0u8; 16];
        for b in u.iter_mut() {
            *b = r.u8();
        }
        o.uuid = Some(u);
    }
    Config { options: o, console_log: r.bool(), protected: r.bool() }
}

/// `old` changed as hv changes a vm.toml after it is made: a clone's new
/// identity, a forward added or taken away, a disk added, a flag turned --
/// or, now and then, another configuration altogether.
fn changed(old: &Config, r: &mut Input) -> Config {
    if r.below(8) == 0 {
        return made_up(r);
    }
    let mut c = old.clone();
    let o = &mut c.options;
    if r.bool() {
        let mut u = [0u8; 16];
        for b in u.iter_mut() {
            *b = r.u8();
        }
        o.uuid = Some(u);
    }
    match r.u8() % 3 {
        0 => {}
        1 => o.forwards.clear(),
        _ => o.forwards.push((r.u16().max(1), r.u16().max(1))),
    }
    if r.bool() {
        o.port = if r.bool() { Some(r.value32()) } else { None };
    }
    if r.bool() {
        o.cmdline = o.cmdline.as_deref().map(without_ip);
    }
    if r.bool() {
        o.disks.push((path(r), r.bool()));
    }
    if r.bool() {
        c.protected = !c.protected;
    }
    if r.bool() {
        c.console_log = !c.console_log;
    }
    if r.bool() {
        o.net = !o.net;
    }
    c
}

/// A rendering as an operator's hand leaves it: comments on lines of their
/// own, blank lines, and comments after values.
fn commented(c: &Config, r: &mut Input) -> String {
    let rendered = render(c).expect("render ran out of memory");
    let mut text = String::new();
    for (i, line) in rendered.lines().enumerate() {
        if r.below(4) == 0 {
            text.push_str(if r.bool() { "\n" } else { "# the operator's own, before a line\n" });
        }
        text.push_str(line);
        /* After a value -- not the header's comment, nor a string's middle. */
        if i != 0 && r.below(4) == 0 {
            text.push_str("  # why");
        }
        text.push('\n');
    }
    if r.bool() {
        text.push_str("# and one at the end");
    }
    text
}

/// The lines of `text` that are comments of their own, in order.
fn comments(text: &str) -> Vec<&str> {
    text.lines().filter(|l| l.trim_start().starts_with('#')).collect()
}

/// What `parse` read, written back and read again: the same.
fn round_trip(c: &Config) {
    let text = render(c).expect("render ran out of memory");
    match parse(&text) {
        Ok(back) => invariant!(back == *c, "a vm.toml written from a configuration is read back as another:\n{}\n\
                                            {:?}\n{:?}", text, c, back),
        Err(e) => invariant!(false, "a vm.toml written from a configuration is not read back -- {}:\n{}", e, text),
    }
}

pub fn config_target(r: &mut Input) {
    match r.u8() % 5 {
        3 => {
            /* A hand's vm.toml rewritten to say another configuration: read
             * back as that one, its comments of their own kept in order. */
            let old = made_up(r);
            let text = commented(&old, r);
            let Ok(read) = parse(&text) else {
                invariant!(false, "a rendering with comments added is not read:\n{}", text);
                return;
            };
            invariant!(read == old, "a rendering with comments added is read as another configuration:\n{}", text);
            let new = changed(&old, r);
            let out = match rewrite(&text, &new) {
                Ok(out) => out,
                Err(e) => {
                    invariant!(false, "a vm.toml that is read is not rewritten -- {}:\n{}", e, text);
                    return;
                }
            };
            match parse(&out) {
                Ok(back) => invariant!(back == new, "a vm.toml rewritten to a configuration is read as another:\n\
                                                     {}\nfrom\n{}\n{:?}\n{:?}", out, text, new, back),
                Err(e) => invariant!(false, "a vm.toml rewritten is not read back -- {}:\n{}\nfrom\n{}", e, out, text),
            }
            invariant!(comments(&out) == comments(&text), "a rewrite lost or moved a comment of its own:\n{}\nfrom\n{}",
                       out, text);
            if old == new {
                invariant!(out == text, "a rewrite to the same configuration changed the text:\n{}\nfrom\n{}", out,
                           text);
            }
        }
        4 => {
            /* A command line made a clone's: every word but ip='s, in order. */
            let line = text(r, 40) + if r.bool() { " ip=10.0.100.5:::255.255.255.0::eth0:off" } else { "" };
            let line = if r.bool() { alloc_line(&line, r) } else { line };
            let out = without_ip(&line);
            let kept: Vec<&str> = line.split_ascii_whitespace().filter(|w| !w.starts_with("ip=")).collect();
            invariant!(out.split_ascii_whitespace().collect::<Vec<_>>() == kept,
                       "{:?} without its ip= is {:?}", line, out);
            if !line.split_ascii_whitespace().any(|w| w.starts_with("ip=")) {
                invariant!(out == line, "{:?}, which has no ip=, became {:?}", line, out);
            }
        }
        0 => {
            /* Whatever the bytes are, as text. */
            let text = String::from_utf8_lossy(r.rest()).into_owned();
            if let Ok(c) = parse(&text) {
                round_trip(&c);
            }
        }
        1 => round_trip(&made_up(r)),
        _ => {
            /* A rendering, and then what a hand does to one. */
            let c = made_up(r);
            let mut text: Vec<char> = render(&c).expect("render ran out of memory").chars().collect();
            while let Some(op) = r.op(4) {
                let at = r.below(text.len() as u64 + 1) as usize;
                match op {
                    0 => {
                        let n = (r.below(8) as usize).min(text.len() - at);
                        text.drain(at..at + n);
                    }
                    1 => text.insert(at, r.pick(&['=', '[', ']', ',', '#', '"', '\'', '\n', ' ', '_', '1', 't'])),
                    2 => text.insert(at, r.pick(&CHARS)),
                    _ => {
                        /* A line given twice. */
                        let line: Vec<char> = text[at..].iter().take_while(|c| **c != '\n').copied().collect();
                        text.splice(at..at, line.into_iter().chain(core::iter::once('\n')));
                    }
                }
            }
            let text: String = text.into_iter().collect();
            if let Ok(c) = parse(&text) {
                round_trip(&c);
            }
        }
    }
}

/// `line` with an `ip=` word put among its words, somewhere.
fn alloc_line(line: &str, r: &mut Input) -> String {
    let mut words: Vec<String> = line.split(' ').map(String::from).collect();
    let at = r.below(words.len() as u64 + 1) as usize;
    words.insert(at, String::from("ip=dhcp"));
    words.join(" ")
}
