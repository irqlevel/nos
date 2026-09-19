//! A device, as this crate's services see one: DHCP, DNS, the shell over
//! UDP, netconsole, the load target, the HTTP client and the protocols under
//! them.
//!
//! These are the names `kcore::net` has -- `Nic`, `UdpHandler`, `Lent`,
//! `UdpListener` -- because the services were written against those. But
//! that is the network layer as a loadable module reaches it, across the C
//! ABI, and the services are *in* the layer: every `nic.ip()` was a call out
//! through kcore and back into `device.rs`, a handle looked up again at the
//! far end. These call the device.

use core::ops::Deref;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::device::{Device, DEVICES};
use crate::frame::{Frame, FrameQueue};

pub use crate::device::{Lent, ListenError, RxContext, RxOwned, UdpHandler};

/// A network device of the table -- `eth0` -- for a service that sends and
/// receives over it rather than drives it. Devices live as long as the
/// kernel does, so a Nic holds nothing but where one is and copies freely;
/// everything a `Device` answers, it answers.
#[derive(Clone, Copy)]
pub struct Nic(&'static Device);

impl Deref for Nic {
    type Target = Device;

    fn deref(&self) -> &Device {
        self.0
    }
}

impl Nic {
    pub(crate) fn of(dev: &'static Device) -> Nic {
        Nic(dev)
    }

    pub fn find(name: &str) -> Option<Nic> {
        DEVICES.find(name.as_bytes()).map(Nic)
    }

    /// The device a word from outside names -- a module's `kcore::net::Nic`,
    /// handed across the C ABI: one of the table's, or none.
    pub fn from_handle(handle: usize) -> Option<Nic> {
        DEVICES.by_handle(handle).map(Nic)
    }

    pub fn device(&self) -> &'static Device {
        self.0
    }

    /// Every UDP datagram to `port`, handed to `handler` -- something that
    /// lives for good, a service's one instance -- from the receive softirq.
    /// Refused for a port someone else has. The listener goes with the
    /// returned handle, once any call still running returns.
    ///
    /// The handler runs on the receive path of every packet the machine gets:
    /// nothing that sleeps, and nothing long.
    pub fn listen<H: UdpHandler>(
        &self, port: u16, handler: &'static H,
    ) -> Result<UdpListener, ListenError> {
        let key = self.0.listen_handler(port, handler, false)?;
        Ok(UdpListener { dev: self.0, port, key })
    }

    /// As `listen`, with `UdpHandler::on_batch_end` called at the end of each
    /// receive batch. A listener that answers from the receive path builds
    /// its replies as the frames arrive and hands them to the NIC there --
    /// one lock and one doorbell for the batch, rather than one of each per
    /// packet.
    pub fn listen_batched<H: UdpHandler>(
        &self, port: u16, handler: &'static H,
    ) -> Result<UdpListener, ListenError> {
        let key = self.0.listen_handler(port, handler, true)?;
        Ok(UdpListener { dev: self.0, port, key })
    }

    /// Queues a frame to transmit; false when the queue had no room and it
    /// was dropped.
    pub fn transmit(&self, frame: Frame) -> bool {
        self.0.count_tx(&frame);
        let mut frames = FrameQueue::new();
        frames.push(frame);
        self.0.submit_tx(frames) == 1
    }
}

/// A UDP port listened on, from `Nic::listen`; given back on drop, once no
/// call of its handler is still running. Task context: the drop may wait.
pub struct UdpListener {
    dev: &'static Device,
    port: u16,
    /// What takes away this listener and nobody else's on the port
    key: usize,
}

impl Drop for UdpListener {
    fn drop(&mut self) {
        self.dev.unlisten_udp(self.port, self.key);
    }
}

/// A device that can be set and cleared without a lock: what a receive path
/// reads once a packet. Only a `Nic` ever goes in, so only a `Nic` comes out
/// -- looked up in the table on the way, like any word.
pub struct AtomicNic(AtomicUsize);

impl AtomicNic {
    pub const fn none() -> Self {
        Self(AtomicUsize::new(0))
    }

    pub fn set(&self, nic: Option<Nic>) {
        self.0.store(nic.map_or(0, |nic| nic.0.handle()), Ordering::Release);
    }

    #[inline]
    pub fn get(&self) -> Option<Nic> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            handle => Nic::from_handle(handle),
        }
    }
}

/// What a listener that answers from the receive path gathers its replies
/// in: at most `N`, sent with one lock and one doorbell.
pub struct TxBatch<const N: usize> {
    frames: FrameQueue,
}

impl<const N: usize> TxBatch<N> {
    pub const fn new() -> Self {
        Self { frames: FrameQueue::new() }
    }

    pub fn is_full(&self) -> bool {
        self.frames.len() >= N
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Takes the frame. False, and the frame released, when there is no room.
    pub fn push(&mut self, frame: Frame) -> bool {
        if self.is_full() {
            return false;
        }
        self.frames.push(frame);
        true
    }

    /// Everything gathered, released unsent.
    pub fn clear(&mut self) {
        drop(self.frames.take());
    }

    /// Everything gathered, to the device: how many it queued. The rest it
    /// releases, and the batch is empty either way.
    pub fn send(&mut self, nic: &Nic) -> usize {
        /* Counted by what they carry on the way, as every sender's are. */
        let mut gathered = self.frames.take();
        let mut counted = FrameQueue::new();
        while let Some(frame) = gathered.pop() {
            nic.count_tx(&frame);
            counted.push(frame);
        }
        nic.submit_tx(counted)
    }
}
