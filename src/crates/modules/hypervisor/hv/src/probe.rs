//! A probe (`hv probe`, plans/vnet.md): a port of the guests' switch with
//! no VM behind it -- its address and MAC the port's, on a network or
//! alone, judged by a firewall as a VM's NIC is -- whose task answers what a
//! guest's stack would of it: ARP for its address, echo, and UDP to port 7
//! (RFC 862's echo); and which sends echo requests and UDP the way a guest's
//! NIC does, through the switch's way in for a port (`PortBackend::send`):
//! its ingress check and its firewall's judgement on the way, both ways.
//! What the gates check a network and its firewall with before any guest
//! boots (tests/vnet-test.py), and what an operator checks one with on a
//! host that runs.
//!
//!     hv probe                                  the probes, and what each has answered
//!     hv probe add <name> [port=N] [network=<name>|vnet:<name>] [firewall=<name>] [public=<a.b.c.d>]
//!                  [address=<a.b.c.d>]
//!     hv probe ping <name> <a.b.c.d> [count=N] [secs=S]
//!     hv probe udp <name> <a.b.c.d> <port> [count=N] [secs=S] [sport=N]
//!     hv probe del <name>
//!
//! On a network of the cluster's (`network=vnet:lan`) a probe is a port of
//! vnet's as a VM's is (`vnet::attach`): made for it under a UUID of its
//! own, at `address=` or the next free one, judged by the cluster's firewall
//! `firewall=` names, and deleted as the probe is.
//!
//! A run -- a ping's echoes, a set of datagrams -- says how many of its
//! sends were answered, how many refused (ICMP communication
//! administratively prohibited: a firewall's, the probe's own outwards or
//! the far end's), and how many otherwise unreachable; the rest went
//! unanswered. A probe's task waits on an event the switch signals as a
//! frame goes into its inbox (`net::Wake`), and at most `TICK_MS` -- what a
//! stop is seen within. x86-64 alone, where the switch is.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use hv::nic::Backend;
use kcore::sync::{Event, Mutex};
use kcore::task::TaskHandle;
use kcore::time::Duration;
use netwire::{arp, eth, icmp, ip, udp, ARP_LEN, ARP_OP_REPLY, ARP_OP_REQUEST, ETH_HDR_LEN, ETH_TYPE_ARP,
              ETH_TYPE_IP, ICMP_HDR_LEN, IP_HDR_LEN, IP_PROTO_ICMP, IP_PROTO_UDP, MAX_FRAME, UDP_HDR_LEN};

use crate::net::{self, PortBackend, Switch, Wake};
use crate::store;
use crate::vms::Vms;

/// What `hv probe` says it takes.
pub const USAGE: &str = "\
hv probe                               the probes: each one's port, address, network and firewall, what it answered
hv probe add <name> [port=N] [network=<name>|vnet:<name>] [firewall=<name>] [public=<a.b.c.d>]
             [address=<a.b.c.d>]       a port of the switch with no VM: it answers ARP, echo and UDP to port 7;
                                       public= a public address mapped one to one to it, as a VM's is; on a
                                       network of the cluster's a port of vnet's, address= its address
hv probe ping <name> <a.b.c.d> [count=N] [secs=S]
hv probe udp <name> <a.b.c.d> <port> [count=N] [secs=S] [sport=N]
                                       sent from it as a guest's NIC sends: answered, refused or nothing;
                                       sport= the datagrams' source port, the same flow run after run
hv probe del <name>";

/// The most sends a run makes: a bit each in what it heard.
const COUNT_MAX: u64 = 64;
const COUNT_DEFAULT: u64 = 3;
/// What a run waits for its answers, unless told, and at most.
const SECS_DEFAULT: u64 = 2;
const SECS_MAX: u64 = 60;
/// RFC 862's echo, which a probe answers.
const ECHO_PORT: u16 = 7;
/// What a run's sends carry -- an echo's identifier, a datagram's source
/// port: this and the run's number among the probes' runs, so that what
/// answers an earlier run is not counted for a later one.
const TAG_BASE: u16 = 40000;
const TAGS: u16 = 20000;
/// What a probe's task waits for a frame at most: a stop is seen within it.
const TICK_MS: u64 = 100;
/// How often a run looks at what it heard, while it waits.
const LOOK_MS: u64 = 20;
/// ICMP destination unreachable's code for communication administratively
/// prohibited (RFC 1812 5.2.7.1): a firewall's no.
const ADMIN_PROHIBITED: u8 = 13;
/// What an echo request of a run carries after its header, and a datagram
/// before the number of its send.
const PAYLOAD: &[u8] = b"nos-probe";

