//! User authentication (RFC 4252) and the connection protocol (RFC 4254):
//! public key logins, then one session channel -- a shell, or a command.

use alloc::string::String;
use alloc::vec::Vec;

use crate::keys::{PublicKey, ED25519};
use crate::terminal::{Action, Editor};
use crate::transport::{
    Transport, DISCONNECT_BY_APPLICATION, DISCONNECT_KEY_EXCHANGE_FAILED, DISCONNECT_MAC_ERROR,
    DISCONNECT_NO_MORE_AUTH_METHODS, DISCONNECT_PROTOCOL_ERROR, MSG_SERVICE_ACCEPT, MSG_SERVICE_REQUEST,
};
use crate::wire::{put_bool, put_name_list, put_string, put_u32, Reader};
use crate::{Config, Error, HostKey, Link, Result, Shell};

const MSG_USERAUTH_REQUEST: u8 = 50;
const MSG_USERAUTH_FAILURE: u8 = 51;
const MSG_USERAUTH_SUCCESS: u8 = 52;
const MSG_USERAUTH_PK_OK: u8 = 60;
const MSG_GLOBAL_REQUEST: u8 = 80;
const MSG_REQUEST_SUCCESS: u8 = 81;
const MSG_REQUEST_FAILURE: u8 = 82;
const MSG_CHANNEL_OPEN: u8 = 90;
const MSG_CHANNEL_OPEN_CONFIRMATION: u8 = 91;
const MSG_CHANNEL_OPEN_FAILURE: u8 = 92;
const MSG_CHANNEL_WINDOW_ADJUST: u8 = 93;
const MSG_CHANNEL_DATA: u8 = 94;
const MSG_CHANNEL_EXTENDED_DATA: u8 = 95;
const MSG_CHANNEL_EOF: u8 = 96;
const MSG_CHANNEL_CLOSE: u8 = 97;
const MSG_CHANNEL_REQUEST: u8 = 98;
const MSG_CHANNEL_SUCCESS: u8 = 99;
const MSG_CHANNEL_FAILURE: u8 = 100;

/* Why a channel is refused (RFC 4254 5.1) */
const OPEN_UNKNOWN_CHANNEL_TYPE: u32 = 3;
const OPEN_RESOURCE_SHORTAGE: u32 = 4;

const SERVICE_USERAUTH: &[u8] = b"ssh-userauth";
const SERVICE_CONNECTION: &[u8] = b"ssh-connection";
const METHOD_PUBLICKEY: &str = "publickey";
const METHOD_NONE: &[u8] = b"none";
/* What OpenSSH's own server asks a quiet client (ClientAliveInterval) */
const KEEPALIVE_REQUEST: &[u8] = b"keepalive@openssh.com";

/// What the client may send before the server has taken it in: the
/// window it is given, topped up once half of it has been.
const WINDOW: u32 = 64 * 1024;
/// The most data in one packet, either way.
const MAX_DATA: u32 = 32 * 1024;
/// The server's number for the one session channel a connection gets.
const CHANNEL_ID: u32 = 0;
/// How long output waits for a client that has shut its window -- a
/// suspended ssh, say -- before the session is given up.
const WINDOW_WAIT_MS: u64 = 5 * 60 * 1000;

const BAD_MESSAGE: Error = Error::Protocol("a malformed message");
const NO_CHANNEL: Error = Error::Protocol("a message for a channel that is not open");

/// Serves one connection from start to end: the version exchange, the key
/// exchange, a login, and the session after it. Ok when the session ended
/// the way sessions do -- its channel closed at one end and then the other.
pub fn serve(link: &mut dyn Link, shell: &mut dyn Shell, host: &HostKey, cfg: &Config) -> Result<()> {
    let mut seed = [0u8; 32];
    if !link.random(&mut seed) {
        return Err(Error::NoRandom);
    }
    let login_by = link.now_ms().saturating_add(cfg.login_grace_ms);
    let mut t = Transport::new(link, host, &seed, cfg.software);
    seed.fill(0);

    let result = connect(&mut t, shell, cfg, login_by);
    if let Err(e) = result {
        if let Some((reason, text)) = disconnect_reason(e) {
            t.disconnect(reason, text);
        }
    }
    result
}

