//! `tls_client`: the kernel's pinned TLS client (`tls::pinned::Client`) --
//! what ipcnet's link to another host will be (plans/ipc.md, I4) -- run as a
//! module will run it: a task that connects over the kernel's TCP, shows its
//! own key, takes the server by its key, says lines and reads them back.
//!
//! The world's server (rustls, through `tlsd`'s session) answers each
//! connection as the input plans it: with its own key, a stranger's, or its
//! own certificate signed by another key; asking for the client's
//! certificate and letting the machine's key in, asking and letting in
//! another's, or not asking; or breaking the protocol -- an HTTP answer where
//! the ServerHello would be, nothing at all, a hang-up, TLS 1.2 alone, no
//! protocol in common, a byte of what it sends flipped. Then it says back
//! every byte it is said.
//!
//! What must hold of every connection, whatever the link does: a session is
//! up only with the server's own key, proved -- never a stranger's, never an
//! impostor's -- and what the client reads is what it said, or less of it,
//! never a byte the link or the server changed. Over a good link each
//! connection comes to what its plan says: refused for its reason, or a
//! session whose every byte comes back, closed cleanly -- the client closing
//! first, as the server it talks to expects of its clients.

use std::sync::Arc;
use std::time::Duration;

use kcore::tcp::{TcpStream, RECV_TIMEOUT};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme};
use tls::pinned::{Client, Fingerprint, Flush, Read, Recv, Refused, Socket, HANDSHAKE_MS};

use crate::machine::sched;
use crate::world::lan::Lan;
use crate::world::tcpm::{Tcp, S};
use crate::world::tlsc::keys;
use crate::world::tlsd::Session;
use crate::world::{App, Link, Net, Peers, World, ETH0_IP, GW_IP, MASK};
use crate::Input;

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// The world's server: on ipcnet's port, at an address on the LAN.
const PORT: u16 = 39240;
const SERVER_IP: u32 = 0x0A00_0264;
const SERVER_MAC: [u8; 6] = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x64];
/// What the client offers by ALPN: ipcnet's.
const ALPN: &[u8] = b"nos-ipc/1";
/// What waits to be sent, at most, before the client flushes.
const MAX_PENDING: usize = 64 * 1024;
/// How long the client waits for what it said to come back.
const ECHO_MS: u64 = 20_000;
/// Connections an input makes at most.
const CONNECTIONS: usize = 8;

/// Whose certificate the server shows, and with whose key it signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Show {
    /// Its own, which the client was given.
    Own,
    /// A stranger's.
    Stranger,
    /// Its own certificate, signed by the stranger's key: what a server
    /// that copied the certificate, and not the key, would show.
    Impostor,
}

/// What the server asks of the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    /// Its certificate, letting the machine's key in.
    Machine,
    /// Its certificate, letting another key in and not the machine's.
    Another,
    /// Nothing.
    Nothing,
}

/// Where the server breaks the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Break {
    None,
    /// An HTTP answer where its ServerHello would be.
    Garbage,
    /// Nothing at all.
    Silent,
    /// It hangs up as soon as the connection is made.
    Cut,
    /// TLS 1.2 alone.
    Tls12,
    /// Only a protocol the client does not offer.
    Alpn,
    /// The byte at this offset of what it sends, flipped.
    Tamper(usize),
}

#[derive(Clone, Debug)]
struct Plan {
    show: Show,
    ask: Ask,
    brk: Break,
    /// What the server speaks by ALPN.
    alpn: Vec<&'static [u8]>,
    /// What the client says.
    lines: Vec<Vec<u8>>,
}

impl Plan {
    fn said(&self) -> Vec<u8> {
        self.lines.concat()
    }
}

