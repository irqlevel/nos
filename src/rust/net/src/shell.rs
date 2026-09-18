//! The shell's network commands. They belong to this layer rather than to
//! `kernel/cmd.cpp`, which has no view of a device, a socket or a resolver
//! to reach through any more.
//!
//! Every message is the one the C++ printed: `scripts/tcp-test.py`,
//! `scripts/netconsole-test.py` and `scripts/netblk-test.py` drive the stack
//! through these and match on what they say.

use core::fmt::Write;

use kcore::cmd::Output;
use kcore::task;
use kcore::time::Duration;

use crate::abi;
use crate::device::{Device, DEVICES};
use crate::dns::MAX_DOMAIN_LEN;
use crate::frame::POOL;
use crate::netconsole::NETCONSOLE;
use crate::tcp::TCP;
use crate::wire::Mac;

/// What every command that needs "the" network device looks for, as the C++
/// did: the first one, which the drivers name eth0.
const DEFAULT_DEVICE: &str = "eth0";

/// A dotted quad, host byte order, as `Net::IpAddress::Print` wrote it.
struct Ip(u32);

impl core::fmt::Display for Ip {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", (self.0 >> 24) & 0xFF, (self.0 >> 16) & 0xFF,
            (self.0 >> 8) & 0xFF, self.0 & 0xFF)
    }
}

/// Six hex pairs, lower case, as `Net::MacAddress::Print` wrote it.
struct MacHex(Mac);

impl core::fmt::Display for MacHex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let b = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5])
    }
}

/// A dotted quad the shell was given, into host byte order.
fn parse_ip(text: &str) -> Option<u32> {
    let mut parts = text.split('.');
    let mut addr = 0u32;
    for _ in 0..4 {
        let octet: u32 = parts.next()?.parse().ok()?;
        if octet > 255 {
            return None;
        }
        addr = (addr << 8) | octet;
    }
    if parts.next().is_some() { None } else { Some(addr) }
}

/// The device a command works on, or None with the reason said.
fn device(name: &str, out: &mut Output) -> Option<&'static Device> {
    match DEVICES.find(name.as_bytes()) {
        Some(dev) => Some(dev),
        None => {
            let _ = writeln!(out, "no network device");
            None
        }
    }
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
            let _ = writeln!(out, "{}  {}  ip:{}  tx:{} rx:{} drop:{}",
                name, MacHex(dev.mac()), Ip(dev.ip()),
                st.tx_total, st.rx_total, st.rx_drop);
            let _ = writeln!(out, "  rx  icmp:{} udp:{} tcp:{} arp:{} other:{}",
                st.rx_icmp, st.rx_udp, st.rx_tcp, st.rx_arp, st.rx_other);
            let _ = writeln!(out, "  tx  icmp:{} udp:{} tcp:{} arp:{} other:{}",
                st.tx_icmp, st.tx_udp, st.tx_tcp, st.tx_arp, st.tx_other);
        }
    }

    let (polls, work, stalls) = DEVICES.poll_counts();
    let _ = writeln!(out, "rx polls {}, poll work {}, stalls {}", polls, work, stalls);
}

pub fn netpool(_args: &str, out: &mut Output) {
    let st = POOL.stats();
    if st.ready == 0 {
        let _ = writeln!(out, "netpool: not set up");
        return;
    }

    let _ = writeln!(out, "frames {} of {} bytes, {} in the ring, {} in cpu caches",
        st.frames, st.capacity, st.in_ring, st.in_caches);
    let _ = writeln!(out, "in flight {}", st.in_flight);
    let _ = writeln!(out, "alloc hits {}, misses {}, oversized {}",
        st.hits, st.misses, st.oversized);
    let _ = writeln!(out, "ring refills {}, flushes {}", st.refills, st.flushes);
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
    let _ = writeln!(out, "TCP stats: tx={} rx={} rxerr={} retx={} conns={}",
        st.tx, st.rx, st.rx_err, st.retransmits, st.conns);

    for index in 0..crate::tcp::MAX_CONNECTIONS {
        let line = match TCP.snapshot(index) {
            Some(line) => line,
            None => continue,
        };
        let _ = writeln!(out, "  [{}] {}:{} -> {}:{}  {}  snd={}/{} rcv={}",
            index, Ip(line.local_ip), line.local_port,
            Ip(line.remote_ip), line.remote_port, line.state.name(),
            line.send_used, line.in_flight, line.recv_used);
    }
}

/* ---- the load target ---- */