fn connect(t: &mut Transport, shell: &mut dyn Shell, cfg: &Config, login_by: u64) -> Result<()> {
    /* The login grace holds through any key exchange the client starts */
    t.set_limit(login_by, "a login");
    t.handshake(login_by)?;
    authenticate(t, shell, cfg, login_by)?;
    t.clear_limit();
    Session::new(t, cfg).run(shell)
}

/// What the client is told as the server ends a connection over `e`.
fn disconnect_reason(e: Error) -> Option<(u32, &'static str)> {
    match e {
        Error::Closed | Error::Disconnected | Error::NoRandom => None,
        Error::Stopped => Some((DISCONNECT_BY_APPLICATION, "the server is stopping")),
        Error::Timeout(_) => Some((DISCONNECT_BY_APPLICATION, "timed out")),
        Error::Mac => Some((DISCONNECT_MAC_ERROR, "a packet failed its integrity check")),
        Error::NoAlgorithm(_) => Some((DISCONNECT_KEY_EXCHANGE_FAILED, "no algorithm in common")),
        Error::Protocol(what) => Some((DISCONNECT_PROTOCOL_ERROR, what)),
        Error::AuthFailed => Some((DISCONNECT_NO_MORE_AUTH_METHODS, "too many authentication failures")),
    }
}

/// A message before the login, which has to come by `login_by`.
fn wait(t: &mut Transport, payload: &mut Vec<u8>, login_by: u64) -> Result<()> {
    if t.next(payload, login_by)? {
        Ok(())
    } else {
        Err(Error::Timeout("a login"))
    }
}

/// Public key authentication (RFC 4252 7), ssh-ed25519 keys only; the
/// "none" a client opens with learns that it is the only method.
fn authenticate(t: &mut Transport, shell: &mut dyn Shell, cfg: &Config, login_by: u64) -> Result<()> {
    let mut payload = Vec::new();
    wait(t, &mut payload, login_by)?;
    let mut r = Reader::new(&payload[1..]);
    if payload[0] != MSG_SERVICE_REQUEST || r.string() != Some(SERVICE_USERAUTH) {
        return Err(Error::Protocol("a service other than ssh-userauth asked for before the login"));
    }
    let mut reply = Vec::new();
    reply.push(MSG_SERVICE_ACCEPT);
    put_string(&mut reply, SERVICE_USERAUTH);
    t.send(&reply)?;

    let mut failures = 0;
    loop {
        wait(t, &mut payload, login_by)?;
        if payload[0] != MSG_USERAUTH_REQUEST {
            return Err(Error::Protocol("a message other than an authentication request before the login"));
        }

        let mut r = Reader::new(&payload[1..]);
        let user = r.utf8().ok_or(BAD_MESSAGE)?;
        let service = r.string().ok_or(BAD_MESSAGE)?;
        let method = r.string().ok_or(BAD_MESSAGE)?;

        let mut counts = true;
        if service == SERVICE_CONNECTION && method == METHOD_PUBLICKEY.as_bytes() {
            let signed = r.bool().ok_or(BAD_MESSAGE)?;
            let alg = r.string().ok_or(BAD_MESSAGE)?;
            let blob = r.string().ok_or(BAD_MESSAGE)?;
            let key = if alg == ED25519.as_bytes() { PublicKey::from_blob(blob) } else { None };
            if let Some(key) = key.filter(|key| shell.authorized(user, key)) {
                if !signed {
                    /* The client asking whether the key would do before it
                       signs anything with it */
                    let mut ok = Vec::new();
                    ok.push(MSG_USERAUTH_PK_OK);
                    put_string(&mut ok, alg);
                    put_string(&mut ok, blob);
                    t.send(&ok)?;
                    continue;
                }
                let signature = r.string().ok_or(BAD_MESSAGE)?;
                if key.verify(&signed_data(t.session_id(), user, service, alg, blob), signature) {
                    t.send(&[MSG_USERAUTH_SUCCESS])?;
                    shell.logged_in(user, &key);
                    return Ok(());
                }
            }
        } else if method == METHOD_NONE {
            /* How a client learns the methods: no attempt of its own */
            counts = false;
        }

        if counts {
            failures += 1;
            if failures >= cfg.max_auth_tries {
                return Err(Error::AuthFailed);
            }
        }
        let mut no = Vec::new();
        no.push(MSG_USERAUTH_FAILURE);
        put_name_list(&mut no, &[METHOD_PUBLICKEY]);
        put_bool(&mut no, false);
        t.send(&no)?;
    }
}

