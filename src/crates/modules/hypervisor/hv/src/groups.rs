//! IOMMU groups (plans/gpu-passthrough.md): the host's functions an IOMMU
//! cannot keep apart, because what one sends may reach another peer to peer
//! without passing it -- which must be one guest's whole, or the host's.
//!
//! A request goes up from its function towards the root complex, and is
//! routed down to a peer wherever a port on its way lets it: a port that
//! isolates -- ACS's Source Validation, P2P Request and Completion Redirect
//! and Upstream Forwarding on, each where the port has it (PCIe 6.0, 6.12) --
//! sends it on up instead, and the IOMMU sees it; one that does not hands it
//! to whatever is beside it. So where a port on a function's way up does not
//! isolate, the function reaches everything below the bridge above that
//! port: its own siblings, and every other port's subtree on that bus -- a
//! switch's downstream port without ACS joins the whole switch's subtree
//! into one group, where Linux's `pci_device_group` would keep each port's
//! apart. A root port without ACS joins what is below it, and what is below
//! every other function of its device -- a device of several root ports
//! routes between them inside -- but no other root port's (the root complex
//! is taken, as Linux takes it, to route no request from one root port to
//! another); so does any port above the function on the way; a
//! function of a device of several with no ACS of its own reaches every
//! other function of it, one with ACS among them -- where Linux joins only
//! those without; a conventional PCI bridge, or a PCIe-to-PCI one, isolates
//! nothing, and what is below it reaches its IOMMU by the
//! bridge's requester id, not its own (`behind_conventional`).
//!
//! Pure -- the bus as the caller read it in (`passthrough`'s), groups out --
//! so that the hv fuzzer holds it to a model of its own (its `groups`
//! target).

use alloc::vec::Vec;

/// ACS's capability and control registers' bits isolation needs (PCIe 6.0,
/// 7.7.8.2): Source Validation, P2P Request Redirect, P2P Completion
/// Redirect, Upstream Forwarding.
pub const ACS_SV: u16 = 1 << 0;
pub const ACS_RR: u16 = 1 << 2;
pub const ACS_CR: u16 = 1 << 3;
pub const ACS_UF: u16 = 1 << 4;
pub const ACS_NEEDED: u16 = ACS_SV | ACS_RR | ACS_CR | ACS_UF;

/// The deepest a walk up the bus goes: a bus has one bridge above it, and
/// 256 buses at most.
const DEPTH_MAX: usize = 256;

/// What a function is, as isolation goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A PCIe endpoint, legacy endpoint or integrated one, or a switch's
    /// upstream port: alone, it needs no ACS; one of several functions, its
    /// ACS keeps the others' requests from it.
    Endpoint,
    /// A root port or a switch's downstream port: it isolates what is below
    /// it by ACS alone (PCIe 6.0, 6.12.1.1).
    Port,
    /// A conventional PCI function, a PCIe-to-PCI bridge, an event
    /// collector: it isolates nothing (Linux's `pci_acs_enabled`).
    Conventional,
}

/// A function of the host's, as the caller read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Function {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub kind: Kind,
    /// One of several functions of its device: its function 0's header
    /// type says so.
    pub multi: bool,
    /// Its ACS capability and control registers, where it has the
    /// capability.
    pub acs: Option<(u16, u16)>,
    /// A bridge's buses below it, secondary to subordinate.
    pub below: Option<(u8, u8)>,
}

impl Function {
    /// Whether its ACS has on what isolation needs, of what it has: a
    /// feature it has not is one it does without (Linux's
    /// `pci_acs_flags_enabled`).
    pub fn acs_on(&self) -> bool {
        self.acs.is_some_and(|(cap, ctrl)| ctrl & cap & ACS_NEEDED == cap & ACS_NEEDED)
    }

    /// Whether it sends a request from below it, or from a function beside
    /// it in its device, on up rather than to a peer.
    pub fn isolates(&self) -> bool {
        match self.kind {
            Kind::Conventional => false,
            Kind::Port => self.acs_on(),
            Kind::Endpoint => !self.multi || self.acs_on(),
        }
    }

