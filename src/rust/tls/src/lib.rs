#![no_std]

//! A TLS client for the kernel's HTTP client.
//!
//! rustls drives the protocol; the socket stays on the C++ side. Because the
//! kernel has no `std`, this uses rustls' unbuffered API: `process_tls_records`
//! hands back one state at a time, and the loop here does the reading, the
//! writing and the buffering that `std::io` would otherwise do.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use kcore::trace;
use rustls::client::UnbufferedClientConnection;
use rustls::pki_types::{ServerName, UnixTime};
use rustls::time_provider::TimeProvider;
use rustls::unbuffered::{ConnectionState, UnbufferedStatus};
use rustls::{ClientConfig, RootCertStore};

/* Every RustCrypto crate that needs randomness goes through getrandom, which
   has no backend for a freestanding target. Point it at the kernel's entropy
   source (virtio-rng / RDRAND), the same pool the `random` command uses. */
fn kernel_getrandom(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    if kcore::random::fill_random(buf) {
        Ok(())
    } else {
        Err(getrandom::Error::UNSUPPORTED)
    }
}

getrandom::register_custom_getrandom!(kernel_getrandom);

/* Enough for a TLS record (16 KB payload plus header and tag) so a record
   never straddles two reads more than once. */
const TLS_BUF_SIZE: usize = 18 * 1024;
/* Reading has to stop somewhere if the peer goes quiet mid-handshake; the
   HTTP client's own idle timeout covers the body. */
const IO_TIMEOUT_MS: u64 = 15_000;
/* A handshake is a few round trips; anything past this is a peer that keeps
   us busy without making progress. */
const MAX_HANDSHAKE_STEPS: u32 = 128;

/* Certificate validity is checked against the wall clock, so a machine with
   a wrong RTC rejects every certificate -- which is the safe direction. */
#[derive(Debug)]
struct KernelTime;

impl TimeProvider for KernelTime {
    fn current_time(&self) -> Option<UnixTime> {
        let secs = kcore::time::wall_clock_secs();
        if secs == 0 {
            return None;
        }
        Some(UnixTime::since_unix_epoch(Duration::from_secs(secs)))
    }
}

/// What one turn of the state machine did.
enum Step {
    /// Made progress; go around again.
    Progress,
    /// Blocked until more TLS bytes arrive from the peer.
    NeedRead,
    /// Handshake finished, application data can flow.
    Ready,
    /// Plaintext was appended to `plaintext`.
    Data,
    /// The peer, or we, closed the connection.
    Closed,
}

/// What a session runs over: a byte stream somebody else opened, and closes.
///
/// A trait rather than a TCP type, because this crate sits *under* the
/// network layer -- the HTTP client there speaks TLS through it -- and so
/// cannot name a connection of that layer. The layer hands its connection in
/// as one of these.
pub trait Transport {
    /// Sends the whole buffer; false if the connection failed part way.
    fn send_all(&mut self, buf: &[u8]) -> bool;

    /// Bytes read, 0 at the end of the stream, negative on an error or when
    /// `timeout_ms` went by with nothing received.
    fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> isize;
}

pub struct TlsStream<T: Transport> {
    conn: UnbufferedClientConnection,
    sock: T,
    /* TLS bytes read from the socket and not yet processed */
    incoming: Vec<u8>,
    incoming_used: usize,
    /* scratch for TLS bytes on their way out */
    outgoing: Vec<u8>,
    /* decrypted application data waiting for the reader */
    plaintext: Vec<u8>,
    plaintext_pos: usize,
    /* the peer sent close_notify, or the socket hit EOF */
    closed: bool,
    /* the connection failed and cannot be used again */
    failed: bool,
}

impl<T: Transport> TlsStream<T> {
    /// A session over a connection somebody else opened, and closes: `host`
    /// is who the certificate has to be for. None when the handshake is
    /// refused -- the certificate, the name, or no protocol in common.
    pub fn connect(sock: T, host: &str) -> Option<Self> {
        let mut stream = Self::new(sock, host)?;
        if stream.handshake() { Some(stream) } else { None }
    }

