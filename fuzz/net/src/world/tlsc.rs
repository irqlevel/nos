//! The world's TLS clients, for the kernel's TLS server (`tls::server`, what
//! the host API serves over): the same rustls the server is built on, its
//! unbuffered client driven by hand over the world's TCP -- showing the key
//! the input chooses (the one the server lets in, a stranger's, or none),
//! speaking the version and offering the protocols it chooses, pinning the
//! server's key as `nosctl` does, and breaking the protocol where it says.
//!
//! A client that is let in says its lines, each once the one before has
//! come back as the server answers it ("ok " and the line), then says
//! goodbye: its close_notify, and its FIN.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{Resumption, UnbufferedClientConnection};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::unbuffered::{ConnectionState, UnbufferedStatus};
use rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};

use crate::machine::sched;
use crate::world::tcpm::Tcp;
use crate::world::Net;

/// Whose key a client shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// The key the server lets in.
    Known,
    /// One it does not.
    Stranger,
    /// None at all.
    Nobody,
    /// The known key's certificate, and a handshake signed by the
    /// stranger's: what a client that copied the certificate, and not the
    /// key, would show.
    Impostor,
}

/// The impostor's certificate and key, handed to rustls as they are --
/// `with_client_auth_cert` would refuse a key that is not its certificate's.
#[derive(Debug)]
struct Impostor(Arc<rustls::sign::CertifiedKey>);

impl rustls::client::ResolvesClientCert for Impostor {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Where a client breaks the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Break {
    None,
    /// Not TLS at all: an HTTP request where the ClientHello would be.
    Garbage,
    /// Connects, and says nothing.
    Silent,
    /// Its ClientHello, and then its FIN.
    Cut,
    /// A bit of the record of its last line flipped.
    Tamper,
    /// TLS 1.2 alone.
    Tls12,
    /// A protocol the server does not speak, and no other.
    Alpn,
}

/// What a client does.
#[derive(Clone, Debug)]
pub struct Plan {
    pub who: Who,
    pub brk: Break,
    /// The protocols it offers, in its order.
    pub alpn: Vec<&'static [u8]>,
    /// What it says once it is in: lines, each ending in a newline.
    pub lines: Vec<Vec<u8>>,
}

/// How a client's connection ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The server's close_notify, after the client's.
    Closed,
    /// rustls stopped: the server's alert, or what the client made of what
    /// it sent.
    Error(String),
    /// The connection reset or closed under it.
    Gone,
}

/// The keys the world's parties hold: P-256 secrets, and the certificates
/// web::x509 makes of them.
pub struct Keys {
    pub server: web::x509::Identity,
    pub known: web::x509::Identity,
    pub stranger: web::x509::Identity,
}

/// When every certificate here is valid: 2026 to 2050. Nobody looks.
const NOT_BEFORE: u64 = 1_767_225_600;
const NOT_AFTER: u64 = 2_524_608_000;

pub fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let make = |secret: u8, name: &str, usage: web::x509::Usage, ips: &'static [[u8; 4]]| {
            let req = web::x509::Request { name, ips, dns: if usage == web::x509::Usage::Server { &["nos"] } else { &[] },
                                           serial: [secret; 16], not_before: NOT_BEFORE, not_after: NOT_AFTER, usage };
            web::x509::self_signed(&[secret; 32], &req).expect("a certificate of the world's own")
        };
        Keys {
            server: make(0x11, "nos 10.0.2.15", web::x509::Usage::Server, &[[10, 0, 2, 15]]),
            known: make(0x22, "panel", web::x509::Usage::Client, &[]),
            stranger: make(0x33, "stranger", web::x509::Usage::Client, &[]),
        }
    })
}

/// The server's key, as a client that pins it checks it.
#[derive(Debug)]
struct PinnedServer {
    key: [u8; 32],
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
                          _name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, Error> {
        match tls::server::fingerprint(end_entity) {
            Some(k) if k == self.key => Ok(ServerCertVerified::assertion()),
            _ => Err(Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
        }
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[derive(Debug)]
struct Clock;

impl rustls::time_provider::TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(sched::wall_secs())))
    }
}

