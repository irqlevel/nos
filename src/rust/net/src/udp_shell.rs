//! The shell over UDP: a datagram carries a command line in, and what the
//! command printed comes back in as many datagrams as it takes.
//!
//! This is the only console some machines have -- the Hetzner boxes have no
//! serial port -- so the things it is careful about are not niceties. A reply
//! that did not fit says so, because a silently short one reads as a command
//! that simply printed less. The datagrams of a long reply are paced, because
//! the narrowest queue between here and the client passes the first few of an
//! unpaced burst and drops the rest without a word. And the task waits on an
//! event rather than looking every so often: polling kept it runnable, and
//! under a flood it took a fifth of the CPU that the receive softirq runs on.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use kcore::net::{Nic, UdpListener};
use kcore::sync::{Event, SpinLock};
use kcore::task::TaskHandle;
use kcore::trace;

use crate::abi;
use crate::udp;

/// "NOSH", at the head of every datagram either way.
pub const MAGIC: u32 = 0x4E4F_5348;
/// On the last datagram of a reply.
pub const FLAG_LAST: u16 = 0x0001;

/// magic(4) + seq(4) + chunk(2) + flags(2) + payload len(2) + reserved(2)
pub const HDR_LEN: usize = 16;

const HDR_MAGIC: usize = 0;
const HDR_SEQ: usize = 4;
const HDR_CHUNK: usize = 8;
const HDR_FLAGS: usize = 10;
const HDR_PAYLOAD_LEN: usize = 12;

/// What one reply datagram carries, after the header.
const CHUNK_LEN: usize = 1384;

/// One buffer for the life of the shell, on the heap: a `ps` on a twenty-CPU
/// box or a `pci` on a server is tens of kilobytes, and the task's whole
/// stack is 32 KiB.
const REPLY_CAP: usize = 32 * 1024;

/// What a command line fits in.
const CMD_MAX: usize = 256;

/// The gap between two reply datagrams.
const PACE_MS: u64 = 1;

const TRUNCATED: &[u8] = b"\n[output truncated]\n";

struct Request {
    cmd: [u8; CMD_MAX],
    len: usize,
    from_ip: u32,
    from_port: u16,
    /// Echoed back exactly as it arrived, in the order it arrived in
    seq: [u8; 4],
    ready: bool,
}

pub struct UdpShell {
    lock: SpinLock,
    request: core::cell::UnsafeCell<Request>,
    /// Signalled when a command is in, and by `stop`
    arrived: Event,

    reply: core::cell::UnsafeCell<Vec<u8>>,
    truncated: core::cell::UnsafeCell<bool>,

    nic: core::cell::UnsafeCell<Option<Nic>>,
    port: core::cell::UnsafeCell<u16>,
    listener: core::cell::UnsafeCell<Option<UdpListener>>,
    task: core::cell::UnsafeCell<Option<TaskHandle>>,
    running: AtomicBool,
}

/* Everything inside is touched with the lock held, or by the one task */
unsafe impl Sync for UdpShell {}
unsafe impl Send for UdpShell {}

impl UdpShell {
    pub fn new() -> Option<UdpShell> {
        let mut reply = Vec::new();
        if reply.try_reserve_exact(REPLY_CAP).is_err() {
            trace!(0, "udpshell: no memory for the {} byte reply buffer", REPLY_CAP);
            return None;
        }

        Some(UdpShell {
            lock: SpinLock::new()?,
            request: core::cell::UnsafeCell::new(Request {
                cmd: [0; CMD_MAX], len: 0, from_ip: 0, from_port: 0,
                seq: [0; 4], ready: false,
            }),
            arrived: Event::new()?,
            reply: core::cell::UnsafeCell::new(reply),
            truncated: core::cell::UnsafeCell::new(false),
            nic: core::cell::UnsafeCell::new(None),
            port: core::cell::UnsafeCell::new(0),
            listener: core::cell::UnsafeCell::new(None),
            task: core::cell::UnsafeCell::new(None),
            running: AtomicBool::new(false),
        })
    }