fn plan(r: &mut Input) -> Plan {
    let show = match r.u8() % 8 {
        0 => Show::Stranger,
        1 => Show::Impostor,
        _ => Show::Own,
    };
    let ask = match r.u8() % 8 {
        0 => Ask::Another,
        1 => Ask::Nothing,
        _ => Ask::Machine,
    };
    let brk = match r.u8() % 24 {
        0 => Break::Garbage,
        1 => Break::Silent,
        2 => Break::Cut,
        3 => Break::Tls12,
        4 => Break::Alpn,
        5 | 6 => Break::Tamper(r.below(3000) as usize),
        _ => Break::None,
    };
    let alpn: Vec<&'static [u8]> = match (brk, r.u8() % 4) {
        (Break::Alpn, _) => vec![b"h2"],
        (_, 0) => vec![],
        (_, 1) => vec![b"h2", ALPN],
        _ => vec![ALPN],
    };
    let lines = (0..r.below(6)).map(|i| {
        let len = r.pick(&[1usize, 10, 100, 1000, 17_000]);
        (0..len).map(|k| b'a' + ((i as usize * 7 + k) % 26) as u8).collect()
    }).collect();
    Plan { show, ask, brk, alpn, lines }
}

/// The server's clock: any time does -- nothing judges a certificate's dates.
#[derive(Debug)]
struct AnyTime;

impl rustls::time_provider::TimeProvider for AnyTime {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(sched::wall_secs())))
    }
}

/// The impostor's certificate and key, handed to rustls as they are --
/// `with_single_cert` would refuse a key that is not its certificate's.
#[derive(Debug)]
struct Impostor(Arc<CertifiedKey>);

impl rustls::server::ResolvesServerCert for Impostor {
    fn resolve(&self, _hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// The client's key, as the world's server pins it.
#[derive(Debug)]
struct PinnedClient {
    key: Fingerprint,
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for PinnedClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
                          _now: UnixTime) -> Result<ClientCertVerified, Error> {
        match tls::pinned::fingerprint(end_entity) {
            Some(k) if k == self.key => Ok(ClientCertVerified::assertion()),
            _ => Err(Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)),
        }
    }

    fn verify_tls12_signature(&self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        Err(Error::InvalidCertificate(CertificateError::BadSignature))
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// The world's server as a connection's plan has it.
fn config(p: &Plan) -> Arc<ServerConfig> {
    let keys = keys();
    let provider = Arc::new(rustls_rustcrypto::provider());
    let versions: &[&'static rustls::SupportedProtocolVersion] =
        if p.brk == Break::Tls12 { &[&rustls::version::TLS12] } else { &[&rustls::version::TLS13] };
    let builder = ServerConfig::builder_with_details(provider.clone(), Arc::new(AnyTime))
        .with_protocol_versions(versions)
        .expect("versions the provider has");
    let builder = match p.ask {
        Ask::Nothing => builder.with_no_client_auth(),
        Ask::Machine | Ask::Another => {
            let who = if p.ask == Ask::Machine { &keys.known } else { &keys.reader };
            let key = tls::pinned::fingerprint(&who.cert).expect("the world's own certificate");
            builder.with_client_cert_verifier(Arc::new(PinnedClient { key,
                                                                      algs: provider.signature_verification_algorithms }))
        }
    };
    let mut config = match p.show {
        Show::Impostor => {
            let key = provider.key_provider
                .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(keys.stranger.key.to_vec())))
                .expect("the stranger's key");
            let certified = CertifiedKey::new(vec![CertificateDer::from(keys.server.cert.clone())], key);
            builder.with_cert_resolver(Arc::new(Impostor(Arc::new(certified))))
        }
        Show::Own | Show::Stranger => {
            let id = if p.show == Show::Own { &keys.server } else { &keys.stranger };
            builder.with_single_cert(vec![CertificateDer::from(id.cert.clone())],
                                     PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key.to_vec())))
                .expect("the world's own key")
        }
    };
    config.alpn_protocols = p.alpn.iter().map(|a| a.to_vec()).collect();
    Arc::new(config)
}

/// The connection the world's server is serving, and how.
struct Served {
    ep: usize,
    plan: Plan,
    session: Option<Session>,
    /// What it has said back of what the client said.
    echoed: usize,
    /// How much it has sent: where a tamper's offset falls.
    sent: usize,
    /// Its garbage said.
    acted: bool,
    /// It has hung up.
    ended: bool,
}

