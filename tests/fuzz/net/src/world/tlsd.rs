//! The world's TLS server, for its HTTP server to speak through: the same
//! rustls the kernel's client is built on, its unbuffered API driven by hand
//! as the kernel's `tls` crate drives the client's -- over the world's TCP,
//! presenting the certificate the input chooses (certs/make.sh made them),
//! in the protocol versions it chooses.

use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::UnbufferedServerConnection;
use rustls::unbuffered::{ConnectionState, UnbufferedStatus};
use rustls::ServerConfig;

use crate::machine::sched;

/// Which certificate the server presents, at 10.0.2.100.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cert {
    /// For its address, from the CA the client trusts.
    Good,
    /// The same, run out in 2021.
    Expired,
    /// From that CA, for 10.0.2.101.
    Other,
    /// For its address, and from nobody.
    Stranger,
}

impl Cert {
    fn der(self) -> (&'static [u8], &'static [u8]) {
        match self {
            Cert::Good => (include_bytes!("../../certs/good.der"), include_bytes!("../../certs/good-key.der")),
            Cert::Expired => (include_bytes!("../../certs/expired.der"),
                              include_bytes!("../../certs/expired-key.der")),
            Cert::Other => (include_bytes!("../../certs/other.der"), include_bytes!("../../certs/other-key.der")),
            Cert::Stranger => (include_bytes!("../../certs/stranger.der"),
                               include_bytes!("../../certs/stranger-key.der")),
        }
    }
}

/// The server's clock: the machine's wall clock, as the fuzzer keeps it.
#[derive(Debug)]
struct Clock;

impl rustls::time_provider::TimeProvider for Clock {
    fn current_time(&self) -> Option<UnixTime> {
        Some(UnixTime::since_unix_epoch(Duration::from_secs(sched::wall_secs())))
    }
}

/// A server's configuration: its certificate, the versions it speaks (1.2,
/// 1.3, or both).
pub fn config(cert: Cert, tls12: bool, tls13: bool) -> Arc<ServerConfig> {
    let versions: Vec<&'static rustls::SupportedProtocolVersion> = [(tls12, &rustls::version::TLS12),
                                                                     (tls13, &rustls::version::TLS13)]
        .into_iter().filter(|v| v.0).map(|v| v.1).collect();
    let (chain, key) = cert.der();
    let provider = Arc::new(rustls_rustcrypto::provider());
    let config = ServerConfig::builder_with_details(provider, Arc::new(Clock))
        .with_protocol_versions(&versions)
        .expect("versions the provider has")
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(chain.to_vec())],
                          PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())))
        .expect("the fuzzer's own certificate and key");
    Arc::new(config)
}

/// How many turns of the state machine one call may take: rustls never
/// needs many, and a loop that does not end is the fuzzer's to report, not
/// to be.
const TURNS: usize = 256;
/// What one record comes to at most, and a margin for a flight of them.
const RECORD: usize = 16 * 1024 + 512;

/// One connection's session, from the server's side.
pub struct Session {
    conn: UnbufferedServerConnection,
    /// What the client sent that rustls has not taken yet.
    incoming: Vec<u8>,
    /// How much of what the connection received has been handed in.
    pub fed: usize,
    /// What the client said, decrypted.
    pub plain: Vec<u8>,
    /// The session is over: the client broke it, or it broke the client.
    pub failed: bool,
}

impl Session {
    pub fn new(config: Arc<ServerConfig>) -> Session {
        Session { conn: UnbufferedServerConnection::new(config).expect("a server connection"), incoming: Vec::new(),
                  fed: 0, plain: Vec::new(), failed: false }
    }

    /// What the connection received past what was handed in, into the
    /// session; what the server says back. `seal` is application data to
    /// encrypt once the session is up, and `close` a close_notify after it.
    fn turn(&mut self, rx: &[u8], mut seal: Option<&[u8]>, mut close: bool) -> Vec<u8> {
        if rx.len() > self.fed {
            self.incoming.extend_from_slice(&rx[self.fed..]);
            self.fed = rx.len();
        }
        let mut out = Vec::new();
        for _ in 0..TURNS {
            if self.failed {
                break;
            }
            let Session { conn, incoming, plain, failed, .. } = self;
            let UnbufferedStatus { mut discard, state } = conn.process_tls_records(incoming);
            let mut stop = false;
            match state {
                Err(_) => {
                    *failed = true;
                    stop = true;
                }
                Ok(ConnectionState::ReadTraffic(mut traffic)) => {
                    while let Some(record) = traffic.next_record() {
                        match record {
                            Ok(record) => {
                                discard += record.discard;
                                plain.extend_from_slice(record.payload);
                            }
                            Err(_) => {
                                *failed = true;
                                break;
                            }
                        }
                    }
                }
                Ok(ConnectionState::EncodeTlsData(mut encoder)) => {
                    let mut buf = vec![0u8; 4 * RECORD];
                    match encoder.encode(&mut buf) {
                        Ok(n) => out.extend_from_slice(&buf[..n]),
                        Err(_) => *failed = true,
                    }
                }
                Ok(ConnectionState::TransmitTlsData(transmit)) => transmit.done(),
                Ok(ConnectionState::WriteTraffic(mut writer)) => {
                    if let Some(data) = seal.take() {
                        let mut buf = vec![0u8; data.len() + (data.len() / (16 * 1024) + 1) * 512];
                        match writer.encrypt(data, &mut buf) {
                            Ok(n) => out.extend_from_slice(&buf[..n]),
                            Err(_) => *failed = true,
                        }
                    } else if close {
                        close = false;
                        let mut buf = vec![0u8; 512];
                        if let Ok(n) = writer.queue_close_notify(&mut buf) {
                            out.extend_from_slice(&buf[..n]);
                        }
                    } else {
                        stop = true;
                    }
                }
                /* Waiting for the client; or the client has said goodbye,
                 * which this server takes as the end of what it hears. */
                Ok(ConnectionState::BlockedHandshake) | Ok(ConnectionState::Closed) => stop = true,
                Ok(ConnectionState::PeerClosed) => {}
                Ok(_) => stop = true,
            }
            if discard != 0 {
                incoming.drain(..discard);
            }
            if stop {
                break;
            }
        }
        out
    }

    /// The client's new bytes in; what the server answers them with.
    pub fn feed(&mut self, rx: &[u8]) -> Vec<u8> {
        self.turn(rx, None, false)
    }

    /// `data`, encrypted -- or nothing, the session not being up.
    pub fn seal(&mut self, rx: &[u8], data: &[u8]) -> Vec<u8> {
        self.turn(rx, Some(data), false)
    }

    /// A close_notify.
    pub fn close_notify(&mut self, rx: &[u8]) -> Vec<u8> {
        self.turn(rx, None, true)
    }
}