    /// Start on `port`. False when one is running already, or the device's
    /// listener table has no room -- which is a real outcome, since DHCP and
    /// DNS have taken slots by now, and one that used to be invisible: the
    /// task ran and said "started" while nothing was ever dispatched to it.
    pub fn start(&'static self, nic: Nic, port: u16) -> bool {
        if port == 0 || self.running.swap(true, Ordering::AcqRel) {
            return false;
        }

        unsafe {
            *self.nic.get() = Some(nic);
            *self.port.get() = port;
        }

        let task = match kcore::task::spawn_with_ctx(
            "udpsh", run, self as *const _ as *mut u8)
        {
            Some(task) => task,
            None => {
                self.running.store(false, Ordering::Release);
                return false;
            }
        };

        match nic.listen_udp(port, on_datagram, self as *const _ as *mut u8) {
            Ok(listener) => unsafe {
                *self.listener.get() = Some(listener);
                *self.task.get() = Some(task);
            },
            Err(err) => {
                trace!(0, "udpshell: port {} could not be listened on ({:?})", port, err);
                task.request_stop();
                self.arrived.signal();
                drop(task);
                self.running.store(false, Ordering::Release);
                return false;
            }
        }

        trace!(0, "udpshell: started on port {}", port);
        true
    }

    pub fn stop(&self) {
        /* Off the port first, so nothing new arrives for a task that is
         * leaving */
        unsafe { *self.listener.get() = None };

        let task = unsafe { (*self.task.get()).take() };
        if let Some(task) = task {
            task.request_stop();
            self.arrived.signal();
            drop(task);
        }

        unsafe {
            *self.nic.get() = None;
            *self.port.get() = 0;
        }
        self.running.store(false, Ordering::Release);
    }

    /* ---- receiving ---- */

    /// A datagram on the shell's port, from the receive softirq.
    fn receive(&self, frame: &[u8]) {
        let datagram = match udp::parse(frame) {
            Some(datagram) => datagram,
            None => return,
        };

        let payload = datagram.payload;
        if payload.len() < HDR_LEN {
            return;
        }
        if crate::wire::be32(payload, HDR_MAGIC) != MAGIC {
            return;
        }

        let declared = crate::wire::be16(payload, HDR_PAYLOAD_LEN) as usize;
        if declared > payload.len() - HDR_LEN {
            return;
        }

        let _guard = self.lock.lock();
        let request = unsafe { &mut *self.request.get() };
        if request.ready {
            /* The one before it has not been run yet */
            return;
        }

        let len = declared.min(CMD_MAX);
        request.cmd[..len].copy_from_slice(&payload[HDR_LEN..HDR_LEN + len]);
        request.len = len;
        request.from_ip = datagram.src_ip;
        request.from_port = datagram.src_port;
        request.seq.copy_from_slice(&payload[HDR_SEQ..HDR_SEQ + 4]);
        request.ready = true;

        /* The task is waiting for this -- or, busy with the command before
         * it, finds the signal when it next waits and goes round again. */
        self.arrived.signal();
    }

    /* ---- replying ---- */

    /// What a command printed, gathered into the reply buffer.
    fn collect(&self, piece: &[u8]) {
        let reply = unsafe { &mut *self.reply.get() };
        let room = REPLY_CAP - reply.len();
        if room == 0 {
            unsafe { *self.truncated.get() = true };
            return;
        }

        let take = piece.len().min(room);
        if take < piece.len() {
            unsafe { *self.truncated.get() = true };
        }
        reply.extend_from_slice(&piece[..take]);
    }

    /// Stamp the marker if anything was dropped. There is by definition no
    /// room left to append it, so it goes over the tail of what did fit.
    fn finish(&self) {
        if !unsafe { *self.truncated.get() } {
            return;
        }

        let reply = unsafe { &mut *self.reply.get() };
        if reply.len() < TRUNCATED.len() {
            return;
        }
        let at = reply.len() - TRUNCATED.len();
        reply[at..].copy_from_slice(TRUNCATED);
    }

