TARGET64 = x86_64-none-elf
CPPFLAGS = -I$(CURDIR)
CXXFLAGS = --target=$(TARGET64) -std=c++11 -g3 -ggdb3 -mno-sse -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -Wall -Wextra -Werror -mcmodel=kernel -mcmodel=large -mno-red-zone
LDFLAGS = -nostdlib -z max-page-size=4096
LD = ld
CC = clang
CXX = clang -x c++
ASM = nasm
AR = ar
MKRESCUE ?= $(shell which grub2-mkrescue grub-mkrescue 2> /dev/null | head -n1)

CXX_SRC =   \
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
    kernel/sched.cpp    \
    kernel/preempt.cpp  \
    lib/stdlib.cpp  \
    lib/list_entry.cpp  \
    lib/error.cpp   \
    mm/memory_map.cpp   \
    mm/new.cpp  \
    mm/sallocator.cpp   \
    mm/spage_allocator.cpp  \
    mm/spool.cpp    \

ASM_SRC =    \
    boot/boot64.asm \
    kernel/asm.asm

OBJS = $(CXX_SRC:.cpp=.o) $(ASM_SRC:.asm=.o)

.PHONY: all check clean %.o

all: clean check nos.iso

check: $(CXX_SRC)
	cppcheck --error-exitcode=22 -q . || exit 1

nos.iso: build/grub.cfg kernel64.elf
	rm -rf iso
	rm -rf bin
	mkdir -p iso/boot/grub
	mkdir -p bin
	cp kernel64.elf iso/boot/kernel64.elf
	cp kernel64.elf bin/kernel64.elf
	rm -rf kernel64.elf
	cp build/grub.cfg iso/boot/grub/grub.cfg
	$(MKRESCUE) -o nos.iso iso
	rm -rf iso

%.o: %.asm
	$(ASM) -felf64 $< -o $@
%.o: %.cpp
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -c $< -o $@

kernel64.elf: build/linker64.ld $(OBJS)
	$(LD) $(LDFLAGS) -T $< -o kernel64.elf $(OBJS)
clean:
	rm -rf $(OBJS) *.elf *.bin *.iso iso
