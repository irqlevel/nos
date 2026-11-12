//! A device of the cs's, as nbscs's engine is handed it: the device held
//! with a queue of its own -- claimed as `nbs-cs`, so that nothing else
//! writes it while the cs has it -- each request started on one of the
//! queue's buffers and ended by that buffer's reap (N12c), as many in flight
//! as there are buffers. A request longer than a buffer goes a buffer's
//! worth at a time; a flush is started and reaped as a write is.
//!
//! Nothing here waits: a request polled reaps its queue and is pending
//! until its buffer comes back, the cs's task asleep on the one event its
//! queues and its mailbox signal (`serve.rs`). Every buffer freed or
//! reaped is counted in the cs's moves, which its loop polls until they
//! stay put: a request that waited for a buffer is polled again once one
//! is free.

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use kcore::block::{Disk, DiskQueue, Found, Started};
use kcore::events::SharedEvent;
use kcore::time::boot_time_ns;
use nbscs::{Device, IoError};

/// The queue's buffers, and each one's bytes: a request's most at once, and
/// a client's write's.
const BUFFERS: usize = 8;
const BUFFER: usize = 256 * 1024;
/// A request not done in this long is the device failed: its buffer the
/// device's until its reap all the same.
const IO_TIMEOUT_NS: u64 = 30_000 * 1_000_000;

/// What a buffer is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Buf {
    Free,
    /// Its request out, since then.
    Out { since: u64 },
    /// Its request done: well or not.
    Done(bool),
    /// Its request out and nobody's any more -- its future dropped, or its
    /// time up: free once its reap comes, and not before.
    Left,
}

pub struct QueueDevice {
    q: DiskQueue,
    bufs: RefCell<Vec<Buf>>,
    /// The cs's moves: a buffer freed or reaped counted there.
    moves: Rc<Cell<u64>>,
}

impl QueueDevice {
    /// Device `ident` of this host's, held with a queue on `event`, its
    /// buffers' moves counted in `moves`.
    pub fn open(ident: &str, event: &SharedEvent, moves: Rc<Cell<u64>>) -> Result<QueueDevice, String> {
        let disk = Disk::find_ident(ident).map_err(|f| match f {
            Found::Many(n) => alloc::format!("{} devices say they are {}: none is taken", n, ident),
            _ => alloc::format!("no device of this host's is {}", ident),
        })?;
        let q = disk.into_queue_on(event, true, "nbs-cs", BUFFERS, BUFFER)
            .map_err(|e| alloc::format!("{} not taken: {}", ident, e))?;
        let bufs = vec![Buf::Free; q.buffers()];
        Ok(QueueDevice { q, bufs: RefCell::new(bufs), moves })
    }

    fn moved(&self) {
        self.moves.set(self.moves.get().wrapping_add(1));
    }

    /// Every request the device has done, its buffer's request told -- or
    /// the buffer freed, where its request is nobody's.
    fn reap(&self) {
        let mut bufs = self.bufs.borrow_mut();
        let n = self.q.reap(|k, ok| {
            if let Some(b) = bufs.get_mut(k) {
                *b = if *b == Buf::Left { Buf::Free } else { Buf::Done(ok) };
            }
        });
        drop(bufs);
        if n != 0 {
            self.moved();
        }
    }

    /// A free buffer, taken for a request: None while every one is out.
    fn take(&self) -> Option<usize> {
        let mut bufs = self.bufs.borrow_mut();
        let k = bufs.iter().position(|b| *b == Buf::Free)?;
        bufs[k] = Buf::Out { since: boot_time_ns() };
        Some(k)
    }

    fn set(&self, k: usize, b: Buf) {
        if let Some(cur) = self.bufs.borrow_mut().get_mut(k) {
            *cur = b;
        }
        if b == Buf::Free {
            self.moved();
        }
    }

    fn state(&self, k: usize) -> Buf {
        self.bufs.borrow().get(k).copied().unwrap_or(Buf::Free)
    }
}

/// What a request does.
enum Kind<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
    Flush,
}

impl Kind<'_> {
    fn len(&self) -> usize {
        match self {
            Kind::Read(b) => b.len(),
            Kind::Write(d) => d.len(),
            Kind::Flush => 0,
        }
    }
}

/// A request of the device's, a buffer's worth at a time: ready once every
/// piece is done -- or one failed, or its time ran out.
struct Request<'a> {
    dev: &'a QueueDevice,
    kind: Kind<'a>,
    offset: u64,
    /// Bytes done; the piece out, its buffer and its bytes; and, for a
    /// flush, whether it was asked.
    done: usize,
    out: Option<(usize, usize)>,
    flush_asked: bool,
}

