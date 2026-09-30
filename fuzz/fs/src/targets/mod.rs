//! The targets: each turns its input into disks and into what the kernel is
//! asked to do with them, and holds what comes of it to what the format,
//! and the layer's own documentation, say.

mod disklog;
mod ext2;
mod ext2bad;
mod fsops;
mod nanofs;
mod part;
mod rootfs;
mod shell;
mod vfs;

use crate::machine::sched;
use crate::{Input, Target};

/// What every input must leave: no task still running, none that ended
/// holding a lock -- the disk log's writer stopped, every task a target
/// started joined.
fn audit() {
    for (id, name, kind, st, holds) in sched::tasks() {
        invariant!(st == sched::St::Done, "task {} '{}' ({:?}) is still {:?} at the end", id, name, kind, st);
        invariant!(holds.is_empty(), "task {} '{}' ended holding {:?}", id, name, holds);
    }
}

macro_rules! audited {
    ($f:path) => {{
        fn run(r: &mut Input) {
            $f(r);
            audit();
        }
        run
    }};
}

pub static ALL: &[Target] = &[
    Target { name: "part", run: audited!(part::part), max_len: 2048, gate: 20000 },
    Target { name: "disklog", run: audited!(disklog::disklog), max_len: 1024, gate: 5000 },
    Target { name: "ext2", run: audited!(ext2::ext2), max_len: 8192, gate: 5000 },
    Target { name: "ext2bad", run: audited!(ext2bad::ext2bad), max_len: 2048, gate: 10000 },
    Target { name: "nanofs", run: audited!(nanofs::nanofs), max_len: 8192, gate: 2000 },
    Target { name: "nanofsbad", run: audited!(nanofs::nanofsbad), max_len: 2048, gate: 5000 },
    Target { name: "vfs", run: audited!(vfs::vfs), max_len: 8192, gate: 4000 },
    Target { name: "shell", run: audited!(shell::shell), max_len: 2048, gate: 2000 },
    Target { name: "rootfs", run: audited!(rootfs::rootfs), max_len: 256, gate: 3000 },
];
