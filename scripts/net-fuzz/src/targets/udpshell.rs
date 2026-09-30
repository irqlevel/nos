//! `udpshell`: the shell over UDP -- the only console some of the machines
//! have -- asked by clients on the LAN and beyond it, in datagrams well made
//! and not: a header cut short, a magic that is not, a length that says more
//! than there is, a line that is no text, a line longer than a line may be,
//! two at once, from an address no reply can go to; and commands that print
//! nothing, a line, more than a reply holds, that run a while -- the fuzzer's
//! own, whose output is known, and the network layer's.
//!
//! Every command in a well-made request is answered, to where it came from,
//! from the shell's port, with its sequence number back: chunks numbered
//! from 0, each at most a datagram's worth, the last and only the last
//! flagged so, together exactly what the command printed -- or, past what a
//! reply holds, the start of it with the marker that says so. A line longer
//! than a line may be is not run cut short, but answered with why not.

use std::cell::RefCell;
use std::rc::Rc;

use netwire::{eth, udp, IP_PROTO_UDP};

use crate::input::noise;
use crate::machine::{self, sched, ETH0_MAC};
use crate::world::frames::{self, Ip, Sum};
use crate::world::lan::Lan;
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, GW_MAC, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const PORT: u16 = 9000;
const MAGIC: u32 = 0x4E4F_5348;
const FLAG_LAST: u16 = 1;
const HDR_LEN: usize = 16;
const CHUNK_LEN: usize = 1384;
const REPLY_CAP: usize = 32 * 1024;
const CMD_MAX: usize = 256;
const TRUNCATED: &[u8] = b"\n[output truncated]\n";

/// The clients: one on the LAN, one beyond the gateway, one that answers
/// no ARP -- which no reply can reach.
const NEAR: u32 = 0x0A00_0264;
const NEAR_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
const FAR: u32 = 0x5DB8_D822;
const DEAF: u32 = 0x0A00_0266;
const DEAF_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x66];

/// One datagram of a reply, as the client got it.
#[derive(Clone, Debug)]
struct Chunk {
    to: u32,
    to_port: u16,
    from_port: u16,
    seq: u32,
    index: u16,
    flags: u16,
    payload: Vec<u8>,
}

struct Wire {
    lan: Lan,
    /// What the clients got: shared with the waits that watch for a reply.
    chunks: Rc<RefCell<Vec<Chunk>>>,
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if self.lan.on_frame(net, frame) {
            return;
        }
        let Some(d) = udp::parse(frame) else { return };
        if d.src_port != PORT {
            return;
        }
        let via = if d.dst_ip & MASK == ETH0_IP & MASK { d.dst_ip } else { GW_IP };
        invariant!(Some(eth::dst(frame)) == self.lan.mac_of(via), "a reply to {} sent to {:02x?}",
                   netwire::Ipv4(d.dst_ip), eth::dst(frame));
        let p = d.payload;
        invariant!(p.len() >= HDR_LEN && netwire::be32(p, 0) == MAGIC, "a reply of {} bytes, not the shell's",
                   p.len());
        let len = usize::from(netwire::be16(p, 12));
        invariant!(len == p.len() - HDR_LEN && len <= CHUNK_LEN && netwire::be16(p, 14) == 0,
                   "a reply chunk saying {} bytes, carrying {}", len, p.len() - HDR_LEN);
        self.chunks.borrow_mut().push(Chunk { to: d.dst_ip, to_port: d.dst_port, from_port: d.src_port,
                                 seq: netwire::be32(p, 4), index: netwire::be16(p, 8), flags: netwire::be16(p, 10),
                                 payload: p[HDR_LEN..].to_vec() });
    }
}

/// A request's datagram: the header, then `line`; `declared` in place of
/// its length when the header lies about it.
fn request(seq: u32, line: &[u8], declared: Option<u16>) -> Vec<u8> {
    let mut m = vec![0u8; HDR_LEN];
    netwire::set_be32(&mut m, 0, MAGIC);
    netwire::set_be32(&mut m, 4, seq);
    netwire::set_be16(&mut m, 12, declared.unwrap_or(line.len() as u16));
    m.extend_from_slice(line);
    m
}

/// What `spew n` prints: the fuzzer's builtin, over again.
fn spew(n: usize) -> Vec<u8> {
    let n = n.min(1 << 20);
    let mut out = Vec::with_capacity(n);
    let mut k = 0u32;
    while out.len() < n {
        let mut line = [0u8; 97];
        for (i, b) in line.iter_mut().enumerate().take(96) {
            *b = b'a' + ((k as usize + i) % 26) as u8;
        }
        line[96] = b'\n';
        let take = (n - out.len()).min(line.len());
        out.extend_from_slice(&line[..take]);
        k = k.wrapping_add(1);
    }
    out
}