/// What a public key login signs (RFC 4252 7): the session id, and the
/// request itself.
fn signed_data(session_id: &[u8], user: &str, service: &[u8], alg: &[u8], blob: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(128 + user.len() + blob.len());
    put_string(&mut data, session_id);
    data.push(MSG_USERAUTH_REQUEST);
    put_string(&mut data, user.as_bytes());
    put_string(&mut data, service);
    put_string(&mut data, METHOD_PUBLICKEY.as_bytes());
    put_bool(&mut data, true);
    put_string(&mut data, alg);
    put_string(&mut data, blob);
    data
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Open, and nothing asked of it yet.
    Idle,
    Shell,
    Exec,
}

struct Channel {
    /// The client's number for it.
    remote: u32,
    /// What the server may still send: the client's window.
    window: u64,
    /// The most the client takes in one packet.
    max_packet: usize,
    /// What the client may still send.
    local_window: u32,
    /// Taken in since the client's window was last topped up.
    consumed: u32,
    /// A terminal was asked for: the client's side is raw.
    pty: bool,
    mode: Mode,
    eof_in: bool,
    close_in: bool,
    close_out: bool,
}

/// What a message handled asks the session to do.
enum Next {
    None,
    Shell,
    Exec(String),
}

struct Session<'s, 'a> {
    t: &'s mut Transport<'a>,
    cfg: &'s Config<'s>,
    channel: Option<Channel>,
    /// Channel data not yet given to the line editor.
    input: Vec<u8>,
    editor: Editor,
    /// The message being handled, and the one being built.
    payload: Vec<u8>,
    message: Vec<u8>,
    /// Output on its way to a terminal, line ends made CR LF.
    translated: Vec<u8>,
    /// The last byte of output was a CR: an LF after it is fine as it is.
    last_cr: bool,
    last_heard: u64,
    keepalive_missed: u32,
}

/// A running command's view of its session: its output out on the channel,
/// and what the client types -- which the line editor gets otherwise, after
/// the command -- in. The first failure is kept for the session to end on
/// once the command returns; nothing is written or read after it.
struct CommandIo<'x, 's, 'a> {
    session: &'x mut Session<'s, 'a>,
    failed: Option<Error>,
}

impl crate::Io for CommandIo<'_, '_, '_> {
    fn write(&mut self, data: &[u8]) {
        if self.failed.is_none() {
            if let Err(e) = self.session.write_output(data) {
                self.failed = Some(e);
            }
        }
    }

    fn read(&mut self, buf: &mut [u8], timeout_ms: u64) -> Option<usize> {
        if self.failed.is_some() {
            return None;
        }
        match self.session.read_typed(buf, timeout_ms) {
            Ok(n) => n,
            Err(e) => {
                self.failed = Some(e);
                None
            }
        }
    }

    fn idle(&mut self, timeout_ms: u64) -> bool {
        if self.failed.is_some() {
            return false;
        }
        match self.session.tend(timeout_ms) {
            Ok(open) => open,
            Err(e) => {
                self.failed = Some(e);
                false
            }
        }
    }
}

impl<'s, 'a> Session<'s, 'a> {
    fn new(t: &'s mut Transport<'a>, cfg: &'s Config<'s>) -> Self {
        Self {
            t,
            cfg,
            channel: None,
            input: Vec::new(),
            editor: Editor::new(),
            payload: Vec::new(),
            message: Vec::new(),
            translated: Vec::new(),
            last_cr: false,
            last_heard: 0,
            keepalive_missed: 0,
        }
    }

