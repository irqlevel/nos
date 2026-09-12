#![no_std]

//! sshd: an SSH server, as a module.
//!
//!     insmod /sshd.ko
//!     sshd allow ssh-ed25519 AAAA... me@laptop
//!     sshd start
//!     sshd
//!     sshd stop
//!
//! The protocol is the ssh crate's (src/rust/ssh); this is the kernel's side
//! of it: the port listened on, a task for each connection, the shell behind
//! a login, and the two files a server keeps -- its host key,
//! /etc/ssh/ssh_host_ed25519_key, made on the first start, and the keys
//! allowed to log in, /etc/ssh/authorized_keys. docs/sshd.md has the rest.

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use kcore::cmd::{Command, Output};
use kcore::error::Error;
use kcore::net::Nic;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::tcp::{TcpListener, TcpStream, RECV_TIMEOUT};
use kcore::time::boot_time_ns;
use ssh::{AuthorizedKey, Config, HostKey, PublicKey};

const HELP: &str = "sshd [start [port] [nic=] | stop | allow <key> | deny <fp> | keys [reload]] - SSH server";
const _: () = assert!(HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const USAGE: &str = "usage: sshd                     what it serves, and who is logged in\n\
                     \x20      sshd start [port] [nic=eth0]  serve (port 22 unless told)\n\
                     \x20      sshd stop\n\
                     \x20      sshd allow <ssh-ed25519 AAAA... [comment]>\n\
                     \x20      sshd deny <SHA256:...>\n\
                     \x20      sshd keys [reload]";

const DEFAULT_PORT: u16 = 22;
const DEFAULT_NIC: &str = "eth0";

const ETC_DIR: &str = "/etc";
const SSH_DIR: &str = "/etc/ssh";
const HOST_KEY_PATH: &str = "/etc/ssh/ssh_host_ed25519_key";
const AUTHORIZED_KEYS_PATH: &str = "/etc/ssh/authorized_keys";
const HOST_KEY_COMMENT: &str = "nos sshd";
/* Larger than any key file, or list of keys, a person writes */
const HOST_KEY_FILE_MAX: usize = 8 * 1024;
const KEYS_FILE_MAX: usize = 64 * 1024;

/* Connections at once, and how many of them may still be logging in: a port
   22 on the internet is knocked on all day long, and what knocks must not
   crowd out the login that matters */
const MAX_SESSIONS: usize = 8;
const MAX_LOGGING_IN: usize = 4;
/* The longest wait for a connection, or on one, at a time: how soon a stop
   is noticed */
const POLL_MS: u64 = 250;
/* A client that takes none of what it is sent for this long -- its TCP
   window shut -- is not coming back for it, and its session is given up, as
   it is when it leaves its SSH window shut as long */
const SEND_STALL_NS: u64 = 5 * 60 * NS_PER_SEC;

const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;

static CONFIG: Config<'static> = Config {
    software: "nos_sshd",
    banner: "nos -- `help` lists the commands, `exit` leaves\n",
    prompt: "$ ",
    login_grace_ms: 30_000,
    max_auth_tries: 6,
    keepalive_ms: 60_000,
    keepalive_max: 3,
};

/* ------------------------------------------------------------------ */
/* The module and its command                                          */
/* ------------------------------------------------------------------ */

struct Sshd {
    _cmd: Command,
    state: Arc<State>,
}

impl kmod::Module for Sshd {}

impl Drop for Sshd {
    fn drop(&mut self) {
        /* Before the fields go -- the command first, whose drop waits out a
           call of it still running: a call whose output an SSH client is not
           taking gives up only once its session sees the stop */
        self.state.server.with(|server| {
            if let Some(s) = server.as_ref() {
                s.shared.stop.store(true, Ordering::Release);
            }
        });
    }
}

