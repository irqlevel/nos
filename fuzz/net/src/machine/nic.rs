//! The machine's NIC: a `net::NetDriver` whose hardware is the fuzzer. What
//! the stack transmits is taken off its queue as a driver's ring takes it,
//! copied onto the wire for the world to see, and handed back to the stack
//! as a finished frame; what the world puts on the wire for the machine
//! comes up at the next receive pass, in frames from the pool, as a
//! driver's harvest does. And its ring can fill: the stack's queue then
//! backs up, as it does behind a NIC that has stopped taking frames, until
//! the world lets it drain.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::sched;

/// The NICs there are: eth0, which every target has, and a second a target
/// may register for itself -- a device none of the machine's services
/// listens on, as the hypervisor's switch is (`targets::tcp`).
pub const NICS: usize = 2;

pub struct FuzzNic {
    index: usize,
}

pub static DRIVERS: [FuzzNic; NICS] = [FuzzNic { index: 0 }, FuzzNic { index: 1 }];

pub struct Wire {
    /// What the stack sent out of each NIC, oldest first.
    pub out: [VecDeque<Vec<u8>>; NICS],
    /// What has arrived at each, for its next receive pass.
    pub inbound: [VecDeque<Vec<u8>>; NICS],
    /// Each NIC's transmit ring is full: nothing more is taken off the
    /// stack's queue.
    pub stalled: [bool; NICS],
    /// Frames a receive pass could not have from the pool, nor the
    /// allocator: dropped, as a NIC with no buffer posted drops them.
    pub no_buffer: u64,
    /// When each NIC's transmit side last started again: what it takes
    /// then is what queued up while it was stopped, made any time before.
    pub unstalled_at: [u64; NICS],
}

pub static WIRE: Mutex<Wire> = Mutex::new(Wire {
    out: [const { VecDeque::new() }; NICS],
    inbound: [const { VecDeque::new() }; NICS],
    stalled: [false; NICS],
    no_buffer: 0,
    unstalled_at: [0; NICS],
});

pub fn wire() -> std::sync::MutexGuard<'static, Wire> {
    WIRE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The NIC's transmit side stops taking frames -- a ring full, a link
/// down -- or starts again: and then, as its completion interrupt would, it
/// has the stack hand it what queued up meanwhile.
pub fn stall(index: usize, stalled: bool) {
    {
        let mut w = wire();
        if w.stalled[index] && !stalled {
            w.unstalled_at[index] = sched::now();
        }
        w.stalled[index] = stalled;
    }
    if !stalled {
        sched::softirq_raise(kcore::softirq::TYPE_NET_TX);
    }
}

impl net::NetDriver for FuzzNic {
    type Tx = ();
    type Rx = ();

    /// Under the device's transmit lock, interrupts off: nothing of the
    /// kernel's is allocated here, and what the wire keeps is the fuzzer's.
    fn flush_tx(&'static self, _tx: &mut (), queue: &mut net::TxQueue<'_>) {
        sched::harness(|| {
            let mut w = wire();
            if w.stalled[self.index] {
                return;
            }
            while let Some(frame) = queue.dequeue() {
                w.out[self.index].push_back(frame.bytes().to_vec());
                /* Sent: the frame back to the stack, to release off the
                 * lock -- never dropped here. */
                queue.done(frame);
            }
        });
        sched::harness(sched::world_kick);
    }

    /// From the receive soft IRQ: what arrived, into frames, up the stack.
    fn process_rx(&'static self, _rx: &mut (), queue: &mut net::RxQueue<'_>) {
        let arrived: Vec<Vec<u8>> = sched::harness(|| wire().inbound[self.index].drain(..).collect());
        let mut frames = net::FrameQueue::new();
        for bytes in arrived {
            let frame = net::Frame::alloc_rx(bytes.len()).and_then(|mut f| f.fill(&bytes).then_some(f));
            match frame {
                Some(f) => frames.push(f),
                None => sched::harness(|| wire().no_buffer += 1),
            }
        }
        queue.deliver(&mut frames);
    }
}
