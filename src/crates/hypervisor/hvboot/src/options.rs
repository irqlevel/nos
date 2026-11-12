//! The loader's command line: its own words -- `hvboot.<name>[=<value>]` --
//! and the rest, which go on to the kernel after its entry's own: what hv
//! adds a guest's line (the TSC's rate, `ip=`, `noapic`) and what the VM's
//! `cmdline=` says. Its words are where Linux finds its own: at blanks
//! outside double quotes (`next_arg`) -- so that the kernel is given a
//! quoted word whole, blanks in it as they were, and `hvboot.entry=` can
//! name a title with a space in it, quoted.

use alloc::string::String;

pub struct Options {
    /// `hvboot.entry=`: the GRUB entry to boot in place of its default --
    /// a number, `N>M` into a submenu, a title or an id, as GRUB's
    /// `default` takes.
    pub entry: Option<String>,
    /// `hvboot.disk=N`: look for GRUB's configuration on disk N alone
    /// (0 for `vda`).
    pub disk: Option<usize>,
    /// `hvboot.verbose`: say each filesystem looked at.
    pub verbose: bool,
    /// `tsc_early_khz=`, for timeouts: the TSC's rate, as hv knows it.
    pub tsc_khz: Option<u64>,
    /// Every other word, in order, a space between: what the kernel is
    /// given after its entry's line.
    pub kernel_line: String,
}

/// Linux's blanks (`isspace`).
fn blank(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c')
}

/// Each word of `line`, where Linux's `next_arg` ends one: at a blank that
/// is not between double quotes.
fn words(line: &str) -> impl Iterator<Item = &str> {
    let b = line.as_bytes();
    let mut at = 0;
    core::iter::from_fn(move || {
        while at < b.len() && blank(b[at]) {
            at += 1;
        }
        if at == b.len() {
            return None;
        }
        let from = at;
        let mut quoted = false;
        while at < b.len() && (quoted || !blank(b[at])) {
            if b[at] == b'"' {
                quoted = !quoted;
            }
            at += 1;
        }
        Some(&line[from..at])
    })
}

/// A value with the double quotes around it taken off, as Linux takes them
/// off a parameter's.
fn unquote(v: &str) -> &str {
    match v.strip_prefix('"') {
        Some(inner) => inner.strip_suffix('"').unwrap_or(inner),
        None => v,
    }
}

impl Options {
    pub fn parse(line: &str) -> Result<Options, String> {
        let mut o = Options { entry: None, disk: None, verbose: false, tsc_khz: None, kernel_line: String::new() };
        for word in words(line) {
            if let Some(own) = word.strip_prefix("hvboot.") {
                let (name, value) = match own.split_once('=') {
                    Some((n, v)) => (n, Some(unquote(v))),
                    None => (own, None),
                };
                match (name, value) {
                    ("entry", Some(v)) if !v.is_empty() => o.entry = Some(String::from(v)),
                    ("disk", Some(v)) => {
                        o.disk = Some(v.parse().map_err(|_| alloc::format!("hvboot.disk={}: not a number", v))?);
                    }
                    ("verbose", None) => o.verbose = true,
                    _ => return Err(alloc::format!("{}: not an option of hvboot's (hvboot.entry=, hvboot.disk=, \
                                                    hvboot.verbose)", word)),
                }
                continue;
            }
            if let Some(khz) = word.strip_prefix("tsc_early_khz=") {
                o.tsc_khz = khz.parse().ok().filter(|&k| k != 0);
            }
            if !o.kernel_line.is_empty() {
                o.kernel_line.push(' ');
            }
            o.kernel_line.push_str(word);
        }
        Ok(o)
    }
}
