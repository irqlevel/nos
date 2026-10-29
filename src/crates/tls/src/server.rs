//! A TLS server for the host API: TLS 1.3 alone, and a client's certificate
//! required and taken by its key alone -- the SHA-256 of its
//! SubjectPublicKeyInfo, which has to be on the list the server was given.
//!
//! What is let in is decided in the handshake, before a byte of what the
//! client says is decrypted, let alone handed to whoever reads it: a client
//! whose key is not on the list, or that presents none, is told so with an
//! alert, and `accept` hands its socket back for the caller to reset. The
//! certificate's dates, its issuer and its chain are not looked at -- the key
//! is what the list names, as an `authorized_keys` does -- but the
//! handshake's signature is: the client has to hold the key it showed.
//!
//! rustls drives the protocol, through its unbuffered API, as the client in
//! lib.rs does; the loop here does the reading and writing, over a
//! [`Socket`] whose every wait is bounded. A read with nothing to read in
//! its time is not an error; what is encrypted waits in the session until a
//! `flush` sends it, for as long as the caller lets it -- so one client that
//! stops reading holds up nobody but itself.
//!
//! Compiled only with the `server` feature: the kernel's own `tls`, the HTTP
//! client's, is built without it.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use kcore::consts::NS_PER_MS;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls13_signature, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{NoServerSessionStorage, UnbufferedServerConnection};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, EncodeError, EncryptError, UnbufferedStatus};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme};
use sha2::{Digest, Sha256};

/// A client's key, as the server knows it: the SHA-256 of the
/// SubjectPublicKeyInfo of the certificate it presents.
pub type Fingerprint = [u8; 32];

/// The longest a handshake may take, by default: a client that has not
/// finished by then is let go.
pub const HANDSHAKE_MS: u64 = 10_000;

/// A TLS record and its header and tag: the most a read has to hold before
/// rustls can take any of it. A record larger is an error.
const RECORD_BUF: usize = 18 * 1024;
/// What one turn of a handshake makes, at most: a flight of records.
const FLIGHT: usize = 4 * RECORD_BUF;
/// How many turns of the state machine one call takes at most: rustls never
/// needs many, and a loop that does not end is a bug to report, not to be.
const MAX_TURNS: usize = 256;
/// The same for a handshake, where every read is a turn: its deadline is
/// what bounds it, and a client's flight a few bytes a segment -- a kilobyte
/// and more of them -- is still a handshake.
const HANDSHAKE_TURNS: usize = 8192;
/// How long the alert that refuses a client is given to go out.
const ALERT_MS: u64 = 1_000;

/// The key of the certificate `cert` (DER), as the server knows a client
/// by: None for what is not a certificate webpki can read.
pub fn fingerprint(cert: &[u8]) -> Option<Fingerprint> {
    let der = CertificateDer::from(cert);
    let parsed = webpki::EndEntityCert::try_from(&der).ok()?;
    Some(Sha256::digest(parsed.subject_public_key_info().as_ref()).into())
}

/// What a session runs over: a byte stream somebody else accepted, and
/// closes or resets. Every wait is bounded, and at least a millisecond.
pub trait Socket {
    /// What arrived within `wait_ms`.
    fn recv(&mut self, buf: &mut [u8], wait_ms: u64) -> Recv;

    /// The bytes taken -- 0 when there was room for none within `wait_ms` --
    /// or None once the connection has failed.
    fn send(&mut self, buf: &[u8], wait_ms: u64) -> Option<usize>;
}

/// What a socket's read came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recv {
    Data(usize),
    /// Nothing within the wait.
    Idle,
    /// The other end has closed its side.
    End,
    Failed,
}

/// Why a handshake did not let the client in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Not finished within its time.
    Timeout,
    /// The client closed before it was done.
    Closed,
    /// The socket failed.
    Io,
    /// Its key is not one the server lets in.
    UnknownKey,
    /// It presented no certificate.
    NoCertificate,
    /// It presented one nobody can read.
    BadCertificate,
    /// Nothing in common: not TLS 1.3, no cipher, group or protocol both
    /// have.
    Incompatible,
    /// It did not speak TLS as it should.
    Protocol,
    /// It sent an alert: it gave up on the server.
    ClientAlert,
    /// No memory for the session.
    NoMemory,
}

