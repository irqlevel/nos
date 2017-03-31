CFLAGS = -std=gnu99 -g3 -ggdb3 -mno-sse -ffreestanding -nostdlib -fno-builtin -Wall -Wextra -Werror
CPPFLAGS = -std=c++11 -mcmodel=kernel -g3 -ggdb3 -mno-sse -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -Wall -Wextra -Werror

LFLAGS = -ffreestanding -nostdlib -fno-builtin
TARGET = i386-none-elf
CC = clang
CPP = clang -x c++
ASM = nasm
AR = ar

CPP_SRC = icxxabi.cpp list_entry.cpp new.cpp sallocator.cpp spage_allocator.cpp spool.cpp vga.cpp kernel.cpp \
	trace.cpp stdlib.cpp panic.cpp debug.cpp error.cpp atomic.cpp cpu_state.cpp \
	gdt_descriptor.cpp gdt.cpp test.cpp memory_map.cpp


all: kernel.iso

kernel.iso: kernel.elf
	rm -rf iso
	rm -rf bin
	mkdir -p iso/boot/grub
	mkdir -p bin
	cp kernel.elf iso/boot/kernel.elf
	cp kernel.elf bin/kernel.elf
	rm -rf kernel.elf
	cp grub.cfg iso/boot/grub/grub.cfg
	grub2-mkrescue -o kernel.iso iso
	rm -rf iso

kernel.elf: $(CPP_SRC)
		rm -rf *.o
		$(ASM) -felf32 boot.asm
		$(ASM) -felf32 helpers32.asm
		$(CPP) $(CPPFLAGS) --target=$(TARGET) -c $(CPP_SRC)
		$(CC) $(LFLAGS) --target=$(TARGET) -T linker.ld -o kernel.elf *.o

clean:
	rm -rf *.o *.elf *.bin *.iso iso
