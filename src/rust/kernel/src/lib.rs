#![no_std]
#![feature(alloc_error_handler)]

extern crate alloc;

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
    #[cfg(target_arch = "x86_64")]
    {
        nvme::init();
        r8168::init();
        tco_init();
    }
}

#[cfg(target_arch = "x86_64")]
fn tco_init() {
    use kcore::tco_wdt::TcoWatchdog;
    use kcore::timer::Timer;
    use kcore::time::Duration;

    let wdt = match TcoWatchdog::probe() {
        Some(w) => w,
        None => {
            kcore::trace!(0, "TCO watchdog: not found");
            return;
        }
    };

    /* The timer callback holds a raw pointer to the TcoWatchdog; the watchdog
       is deliberately leaked so it lives for the kernel's lifetime. */
    let wdt_ptr = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(wdt));

    extern "C" fn kick_wdt(ctx: *mut u8) {
        use core::sync::atomic::{AtomicBool, Ordering};

        let wdt = unsafe { &*(ctx as *const TcoWatchdog) };

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
    match Timer::start(period, kick_wdt, wdt_ptr as *mut u8) {
        /* Dropping the handle would stop the timer (and the unkicked
           watchdog would then reset the machine); leak it so the kick
           runs for the kernel's lifetime. */
        Some(t) => {
            t.leak();
            unsafe { (&mut *wdt_ptr).start(30) };
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
}

/* Contract: rust_fini must be the last thing before halt.  Block/net
   registrations are permanent, so the C++ ops tables keep pointing at the
   devices freed here -- any I/O issued after this call is a use-after-free. */
#[no_mangle]
pub extern "C" fn rust_fini() {
    #[cfg(target_arch = "x86_64")]
    {
        nvme::shutdown();
        r8168::shutdown();
    }
}