impl Refused {
    pub fn name(self) -> &'static str {
        match self {
            Refused::Timeout => "timeout",
            Refused::Closed => "closed",
            Refused::Io => "io",
            Refused::UnknownKey => "unknown key",
            Refused::NoCertificate => "no certificate",
            Refused::BadCertificate => "bad certificate",
            Refused::Incompatible => "incompatible",
            Refused::Protocol => "protocol",
            Refused::ClientAlert => "client alert",
            Refused::NoMemory => "no memory",
        }
    }

    fn of(e: &Error) -> Refused {
        match e {
            Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure) => Refused::UnknownKey,
            Error::InvalidCertificate(_) => Refused::BadCertificate,
            Error::NoCertificatesPresented => Refused::NoCertificate,
            Error::PeerIncompatible(_) | Error::NoApplicationProtocol => Refused::Incompatible,
            Error::AlertReceived(_) => Refused::ClientAlert,
            _ => Refused::Protocol,
        }
    }
}

/// Why a server could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The key is not one rustls can sign with, or not the certificate's.
    Key,
    /// No configuration for TLS 1.3 with the kernel's provider.
    Config,
}

/// The clients' keys a server lets in, and how a handshake's signature is
/// checked: the provider's algorithms, over the key the certificate has.
#[derive(Debug)]
struct Pinned {
    keys: Vec<Fingerprint>,
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for Pinned {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    /* No CA is named: the client presents the certificate of its key,
     * whoever issued it */
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    /* The key, and nothing else of the certificate: its dates and issuer
     * are not looked at -- the list names the key, as an authorized_keys
     * does. ApplicationVerificationFailure is this check's own refusal,
     * which `Refused::of` tells from the rest. */
    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
                          _now: UnixTime) -> Result<ClientCertVerified, Error> {
        let Some(key) = fingerprint(end_entity) else {
            return Err(Error::InvalidCertificate(CertificateError::BadEncoding));
        };
        if self.keys.contains(&key) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure))
        }
    }

    /* The server speaks TLS 1.3 alone, so rustls never asks */
    fn verify_tls12_signature(&self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        Err(Error::InvalidCertificate(CertificateError::BadSignature))
    }

    /* That the client holds the key its certificate has: the signature
     * over the handshake, checked by webpki with the provider's algorithms */
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct)
                              -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// The server's clock. It judges no certificate's dates -- not its own,
/// not a client's -- so any time will do while the wall clock is not set.
#[derive(Debug)]
struct AnyTime;

impl TimeProvider for AnyTime {
    fn current_time(&self) -> Option<UnixTime> {
        let secs = kcore::time::wall_clock_secs().max(1);
        Some(UnixTime::since_unix_epoch(Duration::from_secs(secs)))
    }
}

/// What the server presents and whom it lets in: made once, shared by every
/// connection, and made again whole when the keys it lets in change -- a
/// connection keeps the one it was accepted with.
pub struct Server {
    config: Arc<ServerConfig>,
}

impl Server {
    /// A server with `cert` (DER) and its key `key` (PKCS#8 DER), letting in
    /// the clients whose keys are `clients`, speaking the protocols `alpn`
    /// (in the order it prefers them; none for no ALPN).
    pub fn new(cert: &[u8], key: &[u8], clients: &[Fingerprint], alpn: &[&[u8]]) -> Result<Server, SetupError> {
        let provider = Arc::new(rustls_rustcrypto::provider());
        let verifier = Arc::new(Pinned { keys: clients.to_vec(), algs: provider.signature_verification_algorithms });
        let mut config = ServerConfig::builder_with_details(provider, Arc::new(AnyTime))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| SetupError::Config)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![CertificateDer::from(cert.to_vec())],
                              PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())))
            .map_err(|_| SetupError::Key)?;
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        /* No resumption: a client is a program that connects for long, and
         * every connection shows its key afresh */
        config.session_storage = Arc::new(NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
        Ok(Server { config: Arc::new(config) })
    }

    /// A session with whoever connected on `sock`, its handshake given
    /// `within_ms`: the session, or the socket back and why -- to be reset,
    /// the client told so already where it could be.
    pub fn accept<S: Socket>(&self, sock: S, within_ms: u64) -> Result<ServerStream<S>, (S, Refused)> {
        let conn = match UnbufferedServerConnection::new(self.config.clone()) {
            Ok(conn) => conn,
            Err(_) => return Err((sock, Refused::NoMemory)),
        };
        let mut incoming = Vec::new();
        if incoming.try_reserve_exact(RECORD_BUF).is_err() {
            return Err((sock, Refused::NoMemory));
        }
        incoming.resize(RECORD_BUF, 0);
        let mut stream = ServerStream {
            conn,
            sock,
            incoming,
            used: 0,
            pending: Vec::new(),
            sent: 0,
            plain: Vec::new(),
            plain_pos: 0,
            client: [0; 32],
            closed: false,
            failed: false,
        };
        match stream.handshake(within_ms) {
            Ok(()) => Ok(stream),
            Err(why) => Err((stream.sock, why)),
        }
    }
}