    fn new(sock: T, host: &str) -> Option<Self> {
        let server_name = match ServerName::try_from(host) {
            Ok(name) => name.to_owned(),
            Err(e) => {
                trace!(0, "tls: bad server name {}: {:?}", host, e);
                return None;
            }
        };

        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let provider = Arc::new(rustls_rustcrypto::provider());
        let builder = match ClientConfig::builder_with_details(provider, Arc::new(KernelTime))
            .with_safe_default_protocol_versions()
        {
            Ok(builder) => builder,
            Err(e) => {
                trace!(0, "tls: config: {:?}", e);
                return None;
            }
        };
        let config = builder
            .with_root_certificates(roots)
            .with_no_client_auth();

        let conn = match UnbufferedClientConnection::new(Arc::new(config), server_name) {
            Ok(conn) => conn,
            Err(e) => {
                trace!(0, "tls: connection: {:?}", e);
                return None;
            }
        };

        Some(Self {
            conn,
            sock,
            incoming: vec![0u8; TLS_BUF_SIZE],
            incoming_used: 0,
            outgoing: vec![0u8; TLS_BUF_SIZE],
            plaintext: Vec::new(),
            plaintext_pos: 0,
            closed: false,
            failed: false,
        })
    }

    /* Reads one socketful of TLS bytes into `incoming`. False on EOF or
       error, which ends the connection either way. */
    fn fill(&mut self) -> bool {
        if self.incoming_used == self.incoming.len() {
            /* A record larger than the buffer: the peer is not speaking TLS
               the way we can follow. */
            return false;
        }

        let got = self
            .sock
            .recv(&mut self.incoming[self.incoming_used..], IO_TIMEOUT_MS);
        if got <= 0 {
            trace!(0, "tls: receive ended: {}", got);
            return false;
        }

        self.incoming_used += got as usize;
        true
    }

    /* One turn of the state machine. `app_out`, if any, is application data
       waiting to be encrypted; it is taken once it has been queued. */
    fn step(&mut self, app_out: &mut Option<&[u8]>) -> Option<Step> {
        let Self {
            conn,
            sock,
            incoming,
            incoming_used,
            outgoing,
            plaintext,
            ..
        } = self;

        let UnbufferedStatus { mut discard, state } =
            conn.process_tls_records(&mut incoming[..*incoming_used]);

        let state = match state {
            Ok(state) => state,
            Err(e) => {
                trace!(0, "tls: {:?}", e);
                return None;
            }
        };

        let step = match state {
            ConnectionState::ReadTraffic(mut traffic) => {
                let mut got = false;
                while let Some(record) = traffic.next_record() {
                    let record = match record {
                        Ok(record) => record,
                        Err(e) => {
                            trace!(0, "tls: record: {:?}", e);
                            return None;
                        }
                    };
                    discard += record.discard;
                    plaintext.extend_from_slice(record.payload);
                    got = true;
                }
                if got {
                    Step::Data
                } else {
                    Step::Progress
                }
            }
            ConnectionState::EncodeTlsData(mut encoder) => match encoder.encode(outgoing) {
                Ok(len) => {
                    if !sock.send_all(&outgoing[..len]) {
                        trace!(0, "tls: send of {} bytes failed", len);
                        return None;
                    }
                    Step::Progress
                }
                /* The buffer is sized for a TLS record; a handshake message
                   that does not fit is not something we can send. */
                Err(e) => {
                    trace!(0, "tls: encode: {:?}", e);
                    return None;
                }
            },
            ConnectionState::TransmitTlsData(transmit) => {
                transmit.done();
                Step::Progress
            }
            /* Nothing more to do until the peer says more. The read itself
               happens once this borrow of `incoming` has ended -- calling
               process_tls_records again to find out would process the next
               record a second time, which is a protocol error one round
               trip later. */
            ConnectionState::BlockedHandshake => Step::NeedRead,
            ConnectionState::WriteTraffic(mut writer) => {
                if let Some(data) = app_out.take() {
                    match writer.encrypt(data, outgoing) {
                        Ok(len) => {
                            if !sock.send_all(&outgoing[..len]) {
                                trace!(0, "tls: send of {} bytes failed", len);
                                return None;
                            }
                        }
                        Err(e) => {
                            trace!(0, "tls: encrypt: {:?}", e);
                            return None;
                        }
                    }
                }
                Step::Ready
            }
            ConnectionState::Closed => Step::Closed,
            _ => Step::Progress,
        };

        if discard != 0 {
            incoming.copy_within(discard..*incoming_used, 0);
            *incoming_used -= discard;
        }

        Some(step)
    }

