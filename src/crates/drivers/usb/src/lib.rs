//! USB: the xHCI host controller and the HID boot keyboard on it.
//!
//! This exists for one machine. The Dell Latitude this kernel boots is UEFI
//! with no serial port and no PS/2 controller, so the xHCI keyboard is the
//! only way to type at it. The driver is deliberately small: it enumerates
//! what it finds, keeps a boot-protocol keyboard or a hub that might have one
//! behind it, and releases the slot of anything else.
//!
//! It polls. There is no interrupt handler here at all: a task wakes every
//! few milliseconds, drains the event ring, picks up hot-plug and pumps each
//! keyboard's interrupt-IN endpoint. A keystroke costs at most one poll
//! period of latency, which nobody can feel, and the driver never has to be
//! safe from interrupt context.
//!
//! `scripts/usb-test.py` is its test: it types at an emulated keyboard
//! through QEMU's monitor and reads the shell's answer.

#![no_std]

extern crate alloc;

pub mod controller;
pub mod descriptors;
pub mod device;
pub mod hid;
pub mod regs;
pub mod ring;
pub mod shared;

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use kcore::cmd::{Command, Output};
use kcore::sync::TryLock;
use kcore::task::TaskHandle;
use kcore::trace;

use controller::{Controller, MAX_CONTROLLERS, MAX_PORTS, POLL_PERIOD_MS};
use descriptors::speed_name;

/// The controllers, in the order they were found. Put here once each, during
/// `init`, and polled by the USB task alone: nobody ever waits for this lock,
/// and it is one that may be held across the sleeps a port reset takes.
static CONTROLLERS: TryLock<[Option<Box<Controller>>; MAX_CONTROLLERS]> =
    TryLock::new([const { None }; MAX_CONTROLLERS]);
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// The polling task, while it runs.
static TASK: TryLock<Option<TaskHandle>> = TryLock::new(None);

/// Put the layer's command in front of whoever runs one. Called from
/// `rust_init`.
pub fn init() {
    match Command::register("usb", "usb - show usb controllers and ports", dump) {
        /* The command is the kernel's own and stays for good. */
        Ok(cmd) => core::mem::forget(cmd),
        Err(_) => trace!(0, "usb: cannot register the usb command"),
    }
}

/// Bring up every xHCI controller the PCI scan found and enumerate what is
/// already attached. Runs synchronously in the caller's task -- the reset and
/// port-reset paths sleep -- so that a keyboard is live, and its bring-up
/// traces are on the console, before the shell takes the console over.
fn init_all() {
    /* Serial bus controller / USB / xHCI programming interface */
    const CLASS_SERIAL_BUS: u8 = 0x0C;
    const SUBCLASS_USB: u8 = 0x03;
    const PROG_IF_XHCI: u8 = 0x30;

    let mut found = 0;
    for index in 0..kcore::pci::device_count() {
        if found >= MAX_CONTROLLERS {
            break;
        }
        let pci = match kcore::pci::get_device(index) {
            Some(pci) => pci,
            None => break,
        };
        if pci.class != CLASS_SERIAL_BUS || pci.subclass != SUBCLASS_USB {
            continue;
        }
        if pci.prog_if != PROG_IF_XHCI {
            continue;
        }

        let mut ctrl = match Controller::open(found, pci) {
            Some(ctrl) => ctrl,
            None => continue,
        };
        if !ctrl.init() {
            continue;
        }

        match CONTROLLERS.try_lock() {
            Some(mut controllers) => controllers[found] = Some(ctrl),
            None => {
                /* Only a second `init` running beside this one could hold it. */
                trace!(0, "Xhci: controller table busy, controller {} dropped", found);
                continue;
            }
        }
        found += 1;
        COUNT.store(found, Ordering::Release);
    }

    trace!(0, "Xhci: initialized {} controllers", found);
}

fn poll_all() {
    /* The USB task is the only caller, so the lock is there for the taking. */
    let mut controllers = match CONTROLLERS.try_lock() {
        Some(controllers) => controllers,
        None => return,
    };

    for ctrl in controllers.iter_mut().flatten() {
        ctrl.poll();
    }
}

fn run() {
    while !kcore::task::stopping() {
        poll_all();
        kcore::task::sleep_ms(POLL_PERIOD_MS);
    }
}

/* ---- what `usb` prints ---- */

fn dump(_args: &str, out: &mut Output) {
    let count = COUNT.load(Ordering::Acquire);
    if count == 0 {
        let _ = writeln!(out, "no xhci controllers");
        return;
    }

    for index in 0..count {
        let s = match shared::snapshot(index) {
            Some(s) => s,
            None => continue,
        };

        let _ = writeln!(out, "xhci {:04x}:{:04x} hci {}.{} slots {} ports {} ctx {}",
            s.vendor, s.device, s.hci_version >> 8, s.hci_version & 0xFF,
            s.max_slots, s.num_ports, s.context_size);

        for port in 1..=(s.num_ports as usize).min(MAX_PORTS - 1) {
            let p = s.ports[port];
            if !p.connected {
                continue;
            }
            let _ = writeln!(out, "  port {}: {} speed {:04x}:{:04x} class {} slot {}{}",
                port, speed_name(p.speed), p.vendor_id, p.product_id,
                p.device_class, p.slot_id,
                if p.keyboard { " [boot keyboard]" } else { "" });
        }

        for kbd in s.keyboards.iter() {
            if !kbd.live {
                continue;
            }
            let _ = writeln!(out, "  keyboard slot {}: ep 0x{:X} reports {} errors {}",
                kbd.slot_id, kbd.ep_address, kbd.reports, kbd.errors);
        }
    }
}

/* ---- what the kernel calls ---- */

/// Bring up every controller and enumerate what is attached.
#[no_mangle]
pub extern "C" fn rust_usb_init() {
    init_all();
}

/// Start the task that polls event rings, services keyboards and picks up
/// hot-plug. Called after `rust_usb_init`.
#[no_mangle]
pub extern "C" fn rust_usb_start() -> i32 {
    let mut task = match TASK.try_lock() {
        Some(task) => task,
        None => return -1,
    };
    if task.is_some() {
        return -1;
    }

    match kcore::task::spawn("usb", run) {
        Some(handle) => {
            *task = Some(handle);
            0
        }
        None => -1,
    }
}

/// On the way down: the task finishes its pass and exits.
#[no_mangle]
pub extern "C" fn rust_usb_stop() {
    let handle = match TASK.try_lock() {
        Some(mut task) => task.take(),
        None => None,
    };

    if let Some(handle) = handle {
        handle.request_stop();
        handle.wait();
    }
}
