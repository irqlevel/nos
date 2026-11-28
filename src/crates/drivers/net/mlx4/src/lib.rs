/* ConnectX-3/3 Pro Ethernet. Hardware protocols are adapted from the MIT
 * licensed stormnic-mlx4 project; see LICENSE and NOTICE in this crate. */
#![no_std]

extern crate alloc;

macro_rules! say {
    ($($t:tt)*) => { kcore::klog!(0, $($t)*) };
}
macro_rules! trace {
    ($($t:tt)*) => { kcore::klog!(0, $($t)*) };
}
macro_rules! alarm {
    ($($t:tt)*) => { kcore::klog!(0, $($t)*) };
}
macro_rules! note {
    ($loud:expr, $($t:tt)*) => {
        if $loud { say!($($t)*) } else { trace!($($t)*) }
    };
}

mod bars;
mod diag;
mod dma;
mod eth;
mod fw;
mod hcr;
mod mcg;
mod module;
mod pci;
mod sent;
mod time;

use alloc::boxed::Box;
use alloc::vec::Vec;

use eth::{RxPort, TxPort};
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::{pci as kernel_pci, softirq, timer};
use net::{Frame, NetDriver, RxQueue, TxQueue};

use crate::pci::PciIo;

const VENDOR: u16 = 0x15b3;
const DEVICES: [u16; 2] = [0x1003, 0x1007];
const RX_BUDGET: usize = 64;

struct HcaState {
    hca: fw::Hca,
    pci: PciIo,
}

static HCAS: OnceBox<Mutex<Vec<HcaState>>> = OnceBox::new();

fn hcas() -> Option<&'static Mutex<Vec<HcaState>>> {
    HCAS.get_or_try_init(|| Some(Box::new(Mutex::new(Vec::new())?)))
}

struct Mlx4Device {
    name: [u8; net::device::ETH_NAME_MAX],
    name_len: usize,
}

impl Mlx4Device {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("eth?")
    }

    fn wake_rx(_dev: &'static Mlx4Device) {
        softirq::raise(softirq::TYPE_NET_RX);
    }
}

impl NetDriver for Mlx4Device {
    type Tx = TxPort;
    type Rx = RxPort;

    fn flush_tx(&'static self, tx: &mut TxPort, queue: &mut TxQueue<'_>) {
        while tx.can_send() {
            let Some(frame) = queue.dequeue() else {
                break;
            };
            if let Err(e) = tx.send(frame.bytes()) {
                let why = match e {
                    eth::Send::Full => "ring full",
                    eth::Send::TooLong => "frame too long",
                    eth::Send::Broken => "queue broken",
                };
                kcore::klog!(0, "mlx4: {}: TX dropped: {}", self.name(), why);
            }
            queue.done(frame);
        }
    }

    fn process_rx(&'static self, rx: &mut RxPort, queue: &mut RxQueue<'_>) {
        for _ in 0..RX_BUDGET {
            let Some((mem, len)) = rx.peek_rx() else {
                break;
            };
            let mut frame = match Frame::alloc_rx(len) {
                Some(frame) => frame,
                None => {
                    rx.pop_rx();
                    continue;
                }
            };
            mem.read(0, frame.data_raw_mut(len));
            frame.set_len(len);
            rx.pop_rx();
            queue.enqueue(frame);
        }
    }
}

pub fn init() {
    for device_id in DEVICES {
        let mut start = 0;
        loop {
            match kernel_pci::find_device_from(VENDOR, device_id, start) {
                None => break,
                Some((index, dev)) => {
                    kcore::klog!(0, "mlx4: found {:04x}:{:04x} at {:02x}:{:02x}.{}",
                        VENDOR, device_id, dev.bus, dev.slot, dev.func);
                    init_device(&dev);
                    start = index + 1;
                }
            }
        }
    }
}

fn init_device(dev: &kernel_pci::PciDevice) {
    let mut pci = match PciIo::new(dev) {
        Ok(pci) => pci,
        Err(_) => {
            kcore::klog!(0, "mlx4: cannot map the PCI BARs");
            return;
        }
    };
    let (mut hca, setup) = match fw::open(&mut pci) {
        Ok(opened) => opened,
        Err(_) => {
            kcore::klog!(0, "mlx4: firmware bring-up failed");
            return;
        }
    };
    let mut ethernet = match eth::open(&mut hca, &mut pci, &setup) {
        Ok(ethernet) => ethernet,
        Err(_) => {
            kcore::klog!(0, "mlx4: Ethernet bring-up failed");
            fw::close(hca, &mut pci);
            return;
        }
    };
    ethernet.wait_link(&mut hca, &mut pci, 5000);

    let mut registered = 0;
    for port in core::mem::take(&mut ethernet.ports) {
        let mac = port.mac();
        let port_number = port.num();
        let (rx, tx) = port.into_halves(pci.clone());
        let mut name = [0u8; net::device::ETH_NAME_MAX];
        let name_len = match net::free_eth_name(&mut name) {
            Some(name) => name.len(),
            None => {
                kcore::klog!(0, "mlx4: no ethN name left");
                continue;
            }
        };
        let dev: &'static Mlx4Device = Box::leak(Box::new(Mlx4Device { name, name_len }));
        if net::register(dev.name(), mac, dev, tx, rx).is_none() {
            kcore::klog!(0, "mlx4: cannot register port {}", port_number);
            continue;
        }
        match timer::Timer::start_for(kcore::time::Duration::from_millis(1), dev, Mlx4Device::wake_rx) {
            Some(timer) => timer.leak(),
            None => kcore::klog!(0, "mlx4: {}: cannot start the receive poll", dev.name()),
        }
        registered += 1;
        kcore::klog!(0, "mlx4: port {} registered as {}", port_number, dev.name());
    }

    if registered == 0 {
        fw::close(hca, &mut pci);
        return;
    }
    match hcas() {
        Some(states) => states.lock().push(HcaState { hca, pci }),
        None => {
            kcore::klog!(0, "mlx4: cannot retain the HCA state; leaving it running");
            core::mem::forget((hca, pci));
        }
    }
}

pub fn shutdown() {
    if let Some(states) = HCAS.get() {
        for state in states.lock().iter_mut() {
            fw::quiesce(&mut state.hca, &mut state.pci);
        }
    }
    kcore::klog!(0, "mlx4: shutdown complete");
}
