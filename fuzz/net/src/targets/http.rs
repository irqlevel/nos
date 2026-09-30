//! `http`: the kernel's HTTP client -- `wget`'s -- fetching from a server on
//! the wire that answers as the input says: well, in pieces of every size
//! and at every pace, through redirects; or to break it, with numbers too
//! long for any field, chunk sizes of every length, headers that never end,
//! a redirect to anywhere, a connection reset or left hanging.
//!
//! A well-made answer has one right result -- its status, its body whole
//! in the sink, the redirect followed or not -- and the client must give
//! it. Any answer at all must leave it returning, with no panic, and the
//! machine with nothing held once it has.
//!
//! `https`: the same over TLS -- the kernel's `tls` crate, rustls over the
//! world's TCP -- from a server whose certificate the input chooses: its
//! own, from the CA the client trusts; one run out; one for another
//! address; one nobody vouches for. It speaks TLS 1.2, 1.3 or both, ends
//! with a close_notify or without, and now and then flips a byte of what
//! it sends. A certificate the client must refuse is refused before the
//! request goes -- the server never reads it -- and a flipped byte never
//! comes out of the client as a body it believes: what it gives is the
//! answer's, from its start.

use net::http;
use netwire::Ipv4;

use crate::input::noise;
use crate::world::httpd::{Answer, End, Expect, Httpd, Tls};
use crate::world::tlsd::{self, Cert};
use crate::world::lan::Lan;
use crate::world::tcpm::Tcp;
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, GW_MAC, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

const SERVER_IP: u32 = 0x0A00_0264;
const SERVER_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x64];
const FAR_IP: u32 = 0x5DB8_D822;
const PORTS: [u16; 2] = [80, 8080];
const TLS_PORTS: [u16; 2] = [443, 8443];

/// The client's own limits, as http.rs has them.
const MAX_HEADER: usize = 16384;
const MAX_URL_LEN: usize = 2048;
const MAX_REDIRECTS: usize = 5;

struct Wire {
    lan: Lan,
    tcp: Tcp,
    httpd: Httpd,
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if !self.lan.on_frame(net, frame) {
            self.tcp.on_frame(net, frame);
        }
        self.httpd.serve(&mut self.tcp, net);
    }

    fn next_timer(&self) -> u64 {
        self.tcp.next_timer().min(self.httpd.next_timer(&self.tcp))
    }

    fn on_timer(&mut self, net: &mut Net) {
        self.tcp.on_timer(net);
        self.httpd.serve(&mut self.tcp, net);
    }
}

/// Where the body goes: kept, up to `cap` bytes -- past which it takes
/// fewer than it is offered, as a sink that is full does.
struct Sink {
    got: Vec<u8>,
    cap: usize,
}

impl http::Sink for Sink {
    fn take(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.cap - self.got.len());
        self.got.extend_from_slice(&data[..n]);
        n
    }
}

/// What came back of a GET.
struct Got {
    status: i32,
    content_length: usize,
    body_len: usize,
    ok: u32,
    truncated: u32,
    tls_failed: u32,
    url_too_long: u32,
    body: Vec<u8>,
    location: Vec<u8>,
}

/* ---- answers ---- */

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        404 => "Not Found",
        _ => "Whatever",
    }
}

/// A header line of no consequence: what a server sends besides.
fn filler(r: &mut Input) -> Vec<u8> {
    match r.u8() % 6 {
        0 => b"Server: nginx/1.25.3\r\n".to_vec(),
        1 => b"Content-Type: text/plain; charset=utf-8\r\n".to_vec(),
        2 => b"X-Content-Length-Hint: 5\r\n".to_vec(),
        3 => format!("Date: Wed, {} Sep 2026 10:00:00 GMT\r\n", 1 + r.below(28)).into_bytes(),
        4 => b"Connection: close\r\n".to_vec(),
        _ => format!("X-Pad: {}\r\n", "a".repeat(r.below(400) as usize)).into_bytes(),
    }
}

/// How an answer is cut up on its way: in one piece, in many small ones,
/// or in a few with pauses between.
fn pieces(r: &mut Input, len: usize) -> Vec<(usize, u64)> {
    let mut v = Vec::new();
    let mut left = len;
    let style = r.u8() % 4;
    while left > 0 && v.len() < 64 {
        let n = match style {
            0 => left,
            1 => 1 + r.below(16) as usize,
            _ => 1 + r.below(3000) as usize,
        }.min(left);
        /* Now and then a pause past the client's idle timeout: what came
         * before it is all it gets, and it must not take that for all. */
        let delay = match style {
            3 if r.u8() < 8 => 20 * SEC,
            3 => r.below(400) * MS,
            _ => 0,
        };
        v.push((n, delay));
        left -= n;
    }
    v
}

