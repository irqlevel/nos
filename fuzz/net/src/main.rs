//! The network layer, fuzzed on the host.
//!
//! Everything the network hands the kernel is somebody else's to choose --
//! every frame on the wire, every answer a server gives the HTTP client, the
//! DHCP client and the resolver, every byte an SSH client sends the server
//! -- so a panic, an overflow, a lock broken or a loop that never ends
//! anywhere on those paths is one somebody else can cause. This program is
//! the kernel's own crates -- `net`, `tls`, `fs`, `ssh`, the sshd module's
//! source, over `kcore` and `ffi` as they are -- linked with the rest of a kernel
//! written for the purpose: its C++ half (`machine`: the locks, tasks, soft
//! IRQs, timers, the clock, the entropy pool, the command table), a NIC
//! whose wire is the fuzzer's, and the network around the machine
//! (`world`: the hosts on it -- a gateway, a DNS server, a DHCP server, an
//! HTTP server, a TLS server, an SSH client, a stranger -- each speaking its
//! protocol, well and badly). Each target turns random bytes into what the
//! world does to the machine, and what the machine's programs ask of it;
//! the runner feeds it random bytes from a seed, each input in a process of
//! its own forked from one booted machine -- the network layer's statics
//! are the kernel's, and one input's must not leak into the next -- and
//! catches a panic, a broken invariant, a spin, a crash and a hang, each
//! reported with the seed and iteration that make it again.
#![allow(dead_code)]

extern crate alloc;

#[macro_use]
#[path = "../../common/mod.rs"]
mod common;
mod machine;
mod targets;
mod world;

pub use common::input::{self, Input};
pub use common::runner::Target;

#[global_allocator]
static HEAP: machine::heap::Heap = machine::heap::Heap;

fn main() {
    common::runner::main(&common::runner::Fuzzer {
        name: "net-fuzz",
        targets: targets::ALL,
        boot: machine::boot,
        stats_env: "NET_FUZZ_STATS",
    });
}
