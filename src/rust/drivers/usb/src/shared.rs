//! What `usb` prints, kept apart from the controller itself.
//!
//! The USB task owns every controller outright and is the only thing that
//! touches one. The shell's `usb` command runs on another task, so rather
//! than let it reach into state the task is mutating, the task publishes what
//! the command needs here, under a lock of its own. Nothing else is shared.

use core::cell::UnsafeCell;

use kcore::sync::IrqSpinLock;

use crate::controller::{MAX_CONTROLLERS, MAX_DEVICES, MAX_PORTS};

/// Per-root-port bookkeeping, kept for `usb` even after a non-keyboard
/// device has had its slot released.
#[derive(Clone, Copy, Default)]
pub struct PortRecord {
    pub connected: bool,
    pub enumerated: bool,
    pub speed: u8,
    pub slot_id: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_class: u8,
    pub keyboard: bool,
}

/// One keyboard's line.
#[derive(Clone, Copy, Default)]
pub struct KeyboardRecord {
    pub live: bool,
    pub slot_id: u8,
    pub ep_address: u8,
    pub reports: u32,
    pub errors: u32,
}

#[derive(Clone, Copy)]
pub struct Shared {
    pub live: bool,
    pub vendor: u16,
    pub device: u16,
    pub hci_version: u16,
    pub max_slots: u8,
    pub num_ports: u8,
    pub context_size: u32,
    pub ports: [PortRecord; MAX_PORTS],
    pub keyboards: [KeyboardRecord; MAX_DEVICES],
}

impl Shared {
    const fn new() -> Self {
        Self {
            live: false,
            vendor: 0,
            device: 0,
            hci_version: 0,
            max_slots: 0,
            num_ports: 0,
            context_size: 0,
            ports: [PortRecord {
                connected: false, enumerated: false, speed: 0, slot_id: 0,
                vendor_id: 0, product_id: 0, device_class: 0, keyboard: false,
            }; MAX_PORTS],
            keyboards: [KeyboardRecord {
                live: false, slot_id: 0, ep_address: 0, reports: 0, errors: 0,
            }; MAX_DEVICES],
        }
    }
}

struct Cell {
    lock: IrqSpinLock,
    inner: UnsafeCell<Shared>,
}

/* One task writes, one reads, and the lock is what stands between them. */
unsafe impl Sync for Cell {}

static SHARED: [Cell; MAX_CONTROLLERS] =
    [const { Cell { lock: IrqSpinLock::new(), inner: UnsafeCell::new(Shared::new()) } };
        MAX_CONTROLLERS];

/// Change what the shell will see of controller `index`.
pub fn update(index: usize, edit: impl FnOnce(&mut Shared)) {
    if index >= MAX_CONTROLLERS {
        return;
    }
    let cell = &SHARED[index];
    let _guard = cell.lock.lock();
    edit(unsafe { &mut *cell.inner.get() });
}

/// A copy of it, for printing without holding the lock while the console
/// takes its time.
pub fn snapshot(index: usize) -> Option<Shared> {
    if index >= MAX_CONTROLLERS {
        return None;
    }
    let cell = &SHARED[index];
    let _guard = cell.lock.lock();
    let shared = unsafe { *cell.inner.get() };
    if shared.live { Some(shared) } else { None }
}