/// A well-made answer: a status line, headers, a body framed by a length,
/// by chunks or by the close -- or a redirect to `next`, the URL the client
/// is then to fetch -- and the connection closed after it.
fn well_made(r: &mut Input, next: Option<Vec<u8>>) -> Answer {
    let (status, location) = match next {
        Some(url) => (r.pick(&[301u16, 302, 303, 307, 308]), Some(url)),
        None => (r.pick(&[200u16, 200, 200, 203, 404, 500, 204]), None),
    };
    let body = if location.is_some() && r.bool() { Vec::new() } else {
        noise(r.u32(), match r.u8() % 4 {
            0 => 0,
            1 => r.below(64) as usize,
            2 => r.below(3000) as usize,
            _ => r.below(40000) as usize,
        })
    };
    let mut a = format!("HTTP/1.1 {} {}\r\n", status, reason(status)).into_bytes();
    for _ in 0..r.below(4) {
        a.extend_from_slice(&filler(r));
    }
    if let Some(l) = &location {
        a.extend_from_slice(b"Location: ");
        a.extend_from_slice(l);
        a.extend_from_slice(b"\r\n");
    }
    let framing = r.u8() % 3;
    match framing {
        0 => a.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes()),
        1 => a.extend_from_slice(r.pick(&[b"Transfer-Encoding: chunked\r\n" as &[u8],
                                          b"transfer-encoding: Chunked\r\n", b"TRANSFER-ENCODING:chunked\r\n"])),
        _ => {}
    }
    a.extend_from_slice(b"\r\n");
    if a.len() > MAX_HEADER - 4 {
        a.truncate(0);
        a.extend_from_slice(b"HTTP/1.1 200 OK\r\n\r\n");
    }
    if framing == 1 {
        let mut at = 0;
        while at < body.len() {
            let n = (1 + r.below(5000) as usize).min(body.len() - at);
            let size = if r.bool() { format!("{:x}", n) } else { format!("{:X}", n) };
            a.extend_from_slice(size.as_bytes());
            if r.u8() < 32 {
                a.extend_from_slice(b";name=value");
            }
            a.extend_from_slice(b"\r\n");
            a.extend_from_slice(&body[at..at + n]);
            a.extend_from_slice(b"\r\n");
            at += n;
        }
        a.extend_from_slice(b"0\r\n");
        if r.u8() < 32 {
            a.extend_from_slice(b"X-Trailer: yes\r\n");
        }
        a.extend_from_slice(b"\r\n");
    } else {
        a.extend_from_slice(&body);
    }
    let expect = Expect { status: i32::from(status), body, location };
    let p = pieces(r, a.len());
    Answer { bytes: a, pieces: p, end: End::Close, expect: Some(expect) }
}