/// What a session's read came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// This many bytes of what the client said.
    Data(usize),
    /// Nothing within the wait: not an error.
    Idle,
    /// The client is done: its close_notify, or its end of the stream
    /// between two records.
    End,
    /// The session failed, and is over.
    Failed,
}

/// What a flush came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flush {
    /// Everything that waited has gone.
    Done,
    /// Some still waits: the client is not taking it as fast.
    More,
    Failed,
}

/// What one turn of the state machine did.
enum Turn {
    Progress,
    /// Nothing more until the client says more.
    NeedRead,
    /// The session is up, with nothing of the client's left to process.
    Ready,
    Data,
    /// The client's close_notify: it says no more, and the session can
    /// still be written to and closed (TLS 1.3, RFC 8446 6.1).
    PeerClosed,
    /// Both ends have closed.
    Closed,
    Failed(Error),
    /// No memory to keep what the turn made.
    NoMemory,
}

/// A session, from the handshake on. Dropped, it drops its socket and sends
/// nothing; `close` ends it with a close_notify, and `into_socket` hands the
/// socket back for a reset.
pub struct ServerStream<S: Socket> {
    conn: UnbufferedServerConnection,
    sock: S,
    /// What the client sent that rustls has not taken yet
    incoming: Vec<u8>,
    used: usize,
    /// Encrypted, waiting to go out: from `sent` on
    pending: Vec<u8>,
    sent: usize,
    /// What the client said, decrypted, not yet read: from `plain_pos` on
    plain: Vec<u8>,
    plain_pos: usize,
    client: Fingerprint,
    closed: bool,
    failed: bool,
}

/// `len` bytes more at the end of `buf`, zeroed for `fill` to overwrite:
/// false with no memory for them.
fn grow(buf: &mut Vec<u8>, len: usize) -> bool {
    if buf.try_reserve(len).is_err() {
        return false;
    }
    buf.resize(buf.len() + len, 0);
    true
}

/// What `fill` writes, appended to `buf`: room made for `guess`, and for as
/// much as `fill` says it needs if that was not enough.
fn append_with<E>(buf: &mut Vec<u8>, guess: usize, mut fill: impl FnMut(&mut [u8]) -> Result<usize, E>,
                  needed: impl Fn(&E) -> Option<usize>) -> Result<bool, E> {
    let start = buf.len();
    if !grow(buf, guess) {
        return Ok(false);
    }
    match fill(&mut buf[start..]) {
        Ok(n) => {
            buf.truncate(start + n);
            Ok(true)
        }
        Err(e) => {
            buf.truncate(start);
            let Some(size) = needed(&e) else { return Err(e) };
            if !grow(buf, size) {
                return Ok(false);
            }
            match fill(&mut buf[start..]) {
                Ok(n) => {
                    buf.truncate(start + n);
                    Ok(true)
                }
                Err(e) => {
                    buf.truncate(start);
                    Err(e)
                }
            }
        }
    }
}

fn encode_size(e: &EncodeError) -> Option<usize> {
    match e {
        EncodeError::InsufficientSize(s) => Some(s.required_size),
        EncodeError::AlreadyEncoded => None,
    }
}

fn encrypt_size(e: &EncryptError) -> Option<usize> {
    match e {
        EncryptError::InsufficientSize(s) => Some(s.required_size),
        EncryptError::EncryptExhausted => None,
    }
}

/// The boot clock past `ms` from now.
fn deadline(ms: u64) -> u64 {
    kcore::time::boot_time_ns().saturating_add(ms.saturating_mul(NS_PER_MS))
}

/// What is left of the time to `deadline`, in whole milliseconds -- at least
/// one while any is left.
fn left_ms(deadline: u64) -> u64 {
    let left = deadline.saturating_sub(kcore::time::boot_time_ns());
    if left == 0 { 0 } else { left.div_ceil(NS_PER_MS) }
}