/// What a run -- a ping's echoes, a set of datagrams -- heard back.
#[derive(Clone, Copy, Default)]
struct Run {
    /// The echo identifier, or the datagrams' source port.
    tag: u16,
    udp: bool,
    /// Each send answered, by its number less one.
    answered: u64,
    /// Refused: ICMP communication administratively prohibited about one of
    /// its sends.
    prohibited: u32,
    /// Any other ICMP error about one of its sends.
    unreachable: u32,
}

/// What a probe's task has answered and heard.
#[derive(Default)]
struct Heard {
    /// The run under way, if one is: what its answers are counted into.
    run: Option<Run>,
    echoes: u64,
    datagrams: u64,
    arps: u64,
}

/// What the probe's task, its commands and the switch share: the event the
/// switch signals as a frame goes into the port's inbox, and what was heard.
struct Shared {
    event: Event,
    stop: AtomicBool,
    heard: Mutex<Heard>,
}

impl Wake for Shared {
    fn wake_up(&self) {
        self.event.signal();
    }
}

struct Probe {
    name: String,
    port: usize,
    me: Me,
    network: Option<String>,
    firewall: String,
    public: Option<u32>,
    /// On a network of the cluster's: the UUID its port of vnet's is
    /// under, deleted as it goes -- and the keeper that keeps it till then.
    vnet: Option<[u8; 16]>,
    keeper: Option<Arc<crate::vnet::Keeper>>,
    switch: Arc<Switch>,
    shared: Arc<Shared>,
    task: Option<TaskHandle>,
}

impl Probe {
    /// Its task stopped -- the handle's drop waits for it -- and then its
    /// port given back: nothing sends from it as it goes. Its port of
    /// vnet's deleted, where it has one: what could not be, said.
    fn end(mut self) -> Option<String> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.event.signal();
        drop(self.task.take());
        if let (Some(k), Some(_)) = (&self.keeper, self.vnet) {
            k.released(self.port);
        }
        self.switch.release(self.port);
        let uuid = self.vnet?;
        crate::vnet::detach(&uuid).err()
    }
}

/// Who a probe is on its network: its address and MAC, and how it reaches
/// an address -- a port's MAC on its own subnet, the gateway's beyond it.
#[derive(Clone, Copy)]
struct Me {
    ip: u32,
    mac: [u8; 6],
    /// A cluster network's subnet, address and mask; None on a host's.
    cluster: Option<(u32, u32)>,
}

impl Me {
    /// The MAC a probe sends a packet for `addr` to: its port's, for a port
    /// on its network; the gateway's -- the way out -- for anyone else.
    fn mac_for(&self, addr: u32) -> [u8; 6] {
        match self.cluster {
            Some((net_, mask)) if addr & mask == net_ && addr != net_.wrapping_add(1) => net::vnet_mac(addr),
            Some(_) => net::HOST_MAC,
            None => net::port_at(addr).map_or(net::HOST_MAC, net::port_mac),
        }
    }
}

/// The host's probes.
pub struct Probes {
    list: Mutex<Vec<Probe>>,
    /// The next run's number.
    runs: AtomicU16,
}

/// What the probe's task is handed: what it shares, and its port.
struct Ctx {
    shared: Arc<Shared>,
    backend: PortBackend,
    me: Me,
}

/// The probe's task: each frame in its inbox answered, or counted for the
/// run under way, until it is told to stop.
fn task(mut ctx: Ctx) {
    let mut frame = [0u8; MAX_FRAME];
    let mut out = [0u8; MAX_FRAME];
    while !ctx.shared.stop.load(Ordering::Acquire) {
        ctx.shared.event.wait_for(Duration::from_millis(TICK_MS));
        while let Some(len) = ctx.backend.recv(&mut frame) {
            if let Some(n) = answer(&ctx.shared, &ctx.me, &frame[..len.min(MAX_FRAME)], &mut out) {
                ctx.backend.send(&out[..n]);
            }
        }
    }
}

