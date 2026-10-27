//! A virtio network device, legacy PCI: a guest's `eth0`, over whatever
//! carries its frames (`Backend`) -- the module's switch.
//!
//! Two queues, as legacy has them: 0 the driver fills with buffers for what
//! the device receives, 1 with what it sends. Every frame either way comes
//! after the header legacy puts in front of it -- ten bytes, or twelve once
//! the driver takes mergeable buffers. What the guest sends may leave its
//! checksum to the device, and TCP over IPv4 may come as one send of up to
//! 64 KiB (`offload`): the backend carries such a send whole to a guest that
//! takes it so -- `GUEST_TSO4`, spread over as many of its buffers as it
//! needs -- and cuts it into frames for anyone else. A send is copied out and
//! handed to the backend before the notify returns; what the backend has for the guest is put into
//! its buffers each time round the run loop (`poll`) -- one frame held by the
//! device when the driver has posted none, so nothing is lost for want of a
//! buffer that is on its way.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::offload::{self, Offloads, Partial, Rx, Send as Sent, Takes, Tcp4, TxHeader};
use super::pci::Identity;
use super::virtio::{self, Asked, Broken, Queue, Raise, Room, Seg, Transport};
use crate::memory::GuestMemory;
use crate::{Error, Result};

/// What carries a guest's frames: where what it sends goes, and where what
/// it is to receive comes from.
pub trait Backend: Send {
    /// A frame the guest sent, Ethernet header first.
    fn send(&mut self, frame: &[u8]);
    /// A frame whose checksum the guest left to finish: handed on so to a
    /// guest that takes checksums partial, and finished for anyone else --
    /// which the default does, then `send`.
    fn send_partial(&mut self, frame: &mut [u8], partial: Partial) {
        partial.complete(frame);
        self.send(frame);
    }
    /// A send of TCP over IPv4 the guest left to be cut, and that held up
    /// (`offload::check_tcp4`): carried whole to a guest that takes it so,
    /// and cut into frames for anyone else -- which the default does, a
    /// frame at a time through `send`.
    fn send_tcp4(&mut self, frame: &[u8], layout: Tcp4) {
        let mut seg = [0u8; MAX_FRAME];
        offload::segment_tcp4(frame, layout, &mut seg, &mut |f| self.send(f));
    }
    /// The next frame for the guest, into `buf` (`MAX_FRAME` bytes): its
    /// length, or None when there is none.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// The next frame for the guest as `recv`, and what it is (`Rx`): a
    /// frame whose checksum is left to finish, or a send carried whole, only
    /// to a device that has said its guest takes them (`takes`); `buf` is
    /// `offload::MAX_SEND` bytes. The default hands frames whole.
    fn recv_with(&mut self, buf: &mut [u8]) -> Option<(usize, Rx)> {
        self.recv(buf).map(|len| (len, Rx::Done))
    }
    /// What the guest's driver takes (`Takes`): until it says so again, a
    /// backend hands it nothing else.
    fn takes(&mut self, _takes: Takes) {}
    /// Until `until_ns` on the boot clock, a frame for the guest need not
    /// wake the CPU that does the device's work: the device looks for what
    /// came by then on its own (`Net::rx_due`), its receive interrupts held
    /// till then anyway (`Net::set_rx_moderation`). Nothing to do for a
    /// backend that wakes nobody.
    fn quiet_until(&mut self, _until_ns: u64) {}
}

/// Virtio's type for a network device.
const VIRTIO_TYPE: u16 = 1;
/// Network, Ethernet.
const CLASS: u32 = 0x02_00_00;
/// The header and the configuration below, rounded up.
pub const BAR_SIZE: u32 = 0x40;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const QUEUE_SIZE: u16 = 256;
/// The most chains taken off a queue in one pass: a queue's worth. A driver
/// cannot have more than that available at once, so such a pass takes all
/// that was there when it began; what is posted while it runs -- by the
/// guest's other CPUs, into descriptors the pass gave back -- is the next
/// pass's: the notify that comes with it, or the next round of the devices
/// (`poll`). Unbounded, a guest that kept a ring topped up from a second
/// CPU, by plain writes and no exit, held the CPU running the pass in the
/// host for good, the platform's lock with it (hv-fuzz's racing CPU).
const PASS: usize = QUEUE_SIZE as usize;

