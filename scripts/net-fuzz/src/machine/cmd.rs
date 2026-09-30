//! The shell's command table, as `kernel/cmd.cpp` keeps it: what `kcore::cmd`
//! registers -- the network layer's commands, a module's -- and what the UDP
//! shell and an SSH session dispatch a line to. A command's output goes to
//! a printer, here a sink and maybe a source, the dispatcher's.
//!
//! The kernel's built-in commands are C++ and are not here; in their place a
//! few of the fuzzer's own, each standing for a kind of command a session
//! runs: one that prints a line, one that prints a lot, one that runs a
//! while, one that reads what is typed at it.

use std::sync::Mutex;

use super::sched;

struct Entry {
    name: Vec<u8>,
    handler: ffi::cmd::CmdHandler,
    ctx: usize,
    /// Calls of it running now: what an unregister waits out.
    running: usize,
    alive: bool,
}

static TABLE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

fn table() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

/// How many commands the kernel's table has room for, dynamic ones.
const DYNAMIC_MAX: usize = 128;

pub fn register(name: Vec<u8>, _help: Vec<u8>, handler: ffi::cmd::CmdHandler, ctx: usize) -> usize {
    let mut t = table();
    if name.is_empty() || t.iter().any(|e| e.alive && e.name == name) || t.iter().filter(|e| e.alive).count() >= DYNAMIC_MAX {
        return 0;
    }
    t.push(Entry { name, handler, ctx, running: 0, alive: true });
    t.len()
}

/// Takes it away, once no call of it is running.
pub fn unregister(handle: usize) {
    {
        let mut t = table();
        match handle.checked_sub(1).and_then(|i| t.get_mut(i)) {
            Some(e) if e.alive => e.alive = false,
            _ => panic!("invariant: command {} unregistered that is not registered", handle),
        }
    }
    loop {
        let running = table()[handle - 1].running;
        if running == 0 {
            return;
        }
        sched::sleep_ns(1_000_000);
    }
}

/// Where a command's output goes, and what is typed at it comes from: the
/// word `kcore::cmd::Output` carries is the address of one of these.
struct Printer {
    sink: ffi::cmd::CmdSink,
    source: Option<ffi::cmd::CmdSource>,
    ctx: usize,
}

pub fn printer_write(out: usize, bytes: &[u8]) {
    // SAFETY: a printer is only ever the address of the one `dispatch`
    // made, which lives until the command returns -- and a command writes
    // to its printer only while it runs.
    let p = unsafe { &*(out as *const Printer) };
    /* The dispatcher's sink: the kernel's code (the UDP shell's, a
     * session's), however deep inside the fuzzer's this is called. */
    // SAFETY: the sink and its context are the dispatcher's, alive for
    // the call.
    sched::kernel(|| unsafe { (p.sink)(p.ctx as *mut core::ffi::c_void, bytes.as_ptr(), bytes.len()) })
}

pub fn printer_read(out: usize, buf: &mut [u8], timeout_ns: u64) -> isize {
    // SAFETY: as in `printer_write`.
    let p = unsafe { &*(out as *const Printer) };
    match p.source {
        // SAFETY: the source and its context are the dispatcher's.
        Some(source) => sched::kernel(|| unsafe {
            source(p.ctx as *mut core::ffi::c_void, buf.as_mut_ptr(), buf.len(), timeout_ns)
        }),
        None => -1,
    }
}

