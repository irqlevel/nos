//! A device of the cs's, as nbscs's engine is handed it: the device held
//! with a queue of its own -- claimed as `nbs-cs`, so that nothing else
//! writes it while the cs has it -- each request waited for in turn, its
//! future ready at its first poll (until N12c starts and reaps them).

use alloc::string::String;
use core::future::{ready, Future};

use kcore::block::{Disk, DiskQueue, Found};
use kcore::events::SharedEvent;
use kcore::time::Duration;
use nbscs::{Device, IoError};

/// The queue's one buffer: the most of a request, and a client's write's.
const BUFFER: usize = 256 * 1024;
/// A request not done in this long is the device failed.
const IO_TIMEOUT_MS: u64 = 30_000;

pub struct QueueDevice {
    q: DiskQueue,
}

impl QueueDevice {
    /// Device `ident` of this host's, held with a queue on `event`.
    pub fn open(ident: &str, event: &SharedEvent) -> Result<QueueDevice, String> {
        let disk = Disk::find_ident(ident).map_err(|f| match f {
            Found::Many(n) => alloc::format!("{} devices say they are {}: none is taken", n, ident),
            _ => alloc::format!("no device of this host's is {}", ident),
        })?;
        let q = disk.into_queue_on(event, true, "nbs-cs", 1, BUFFER)
            .map_err(|e| alloc::format!("{} not taken: {}", ident, e))?;
        Ok(QueueDevice { q })
    }

    fn timeout() -> Duration {
        Duration::from_millis(IO_TIMEOUT_MS)
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
        ready(self.q.read_wait(0, offset, buf, Self::timeout()).map_err(|_| IoError))
    }

    fn write<'a>(&'a self, offset: u64, data: &'a [u8]) -> impl Future<Output = Result<(), IoError>> + 'a {
        ready(self.q.write_wait(0, offset, data, Self::timeout()).map_err(|_| IoError))
    }

    fn flush(&self) -> impl Future<Output = Result<(), IoError>> + '_ {
        ready(self.q.flush().map_err(|_| IoError))
    }

    fn zero(&self, offset: u64, len: u64) -> impl Future<Output = Result<(), IoError>> + '_ {
        ready(self.zero_now(offset, len))
    }
}

impl QueueDevice {
    fn zero_now(&self, offset: u64, len: u64) -> Result<(), IoError> {
        /* The buffer zeroed once, then written over and over: nothing else
           lends it meanwhile */
        {
            let mut lent = self.q.lend(0).ok_or(IoError)?;
            lent.fill(0);
        }
        let mut at = 0u64;
        while at < len {
            let n = (len - at).min(self.q.buffer_len() as u64) as usize;
            self.q.write_wait_lent(0, offset + at, n, Self::timeout()).map_err(|_| IoError)?;
            at += n as u64;
        }
        Ok(())
    }
}