/// Features: the guest's checksums completed by the device, and checksums
/// left partial handed to it; a MAC address of the device's; sends of TCP
/// over IPv4 handed to it whole, and taken from it to cut; mergeable
/// receive buffers -- a frame spread over as many as it needs -- and a link
/// status. Each offload of TCP needs its direction's checksums: a send cut
/// or carried whole has them partial.
const F_CSUM: u32 = 1 << 0;
const F_GUEST_CSUM: u32 = 1 << 1;
const F_MAC: u32 = 1 << 5;
const F_GUEST_TSO4: u32 = 1 << 7;
const F_HOST_TSO4: u32 = 1 << 11;
const F_MRG_RXBUF: u32 = 1 << 15;
const F_STATUS: u32 = 1 << 16;
/// What every NIC here offers, and the offloads on top (`set_offloads`).
/// EVENT_IDX among the first: a kick asked for on the transmit queue when
/// the device has sent every chain there was, and on the receive queue only
/// when a frame waits for buffers -- the driver's refills otherwise cost no
/// exit, the next frame taking what they posted.
const BASE_FEATURES: u32 = F_MAC | F_STATUS | virtio::F_EVENT_IDX;
const OFFLOADS: u32 = F_CSUM | F_GUEST_CSUM | F_GUEST_TSO4 | F_HOST_TSO4 | F_MRG_RXBUF;
const LINK_UP: u16 = 1;

/// The configuration: the MAC, then the status.
const CFG_MAC: usize = 0;
const CFG_STATUS: usize = 6;
const CFG_BYTES: usize = 8;
const _: () = assert!(virtio::DEVICE_CONFIG_MSIX as usize + CFG_BYTES <= BAR_SIZE as usize,
                      "the configuration fits the BAR, MSI-X's header and all");

/// Legacy's header, before every frame either way: ten bytes, and two more
/// -- how many buffers a frame was spread over -- once the driver takes
/// mergeable buffers.
const HDR: usize = TxHeader::LEN;
const HDR_MRG: usize = HDR + 2;
/// The most buffers a frame is spread over: a whole send of 64 KiB in the
/// 1,500-odd bytes Linux posts at the least, with room. A frame they would
/// not hold is dropped rather than wait for good.
const MAX_MERGE: u16 = 64;
/// The longest frame either way: Ethernet's, less the FCS.
pub const MAX_FRAME: usize = offload::MAX_FRAME;
const MIN_FRAME: usize = 14;

/// What the device has done, for a report.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    /// Frames sent and received -- a send of TCP up to 64 KiB counted as the
    /// frames it is cut into, and one received whole as one.
    pub sent: u64,
    pub received: u64,
    /// Sends of TCP up to 64 KiB, cut for their receivers or carried
    /// whole; and those received whole.
    pub large: u64,
    pub whole: u64,
    /// Sent frames that were not frames: too short, or longer than was
    /// offered.
    pub dropped: u64,
    /// The driver's notifies, to send and to receive -- each an exit -- and
    /// the interrupts raised for each queue: what EVENT_IDX spares.
    pub tx_notifies: u64,
    pub rx_notifies: u64,
    pub tx_interrupts: u64,
    pub rx_interrupts: u64,
}

