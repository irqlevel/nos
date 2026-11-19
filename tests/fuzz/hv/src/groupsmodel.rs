//! IOMMU groups (`modules/hypervisor/hv/src/groups.rs`) held to the tree
//! they are made of: a random PCI hierarchy -- root ports and devices of
//! several of them, switches, endpoints of one function or several,
//! integrated ones, conventional bridges and devices, ACS here and there, all
//! of it or a part -- its buses numbered as firmware numbers them, depth
//! first; and a judge that knows which function is below which bridge from
//! the tree itself, where the code reads it back from bus numbers.
//!
//! The judge's rules are the policy the module keeps (groups.rs's comment):
//! a function of a device of several with no ACS of its own is one group
//! with all of its device; a function below a port that does not isolate is
//! one group with everything below the bridge above that port -- or, for a
//! root port, with its device's every function and everything below each;
//! every pair so joined, and the closure. Then the partition the code makes
//! must be the judge's exactly: a function grouped too little is a guest's
//! device another can reach peer to peer, one grouped too much a device
//! refused for nothing. And the conventional bridge a function is below,
//! and the port that opens its group, as the judge finds them on the way
//! up. A bus two bridges claim, or one whose bridge is on it itself, is one
//! the code must refuse to group at all.

use crate::groups::{self, Function, Kind};
use crate::targets::Input;

const NEEDED: u16 = 0x1D; // SV, RR, CR, UF

/// A device of the tree: its functions, and for a bridge function the
/// devices on the bus below it; the bridge function it is below.
struct Device {
    functions: Vec<Function>,
    below: Vec<Vec<usize>>,
    parent: Option<(usize, usize)>,
}

struct Tree {
    devices: Vec<Device>,
    roots: Vec<usize>,
}

fn acs(r: &mut Input) -> Option<(u16, u16)> {
    match r.u8() % 4 {
        0 => None,
        1 => {
            let cap = r.u16() & 0x7F;
            Some((cap, cap & NEEDED))
        }
        _ => Some((r.u16() & 0x7F, r.u16() & 0x7F)),
    }
}

impl Tree {
    /// A device of `n` functions of `kind`, bridges or not, its ACS of the
    /// input's, below the bridge function `parent`.
    fn add(&mut self, r: &mut Input, kind: Kind, n: usize, bridge: bool, parent: Option<(usize, usize)>) -> usize {
        let multi = n > 1;
        let functions = (0..n)
            .map(|f| Function { bus: 0, slot: 0, func: f as u8, kind, multi, acs: acs(r),
                                below: bridge.then_some((0, 0)) })
            .collect();
        self.devices.push(Device { functions, below: vec![Vec::new(); n], parent });
        let d = self.devices.len() - 1;
        match parent {
            Some((p, f)) => self.devices[p].below[f].push(d),
            None => self.roots.push(d),
        }
        d
    }

    /// What is below a root port's or a downstream port's function: one
    /// device on its link -- an endpoint of one function or several, a
    /// switch, a PCIe-to-PCI bridge with conventional devices behind it --
    /// or none.
    fn below_port(&mut self, r: &mut Input, port: (usize, usize), depth: usize) {
        match r.u8() % 8 {
            0 => {}
            1 | 2 if depth < 3 => {
                let up = self.add(r, Kind::Endpoint, 1, true, Some(port));
                for _ in 0..1 + r.below(4) {
                    let down = self.add(r, Kind::Port, 1, true, Some((up, 0)));
                    self.below_port(r, (down, 0), depth + 1);
                }
            }
            3 if depth < 3 => {
                let b = self.add(r, Kind::Conventional, 1, true, Some(port));
                for _ in 0..1 + r.below(3) {
                    let n = 1 + r.below(2) as usize;
                    self.add(r, Kind::Conventional, n, false, Some((b, 0)));
                }
            }
            _ => {
                let n = if r.u8() < 160 { 1 } else { 2 + r.below(3) as usize };
                self.add(r, Kind::Endpoint, n, false, Some(port));
            }
        }
    }

    fn make(r: &mut Input) -> Tree {
        let mut t = Tree { devices: Vec::new(), roots: Vec::new() };
        for _ in 0..1 + r.below(5) {
            match r.u8() % 6 {
                0 => {
                    let n = if r.bool() { 1 } else { 2 + r.below(3) as usize };
                    t.add(r, Kind::Endpoint, n, false, None);
                }
                1 if r.u8() < 64 => {
                    t.add(r, Kind::Conventional, 1, false, None);
                }
                _ => {
                    /* A root port, or a device of several of them, each
                     * with its own link */
                    let n = if r.u8() < 200 { 1 } else { 2 + r.below(3) as usize };
                    let rp = t.add(r, Kind::Port, n, true, None);
                    for f in 0..n {
                        t.below_port(r, (rp, f), 0);
                    }
                }
            }
        }
        t
    }

    /// The buses numbered depth first, as firmware numbers them: each device
    /// a slot of its bus, each bridge function's bus below it the next free
    /// one, its subordinate the last below it.
    fn number(&mut self) {
        let mut next = 1u16;
        let roots = self.roots.clone();
        for (slot, &d) in roots.iter().enumerate() {
            self.place(d, 0, slot as u8, &mut next);
        }
    }