fn init() -> kcore::error::Result<Box<dyn kmod::Module>> {
    let state = Arc::new(State {
        server: Locked::new(None).ok_or(Error::NoMemory)?,
        keys: Arc::new(Locked::new(Vec::new()).ok_or(Error::NoMemory)?),
        files: Locked::new(()).ok_or(Error::NoMemory)?,
    });

    /* The keys as the file has them from the start, so that `keys`, `allow`
       and `deny` see what `start` will; what could not be taken goes to the
       log. Nothing else runs yet to race it for the file. */
    let mut notes = String::new();
    match load_keys(&state.keys, &mut notes) {
        Ok(count) => kcore::trace!(0, "sshd: {} authorized keys in {}", count, AUTHORIZED_KEYS_PATH),
        Err(problem) => kcore::trace!(0, "sshd: {}", problem),
    }
    for line in notes.lines() {
        kcore::trace!(0, "{}", line);
    }

    let st = state.clone();
    let cmd = Command::register("sshd", HELP, move |args, out| {
        if let Err(problem) = run(&st, args, out) {
            let _ = writeln!(out, "sshd: {}", problem);
        }
    })?;

    /* After the drop above sets the stop: the command goes, waiting out a
       call still running, and the server with the last reference to the
       state -- its drop waits for the listener and every session */
    Ok(Box::new(Sshd { _cmd: cmd, state }))
}

kmod::module!(name: "sshd", init: init);

/// A value behind a kernel mutex, which sleeps: never held while waiting
/// for a task, nor taken by anything such a task could be waiting in.
struct Locked<T> {
    lock: Mutex,
    value: UnsafeCell<T>,
}

/* value is only reached under lock */
unsafe impl<T: Send> Send for Locked<T> {}
unsafe impl<T: Send> Sync for Locked<T> {}

impl<T> Locked<T> {
    fn new(value: T) -> Option<Self> {
        Some(Self { lock: Mutex::new()?, value: UnsafeCell::new(value) })
    }

    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _guard = self.lock.lock();
        f(unsafe { &mut *self.value.get() })
    }
}

type Keys = Locked<Vec<AuthorizedKey>>;

/// The module's: the server while there is one, and the keys allowed in,
/// kept whether it runs or not -- `allow` and `keys` work either way.
struct State {
    server: Locked<Option<Server>>,
    keys: Arc<Keys>,
    /* Held through every change to the files -- the authorized keys read,
       changed and written back, the host key made -- which the console, the
       UDP shell and every session may ask for at once */
    files: Locked<()>,
}

fn run(state: &State, args: &str, out: &mut Output) -> Result<(), String> {
    let mut words = args.split_whitespace();
    match words.next() {
        None | Some("status") => {
            status(state, out);
            Ok(())
        }
        Some("start") => start(state, words, out),
        Some("stop") => stop(state, out),
        Some("allow") => {
            let line = args.trim_start().strip_prefix("allow").unwrap_or("").trim();
            allow(state, line, out)
        }
        Some("deny") => deny(state, words.next(), out),
        Some("keys") => match words.next() {
            None => {
                list_keys(state, out);
                Ok(())
            }
            Some("reload") => {
                let count = state.files.with(|_| load_keys(&state.keys, out))?;
                let _ = writeln!(out, "sshd: {} authorized keys, from {}", count, AUTHORIZED_KEYS_PATH);
                Ok(())
            }
            Some(other) => Err(format!("what is '{}'?\n{}", other, USAGE)),
        },
        Some(other) => Err(format!("what is '{}'?\n{}", other, USAGE)),
    }
}

