ENTRY(_start)

SECTIONS
{
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
