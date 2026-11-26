//! The shell's network commands. They belong to this layer rather than to
//! `kernel/cmd.cpp`, which has no view of a device, a socket or a resolver
//! to reach through any more.
//!
//! Every message is the one the C++ printed: `tests/tcp-test.py`,
//! `tests/netconsole-test.py` and `tests/netblk-test.py` drive the stack
//! through these and match on what they say.

use alloc::vec::Vec;
use core::fmt::Write;

use kcore::cmd::Output;
use kcore::task;
use kcore::time::Duration;

use crate::abi;
use crate::device::DEVICES;
use crate::dns::MAX_DOMAIN_LEN;
use crate::frame::{self, POOL};
use crate::netconsole::NETCONSOLE;
use crate::settings::{self, Dhcp};
use crate::tcp::TCP;
use crate::wire::{Ipv4 as Ip, MacHex};

/// What every command that needs "the" network device looks for, as the C++
/// did: the first one, which the drivers name eth0.
const DEFAULT_DEVICE: &str = "eth0";

/// A dotted quad the shell was given, into host byte order.
fn parse_ip(text: &str) -> Option<u32> {
    crate::wire::parse_ipv4(text.as_bytes())
}

/* ---- what the counters say ---- */

pub fn net(_args: &str, out: &mut Output) {
    let count = DEVICES.count();
    if count == 0 {
        let _ = writeln!(out, "no network devices");
    } else {
        for index in 0..count {
            let dev = match DEVICES.at(index) {
                Some(dev) => dev,
                None => continue,
            };
            let name = core::str::from_utf8(dev.name()).unwrap_or("?");
            let st = dev.stats();
            let _ = write!(out, "{}  {}  ip:{}", name, MacHex(dev.mac()), Ip(dev.ip()));
            /* The subnet and the way off it: what a static address was given,
             * or a lease brought */
            if dev.mask() != 0 {
                let _ = write!(out, "/{}", dev.mask().count_ones());
            }
            if dev.gw() != 0 {
                let _ = write!(out, " gw:{}", Ip(dev.gw()));
            }
            let _ = writeln!(out, "  tx:{} rx:{} drop:{} rxfull:{} txfull:{}  bytes tx:{} rx:{}",
                st.tx_total, st.rx_total, st.rx_drop, st.rx_full, st.tx_full, st.tx_bytes, st.rx_bytes);
            let _ = writeln!(out, "  rx  icmp:{} udp:{} tcp:{} arp:{} other:{} nat:{}",
                st.rx_icmp, st.rx_udp, st.rx_tcp, st.rx_arp, st.rx_other, st.rx_nat);
            let _ = writeln!(out, "  tx  icmp:{} udp:{} tcp:{} arp:{} other:{} refused:{}",
                st.tx_icmp, st.tx_udp, st.tx_tcp, st.tx_arp, st.tx_other, st.tx_refused);
        }
    }

    let (polls, work, stalls) = DEVICES.poll_counts();
    let _ = writeln!(out, "rx polls {}, poll work {}, stalls {}", polls, work, stalls);

    /* What the boot gave every interface, and from where: edited,
     * /etc/net.toml is read again at the next boot */
    let given = settings::current();
    let _ = match (given.source, settings::refused()) {
        (settings::Source::File, _) => writeln!(out, "{} settings: {} ({})", DEFAULT_DEVICE, given, settings::PATH),
        (_, Some(why)) => writeln!(out, "{} settings: {} (the command line's: {} refused, {})", DEFAULT_DEVICE,
            given, settings::PATH, why),
        (_, None) => writeln!(out, "{} settings: {} (the command line's: no {})", DEFAULT_DEVICE, given,
            settings::PATH),
    };
    for index in 1..settings::MAX_INTERFACES {
        let interface = given.interface(index);
        if interface.address.is_none() && interface.dhcp != Dhcp::Boot {
            continue;
        }
        let _ = writeln!(out, "eth{} settings: {} ({})", index, interface, settings::PATH);
    }
    if given.default != 0 {
        let _ = writeln!(out, "default route: eth{}", given.default);
    }
    for route in given.routes[..given.route_count].iter().flatten() {
        let _ = writeln!(out, "route {}/{} via {} dev eth{}", Ip(route.net), route.mask.count_ones(),
            Ip(route.gw), route.dev);
    }
}