fn start<'a>(state: &State, words: impl Iterator<Item = &'a str>, out: &mut Output) -> Result<(), String> {
    let mut port = DEFAULT_PORT;
    let mut nic_name = DEFAULT_NIC;
    for word in words {
        match word.split_once('=') {
            Some(("nic", v)) => nic_name = v,
            None => {
                port = match word.parse::<u16>() {
                    Ok(p) if p != 0 => p,
                    _ => return Err(format!("{}: not a TCP port", word)),
                }
            }
            _ => return Err(format!("what is '{}'?\n{}", word, USAGE)),
        }
    }

    if let Some(port) = state.server.with(|server| server.as_ref().map(|s| s.port)) {
        return Err(format!("serving port {} already -- sshd stop first", port));
    }
    let nic = Nic::find(nic_name).ok_or_else(|| format!("no network device {} -- `net` lists them", nic_name))?;

    let (host, keys) = state
        .files
        .with(|_| -> Result<(HostKey, usize), String> { Ok((load_host_key(out)?, load_keys(&state.keys, out)?)) })?;
    let fingerprint = host.public().fingerprint();
    let listener = TcpListener::bind(&nic, port)
        .map_err(|_| format!("cannot listen on TCP port {}: someone has it, or no connection slot is free", port))?;

    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        host,
        fingerprint: fingerprint.clone(),
        keys: state.keys.clone(),
        sessions: Locked::new(Vec::new()).ok_or_else(|| "no memory".to_string())?,
        next_id: AtomicU64::new(1),
        stats: Stats::default(),
    });

    let ctx = Box::into_raw(Box::new(ListenerCtx { shared: shared.clone(), listener })) as *mut u8;
    let task = match kcore::task::spawn_with_ctx("sshd", listener_main, ctx) {
        Some(task) => task,
        None => {
            drop(unsafe { Box::from_raw(ctx as *mut ListenerCtx) });
            return Err("could not start its task".to_string());
        }
    };
    let server = Server { port, nic: nic_name.to_string(), started: boot_time_ns(), shared, listener: Some(task) };

    /* Checked again where two starts cannot both get past it */
    let lost = state.server.with(|slot| {
        if slot.is_some() {
            Some(server)
        } else {
            *slot = Some(server);
            None
        }
    });
    if let Some(server) = lost {
        drop(server);
        return Err("started from somewhere else meanwhile".to_string());
    }

    kcore::trace!(0, "sshd: listening on port {}, host key {}", port, fingerprint);
    let _ = writeln!(
        out,
        "sshd: listening on port {} -- {} {} -- host key {} (ED25519), {} authorized keys",
        port,
        nic_name,
        Ip(nic.ip()),
        fingerprint,
        keys
    );
    if keys == 0 {
        let _ = writeln!(out, "sshd: nobody can log in yet -- sshd allow <the line of an ssh-ed25519 .pub file>");
    }
    Ok(())
}

fn stop(state: &State, out: &mut Output) -> Result<(), String> {
    /* From one of the server's own sessions the stop would wait for the
       very task it runs in */
    let me = kcore::task::current_id();
    let own = state.server.with(|server| {
        server.as_ref().map_or(false, |s| s.shared.sessions.with(|v| v.iter().any(|e| e.task_id == me)))
    });
    if own {
        return Err("this session would wait for itself to end -- stop it from the console or the UDP shell, or rmmod sshd".to_string());
    }

    let server = state.server.with(|server| server.take()).ok_or_else(|| "not running".to_string())?;
    let port = server.port;
    /* Out of the lock: this waits for the listener and every session */
    drop(server);
    kcore::trace!(0, "sshd: stopped serving port {}", port);
    let _ = writeln!(out, "sshd: stopped serving port {}", port);
    Ok(())
}

fn status(state: &State, out: &mut Output) {
    /* Copied out under the locks, printed after: the output may be an SSH
       session's, and the client in no hurry to take it */
    let view = state.server.with(|server| {
        server.as_ref().map(|s| {
            let sessions: Vec<(Peer, u64, Option<String>)> = s.shared.sessions.with(|v| {
                v.iter().filter(|e| !e.done).map(|e| (e.peer, e.started, e.user.clone())).collect()
            });
            (s.port, s.nic.clone(), s.started, s.shared.fingerprint.clone(), s.shared.stats.snapshot(), sessions)
        })
    });
    let keys = state.keys.with(|keys| keys.len());

    let (port, nic, started, fingerprint, stats, sessions) = match view {
        Some(view) => view,
        None => {
            let _ = writeln!(out, "sshd: not running -- sshd start [port]; {} authorized keys", keys);
            return;
        }
    };

    let now = boot_time_ns();
    let ip = Nic::find(&nic).map_or(0, |n| n.ip());
    let _ = writeln!(
        out,
        "sshd: listening on port {} -- {} {} -- for {} s, host key {} (ED25519)",
        port,
        nic,
        Ip(ip),
        now.saturating_sub(started) / NS_PER_SEC,
        fingerprint
    );
    let _ = writeln!(
        out,
        "  {} authorized keys; connections {}, refused {}, logins {}, gone before a login {}",
        keys, stats[0], stats[1], stats[2], stats[3]
    );
    for (peer, since, user) in sessions {
        let secs = now.saturating_sub(since) / NS_PER_SEC;
        match user {
            Some(user) => {
                let _ = writeln!(out, "  {} from {}, {} s", user, peer, secs);
            }
            None => {
                let _ = writeln!(out, "  {} logging in, {} s", peer, secs);
            }
        }
    }
}

/* ------------------------------------------------------------------ */
/* Keys                                                                */
/* ------------------------------------------------------------------ */