    fn run(mut self, shell: &mut dyn Shell) -> Result<()> {
        self.last_heard = self.t.now_ms();
        loop {
            if self.shell_active() {
                if !self.input.is_empty() {
                    self.take_input(shell)?;
                    continue;
                }
                /* EOF from a shell's client is its ^D */
                if self.channel.as_ref().map_or(false, |c| c.eof_in) {
                    self.finish(0)?;
                }
            }
            if self.channel.as_ref().map_or(false, |c| c.close_in && c.close_out) {
                return Ok(());
            }

            let deadline = self.last_heard.saturating_add(self.cfg.keepalive_ms);
            if self.t.next(&mut self.payload, deadline)? {
                self.heard();
                match self.on_message(false)? {
                    Next::None => {}
                    Next::Shell => self.greet()?,
                    Next::Exec(line) => {
                        self.run_line(shell, &line)?;
                        self.finish(0)?;
                    }
                }
            } else {
                if self.keepalive_missed >= self.cfg.keepalive_max {
                    return Err(Error::Timeout("a client that stopped answering"));
                }
                self.keepalive()?;
                self.keepalive_missed += 1;
                self.last_heard = self.t.now_ms();
            }
        }
    }

    fn heard(&mut self) {
        self.last_heard = self.t.now_ms();
        self.keepalive_missed = 0;
    }

    fn shell_active(&self) -> bool {
        matches!(&self.channel, Some(c) if c.mode == Mode::Shell && !c.close_in && !c.close_out)
    }

    fn pty(&self) -> bool {
        self.channel.as_ref().map_or(false, |c| c.pty)
    }

    fn channel_for(&mut self, recipient: u32) -> Result<&mut Channel> {
        match &mut self.channel {
            Some(c) if recipient == CHANNEL_ID => Ok(c),
            _ => Err(NO_CHANNEL),
        }
    }

    /// Handles the message in `payload`. `busy`: output is waiting on the
    /// client's window, mid-command, and no new command can start.
    fn on_message(&mut self, busy: bool) -> Result<Next> {
        let payload = core::mem::take(&mut self.payload);
        let result = self.dispatch(&payload, busy);
        self.payload = payload;
        result
    }

    fn dispatch(&mut self, p: &[u8], busy: bool) -> Result<Next> {
        let mut r = Reader::new(&p[1..]);
        match p[0] {
            MSG_CHANNEL_OPEN => self.open(&mut r)?,
            MSG_CHANNEL_REQUEST => return self.request(&mut r, busy),
            MSG_CHANNEL_DATA => {
                let recipient = r.u32().ok_or(BAD_MESSAGE)?;
                let data = r.string().ok_or(BAD_MESSAGE)?;
                let channel = self.channel_for(recipient)?;
                channel.local_window = channel
                    .local_window
                    .checked_sub(data.len() as u32)
                    .ok_or(Error::Protocol("more data than the window allows"))?;
                if !channel.eof_in {
                    self.input.extend_from_slice(data);
                }
            }
            MSG_CHANNEL_EXTENDED_DATA => {
                /* Nothing a session's client has to say on stderr: counted
                   against the window, taken in and dropped */
                let recipient = r.u32().ok_or(BAD_MESSAGE)?;
                r.u32().ok_or(BAD_MESSAGE)?;
                let data = r.string().ok_or(BAD_MESSAGE)?;
                let channel = self.channel_for(recipient)?;
                channel.local_window = channel
                    .local_window
                    .checked_sub(data.len() as u32)
                    .ok_or(Error::Protocol("more data than the window allows"))?;
                channel.consumed += data.len() as u32;
            }
            MSG_CHANNEL_WINDOW_ADJUST => {
                let recipient = r.u32().ok_or(BAD_MESSAGE)?;
                let bytes = r.u32().ok_or(BAD_MESSAGE)?;
                let channel = self.channel_for(recipient)?;
                channel.window = core::cmp::min(channel.window + u64::from(bytes), u64::from(u32::MAX));
            }
            MSG_CHANNEL_EOF => {
                let recipient = r.u32().ok_or(BAD_MESSAGE)?;
                self.channel_for(recipient)?.eof_in = true;
            }
            MSG_CHANNEL_CLOSE => {
                let recipient = r.u32().ok_or(BAD_MESSAGE)?;
                let channel = self.channel_for(recipient)?;
                channel.close_in = true;
                if !channel.close_out {
                    channel.close_out = true;
                    let remote = channel.remote;
                    self.channel_message(MSG_CHANNEL_CLOSE, remote)?;
                }
            }
            MSG_GLOBAL_REQUEST => {
                r.string().ok_or(BAD_MESSAGE)?;
                if r.bool().ok_or(BAD_MESSAGE)? {
                    self.t.send(&[MSG_REQUEST_FAILURE])?;
                }
            }
            /* Answers to keepalives, to requests the server never made with
               want_reply, to a channel it never opened; and a second login,
               which is ignored (RFC 4252 5.1) */
            MSG_REQUEST_SUCCESS
            | MSG_REQUEST_FAILURE
            | MSG_CHANNEL_SUCCESS
            | MSG_CHANNEL_FAILURE
            | MSG_CHANNEL_OPEN_CONFIRMATION
            | MSG_CHANNEL_OPEN_FAILURE
            | MSG_USERAUTH_REQUEST => {}
            _ => self.t.unimplemented()?,
        }
        Ok(Next::None)
    }