    fn handshake(&mut self) -> bool {
        for _ in 0..MAX_HANDSHAKE_STEPS {
            let mut nothing = None;
            match self.step(&mut nothing) {
                Some(Step::Ready) => return true,
                Some(Step::NeedRead) => {
                    if !self.fill() {
                        self.failed = true;
                        return false;
                    }
                }
                Some(Step::Closed) | None => {
                    self.failed = true;
                    return false;
                }
                Some(_) => {}
            }
        }

        trace!(0, "tls: handshake unfinished after {} steps", MAX_HANDSHAKE_STEPS);
        self.failed = true;
        false
    }

    /// Encrypts and sends the whole buffer: its length, or -1.
    pub fn send(&mut self, data: &[u8]) -> isize {
        if self.failed {
            return -1;
        }

        let mut pending = Some(data);
        for _ in 0..MAX_HANDSHAKE_STEPS {
            match self.step(&mut pending) {
                Some(Step::Ready) => {
                    if pending.is_none() {
                        return data.len() as isize;
                    }
                }
                Some(Step::NeedRead) => {
                    if !self.fill() {
                        self.failed = true;
                        return -1;
                    }
                }
                Some(Step::Closed) | None => {
                    self.failed = true;
                    return -1;
                }
                Some(_) => {}
            }
        }

        self.failed = true;
        -1
    }

    /// Decrypted data: the byte count, 0 at the end of the stream, -1 on
    /// error.
    pub fn recv(&mut self, buf: &mut [u8]) -> isize {
        loop {
            /* Hand back what was already decrypted first. */
            if self.plaintext_pos < self.plaintext.len() {
                let avail = self.plaintext.len() - self.plaintext_pos;
                let take = if buf.len() < avail { buf.len() } else { avail };
                buf[..take]
                    .copy_from_slice(&self.plaintext[self.plaintext_pos..self.plaintext_pos + take]);
                self.plaintext_pos += take;
                if self.plaintext_pos == self.plaintext.len() {
                    self.plaintext.clear();
                    self.plaintext_pos = 0;
                }
                return take as isize;
            }

            if self.closed {
                return 0;
            }
            if self.failed {
                return -1;
            }

            let mut nothing = None;
            match self.step(&mut nothing) {
                Some(Step::Data) => {}
                /* Ready means the session is up with nothing decrypted
                   pending: like NeedRead, the next thing to do is listen. */
                Some(Step::NeedRead) | Some(Step::Ready) => {
                    if !self.fill() {
                        /* EOF without close_notify is how many servers end
                           a Connection: close response. The caller sees a
                           clean end of stream, and the HTTP framing above
                           decides whether that was short. */
                        self.closed = true;
                    }
                }
                Some(Step::Closed) => {
                    self.closed = true;
                    return 0;
                }
                None => {
                    self.failed = true;
                    return -1;
                }
                Some(Step::Progress) => {}
            }
        }
    }

    fn close(&mut self) {
        if self.failed {
            return;
        }

        let Self {
            conn,
            sock,
            incoming,
            incoming_used,
            outgoing,
            ..
        } = self;

        let UnbufferedStatus { state, .. } =
            conn.process_tls_records(&mut incoming[..*incoming_used]);
        if let Ok(ConnectionState::WriteTraffic(mut writer)) = state {
            if let Ok(len) = writer.queue_close_notify(outgoing) {
                sock.send_all(&outgoing[..len]);
            }
        }
    }
}

/// Sends close_notify. The TCP connection itself stays open, and is closed
/// by whoever opened it.
impl<T: Transport> Drop for TlsStream<T> {
    fn drop(&mut self) {
        self.close();
    }
}