/// The host key in its file, or a new one -- saved there for the next
/// start, if the filesystem takes it.
fn load_host_key(out: &mut Output) -> Result<HostKey, String> {
    match kcore::fs::read(HOST_KEY_PATH, HOST_KEY_FILE_MAX) {
        Ok(text) => HostKey::from_openssh(&text)
            .map_err(|why| format!("{}: {} -- move it out of the way and a new one is made", HOST_KEY_PATH, why)),
        Err(Error::NotFound) => {
            let mut seed = [0u8; 32];
            if !kcore::random::fill_random(&mut seed) {
                return Err("nothing from the random pool for a host key: it is not seeded yet".to_string());
            }
            let key = HostKey::from_seed(&seed);
            seed.fill(0);

            let check = kcore::random::random_u64().unwrap_or(0) as u32;
            let text = key.to_openssh(HOST_KEY_COMMENT, check);
            /* Created, never written over: a key file that is there after
               all -- the read above failed on a bad block, say -- is kept,
               and the start refused */
            let saved = kcore::fs::create_dir(ETC_DIR)
                .and_then(|_| kcore::fs::create_dir(SSH_DIR))
                .and_then(|_| kcore::fs::create(HOST_KEY_PATH, text.as_bytes()));
            let _ = match saved {
                Ok(()) => writeln!(out, "sshd: made a host key, {}", HOST_KEY_PATH),
                Err(Error::Busy) => {
                    return Err(format!(
                        "{} is there and could not be read -- try again, or move it out of the way",
                        HOST_KEY_PATH
                    ))
                }
                Err(_) => writeln!(
                    out,
                    "sshd: made a host key, and could not keep it in {}: it changes at the next start",
                    HOST_KEY_PATH
                ),
            };
            Ok(key)
        }
        Err(Error::InvalidValue) => Err(format!("{} is larger than any key file", HOST_KEY_PATH)),
        Err(e) => Err(format!("cannot read {}: {}", HOST_KEY_PATH, e)),
    }
}

/// The authorized_keys file as text, empty when there is none.
fn read_keys_file() -> Result<Vec<u8>, String> {
    match kcore::fs::read(AUTHORIZED_KEYS_PATH, KEYS_FILE_MAX) {
        Ok(text) => Ok(text),
        Err(Error::NotFound) => Ok(Vec::new()),
        Err(Error::InvalidValue) => Err(format!("{} is larger than {} KiB", AUTHORIZED_KEYS_PATH, KEYS_FILE_MAX / 1024)),
        Err(e) => Err(format!("cannot read {}: {}", AUTHORIZED_KEYS_PATH, e)),
    }
}

fn write_keys_file(text: &[u8]) -> Result<(), String> {
    kcore::fs::create_dir(ETC_DIR)
        .and_then(|_| kcore::fs::create_dir(SSH_DIR))
        .and_then(|_| kcore::fs::write(AUTHORIZED_KEYS_PATH, text))
        .map_err(|_| format!("cannot write {}", AUTHORIZED_KEYS_PATH))
}

/// Reads the authorized_keys file into `keys`, in place of what they were:
/// how many it took. A line it cannot take -- not text, not a key it knows
/// -- is said, and skipped, the rest taken. With the files lock held.
fn load_keys(keys: &Keys, out: &mut dyn Write) -> Result<usize, String> {
    let text = read_keys_file()?;

    let mut list: Vec<AuthorizedKey> = Vec::new();
    for (number, line) in text.split(|&b| b == b'\n').enumerate() {
        let parsed = match core::str::from_utf8(line) {
            Ok(line) => AuthorizedKey::parse(line),
            Err(_) => Err("not text"),
        };
        match parsed {
            Ok(Some(key)) => {
                if !list.iter().any(|k| k.key == key.key) {
                    list.push(key);
                }
            }
            Ok(None) => {}
            Err(why) => {
                let _ = writeln!(out, "sshd: {} line {}: {}, skipped", AUTHORIZED_KEYS_PATH, number + 1, why);
            }
        }
    }

    let count = list.len();
    let old = keys.with(|keys| core::mem::replace(keys, list));
    drop(old);
    Ok(count)
}

