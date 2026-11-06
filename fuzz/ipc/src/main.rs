//! The kernel's message channel, fuzzed on the host.
//!
//! Every service of the kernel's talks through the channel -- the image's
//! tasks and the modules alike -- so a message lost, doubled or out of
//! order, a Down that never comes, a name that outlives its mailbox or a
//! queue that holds more than it says is a bug in every one of them at
//! once. This program is the kernel's own `ipc` crate, over `kcore` and
//! `ffi` as they are, linked with the rest of a kernel written for the
//! purpose (`machine`: the fuzzers' common one, and the one thing of the
//! C++ the channel asks that it has not: which module an address is in).
//!
//! Its target (`target`) turns random bytes into mailboxes of the image's
//! and of made-up modules, and into what is done with them -- messages by
//! address and by name, names, monitors, watches, receivers that sleep
//! until a message, a wake or a close, modules unloaded, the halt -- and
//! holds every answer and every queue to a model of its own: each message
//! received once, in order, with the bytes it was sent with; every Down
//! exactly once; the names and the counters what the model says.
#![allow(dead_code)]

extern crate alloc;

#[macro_use]
#[path = "../../common/mod.rs"]
mod common;
mod machine;
mod target;
mod wire;

pub use common::input::{self, Input};
pub use common::runner::{reached, Target};

#[global_allocator]
static HEAP: common::machine::heap::Heap = common::machine::heap::Heap;

fn main() {
    common::runner::main(&common::runner::Fuzzer {
        name: "ipc-fuzz",
        targets: target::ALL,
        boot: machine::boot,
        begin: || {},
        stats_env: "IPC_FUZZ_STATS",
    });
}
