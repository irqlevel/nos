//! An HTTP server of the world's, on the world's TCP: it reads what the
//! machine's HTTP client asks and answers it -- in pieces, at the pace and
//! in the shape the input says, and then ends the connection, or resets it,
//! or holds it open and says nothing more.
//!
//! What it answers is either well made, and then what the client must make
//! of it is known exactly (`Answer::expect`), or made to break the client:
//! numbers longer than any field, chunk sizes of every length, headers
//! that do not end, a redirect to anywhere.
//!
//! On the ports it is told to, it speaks TLS (`tlsd`): the request comes in
//! through a session, and the answer goes out through it, in records as its
//! pieces go -- a close_notify after it or not, and, when the input says, a
//! byte of what it sends flipped on the way.

use std::sync::Arc;

use super::tcpm::{Tcp, S};
use super::tlsd::Session;
use super::Net;
use crate::machine::sched;

/// How a connection ends once the answer is out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum End {
    /// A FIN after the last byte.
    Close,
    /// A reset instead.
    Reset,
    /// Nothing: the connection stays open, the server quiet.
    Hold,
}

/// One answer: its bytes, how they go out, and -- when it is well made --
/// what the client must make of it.
pub struct Answer {
    pub bytes: Vec<u8>,
    /// How many bytes go at a time, and how long after the one before.
    pub pieces: Vec<(usize, u64)>,
    pub end: End,
    pub expect: Option<Expect>,
}

/// What a well-made answer is to the client.
#[derive(Clone, Debug)]
pub struct Expect {
    pub status: i32,
    pub body: Vec<u8>,
    /// Its Location, when it is a redirect.
    pub location: Option<Vec<u8>>,
}

/// A request as the server saw it.
#[derive(Clone, Debug)]
pub struct Request {
    pub raw: Vec<u8>,
    pub path: Vec<u8>,
    pub host: Vec<u8>,
}

/// One connection's exchange: the request coming in, then the answer
/// going out.
struct Exchange {
    ep: usize,
    answer: usize,
    request: Option<Request>,
    sent: usize,
    piece: usize,
    next_at: u64,
    ended: bool,
    /// Its TLS session, on a TLS port, and how many bytes of it have gone.
    tls: Option<Session>,
    sent_tls: usize,
}

/// How the server speaks TLS: on which ports, and what it does besides.
pub struct Tls {
    pub config: Arc<rustls::ServerConfig>,
    pub ports: Vec<u16>,
    /// A close_notify before the connection ends, or not.
    pub close_notify: bool,
    /// Which byte of what a connection sends is flipped, if any.
    pub corrupt_at: Option<usize>,
}

pub struct Httpd {
    pub answers: Vec<Answer>,
    exchanges: Vec<Exchange>,
    /// Every request the server was asked, in order.
    pub requests: Vec<Request>,
    pub tls: Option<Tls>,
    /// TLS sessions that failed before a request came through them.
    pub tls_refused: usize,
}

impl Httpd {
    pub fn new(answers: Vec<Answer>) -> Httpd {
        Httpd { answers, exchanges: Vec::new(), requests: Vec::new(), tls: None, tls_refused: 0 }
    }

    /// What the server sends on an exchange's connection: through its
    /// session, if it has one, flipped where the input says.
    fn send(tls: &Option<Tls>, x: &mut Exchange, tcp: &mut Tcp, net: &mut Net, mut bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let (Some(t), Some(_)) = (tls, &x.tls) {
            if let Some(at) = t.corrupt_at {
                if at >= x.sent_tls && at < x.sent_tls + bytes.len() {
                    bytes[at - x.sent_tls] ^= 0x40;
                }
            }
            x.sent_tls += bytes.len();
        }
        tcp.eps[x.ep].write_bytes(&bytes);
        tcp.output(net, x.ep);
    }

