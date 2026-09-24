#![no_std]

//! An SSH server: the protocol, and nothing of the machine it runs on.
//!
//! What the machine gives -- the TCP connection, the random pool, the
//! clock, the shell, which keys may log in -- comes in through two traits,
//! `Link` and `Shell`, and the sshd module (src/rust/modules/sshd) is what
//! implements them. What is here is RFC 4253, 4252 and 4254 cut down to one
//! algorithm of each kind, the ones every OpenSSH of the last ten years
//! offers first or nearly:
//!
//! - key exchange: curve25519-sha256 (RFC 8731), and OpenSSH's strict key
//!   exchange, the fix for the Terrapin prefix truncation;
//! - host key and user keys: ssh-ed25519 (RFC 8709);
//! - cipher: chacha20-poly1305@openssh.com, an AEAD -- so no MAC;
//! - no compression.
//!
//! A connection runs from end to end in one call to `serve`, on the caller's
//! task: the version exchange, the key exchange, public key authentication,
//! then one session channel -- a shell behind a line editor, or a single
//! command -- until either side closes it. Rekeying is the client's to
//! start, and is followed wherever it comes, command output included.

extern crate alloc;

mod cipher;
mod connection;
pub mod keys;
mod terminal;
mod transport;
mod wire;

use core::fmt;

pub use connection::serve;
pub use keys::{AuthorizedKey, HostKey, PublicKey};

/// What a receive got.
pub enum Recv {
    /// This many bytes, at the front of the buffer.
    Data(usize),
    /// Nothing, before the wait ran out.
    Timeout,
    /// The connection is closed, or gone.
    Closed,
}

/// The connection underneath a session, and what else the session needs of
/// the machine.
pub trait Link {
    /// Bytes that have arrived, into `buf`, waiting `timeout_ms` at most for
    /// the first of them.
    fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> Recv;
    /// Sends the whole of `data`: false once the connection is gone -- or
    /// the server is stopping while the peer takes nothing.
    fn send(&mut self, data: &[u8]) -> bool;
    /// Milliseconds on a clock that only goes forward.
    fn now_ms(&self) -> u64;
    /// Whether the server is stopping: the session ends at its next wait.
    fn stopping(&self) -> bool;
    /// Fills `buf` from the machine's random pool; false when there is
    /// nothing to be had from it.
    fn random(&mut self, buf: &mut [u8]) -> bool;
}

/// Who may log in, and what a session does once someone has.
pub trait Shell {
    /// Whether `key` may log in as `user`.
    fn authorized(&mut self, user: &str, key: &PublicKey) -> bool;
    /// `user` has logged in, with `key`.
    fn logged_in(&mut self, _user: &str, _key: &PublicKey) {}
    /// Runs one command line, what it prints going to `io` as it prints it,
    /// and what the client types while it runs there for it to read. `exit`
    /// and `logout` never get here: the session ends on those.
    fn run(&mut self, line: &str, io: &mut dyn Io);
}

/// What a command a session runs has of the session, for as long as it runs.
pub trait Io {
    /// Output, on the channel as it comes -- line ends made CR LF for a
    /// terminal.
    fn write(&mut self, data: &[u8]);
    /// What the client has typed since the command began, or since the last
    /// read, up to `buf.len()` bytes -- waiting up to `timeout_ms` for some
    /// when there is none yet. Raw: with a terminal, the keys as they were
    /// pressed; and what a command does not read is the line editor's once
    /// it returns. `Some(0)` when the time passed with nothing; `None` when
    /// nothing more will come: the channel's EOF or close, or the session
    /// failing -- which the session then ends on.
    fn read(&mut self, buf: &mut [u8], timeout_ms: u64) -> Option<usize>;
    /// Tends the session for up to `timeout_ms`, for a command that runs on
    /// with nothing to write or read meanwhile: what the client sends is
    /// handled as it comes -- a keepalive answered, a window taken, typing
    /// kept for the command's next read or the line editor after it -- and
    /// a client gone quiet is asked whether it is still there, as between
    /// commands. A client asks too (`ServerAliveInterval`), and hangs up on
    /// a server that does not answer. False once the session is over: the
    /// channel closed, the client gone, the session failing -- which it
    /// then ends on.
    fn idle(&mut self, timeout_ms: u64) -> bool;
}

/// How a server behaves: the same for all its sessions.
pub struct Config<'a> {
    /// The software part of the version line, what follows "SSH-2.0-".
    pub software: &'a str,
    /// What a shell session is greeted with, before its first prompt.
    pub banner: &'a str,
    /// The shell's prompt.
    pub prompt: &'a str,
    /// From the connection to a login, at most: past this it is dropped.
    pub login_grace_ms: u64,
    /// Failed authentication attempts a connection gets.
    pub max_auth_tries: u32,
    /// Quiet this long, and the client is asked whether it is still there;
    pub keepalive_ms: u64,
    /// this many questions unanswered, and it is dropped.
    pub keepalive_max: u32,
}

/// Why a session ended, other than by its channel closing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The connection closed, or failed, under the session.
    Closed,
    /// The client said it was leaving (SSH_MSG_DISCONNECT).
    Disconnected,
    /// The server is stopping.
    Stopped,
    /// A wait that ran out: what for.
    Timeout(&'static str),
    /// A packet whose tag did not check out.
    Mac,
    /// Nothing the client offered, of this kind, is something the server
    /// does.
    NoAlgorithm(&'static str),
    /// The client broke the protocol: how.
    Protocol(&'static str),
    /// Too many failed authentication attempts.
    AuthFailed,
    /// The random pool gave nothing.
    NoRandom,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Closed => f.write_str("the connection closed"),
            Error::Disconnected => f.write_str("the client disconnected"),
            Error::Stopped => f.write_str("the server is stopping"),
            Error::Timeout(what) => write!(f, "timed out waiting for {}", what),
            Error::Mac => f.write_str("a packet failed its integrity check"),
            Error::NoAlgorithm(what) => write!(f, "no {} in common with the client", what),
            Error::Protocol(what) => write!(f, "protocol error: {}", what),
            Error::AuthFailed => f.write_str("too many failed authentication attempts"),
            Error::NoRandom => f.write_str("nothing from the random pool"),
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;