struct Wire {
    lan: Lan,
    tcp: Tcp,
    /// The plan of the connection the machine makes next.
    plan: Option<Plan>,
    served: Option<Served>,
    /// The peer's connections looked at so far: each new one is served.
    known: usize,
}

impl Wire {
    /// What the server sends, through the link -- the plan's byte flipped.
    fn send(tcp: &mut Tcp, net: &mut Net, s: &mut Served, mut bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Break::Tamper(at) = s.plan.brk {
            if at >= s.sent && at < s.sent + bytes.len() {
                bytes[at - s.sent] ^= 0x40;
            }
        }
        s.sent += bytes.len();
        tcp.eps[s.ep].write_bytes(&bytes);
        tcp.output(net, s.ep);
    }

    fn tend(&mut self, net: &mut Net) {
        /* A connection the server has not seen: the plan's, or one nobody
         * planned, hung up on */
        for e in self.known..self.tcp.eps.len() {
            if self.tcp.eps[e].data.is_none() {
                continue;
            }
            match self.plan.take() {
                Some(plan) => self.served = Some(Served { ep: e, plan, session: None, echoed: 0, sent: 0,
                                                          acted: false, ended: false }),
                None => self.tcp.close(net, e),
            }
        }
        self.known = self.tcp.eps.len();

        let Some(s) = self.served.as_mut() else { return };
        if s.ended || !matches!(self.tcp.eps[s.ep].st, S::Established | S::CloseWait) {
            return;
        }
        let unread = self.tcp.eps[s.ep].unread;
        if unread != 0 {
            self.tcp.read(net, s.ep, unread);
        }
        let rx = self.tcp.eps[s.ep].data.as_ref().expect("a served connection").rx.clone();
        match s.plan.brk {
            Break::Cut => {
                s.ended = true;
                self.tcp.close(net, s.ep);
                return;
            }
            Break::Silent => return,
            Break::Garbage => {
                if !rx.is_empty() && !s.acted {
                    s.acted = true;
                    Self::send(&mut self.tcp, net, s, b"HTTP/1.1 400 Bad Request\r\n\r\n".to_vec());
                }
                return;
            }
            _ => {}
        }
        let mut session = s.session.take().unwrap_or_else(|| Session::new(config(&s.plan)));
        let out = session.feed(&rx);
        Self::send(&mut self.tcp, net, s, out);
        if session.failed {
            s.ended = true;
            s.session = Some(session);
            self.tcp.close(net, s.ep);
            return;
        }
        /* Every byte the client said, said back */
        if session.plain.len() > s.echoed {
            let data = session.plain[s.echoed..].to_vec();
            let out = session.seal(&rx, &data);
            if !out.is_empty() {
                s.echoed += data.len();
                Self::send(&mut self.tcp, net, s, out);
            }
        }
        /* The client's FIN, after its close_notify: the server's goodbye,
         * and its own FIN after the client's */
        if self.tcp.eps[s.ep].st == S::CloseWait {
            let out = session.close_notify(&rx);
            Self::send(&mut self.tcp, net, s, out);
            s.ended = true;
            self.tcp.close(net, s.ep);
        }
        s.session = Some(session);
    }
}

impl Peers for Wire {
    fn on_frame(&mut self, net: &mut Net, frame: &[u8]) {
        if !self.lan.on_frame(net, frame) {
            self.tcp.on_frame(net, frame);
        }
        self.tend(net);
    }

    fn next_timer(&self) -> u64 {
        self.tcp.next_timer()
    }

    fn on_timer(&mut self, net: &mut Net) {
        self.tcp.on_timer(net);
        self.tend(net);
    }
}

/// What a connection came to, at the machine's end.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// The TCP connection was never made.
    NoConnection,
    Refused(Refused),
    Session { key: Fingerprint, alpn: Option<Vec<u8>>, got: Vec<u8>, ended: &'static str, clean: bool },
}

/// The kernel's TCP, as a TLS session runs over it.
struct Conn(TcpStream);