/// An answer made to break the client: a well-made one with something
/// wrong put in, or bytes of no protocol at all, ended any way.
fn broken(r: &mut Input) -> Answer {
    let mut a = match r.u8() % 10 {
        /* A status code longer than any number. */
        0 => format!("HTTP/1.1 {} OK\r\nContent-Length: 2\r\n\r\nhi", "9".repeat(1 + r.below(30) as usize))
            .into_bytes(),
        /* A chunk size longer than any number, of every hex digit. */
        1 => {
            let digits = 1 + r.below(40) as usize;
            let d = r.pick(&['f', 'F', '1', '9', '0', 'a']);
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{}\r\nabc\r\n0\r\n\r\n",
                    d.to_string().repeat(digits)).into_bytes()
        }
        /* A content length longer than any number, or past the cap. */
        2 => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\nbody",
                     r.pick(&["99999999999999999999999999", "20971521", "18446744073709551616", "-1", "0x10"]))
            .into_bytes(),
        /* A redirect to a URL with a port longer than any number, or none. */
        3 => format!("HTTP/1.1 302 Found\r\nLocation: http://{}:{}/x\r\n\r\n", Ipv4(SERVER_IP),
                     r.pick(&["99999999999", "4294967297", "65536", "0", "", "8080x", "80"])).into_bytes(),
        /* A redirect's target past every buffer. */
        4 => format!("HTTP/1.1 301 Moved\r\nLocation: http://{}/{}\r\n\r\n", Ipv4(SERVER_IP),
                     "p".repeat(r.pick(&[2000usize, 2030, 2040, 3000, 20000]))).into_bytes(),
        /* Headers that never end, to the client's limit and past it. */
        5 => {
            let mut v = b"HTTP/1.1 200 OK\r\n".to_vec();
            let n = r.pick(&[MAX_HEADER - 30, MAX_HEADER - 1, MAX_HEADER, MAX_HEADER + 10, 40000]);
            while v.len() < n {
                v.extend_from_slice(b"X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
            }
            v
        }
        /* Chunking that is wrong in the middle: a size that lies, CRLFs
         * missing, nothing but a line ending. */
        6 => {
            let mut v = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            for _ in 0..1 + r.below(8) {
                match r.u8() % 5 {
                    0 => v.extend_from_slice(b"5\r\nabcdefghij\r\n"),
                    1 => v.extend_from_slice(b"10\r\nshort\r\n"),
                    2 => v.extend_from_slice(b"\r\n"),
                    3 => v.extend_from_slice(b"zz\r\n"),
                    _ => v.extend_from_slice(b"3;;;;;;;;;;;;;;\r\nabc\r\n"),
                }
            }
            v
        }
        /* No status line: bytes. */
        7 => noise(r.u32(), r.below(3000) as usize),
        /* A well-made answer with bytes flipped in it. */
        8 => {
            let mut w = well_made(r, None).bytes;
            for _ in 0..1 + r.below(4) {
                if !w.is_empty() {
                    let i = r.below(w.len() as u64) as usize;
                    w[i] = r.u8();
                }
            }
            w
        }
        /* A well-made answer cut short. */
        _ => {
            let mut w = well_made(r, None).bytes;
            let n = r.below(w.len() as u64 + 1) as usize;
            w.truncate(n);
            w
        }
    };
    if r.u8() < 16 {
        a.extend_from_slice(&noise(r.u32(), r.below(100) as usize));
    }
    let p = pieces(r, a.len());
    let end = match r.u8() % 8 {
        0 => End::Reset,
        1 => End::Hold,
        _ => End::Close,
    };
    Answer { bytes: a, pieces: p, end, expect: None }
}

pub fn http(r: &mut Input) {
    run(r, false);
}

pub fn https(r: &mut Input) {
    run(r, true);
}

/// A URL for a hop: https (on a TLS port) or http, the port left out now
/// and then when it is the scheme's own.
fn hop(r: &mut Input, host: &str, tls: bool, path: &str) -> String {
    let (scheme, port, default) = if tls { ("https", r.pick(&TLS_PORTS), 443) } else { ("http", r.pick(&PORTS), 80) };
    if port == default && r.bool() {
        format!("{}://{}{}", scheme, host, path)
    } else {
        format!("{}://{}:{}{}", scheme, host, port, path)
    }
}

