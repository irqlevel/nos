/* hvboot's link: one image at 1 MiB, the lowest a PVH image may be put by
 * hv (src/crates/hv/src/pvh.rs: below it are the start info, the command line,
 * the MP and the ACPI tables), its 32-bit entry first. The Xen note that
 * names the entry is a segment of its own, PT_NOTE, which is where hv and
 * every other PVH loader look for it. Nothing in it is position-independent:
 * it runs where it is linked, identity-mapped. */

ENTRY(pvh_entry)

PHDRS
{
    image PT_LOAD FLAGS(7);
    note PT_NOTE FLAGS(4);
}

SECTIONS
{
    . = 0x100000;
    __image_start = .;

    .text : {
        KEEP(*(.text.pvh_entry))
        *(.text .text.*)
    } :image

    .rodata : {
        *(.rodata .rodata.*)
    } :image

    .data : {
        *(.data .data.*)
    } :image

    .note.Xen : {
        KEEP(*(.note.Xen))
    } :image :note

    .bss (NOLOAD) : {
        . = ALIGN(4096);
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4096);
        __bss_end = .;
    } :image

    __image_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr .comment .note.GNU-stack .note.gnu.*)
    }
}