pub struct Net {
    transport: Transport,
    backend: Box<dyn Backend>,
    mac: [u8; 6],
    segs: Vec<Seg>,
    /// A send, header included: a frame, or up to 64 KiB of TCP.
    tx_buf: Vec<u8>,
    /// The header and the frame behind it -- or a send carried whole --
    /// what goes into the guest's buffers as it lies, the frame always at
    /// `HDR_MRG` and its header right in front of it.
    rx_buf: Vec<u8>,
    /// A frame in `rx_buf` still waiting for buffers: its length, and what
    /// it is.
    held: Option<(usize, Rx)>,
    /// What the backend was told the guest takes.
    told: Takes,
    /// A transmit pass ended at its bound, with chains perhaps still on the
    /// ring: the next round of the devices goes on with them.
    tx_more: bool,
    /// Receive interrupts, moderated (`set_rx_moderation`): no closer than
    /// this after the last -- 0, each at once; the next allowed at
    /// `rx_quiet_until`; and frames delivered since the last with none
    /// raised for them yet, owed one then.
    rx_quiet_ns: u64,
    rx_quiet_until: u64,
    rx_owed: bool,
    broken: Option<Broken>,
    pub stats: Stats,
}

impl Net {
    pub fn new(backend: Box<dyn Backend>, mac: [u8; 6]) -> Result<Net> {
        let mut queues = Vec::new();
        queues.try_reserve_exact(2).map_err(|_| Error::NoMemory)?;
        queues.push(Queue::new(QUEUE_SIZE));
        queues.push(Queue::new(QUEUE_SIZE));
        let mut segs = Vec::new();
        segs.try_reserve_exact(usize::from(QUEUE_SIZE)).map_err(|_| Error::NoMemory)?;
        let mut tx_buf = Vec::new();
        tx_buf.try_reserve_exact(HDR_MRG + offload::MAX_SEND).map_err(|_| Error::NoMemory)?;
        tx_buf.resize(HDR_MRG + offload::MAX_SEND, 0);
        let mut rx_buf = Vec::new();
        rx_buf.try_reserve_exact(HDR_MRG + offload::MAX_SEND).map_err(|_| Error::NoMemory)?;
        rx_buf.resize(HDR_MRG + offload::MAX_SEND, 0);
        Ok(Net {
            transport: Transport::new(BASE_FEATURES | OFFLOADS, queues),
            backend,
            mac,
            segs,
            tx_buf,
            rx_buf,
            held: None,
            told: Takes::default(),
            tx_more: false,
            rx_quiet_ns: 0,
            rx_quiet_until: 0,
            rx_owed: false,
            broken: None,
            stats: Stats::default(),
        })
    }

    pub fn identity() -> Identity {
        Identity {
            vendor: virtio::VENDOR,
            device: virtio::LEGACY_DEVICE_BASE + VIRTIO_TYPE - 1,
            revision: 0,
            class: CLASS,
            subsystem_vendor: virtio::VENDOR,
            subsystem: VIRTIO_TYPE,
        }
    }

    pub fn broken(&self) -> Option<Broken> {
        self.broken
    }

    /// Whether a queue of its has its rings copied (`Transport::rings_copied`).
    pub fn rings_copied(&self) -> bool {
        self.transport.rings_copied()
    }

    /// The offloads offered -- checksums and TCP's sends of up to 64 KiB,
    /// both ways, and mergeable buffers -- or not, the device as it was
    /// before them, for a guest to be measured or run without. Before the
    /// driver reads the features: at the guest's next boot otherwise.
    pub fn set_offloads(&mut self, on: bool) {
        self.transport.set_device_features(if on { BASE_FEATURES | OFFLOADS } else { BASE_FEATURES });
    }

    /// Receive interrupts no closer than `ns` apart -- 0, as each frame
    /// comes -- as a real NIC's interrupt throttling has them. What arrives
    /// goes into the guest's buffers at once; its interrupt waits until the
    /// time since the last is up, and then one takes all that came. A frame
    /// after a quiet spell is interrupted for at once. Without it a guest
    /// that drains its ring faster than frames come takes an interrupt for
    /// every frame and acknowledges every one or two: two guests sending
    /// TCP through the switch exchanged five times the ACKs they did when
    /// halt polling held frames back by accident (docs/hypervisor.md).
    pub fn set_rx_moderation(&mut self, ns: u64) {
        self.rx_quiet_ns = ns;
    }