/// What the probe that is `who` makes of `frame`: an answer written into
/// `out` -- its length -- where it is one to answer, and what answers or
/// refuses a run of its counted.
fn answer(s: &Shared, who: &Me, frame: &[u8], out: &mut [u8]) -> Option<usize> {
    if frame.len() < ETH_HDR_LEN {
        return None;
    }
    let (me, my_mac) = (who.ip, who.mac);
    match eth::ether_type(frame) {
        ETH_TYPE_ARP => {
            let a = frame.get(ETH_HDR_LEN..ETH_HDR_LEN + ARP_LEN)?;
            if arp::opcode(a) != ARP_OP_REQUEST || arp::target_ip(a) != me {
                return None;
            }
            let (peer_mac, peer) = (arp::sender_mac(a), arp::sender_ip(a));
            let reply = out.get_mut(..ETH_HDR_LEN + ARP_LEN)?;
            eth::write(reply, &peer_mac, &my_mac, ETH_TYPE_ARP);
            arp::write(&mut reply[ETH_HDR_LEN..], ARP_OP_REPLY, &my_mac, me, &peer_mac, peer);
            s.heard.lock().arps += 1;
            Some(ETH_HDR_LEN + ARP_LEN)
        }
        ETH_TYPE_IP => answer_ip(s, who, &frame[ETH_HDR_LEN..], out),
        _ => None,
    }
}

/// `answer`'s IPv4: `p` the packet, the frame's body.
fn answer_ip(s: &Shared, who: &Me, p: &[u8], out: &mut [u8]) -> Option<usize> {
    if p.len() < IP_HDR_LEN || ip::version(p) != 4 || ip::is_fragment(p) {
        return None;
    }
    let ihl = ip::header_len(p);
    let total = usize::from(ip::total_len(p));
    let me = who.ip;
    if ihl == 0 || total < ihl || total > p.len() || ip::dst(p) != me {
        return None;
    }
    let from = ip::src(p);
    let l4 = &p[ihl..total];
    match ip::protocol(p) {
        IP_PROTO_ICMP if l4.len() >= ICMP_HDR_LEN => match icmp::kind(l4) {
            icmp::ECHO_REQUEST => {
                /* Its data with it, as a stack answers */
                let n = ETH_HDR_LEN + IP_HDR_LEN + l4.len();
                let reply = out.get_mut(..n)?;
                eth::write(reply, &who.mac_for(from), &who.mac, ETH_TYPE_IP);
                let body = &mut reply[ETH_HDR_LEN + IP_HDR_LEN..];
                body.copy_from_slice(l4);
                icmp::write(body, icmp::ECHO_REPLY, 0, icmp::id(l4), icmp::seq(l4), l4.len());
                ip::write(&mut reply[ETH_HDR_LEN..], IP_PROTO_ICMP, me, from, l4.len(), 0);
                s.heard.lock().echoes += 1;
                Some(n)
            }
            icmp::ECHO_REPLY => {
                if let Some(run) = s.heard.lock().run.as_mut().filter(|r| !r.udp && r.tag == icmp::id(l4)) {
                    mark(run, icmp::seq(l4));
                }
                None
            }
            icmp::DEST_UNREACH | icmp::TIME_EXCEEDED => {
                refused(s, me, icmp::kind(l4), icmp::code(l4), &l4[ICMP_HDR_LEN..]);
                None
            }
            _ => None,
        },
        IP_PROTO_UDP if l4.len() >= UDP_HDR_LEN => {
            let len = usize::from(udp::length(l4)).clamp(UDP_HDR_LEN, l4.len());
            let payload = &l4[UDP_HDR_LEN..len];
            let (sport, dport) = (udp::src_port(l4), udp::dst_port(l4));
            if dport == ECHO_PORT {
                let route = udp::Route {
                    src_mac: who.mac, dst_mac: who.mac_for(from), src_ip: me, dst_ip: from,
                    src_port: ECHO_PORT, dst_port: sport, dont_fragment: false,
                };
                let n = udp::write_frame(out, &route, payload.len())?;
                out[udp::PAYLOAD_AT..n].copy_from_slice(payload);
                s.heard.lock().datagrams += 1;
                return Some(n);
            }
            /* An echo of a run's datagram: its number after the payload */
            if let Some(run) = s.heard.lock().run.as_mut().filter(|r| r.udp && r.tag == dport) {
                if let Some(&n) = payload.strip_prefix(PAYLOAD).and_then(|rest| rest.first()) {
                    mark(run, u16::from(n));
                }
            }
            None
        }
        _ => None,
    }
}