pub fn netpool(args: &str, out: &mut Output) {
    let st = POOL.stats();
    if !st.ready {
        let _ = writeln!(out, "netpool: not set up");
        return;
    }
    if args.trim() == "bench" {
        netpool_bench(out);
        return;
    }

    let _ = writeln!(out, "frames {} of {} bytes, {} in the ring, {} in cpu caches",
        st.frames, st.capacity, st.in_ring, st.in_caches);
    let _ = writeln!(out, "in flight {}", st.in_flight);
    let _ = writeln!(out, "alloc hits {}, misses {}, oversized (refused) {}",
        st.hits, st.misses, st.oversized);
    let _ = writeln!(out, "ring refills {}, flushes {}, frames lost {}", st.refills, st.flushes, st.lost);
}

/// `netpool bench`: what a frame's allocation and release cost on this CPU,
/// as the image takes one and as a module does -- its room zeroed, so that
/// nothing of the frame's last user goes out with it -- one at a time, hot in
/// this CPU's cache, and a ring's worth at a time, through the pool's ring.
fn netpool_bench(out: &mut Output) {
    /* Frames a transmit ring holds, about: a driver's ring is 256 here. */
    const RING: usize = 256;
    /* Frames each case allocates and releases in all. */
    const FRAMES: usize = 1 << 20;
    let Some(hz) = kcore::time::cycle_counter_hz() else {
        let _ = writeln!(out, "netpool bench: the cycle counter's rate is not known");
        return;
    };
    let cases: [(&str, usize, bool); 4] = [
        ("as the image takes one, 1472 bytes", 1472, false),
        ("as a module does, 64 bytes zeroed", 64, true),
        ("as a module does, 1472 bytes zeroed", 1472, true),
        ("as a module does, 2048 bytes zeroed", 2048, true),
    ];
    for (what, len, module) in cases {
        let mut ns = [0u64; 2];
        for (i, at_once) in [1usize, RING].into_iter().enumerate() {
            let mut held = Vec::new();
            if held.try_reserve_exact(at_once).is_err() {
                let _ = writeln!(out, "netpool bench: no memory");
                return;
            }
            let Some(cycles) = frame::bench_alloc(len, module, FRAMES / at_once, &mut held) else {
                let _ = writeln!(out, "netpool bench: the pool ran short of {} frames", at_once);
                return;
            };
            ns[i] = (cycles as u128 * 1_000_000_000 / hz as u128 / FRAMES as u128) as u64;
        }
        let _ = writeln!(out, "a frame {}: {} ns one at a time, {} ns {} at a time", what, ns[0], ns[1], RING);
    }
}

pub fn arp(_args: &str, out: &mut Output) {
    const MAX_ENTRIES: usize = 16;
    let arp = match abi::arp_table() {
        Some(arp) => arp,
        None => return,
    };

    let mut entries = [(0u32, [0u8; 6]); MAX_ENTRIES];
    let count = arp.snapshot(&mut entries);
    if count == 0 {
        let _ = writeln!(out, "arp table empty");
        return;
    }

    for (ip, mac) in &entries[..count] {
        let _ = writeln!(out, "{}  {}", Ip(*ip), MacHex(*mac));
    }
}

pub fn netconsole(_args: &str, out: &mut Output) {
    let st = NETCONSOLE.stats();
    if st.enabled == 0 {
        let _ = writeln!(out, "netconsole: disabled (boot with netconsole=ip:port)");
        return;
    }

    let _ = writeln!(out, "netconsole: {}:{} src port {} dev {}",
        Ip(st.dst_ip), st.dst_port, st.src_port,
        if st.attached != 0 { "attached" } else { "none" });
    let _ = writeln!(out,
        "  buffered {}/{} bytes, dropped {} msgs, sent {} pkts, tx failed {}",
        st.used, st.capacity, st.dropped, st.sent, st.tx_failed);
    let _ = writeln!(out, "  next seq {}, backlog cap {} bytes{}",
        st.seq, st.tail_keep, if st.trimmed != 0 { ", applied" } else { "" });
}

pub fn icmpstat(_args: &str, out: &mut Output) {
    let icmp = match abi::icmp() {
        Some(icmp) => icmp,
        None => return,
    };
    let st = icmp.stats();

    let _ = writeln!(out, "echo request  rx:{} tx:{}", st.echo_req_rx, st.echo_req_tx);
    let _ = writeln!(out, "echo reply    rx:{} tx:{} tx-fail:{}",
        st.echo_reply_rx, st.echo_reply_tx, st.echo_reply_tx_fail);
    let _ = writeln!(out, "other         rx:{} short:{} badcsum:{}",
        st.rx_other, st.rx_too_short, st.rx_bad_csum);
}