    /// When the device must be looked at again by its own round, if it is
    /// to keep what it promised its backend (`Backend::quiet_until`): the
    /// interrupt owed for what it delivered, and frames that came meanwhile
    /// and woke nobody. None when nothing is promised past `now`.
    pub fn rx_due(&self, now: u64) -> Option<u64> {
        (self.rx_quiet_until > now || self.rx_owed).then_some(self.rx_quiet_until)
    }

    /// Until when its receive interrupts are held, as the last one raised
    /// left them: what has whoever does the device's rounds come back then,
    /// when it changes behind its back (`LinuxGuest::run`).
    pub fn rx_window(&self) -> u64 {
        self.rx_quiet_until
    }

    /// Whether a transmit pass ended at its bound: the next round goes on
    /// with it, and is due at once -- a driver that kicks only when asked
    /// (EVENT_IDX) gives the device no other reason to come back.
    pub fn more(&self) -> bool {
        self.tx_more
    }

    fn config(&self) -> [u8; CFG_BYTES] {
        let mut c = [0u8; CFG_BYTES];
        c[CFG_MAC..CFG_MAC + 6].copy_from_slice(&self.mac);
        c[CFG_STATUS..CFG_STATUS + 2].copy_from_slice(&LINK_UP.to_le_bytes());
        c
    }

    /// Offer MSI-X, with a table of `entries`.
    pub fn offer_msix(&mut self, entries: u16) -> Option<()> {
        self.transport.offer_msix(entries)
    }

    /// Whether the function has MSI-X on, as its capability says now.
    pub fn set_msix_enabled(&mut self, on: bool) {
        self.transport.set_msix_enabled(on);
    }

    /// Whether it asserts its INTx line now: a level, for an IO-APIC's pin.
    pub fn line(&self) -> bool {
        self.transport.line()
    }

    /// A read of `size` bytes at `offset` in the BAR.
    pub fn io_read(&mut self, offset: u16, size: u8) -> u32 {
        let config = self.transport.config_offset();
        if offset < config {
            return self.transport.read(offset, size);
        }
        let c = self.config();
        let mut v = 0u32;
        for i in 0..u16::from(size) {
            if let Some(b) = c.get(usize::from(offset - config + i)) {
                v |= u32::from(*b) << (8 * i);
            }
        }
        v
    }

    /// A write of `size` bytes at `offset` in the BAR, at `now`, and the
    /// interrupts it leaves the device with.
    pub fn io_write(&mut self, offset: u16, size: u8, value: u32, mem: &GuestMemory, now: u64) -> Raise {
        if offset >= self.transport.config_offset() {
            return Raise::default();
        }
        match self.transport.write(offset, size, value, mem, now) {
            Asked::Notify(TX_QUEUE) => {
                self.stats.tx_notifies += 1;
                self.send(mem)
            }
            /* Buffers posted for receiving: whatever is waiting can go. */
            Asked::Notify(RX_QUEUE) => {
                self.stats.rx_notifies += 1;
                self.deliver(mem, now)
            }
            Asked::Reset => {
                self.broken = None;
                self.held = None;
                self.tx_more = false;
                self.rx_owed = false;
                self.tell(Takes::default());
                Raise::default()
            }
            _ => Raise::default(),
        }
    }

