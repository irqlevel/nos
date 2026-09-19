//! A device, as this crate's services see one: DHCP, DNS, the shell over
//! UDP, netconsole, the HTTP client and the protocols under them.
//!
//! These are the names `kcore::net` has -- `Nic`, `UdpHandler`, `Lent`,
//! `UdpListener` -- because the services were written against those. But
//! that is the network layer as a loadable module reaches it, across the C
//! ABI, and the services are *in* the layer: every `nic.ip()` was a call out
//! through kcore and back into `device.rs`, a handle looked up again at the
//! far end. These call the device.

use core::ops::Deref;

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
        let key = self.0.listen_handler(port, handler)?;
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