/// Send number `seq` (from 1) of `run` answered.
fn mark(run: &mut Run, seq: u16) {
    if (1..=COUNT_MAX).contains(&u64::from(seq)) {
        run.answered |= 1 << (seq - 1);
    }
}

/// An ICMP error to the probe at `me`, `quoted` the packet it is about: a
/// refusal or an unreachable of the run under way, where it is about one
/// of its sends.
fn refused(s: &Shared, me: u32, kind: u8, code: u8, quoted: &[u8]) {
    const QUOTED_L4: usize = 8;
    if quoted.len() < IP_HDR_LEN || ip::version(quoted) != 4 || ip::src(quoted) != me {
        return;
    }
    let ihl = ip::header_len(quoted);
    let Some(l4) = quoted.get(ihl..ihl + QUOTED_L4).filter(|_| ihl != 0) else { return };
    let mut heard = s.heard.lock();
    let Some(run) = heard.run.as_mut() else { return };
    let ours = match ip::protocol(quoted) {
        IP_PROTO_ICMP => !run.udp && icmp::kind(l4) == icmp::ECHO_REQUEST && icmp::id(l4) == run.tag,
        IP_PROTO_UDP => run.udp && udp::src_port(l4) == run.tag,
        _ => false,
    };
    if !ours {
        return;
    }
    if kind == icmp::DEST_UNREACH && code == ADMIN_PROHIBITED {
        run.prohibited += 1;
    } else {
        run.unreachable += 1;
    }
}

/// `key=value` among `words`, parsed by `parse`: None where it is not
/// there, an error where it is and is not one.
fn option<T>(words: &[&str], key: &str, parse: impl Fn(&str) -> Option<T>) -> Result<Option<T>, String> {
    for w in words {
        if let Some(v) = w.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return parse(v).map(Some).ok_or_else(|| alloc::format!("\"{}\" is not a value {}= takes", v, key));
        }
    }
    Ok(None)
}

/// Every word a `key=value` whose key is one of `known`.
fn known_options(words: &[&str], known: &[&str]) -> Result<(), String> {
    for w in words {
        if let Some((k, _)) = w.split_once('=') {
            if !known.contains(&k) {
                return Err(alloc::format!("\"{}\" is not a word hv probe takes here -- {}=", w,
                                          known.join("=, ")));
            }
        } else {
            return Err(alloc::format!("\"{}\" is not a word hv probe takes here", w));
        }
    }
    Ok(())
}

impl Probes {
    pub fn new() -> Option<Probes> {
        Some(Probes { list: Mutex::new(Vec::new())?, runs: AtomicU16::new(0) })
    }

    /// Every probe ended: what the module's going does before the switch
    /// goes. Taken out under the lock and ended after it, each joining its
    /// task.
    pub fn close(&self) {
        let all = core::mem::take(&mut *self.list.lock());
        for p in all {
            if let Some(why) = p.end() {
                kcore::klog!(0, "hv probe: its port of vnet's left -- {}: `vnet port delete` it", why);
            }
        }
    }