    /// What the driver has queued to send, to the backend: a pass's worth.
    fn send(&mut self, mem: &GuestMemory) -> Raise {
        self.tx_more = false;
        if self.broken.is_some() {
            return Raise::default();
        }
        let mut sent = false;
        let mut left = PASS;
        loop {
            if left == 0 {
                self.tx_more = true;
                break;
            }
            left -= 1;
            let mut segs = core::mem::take(&mut self.segs);
            let popped = match self.transport.queue(TX_QUEUE) {
                Some(q) if q.ready() => q.pop(mem, &mut segs),
                _ => Ok(None),
            };
            let result = match popped {
                Ok(Some(head)) => {
                    self.transmit(mem, &segs);
                    self.transport.queue(TX_QUEUE).ok_or(Broken::Index).and_then(|q| q.push(mem, head, 0))
                }
                Ok(None) => {
                    self.segs = segs;
                    /* Every chain there was sent: a kick asked for the
                     * next, and what came before the ask sent now. */
                    match self.transport.queue(TX_QUEUE).ok_or(Broken::Index).and_then(|q| q.ask_kick(mem)) {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(e) => {
                            self.broken = Some(e);
                            break;
                        }
                    }
                }
                Err(e) => Err(e),
            };
            self.segs = segs;
            match result {
                Ok(()) => sent = true,
                Err(e) => {
                    self.broken = Some(e);
                    break;
                }
            }
        }
        let wants = sent && self.transport.queue(TX_QUEUE).map_or(false, |q| q.wants_interrupt(mem));
        if !wants {
            return Raise::default();
        }
        self.stats.tx_interrupts += 1;
        self.transport.interrupt(TX_QUEUE)
    }

    /// One chain the driver sent: the header, then the frame -- or a send
    /// of TCP up to 64 KiB -- taken whole, and on to the backend as the
    /// header says (`offload::transmit`).
    fn transmit(&mut self, mem: &GuestMemory, segs: &[Seg]) {
        let features = self.transport.driver_features();
        let offloads = Offloads { csum: features & F_CSUM != 0, tso4: features & F_HOST_TSO4 != 0 };
        let hdr = if features & F_MRG_RXBUF != 0 { HDR_MRG } else { HDR };
        let most = if offloads.tso4 { hdr + offload::MAX_SEND } else { hdr + MAX_FRAME };
        let total: u64 = segs.iter().filter(|s| !s.write).map(|s| u64::from(s.len)).sum();
        let len = total as usize;
        if total < (hdr + MIN_FRAME) as u64 || total > most as u64 {
            self.stats.dropped += 1;
            return;
        }
        let mut done = 0usize;
        for s in segs.iter().filter(|s| !s.write) {
            let n = s.len as usize;
            /* A buffer of no bytes has none to take, wherever it says it is. */
            let Some(at) = s.at(0) else { continue };
            if mem.read(at, &mut self.tx_buf[done..done + n]).is_err() {
                self.stats.dropped += 1;
                return;
            }
            done += n;
        }
        let Net { tx_buf, backend, stats, .. } = self;
        let mut header = [0u8; HDR];
        header.copy_from_slice(&tx_buf[..HDR]);
        let header = TxHeader::parse(&header);
        match offload::transmit(&header, &mut tx_buf[hdr..len], offloads) {
            Ok(Sent::Frame(frame, None)) => {
                backend.send(frame);
                stats.sent += 1;
            }
            Ok(Sent::Frame(frame, Some(partial))) => {
                backend.send_partial(frame, partial);
                stats.sent += 1;
            }
            Ok(Sent::Tcp4 { frame, layout }) => {
                stats.sent += layout.frames(frame.len() - layout.headers()) as u64;
                stats.large += 1;
                backend.send_tcp4(frame, layout);
            }
            Err(_) => stats.dropped += 1,
        }
    }

    /// A round of the device at `now`, each time round the run loop: what a
    /// transmit pass left at its bound sent, and what the backend has for
    /// the guest delivered -- and the interrupts that leaves the device
    /// with, a receive interrupt owed among them once its time is up.
    pub fn poll(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        let sent = if self.tx_more { self.send(mem) } else { Raise::default() };
        sent.or(self.deliver(mem, now))
    }

