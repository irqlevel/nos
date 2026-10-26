ENTRY(Start64)

/* QEMU virt: RAM at 0x40000000. The Linux Image header requests
   text_offset 0x200000, so the kernel is loaded at phys 0x40200000.
   Virtual layout keeps the linear-map invariant VA = phys + KernelSpaceBase
   (0xFFFF800000000000), mirroring the x86 script. */

SECTIONS
{
    . = 0xFFFF800040200000;
    kernel_start = .;

    .head.text : AT (ADDR(.head.text) - 0xFFFF800000000000)
    {
        KEEP(*(.head.text))
    }

    .text : AT (ADDR(.text) - 0xFFFF800000000000)
    {
        *(.text)
        *(.text.*)
    }

    /* W^X boundaries (page-aligned): [kernel_start, kernel_text) = RX,
       [kernel_text, kernel_rodata) = RO+NX, [kernel_rodata, kernel_end) = RW+NX. */
    . = ALIGN(4K);
    kernel_text = .;

    .rodata : AT (ADDR(.rodata) - 0xFFFF800000000000)
    {
        *(.rodata)
        *(.rodata.*)
    }

    . = ALIGN(4K);
    kernel_rodata = .;

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
    kernel_end = .;

    _image_size = kernel_end - kernel_start;

    /* Static constructors and destructors, which this kernel never runs: a
       global whose initialisation needs code at startup is left as zeroes,
       right by coincidence at best -- Pci's config lock and the arm64
       console's went unregistered with the watchdog that way. Collected
       where nothing loads them, to be counted: one is a failed link. Such
       a global wants a constexpr constructor and a trivial destructor, or
       to be a function-local static (CLAUDE.md, "Static constructors are
       unreliable"). */
    .static_init (INFO) : {
        *(.preinit_array)
        *(.init_array) *(.init_array.*)
        *(.ctors) *(.ctors.*)
        *(.fini_array) *(.fini_array.*)
        *(.dtors) *(.dtors.*)
    }
    ASSERT(SIZEOF(.static_init) == 0, "a static constructor or destructor: this kernel runs none (CLAUDE.md)")

    /DISCARD/ :
    {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