/// What a reply of `output` is: all of it, or -- past what a reply holds --
/// its start, the marker over the end of that.
fn replied(mut output: Vec<u8>) -> Vec<u8> {
    if output.len() > REPLY_CAP {
        output.truncate(REPLY_CAP);
        let at = REPLY_CAP - TRUNCATED.len();
        output[at..].copy_from_slice(TRUNCATED);
    }
    output
}

/// A command line, and what it prints when that is known.
fn command(r: &mut Input) -> (String, Option<Vec<u8>>) {
    match r.u8() % 16 {
        0..=3 => {
            let words: Vec<String> = (0..r.below(6)).map(|_| format!("w{}", r.u16())).collect();
            let spaces = " ".repeat(r.below(3) as usize);
            let args = format!("{}{}", words.join(" "), spaces);
            let line = format!("echo {}", args);
            /* The command's arguments start at its first word. */
            let out = format!("{}\n", args.trim_start()).into_bytes();
            (line, Some(out))
        }
        4 | 5 => {
            let n = r.pick(&[0usize, 1, 96, 97, 1383, 1384, 1385, 2768, 32747, 32767, 32768, 32769, 40000, 70000]);
            (format!("spew {}", n), Some(spew(n)))
        }
        6 => (format!("nap {}", r.below(2000)), Some(b"awake\n".to_vec())),
        7 => ("frob a b".to_string(), Some(b"unknown command: frob\n".to_vec())),
        /* A line that fills a line, or is just too long for one. */
        8 => {
            let n = r.pick(&[CMD_MAX, CMD_MAX + 1, CMD_MAX + 200, 1000]);
            let args = "x".repeat(n - 5);
            let line = format!("echo {}", args);
            let out = if n <= CMD_MAX {
                format!("{}\n", args).into_bytes()
            } else {
                format!("command too long: {} bytes, at most {}\n", n, CMD_MAX).into_bytes()
            };
            (line, Some(out))
        }
        9 => ("dhcp".to_string(), Some(b"DHCP disabled (dhcp=off)\n".to_vec())),
        10 => ("dnsflush".to_string(), Some(b"dns cache flushed\n".to_vec())),
        _ => {
            let cmds = ["net", "netpool", "arp", "icmpstat", "tcpstat", "netconsole", "nat", "nslookup x.test",
                        "udpsend 10.0.2.100 9 hello", "udpsend", "ping", "ping 10.0.2.100", "help"];
            (r.pick(&cmds).to_string(), None)
        }
    }
}

/// The request from `client`, on port `cport`.
fn send(w: &mut World<Wire>, client: u32, cport: u16, dgram: &[u8]) {
    let mac = if client & MASK == ETH0_IP & MASK { w.peers.lan.mac_of(client).unwrap_or(DEAF_MAC) } else { GW_MAC };
    let d = frames::udp(client, ETH0_IP, cport, PORT, dgram, Sum::Right);
    let frame = frames::ipv4(ETH0_MAC, mac, &Ip::new(client, ETH0_IP, IP_PROTO_UDP), &d);
    w.net.send(frame);
}

/// Whether the reply to `seq` has come to its last chunk.
fn ended(chunks: &RefCell<Vec<Chunk>>, seq: u32) -> bool {
    chunks.borrow().iter().any(|c| c.seq == seq && c.flags & FLAG_LAST != 0)
}

/// The reply to `seq`, once it is whole: what the chunks say, checked.
fn reply_of(w: &World<Wire>, seq: u32, client: u32, cport: u16) -> Option<Vec<u8>> {
    let all = w.peers.chunks.borrow();
    let chunks: Vec<&Chunk> = all.iter().filter(|c| c.seq == seq).collect();
    let last = chunks.iter().find(|c| c.flags & FLAG_LAST != 0)?;
    let mut out = Vec::new();
    for i in 0..=last.index {
        let c = chunks.iter().find(|c| c.index == i)?;
        out.extend_from_slice(&c.payload);
    }
    for c in &chunks {
        invariant!(c.to == client && c.to_port == cport && c.from_port == PORT,
                   "the reply to request {} went to {}:{} from port {}, the request coming from {}:{}", seq,
                   netwire::Ipv4(c.to), c.to_port, c.from_port, netwire::Ipv4(client), cport);
        invariant!(c.index <= last.index && (c.flags & FLAG_LAST == 0) == (c.index != last.index),
                   "the reply to request {}: chunk {} flagged {:#x}, the last being {}", seq, c.index, c.flags,
                   last.index);
    }
    Some(out)
}

