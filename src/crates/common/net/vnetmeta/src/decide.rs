//! What the state decides as pure functions of itself -- an address given, an
//! attach answered -- so that the fuzzer holds them as it holds `apply`
//! (plans/vnet.md: no gate on tests/hub.py runs in CI).

use alloc::collections::BTreeSet;

use crate::{Network, Port, State};

/// The address a port is given where it asks for none: the next free one
/// round the subnet from where the last was given -- past it, so that an
/// address let go of is not the next one given -- that a port may have.
/// None where every one is taken.
pub(crate) fn next_free(n: &Network, taken: &BTreeSet<(u32, u32)>) -> Option<u32> {
    let size = 1u64 << (32 - u32::from(n.prefix));
    let start = if n.holds(n.next) { n.next } else { n.net };
    let mut a = start;
    for _ in 0..size {
        if n.may_have(a) && !taken.contains(&(n.vni, a)) {
            return Some(a);
        }
        a = after(n, a);
    }
    None
}

/// The address after `a` round the subnet.
pub(crate) fn after(n: &Network, a: u32) -> u32 {
    if a >= n.broadcast() { n.net } else { a + 1 }
}

/// What an attach of a VM's NIC on node `node` is answered (plans/vnet.md,
/// "hv and vnet"): its port bound here -- with its network and its public
/// address, if any -- bound to another node, or none yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attach<'a> {
    Here { port: &'a Port, network: &'a Network, public: Option<u32> },
    /// Bound to another host: the node it is on.
    Elsewhere(u64),
    None,
}

pub fn attach<'a>(state: &'a State, uuid: &[u8; 16], nic: u8, node: u64) -> Attach<'a> {
    let Some(port) = state.port_of(uuid, nic) else { return Attach::None };
    if port.node != node {
        return Attach::Elsewhere(port.node);
    }
    match state.network_of(port.vni) {
        Some(network) => Attach::Here { port, network, public: state.public_on(port.id).map(|p| p.address) },
        /* A port's network is never deleted under it: the state's check
           holds it, so this is no state applying makes */
        None => Attach::None,
    }
}