fn config(plan: &Plan) -> Arc<ClientConfig> {
    let keys = keys();
    let provider = Arc::new(rustls_rustcrypto::provider());
    let algs = provider.signature_verification_algorithms;
    let versions: &[&'static rustls::SupportedProtocolVersion] =
        if plan.brk == Break::Tls12 { &[&rustls::version::TLS12] } else { &[&rustls::version::TLS13] };
    let server = tls::server::fingerprint(&keys.server.cert).expect("the server's certificate");
    let builder = ClientConfig::builder_with_details(provider.clone(), Arc::new(Clock))
        .with_protocol_versions(versions)
        .expect("versions the provider has")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer { key: server, algs }));
    let id = match plan.who {
        Who::Known => Some(&keys.known),
        Who::Stranger => Some(&keys.stranger),
        Who::Nobody | Who::Impostor => None,
    };
    let mut config = match (id, plan.who) {
        (Some(id), _) => builder
            .with_client_auth_cert(vec![CertificateDer::from(id.cert.clone())],
                                   PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(id.key.to_vec())))
            .expect("the world's own key"),
        (None, Who::Impostor) => {
            let key = provider.key_provider
                .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(keys.stranger.key.to_vec())))
                .expect("the stranger's key");
            let certified = rustls::sign::CertifiedKey::new(vec![CertificateDer::from(keys.known.cert.clone())], key);
            builder.with_client_cert_resolver(Arc::new(Impostor(Arc::new(certified))))
        }
        (None, _) => builder.with_no_client_auth(),
    };
    config.alpn_protocols = plan.alpn.iter().map(|p| p.to_vec()).collect();
    config.resumption = Resumption::disabled();
    Arc::new(config)
}

/// How many turns one pump takes at most.
const TURNS: usize = 256;
const RECORD: usize = 16 * 1024 + 512;

/// One client: its connection's end in the world's TCP, and its session.
pub struct Client {
    pub ep: usize,
    pub plan: Plan,
    conn: Option<UnbufferedClientConnection>,
    /// How much of what the connection received has been handed in.
    fed: usize,
    incoming: Vec<u8>,
    started: bool,
    /// Its handshake is done, as it sees it -- which, in TLS 1.3, is before
    /// the server has judged its key.
    pub up: bool,
    pub lines_sent: usize,
    /// What the server said, decrypted.
    pub got: Vec<u8>,
    pub said_bye: bool,
    pub ending: Option<Ending>,
    tampered: bool,
    /// When it connected.
    pub at: u64,
}

impl Client {
    pub fn new(ep: usize, plan: Plan) -> Client {
        let conn = match plan.brk {
            Break::Garbage | Break::Silent => None,
            _ => Some(UnbufferedClientConnection::new(config(&plan), ServerName::try_from("nos").expect("a name")
                                                         .to_owned()).expect("a client connection")),
        };
        Client { ep, plan, conn, fed: 0, incoming: Vec::new(), started: false, up: false, lines_sent: 0, got: Vec::new(),
                 said_bye: false, ending: None, tampered: false, at: sched::now() }
    }

    pub fn over(&self) -> bool {
        self.ending.is_some()
    }

    /// What it expects back: each of its lines, as the server answers it.
    pub fn answers(&self) -> Vec<u8> {
        self.answers_of(self.plan.lines.len())
    }

    /// The answers to its first `k` lines.
    pub fn answers_of(&self, k: usize) -> Vec<u8> {
        self.plan.lines[..k].iter().flat_map(|l| [b"ok ".as_slice(), l.as_slice()].concat()).collect()
    }

    /// The line it tampers with: its last.
    fn tamper_at(&self) -> Option<usize> {
        (self.plan.brk == Break::Tamper).then(|| self.plan.lines.len().saturating_sub(1))
    }

    fn raw(tcp: &mut Tcp, net: &mut Net, ep: usize, bytes: &[u8]) {
        tcp.eps[ep].write_bytes(bytes);
        tcp.output(net, ep);
    }