impl Socket for Conn {
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv {
        match self.0.recv(buf, wait_ms.max(1)) {
            n if n > 0 => Recv::Data(n as usize),
            0 => Recv::End,
            RECV_TIMEOUT => Recv::Idle,
            _ => Recv::Failed,
        }
    }

    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize> {
        let n = self.0.send(buf, wait_ms.max(1));
        if n >= 0 { Some(n as usize) } else { None }
    }
}

/// A connection, as a module makes one: the handshake, what it has to say
/// said and read back, and its goodbye.
fn connect((client, lines): (Arc<Client>, Vec<Vec<u8>>)) -> Outcome {
    let Some(nic) = kcore::net::Nic::find("eth0") else { return Outcome::NoConnection };
    let Some(stream) = TcpStream::connect(&nic, SERVER_IP, PORT) else { return Outcome::NoConnection };
    let mut s = match client.connect(Conn(stream), SERVER_IP, HANDSHAKE_MS) {
        Ok(s) => s,
        Err((sock, why)) => {
            sock.0.abort();
            return Outcome::Refused(why);
        }
    };
    let key = *s.server();
    let alpn = s.alpn().map(|a| a.to_vec());
    let want: usize = lines.iter().map(|l| l.len()).sum();
    let mut ended = "done";
    'say: for line in &lines {
        while !s.write(line, MAX_PENDING) {
            if s.flush(1000) == Flush::Failed {
                ended = "failed";
                break 'say;
            }
        }
    }
    let mut got = Vec::new();
    let mut buf = [0u8; 2048];
    let mut idle = 0;
    while ended == "done" && got.len() < want {
        if s.flush(1000) == Flush::Failed {
            ended = "failed";
            break;
        }
        match s.read(&mut buf, 1000) {
            Read::Data(n) => {
                got.extend_from_slice(&buf[..n]);
                idle = 0;
            }
            Read::Idle => {
                idle += 1000;
                if idle >= ECHO_MS {
                    ended = "idle";
                }
            }
            Read::End => ended = "end",
            Read::Failed => ended = "failed",
        }
    }
    let clean = if ended == "done" {
        match s.close(2000) {
            (sock, true) => {
                drop(sock);
                true
            }
            (sock, false) => {
                sock.0.abort();
                false
            }
        }
    } else {
        s.into_socket().0.abort();
        false
    };
    Outcome::Session { key, alpn, got, ended, clean }
}

/// What must hold of every connection, over any link: a session only with
/// the server's own key, and nothing read but what was said.
fn hold_always(p: &Plan, o: &Outcome, server: &Fingerprint) {
    if let Outcome::Session { key, got, ended, .. } = o {
        invariant!(key == server, "a session with a key the client was not given: {:?}", p);
        invariant!(p.show == Show::Own, "a session with a server showing {:?}: {:?}", p.show, p.brk);
        let said = p.said();
        invariant!(got.len() <= said.len() && got[..] == said[..got.len()],
                   "read {} bytes that are not what was said ({} said), the connection ended {}", got.len(),
                   said.len(), ended);
    }
}

