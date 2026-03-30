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
    nvme::init();
    r8168::init();
    tco_init();
}

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

    /* Start with a 30-second timeout */
    wdt.start(30);
    kcore::trace!(0, "TCO watchdog: started, timeout=30s");

    /* Kick every 10 seconds from a periodic timer.
       The timer callback holds a raw pointer to the TcoWatchdog; the watchdog
       is deliberately leaked so it lives for the kernel's lifetime. */
    let wdt_ptr = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(wdt));

    extern "C" fn kick_wdt(ctx: *mut u8) {
        let wdt = unsafe { &*(ctx as *const TcoWatchdog) };
        wdt.kick();
    }

    let period = Duration::from_secs(10);
    if Timer::start(period, kick_wdt, wdt_ptr as *mut u8).is_none() {
        kcore::trace!(0, "TCO watchdog: failed to start kick timer");
    }
}

#[no_mangle]
pub extern "C" fn rust_test() {
    hello::test();
}

#[no_mangle]
pub extern "C" fn rust_fini() {
    nvme::shutdown();
    r8168::shutdown();
}
