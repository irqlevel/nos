ENTRY(Start32)

SECTIONS
{
	.trampoline 0x8000 : {
		*(.trampoline)
	}

	.trampolinerodata 0x9000 : {
		*(.trampolinerodata)
	}

	.trampolinedata 0xA000 : {
		*(.trampolinedata)
	}

	/* The AP boot stacks in here are sized by MAX_CPUS (boot64.asm), so
	   this section grows with the CPU cap. It lives in low memory, which
	   the kernel shares with the BIOS data area, the EBDA and whatever
	   GRUB puts below 640K -- fail the build rather than discover the
	   overlap as a boot-time corruption. At 64 CPUs it ends at 0x55000. */
	ASSERT(. <= 0x80000, "trampolinedata (AP boot stacks) runs into the top of low memory")

	. = 0xFFFF800001000000;
	KernelStart = .;

	.multiboot : AT (ADDR (.multiboot) - 0xFFFF800000000000) {
		*(.multiboot)
	}

	.text ALIGN (4K) : AT (ADDR (.text) - 0xFFFF800000000000) {
		*(.text)
                *(.text.*)
		*(.ltext)
		*(.ltext.*)
	}

	/* W^X boundaries (page-aligned): [KernelStart, KernelText) = RX,
	   [KernelText, KernelRodata) = RO+NX, [KernelRodata, KernelEnd) = RW+NX */
	. = ALIGN(4K);
	KernelText = .;

	.rodata ALIGN(4K) : AT (ADDR (.rodata) - 0xFFFF800000000000) {
		*(.rodata)
		*(.rodata.*)
		*(.lrodata)
		*(.lrodata.*)
	}

	. = ALIGN(4K);
	KernelRodata = .;

	.data ALIGN(4K) : AT (ADDR (.data) - 0xFFFF800000000000) {
		*(.data)
                *(.data.*)
		*(.ldata)
		*(.ldata.*)
	}

	.bss ALIGN(4K) : AT (ADDR (.bss) - 0xFFFF800000000000) {
		*(COMMON)
		*(.bss)
                *(.bss.*)
		*(.lbss)
		*(.lbss.*)
	}

        KernelEnd = .;
}