    /// `hv probe ...` (`USAGE`).
    pub fn command(&self, vms: &Vms, args: &str, out: &mut dyn Write) {
        let words: Vec<&str> = args.split_ascii_whitespace().collect();
        let done = match words.as_slice() {
            [] => self.list(out),
            ["help"] => {
                let _ = writeln!(out, "{}", USAGE);
                Ok(())
            }
            ["add", name, rest @ ..] => self.add(vms, name, rest, out),
            ["del", name] => self.del(name, out),
            ["ping", name, to, rest @ ..] => self.run(name, to, None, rest, out),
            ["udp", name, to, port, rest @ ..] => match port.parse::<u16>() {
                Ok(p) if p != 0 => self.run(name, to, Some(p), rest, out),
                _ => Err(alloc::format!("\"{}\" is not a port -- 1 to 65535", port)),
            },
            _ => {
                let _ = writeln!(out, "{}", USAGE);
                Ok(())
            }
        };
        if let Err(e) = done {
            let _ = writeln!(out, "hv probe: {}", e);
        }
    }

    fn list(&self, out: &mut dyn Write) -> Result<(), String> {
        let list = self.list.lock();
        if list.is_empty() {
            let _ = writeln!(out, "no probe -- hv probe add <name>");
        }
        for p in list.iter() {
            let h = p.shared.heard.lock();
            let public = p.public.map_or(String::new(), |a| alloc::format!(", public {}", netwire::Ipv4(a)));
            /* A port of vnet's: what the keeper says of it */
            let state = match p.keeper.as_ref().filter(|_| p.vnet.is_some()).and_then(|k| k.said(p.port)) {
                Some((_, _, Some(why))) => alloc::format!(" (shut: {})", why),
                Some((_, false, None)) => String::from(" (unconfirmed)"),
                _ => String::new(),
            };
            let _ = writeln!(out, "{}: port {}, {} ({}){}, firewall {}{}; answered {} echoes, {} datagrams, {} ARP \
                                   questions", p.name, p.port, net::dotted(p.me.ip),
                             p.network.as_deref().unwrap_or("alone"), state, p.firewall, public, h.echoes, h.datagrams,
                             h.arps);
        }
        Ok(())
    }

