TARGET64 = x86_64-none-elf
VERSION ?= dev
CPPFLAGS = -I$(CURDIR)/src/cpp -I$(CURDIR)/src/cpp/lib
CXXFLAGS = --target=$(TARGET64) -std=c++20 -g3 -ggdb3 -mno-sse -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -fno-omit-frame-pointer -Wall -Wextra -Werror -mcmodel=kernel -mcmodel=large -mno-red-zone -DKERNEL_VERSION=\"$(VERSION)\"
LDFLAGS = -nostdlib -z max-page-size=4096
LD = ld
CC = clang
CXX = clang -x c++
ASM = nasm
NM = nm
AR = ar
MKRESCUE ?= $(shell which grub2-mkrescue grub-mkrescue 2> /dev/null | head -n1)

CXX_SRC =   \
    src/cpp/boot/grub.cpp    \
    src/cpp/drivers/serial.cpp  \
    src/cpp/drivers/pic.cpp \
    src/cpp/drivers/pit.cpp \
    src/cpp/drivers/hpet.cpp \
    src/cpp/drivers/8042.cpp    \
    src/cpp/drivers/acpi.cpp    \
    src/cpp/drivers/lapic.cpp   \
    src/cpp/drivers/ioapic.cpp  \
    src/cpp/drivers/vga.cpp \
    src/cpp/drivers/pci.cpp \
    src/cpp/drivers/msix.cpp \
    src/cpp/drivers/virtqueue.cpp \
    src/cpp/drivers/virtio_pci.cpp \
    src/cpp/drivers/virtio_blk.cpp \
    src/cpp/drivers/virtio_scsi.cpp \
    src/cpp/drivers/virtio_net.cpp \
    src/cpp/drivers/virtio_rng.cpp \
    src/cpp/drivers/rtc.cpp \
    src/cpp/kernel/icxxabi.cpp    \
    src/cpp/kernel/interrupt.cpp   \
    src/cpp/kernel/task.cpp \
    src/cpp/kernel/test.cpp \
    src/cpp/kernel/main.cpp \
    src/cpp/kernel/trace.cpp \
    src/cpp/kernel/timer.cpp    \
    src/cpp/kernel/panic.cpp    \
    src/cpp/kernel/debug.cpp    \
    src/cpp/kernel/atomic.cpp   \
    src/cpp/kernel/gdt.cpp  \
    src/cpp/kernel/gdt_descriptor.cpp   \
    src/cpp/kernel/idt_descriptor.cpp   \
    src/cpp/kernel/idt.cpp  \
    src/cpp/kernel/cpu.cpp  \
    src/cpp/kernel/cmd.cpp  \
    src/cpp/block/block_device.cpp \
    src/cpp/block/partition.cpp \
    src/cpp/net/net_device.cpp \
    src/cpp/net/net_frame.cpp \
    src/cpp/net/arp.cpp \
    src/cpp/net/dhcp.cpp \
    src/cpp/net/icmp.cpp \
    src/cpp/net/udp_shell.cpp \
    src/cpp/net/dns.cpp \
    src/cpp/net/tcp.cpp \
    src/cpp/net/http.cpp \
    src/cpp/fs/vfs.cpp \
    src/cpp/fs/ramfs.cpp \
    src/cpp/fs/block_io.cpp \
    src/cpp/fs/nanofs.cpp \
    src/cpp/fs/ext2.cpp \
    src/cpp/fs/procfs.cpp \
    src/cpp/kernel/mutex.cpp \
    src/cpp/kernel/wait_group.cpp \
    src/cpp/kernel/softirq.cpp \
    src/cpp/kernel/exception.cpp    \
    src/cpp/kernel/dmesg.cpp    \
    src/cpp/kernel/sched.cpp    \
    src/cpp/kernel/preempt.cpp  \
    src/cpp/kernel/time.cpp \
    src/cpp/kernel/tsc.cpp \
    src/cpp/kernel/spin_lock.cpp \
    src/cpp/kernel/watchdog.cpp \
    src/cpp/kernel/object_table.cpp \
    src/cpp/kernel/parameters.cpp \
    src/cpp/kernel/raw_spin_lock.cpp \
    src/cpp/kernel/raw_rw_spin_lock.cpp \
    src/cpp/kernel/rw_mutex.cpp \
    src/cpp/kernel/stack_trace.cpp \
    src/cpp/kernel/symtab.cpp \
    src/cpp/kernel/entropy.cpp \
    src/cpp/kernel/rust_ffi.cpp \
    src/cpp/lib/stdlib.cpp  \
    src/cpp/lib/format.cpp \
    src/cpp/lib/bitmap.cpp \
    src/cpp/lib/checksum.cpp \
    src/cpp/lib/list_entry.cpp  \
    src/cpp/lib/error.cpp   \
    src/cpp/mm/memory_map.cpp   \
    src/cpp/mm/new.cpp  \
    src/cpp/mm/allocator.cpp   \
    src/cpp/mm/page_allocator.cpp  \
    src/cpp/mm/va_allocator.cpp \
    src/cpp/mm/pool.cpp    \
    src/cpp/mm/page_table.cpp \
    src/cpp/mm/block_allocator.cpp \

ASM_SRC =    \
    src/cpp/boot/boot64.asm \
    src/cpp/kernel/asm.asm \
    src/cpp/lib/stdlib_asm.asm

OBJS = $(CXX_SRC:.cpp=.o) $(ASM_SRC:.asm=.o)
DEPS = $(CXX_SRC:.cpp=.d)

RUST_TARGET = x86_64-unknown-none
RUST_LIB = src/rust/target/$(RUST_TARGET)/release/libkernel.a

.PHONY: all check nocheck clean %.o rust

-include $(DEPS)

all: check nos.iso

nocheck: nos.iso

check: $(CXX_SRC)
	cppcheck --error-exitcode=22 -q src/cpp || exit 1

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

rust $(RUST_LIB):
	cd src/rust && cargo build --release

kernel64_pass1.elf: build/linker64.ld $(OBJS) $(RUST_LIB)
	$(LD) $(LDFLAGS) -T $< -o $@ $(OBJS) $(RUST_LIB)

src/cpp/kernel/symtab_data.cpp: kernel64_pass1.elf
	@echo "Generating symbol table..."
	@printf '#include "symtab.h"\nnamespace Kernel {\nconst SymEntry SymbolTable::Symbols[] = {\n' > $@
	@$(NM) -Cn $< | awk '/^[0-9a-fA-F]+ [Tt] / { addr=$$1; name=""; for(i=3;i<=NF;i++){name=name (i>3?" ":"") $$i}; sub(/\(.*/, "", name); gsub(/"/, "\\\"", name); printf "    { 0x%s, \"%s\" },\n", addr, name }' >> $@
	@printf '};\nconst size_t SymbolTable::SymbolCount = sizeof(Symbols)/sizeof(Symbols[0]);\n}\n' >> $@

src/cpp/kernel/symtab_data.o: src/cpp/kernel/symtab_data.cpp src/cpp/kernel/symtab.h
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -c $< -o $@

kernel64.elf: build/linker64.ld $(OBJS) src/cpp/kernel/symtab_data.o $(RUST_LIB)
	$(LD) $(LDFLAGS) -T $< -o $@ $(OBJS) src/cpp/kernel/symtab_data.o $(RUST_LIB)

clean:
	rm -rf $(OBJS) $(DEPS) src/cpp/kernel/symtab_data.cpp src/cpp/kernel/symtab_data.o kernel64_pass1.elf *.elf *.bin *.iso iso
	cd src/rust && cargo clean 2>/dev/null || true