    /// The connection is up: what the client says first.
    pub fn start(&mut self, tcp: &mut Tcp, net: &mut Net) {
        if self.started {
            return;
        }
        self.started = true;
        match self.plan.brk {
            Break::Garbage => Self::raw(tcp, net, self.ep, b"GET / HTTP/1.1\r\nHost: nos\r\n\r\n"),
            Break::Silent => {}
            _ => {
                self.turns(tcp, net);
                if self.plan.brk == Break::Cut {
                    tcp.close(net, self.ep);
                }
            }
        }
    }

    /// What the server sent since the last call, taken in and answered.
    pub fn pump(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let Some(data) = tcp.eps[self.ep].data.as_ref() else { return };
        if data.rx.len() > self.fed {
            self.incoming.extend_from_slice(&data.rx[self.fed..]);
            self.fed = data.rx.len();
        }
        let unread = tcp.eps[self.ep].unread;
        if unread != 0 {
            tcp.read(net, self.ep, unread);
        }
        if self.ending.is_none() && self.started && self.plan.brk != Break::Cut {
            self.turns(tcp, net);
        }
    }

    fn turns(&mut self, tcp: &mut Tcp, net: &mut Net) {
        let want = self.answers().len();
        /* How much has to have come back before each line is said */
        let before: Vec<usize> = (0..self.plan.lines.len()).map(|k| self.answers_of(k).len()).collect();
        let tamper_at = self.tamper_at();
        let Some(conn) = self.conn.as_mut() else { return };
        for _ in 0..TURNS {
            let UnbufferedStatus { mut discard, state } = conn.process_tls_records(&mut self.incoming);
            let mut stop = false;
            match state {
                Err(e) => {
                    self.ending = Some(Ending::Error(format!("{:?}", e)));
                    stop = true;
                }
                Ok(ConnectionState::ReadTraffic(mut traffic)) => {
                    while let Some(record) = traffic.next_record() {
                        match record {
                            Ok(record) => {
                                discard += record.discard;
                                self.got.extend_from_slice(record.payload);
                            }
                            Err(e) => {
                                self.ending = Some(Ending::Error(format!("{:?}", e)));
                                stop = true;
                                break;
                            }
                        }
                    }
                }
                Ok(ConnectionState::EncodeTlsData(mut encoder)) => {
                    let mut buf = vec![0u8; 4 * RECORD];
                    match encoder.encode(&mut buf) {
                        Ok(n) => Self::raw(tcp, net, self.ep, &buf[..n]),
                        Err(e) => {
                            self.ending = Some(Ending::Error(format!("{:?}", e)));
                            stop = true;
                        }
                    }
                }
                Ok(ConnectionState::TransmitTlsData(transmit)) => transmit.done(),
                Ok(ConnectionState::WriteTraffic(mut writer)) => {
                    self.up = true;
                    if self.lines_sent < self.plan.lines.len() && self.got.len() >= before[self.lines_sent] {
                        let line = self.plan.lines[self.lines_sent].clone();
                        let mut buf = vec![0u8; line.len() + 512];
                        match writer.encrypt(&line, &mut buf) {
                            Ok(n) => {
                                if tamper_at == Some(self.lines_sent) && !self.tampered {
                                    self.tampered = true;
                                    buf[n - 1] ^= 0x01;
                                }
                                Self::raw(tcp, net, self.ep, &buf[..n]);
                                self.lines_sent += 1;
                            }
                            Err(e) => {
                                self.ending = Some(Ending::Error(format!("{:?}", e)));
                                stop = true;
                            }
                        }
                    } else if !self.said_bye && self.lines_sent == self.plan.lines.len() && self.got.len() >= want {
                        /* Every answer back: goodbye */
                        self.said_bye = true;
                        let mut buf = vec![0u8; 512];
                        if let Ok(n) = writer.queue_close_notify(&mut buf) {
                            Self::raw(tcp, net, self.ep, &buf[..n]);
                        }
                        tcp.close(net, self.ep);
                    } else {
                        stop = true;
                    }
                }
                Ok(ConnectionState::PeerClosed) | Ok(ConnectionState::Closed) => {
                    self.ending = Some(Ending::Closed);
                    stop = true;
                }
                Ok(_) => stop = true,
            }
            if discard != 0 {
                self.incoming.drain(..discard);
            }
            if stop {
                break;
            }
        }
    }
}