impl Request<'_> {
    /// The next piece started: Ready where it cannot be, Pending started or
    /// with every buffer out.
    fn start(&mut self) -> Poll<Result<(), IoError>> {
        let Some(k) = self.dev.take() else { return Poll::Pending };
        let n = (self.kind.len() - self.done).min(self.dev.q.buffer_len());
        if let Kind::Write(d) = &self.kind {
            let Some(mut lent) = self.dev.q.lend(k) else {
                self.dev.set(k, Buf::Free);
                return Poll::Ready(Err(IoError));
            };
            lent[..n].copy_from_slice(&d[self.done..self.done + n]);
        }
        let at = self.offset + self.done as u64;
        let started = match self.kind {
            Kind::Read(_) => self.dev.q.start(k, false, at, n),
            Kind::Write(_) => self.dev.q.start(k, true, at, n),
            Kind::Flush => {
                self.flush_asked = true;
                self.dev.q.start_flush(k)
            }
        };
        match started {
            Ok(Started::InFlight) => {
                self.dev.q.kick();
                self.out = Some((k, n));
                Poll::Pending
            }
            /* Done on the spot -- a device with no asynchronous path */
            Ok(Started::Done) => {
                self.dev.set(k, Buf::Done(true));
                self.out = Some((k, n));
                Poll::Pending
            }
            Err(_) => {
                self.dev.set(k, Buf::Free);
                Poll::Ready(Err(IoError))
            }
        }
    }
}

impl Future for Request<'_> {
    type Output = Result<(), IoError>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        this.dev.reap();
        loop {
            let Some((k, n)) = this.out else {
                let left = this.kind.len() - this.done;
                if left == 0 && (!matches!(this.kind, Kind::Flush) || this.flush_asked) {
                    return Poll::Ready(Ok(()));
                }
                match this.start() {
                    Poll::Ready(r) => return Poll::Ready(r),
                    /* Started: looked at below -- done on the spot, it is */
                    Poll::Pending if this.out.is_some() => continue,
                    /* Every buffer out: polled again once one is freed */
                    Poll::Pending => return Poll::Pending,
                }
            };
            match this.dev.state(k) {
                Buf::Done(ok) => {
                    if ok {
                        if let Kind::Read(buf) = &mut this.kind {
                            let Some(lent) = this.dev.q.lend(k) else {
                                this.dev.set(k, Buf::Free);
                                this.out = None;
                                return Poll::Ready(Err(IoError));
                            };
                            buf[this.done..this.done + n].copy_from_slice(&lent[..n]);
                        }
                    }
                    this.dev.set(k, Buf::Free);
                    this.out = None;
                    if !ok {
                        return Poll::Ready(Err(IoError));
                    }
                    this.done += n;
                }
                Buf::Out { since } if boot_time_ns().saturating_sub(since) > IO_TIMEOUT_NS => {
                    /* Its time up: the device failed; its buffer the
                       device's until the reap comes */
                    this.dev.set(k, Buf::Left);
                    this.out = None;
                    return Poll::Ready(Err(IoError));
                }
                _ => return Poll::Pending,
            }
        }
    }
}

impl Drop for Request<'_> {
    fn drop(&mut self) {
        /* A piece out and nobody waiting for it: its buffer freed by its
           reap -- or now, where it is done already */
        if let Some((k, _)) = self.out.take() {
            let b = match self.dev.state(k) {
                Buf::Done(_) => Buf::Free,
                _ => Buf::Left,
            };
            self.dev.set(k, b);
        }
    }
}

impl Device for QueueDevice {
    fn bytes(&self) -> u64 {
        self.q.size()
    }

    fn sector(&self) -> u64 {
        self.q.disk().sector_size()
    }

    fn read<'a>(&'a self, offset: u64, buf: &'a mut [u8]) -> impl Future<Output = Result<(), IoError>> + 'a {
        Request { dev: self, kind: Kind::Read(buf), offset, done: 0, out: None, flush_asked: false }
    }

    fn write<'a>(&'a self, offset: u64, data: &'a [u8]) -> impl Future<Output = Result<(), IoError>> + 'a {
        Request { dev: self, kind: Kind::Write(data), offset, done: 0, out: None, flush_asked: false }
    }

    fn flush(&self) -> impl Future<Output = Result<(), IoError>> + '_ {
        Request { dev: self, kind: Kind::Flush, offset: 0, done: 0, out: None, flush_asked: false }
    }

    /* A buffer's worth of zeros a request: a slot's 4 MiB in sixteen */
    fn zero(&self, offset: u64, len: u64) -> impl Future<Output = Result<(), IoError>> + '_ {
        async move {
            let mut zeros = Vec::new();
            zeros.try_reserve_exact((len as usize).min(BUFFER)).map_err(|_| IoError)?;
            zeros.resize((len as usize).min(BUFFER), 0);
            let mut at = 0u64;
            while at < len {
                let n = (len - at).min(zeros.len() as u64) as usize;
                self.write(offset + at, &zeros[..n]).await?;
                at += n as u64;
            }
            Ok(())
        }
    }
}