    /// Whether a function on `bus` is below it.
    fn holds(&self, bus: u8) -> bool {
        self.below.is_some_and(|(secondary, subordinate)| secondary <= bus && bus <= subordinate)
    }
}

/// The bridge whose bus below is `bus`: the one a request from there goes
/// up through. None on the root bus -- or where two claim it, a bus this
/// cannot be sure of.
fn above(all: &[Function], bus: u8) -> Option<usize> {
    let mut found = None;
    for (i, f) in all.iter().enumerate() {
        if f.below.is_some_and(|(secondary, _)| secondary == bus) {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

/// What a request of `all[a]`'s may reach without its IOMMU: every function
/// beside it in its device where it does not isolate them, and for each
/// port on its way up that does not isolate, everything below the bridge
/// above that port -- or, for one on the root bus, its device's every
/// function and everything below each. Into `out`, `a` among it.
fn reach(all: &[Function], a: usize, out: &mut Vec<usize>) {
    let me = all[a];
    out.push(a);
    if me.multi && !me.isolates() {
        out.extend((0..all.len()).filter(|&g| g != a && all[g].bus == me.bus && all[g].slot == me.slot));
    }
    let mut bus = me.bus;
    for _ in 0..DEPTH_MAX {
        let Some(p) = above(all, bus) else { return };
        let port = all[p];
        if !port.isolates() {
            out.push(p);
            match above(all, port.bus) {
                Some(c) => {
                    let wide = all[c];
                    out.extend((0..all.len()).filter(|&g| wide.holds(all[g].bus)));
                }
                None => {
                    let device = |h: &Function| h.bus == port.bus && h.slot == port.slot;
                    out.extend((0..all.len()).filter(|&g| {
                        device(&all[g]) || all.iter().any(|h| device(h) && h.holds(all[g].bus))
                    }));
                }
            }
        }
        bus = port.bus;
    }
}

/// The groups of `all`: an index for each function, the same for two
/// functions exactly where they are of one group -- each joined to every
/// function it reaches (`reach`), and so to every one that reaches it. None
/// for no memory, and for a bus this cannot be sure of: two bridges with one
/// bus below them, or a bridge whose bus below is its own.
pub fn groups(all: &[Function]) -> Option<Vec<usize>> {
    for (i, f) in all.iter().enumerate() {
        let Some((secondary, _)) = f.below else { continue };
        if secondary == f.bus || all[..i].iter().any(|g| g.below.is_some_and(|(s, _)| s == secondary)) {
            return None;
        }
    }
    let mut parent = Vec::new();
    parent.try_reserve_exact(all.len()).ok()?;
    parent.extend(0..all.len());
    let mut reached = Vec::new();
    reached.try_reserve(all.len()).ok()?;
    for a in 0..all.len() {
        reached.clear();
        reach(all, a, &mut reached);
        for &b in &reached {
            let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
            if ra != rb {
                parent[ra.max(rb)] = ra.min(rb);
            }
        }
    }
    Some((0..all.len()).map(|i| root(&mut parent, i)).collect())
}

/// The root of `i`'s set, the path to it halved on the way.
fn root(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

/// The conventional bridge on `all[a]`'s way up, where there is one: what
/// is below it reaches its IOMMU by the bridge's requester id, so no domain
/// of its own is to be had for it.
pub fn behind_conventional(all: &[Function], a: usize) -> Option<usize> {
    let mut bus = all[a].bus;
    for _ in 0..DEPTH_MAX {
        let p = above(all, bus)?;
        if all[p].kind == Kind::Conventional {
            return Some(p);
        }
        bus = all[p].bus;
    }
    None
}

/// The port that makes `all[a]` reach beyond itself and its device, where
/// one does: the nearest on its way up that does not isolate -- what a
/// refusal names.
pub fn open_port(all: &[Function], a: usize) -> Option<usize> {
    let mut bus = all[a].bus;
    for _ in 0..DEPTH_MAX {
        let p = above(all, bus)?;
        if !all[p].isolates() {
            return Some(p);
        }
        bus = all[p].bus;
    }
    None
}