    /// `hv probe add <name> [port=N] [network=<name>|vnet:<name>] [firewall=<name>] [public=<a.b.c.d>]
    /// [address=<a.b.c.d>]`.
    fn add(&self, vms: &Vms, name: &str, words: &[&str], out: &mut dyn Write) -> Result<(), String> {
        if !store::valid_name(name) || name == "help" {
            return Err(alloc::format!("\"{}\" is not a probe's name -- a letter or a digit, then letters, digits, \
                                       . _ and -", name));
        }
        known_options(words, &["port", "network", "firewall", "public", "address"])?;
        let public = option(words, "public", crate::config::public)?;
        let address = option(words, "address", crate::config::public)?;
        let port = option(words, "port", |v| v.parse::<usize>().ok().filter(|&p| p < net::MAX_PORTS))?;
        let network = option(words, "network", |v| crate::config::valid_network(v).then(|| String::from(v)))?;
        let given = option(words, "firewall", |v| firewall::valid_name(v).then(|| String::from(v)))?;
        if self.list.lock().iter().any(|p| p.name == name) {
            return Err(alloc::format!("there is a probe {} already", name));
        }
        let cluster = network.as_deref().and_then(crate::vnet::cluster_network);
        match cluster {
            Some(_) if port.is_some() || public.is_some() => {
                return Err(String::from("port= and public= are the host's networks': on a network of the cluster's \
                                         its port is vnet's"));
            }
            None if address.is_some() => {
                return Err(String::from("address= is for a network of the cluster's (network=vnet:<name>)"));
            }
            _ => {}
        }
        let switch = vms.switch_made()?;
        let event = Event::new().ok_or_else(|| String::from("out of memory for its event"))?;
        let heard = Mutex::new(Heard::default()).ok_or_else(|| String::from("out of memory"))?;
        let shared = Arc::new(Shared { event, stop: AtomicBool::new(false), heard });
        let keeper = vms.keeper();
        let (at, me, vnet, firewall) = match cluster {
            /* A port of vnet's, under a UUID of its own: deleted again where
             * the probe goes no further */
            Some(on) => {
                let uuid = crate::guest::random_uuid();
                let a = crate::vnet::attach(&uuid, on, address, given.as_deref(), true)?;
                let claimed = vms.firewalls.place_cluster(&switch, &a.firewall)
                    .and_then(|place| switch.claim_vnet(&shared, &a.identity, place).map_err(String::from));
                let at = match claimed {
                    Ok(at) => at,
                    Err(why) => {
                        let _ = crate::vnet::detach(&uuid);
                        return Err(why);
                    }
                };
                if let Some(k) = &keeper {
                    if let Err(why) = k.attached(at, &uuid, &a) {
                        let _ = writeln!(out, "hv probe: its port not kept -- {}: what vnet changes of it is not seen", why);
                    }
                }
                /* Its way out, where its network has one, as a VM's */
                let _ = switch.way_out();
                let me = Me { ip: a.identity.ip, mac: net::vnet_mac(a.identity.ip),
                              cluster: Some((a.identity.net, a.identity.mask())) };
                (at, me, Some(uuid), a.firewall.name)
            }
            None => {
                let firewall = given.unwrap_or_else(|| String::from(crate::firewalls::DEFAULT));
                let place = vms.firewalls.place(&switch, &firewall)?;
                let claimed = match port {
                    Some(p) => switch.claim_at(p, &shared, network.as_deref(), place),
                    None => switch.claim(&shared, network.as_deref(), place),
                };
                let at = claimed.map_err(String::from)?;
                (at, Me { ip: net::port_ip(at), mac: net::port_mac(at), cluster: None }, None, firewall)
            }
        };
        let probe = Probe {
            name: String::new(), port: at, me, network, firewall, public, vnet, keeper, switch: switch.clone(),
            shared: shared.clone(), task: None,
        };
        /* Its public address, one to one, as a VM's: through NAT, on its
         * way out -- the port given back where it cannot be had */
        if let Some(addr) = public {
            let mapped = switch.way_out().map_err(|e| String::from(net::nat_why(e)))
                .and_then(|_| switch.map_public(at, addr));
            if let Err(why) = mapped {
                let _ = probe.end();
                return Err(alloc::format!("its public address {}: {}", netwire::Ipv4(addr), why));
            }
        }
        let ctx = Ctx { shared, backend: switch.backend(at), me };
        let Some(handle) = kcore::task::spawn_with("hv/probe", ctx, task) else {
            let _ = probe.end();
            return Err(String::from("its task could not be started"));
        };
        let mut probe = Probe { task: Some(handle), ..probe };
        if probe.name.try_reserve_exact(name.len()).is_err() {
            let _ = probe.end();
            return Err(String::from("out of memory"));
        }
        probe.name.push_str(name);
        let said = alloc::format!("hv probe: {} on port {}, {} ({}), firewall {}{}", name, at, net::dotted(me.ip),
                                  probe.network.as_deref().unwrap_or("alone"), probe.firewall,
                                  public.map_or(String::new(), |a| alloc::format!(", public {}", netwire::Ipv4(a))));
        /* Under the lock again: one of the name made meanwhile is the one
         * kept, and this one ended -- after the lock, as its task joins. */
        let mut list = self.list.lock();
        if list.iter().any(|p| p.name == name) || list.try_reserve(1).is_err() {
            drop(list);
            let _ = probe.end();
            return Err(alloc::format!("there is a probe {} already, or no memory for another", name));
        }
        list.push(probe);
        drop(list);
        let _ = writeln!(out, "{}", said);
        Ok(())
    }

    fn del(&self, name: &str, out: &mut dyn Write) -> Result<(), String> {
        let p = {
            let mut list = self.list.lock();
            let i = list.iter().position(|p| p.name == name).ok_or_else(|| alloc::format!("no probe {}", name))?;
            list.remove(i)
        };
        let port = p.port;
        let left = p.end();
        let _ = writeln!(out, "hv probe: {} gone, port {} given back", name, port);
        if let Some(why) = left {
            let _ = writeln!(out, "hv probe: its port of vnet's left -- {}: `vnet port delete` it", why);
        }
        Ok(())
    }

