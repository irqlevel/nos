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

use core::fmt::Write;

use eth::{RxPort, TxPort};
use kcore::cmd::{Command, Output};
use kcore::once::OnceBox;
use kcore::sync::Mutex;
use kcore::{pci as kernel_pci, softirq, timer};
use net::{Frame, NetDriver, RxQueue, TxQueue};

use crate::pci::PciIo;

const DUMP_HELP: &str = "mlxdump [n] - QUERY_QP of the nth mlx4";
const _: () = assert!(DUMP_HELP.len() <= kcore::cmd::HELP_MAX, "`help` would cut it short");

const VENDOR: u16 = 0x15b3;
const DEVICES: [u16; 2] = [0x1003, 0x1007];
const RX_BUDGET: usize = 64;

struct HcaState {
    hca: fw::Hca,
    pci: PciIo,
    ports: Vec<(u8, u32, u32)>,
}

static HCAS: OnceBox<Mutex<Vec<HcaState>>> = OnceBox::new();

fn hcas() -> Option<&'static Mutex<Vec<HcaState>>> {
    HCAS.get_or_try_init(|| Some(Box::new(Mutex::new(Vec::new())?)))
}

fn dump(args: &str, out: &mut Output) {
    let n = match args.trim() {
        "" => 0,
        word => match word.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                let _ = writeln!(out, "usage: mlxdump [n]");
                return;
            }
        },
    };
    let Some(states) = HCAS.get() else {
        let _ = writeln!(out, "mlxdump: no cards");
        return;
    };
    let mut states = states.lock();
    let Some(state) = states.get_mut(n) else {
        let _ = writeln!(out, "mlxdump: no card {}", n);
        return;
    };
    let HcaState { hca, pci, ports } = state;
    for &(port, rx_qpn, tx_qpn) in ports.iter() {
        for (kind, qpn) in [("rx", rx_qpn), ("tx", tx_qpn)] {
            match hca.hcr.with_out(pci, hcr::QUERY_QP, 0, qpn) {
                Ok(()) => {
                    let context = hca.hcr.outbox.be32(0);
                    let _ = writeln!(out, "port {} {} qpn {:#x}: context {:#010x}; full mailbox in dmesg",
                        port, kind, qpn, context);
                    eth::dump_qp_mailbox(&hca.hcr.outbox);
                }
                Err(e) => {
                    let _ = writeln!(out, "port {} {} qpn {:#x}: QUERY_QP: {}", port, kind, qpn, e);
                }
            }
        }
    }
    hca.hcr.log_catas(pci);
}

struct Mlx4Device {
    name: [u8; net::device::ETH_NAME_MAX],
    name_len: usize,
}

impl Mlx4Device {
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("eth?")
    }

    fn wake_queues(_dev: &'static Mlx4Device) {
        /* flush_tx is also what reaps completions. A full queue can sleep
         * with no new frame to call it, so poll both directions. */
        softirq::raise_mask((1 << softirq::TYPE_NET_RX) | (1 << softirq::TYPE_NET_TX));
    }
}

impl NetDriver for Mlx4Device {
    type Tx = TxPort;
    type Rx = RxPort;

    fn flush_tx(&'static self, tx: &mut TxPort, queue: &mut TxQueue<'_>) {
        let mut submitted = 0;
        while tx.can_send() {
            let Some(frame) = queue.dequeue() else {
                break;
            };
            match tx.send(frame.bytes()) {
                Ok(()) => submitted += 1,
                Err(e) => {
                    let why = match e {
                        eth::Send::Full => "ring full",
                        eth::Send::TooLong => "frame too long",
                        eth::Send::Broken => "queue broken",
                    };
                    kcore::klog!(0, "mlx4: {}: TX dropped: {}", self.name(), why);
                }
            }
            queue.done(frame);
        }
        if submitted != 0 && tx.doorbell().is_err() {
            kcore::klog!(0, "mlx4: {}: TX doorbell failed", self.name());
        }
    }

    fn process_rx(&'static self, rx: &mut RxPort, queue: &mut RxQueue<'_>) {
        let mut rounds = 0;
        for _ in 0..RX_BUDGET {
            let Some((mem, len)) = rx.peek_rx() else {
                break;
            };
            rounds += 1;
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
        /* The periodic timer's shortest period is the 10 ms tick. Do not
         * leave completions waiting for it after a budget-limited pass. */
        if rounds == RX_BUDGET && rx.peek_rx().is_some() {
            softirq::raise(softirq::TYPE_NET_RX);
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
    let mut qpns = Vec::new();
    for port in core::mem::take(&mut ethernet.ports) {
        let mac = port.mac();
        let port_number = port.num();
        let (rx_qpn, tx_qpn) = port.qpns();
        qpns.push((port_number, rx_qpn, tx_qpn));
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
        match timer::Timer::start_for(kcore::time::Duration::from_millis(1), dev, Mlx4Device::wake_queues) {
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
        Some(states) => {
            let mut states = states.lock();
            states.push(HcaState { hca, pci, ports: qpns });
            if states.len() == 1 {
                if let Ok(cmd) = Command::register("mlxdump", DUMP_HELP, dump) {
                    core::mem::forget(cmd);
                }
            }
        }
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
