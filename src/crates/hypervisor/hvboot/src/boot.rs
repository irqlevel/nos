//! The boot, in order: what hv handed over; the virtio disks; what to boot
//! off them and where it goes in RAM, which `plan` works out with no
//! platform in it -- the harness runs the same over a disk image; that
//! kernel and initrd read into RAM; the zero page; and the jump into the
//! kernel.

use alloc::string::String;
use alloc::vec::Vec;
use core::convert::Infallible;

use crate::bzimage::{self, ENTRY64_OFFSET, ZERO_PAGE_BYTES};
use crate::io::rdtsc;
use crate::options::Options;
use crate::plan::{self, letter};
use crate::ram::{Ram, E820_RAM};
use crate::start::StartInfo;
use crate::virtio::Disk;
use crate::{phys, ram};

fn ms(cycles: u64, khz: Option<u64>) -> u64 {
    khz.map_or(0, |k| cycles / k.max(1))
}

pub fn run(start_info: u64) -> Result<Infallible, String> {
    let si = StartInfo::read(start_info)?;
    let opts = Options::parse(&si.cmdline)?;
    let ram_bytes: u64 = si.memmap.iter().filter(|e| e.kind == E820_RAM).map(|e| e.size).sum();
    say!("started by PVH: {} MiB of RAM, its command line \"{}\"", ram_bytes >> 20, si.cmdline);
    for (addr, len) in &si.modules {
        say!("a module of {} KiB at {:#x} -- an initrd= given to hv -- is not used: the entry's own initrds are, \
              and the RAM it is in may be given to them", len >> 10, addr);
    }
    let mut ram = Ram::new(&si.memmap, crate::image_range());
    let t0 = rdtsc();

    let found = crate::pci::virtio_disks();
    if found.is_empty() {
        return Err(String::from("no virtio disk on the guest's PCI bus: give the VM a disk= with its image"));
    }
    let mut disks = Vec::new();
    for (i, d) in found.iter().enumerate() {
        let disk = Disk::new(d.io_base, letter(i), opts.tsc_khz)?;
        if opts.verbose {
            say!("vd{}: {} MiB{}, PCI slot {}", letter(i), disk.sectors >> 11,
                 if disk.read_only { ", read-only" } else { "" }, d.slot);
        }
        disks.push(disk);
    }

    let (plan, place) = plan::plan(&mut disks, &opts, &mut ram)?;
    let (kaddr, initrd_at) = (place.kernel.addr(), place.initrd.as_ref().map_or(0, ram::Loan::addr));
    let kernel = phys::lend(place.kernel)?;
    let initrd: &mut [u8] = match place.initrd {
        Some(loan) => phys::lend(loan)?,
        None => &mut [],
    };
    plan::load(&mut disks, &plan, kernel, initrd)?;

    /* The command line and the zero page are the kernel's from the jump on:
     * kept for good, in the loader's heap -- RAM the kernel's memory map
     * has, which its decompressor stays clear of until it has copied them. */
    let mut cmdline = Vec::with_capacity(plan.line.len() + 1);
    cmdline.extend_from_slice(plan.line.as_bytes());
    cmdline.push(0);
    let cmdline: &'static [u8] = cmdline.leak();
    let page: &'static mut [u8; ZERO_PAGE_BYTES] = alloc::boxed::Box::leak(alloc::boxed::Box::new([0u8; ZERO_PAGE_BYTES]));
    bzimage::zero_page(&plan.first, &plan.header, cmdline.as_ptr() as u64, plan.line.len(),
                       (initrd_at, plan.initrd_len), &si.memmap, si.rsdp, page)?;

    let t1 = rdtsc();
    let h = &plan.header;
    say!("{}: boot protocol {}.{:02}, {} KiB at {:#x}{}, initrd {} KiB at {:#x}, read in {} ms", plan.kernel_name,
         h.version >> 8, h.version & 0xFF, plan.kernel_len >> 10, kaddr,
         if h.relocatable { "" } else { " (not relocatable)" }, plan.initrd_len >> 10, initrd_at,
         ms(t1.wrapping_sub(t0), opts.tsc_khz));
    say!("its command line: {}", plan.line);
    for d in &disks {
        d.reset();
    }
    jump(kaddr + ENTRY64_OFFSET, page.as_ptr() as u64)
}

/// Into the kernel: its 64-bit entry, `RSI` at the zero page, interrupts
/// off, the GDT's 0x10 and 0x18 the code and data it runs on.
fn jump(entry: u64, zero_page: u64) -> ! {
    // SAFETY: the kernel is in RAM at its load address, with the room its
    // init_size asks for; the zero page, the command line and the initrd it
    // points at are in memory nothing will reuse before the kernel copies
    // them; the identity map covers all of it; and nothing of the loader
    // runs after the jump.
    unsafe {
        core::arch::asm!("cli", "xor ebp, ebp", "jmp rax", in("rax") entry, in("rsi") zero_page,
                         options(noreturn))
    }
}
