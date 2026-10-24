//! A VM's `vm.toml` (`modules/hv/src/config.rs`): text an operator writes
//! by hand, so whatever it is, `parse` must not panic -- and what it reads,
//! `render` must write back so that `parse` reads the same again. A
//! configuration made up the input's way -- its strings full of quotes,
//! backslashes, control characters and characters past ASCII -- must come
//! back from its own rendering whole; and the rendering cut, spliced and
//! scribbled on must still be read without a panic, and what it reads still
//! hold to the round trip.

use crate::targets::Input;
use crate::vmconfig::{parse, render, Config, Options};

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
    for _ in 0..r.below(4) {
        let (host, guest) = (r.u16().max(1), r.u16().max(1));
        o.forwards.push((host, guest));
    }
    Config { options: o, console_log: r.bool() }
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
    match r.u8() % 3 {
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