    /// `hv probe ping|udp <name> <a.b.c.d> ...`: `count` echoes -- or
    /// datagrams to `udp`'s port -- sent from the probe, and what came of
    /// them within `secs`.
    fn run(&self, name: &str, to: &str, udp_port: Option<u16>, words: &[&str], out: &mut dyn Write)
        -> Result<(), String>
    {
        known_options(words, if udp_port.is_some() { &["count", "secs", "sport"] } else { &["count", "secs"] })?;
        let count = option(words, "count", |v| v.parse::<u64>().ok().filter(|n| (1..=COUNT_MAX).contains(n)))?
            .unwrap_or(COUNT_DEFAULT);
        let secs = option(words, "secs", |v| v.parse::<u64>().ok().filter(|n| (1..=SECS_MAX).contains(n)))?
            .unwrap_or(SECS_DEFAULT);
        let dst = netwire::parse_ipv4(to.as_bytes()).ok_or_else(|| alloc::format!("\"{}\" is not an address", to))?;
        let (shared, mut backend, who) = {
            let list = self.list.lock();
            let p = list.iter().find(|p| p.name == name).ok_or_else(|| alloc::format!("no probe {}", name))?;
            (p.shared.clone(), p.switch.backend(p.port), p.me)
        };
        /* A run's own source port -- or one asked for, so that two runs are
         * one flow, which a firewall's change may cut between them */
        let sport = option(words, "sport", |v| v.parse::<u16>().ok().filter(|&p| p != 0))?;
        let tag = sport.unwrap_or_else(|| TAG_BASE + self.runs.fetch_add(1, Ordering::Relaxed) % TAGS);
        shared.heard.lock().run = Some(Run { tag, udp: udp_port.is_some(), ..Run::default() });
        let me = who.ip;
        let mut frame = [0u8; MAX_FRAME];
        for seq in 1..=count as u16 {
            let n = match udp_port {
                None => {
                    let len = ICMP_HDR_LEN + PAYLOAD.len();
                    let n = ETH_HDR_LEN + IP_HDR_LEN + len;
                    eth::write(&mut frame, &who.mac_for(dst), &who.mac, ETH_TYPE_IP);
                    let body = &mut frame[ETH_HDR_LEN + IP_HDR_LEN..n];
                    body[ICMP_HDR_LEN..].copy_from_slice(PAYLOAD);
                    icmp::write(body, icmp::ECHO_REQUEST, 0, tag, seq, len);
                    ip::write(&mut frame[ETH_HDR_LEN..], IP_PROTO_ICMP, me, dst, len, seq);
                    n
                }
                Some(to_port) => {
                    let route = udp::Route {
                        src_mac: who.mac, dst_mac: who.mac_for(dst), src_ip: me, dst_ip: dst,
                        src_port: tag, dst_port: to_port, dont_fragment: false,
                    };
                    let n = udp::write_frame(&mut frame, &route, PAYLOAD.len() + 1)
                        .ok_or_else(|| String::from("no room for a datagram"))?;
                    frame[udp::PAYLOAD_AT..n - 1].copy_from_slice(PAYLOAD);
                    frame[n - 1] = seq as u8;
                    n
                }
            };
            backend.send(&frame[..n]);
        }
        /* Its answers, as they come: done once each send has one, or a
         * refusal, or the time is up. */
        let all = if count == COUNT_MAX { u64::MAX } else { (1u64 << count) - 1 };
        let until = kcore::time::boot_time_ns().saturating_add(secs * kcore::consts::NS_PER_SEC);
        loop {
            let done = {
                let heard = shared.heard.lock();
                heard.run.is_some_and(|r| r.answered == all
                    || u64::from(r.prohibited + r.unreachable) + u64::from(r.answered.count_ones()) >= count)
            };
            if done || kcore::time::boot_time_ns() >= until {
                break;
            }
            kcore::task::sleep_ms(LOOK_MS);
        }
        let run = shared.heard.lock().run.take().unwrap_or_default();
        let what = if udp_port.is_some() { "datagrams" } else { "echoes" };
        let _ = writeln!(out, "hv probe: {} -> {}{}: {} {} sent, {} answered, {} refused, {} unreachable", name,
                         net::dotted(dst), udp_port.map_or(String::new(), |p| alloc::format!(":{}", p)), count, what,
                         run.answered.count_ones(), run.prohibited, run.unreachable);
        Ok(())
    }
}
