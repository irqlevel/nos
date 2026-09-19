#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

mod sha256;

use ffi::alloc::KernelAllocator;

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    ffi::panic::panic_handler(info)
}

#[alloc_error_handler]
fn alloc_error(_layout: core::alloc::Layout) -> ! {
    ffi::panic::alloc_error()
}

#[no_mangle]
pub extern "C" fn rust_init() {
    hello::hello();
    /* The partition table reader: its own entry point is called from the
       boot path, and this puts its shell command in front of whoever runs
       one. */
    block::init();
    fs::init();
    net::init();
    sha256::init();
    nvme::init();
    usb::init();
    virtio_rng::init();
    virtio_blk::init();
    virtio_net::init();
    virtio_scsi::init();
    r8168::init();
    r8125::init();
    igb::init();
    #[cfg(target_arch = "x86_64")]
    {
        tco_init();
    }
}

#[cfg(target_arch = "x86_64")]
fn tco_init() {
    use kcore::tco_wdt::TcoWatchdog;
    use kcore::timer::Timer;
    use kcore::time::Duration;

    /* An ACPI WDAT table means the firmware owns the watchdog and expects
       the OS to drive it through WDAT instructions -- which nos does not
       implement.  Grabbing the TCO registers underneath it would be fighting
       the platform over the same hardware, so stand down; Linux skips the
       native iTCO driver on these systems for the same reason. */
    if kcore::acpi::has_firmware_watchdog() {
        kcore::trace!(0, "TCO watchdog: ACPI WDAT present, firmware owns the watchdog");
        return;
    }

    let wdt = match TcoWatchdog::probe() {
        Some(w) => w,
        None => {
            kcore::trace!(0, "TCO watchdog: not found");
            return;
        }
    };

    kcore::trace!(0, "TCO watchdog: TCOBASE 0x{:04X} via {}{}",
        wdt.base(), wdt.source().as_str(),
        if wdt.source().no_reboot_cleared() { "" } else { ", NO_REBOOT left as firmware set it" });

    /* The kick timer looks at the watchdog for as long as the kernel runs,
       so that is how long the watchdog lives. */
    let wdt: &'static TcoWatchdog = alloc::boxed::Box::leak(alloc::boxed::Box::new(wdt));

    fn kick_wdt(wdt: &'static TcoWatchdog) {
        use core::sync::atomic::{AtomicBool, Ordering};

        /* ~16 ticks (0.6s each) have elapsed since the last kick, so a
           count still at the armed value means the timer never ran
           (chipset-specific NO_REBOOT clearing failed) and a hang will
           not reset the machine.  Warn once. */
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !wdt.is_counting() && !WARNED.swap(true, Ordering::Relaxed) {
            kcore::trace!(0, "TCO watchdog: timer not counting, reset-on-hang is not armed");
        }

        wdt.kick();
    }

    /* Secure the kick timer BEFORE arming the hardware: a timer failure
       after start() would leave the watchdog counting with nobody kicking
       it -- a spontaneous, unattributable hard reset ~30-60s into boot on
       machines where NO_REBOOT was cleared. Kick every 10 seconds against
       a 30-second timeout. */
    let period = Duration::from_secs(10);
    match Timer::start_for(period, wdt, kick_wdt) {
        /* Dropping the handle would stop the timer (and the unkicked
           watchdog would then reset the machine); leak it so the kick
           runs for the kernel's lifetime. */
        Some(t) => {
            t.leak();
            wdt.start(30);
            kcore::trace!(0, "TCO watchdog: started, timeout=30s");
        }
        None => {
            kcore::trace!(0, "TCO watchdog: kick timer unavailable, not arming");
        }
    }
}

#[no_mangle]
pub extern "C" fn rust_test() {
    hello::test();
    sha256::selftest();
}

/* Contract: rust_fini must be the last thing before halt.  Block/net
   registrations are permanent, so the C++ ops tables keep pointing at the
   devices freed here -- any I/O issued after this call is a use-after-free. */
#[no_mangle]
pub extern "C" fn rust_fini() {
    nvme::shutdown();
    r8168::shutdown();
    r8125::shutdown();
    igb::shutdown();
}
