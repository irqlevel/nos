TARGET64 = x86_64-none-elf
VERSION ?= dev
CPPFLAGS = -I$(CURDIR) -I$(CURDIR)/lib
CXXFLAGS = --target=$(TARGET64) -std=c++14 -g3 -ggdb3 -mno-sse -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -Wall -Wextra -Werror -mcmodel=kernel -mcmodel=large -mno-red-zone -DKERNEL_VERSION=\"$(VERSION)\"
LDFLAGS = -nostdlib -z max-page-size=4096
LD = ld
CC = clang
CXX = clang -x c++
ASM = nasm
AR = ar
MKRESCUE ?= $(shell which grub2-mkrescue grub-mkrescue 2> /dev/null | head -n1)

CXX_SRC =   \
    boot/grub.cpp    \
    drivers/serial.cpp  \
    drivers/pic.cpp \
    drivers/pit.cpp \
    drivers/8042.cpp    \
    drivers/acpi.cpp    \
    drivers/lapic.cpp   \
    drivers/ioapic.cpp  \
    drivers/vga.cpp \
    drivers/pci.cpp \
    drivers/virtqueue.cpp \
    drivers/virtio_pci.cpp \
    drivers/virtio_blk.cpp \
    drivers/virtio_net.cpp \
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
    kernel/gdt.cpp  \
    kernel/gdt_descriptor.cpp   \
    kernel/idt_descriptor.cpp   \
    kernel/idt.cpp  \
    kernel/cpu.cpp  \
    kernel/cmd.cpp  \
    kernel/block_device.cpp \
    net/net_device.cpp \
    net/arp.cpp \
    net/dhcp.cpp \
    net/icmp.cpp \
    kernel/softirq.cpp \
    kernel/exception.cpp    \
    kernel/dmesg.cpp    \
    kernel/sched.cpp    \
    kernel/preempt.cpp  \
    kernel/time.cpp \
    kernel/spin_lock.cpp \
    kernel/watchdog.cpp \
    kernel/object_table.cpp \
    kernel/parameters.cpp \
    kernel/raw_spin_lock.cpp \
    kernel/stack_trace.cpp \
    lib/stdlib.cpp  \
    lib/bitmap.cpp \
    lib/list_entry.cpp  \
    lib/error.cpp   \
    mm/memory_map.cpp   \
    mm/new.cpp  \
    mm/allocator.cpp   \
    mm/page_allocator.cpp  \
    mm/pool.cpp    \
    mm/page_table.cpp \
    mm/block_allocator.cpp \

ASM_SRC =    \
    boot/boot64.asm \
    kernel/asm.asm

OBJS = $(CXX_SRC:.cpp=.o) $(ASM_SRC:.asm=.o)
DEPS = $(CXX_SRC:.cpp=.d)

.PHONY: all check nocheck clean %.o

-include $(DEPS)

all: check nos.iso

nocheck: nos.iso

check: $(CXX_SRC)
	cppcheck --error-exitcode=22 -q . || exit 1

nos.iso: build/grub.cfg kernel64.elf
	rm -rf iso
	rm -rf bin
	mkdir -p iso/boot/grub
	mkdir -p bin
	cp kernel64.elf iso/boot/kernel64.elf
	cp kernel64.elf bin/kernel64.elf
	cp build/grub.cfg iso/boot/grub/grub.cfg
	$(MKRESCUE) -o nos.iso iso
	rm -rf iso

%.o: %.asm
	$(ASM) -felf64 $< -o $@
%.o: %.cpp
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -MMD -c $< -o $@

kernel64.elf: build/linker64.ld $(OBJS)
	$(LD) $(LDFLAGS) -T $< -o kernel64.elf $(OBJS)
clean:
	rm -rf $(OBJS) $(DEPS) *.elf *.bin *.iso iso
