//! Port I/O -- how every device of hv's this reaches is reached: the 8250,
//! PCI's configuration mechanism, virtio's legacy registers, ACPI's PM1
//! control -- the time-stamp counter, and turning the guest off.
//!
//! A port access touches no memory of this program's, so these are safe to
//! call: what a device does in answer is the device's, and the only device
//! that writes memory, the virtio disk, writes only buffers `virtio` hands
//! it. Each is a compiler barrier as well -- none is `nomem` -- since a port
//! access is where a device is told to read memory (virtio's notify) or is
//! asked what it has written (its ISR): no store may be moved past the one,
//! nor a load of what the device wrote before the other.

use core::arch::asm;

pub fn outb(port: u16, v: u8) {
    // SAFETY: a port write; no memory of this program's is touched.
    unsafe { asm!("out dx, al", in("dx") port, in("al") v, options(nostack, preserves_flags)) }
}

pub fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: a port read, into a register.
    unsafe { asm!("in al, dx", in("dx") port, out("al") v, options(nostack, preserves_flags)) }
    v
}

pub fn outw(port: u16, v: u16) {
    // SAFETY: as `outb`.
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") v, options(nostack, preserves_flags)) }
}

pub fn inw(port: u16) -> u16 {
    let v: u16;
    // SAFETY: as `inb`.
    unsafe { asm!("in ax, dx", in("dx") port, out("ax") v, options(nostack, preserves_flags)) }
    v
}

pub fn outl(port: u16, v: u32) {
    // SAFETY: as `outb`.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") v, options(nostack, preserves_flags)) }
}

pub fn inl(port: u16) -> u32 {
    let v: u32;
    // SAFETY: as `inb`.
    unsafe { asm!("in eax, dx", in("dx") port, out("eax") v, options(nostack, preserves_flags)) }
    v
}

/// The time-stamp counter, for timeouts; its rate is what hv tells the
/// guest in `tsc_early_khz=`, when it does.
pub fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: reads the counter into two registers.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) }
    (u64::from(hi) << 32) | u64::from(lo)
}

/* ACPI's PM1 control register, where hv's machine has it, and the write
 * that puts the guest in S5: SLP_TYP 5 -- the one sleep state its DSDT's
 * \_S5 names -- and SLP_EN (src/rust/hv/src/devices/pm.rs). */
const PM1_CNT: u16 = 0x604;
const SLP_TYP_S5: u16 = 5 << 10;
const SLP_EN: u16 = 1 << 13;

/// Turn the guest off, as its ACPI says to -- hv stops the VM with "the
/// guest powered itself off" -- or, should that not stop it, halt for good.
pub fn power_off() -> ! {
    outw(PM1_CNT, SLP_TYP_S5 | SLP_EN);
    loop {
        // SAFETY: interrupts off and halted; nothing runs after.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}