fn run(r: &mut Input, tls: bool) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);

    /* The certificate is for 10.0.2.100: over TLS the server is there. */
    let far = !tls && r.u8() < 32;
    let (server_ip, server_mac) = if far { (FAR_IP, GW_MAC) } else { (SERVER_IP, SERVER_MAC) };
    let link = if r.u8() < 200 { Link::perfect() } else { Link::from_input(r) };
    let mut lan = Lan::new();
    lan.add(SERVER_IP, SERVER_MAC);
    let mut tcp = Tcp::new(server_ip, server_mac);
    tcp.serve = if tls { [PORTS, TLS_PORTS].concat() } else { PORTS.to_vec() };
    tcp.accept_window = r.pick(&[65535u32, 8192, 1024, 100]);
    tcp.accept_mss = Some(r.pick(&[1460u16, 536, 100, 1]));

    /* The URL, and the chain of answers behind it: well made, each a
     * redirect to the next until the last, or with something broken in. */
    let host = Ipv4(server_ip).to_string();
    let first_port = r.pick(&PORTS);
    let path = match r.u8() % 7 {
        0 => "/".to_string(),
        1 => format!("/{}", "q".repeat(r.pick(&[2000usize, 2020, 2030, 2050]))),
        /* A query and no path: the path is "/" (RFC 3986 3.3). */
        2 => format!("?a={}", r.u16()),
        _ => format!("/file{}?a=b&c={}", r.below(100), r.u16()),
    };
    let url = if tls {
        hop(r, &host, true, &path)
    } else if first_port == 80 && r.bool() {
        format!("http://{}{}", host, path)
    } else {
        format!("http://{}:{}{}", host, first_port, path)
    };
    let hops = if r.u8() < 64 { r.below(8) as usize } else { 0 };
    let mut answers = Vec::new();
    let mut urls = vec![url.clone()];
    for k in 0..=hops {
        if r.u8() < 48 {
            answers.push(broken(r));
            break;
        }
        let next = if k < hops {
            let port = r.pick(&PORTS);
            let u = match r.u8() % 8 {
                0 => b"/relative/path".to_vec(),
                1 => format!("ftp://{}/x", host).into_bytes(),
                _ if tls => {
                    let secure = r.bool();
                    hop(r, &host, secure, &format!("/hop{}", k + 1)).into_bytes()
                }
                _ => format!("http://{}:{}/hop{}", host, port, k + 1).into_bytes(),
            };
            urls.push(String::from_utf8_lossy(&u).into_owned());
            Some(u)
        } else {
            None
        };
        answers.push(well_made(r, next));
    }
    let cap = if r.u8() < 16 { r.below(1000) as usize } else { 1 << 20 };

    let mut httpd = Httpd::new(answers);
    let cert = match r.u8() % 16 {
        0 => Cert::Expired,
        1 => Cert::Other,
        2 => Cert::Stranger,
        _ => Cert::Good,
    };
    let corrupt = tls && r.u8() < 16;
    if tls {
        let (v12, v13) = match r.u8() % 4 {
            0 => (true, false),
            1 => (false, true),
            _ => (true, true),
        };
        httpd.tls = Some(Tls { config: tlsd::config(cert, v12, v13), ports: TLS_PORTS.to_vec(), close_notify: r.bool(),
                               corrupt_at: corrupt.then(|| r.below(4000) as usize) });
    }
    let mut w = World::new(link, Wire { lan, tcp, httpd });
    let get_url = url.clone().into_bytes();
    let app = App::spawn("wget", 1, move || {
        let mut sink = Sink { got: Vec::new(), cap };
        let mut location = [0u8; 256];
        let resp = http::get_with_location(&nic, &get_url, &mut sink, &mut location);
        let end = location.iter().position(|&b| b == 0).unwrap_or(location.len());
        Got { status: resp.status, content_length: resp.content_length, body_len: resp.body_len, ok: resp.ok,
              truncated: resp.truncated, tls_failed: resp.tls_failed, url_too_long: resp.url_too_long,
              body: sink.got, location: location[..end].to_vec() }
    });
    /* Every hop: a connect, a request, an answer with pauses, an idle
     * timeout at the end of each -- well within this. */
    let done = w.wait_app(&app, 600 * SEC);
    invariant!(done, "the GET of {} never returned", url);
    let got = app.take().expect("done");
    judge(&w, &url, &got, cap, tls, cert, corrupt);

    /* Everything the client opened, closed: the server hangs up too, and
     * the timers run out. */
    for e in 0..w.peers.tcp.eps.len() {
        if w.peers.tcp.eps[e].open() {
            w.peers.tcp.close(&mut w.net, e);
        }
    }
    w.net.link = Link::perfect();
    w.run_for(200 * SEC);
    super::audit();
}