    /// Whatever the peer's connections have brought in, and whatever the
    /// answers owe them now.
    pub fn serve(&mut self, tcp: &mut Tcp, net: &mut Net) {
        /* A connection the server has not seen: the next answer is its --
         * through a session of its own on a TLS port. */
        for (e, ep) in tcp.eps.iter().enumerate() {
            if ep.data.is_some() && !self.exchanges.iter().any(|x| x.ep == e) {
                let answer = self.exchanges.len();
                let tls = self.tls.as_ref().filter(|t| t.ports.contains(&ep.port))
                    .map(|t| Session::new(t.config.clone()));
                self.exchanges.push(Exchange { ep: e, answer, request: None, sent: 0, piece: 0, next_at: 0,
                                               ended: false, tls, sent_tls: 0 });
            }
        }
        let now = sched::now();
        for x in self.exchanges.iter_mut() {
            /* The server reads what came, as it comes: its window opens. */
            let unread = tcp.eps[x.ep].unread;
            if unread != 0 {
                tcp.read(net, x.ep, unread);
            }
            let rx = tcp.eps[x.ep].data.as_ref().expect("a served connection").rx.clone();
            if let Some(session) = x.tls.as_mut() {
                let out = session.feed(&rx);
                let failed = session.failed;
                Self::send(&self.tls, x, tcp, net, out);
                if failed && !x.ended {
                    /* The session is broken: the server hangs up. */
                    x.ended = true;
                    if x.request.is_none() {
                        self.tls_refused += 1;
                    }
                    tcp.close(net, x.ep);
                    continue;
                }
            }
            if x.request.is_none() {
                let plain = x.tls.as_ref().map_or(&rx, |s| &s.plain);
                if let Some(end) = plain.windows(4).position(|w| w == b"\r\n\r\n") {
                    let req = parse_request(&plain[..end + 4]);
                    self.requests.push(req.clone());
                    x.request = Some(req);
                    x.next_at = now;
                }
                continue;
            }
            if x.ended || !matches!(tcp.eps[x.ep].st, S::Established | S::CloseWait) {
                continue;
            }
            let Some(a) = self.answers.get(x.answer) else {
                /* No answer left for it: the server hangs up. */
                x.ended = true;
                tcp.close(net, x.ep);
                continue;
            };
            while now >= x.next_at && x.sent < a.bytes.len() {
                let (n, delay) = a.pieces.get(x.piece).copied().unwrap_or((a.bytes.len(), 0));
                let n = n.clamp(1, a.bytes.len() - x.sent);
                let piece = &a.bytes[x.sent..x.sent + n];
                let bytes = match x.tls.as_mut() {
                    Some(session) => session.seal(&rx, piece),
                    None => piece.to_vec(),
                };
                Self::send(&self.tls, x, tcp, net, bytes);
                x.sent += n;
                x.piece += 1;
                x.next_at = now + delay;
            }
            if x.sent == a.bytes.len() && now >= x.next_at {
                x.ended = true;
                if let (Some(session), Some(t)) = (x.tls.as_mut(), self.tls.as_ref()) {
                    if t.close_notify && a.end != End::Reset {
                        let bytes = session.close_notify(&rx);
                        Self::send(&self.tls, x, tcp, net, bytes);
                    }
                }
                match a.end {
                    End::Close => tcp.close(net, x.ep),
                    End::Reset => tcp.abort(net, x.ep),
                    End::Hold => {}
                }
            }
        }
    }

    /// When the server next has something to send.
    pub fn next_timer(&self, tcp: &Tcp) -> u64 {
        self.exchanges.iter().filter(|x| x.request.is_some() && !x.ended)
            .filter(|x| matches!(tcp.eps[x.ep].st, S::Established | S::CloseWait))
            .map(|x| x.next_at).min().unwrap_or(u64::MAX)
    }
}

fn parse_request(raw: &[u8]) -> Request {
    let text = raw.to_vec();
    let line_end = text.windows(2).position(|w| w == b"\r\n").unwrap_or(text.len());
    let line = &text[..line_end];
    let mut parts = line.split(|&b| b == b' ');
    let _method = parts.next().unwrap_or(b"");
    let path = parts.next().unwrap_or(b"").to_vec();
    let host = text.windows(6).position(|w| w == b"Host: ")
        .map(|at| {
            let rest = &text[at + 6..];
            rest[..rest.windows(2).position(|w| w == b"\r\n").unwrap_or(rest.len())].to_vec()
        })
        .unwrap_or_default();
    Request { raw: text, path, host }
}