pub fn netload(args: &str, out: &mut Output) {
    const USAGE: &str = "usage: netload [start [port] [sink] | stop | reset]";

    let load = abi::net_load();
    let mut tokens = args.split_whitespace();
    let verb = match tokens.next() {
        Some(verb) => verb,
        None => { dump_netload(out); return; }
    };

    match verb {
        "stop" => {
            if !load.is_running() {
                let _ = writeln!(out, "netload: not running");
                return;
            }
            load.stop();
            let _ = writeln!(out, "netload: stopped");
            return;
        }
        "reset" => {
            load.reset_counters();
            let _ = writeln!(out, "netload: counters cleared");
            return;
        }
        "start" => {}
        _ => { let _ = writeln!(out, "{}", USAGE); return; }
    }

    if load.is_running() {
        let _ = writeln!(out, "netload: already running");
        return;
    }

    /* start [port] [sink], and `sink` alone means the default port. */
    let mut port = crate::net_load::DEFAULT_PORT;
    let mut echo = true;
    match tokens.next() {
        None => {}
        Some("sink") => echo = false,
        Some(text) => match text.parse::<u16>() {
            Ok(parsed) if parsed != 0 => {
                port = parsed;
                if tokens.next() == Some("sink") {
                    echo = false;
                }
            }
            _ => { let _ = writeln!(out, "{}", USAGE); return; }
        },
    }

    let dev = match DEVICES.find(DEFAULT_DEVICE.as_bytes()) {
        Some(dev) => dev,
        None => { let _ = writeln!(out, "netload: no eth0"); return; }
    };

    if !load.start(dev.as_nic(), port, echo) {
        let _ = writeln!(out, "netload: could not start on port {}", port);
        return;
    }

    let _ = writeln!(out, "netload: listening on udp {}, {}", port,
        if echo { "echo" } else { "sink" });
}

fn dump_netload(out: &mut Output) {
    let st = abi::net_load().stats();
    if st.running == 0 {
        let _ = writeln!(out, "netload: not running");
        return;
    }

    let _ = writeln!(out, "netload: port {}, {}", st.port,
        if st.echo != 0 { "echo" } else { "sink" });
    let _ = writeln!(out, "rx {} packets, {} bytes", st.rx_packets, st.rx_bytes);
    let _ = writeln!(out, "tx {} packets, {} failed", st.tx_packets, st.tx_failed);
    let _ = writeln!(out, "rate {} rx-pps, {} tx-pps, {} rx-bytes/s",
        st.rx_pps, st.tx_pps, st.rx_bps);

    /* Which CPUs the driver's interrupts actually landed on: a load test that
       runs entirely on one core is measuring one core. */
    let _ = write!(out, "per cpu rx:");
    for cpu in 0..kcore::consts::MAX_CPUS {
        let rx = abi::net_load().cpu_rx(cpu);
        if rx != 0 {
            let _ = write!(out, " {}:{}", cpu, rx);
        }
    }
    let _ = writeln!(out);
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

    let dev = match device(DEFAULT_DEVICE, out) { Some(dev) => dev, None => return };

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

    let dev = match device(DEFAULT_DEVICE, out) { Some(dev) => dev, None => return };
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
    if kcore::net::dhcp_off() {
        let _ = writeln!(out, "DHCP disabled (dhcp=off)");
        return;
    }

    let name = args.split_whitespace().next().unwrap_or(DEFAULT_DEVICE);
    let dev = match DEVICES.find(name.as_bytes()) {
        Some(dev) => dev,
        None => { let _ = writeln!(out, "device '{}' not found", name); return; }
    };

    let client = match abi::dhcp() { Some(client) => client, None => return };

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

    if kcore::net::dns_on() && lease.dns != 0 {
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
        ("netpool", "netpool - recycled net frame pool state", netpool),
        ("arp", "arp - show ARP table", arp),
        ("netconsole", "netconsole - kernel log over UDP state", netconsole),
        ("icmpstat", "icmpstat - ICMP counters", icmpstat),
        ("tcpstat", "tcpstat - TCP connections and counters", tcpstat),
        ("netload", "netload [start [port] [sink]|stop|reset] - udp load target", netload),
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
            Err(_) => kcore::trace!(0, "net: cannot register the {} command", name),
        }
    }
}

/* ---- what boot asks of the layer ---- */

/// `dhcp=auto`: the shell's task runs a DHCP client on eth0 as it starts,
/// and brings a resolver up on whatever server the lease names. What it
/// prints goes to the console the shell is about to take.
///
/// # Safety
/// `printer` is a `Stdlib::Printer*` that outlives the call.
#[no_mangle]
pub unsafe extern "C" fn rust_net_dhcp_auto(printer: *mut core::ffi::c_void) {
    let mut out = unsafe { Output::from_raw(printer) };

    let dev = match DEVICES.find(DEFAULT_DEVICE.as_bytes()) {
        Some(dev) => dev,
        /* No device: nothing to say, as the C++ said nothing either. */
        None => return,
    };
    let client = match abi::dhcp() { Some(client) => client, None => return };

    let _ = writeln!(out, "DHCP auto on {}...", DEFAULT_DEVICE);
    if !client.start(dev.as_nic()) {
        let _ = writeln!(out, "DHCP auto failed");
        return;
    }

    for _ in 0..100 {
        if client.is_ready() {
            break;
        }
        task::sleep(Duration::from_millis(100));
    }
    if !client.is_ready() {
        let _ = writeln!(out, "DHCP auto timeout");
        return;
    }

    let lease = client.lease();
    let _ = writeln!(out, "DHCP ip: {}", Ip(lease.ip));

    if kcore::net::dns_on() && lease.dns != 0 {
        if let Some(dns) = abi::dns() {
            if dns.start(dev.as_nic(), lease.dns) {
                let _ = writeln!(out, "DNS resolver started, server: {}", Ip(lease.dns));
            }
        }
    }
}

/// On the way down: the client stops asking for a lease.
#[no_mangle]
pub extern "C" fn rust_net_dhcp_stop() {
    if let Some(client) = abi::dhcp() {
        client.stop();
    }
}
