//! hvboot: the loader a guest of nos's hypervisor is started in, by PVH, to
//! boot the kernel its own disk's GRUB would. A distribution's cloud image
//! then boots as it ships -- its kernel updates included -- with no kernel
//! or initrd taken out of it and kept beside it as nos's files
//! (docs/hypervisor.md, "A guest's own kernel").
//!
//! What it does, in order:
//! - the PVH entry (`pvh_entry`, below): 32-bit protected mode, paging off;
//!   it clears its BSS, identity-maps the first 4 GiB, turns long mode on
//!   and calls [`main`] with the start info hv left in `EBX`;
//! - reads the start info: the command line hv gave it, the memory map, the
//!   ACPI RSDP (`start`);
//! - finds the guest's disks -- legacy virtio-blk on PCI bus 0, hv's -- and
//!   on them, partition by partition, an ext2/3/4 filesystem with GRUB's
//!   configuration (`pci`, `virtio`, `gpt`, `ext`);
//! - picks the entry GRUB would boot -- its default, or grubenv's next or
//!   saved one -- and reads its kernel and initrd (`grub`);
//! - boots the kernel by Linux's 64-bit boot protocol, its command line the
//!   entry's and then hv's (`bzimage`, `boot`).
//!
//! Anything it cannot do it says on the console and turns the guest off: a
//! VM whose loader failed stops, the reason in its console, rather than
//! sitting halted with nothing said. It runs in the guest: nothing in it is
//! trusted by the host, and what it reads -- partition tables, filesystems,
//! GRUB's scripts -- is the guest's own.
//!
//! The parsers (`gpt`, `ext`, `grub`, `bzimage`) are plain Rust over byte
//! slices and `alloc`, with no `unsafe`; the rest -- ports, physical memory,
//! the jump -- is the platform, and `unsafe` is there alone.
#![no_std]

extern crate alloc;

#[macro_use]
mod console;
mod boot;
mod bzimage;
mod crc32;
mod disk;
mod ext;
mod gpt;
mod grub;
mod heap;
mod idt;
mod io;
mod pci;
mod phys;
mod start;
mod virtio;

use core::arch::global_asm;

/* The PVH entry: 32-bit protected mode with flat segments, paging off, EBX
 * the start info's physical address (src/crates/hv/src/pvh.rs, Xen's
 * docs/misc/pvh.pandoc). It clears the BSS -- page tables, stack, heap --
 * builds an identity map of the first 4 GiB in 2 MiB pages, turns on PAE,
 * long mode and paging, loads a GDT whose 64-bit code and data are at 0x10
 * and 0x18 -- the selectors Linux's 64-bit boot protocol asks for, which
 * the kernel is entered on -- and calls `main` with the start info.
 *
 * The note after it names the entry, which is how hv finds it: owner "Xen",
 * type XEN_ELFNOTE_PHYS32_ENTRY (18), a 4-byte physical address. */
global_asm!(
    r#"
    .section .text.pvh_entry, "ax"
    .code32
    .globl pvh_entry
pvh_entry:
    cli
    cld
    mov esi, ebx

    mov edi, offset __bss_start
    mov ecx, offset __bss_end
    sub ecx, edi
    xor eax, eax
    rep stosb

    mov eax, offset boot_pdpt
    or eax, 3
    mov dword ptr [boot_pml4], eax

    xor ecx, ecx
2:
    mov eax, ecx
    shl eax, 12
    add eax, offset boot_pd
    or eax, 3
    mov dword ptr [boot_pdpt + ecx * 8], eax
    inc ecx
    cmp ecx, 4
    jb 2b

    xor ecx, ecx
3:
    mov eax, ecx
    shl eax, 21
    or eax, 0x83
    mov dword ptr [boot_pd + ecx * 8], eax
    inc ecx
    cmp ecx, 2048
    jb 3b

    mov eax, offset boot_pml4
    mov cr3, eax
    mov eax, cr4
    or eax, 0x20
    mov cr4, eax
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x100
    wrmsr
    mov eax, cr0
    or eax, 0x80000001
    mov cr0, eax

    lgdt [boot_gdt_ptr]
    jmp fword ptr [boot_far_long]

    .code64
pvh_long:
    mov ax, 0x18
    mov ds, ax
    mov es, ax
    mov ss, ax
    xor eax, eax
    mov fs, ax
    mov gs, ax
    lea rsp, [rip + boot_stack_top]
    mov edi, esi
    call hvboot_main
4:
    cli
    hlt
    jmp 4b

    .section .rodata.boot_gdt, "a"
    .balign 16
boot_gdt:
    .quad 0
    .quad 0
    .quad 0x00AF9B000000FFFF
    .quad 0x00CF93000000FFFF
boot_gdt_end:
boot_gdt_ptr:
    .word boot_gdt_end - boot_gdt - 1
    .quad boot_gdt
    /* The far jump into long mode: the 64-bit code at 0x10, as a 32-bit
     * offset and a selector. */
boot_far_long:
    .long pvh_long
    .word 0x10

    .section .note.Xen, "a"
    .balign 4
    .long 4
    .long 4
    .long 18
    .asciz "Xen"
    .long pvh_entry

    .section .bss.boot, "aw", @nobits
    .balign 4096
boot_pml4:
    .skip 4096
boot_pdpt:
    .skip 4096
boot_pd:
    .skip 4096 * 4
    .balign 16
boot_stack:
    .skip 128 * 1024
boot_stack_top:
"#
);

/// Where the image is: from its first byte to the end of its BSS, as the
/// linker script lays it out -- memory a kernel or an initrd must not be
/// read over while this still runs.
pub fn image_range() -> (u64, u64) {
    extern "C" {
        static __image_start: u8;
        static __image_end: u8;
    }
    /* Only the symbols' addresses are taken: nothing is read through them. */
    let start = core::ptr::addr_of!(__image_start) as u64;
    let end = core::ptr::addr_of!(__image_end) as u64;
    (start, end)
}

/// The Rust side of the entry: long mode, the identity map, a stack of its
/// own. Never returns: it boots a kernel, or turns the guest off.
#[no_mangle]
extern "C" fn hvboot_main(start_info: u32) -> ! {
    idt::install();
    match boot::run(u64::from(start_info)) {
        Ok(never) => match never {},
        Err(why) => {
            say!("cannot boot: {}", why);
            io::power_off()
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    say!("panic: {}", info);
    io::power_off()
}