/// What a connection must have come to, over a link that loses nothing.
fn judge(p: &Plan, o: &Outcome, server: &Fingerprint) {
    hold_always(p, o, server);
    let refused = |want: &[Refused]| {
        invariant!(matches!(o, Outcome::Refused(why) if want.contains(why)),
                   "a server {:?} asking {:?} {:?} speaking {:?}: {:?}, not refused as {:?}", p.show, p.ask, p.brk,
                   p.alpn.iter().map(|a| String::from_utf8_lossy(a)).collect::<Vec<_>>(), o, want);
    };
    match p.brk {
        Break::Garbage => refused(&[Refused::Protocol]),
        Break::Silent => refused(&[Refused::Timeout]),
        Break::Cut => refused(&[Refused::Closed, Refused::Io]),
        /* The world's server hangs up on a handshake it refuses without
         * sending its alert (`tlsd`): the client is told, or sees the end */
        Break::Tls12 | Break::Alpn => refused(&[Refused::ServerAlert, Refused::Incompatible, Refused::Closed]),
        /* Wherever the byte fell: refused, or a session cut short -- what
         * `hold_always` holds it to is that nothing changed was read */
        Break::Tamper(_) => {}
        Break::None => match (p.show, p.ask) {
            (Show::Stranger, _) => refused(&[Refused::UnknownKey]),
            /* Its certificate is the server's, but it does not hold the key:
             * the handshake's signature gives it away */
            (Show::Impostor, _) => refused(&[Refused::BadCertificate]),
            /* The server refuses the client's key after the client's
             * Finished -- which ends the client's handshake in TLS 1.3 -- so
             * the client may be in a session the server's alert then ends:
             * either way, nothing read */
            (Show::Own, Ask::Another) => {
                let ok = match o {
                    Outcome::Refused(why) => *why == Refused::ServerAlert,
                    Outcome::Session { got, ended, clean, .. } => got.is_empty() && *ended != "done" && !clean,
                    Outcome::NoConnection => false,
                };
                invariant!(ok || p.lines.is_empty() && matches!(o, Outcome::Session { .. }),
                           "a server that refused the client's key: {:?}", o);
            }
            (Show::Own, _) => {
                let Outcome::Session { alpn, got, ended, clean, .. } = o else {
                    invariant!(false, "the server's own key was not taken: {:?} {:?}, came to {:?}", p.ask, p.alpn, o);
                    return;
                };
                let agreed = p.alpn.contains(&ALPN).then(|| ALPN.to_vec());
                invariant!(*alpn == agreed, "agreed on {:?} where the server spoke {:?}", alpn, p.alpn);
                invariant!(*got == p.said() && *ended == "done" && *clean,
                           "a session read {} bytes of {} said, ended {}, closed cleanly: {}", got.len(),
                           p.said().len(), ended, clean);
            }
        },
    }
}

pub fn tls_client(r: &mut Input) {
    let nic = net::Nic::find("eth0").expect("eth0");
    nic.set_ip(ETH0_IP);
    nic.set_mask(MASK);
    nic.set_gw(GW_IP);

    let keys = keys();
    let server = tls::pinned::fingerprint(&keys.server.cert).expect("the server's certificate");
    let client = Arc::new(Client::new(&keys.known.cert, &keys.known.key, &[server], &[ALPN])
        .expect("the client's setup"));

    let link = if r.u8() < 216 { Link::perfect() } else { Link::from_input(r) };
    let lossless = link.lossless();
    let mut lan = Lan::new();
    lan.add(SERVER_IP, SERVER_MAC);
    let mut tcp = Tcp::new(SERVER_IP, SERVER_MAC);
    tcp.serve.push(PORT);
    let mut w = World::new(link, Wire { lan, tcp, plan: None, served: None, known: 0 });
    w.run_for(100 * MS);

    let mut made = 0;
    while let Some(op) = r.op(4) {
        if op == 3 {
            w.run_for(r.below(3000) * MS);
            continue;
        }
        if made == CONNECTIONS {
            continue;
        }
        made += 1;
        let p = plan(r);
        w.peers.plan = Some(p.clone());
        let lines = p.lines.clone();
        let c = client.clone();
        let app = App::spawn("tls/client", r.below(4) as u32, move || connect((c, lines)));
        invariant!(w.wait_app(&app, 2 * HANDSHAKE_MS * MS + 120 * SEC), "a connection's task never ended: {:?}", p);
        let outcome = app.take().expect("a connection's outcome");
        if lossless {
            judge(&p, &outcome, &server);
        } else {
            hold_always(&p, &outcome, &server);
        }
        if matches!(outcome, Outcome::Session { .. }) {
            crate::common::runner::reached("tls_client: a session");
        }
        /* A connection nobody planned is hung up on */
        w.peers.plan = None;
        w.run_for(100 * MS);
    }

    /* Everything over: the machine's TIME-WAITs run out, the world's ends
     * close */
    for e in 0..w.peers.tcp.eps.len() {
        if w.peers.tcp.eps[e].open() {
            w.peers.tcp.close(&mut w.net, e);
        }
    }
    w.run_for(200 * SEC);
    super::audit();
}
