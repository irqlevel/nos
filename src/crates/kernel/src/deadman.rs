//! `deadman=N`: a boot on trial. N seconds into the boot the kernel panics --
//! its report saying why -- unless the shell's `deadman off` came first, and
//! with `panic=M` the machine resets M seconds after that. For a server
//! whose only console is its network, booted into nos once to try something
//! (nos-install's one-shot boot, docs/install.md): a nos that never reaches
//! the network, or wedges where no watchdog looks -- its disks gone, its
//! `/etc/rc` stuck -- is back on the server's own system by itself, and what
//! became of it is in netconsole's collector and the disk log. `deadman N`
//! sets the deadline N seconds from now, whether one was set or not;
//! `deadman` says where it stands.
//!
//! The deadline is looked at once a second, in the boot CPU's tick, by a
//! timer started the first time a deadline is set: a boot that never asks
//! for one runs nothing for it.

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use kcore::cmd::{Command, Output};
use kcore::klog;
use kcore::once::OnceBox;
use kcore::time::{self, Duration};
use kcore::timer::Timer;

/// How often the deadline is looked at.
const PERIOD: Duration = Duration::from_secs(1);
/// The seconds `deadman N` takes: what kernel/parameters.h's
/// DEADMAN_SECS_MIN and DEADMAN_SECS_MAX hold `deadman=N` to.
const SECS_MIN: u64 = 10;
const SECS_MAX: u64 = 86_400;
const NS_PER_SEC: u64 = 1_000_000_000;

const USAGE: &str = "usage: deadman [off|<secs>]";

/// The deadline, in nanoseconds since boot: 0 for none.
struct Deadman {
    deadline_ns: AtomicU64,
}

static DEADMAN: Deadman = Deadman { deadline_ns: AtomicU64::new(0) };
/// The timer that looks at it, once a deadline was first set: it runs for
/// as long as the kernel does.
static TIMER: OnceBox<Timer> = OnceBox::new();

/// Called from `rust_init`, before anything else there: `deadman=N`'s
/// deadline set, counted from the start of the boot, and the command put
/// in front of whoever runs one.
pub fn init() {
    let registered = match Command::register("deadman", "deadman [off|<secs>] - the boot's deadline: a panic past it", deadman) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => {
            core::mem::forget(cmd);
            true
        }
        Err(_) => false,
    };
    let secs = kcore::system::deadman_secs();
    if secs == 0 {
        if !registered {
            klog!(0, "deadman: cannot register the command");
        }
        return;
    }
    /* Asked for, it is set whether or not it can be called off: a machine
       reset is the side this fails to. */
    if !set(secs.saturating_mul(NS_PER_SEC)) {
        klog!(0, "deadman: no timer to be had, deadman={} not set", secs);
    } else if registered {
        klog!(0, "deadman: a panic {} s into the boot, unless `deadman off`", secs);
    } else {
        klog!(0, "deadman: a panic {} s into the boot -- the command could not be registered, nothing can call it off",
            secs);
    }
}

/// The deadline set at `at_ns` since boot, the timer that looks at it
/// started where it was not: false, and nothing set, where no timer could be
/// had.
fn set(at_ns: u64) -> bool {
    if TIMER.get_or_try_init(|| Timer::start_for(PERIOD, &DEADMAN, look).map(Box::new)).is_none() {
        return false;
    }
    DEADMAN.deadline_ns.store(at_ns, Ordering::Release);
    true
}

/// The tick's look: past a deadline, the panic.
fn look(deadman: &'static Deadman) {
    let at = deadman.deadline_ns.load(Ordering::Acquire);
    let now = time::boot_time_ns();
    if at == 0 || now < at {
        return;
    }
    panic!("deadman: {} s into the boot, past its deadline, and nothing called it off", now / NS_PER_SEC);
}

fn deadman(args: &str, out: &mut Output) {
    let mut words = args.split_whitespace();
    match (words.next(), words.next()) {
        (None, _) => {}
        (Some("off"), None) => {
            if DEADMAN.deadline_ns.swap(0, Ordering::AcqRel) != 0 {
                klog!(0, "deadman: called off");
            }
        }
        (Some(word), None) => {
            let Some(secs) = word.parse::<u64>().ok().filter(|s| (SECS_MIN..=SECS_MAX).contains(s)) else {
                let _ = writeln!(out, "{}: seconds, {} to {}", USAGE, SECS_MIN, SECS_MAX);
                return;
            };
            if !set(time::boot_time_ns().saturating_add(secs * NS_PER_SEC)) {
                let _ = writeln!(out, "deadman: no timer to be had, not set");
                return;
            }
            klog!(0, "deadman: a panic in {} s, unless `deadman off`", secs);
        }
        _ => {
            let _ = writeln!(out, "{}", USAGE);
            return;
        }
    }
    match DEADMAN.deadline_ns.load(Ordering::Acquire) {
        0 => {
            let _ = writeln!(out, "deadman: off");
        }
        at => {
            let left = at.saturating_sub(time::boot_time_ns()).div_ceil(NS_PER_SEC);
            let _ = writeln!(out, "deadman: a panic in {} s, {} s into the boot", left, at / NS_PER_SEC);
        }
    }
}