    /// What the backend has for the guest, into the buffers the driver has
    /// posted -- a pass's worth -- and the interrupts that leaves the device
    /// with at `now`. What is left waits with the backend for the next round.
    fn deliver(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        if self.broken.is_some() || !self.transport.queue(RX_QUEUE).map_or(false, |q| q.ready()) {
            /* A queue taken down, or a ring the device stopped following,
             * is owed nothing: kept owing, it would be due for good, and
             * the round that looks at it run without a pause (hv-fuzz
             * found it). Nor does it take sends whole. */
            self.rx_owed = false;
            self.tell(Takes::default());
            return Raise::default();
        }
        let features = self.transport.driver_features();
        let merged = features & F_MRG_RXBUF != 0;
        let takes = Takes {
            partial: features & F_GUEST_CSUM != 0,
            whole: features & (F_GUEST_TSO4 | F_GUEST_CSUM) == F_GUEST_TSO4 | F_GUEST_CSUM,
        };
        self.tell(takes);
        let hdr = if merged { HDR_MRG } else { HDR };
        let start = HDR_MRG - hdr;
        let mut delivered = false;
        for _ in 0..PASS {
            let (len, rx) = match self.held {
                Some(h) => h,
                None => match self.backend.recv_with(&mut self.rx_buf[HDR_MRG..]) {
                    Some((len, Rx::Done)) if (MIN_FRAME..=MAX_FRAME).contains(&len) => (len, Rx::Done),
                    Some((len, Rx::Partial(p))) if (MIN_FRAME..=MAX_FRAME).contains(&len) => {
                        if takes.partial {
                            (len, Rx::Partial(p))
                        } else {
                            /* Its checksum finished here for a driver that
                             * takes none partial, whoever handed it over. */
                            p.complete(&mut self.rx_buf[HDR_MRG..HDR_MRG + len]);
                            (len, Rx::Done)
                        }
                    }
                    Some((len, Rx::Whole(t))) if takes.whole && (t.headers()..=offload::MAX_SEND).contains(&len) => {
                        (len, Rx::Whole(t))
                    }
                    Some(_) => {
                        self.stats.dropped += 1;
                        continue;
                    }
                    None => break,
                },
            };
            /* Its header in front of it: nothing to say of a frame with its
             * checksums whole; one left to finish says where; a send carried
             * whole is to be cut by its segment size, its checksum partial. */
            let header = rx.header();
            self.rx_buf[start..start + HDR].copy_from_slice(&header.bytes());
            let total = hdr + len;
            let mut segs = core::mem::take(&mut self.segs);
            let result = if merged {
                self.merge(mem, total, start, &mut segs)
            } else {
                self.one_chain(mem, total, start, &mut segs)
            };
            self.segs = segs;
            match result {
                Ok(Taken::In) => {
                    self.held = None;
                    delivered = true;
                    self.stats.received += 1;
                    if matches!(rx, Rx::Whole(_)) {
                        self.stats.whole += 1;
                    }
                }
                Ok(Taken::Wait) => {
                    /* Not buffers enough yet: the frame waits here, a kick
                     * asked for the driver's next -- and what it posted
                     * before the ask looked at now, the pass's bound
                     * bounding the looks. */
                    self.held = Some((len, rx));
                    match self.transport.queue(RX_QUEUE).ok_or(Broken::Index).and_then(|q| q.ask_kick(mem)) {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(e) => {
                            self.held = None;
                            self.broken = Some(e);
                            break;
                        }
                    }
                }
                Ok(Taken::Dropped) => {
                    self.held = None;
                    self.stats.dropped += 1;
                }
                Err(e) => {
                    self.held = None;
                    self.broken = Some(e);
                    break;
                }
            }
        }
        if delivered {
            self.rx_owed = true;
        }
        self.rx_interrupt(mem, now)
    }