fn allow(state: &State, line: &str, out: &mut Output) -> Result<(), String> {
    let key = AuthorizedKey::parse(line)
        .map_err(|why| format!("{} -- give it the line of a .pub file: ssh-ed25519 AAAA... comment", why))?
        .ok_or_else(|| format!("which key?\n{}", USAGE))?;
    let fingerprint = key.key.fingerprint();

    /* The file read, added to and written back with the files lock held:
       two at once would each write the other's key away */
    state.files.with(|_| {
        if state.keys.with(|keys| keys.iter().any(|k| k.key == key.key)) {
            return Err(format!("{} is allowed already", fingerprint));
        }

        /* Into the file too, for the next start and the next boot */
        let mut text = read_keys_file()?;
        if !text.is_empty() && !text.ends_with(b"\n") {
            text.push(b'\n');
        }
        text.extend_from_slice(key.to_line().as_bytes());
        text.push(b'\n');
        let saved = write_keys_file(&text);

        let comment = key.comment.clone();
        state.keys.with(|keys| keys.push(key));
        match saved {
            Ok(()) => {
                let _ = writeln!(out, "sshd: allowed {} {}", fingerprint, comment);
            }
            Err(why) => {
                let _ = writeln!(out, "sshd: allowed {} {} until the next start: {}", fingerprint, comment, why);
            }
        }
        Ok(())
    })
}

fn deny(state: &State, which: Option<&str>, out: &mut Output) -> Result<(), String> {
    let which = which.ok_or_else(|| format!("which key? `sshd keys` shows their fingerprints\n{}", USAGE))?;

    state.files.with(|_| {
        let from_memory = state.keys.with(|keys| {
            let before = keys.len();
            keys.retain(|k| k.key.fingerprint() != which);
            before - keys.len()
        });

        /* And out of the file whatever the list in memory holds -- it is only
           ever what the file had, or less: every line but that key's, byte for
           byte, the ones that are not text or not keys included */
        let text = read_keys_file()?;
        let mut kept: Vec<u8> = Vec::with_capacity(text.len());
        let mut from_file = 0;
        for line in text.split_inclusive(|&b| b == b'\n') {
            let parsed = core::str::from_utf8(line).ok().and_then(|l| AuthorizedKey::parse(l).ok().flatten());
            if matches!(&parsed, Some(key) if key.key.fingerprint() == which) {
                from_file += 1;
                continue;
            }
            kept.extend_from_slice(line);
        }

        if from_memory == 0 && from_file == 0 {
            return Err(format!("no key {} -- `sshd keys` lists them", which));
        }
        if from_file != 0 {
            if let Err(why) = write_keys_file(&kept) {
                let _ = writeln!(out, "sshd: denied {} until the next start: {}", which, why);
                return Ok(());
            }
        }
        let _ = writeln!(out, "sshd: denied {}", which);
        Ok(())
    })
}

fn list_keys(state: &State, out: &mut Output) {
    let lines: Vec<String> =
        state.keys.with(|keys| keys.iter().map(|k| format!("{} {}", k.key.fingerprint(), k.comment)).collect());
    if lines.is_empty() {
        let _ = writeln!(out, "sshd: no authorized keys -- sshd allow <key>, or put them in {}", AUTHORIZED_KEYS_PATH);
    }
    for line in lines {
        let _ = writeln!(out, "{}", line);
    }
}

/* ------------------------------------------------------------------ */
/* The server: a listener task, and a task for each connection         */
/* ------------------------------------------------------------------ */

struct Server {
    port: u16,
    nic: String,
    started: u64,
    shared: Arc<Shared>,
    listener: Option<TaskHandle>,
}

impl Drop for Server {
    fn drop(&mut self) {
        /* The listener notices within a poll, closes the port -- which resets
           whoever connected and was not taken in yet -- and waits for every
           session, each of which notices at its next wait */
        self.shared.stop.store(true, Ordering::Release);
        drop(self.listener.take());
    }
}

/// What the listener and the sessions share.
struct Shared {
    stop: AtomicBool,
    host: HostKey,
    fingerprint: String,
    keys: Arc<Keys>,
    sessions: Locked<Vec<SessionInfo>>,
    next_id: AtomicU64,
    stats: Stats,
}

struct SessionInfo {
    id: u64,
    peer: Peer,
    started: u64,
    /// Who logged in; None while it is still logging in.
    user: Option<String>,
    /// Its task: the listener's to wait for.
    task: Option<TaskHandle>,
    /// The task, as kcore::task::current_id names it.
    task_id: usize,
    /// It has returned, or is about to: the listener may wait for it.
    done: bool,
}

