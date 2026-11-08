//! `kernel.echo`: a mailbox of the image's that answers whatever it is sent
//! with what it was sent -- the same tag, the same reference, the payload
//! back -- for the boot's test of the module side (test_module_ipc), and a
//! ping for whoever wants one (`ipc echo`, `ipc ping`). It runs only while
//! started, and its name is the cluster's: a peer's `ipc ping
//! kernel.echo@host` reaches it through ipcnet.
//!
//! Sent `TAG_ECHO_PESTER`, it also monitors the sender and sends it a
//! message every millisecond until the sender's Down comes: a module
//! unloaded under traffic, which is what the boot's test does with it.
//!
//! A ping (`ipc ping`'s tag) it holds to the ping's own pattern as it comes,
//! before it goes back: so that an answer the pinger finds wrong says which
//! way it was made wrong -- on its way here, if the echo found it wrong too;
//! on its way back, if the echo found it right.

use crate::{ping, Addr, Limits, Mailbox, Message, Payload, Scope, To, TAG_BULK, TAG_DOWN};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use kcore::klog;
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::task::TaskHandle;
use kcore::time::Duration;

/// "PEST": monitor me, and keep sending until I am gone.
pub const TAG_ECHO_PESTER: u32 = 0x5045_5354;

const NAME: &str = "kernel.echo";
const LIMITS: Limits = Limits { messages: 64, bytes: 4 << 20 };
/// How often the pestered are sent to, and how long the echo sleeps with
/// nobody to pester.
const PESTER_MS: u64 = 1;
const IDLE_MS: u64 = 100;
/// Senders pestered at once.
const MAX_PESTERED: usize = 8;
/// Wrong pings said in the log, at most: each is counted.
const SAY_WRONG: u64 = 8;

struct State {
    task: Option<TaskHandle>,
    addr: Addr,
}

static STATE: OnceBox<Mutex<State>> = OnceBox::new();

fn state() -> Option<&'static Mutex<State>> {
    STATE.get_or_try_init(|| Some(Box::new(Mutex::new(State { task: None, addr: Addr::NONE })?)))
}

static ECHOED: AtomicU64 = AtomicU64::new(0);
static DOWNS: AtomicU64 = AtomicU64::new(0);
static PESTER_SENT: AtomicU64 = AtomicU64::new(0);
static PESTERING: AtomicU64 = AtomicU64::new(0);
static PINGS: AtomicU64 = AtomicU64::new(0);
static PINGS_WRONG: AtomicU64 = AtomicU64::new(0);

/// What the echo has done since boot, and whether it runs.
pub struct EchoStatus {
    pub running: bool,
    pub echoed: u64,
    pub downs: u64,
    pub pester_sent: u64,
    pub pestering: u64,
    /// Pings held to their pattern as they came, and how many were not it.
    pub pings: u64,
    pub pings_wrong: u64,
}

pub fn echo_status() -> EchoStatus {
    EchoStatus {
        running: state().is_some_and(|s| s.lock().task.is_some()),
        echoed: ECHOED.load(Ordering::Relaxed),
        downs: DOWNS.load(Ordering::Relaxed),
        pester_sent: PESTER_SENT.load(Ordering::Relaxed),
        pestering: PESTERING.load(Ordering::Relaxed),
        pings: PINGS.load(Ordering::Relaxed),
        pings_wrong: PINGS_WRONG.load(Ordering::Relaxed),
    }
}

/// A ping held to its pattern as it came: counted, and the first few that
/// are not it said in the log, in full.
fn check_ping(msg: &Message, scratch: &mut [u8]) {
    PINGS.fetch_add(1, Ordering::Relaxed);
    let len = msg.payload().len();
    if let Some(w) = ping::check(msg.payload(), len, msg.reference(), scratch) {
        if PINGS_WRONG.fetch_add(1, Ordering::Relaxed) < SAY_WRONG {
            klog!(0, "ipc echo: ping {} from {} not as it was made: {}", msg.reference(), msg.from(), w.said(len));
        }
    }
}

/// The echo started, its mailbox named `kernel.echo`: why not, if not.
pub fn echo_start() -> Result<(), &'static str> {
    let state = state().ok_or("no memory")?;
    let mut s = state.lock();
    if s.task.is_some() {
        return Err("it runs already");
    }
    let mb = Mailbox::new(LIMITS).map_err(|_| "no mailbox for it")?;
    mb.register(NAME, Scope::Cluster).map_err(|_| "kernel.echo is another mailbox's")?;
    s.addr = mb.addr();
    s.task = Some(kcore::task::spawn_with("ipc/echo", mb, serve).ok_or("no task for it")?);
    Ok(())
}

/// The echo stopped, and waited for: false if it did not run.
pub fn echo_stop() -> bool {
    let Some(state) = state() else { return false };
    /* Taken under the lock, waited for after it: the wait is the task's end */
    let (task, addr) = {
        let mut s = state.lock();
        (s.task.take(), s.addr)
    };
    let Some(task) = task else { return false };
    task.request_stop();
    crate::wake(addr);
    drop(task);
    true
}

fn serve(mb: Mailbox) {
    let mut pestered: Vec<(Addr, u64)> = Vec::new();
    /* Room to read a ping back a piece at a time: with none, pings go back
     * unchecked */
    let mut scratch: Vec<u8> = Vec::new();
    let checking = scratch.try_reserve_exact(ping::PIECE).is_ok();
    if checking {
        scratch.resize(ping::PIECE, 0);
    }
    while !kcore::task::stopping() {
        let wait = if pestered.is_empty() { IDLE_MS } else { PESTER_MS };
        if let Some(msg) = mb.recv(Duration::from_millis(wait)) {
            if msg.tag() == TAG_DOWN {
                if let Some(i) = pestered.iter().position(|&(_, id)| id == msg.reference()) {
                    pestered.swap_remove(i);
                    DOWNS.fetch_add(1, Ordering::Relaxed);
                }
            } else if !msg.is_system() {
                let (from, tag, reference) = (msg.from(), msg.tag(), msg.reference());
                if checking && tag & !TAG_BULK == ping::TAG {
                    check_ping(&msg, &mut scratch);
                }
                if tag == TAG_ECHO_PESTER && pestered.len() < MAX_PESTERED && pestered.try_reserve(1).is_ok() {
                    if let Ok(id) = mb.monitor(from) {
                        pestered.push((from, id));
                    }
                }
                /* The payload goes back as it came: moved, not copied */
                if mb.send(To::Addr(from), tag, reference, msg.into_payload()).is_ok() {
                    ECHOED.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        for &(to, _) in &pestered {
            /* Full is fine: the point is traffic, not every message */
            if mb.send(To::Addr(to), TAG_ECHO_PESTER, 0, Payload::empty()).is_ok() {
                PESTER_SENT.fetch_add(1, Ordering::Relaxed);
            }
        }
        PESTERING.store(pestered.len() as u64, Ordering::Relaxed);
    }
    for (_, id) in pestered {
        mb.demonitor(id);
    }
    PESTERING.store(0, Ordering::Relaxed);
}