impl<S: Socket> ServerStream<S> {
    /// The key the client showed, which is one the server lets in.
    pub fn client(&self) -> &Fingerprint {
        &self.client
    }

    /// The protocol the two agreed on by ALPN, if any.
    pub fn alpn(&self) -> Option<&[u8]> {
        self.conn.alpn_protocol()
    }

    /// The socket, for what only it knows: who is at the other end.
    pub fn socket(&self) -> &S {
        &self.sock
    }

    /// How many encrypted bytes wait to be sent.
    pub fn pending(&self) -> usize {
        self.pending.len() - self.sent
    }

    /// One turn of the state machine: `app_out`, when given, encrypted once
    /// the session can, and taken.
    fn turn(&mut self, app_out: &mut Option<&[u8]>) -> Turn {
        let Self { conn, incoming, used, pending, plain, .. } = self;
        let UnbufferedStatus { mut discard, state } = conn.process_tls_records(&mut incoming[..*used]);
        let turn = match state {
            Err(e) => Turn::Failed(e),
            Ok(ConnectionState::ReadTraffic(mut traffic)) => {
                let mut turn = Turn::Data;
                while let Some(record) = traffic.next_record() {
                    match record {
                        Ok(record) => {
                            discard += record.discard;
                            if plain.try_reserve(record.payload.len()).is_err() {
                                turn = Turn::NoMemory;
                                break;
                            }
                            plain.extend_from_slice(record.payload);
                        }
                        Err(e) => {
                            turn = Turn::Failed(e);
                            break;
                        }
                    }
                }
                turn
            }
            Ok(ConnectionState::EncodeTlsData(mut encoder)) => {
                match append_with(pending, FLIGHT, |buf| encoder.encode(buf), encode_size) {
                    Ok(true) => Turn::Progress,
                    Ok(false) => Turn::NoMemory,
                    Err(_) => Turn::Failed(Error::EncryptError),
                }
            }
            /* What EncodeTlsData made is in `pending`, which goes out in
             * order with whatever follows it: the promise rustls asks for */
            Ok(ConnectionState::TransmitTlsData(transmit)) => {
                transmit.done();
                Turn::Progress
            }
            Ok(ConnectionState::BlockedHandshake) => Turn::NeedRead,
            Ok(ConnectionState::WriteTraffic(mut writer)) => match app_out.take() {
                Some(data) => {
                    let guess = data.len() + (data.len() / (16 * 1024) + 1) * 64;
                    match append_with(pending, guess, |buf| writer.encrypt(data, buf), encrypt_size) {
                        Ok(true) => Turn::Ready,
                        Ok(false) => Turn::NoMemory,
                        Err(_) => Turn::Failed(Error::EncryptError),
                    }
                }
                None => Turn::Ready,
            },
            Ok(ConnectionState::PeerClosed) => Turn::PeerClosed,
            Ok(ConnectionState::Closed) => Turn::Closed,
            Ok(_) => Turn::Progress,
        };
        if discard != 0 {
            incoming.copy_within(discard..*used, 0);
            *used -= discard;
        }
        turn
    }

    /// One read of the socket into `incoming`, for at most `wait_ms`. A
    /// record larger than the buffer is not TLS this server can follow.
    fn fill(&mut self, wait_ms: u64) -> Recv {
        if self.used == self.incoming.len() {
            return Recv::Failed;
        }
        match self.sock.recv(&mut self.incoming[self.used..], wait_ms.max(1)) {
            Recv::Data(n) => {
                self.used += n.min(self.incoming.len() - self.used);
                Recv::Data(n)
            }
            other => other,
        }
    }

    /// Sends what waits until `deadline`: true once all of it has gone.
    fn send_until(&mut self, deadline: u64) -> Option<bool> {
        while self.sent < self.pending.len() {
            let wait = left_ms(deadline);
            if wait == 0 {
                return Some(false);
            }
            let n = self.sock.send(&self.pending[self.sent..], wait)?;
            self.sent += n.min(self.pending.len() - self.sent);
        }
        self.pending.clear();
        self.sent = 0;
        Some(true)
    }

