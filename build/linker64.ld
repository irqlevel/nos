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

	. = 0xFFFF800001000000;

	.multiboot : AT (ADDR (.multiboot) - 0xFFFF800000000000) {
		*(.multiboot)
	}

	.text ALIGN (4K) : AT (ADDR (.text) - 0xFFFF800000000000) {
		*(.text)
	}

	.rodata ALIGN(4K) : AT (ADDR (.rodata) - 0xFFFF800000000000) {
		*(.rodata)
	}

	.data ALIGN(4K) : AT (ADDR (.data) - 0xFFFF800000000000) {
		*(.data)
	}

	.bss ALIGN(4K) : AT (ADDR (.bss) - 0xFFFF800000000000) {
		*(COMMON)
		*(.bss)
	}
}