pub fn tcpstat(_args: &str, out: &mut Output) {
    let st = TCP.stats();
    let _ = writeln!(out, "TCP stats: tx={} rx={} rxerr={} retx={} conns={} refused={} halfopen={} rstheld={} \
        recoveries={} timeouts={} spurious={} probes={} undos={} dsacks={} reneged={} outside={} badack={} \
        unkept={} overrun={} reclaimed={}",
        st.tx, st.rx, st.rx_err, st.retransmits, st.conns, st.syn_refused, st.half_open, st.rst_held,
        st.recoveries, st.timeouts, st.spurious, st.probes, st.undos, st.dsacks, st.reneged, st.rx_outside,
        st.rx_badack, st.rx_unkept, st.rx_overrun, st.reclaimed);

    for index in 0..crate::tcp::MAX_CONNECTIONS {
        let line = match TCP.snapshot(index) {
            Some(line) => line,
            None => continue,
        };
        let _ = write!(out, "  [{}] {}:{} -> {}:{}  {}  snd={}/{} rcv={}/{} wnd={} cwnd={} rto={}",
            index, Ip(line.local_ip), line.local_port,
            Ip(line.remote_ip), line.remote_port, line.state.name(),
            line.send_used, line.in_flight, line.recv_used, line.recv_size, line.window, line.cwnd, line.rto_ms);
        /* A link's own floor, where it was given one */
        if line.min_rto_ms != crate::tcp::MIN_RTO_MS {
            let _ = write!(out, " floor={}", line.min_rto_ms);
        }
        if line.sack {
            let _ = write!(out, " sack");
        }
        if let Some(recovery) = line.recovery {
            let _ = write!(out, " recovery={} sacked={} lost={}", recovery, line.sacked, line.lost);
        }
        match line.scales {
            Some((ours, theirs)) => { let _ = writeln!(out, " scale={}/{}", ours, theirs); }
            None => { let _ = writeln!(out); }
        }
    }
}

/* ---- asking the network things ---- */

pub fn udpsend(args: &str, out: &mut Output) {
    const USAGE: &str = "usage: udpsend <ip> <port> <message>";
    /* What the C++ sent from, and what a listener on the other side sees. */
    const SOURCE_PORT: u16 = 12345;

    let mut tokens = args.split_whitespace();
    let (ip_text, port_text) = match (tokens.next(), tokens.next()) {
        (Some(ip), Some(port)) => (ip, port),
        _ => { let _ = writeln!(out, "{}", USAGE); return; }
    };

    let port: u32 = match port_text.parse() {
        Ok(port) if port <= 65535 => port,
        _ => { let _ = writeln!(out, "invalid port"); return; }
    };

    let msg = rest_after(args, 2);
    if msg.is_empty() {
        let _ = writeln!(out, "{}", USAGE);
        return;
    }

    let dst = match parse_ip(ip_text) {
        Some(dst) => dst,
        None => { let _ = writeln!(out, "invalid IP '{}'", ip_text); return; }
    };

    let dev = match DEVICES.route(dst) {
        Some(dev) => dev,
        None => { let _ = writeln!(out, "no route to {}", Ip(dst)); return; }
    };

    let arp = match abi::arp_table() { Some(arp) => arp, None => return };

    /* The headers, the ARP resolution and the send are the layer's. */
    if crate::udp::send(&dev.as_nic(), arp, dst, port as u16, dev.ip(),
            SOURCE_PORT, msg.as_bytes()) {
        let _ = writeln!(out, "sent {} bytes to {}:{}", msg.len(), ip_text, port);
    } else {
        let _ = writeln!(out, "send failed");
    }
}

/// Everything after the first `count` whitespace-separated tokens, leading
/// spaces dropped.
fn rest_after(args: &str, count: usize) -> &str {
    let mut rest = args.trim_start();
    for _ in 0..count {
        match rest.find(char::is_whitespace) {
            Some(at) => rest = rest[at..].trim_start_matches(' '),
            None => return "",
        }
    }
    rest
}

