CPPFLAGS = -I$(CURDIR) -std=c++11 -mcmodel=kernel -g3 -ggdb3 -mno-sse -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -Wall -Wextra -Werror -mcmodel=large -mno-red-zone -mcmodel=large
LFLAGS = -nostdlib -z max-page-size=4096
TARGET64 = x86_64-none-elf
CC = clang
CPP = clang -x c++
ASM = nasm
AR = ar

CPP_SRC =   \
    drivers/serial.cpp  \
    drivers/pic.cpp \
    drivers/pit.cpp \
    drivers/8042.cpp    \
    drivers/acpi.cpp    \
    drivers/lapic.cpp   \
    drivers/ioapic.cpp  \
    drivers/vga.cpp \
    kernel/icxxabi.cpp    \
    kernel/interrupt.cpp   \
    kernel/task.cpp \
    kernel/test.cpp \
    kernel/main.cpp \
    kernel/trace.cpp \
    kernel/timer.cpp    \
    kernel/panic.cpp    \
    kernel/debug.cpp    \
    kernel/atomic.cpp   \
    kernel/idt_descriptor.cpp   \
    kernel/idt.cpp  \
    kernel/cpu.cpp  \
    kernel/cmd.cpp  \
    kernel/exception.cpp    \
    kernel/dmesg.cpp    \
    lib/stdlib.cpp  \
    lib/list_entry.cpp  \
    lib/error.cpp   \
    mm/memory_map.cpp   \
    mm/new.cpp  \
    mm/sallocator.cpp   \
    mm/spage_allocator.cpp  \
    mm/spool.cpp    \

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
	$(ASM) -felf64 boot/boot64.asm -o boot64.o
	$(ASM) -felf64 kernel/asm.asm -o asm.o
	$(CPP) $(CPPFLAGS) --target=$(TARGET64) -c $(CPP_SRC)
	ld $(LFLAGS) -T linker64.ld -o kernel64.elf *.o
	rm -rf *.o

clean:
	rm -rf *.o *.elf *.bin *.iso iso