    /// The handshake, to the end or to its refusal, within `within_ms`.
    fn handshake(&mut self, within_ms: u64) -> Result<(), Refused> {
        let until = deadline(within_ms);
        for _ in 0..HANDSHAKE_TURNS {
            /* What a turn made goes out before the next read: the client
             * waits for it */
            if self.pending() != 0 {
                match self.send_until(until) {
                    Some(true) => {}
                    Some(false) => return Err(Refused::Timeout),
                    None => return Err(Refused::Io),
                }
            }
            match self.turn(&mut None) {
                Turn::Ready => return self.admitted(),
                Turn::NeedRead => {
                    let wait = left_ms(until);
                    if wait == 0 {
                        return Err(Refused::Timeout);
                    }
                    match self.fill(wait) {
                        Recv::Data(_) | Recv::Idle => {}
                        Recv::End => return Err(Refused::Closed),
                        Recv::Failed => return Err(Refused::Io),
                    }
                }
                Turn::Progress | Turn::Data => {}
                /* A client with nothing to say sends its Finished and its
                 * close_notify in one flight, and rustls says it closed
                 * before it says the session is up: a handshake done, a
                 * session let in and already over */
                Turn::PeerClosed | Turn::Closed if !self.conn.is_handshaking() => {
                    self.closed = true;
                    return self.admitted();
                }
                Turn::PeerClosed | Turn::Closed => return Err(Refused::Closed),
                Turn::NoMemory => return Err(Refused::NoMemory),
                Turn::Failed(e) => {
                    self.send_alert();
                    return Err(Refused::of(&e));
                }
            }
        }
        Err(Refused::Protocol)
    }

    /// The handshake is done: the key the client showed -- which the
    /// verifier let in, and whose signature it checked -- kept for the
    /// caller. A session up without one would be a client nobody checked:
    /// refused rather than trusted.
    fn admitted(&mut self) -> Result<(), Refused> {
        let key = self.conn.peer_certificates().and_then(|certs| certs.first()).and_then(|c| fingerprint(c));
        match key {
            Some(key) => {
                self.client = key;
                Ok(())
            }
            None => Err(Refused::NoCertificate),
        }
    }

    /// The alert rustls queued with its refusal, sent if it can be within
    /// `ALERT_MS`: what tells the client why, before its reset.
    ///
    /// One more turn, and no other: what rustls has queued it hands out
    /// before it parses anything, so that turn takes the alert -- but a
    /// record it could not deframe is not discarded with the error, and a
    /// turn after it would deframe that record again and refuse it with a
    /// second fatal alert, which rustls asserts it never sends. (Nor can the
    /// turn be given nothing to parse: rustls keeps ranges into the bytes it
    /// was handed, and every call must hand it the same.)
    fn send_alert(&mut self) {
        let Self { conn, incoming, used, pending, .. } = self;
        let UnbufferedStatus { state, .. } = conn.process_tls_records(&mut incoming[..*used]);
        if let Ok(ConnectionState::EncodeTlsData(mut encoder)) = state {
            if matches!(append_with(pending, FLIGHT, |buf| encoder.encode(buf), encode_size), Ok(true)) {
                let _ = self.send_until(deadline(ALERT_MS));
            }
        }
    }

    /// What the client said, for at most `wait_ms`: Idle when it said
    /// nothing in that time.
    pub fn read(&mut self, buf: &mut [u8], wait_ms: u64) -> Read {
        let until = deadline(wait_ms);
        for _ in 0..MAX_TURNS {
            if self.plain_pos < self.plain.len() {
                let take = buf.len().min(self.plain.len() - self.plain_pos);
                buf[..take].copy_from_slice(&self.plain[self.plain_pos..self.plain_pos + take]);
                self.plain_pos += take;
                if self.plain_pos == self.plain.len() {
                    self.plain.clear();
                    self.plain_pos = 0;
                }
                return Read::Data(take);
            }
            if self.closed {
                return Read::End;
            }
            if self.failed {
                return Read::Failed;
            }
            match self.turn(&mut None) {
                Turn::Data | Turn::Progress => {}
                Turn::Ready | Turn::NeedRead => {
                    let wait = left_ms(until);
                    if wait == 0 {
                        return Read::Idle;
                    }
                    match self.fill(wait) {
                        Recv::Data(_) => {}
                        Recv::Idle => return Read::Idle,
                        /* The end of the stream between two records is the
                         * client gone; inside one, the stream was cut */
                        Recv::End if self.used == 0 => self.closed = true,
                        Recv::End | Recv::Failed => self.failed = true,
                    }
                }
                Turn::PeerClosed | Turn::Closed => self.closed = true,
                Turn::Failed(_) | Turn::NoMemory => self.failed = true,
            }
        }
        self.failed = true;
        Read::Failed
    }