/// Runs a command line: its first word the command, the rest its
/// arguments, as cmd.cpp splits it.
pub fn dispatch(line: &[u8], sink: ffi::cmd::CmdSink, source: Option<ffi::cmd::CmdSource>, ctx: usize) {
    let printer = Printer { sink, source, ctx };
    let out = &printer as *const Printer as usize;
    let text = String::from_utf8_lossy(line).into_owned();
    let trimmed = text.trim_start();
    let (name, args) = match trimmed.find(char::is_whitespace) {
        Some(at) => (&trimmed[..at], trimmed[at..].trim_start()),
        None => (trimmed, ""),
    };
    if name.is_empty() {
        return;
    }

    let found = sched::harness(|| {
        let mut t = table();
        t.iter_mut().enumerate().find(|(_, e)| e.alive && e.name == name.as_bytes()).map(|(i, e)| {
            e.running += 1;
            (i, e.handler, e.ctx)
        })
    });
    match found {
        Some((i, handler, hctx)) => {
            /* The command is the kernel's code, as the dispatcher is. */
            // SAFETY: the handler and its context were registered together,
            // and the entry is alive while `running` is not 0.
            sched::kernel(|| unsafe {
                handler(hctx as *mut core::ffi::c_void, args.as_ptr(), args.len(), out as *mut core::ffi::c_void)
            });
            sched::harness(|| table()[i].running -= 1);
        }
        None => builtin(name, args, out),
    }
}

/// A command line run as the console runs one, on the task this is called
/// from: what it printed.
pub fn run(line: &str) -> String {
    unsafe extern "C" fn sink(ctx: *mut core::ffi::c_void, buf: *const u8, len: usize) {
        // SAFETY: `ctx` is the buffer `run` made, alive until the dispatch
        // returns, and `buf` is `len` bytes of the command's output.
        let (out, bytes) = unsafe { (&*(ctx as *const Mutex<Vec<u8>>), std::slice::from_raw_parts(buf, len)) };
        sched::harness(|| out.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(bytes));
    }
    let out: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    dispatch(line.as_bytes(), sink, None, &out as *const Mutex<Vec<u8>> as usize);
    let bytes = out.into_inner().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The fuzzer's stand-ins for the kernel's built-in commands.
fn builtin(name: &str, args: &str, out: usize) {
    let say = |s: &[u8]| printer_write(out, s);
    match name {
        /* A line of output. */
        "echo" => {
            say(args.as_bytes());
            say(b"\n");
        }
        /* A lot of output, a piece at a time: past a UDP reply's room, past
         * an SSH client's window. */
        "spew" => {
            let n: usize = args.split_whitespace().next().and_then(|a| a.parse().ok()).unwrap_or(4096).min(1 << 20);
            let mut line = [0u8; 97];
            let mut done = 0;
            let mut k = 0u32;
            while done < n {
                for (i, b) in line.iter_mut().enumerate() {
                    *b = b'a' + ((k as usize + i) % 26) as u8;
                }
                line[96] = b'\n';
                let take = (n - done).min(line.len());
                say(&line[..take]);
                done += take;
                k = k.wrapping_add(1);
            }
        }
        /* A command that runs for a while, printing nothing. */
        "nap" => {
            let ms: u64 = args.split_whitespace().next().and_then(|a| a.parse().ok()).unwrap_or(50).min(600_000);
            sched::harness(|| sched::sleep_ns(ms * 1_000_000));
            say(b"awake\n");
        }
        /* What is typed at it, back, until it has had `n` bytes or nothing
         * more comes: a command that reads, as `hv attach` does. */
        "type" => {
            let n: usize = args.split_whitespace().next().and_then(|a| a.parse().ok()).unwrap_or(16).min(65536);
            let mut got = 0;
            let mut quiet = 0;
            let mut buf = [0u8; 64];
            /* Three waits with nothing and it gives up, as a person at the
             * other end would be expected to have typed by then. */
            while got < n && quiet < 3 {
                let want = (n - got).min(buf.len());
                match printer_read(out, &mut buf[..want], 20_000_000) {
                    k if k > 0 => {
                        say(&buf[..k as usize]);
                        got += k as usize;
                        quiet = 0;
                    }
                    0 => quiet += 1,
                    _ => break,
                }
            }
            say(b"\n");
        }
        _ => {
            say(b"unknown command: ");
            say(name.as_bytes());
            say(b"\n");
        }
    }
}