#[derive(Default)]
struct Stats {
    connections: AtomicU64,
    refused: AtomicU64,
    logins: AtomicU64,
    unauthenticated: AtomicU64,
}

impl Stats {
    fn snapshot(&self) -> [u64; 4] {
        [
            self.connections.load(Ordering::Relaxed),
            self.refused.load(Ordering::Relaxed),
            self.logins.load(Ordering::Relaxed),
            self.unauthenticated.load(Ordering::Relaxed),
        ]
    }
}

struct ListenerCtx {
    shared: Arc<Shared>,
    listener: TcpListener,
}

extern "C" fn listener_main(ctx: *mut u8) {
    let ctx = unsafe { Box::from_raw(ctx as *mut ListenerCtx) };
    let ListenerCtx { shared, listener } = *ctx;

    while !shared.stop.load(Ordering::Acquire) {
        reap(&shared);
        if let Some(stream) = listener.accept(POLL_MS) {
            admit(&shared, stream);
        }
    }

    /* Nobody else in: the port closes, and a connection past the kernel's
       handshake that was never taken in is reset */
    drop(listener);

    /* Every session sees the stop at its next wait; each is waited for out
       of the lock */
    let tasks: Vec<TaskHandle> =
        shared.sessions.with(|sessions| sessions.iter_mut().filter_map(|s| s.task.take()).collect());
    drop(tasks);
}

/// Waits for the sessions that have finished, out of the lock.
fn reap(shared: &Shared) {
    let finished: Vec<TaskHandle> = shared.sessions.with(|sessions| {
        let mut finished = Vec::new();
        sessions.retain_mut(|s| {
            if !s.done {
                return true;
            }
            if let Some(task) = s.task.take() {
                finished.push(task);
            }
            false
        });
        finished
    });
    drop(finished);
}

fn admit(shared: &Arc<Shared>, stream: TcpStream) {
    shared.stats.connections.fetch_add(1, Ordering::Relaxed);
    let peer = Peer(stream.peer());

    let (total, logging_in) =
        shared.sessions.with(|v| (v.len(), v.iter().filter(|s| s.user.is_none()).count()));
    if total >= MAX_SESSIONS || logging_in >= MAX_LOGGING_IN {
        shared.stats.refused.fetch_add(1, Ordering::Relaxed);
        kcore::trace!(1, "sshd: {} refused: {} sessions, {} of them logging in", peer, total, logging_in);
        /* Reset, not closed: closing first on everyone turned away would
           keep a TCP slot a minute each, and run the kernel out of them */
        stream.abort();
        return;
    }

    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    shared.sessions.with(|v| {
        v.push(SessionInfo { id, peer, started: boot_time_ns(), user: None, task: None, task_id: 0, done: false })
    });

    let ctx = Box::into_raw(Box::new(SessionCtx { shared: shared.clone(), stream, id })) as *mut u8;
    /* Named by its peer, the way `sshd` lists it */
    match kcore::task::spawn_with_ctx(&format!("sshd/{}", peer), session_main, ctx) {
        Some(task) => {
            let task_id = task.id();
            /* The listener alone takes entries out, so the session's is
               still there, whether or not it has finished by now */
            shared.sessions.with(|v| {
                if let Some(s) = v.iter_mut().find(|s| s.id == id) {
                    s.task_id = task_id;
                    s.task = Some(task);
                }
            });
        }
        None => {
            drop(unsafe { Box::from_raw(ctx as *mut SessionCtx) });
            shared.sessions.with(|v| v.retain(|s| s.id != id));
            kcore::trace!(0, "sshd: no task for a session from {}", peer);
        }
    }
}

struct SessionCtx {
    shared: Arc<Shared>,
    stream: TcpStream,
    id: u64,
}