pub fn udpshell(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);
    /* As `dhcp=off` has it: the command says so rather than starting a
     * client nobody answers. */
    machine::set_params(machine::Params { dhcp_off: true, ..machine::params() });

    let mut lan = Lan::new();
    lan.add(NEAR, NEAR_MAC);
    let link = if r.u8() < 216 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let mut w = World::new(link, Wire { lan, chunks: Rc::new(RefCell::new(Vec::new())) });

    let shell: &'static net::udp_shell::UdpShell =
        Box::leak(Box::new(net::udp_shell::UdpShell::new().expect("a UDP shell")));
    invariant!(shell.start(nic, PORT), "the UDP shell would not start");
    invariant!(!shell.start(nic, PORT), "the UDP shell started twice");
    let mut running = true;
    let mut seq = 0u32;

    while let Some(op) = r.op(12) {
        seq = seq.wrapping_add(1 + r.below(3) as u32);
        match op {
            0..=5 => {
                let (line, expect) = command(r);
                let ending = r.pick(&["", "\n", "\r\n", "\n\r\n"]);
                let client = r.pick(&[NEAR, NEAR, FAR, DEAF]);
                let cport = r.pick(&[40000u16, 9000, 1, 65535]);
                send(&mut w, client, cport, &request(seq, format!("{}{}", line, ending).as_bytes(), None));
                let end = sched::now() + 120 * SEC;
                let chunks = w.peers.chunks.clone();
                w.run_until(end, || ended(&chunks, seq));
                let got = reply_of(&w, seq, client, cport);
                if !running || client == DEAF {
                    invariant!(got.is_none() && w.peers.chunks.borrow().iter().all(|c| c.seq != seq),
                               "request {} answered, to {} which no reply can reach, or by a shell stopped", seq,
                               netwire::Ipv4(client));
                    continue;
                }
                if !lossless {
                    if let (Some(got), Some(expect)) = (&got, &expect) {
                        invariant!(*got == replied(expect.clone()), "'{}' answered {} bytes, not the {} it printed",
                                   line, got.len(), expect.len());
                    }
                    continue;
                }
                let Some(got) = got else {
                    invariant!(false, "'{}' from {}:{} (request {}) never answered", line, netwire::Ipv4(client),
                               cport, seq);
                    continue;
                };
                if let Some(expect) = expect {
                    let want = replied(expect);
                    invariant!(got == want, "'{}' answered {} bytes {:?}..., it having printed {} {:?}...",
                               line.chars().take(40).collect::<String>(), got.len(),
                               String::from_utf8_lossy(&got[..got.len().min(40)]), want.len(),
                               String::from_utf8_lossy(&want[..want.len().min(40)]));
                }
            }
            6 => {
                /* Requests made wrong: nothing to run, nothing to answer --
                 * and a line that is no text is dropped. */
                let dgram = match r.u8() % 6 {
                    0 => noise(r.u32(), r.below(HDR_LEN as u64) as usize),
                    1 => {
                        let mut d = request(seq, b"echo x", None);
                        d[0] ^= 0x20;
                        d
                    }
                    2 => request(seq, b"echo x", Some(r.pick(&[7u16, 100, 0xFFFF]))),
                    3 => request(seq, &[0xFF, 0xFE, b'e', 0xC3], None),
                    4 => request(seq, r.pick(&[b"" as &[u8], b"\n", b"\r\n"]), None),
                    _ => noise(r.u32(), r.below(600) as usize),
                };
                send(&mut w, NEAR, 40000, &dgram);
                w.run_for(200 * MS);
                invariant!(w.peers.chunks.borrow().iter().all(|c| c.seq != seq),
                           "a request made wrong (request {}) answered", seq);
            }
            7 => {
                /* Two at once: the second may be dropped, never mixed with
                 * the first. */
                send(&mut w, NEAR, 40000, &request(seq, b"echo first", None));
                send(&mut w, NEAR, 40001, &request(seq.wrapping_add(1), b"echo second", None));
                seq = seq.wrapping_add(1);
                w.run_for(3 * SEC);
                for (s, port, text) in [(seq.wrapping_sub(1), 40000u16, b"first\n" as &[u8]), (seq, 40001, b"second\n")] {
                    if let Some(got) = reply_of(&w, s, NEAR, port) {
                        invariant!(got == text, "request {} answered {:?}", s, String::from_utf8_lossy(&got));
                    }
                }
            }
            8 if running && r.u8() < 64 => {
                let app = App::spawn("udpsh-stop", 1, move || shell.stop());
                invariant!(w.wait_app(&app, 30 * SEC), "the UDP shell took half a minute to stop");
                running = false;
            }
            9 if !running => {
                invariant!(shell.start(nic, PORT), "the UDP shell would not start again");
                running = true;
            }
            _ => w.run_for(r.below(3000) * MS),
        }
    }

    if running {
        let app = App::spawn("udpsh-stop", 1, move || shell.stop());
        invariant!(w.wait_app(&app, 30 * SEC), "the UDP shell took half a minute to stop");
    }
    w.run_for(10 * SEC);
    super::audit();
}
