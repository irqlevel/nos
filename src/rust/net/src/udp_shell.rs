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

/// What the receive path and the task share.
struct State {
    request: Request,
    nic: Option<Nic>,
    port: u16,
}

/// What a command printed, gathered for the reply. The task's own: it takes
/// this out of the shell when it starts and works on it as a local, so the
/// output of a command -- which may sleep, and run for seconds -- is never
/// written under a lock.
struct Reply {
    buf: Vec<u8>,
    truncated: bool,
}

impl Reply {
    fn clear(&mut self) {
        self.buf.clear();
        self.truncated = false;
    }

    fn collect(&mut self, piece: &[u8]) {
        let room = REPLY_CAP - self.buf.len();
        let take = piece.len().min(room);
        if take < piece.len() {
            self.truncated = true;
        }
        self.buf.extend_from_slice(&piece[..take]);
    }

    /// Stamp the marker if anything was dropped. There is by definition no
    /// room left to append it, so it goes over the tail of what did fit.
    fn finish(&mut self) {
        if !self.truncated || self.buf.len() < TRUNCATED.len() {
            return;
        }
        let at = self.buf.len() - TRUNCATED.len();
        self.buf[at..].copy_from_slice(TRUNCATED);
    }
}

pub struct UdpShell {
    state: SpinLock<State>,
    /// Signalled when a command is in, and by `stop`
    arrived: Event,
    /// The reply buffer, while no task has it: made once, so that a shell
    /// that cannot have one fails to start rather than to answer.
    reply: SpinLock<Option<Reply>>,
    /* Never dropped under a lock: giving a port back waits for the receive
     * path, and giving a task back waits for the task. */
    listener: SpinLock<Option<UdpListener>>,
    task: SpinLock<Option<TaskHandle>>,
    running: AtomicBool,
}

impl UdpShell {
    pub fn new() -> Option<UdpShell> {
        let mut reply = Vec::new();
        if reply.try_reserve_exact(REPLY_CAP).is_err() {
            trace!(0, "udpshell: no memory for the {} byte reply buffer", REPLY_CAP);
            return None;
        }

        Some(UdpShell {
            state: SpinLock::new(State {
                request: Request {
                    cmd: [0; CMD_MAX], len: 0, from_ip: 0, from_port: 0,
                    seq: [0; 4], ready: false,
                },
                nic: None,
                port: 0,
            })?,
            arrived: Event::new()?,
            reply: SpinLock::new(Some(Reply { buf: reply, truncated: false }))?,
            listener: SpinLock::new(None)?,
            task: SpinLock::new(None)?,
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

        {
            let mut state = self.state.lock();
            state.nic = Some(nic);
            state.port = port;
        }

        let task = match kcore::task::spawn_for("udpsh", self, UdpShell::run) {
            Some(task) => task,
            None => {
                self.running.store(false, Ordering::Release);
                return false;
            }
        };

        match nic.listen(port, self) {
            Ok(listener) => {
                *self.listener.lock() = Some(listener);
                *self.task.lock() = Some(task);
            }
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
        let listener = self.listener.lock().take();
        drop(listener);

        let task = self.task.lock().take();
        if let Some(task) = task {
            task.request_stop();
            self.arrived.signal();
            drop(task);
        }

        {
            let mut state = self.state.lock();
            state.nic = None;
            state.port = 0;
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

        let mut state = self.state.lock();
        let request = &mut state.request;
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
        let port = self.state.lock().port;
        udp::send(nic, arp, to_ip, to_port, nic.ip(), port,
            &buf[..HDR_LEN + payload.len()]);
    }

    fn run(&'static self) {
        /* The reply buffer is this task's for as long as it runs. */
        let mut reply = match self.reply.lock().take() {
            Some(reply) => reply,
            None => return,
        };

        self.serve(&mut reply);

        *self.reply.lock() = Some(reply);
    }

    fn serve(&self, reply: &mut Reply) {
        while !kcore::task::stopping() {
            let (cmd, len, to_ip, to_port, seq) = {
                let mut state = self.state.lock();
                let request = &mut state.request;
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

            reply.clear();
            kcore::cmd::dispatch(line, &mut |piece: &[u8]| reply.collect(piece));
            reply.finish();

            let nic = match self.state.lock().nic {
                Some(nic) => nic,
                None => continue,
            };
            let reply = &reply.buf;

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

/// What the receive path hands every datagram on the shell's port to.
impl kcore::net::UdpHandler for UdpShell {
    fn on_frame(&'static self, frame: kcore::net::Lent<'_>, _rx: &mut kcore::net::RxContext) {
        self.receive(frame.bytes());
    }
}
