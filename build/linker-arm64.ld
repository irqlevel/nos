ENTRY(Start64)

/* QEMU virt: RAM at 0x40000000. The Linux Image header requests
   text_offset 0x200000, so the kernel is loaded at phys 0x40200000.
   Virtual layout keeps the linear-map invariant VA = phys + KernelSpaceBase
   (0xFFFF800000000000), mirroring the x86 script. */

SECTIONS
{
    . = 0xFFFF800040200000;
    KernelStart = .;

    .head.text : AT (ADDR(.head.text) - 0xFFFF800000000000)
    {
        KEEP(*(.head.text))
    }

    .text : AT (ADDR(.text) - 0xFFFF800000000000)
    {
        *(.text)
        *(.text.*)
    }

    /* W^X boundaries (page-aligned): [KernelStart, KernelText) = RX,
       [KernelText, KernelRodata) = RO+NX, [KernelRodata, KernelEnd) = RW+NX. */
    . = ALIGN(4K);
    KernelText = .;

    .rodata : AT (ADDR(.rodata) - 0xFFFF800000000000)
    {
        *(.rodata)
        *(.rodata.*)
    }

    . = ALIGN(4K);
    KernelRodata = .;

    .data : AT (ADDR(.data) - 0xFFFF800000000000)
    {
        *(.data)
        *(.data.*)
        *(.got)
        *(.got.plt)
    }

    . = ALIGN(4K);
    .bss : AT (ADDR(.bss) - 0xFFFF800000000000)
    {
        __bss_start = .;
        *(.bss)
        *(.bss.*)
        *(COMMON)
        . = ALIGN(4K);
        __bss_end = .;
    }

    . = ALIGN(4K);
    KernelEnd = .;

    _image_size = KernelEnd - KernelStart;

    /DISCARD/ :
    {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
