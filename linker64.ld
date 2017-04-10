ENTRY(_start)

SECTIONS
{
	.trampolinedata 0x5000 : {
		*(.trampolinedata)
	}

	.trampoline 0x6000 : {
		*(.trampoline)
	}

	. = 0x1000000;

	.multiboot : {
		*(.multiboot)
	}

	.text : {
		*(.text)
	}

	.rodata ALIGN(4K) : {
		*(.rodata)
	}

	.data ALIGN(4K) : {
		*(.data)
	}

	.bss ALIGN(4K) : {
		*(COMMON)
		*(.bss)
		*(.bootstrap_stack)
	}
}
