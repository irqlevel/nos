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
}