    /// `data`, encrypted, put after what waits to be sent -- unless that
    /// would leave more than `max_pending` waiting: false then, nothing
    /// taken, and the caller flushes first. False for a session that has
    /// failed too. A `data` longer than `max_pending` never fits: it goes in
    /// pieces. A client that has said its close_notify is still written to:
    /// it closed its side, not the server's -- an answer to what it said
    /// before goes out, and the server's own goodbye after.
    pub fn write(&mut self, data: &[u8], max_pending: usize) -> bool {
        if self.failed {
            return false;
        }
        if self.pending().saturating_add(data.len()) > max_pending {
            return false;
        }
        if data.is_empty() {
            return true;
        }
        let mut out = Some(data);
        for _ in 0..MAX_TURNS {
            match self.turn(&mut out) {
                Turn::Ready if out.is_none() => return true,
                Turn::Ready | Turn::Progress | Turn::Data => {}
                /* What it said before is in `plain`, still to be read; from
                 * here on it says nothing more */
                Turn::PeerClosed => self.closed = true,
                Turn::NeedRead | Turn::Closed | Turn::Failed(_) | Turn::NoMemory => break,
            }
        }
        self.failed = true;
        false
    }

    /// What waits sent, for at most `wait_ms`.
    pub fn flush(&mut self, wait_ms: u64) -> Flush {
        if self.failed {
            return Flush::Failed;
        }
        match self.send_until(deadline(wait_ms)) {
            Some(true) => Flush::Done,
            Some(false) => {
                /* What has gone, given back */
                if self.sent > self.pending.len() / 2 {
                    self.pending.drain(..self.sent);
                    self.sent = 0;
                }
                Flush::More
            }
            None => {
                self.failed = true;
                Flush::Failed
            }
        }
    }

    /// A close_notify after what waits, and then the client's end of the
    /// stream, each waited for within `wait_ms`; the socket back, and
    /// whether both happened. True: close it -- the client closed first, so
    /// the slot left in TIME-WAIT is the client's. False: reset it. A
    /// server that closed on a close_notify alone would send its FIN before
    /// the client's arrived, and both ends would keep TIME-WAIT.
    pub fn close(mut self, wait_ms: u64) -> (S, bool) {
        if self.failed {
            return (self.sock, false);
        }
        let until = deadline(wait_ms);
        let mut queued = false;
        for _ in 0..MAX_TURNS {
            let Self { conn, incoming, used, pending, .. } = &mut self;
            let UnbufferedStatus { mut discard, state } = conn.process_tls_records(&mut incoming[..*used]);
            let done = match state {
                Ok(ConnectionState::WriteTraffic(mut writer)) => {
                    queued = matches!(append_with(pending, 64, |buf| writer.queue_close_notify(buf), encrypt_size),
                                      Ok(true));
                    true
                }
                /* What the client said meanwhile is not read any more */
                Ok(ConnectionState::ReadTraffic(mut traffic)) => {
                    while let Some(Ok(record)) = traffic.next_record() {
                        discard += record.discard;
                    }
                    false
                }
                Ok(ConnectionState::EncodeTlsData(mut encoder)) => {
                    !matches!(append_with(pending, FLIGHT, |buf| encoder.encode(buf), encode_size), Ok(true))
                }
                Ok(ConnectionState::TransmitTlsData(transmit)) => {
                    transmit.done();
                    false
                }
                _ => true,
            };
            if discard != 0 {
                incoming.copy_within(discard..*used, 0);
                *used -= discard;
            }
            if done {
                break;
            }
        }
        if !queued || self.send_until(until) != Some(true) {
            return (self.sock, false);
        }
        /* The client's FIN: whatever it sends before it is not read */
        let mut scratch = [0u8; 256];
        loop {
            let wait = left_ms(until);
            if wait == 0 {
                return (self.sock, false);
            }
            match self.sock.recv(&mut scratch, wait) {
                Recv::End => return (self.sock, true),
                Recv::Data(_) | Recv::Idle => {}
                Recv::Failed => return (self.sock, false),
            }
        }
    }

    /// The socket back, nothing more said: for a reset.
    pub fn into_socket(self) -> S {
        self.sock
    }
}