extern "C" fn session_main(ctx: *mut u8) {
    let ctx = unsafe { Box::from_raw(ctx as *mut SessionCtx) };
    let SessionCtx { shared, stream, id } = *ctx;
    let peer = Peer(stream.peer());
    let started = boot_time_ns();

    let mut link = KernelLink { stream, shared: &shared };
    let mut shell = KernelShell { shared: &shared, id, peer, user: None };
    let result = ssh::serve(&mut link, &mut shell, &shared.host, &CONFIG);
    /* A session that got to a login closes the usual way: its client has
       mostly closed first, and the TIME-WAIT is the client's. One that never
       did -- a scanner, a login that timed out -- is reset: the server would
       be closing first, and every such close would keep a TCP slot a minute */
    if shell.user.is_some() {
        drop(link);
    } else {
        link.stream.abort();
    }

    let secs = boot_time_ns().saturating_sub(started) / NS_PER_SEC;
    match (&shell.user, result) {
        (Some(user), Ok(())) => kcore::trace!(0, "sshd: {} from {} logged out after {} s", user, peer, secs),
        (Some(user), Err(e)) => kcore::trace!(0, "sshd: {} from {} gone after {} s: {}", user, peer, secs, e),
        (None, Ok(())) => {}
        (None, Err(e)) => {
            shared.stats.unauthenticated.fetch_add(1, Ordering::Relaxed);
            kcore::trace!(1, "sshd: {} gone before a login: {}", peer, e);
        }
    }

    shared.sessions.with(|v| {
        if let Some(s) = v.iter_mut().find(|s| s.id == id) {
            s.done = true;
        }
    });
}

/// The connection, and what else ssh needs of the kernel.
struct KernelLink<'a> {
    stream: TcpStream,
    shared: &'a Shared,
}

impl ssh::Link for KernelLink<'_> {
    fn recv(&mut self, buf: &mut [u8], timeout_ms: u64) -> ssh::Recv {
        /* 0 would be forever to the kernel */
        let n = self.stream.recv(buf, timeout_ms.max(1));
        if n > 0 {
            ssh::Recv::Data(n as usize)
        } else if n == RECV_TIMEOUT {
            ssh::Recv::Timeout
        } else {
            ssh::Recv::Closed
        }
    }

    fn send(&mut self, mut data: &[u8]) -> bool {
        /* A client that takes nothing -- a suspended ssh -- must not keep a
           stop waiting: room is waited for a poll at a time, and for
           SEND_STALL_NS in all */
        let mut progress = boot_time_ns();
        while !data.is_empty() {
            let n = self.stream.send(data, POLL_MS);
            if n < 0 {
                return false;
            }
            if n == 0 {
                if self.shared.stop.load(Ordering::Acquire)
                    || boot_time_ns().saturating_sub(progress) >= SEND_STALL_NS
                {
                    return false;
                }
                continue;
            }
            progress = boot_time_ns();
            data = &data[n as usize..];
        }
        true
    }

    fn now_ms(&self) -> u64 {
        boot_time_ns() / NS_PER_MS
    }

    fn stopping(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire)
    }

    fn random(&mut self, buf: &mut [u8]) -> bool {
        kcore::random::fill_random(buf)
    }
}

/// Who may log in -- the keys in the authorized list, whatever the user
/// name, there being one user -- and the kernel's shell behind the login.
struct KernelShell<'a> {
    shared: &'a Shared,
    id: u64,
    peer: Peer,
    user: Option<String>,
}

impl ssh::Shell for KernelShell<'_> {
    fn authorized(&mut self, _user: &str, key: &PublicKey) -> bool {
        self.shared.keys.with(|keys| keys.iter().any(|k| k.key == *key))
    }

    fn logged_in(&mut self, user: &str, key: &PublicKey) {
        self.user = Some(user.to_string());
        let name = user.to_string();
        let id = self.id;
        self.shared.sessions.with(|v| {
            if let Some(s) = v.iter_mut().find(|s| s.id == id) {
                s.user = Some(name);
            }
        });
        self.shared.stats.logins.fetch_add(1, Ordering::Relaxed);

        let comment = self
            .shared
            .keys
            .with(|keys| keys.iter().find(|k| k.key == *key).map(|k| k.comment.clone()))
            .unwrap_or_default();
        kcore::trace!(0, "sshd: {} logged in from {} with {} {}", user, self.peer, key.fingerprint(), comment);
    }

    fn run(&mut self, line: &str, out: &mut dyn FnMut(&[u8])) {
        kcore::cmd::dispatch(line, out);
    }
}

/* An IPv4 address in host byte order, dotted */
struct Ip(u32);

impl fmt::Display for Ip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0.to_be_bytes();
        write!(f, "{}.{}.{}.{}", b[0], b[1], b[2], b[3])
    }
}

/* The other end of a connection: address and port */
#[derive(Clone, Copy)]
struct Peer((u32, u16));

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", Ip(self.0 .0), self.0 .1)
    }
}