    fn place(&mut self, d: usize, bus: u8, slot: u8, next: &mut u16) {
        for f in self.devices[d].functions.iter_mut() {
            f.bus = bus;
            f.slot = slot;
        }
        for k in 0..self.devices[d].functions.len() {
            if self.devices[d].functions[k].below.is_none() {
                continue;
            }
            let secondary = (*next).min(255) as u8;
            *next += 1;
            let below = self.devices[d].below[k].clone();
            for (s, &c) in below.iter().enumerate() {
                self.place(c, secondary, s as u8, next);
            }
            let subordinate = (*next - 1).min(255) as u8;
            self.devices[d].functions[k].below = Some((secondary, subordinate));
        }
    }

    /// Every function, flat, and the device and function number each is.
    fn flat(&self) -> (Vec<Function>, Vec<(usize, usize)>) {
        let mut all = Vec::new();
        let mut of = Vec::new();
        for (d, dev) in self.devices.iter().enumerate() {
            for (k, f) in dev.functions.iter().enumerate() {
                all.push(*f);
                of.push((d, k));
            }
        }
        (all, of)
    }

    /// Every device below the bridge function `(d, k)`, by the tree.
    fn subtree(&self, d: usize, k: usize, out: &mut Vec<usize>) {
        for &c in &self.devices[d].below[k] {
            out.push(c);
            for ck in 0..self.devices[c].functions.len() {
                self.subtree(c, ck, out);
            }
        }
    }
}

/// The judge's own: whether `f` sends on up what comes from below it, or
/// from beside it in its device (PCIe 6.0, 6.12).
fn isolates(f: &Function) -> bool {
    let on = |f: &Function| f.acs.is_some_and(|(cap, ctrl)| (ctrl & cap & NEEDED) == (cap & NEEDED));
    match f.kind {
        Kind::Conventional => false,
        Kind::Port => on(f),
        Kind::Endpoint => !f.multi || on(f),
    }
}

fn find(p: &mut [usize], mut i: usize) -> usize {
    while p[i] != i {
        i = p[i];
    }
    i
}

fn join(p: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(p, a), find(p, b));
    if ra != rb {
        p[ra] = rb;
    }
}

pub fn groups_target(r: &mut Input) {
    let mut tree = Tree::make(r);
    tree.number();
    let (mut all, of) = tree.flat();
    if all.len() > 200 {
        return;
    }
    /* Now and then a bus two bridges claim, or a bridge on its own bus: a
     * bus the code cannot be sure of */
    let bridges: Vec<usize> = (0..all.len()).filter(|&i| all[i].below.is_some()).collect();
    let broken = !bridges.is_empty() && r.u8() < 16;
    if broken {
        let a = bridges[r.below(bridges.len() as u64) as usize];
        let b = bridges[r.below(bridges.len() as u64) as usize];
        if a == b {
            all[a].below = Some((all[a].bus, all[a].bus));
        } else {
            all[a].below = all[b].below;
        }
    }
    let got = groups::groups(&all);
    if broken {
        invariant!(got.is_none(), "a bus claimed twice, or a bridge on its own, grouped: {:?}", got);
        return;
    }
    let Some(got) = got else {
        invariant!(false, "a bus the code would not group: {:?}", all);
        return;
    };

    /* The judge's groups: by the tree */
    let n = all.len();
    let index = |d: usize, k: usize| -> usize { (0..n).find(|&i| of[i] == (d, k)).unwrap() };
    let functions_of = |d: usize| -> Vec<usize> { (0..n).filter(|&i| of[i].0 == d).collect() };
    let mut p: Vec<usize> = (0..n).collect();
    for a in 0..n {
        let (d, _) = of[a];
        if all[a].multi && !isolates(&all[a]) {
            for g in functions_of(d) {
                join(&mut p, a, g);
            }
        }
        let mut at = tree.devices[d].parent;
        while let Some((pd, pk)) = at {
            let port = index(pd, pk);
            if !isolates(&all[port]) {
                join(&mut p, a, port);
                let mut below = Vec::new();
                match tree.devices[pd].parent {
                    Some((gd, gk)) => tree.subtree(gd, gk, &mut below),
                    None => {
                        below.push(pd);
                        for k in 0..tree.devices[pd].functions.len() {
                            tree.subtree(pd, k, &mut below);
                        }
                    }
                }
                for dev in below {
                    for g in functions_of(dev) {
                        join(&mut p, a, g);
                    }
                }
            }
            at = tree.devices[pd].parent;
        }
    }
    for a in 0..n {
        for b in 0..n {
            let same_want = find(&mut p, a) == find(&mut p, b);
            let same_got = got[a] == got[b];
            invariant!(same_want == same_got, "{:02x}:{:02x}.{} and {:02x}:{:02x}.{} {} one group, the tree says {}",
                       all[a].bus, all[a].slot, all[a].func, all[b].bus, all[b].slot, all[b].func,
                       if same_got { "in" } else { "not in" }, if same_want { "they are" } else { "they are not" });
        }
        /* The conventional bridge it is below, and the port that opens its
         * group, as the tree has them on the way up */
        let mut conventional = None;
        let mut open = None;
        let mut at = tree.devices[of[a].0].parent;
        while let Some((pd, pk)) = at {
            let port = index(pd, pk);
            if conventional.is_none() && all[port].kind == Kind::Conventional {
                conventional = Some(port);
            }
            if open.is_none() && !isolates(&all[port]) {
                open = Some(port);
            }
            at = tree.devices[pd].parent;
        }
        invariant!(groups::behind_conventional(&all, a) == conventional, "the conventional bridge above {:?}: {:?}, \
                   not {:?}", all[a], groups::behind_conventional(&all, a), conventional);
        invariant!(groups::open_port(&all, a) == open, "the port that opens {:?}'s group: {:?}, not {:?}", all[a],
                   groups::open_port(&all, a), open);
    }
}