    /// The `total` bytes at `rx_buf[start..]`, header and frame, spread over
    /// as many of the driver's mergeable buffers as they take -- the count
    /// put in the header first -- or waiting for more, or dropped when as
    /// many as a frame may take would not hold it.
    fn merge(&mut self, mem: &GuestMemory, total: usize, start: usize, segs: &mut Vec<Seg>) -> core::result::Result<Taken, Broken> {
        let q = self.transport.queue(RX_QUEUE).ok_or(Broken::Index)?;
        let chains = match q.room_ahead(mem, total as u64, MAX_MERGE, segs)? {
            Room::Enough(n) => n,
            Room::Wait => return Ok(Taken::Wait),
            Room::Never => return Ok(Taken::Dropped),
        };
        self.rx_buf[start + HDR..start + HDR_MRG].copy_from_slice(&chains.to_le_bytes());
        let mut done = 0usize;
        for _ in 0..chains {
            let q = self.transport.queue(RX_QUEUE).ok_or(Broken::Index)?;
            /* The chains looked at a moment ago: a driver that took them
             * back meanwhile, from another CPU, has a frame cut short. */
            let Some(head) = q.pop(mem, segs)? else { break };
            let written = write_chain(mem, segs, &self.rx_buf[start + done..start + total]);
            done += written;
            self.transport.queue(RX_QUEUE).ok_or(Broken::Index)?.push(mem, head, written as u32)?;
        }
        Ok(if done == total { Taken::In } else { Taken::Dropped })
    }

    /// The `total` bytes at `rx_buf[start..]` into one chain of the
    /// driver's: waiting for one, or -- too short for them -- the frame
    /// dropped and the chain given back empty.
    fn one_chain(&mut self, mem: &GuestMemory, total: usize, start: usize, segs: &mut Vec<Seg>) -> core::result::Result<Taken, Broken> {
        let q = self.transport.queue(RX_QUEUE).ok_or(Broken::Index)?;
        let Some(head) = q.pop(mem, segs)? else { return Ok(Taken::Wait) };
        let room: u64 = segs.iter().filter(|s| s.write).map(|s| u64::from(s.len)).sum();
        let written = if room < total as u64 { 0 } else { write_chain(mem, segs, &self.rx_buf[start..start + total]) };
        self.transport.queue(RX_QUEUE).ok_or(Broken::Index)?.push(mem, head, written as u32)?;
        Ok(if written == total { Taken::In } else { Taken::Dropped })
    }

    /// The backend told what the driver comes to take, or stops taking.
    fn tell(&mut self, takes: Takes) {
        if self.told != takes {
            self.told = takes;
            self.backend.takes(takes);
        }
    }

    /// The receive interrupt owed for what was delivered, if its time is up
    /// at `now` -- and the next not before `rx_quiet_ns` from now, frames
    /// for the guest until then waking nobody (`Backend::quiet_until`): the
    /// device's round comes for them by then (`rx_due`). One the driver has
    /// said it does not want -- it is taking them already -- is not owed.
    fn rx_interrupt(&mut self, mem: &GuestMemory, now: u64) -> Raise {
        if !self.rx_owed || now < self.rx_quiet_until {
            return Raise::default();
        }
        self.rx_owed = false;
        if !self.transport.queue(RX_QUEUE).map_or(false, |q| q.wants_interrupt(mem)) {
            return Raise::default();
        }
        if self.rx_quiet_ns != 0 {
            self.rx_quiet_until = now.saturating_add(self.rx_quiet_ns);
            self.backend.quiet_until(self.rx_quiet_until);
        }
        self.stats.rx_interrupts += 1;
        self.transport.interrupt(RX_QUEUE)
    }

}

/// What became of a frame offered to the driver's buffers.
enum Taken {
    In,
    Wait,
    Dropped,
}

/// `data` into the chain's writable buffers, one copy a buffer -- `GuestMemory`
/// taking it a page at a time -- as much as they hold: what was written. It
/// went a byte at a time once, through a 256-byte bounce buffer whose every
/// piece was mapped on its own -- host work on the vCPU's own CPU, for every
/// frame, while the guest waited.
fn write_chain(mem: &GuestMemory, segs: &[Seg], data: &[u8]) -> usize {
    let mut done = 0usize;
    for s in segs.iter().filter(|s| s.write) {
        if done == data.len() {
            break;
        }
        let n = (data.len() - done).min(s.len as usize);
        let Some(at) = s.at(0) else { continue };
        if mem.write(at, &data[done..done + n]).is_err() {
            break;
        }
        done += n;
    }
    done
}