/// The GET's result, against what the answers make it -- served over TLS
/// with `cert` when `tls`, and a byte of it flipped when `corrupt`.
fn judge(w: &World<Wire>, url: &str, got: &Got, cap: usize, tls: bool, cert: Cert, corrupt: bool) {
    invariant!(got.body.len() == got.body_len, "the sink took {} bytes, the response says {}", got.body.len(),
               got.body_len);
    if !tls {
        invariant!(got.tls_failed == 0, "a plain GET failed its TLS");
    }
    if url.len() >= MAX_URL_LEN {
        invariant!(got.url_too_long == 1 && got.ok == 0, "a URL of {} bytes fetched", url.len());
        return;
    }

    /* Each request the server was asked: the path and host of the URL it
     * was fetched for. Hop k was asked for the k-th URL of the chain. */
    let answers = &w.peers.httpd.answers;
    let requests = &w.peers.httpd.requests;
    for (k, req) in requests.iter().enumerate() {
        let Some(want) = hop_url(answers, url, k) else { break };
        let (host, path) = split_url(&want);
        invariant!(req.path == path.as_bytes() && req.host == host.as_bytes(),
                   "hop {} asked for {:?} of {:?}, its URL being {}", k, String::from_utf8_lossy(&req.path),
                   String::from_utf8_lossy(&req.host), want);
        invariant!(req.raw.starts_with(b"GET ") && req.raw.ends_with(b"\r\nConnection: close\r\n\r\n"),
                   "a request of {:?}", String::from_utf8_lossy(&req.raw));
    }

    /* What the chain of answers comes to, when every answer of it that was
     * asked for is well made. */
    let mut k = 0;
    loop {
        let Some(a) = answers.get(k) else { return };
        let Some(e) = &a.expect else { return };
        /* A hop over TLS to a server the client must not believe: refused,
         * and its request never sent. */
        if cert != Cert::Good && hop_url(answers, url, k).is_some_and(|u| u.starts_with("https://")) {
            invariant!(requests.len() <= k, "a request of hop {} read through TLS, the server's certificate {:?}", k,
                       cert);
            /* Refused as such -- unless the link lost the way there first. */
            invariant!(got.ok == 0 && got.body.is_empty() && (got.tls_failed == 1 || !w.net.link.lossless()),
                       "hop {} fetched over TLS from a server whose certificate is {:?}: ok {} tls_failed {}", k, cert,
                       got.ok, got.tls_failed);
            return;
        }
        if k >= requests.len() {
            /* Never asked for: the hop before it failed, which a well-made
             * chain on a good link does not. */
            return;
        }
        let follows = e.location.as_ref().is_some_and(|l| l.starts_with(b"http://") || l.starts_with(b"https://"));
        /* One redirect too many -- followable or not, the client stops at
         * the sixth -- comes back as nothing at all. */
        if e.location.is_some() && k == MAX_REDIRECTS {
            invariant!(got.ok == 0 && got.status == 0, "the {}th redirect followed: {} came back", k + 1, got.status);
            return;
        }
        if e.location.is_some() && follows {
            k += 1;
            continue;
        }
        let want = &e.body[..e.body.len().min(cap)];
        let stalled = answers.iter().any(|a| a.pieces.iter().any(|p| p.1 >= 10 * SEC));
        if !w.net.link.lossless() || corrupt || stalled {
            /* Over a link that loses and reorders, a pause long enough for
             * the client's idle timeout is the link's: what did arrive is
             * still the answer's, from its start. */
            invariant!(want.starts_with(&got.body), "a body of {} bytes came back as {}, not its start ({:?} for {:?}, \
                       status {}, ok {}, truncated {}, hop {}{})", e.body.len(), got.body.len(),
                       String::from_utf8_lossy(&got.body[..got.body.len().min(48)]),
                       String::from_utf8_lossy(&want[..want.len().min(48)]), got.status, got.ok, got.truncated, k,
                       if corrupt { ", a byte flipped" } else { "" });
            /* And what the client calls whole is whole: a body that stopped
             * coming is never taken for all of it. */
            if got.ok == 1 && got.truncated == 0 && got.status == e.status && e.location.is_none() {
                invariant!(got.body == want, "a body of {} bytes came back as {}, and said to be whole", e.body.len(),
                           got.body.len());
            }
            return;
        }
        invariant!(got.ok == 1, "a well-made answer of {} to {} came back not ok", e.status, url);
        invariant!(got.status == e.status, "status {} came back for an answer of {}", got.status, e.status);
        if let Some(l) = &e.location {
            invariant!(got.body.is_empty(), "a redirect's body taken");
            let keep = l.len().min(255);
            invariant!(got.location == l[..keep], "the redirect not followed told as {:?}, it being to {:?}",
                       String::from_utf8_lossy(&got.location), String::from_utf8_lossy(l));
            return;
        }
        let whole = e.body.len() <= cap;
        invariant!(got.body == want, "a body of {} bytes came back as {} ({} of them right)", e.body.len(),
                   got.body.len(), got.body.iter().zip(e.body.iter()).take_while(|(a, b)| a == b).count());
        invariant!((got.truncated == 0) == whole, "a body of {} bytes into a sink of {} came back {}truncated",
                   e.body.len(), cap, if got.truncated == 0 { "not " } else { "" });
        let _ = got.content_length;
        return;
    }
}

/// The URL hop `k` of the chain fetches.
fn hop_url(answers: &[Answer], first: &str, k: usize) -> Option<String> {
    let mut url = first.to_string();
    for a in answers.iter().take(k) {
        let l = a.expect.as_ref()?.location.as_ref()?;
        url = String::from_utf8_lossy(l).into_owned();
    }
    Some(url)
}

/// A URL's host and path, as the client sends them.
fn split_url(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")).unwrap_or(url);
    let host_end = rest.find(['/', ':', '?']).unwrap_or(rest.len());
    let host = rest[..host_end].to_string();
    let path = match (rest.find('/'), rest.find('?')) {
        (Some(slash), q) if q.is_none_or(|q| slash < q) => rest[slash..].to_string(),
        (_, Some(q)) => format!("/{}", &rest[q..]),
        _ => "/".to_string(),
    };
    (host, path)
}