pub fn ping(args: &str, out: &mut Output) {
    const ROUNDS: u16 = 5;
    const TIMEOUT_MS: u64 = 3000;

    let host = match args.split_whitespace().next() {
        Some(host) if host.len() <= MAX_DOMAIN_LEN => host,
        _ => { let _ = writeln!(out, "usage: ping <ip|hostname>"); return; }
    };

    let dst = match parse_ip(host) {
        Some(dst) => dst,
        None => match resolve(host) {
            Some(dst) => dst,
            None => { let _ = writeln!(out, "cannot resolve '{}'", host); return; }
        },
    };

    /* The connected or additional route to it, else the configured default. */
    let dev = match DEVICES.route(dst) {
        Some(dev) => dev,
        None => { let _ = writeln!(out, "no route to {}", Ip(dst)); return; }
    };
    let (icmp, arp) = match (abi::icmp(), abi::arp_table()) {
        (Some(icmp), Some(arp)) => (icmp, arp),
        _ => return,
    };

    /* An id of this run's own, so two pings at once do not take each
     * other's replies. */
    let id = (kcore::time::boot_time_ns() & 0xFFFF) as u16;

    let _ = writeln!(out, "PING {}", host);
    let mut received = 0;

    for seq in 0..ROUNDS {
        if !icmp.send_echo_request(&dev.as_nic(), arp, dst, id, seq) {
            let _ = writeln!(out, "send failed seq={}", seq);
        } else if let Some(rtt_ns) = icmp.wait_reply(id, seq, TIMEOUT_MS) {
            let _ = writeln!(out, "reply from {}: seq={} time={} ms",
                host, seq, rtt_ns / 1_000_000);
            received += 1;
        } else {
            let _ = writeln!(out, "request timeout seq={}", seq);
        }

        if seq + 1 < ROUNDS {
            task::sleep(Duration::from_millis(1000));
        }
    }

    let _ = writeln!(out, "{}/{} received", received, ROUNDS);
}

fn resolve(host: &str) -> Option<u32> {
    let dns = abi::dns()?;
    if !dns.is_ready() {
        return None;
    }
    dns.resolve(host.as_bytes(), crate::dns::DEFAULT_TIMEOUT_MS)
}

pub fn nslookup(args: &str, out: &mut Output) {
    let host = match args.split_whitespace().next() {
        Some(host) if host.len() <= MAX_DOMAIN_LEN => host,
        _ => { let _ = writeln!(out, "usage: nslookup <hostname>"); return; }
    };

    let dns = match abi::dns() {
        Some(dns) if dns.is_ready() => dns,
        _ => { let _ = writeln!(out, "DNS resolver not initialized"); return; }
    };

    match dns.resolve(host.as_bytes(), crate::dns::DEFAULT_TIMEOUT_MS) {
        Some(ip) => { let _ = writeln!(out, "{} -> {}", host, Ip(ip)); }
        None => { let _ = writeln!(out, "failed to resolve '{}'", host); }
    }
}

pub fn dnsflush(_args: &str, out: &mut Output) {
    if let Some(dns) = abi::dns() {
        dns.flush();
    }
    let _ = writeln!(out, "dns cache flushed");
}

pub fn dhcp(args: &str, out: &mut Output) {
    let name = args.split_whitespace().next().unwrap_or(DEFAULT_DEVICE);
    let dev = match DEVICES.find(name.as_bytes()) {
        Some(dev) => dev,
        None => { let _ = writeln!(out, "device '{}' not found", name); return; }
    };
    let configured = dev.eth_index().map(|index| settings::current().interface(index).dhcp);
    if configured == Some(Dhcp::Off) {
        if name == DEFAULT_DEVICE {
            let _ = writeln!(out, "DHCP disabled (dhcp=off)");
        } else {
            let _ = writeln!(out, "DHCP disabled on {}", name);
        }
        return;
    }

    let client = match abi::dhcp(dev) { Some(client) => client, None => return };

    if client.is_ready() {
        let _ = writeln!(out, "already bound: {}", Ip(client.lease().ip));
        return;
    }

    let _ = writeln!(out, "DHCP discovering on {}...", name);
    if !client.start(dev.as_nic()) {
        let _ = writeln!(out, "failed to start DHCP");
        return;
    }

    /* Up to ten seconds for a lease */
    for _ in 0..100 {
        if client.is_ready() {
            break;
        }
        task::sleep(Duration::from_millis(100));
    }

    if !client.is_ready() {
        let _ = writeln!(out, "DHCP timeout");
        return;
    }

    let lease = client.lease();
    let _ = writeln!(out, "ip:     {}", Ip(lease.ip));
    let _ = writeln!(out, "mask:   {}", Ip(lease.mask));
    let _ = writeln!(out, "router: {}", Ip(lease.router));
    let _ = writeln!(out, "dns:    {}", Ip(lease.dns));
    let _ = writeln!(out, "lease:  {} seconds", lease.lease_secs);

    if settings::current().lease_dns && lease.dns != 0 {
        if let Some(dns) = abi::dns() {
            if !dns.is_ready() && dns.start(dev.as_nic(), lease.dns) {
                let _ = writeln!(out, "DNS resolver started, server: {}", Ip(lease.dns));
            }
        }
    }
}