    fn open(&mut self, r: &mut Reader) -> Result<()> {
        let kind = r.string().ok_or(BAD_MESSAGE)?;
        let sender = r.u32().ok_or(BAD_MESSAGE)?;
        let window = r.u32().ok_or(BAD_MESSAGE)?;
        let max_packet = r.u32().ok_or(BAD_MESSAGE)?;

        let refusal = if kind != b"session" {
            Some((OPEN_UNKNOWN_CHANNEL_TYPE, "only session channels are served"))
        } else if self.channel.is_some() {
            Some((OPEN_RESOURCE_SHORTAGE, "one session to a connection"))
        } else {
            None
        };

        let mut m = Vec::with_capacity(64);
        match refusal {
            Some((code, text)) => {
                m.push(MSG_CHANNEL_OPEN_FAILURE);
                put_u32(&mut m, sender);
                put_u32(&mut m, code);
                put_string(&mut m, text.as_bytes());
                put_string(&mut m, b"");
            }
            None => {
                self.channel = Some(Channel {
                    remote: sender,
                    window: u64::from(window),
                    max_packet: core::cmp::max(1, core::cmp::min(max_packet, MAX_DATA)) as usize,
                    local_window: WINDOW,
                    consumed: 0,
                    pty: false,
                    mode: Mode::Idle,
                    eof_in: false,
                    close_in: false,
                    close_out: false,
                });
                m.push(MSG_CHANNEL_OPEN_CONFIRMATION);
                put_u32(&mut m, sender);
                put_u32(&mut m, CHANNEL_ID);
                put_u32(&mut m, WINDOW);
                put_u32(&mut m, MAX_DATA);
            }
        }
        self.t.send(&m)
    }

    /// A channel request (RFC 4254 6): a terminal, then a shell or one
    /// command. The rest -- env, subsystems, forwarding -- is refused.
    fn request(&mut self, r: &mut Reader, busy: bool) -> Result<Next> {
        let recipient = r.u32().ok_or(BAD_MESSAGE)?;
        let kind = r.string().ok_or(BAD_MESSAGE)?;
        let want_reply = r.bool().ok_or(BAD_MESSAGE)?;
        let channel = self.channel_for(recipient)?;
        let remote = channel.remote;
        let idle = channel.mode == Mode::Idle && !busy;

        let mut next = Next::None;
        let granted = match kind {
            b"pty-req" => {
                channel.pty = true;
                true
            }
            b"window-change" => true,
            b"shell" if idle => {
                channel.mode = Mode::Shell;
                next = Next::Shell;
                true
            }
            b"exec" if idle => match r.utf8() {
                Some(command) => {
                    channel.mode = Mode::Exec;
                    next = Next::Exec(String::from(command));
                    true
                }
                None => false,
            },
            _ => false,
        };

        if want_reply {
            let answer = if granted { MSG_CHANNEL_SUCCESS } else { MSG_CHANNEL_FAILURE };
            self.channel_message(answer, remote)?;
        }
        Ok(next)
    }

