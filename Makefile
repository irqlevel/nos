CPPFLAGS = -std=c++11 -mcmodel=kernel -g3 -ggdb3 -mno-sse -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -Wall -Wextra -Werror -mcmodel=large -mno-red-zone -mcmodel=large
LFLAGS = -nostdlib -z max-page-size=4096
TARGET64 = x86_64-none-elf
CC = clang
CPP = clang -x c++
ASM = nasm
AR = ar

CPP_SRC = icxxabi.cpp list_entry.cpp new.cpp sallocator.cpp spage_allocator.cpp spool.cpp vga.cpp main.cpp \
	trace.cpp stdlib.cpp panic.cpp debug.cpp error.cpp atomic.cpp 8042.cpp idt_descriptor.cpp idt.cpp \
	memory_map.cpp test.cpp serial.cpp pic.cpp exception.cpp pit.cpp timer.cpp acpi.cpp cpu.cpp cmd.cpp \
	dmesg.cpp lapic.cpp ioapic.cpp interrupt.cpp

all: check nos.iso

check: $(CPP_SRC)
	cppcheck --error-exitcode=22 -q . || exit 1

nos.iso: kernel64.elf
	rm -rf iso
	rm -rf bin
	mkdir -p iso/boot/grub
	mkdir -p bin
	cp kernel64.elf iso/boot/kernel64.elf
	cp kernel64.elf bin/kernel64.elf
	rm -rf kernel64.elf
	cp grub.cfg iso/boot/grub/grub.cfg
	grub2-mkrescue -o nos.iso iso
	rm -rf iso

kernel64.elf: $(CPP_SRC)
	rm -rf *.o
	$(ASM) -felf64 boot64.asm
	$(ASM) -felf64 asm.asm
	$(CPP) $(CPPFLAGS) --target=$(TARGET64) -c $(CPP_SRC)
	ld $(LFLAGS) -T linker64.ld -o kernel64.elf *.o

clean:
	rm -rf *.o *.elf *.bin *.iso iso