    /// One reply datagram: the header, then this much of the output.
    fn send_chunk(
        &self, nic: &Nic, to_ip: u32, to_port: u16, seq: &[u8; 4], chunk: u16, last: bool,
        payload: &[u8],
    ) {
        let mut buf = [0u8; HDR_LEN + CHUNK_LEN];
        crate::wire::set_be32(&mut buf, HDR_MAGIC, MAGIC);
        buf[HDR_SEQ..HDR_SEQ + 4].copy_from_slice(seq);
        crate::wire::set_be16(&mut buf, HDR_CHUNK, chunk);
        crate::wire::set_be16(&mut buf, HDR_FLAGS, if last { FLAG_LAST } else { 0 });
        crate::wire::set_be16(&mut buf, HDR_PAYLOAD_LEN, payload.len() as u16);
        buf[HDR_LEN..HDR_LEN + payload.len()].copy_from_slice(payload);

        let arp = match abi::arp_table() {
            Some(arp) => arp,
            None => return,
        };
        let port = unsafe { *self.port.get() };
        udp::send(nic, arp, to_ip, to_port, nic.ip(), port,
            &buf[..HDR_LEN + payload.len()]);
    }

    fn run(&self) {
        while !kcore::task::stopping() {
            let (cmd, len, to_ip, to_port, seq) = {
                let _guard = self.lock.lock();
                let request = unsafe { &mut *self.request.get() };
                if !request.ready {
                    (None, 0, 0, 0, [0u8; 4])
                } else {
                    request.ready = false;
                    (Some(request.cmd), request.len, request.from_ip,
                     request.from_port, request.seq)
                }
            };

            let cmd = match cmd {
                Some(cmd) => cmd,
                None => {
                    self.arrived.wait();
                    continue;
                }
            };

            /* Whatever line ending the client used is not part of the
             * command */
            let mut len = len;
            while len > 0 && (cmd[len - 1] == b'\n' || cmd[len - 1] == b'\r') {
                len -= 1;
            }
            if len == 0 {
                continue;
            }

            let line = match core::str::from_utf8(&cmd[..len]) {
                Ok(line) => line,
                Err(_) => {
                    trace!(0, "udpshell: a command that is not text, dropped");
                    continue;
                }
            };

            trace!(0, "udpshell: cmd '{}' from {}.{}.{}.{}:{}", line,
                (to_ip >> 24) & 0xFF, (to_ip >> 16) & 0xFF,
                (to_ip >> 8) & 0xFF, to_ip & 0xFF, to_port);

            unsafe {
                (*self.reply.get()).clear();
                *self.truncated.get() = false;
            }
            kcore::cmd::dispatch(line, &mut |piece: &[u8]| self.collect(piece));
            self.finish();

            let nic = match unsafe { *self.nic.get() } {
                Some(nic) => nic,
                None => continue,
            };

            /* The reply is read here and nothing else writes it until the
             * next command, which this same task takes. */
            let reply = unsafe { &*self.reply.get() };

            if reply.is_empty() {
                /* Nothing printed: a header alone, flagged last, so the
                 * client knows the command is done rather than lost. */
                self.send_chunk(&nic, to_ip, to_port, &seq, 0, true, &[]);
                continue;
            }

            let mut at = 0;
            let mut chunk = 0u16;
            while at < reply.len() {
                let take = (reply.len() - at).min(CHUNK_LEN);
                let last = at + take == reply.len();
                self.send_chunk(&nic, to_ip, to_port, &seq, chunk, last,
                    &reply[at..at + take]);

                at += take;
                chunk = chunk.wrapping_add(1);

                /* Paced for the reason the netconsole drain is: a reply can
                 * be twenty-odd datagrams, and an unpaced burst that size
                 * loses all but the first few somewhere on the way. */
                if at < reply.len() {
                    kcore::task::sleep_ms(PACE_MS);
                }
            }
        }
    }
}

/// The task the shell runs in.
extern "C" fn run(ctx: *mut u8) {
    if ctx.is_null() {
        return;
    }
    let shell = unsafe { &*(ctx as *const UdpShell) };
    shell.run();
}

/// The frame listener: what the receive softirq hands every datagram on the
/// shell's port.
extern "C" fn on_datagram(ctx: *mut u8, frame: usize) {
    if ctx.is_null() {
        return;
    }
    let shell = unsafe { &*(ctx as *const UdpShell) };
    let bytes = unsafe { kcore::net::NetFrame::lent(frame) };
    shell.receive(bytes);
}