    /// A message that is its number and the client's channel number.
    fn channel_message(&mut self, msg: u8, remote: u32) -> Result<()> {
        let mut m = [0u8; 5];
        m[0] = msg;
        m[1..].copy_from_slice(&remote.to_be_bytes());
        self.t.send(&m)
    }

    /* The banner for a person at a terminal only: a script piping lines into
       `ssh host` wants the commands' output and nothing else */
    fn greet(&mut self) -> Result<()> {
        if !self.pty() {
            return Ok(());
        }
        let banner = self.cfg.banner;
        self.write_output(banner.as_bytes())?;
        self.prompt()
    }

    fn prompt(&mut self) -> Result<()> {
        if !self.pty() {
            return Ok(());
        }
        let prompt = self.cfg.prompt;
        self.send_data(prompt.as_bytes())
    }

    /// Gives what the client typed to the line editor, running each line it
    /// finishes.
    fn take_input(&mut self, shell: &mut dyn Shell) -> Result<()> {
        let input = core::mem::take(&mut self.input);
        let pty = self.pty();
        let prompt = self.cfg.prompt;
        let mut echo = Vec::new();

        for &b in input.iter() {
            match self.editor.key(b, prompt, &mut echo) {
                Action::None => {}
                Action::Line(line) => {
                    if pty {
                        self.send_data(&echo)?;
                    }
                    echo.clear();
                    let command = line.trim();
                    if command == "exit" || command == "logout" {
                        return self.finish(0);
                    }
                    if !command.is_empty() {
                        self.run_line(shell, command)?;
                    }
                    if !self.shell_active() {
                        return Ok(());
                    }
                    self.prompt()?;
                }
                Action::Logout => {
                    if pty {
                        self.send_data(b"logout\r\n")?;
                    }
                    return self.finish(0);
                }
            }
        }
        if pty && !echo.is_empty() {
            self.send_data(&echo)?;
        }
        self.taken(input.len() as u32)
    }

    /// `n` bytes of channel data taken in: the client's window topped up
    /// once half of it has been.
    fn taken(&mut self, n: u32) -> Result<()> {
        let Some(channel) = &mut self.channel else {
            return Ok(());
        };
        channel.consumed += n;
        if channel.consumed < WINDOW / 2 || channel.close_out {
            return Ok(());
        }
        let adjust = core::mem::take(&mut channel.consumed);
        channel.local_window += adjust;
        let remote = channel.remote;

        let mut m = Vec::with_capacity(9);
        m.push(MSG_CHANNEL_WINDOW_ADJUST);
        put_u32(&mut m, remote);
        put_u32(&mut m, adjust);
        self.t.send(&m)
    }