/* ---- registration ---- */

pub fn register_all() {
    let commands: &[(&str, &str, fn(&str, &mut Output))] = &[
        ("net", "net - show network devices", net),
        ("netpool", "netpool [bench] - recycled net frame pool state, or what taking a frame costs", netpool),
        ("arp", "arp - show ARP table", arp),
        ("nat", "nat [allow|deny <a.b.c.d[/n]>] - NAT state, where guests are kept from, counters and mappings", crate::nat::shell),
        ("netconsole", "netconsole - kernel log over UDP state", netconsole),
        ("icmpstat", "icmpstat - ICMP counters", icmpstat),
        ("tcpstat", "tcpstat - TCP connections and counters", tcpstat),
        ("udpsend", "udpsend <ip> <port> <msg> - send UDP packet", udpsend),
        ("ping", "ping <ip|hostname> - ICMP echo", ping),
        ("nslookup", "nslookup <hostname> - resolve a name", nslookup),
        ("dnsflush", "dnsflush - empty the DNS cache", dnsflush),
        ("dhcp", "dhcp [dev] - run DHCP client", dhcp),
        ("wget", "wget [-o <path>] <url> [path] - fetch over http or https", crate::wget::wget),
    ];

    for (name, help, handler) in commands {
        let handler = *handler;
        match kcore::cmd::Command::register(name, help, move |args, out| handler(args, out)) {
            /* The command is the kernel's own and stays for good. */
            Ok(cmd) => core::mem::forget(cmd),
            Err(_) => kcore::klog!(0, "net: cannot register the {} command", name),
        }
    }
}

/* ---- what boot asks of the layer ---- */

/// The shell's task, as it starts: every interface whose settings ask DHCP
/// at boot gets its own client. They run together, and the default
/// interface's lease names the resolver first. What it prints goes to the
/// console the shell is about to take.
///
/// # Safety
/// `printer` is a `stdlib::Printer*` that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn rust_net_dhcp_auto(printer: *mut core::ffi::c_void) {
    let mut out = unsafe { Output::from_raw(printer) };
    let settings = settings::current();
    let mut started = Vec::new();

    for index in 0..settings::MAX_INTERFACES {
        let interface = settings.interface(index);
        if interface.dhcp != Dhcp::Boot {
            continue;
        }
        if let Some(given) = interface.address {
            let _ = writeln!(out, "DHCP auto: eth{} has a static address, {}", index, Ip(given.ip));
            continue;
        }
        let Some(dev) = DEVICES.find_eth(index) else { continue };
        let Some(client) = abi::dhcp(dev) else { continue };
        let _ = writeln!(out, "DHCP auto on eth{}...", index);
        if !client.start(dev.as_nic()) {
            let _ = writeln!(out, "DHCP auto failed on eth{}", index);
            continue;
        }
        started.push((index, dev, client));
    }

    for _ in 0..100 {
        if started.iter().all(|(_, _, client)| client.is_ready()) {
            break;
        }
        task::sleep(Duration::from_millis(100));
    }

    for &(index, _, client) in &started {
        if !client.is_ready() {
            let _ = writeln!(out, "DHCP auto timeout on eth{}", index);
            continue;
        }
        if started.len() == 1 {
            let _ = writeln!(out, "DHCP ip: {}", Ip(client.lease().ip));
        } else {
            let _ = writeln!(out, "DHCP ip: {} (eth{})", Ip(client.lease().ip), index);
        }
    }
    for preferred in [true, false] {
        for &(index, dev, client) in &started {
            if (index == settings.default) != preferred || !client.is_ready() {
                continue;
            }
            let lease = client.lease();
            if !settings.lease_dns || lease.dns == 0 {
                continue;
            }
            if let Some(dns) = abi::dns() {
                if !dns.is_ready() && dns.start(dev.as_nic(), lease.dns) {
                    let _ = writeln!(out, "DNS resolver started, server: {}", Ip(lease.dns));
                }
            }
        }
    }
}