    /// Runs a command line, its output going out as it comes and the
    /// client's typing there for it to read.
    fn run_line(&mut self, shell: &mut dyn Shell, line: &str) -> Result<()> {
        let mut io = CommandIo { session: self, failed: None };
        shell.run(line, &mut io);
        match io.failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// What the client typed, for a command that reads it: from what has
    /// come already, else from the messages that come before `timeout_ms`
    /// is out -- which are handled as they are while output waits, a rekey
    /// or a close among them. `Ok(None)` once no more will come.
    fn read_typed(&mut self, buf: &mut [u8], timeout_ms: u64) -> Result<Option<usize>> {
        let deadline = self.t.now_ms().saturating_add(timeout_ms);
        loop {
            if !self.input.is_empty() {
                let n = core::cmp::min(buf.len(), self.input.len());
                buf[..n].copy_from_slice(&self.input[..n]);
                self.input.drain(..n);
                self.taken(n as u32)?;
                return Ok(Some(n));
            }
            match &self.channel {
                Some(c) if !c.eof_in && !c.close_in && !c.close_out => {}
                _ => return Ok(None),
            }
            if buf.is_empty() || !self.t.next(&mut self.payload, deadline)? {
                return Ok(Some(0));
            }
            self.heard();
            self.on_message(true)?;
        }
    }

    /// While a command runs and neither writes nor reads: the messages that
    /// come before `timeout_ms` is out handled as `read_typed` handles them
    /// -- typing kept, not taken -- and a quiet client asked whether it is
    /// there, as `run` asks one between commands. Whether the channel is
    /// still open.
    fn tend(&mut self, timeout_ms: u64) -> Result<bool> {
        let deadline = self.t.now_ms().saturating_add(timeout_ms);
        loop {
            match &self.channel {
                Some(c) if !c.close_in && !c.close_out => {}
                _ => return Ok(false),
            }
            let now = self.t.now_ms();
            if now >= deadline {
                return Ok(true);
            }
            let ask_at = self.last_heard.saturating_add(self.cfg.keepalive_ms);
            if now >= ask_at {
                if self.keepalive_missed >= self.cfg.keepalive_max {
                    return Err(Error::Timeout("a client that stopped answering"));
                }
                self.keepalive()?;
                self.keepalive_missed += 1;
                self.last_heard = now;
                continue;
            }
            if self.t.next(&mut self.payload, core::cmp::min(deadline, ask_at))? {
                self.heard();
                self.on_message(true)?;
            }
        }
    }

    /// Output for the channel: line ends made CR LF for a terminal, which
    /// is raw, then out as the client's window allows.
    fn write_output(&mut self, data: &[u8]) -> Result<()> {
        if !self.pty() {
            return self.send_data(data);
        }
        let mut translated = core::mem::take(&mut self.translated);
        translated.clear();
        for &b in data {
            if b == b'\n' && !self.last_cr {
                translated.push(b'\r');
            }
            translated.push(b);
            self.last_cr = b == b'\r';
        }
        let result = self.send_data(&translated);
        self.translated = translated;
        result
    }

    /// Channel data, in packets no bigger than the client takes and no more
    /// than its window has room for, waiting for it to open the window when
    /// it is shut. Dropped once the channel is closing: nobody is left to
    /// read it.
    fn send_data(&mut self, mut data: &[u8]) -> Result<()> {
        while !data.is_empty() {
            let (remote, room) = match &self.channel {
                Some(c) if !c.close_in && !c.close_out => {
                    (c.remote, core::cmp::min(c.window, c.max_packet as u64) as usize)
                }
                _ => return Ok(()),
            };
            if room == 0 {
                self.wait_window()?;
                continue;
            }

            let n = core::cmp::min(room, data.len());
            let mut m = core::mem::take(&mut self.message);
            m.clear();
            m.push(MSG_CHANNEL_DATA);
            put_u32(&mut m, remote);
            put_string(&mut m, &data[..n]);
            let sent = self.t.send(&m);
            self.message = m;
            sent?;

            if let Some(c) = &mut self.channel {
                c.window -= n as u64;
            }
            data = &data[n..];
        }
        Ok(())
    }

    /// The client's window is shut: handles what comes meanwhile -- typing
    /// kept for later, a rekey, a close that makes the rest of the output
    /// moot -- until it opens it.
    fn wait_window(&mut self) -> Result<()> {
        let deadline = self.t.now_ms().saturating_add(WINDOW_WAIT_MS);
        if !self.t.next(&mut self.payload, deadline)? {
            return Err(Error::Timeout("the client to take more output"));
        }
        self.heard();
        self.on_message(true)?;
        Ok(())
    }

    /// Ends the channel as a shell's exit does: the status, EOF, CLOSE.
    fn finish(&mut self, status: u32) -> Result<()> {
        let remote = match &mut self.channel {
            Some(c) if !c.close_out => {
                c.close_out = true;
                c.remote
            }
            _ => return Ok(()),
        };

        let mut m = Vec::with_capacity(32);
        m.push(MSG_CHANNEL_REQUEST);
        put_u32(&mut m, remote);
        put_string(&mut m, b"exit-status");
        put_bool(&mut m, false);
        put_u32(&mut m, status);
        self.t.send(&m)?;
        self.channel_message(MSG_CHANNEL_EOF, remote)?;
        self.channel_message(MSG_CHANNEL_CLOSE, remote)
    }

    /// Asks a quiet client whether it is still there; any answer will do.
    fn keepalive(&mut self) -> Result<()> {
        let mut m = Vec::with_capacity(32);
        m.push(MSG_GLOBAL_REQUEST);
        put_string(&mut m, KEEPALIVE_REQUEST);
        put_bool(&mut m, true);
        self.t.send(&m)
    }
}