/// On the way down: every client stops asking for a lease.
#[no_mangle]
pub extern "C" fn rust_net_dhcp_stop() {
    abi::dhcp_stop_all();
}

/// Every static address, put on its interface before anything that sends --
/// the netconsole first, which waits for an address and has none on a network
/// with no DHCP server. The mask and the gateway go first and the address
/// last: an address is what everything here takes for a device ready to
/// send from, and each is read with the order its store made.
fn configure_static() {
    let settings = settings::current();
    for index in 0..settings::MAX_INTERFACES {
        let Some(given) = settings.interface(index).address else { continue };
        let Some(dev) = DEVICES.find_eth(index) else {
            kcore::klog!(0, "net: eth{}'s settings give it a static address, but there is no such device", index);
            continue;
        };
        let nic = dev.as_nic();
        nic.set_mask(given.mask);
        nic.set_gw(given.gw);
        nic.set_ip(given.ip);
        kcore::klog!(0, "net: eth{} static ip {} mask {} gw {}", index, Ip(given.ip), Ip(given.mask), Ip(given.gw));
    }
    if settings.is_multi() && DEVICES.default_device().is_none() {
        kcore::klog!(0, "net: the default route is on eth{}, which is not registered", settings.default);
    }
}

/// A resolver's server given -- `/etc/net.toml`'s `dns`, or the command
/// line's `dns=A.B.C.D`: the resolver on it from the start, on the default
/// route's interface -- with a static address or a lease to come, whose own
/// server then does not replace it.
fn start_given_resolver() {
    let settings = settings::current();
    let server = settings.dns;
    if server == 0 {
        return;
    }
    let Some(dev) = DEVICES.default_device() else {
        kcore::klog!(0, "dns: the settings give a resolver, but there is no eth{} to resolve on", settings.default);
        return;
    };
    match abi::dns() {
        Some(dns) if !dns.is_ready() => {
            if !dns.start(dev.as_nic(), server) {
                kcore::klog!(0, "dns: the resolver on {} did not start", Ip(server));
            }
        }
        _ => {}
    }
}

/// The services boot starts once the devices are up and the shell exists:
/// every static address, and the resolver on the default interface; the
/// netconsole, if the command line asked for one, and the shell over UDP.
/// Those two stay on eth0, which is what the C++ looked for.
#[no_mangle]
pub extern "C" fn rust_net_start_services(udp_shell_port: u16) {
    configure_static();
    start_given_resolver();

    if NETCONSOLE.is_enabled() {
        match DEVICES.find(DEFAULT_DEVICE.as_bytes()) {
            None => kcore::klog!(0, "Netconsole: eth0 not found"),
            Some(dev) => {
                if !NETCONSOLE.start(dev.as_nic()) {
                    kcore::klog!(0, "Netconsole: failed to start");
                }
            }
        }
    }

    if udp_shell_port != 0 {
        match DEVICES.find(DEFAULT_DEVICE.as_bytes()) {
            None => kcore::klog!(0, "UdpShell: eth0 not found"),
            Some(dev) => {
                if !abi::udp_shell_start(dev.as_nic(), udp_shell_port) {
                    kcore::klog!(0, "UdpShell: failed to start on port {}", udp_shell_port);
                }
            }
        }
    }
}

/// And what stops them on the way down, apart: the shell over UDP with the
/// other shells, before the modules go -- it calls into their commands --
/// and the netconsole as late as it can be, after the modules and the
/// filesystems and before the soft IRQs, so that on a machine whose only
/// console is the network the halt's own lines reach the collector: a
/// module's exit that waits for guests to turn themselves off among them.
#[no_mangle]
pub extern "C" fn rust_net_stop_shell() {
    abi::udp_shell_stop();
}

#[no_mangle]
pub extern "C" fn rust_net_stop_netconsole() {
    NETCONSOLE.stop();
}
