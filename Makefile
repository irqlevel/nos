ARCH ?= x86_64
VERSION ?= dev
OUT = out/$(ARCH)

# Per-arch toolchain and flags. x86_64 flags are verbatim the historical ones
# (order preserved: -mcmodel=kernel then -mcmodel=large, large wins).
TARGET_x86_64 = x86_64-none-elf
ARCH_CXXFLAGS_x86_64 = -mno-sse -mcmodel=kernel -mcmodel=large -mno-red-zone
LD_x86_64 = ld
NM_x86_64 = nm
RUST_TARGET_x86_64 = x86_64-unknown-none
LDSCRIPT_x86_64 = build/linker-x86_64.ld
KERNEL_x86_64 = kernel64.elf

TARGET_aarch64 = aarch64-none-elf
ARCH_CXXFLAGS_aarch64 = -mgeneral-regs-only -mno-outline-atomics
LD_aarch64 = ld.lld
NM_aarch64 = llvm-nm
RUST_TARGET_aarch64 = aarch64-unknown-none-softfloat
LDSCRIPT_aarch64 = build/linker-arm64.ld
KERNEL_aarch64 = kernel-arm64.elf
OBJCOPY = llvm-objcopy

TARGET = $(TARGET_$(ARCH))
LD = $(LD_$(ARCH))
NM = $(NM_$(ARCH))
RUST_TARGET = $(RUST_TARGET_$(ARCH))
LDSCRIPT = $(LDSCRIPT_$(ARCH))
KERNEL = $(KERNEL_$(ARCH))

CPPFLAGS = -I$(CURDIR)/src/cpp -I$(CURDIR)/src/cpp/lib
CXXFLAGS = --target=$(TARGET) -std=c++20 -g3 -ggdb3 -fno-exceptions -fno-rtti -ffreestanding -nostdlib -fno-builtin -fno-omit-frame-pointer -Wall -Wextra -Werror $(ARCH_CXXFLAGS_$(ARCH)) -DKERNEL_VERSION=\"$(VERSION)\"
LDFLAGS = -nostdlib -z max-page-size=4096
CC = clang
CXX = clang -x c++
ASM = nasm
AR = ar
MKRESCUE ?= $(shell which grub2-mkrescue grub-mkrescue 2> /dev/null | head -n1)

CXX_SRC_x86_64 =   \
    src/cpp/arch/x86_64/grub.cpp    \
    src/cpp/arch/x86_64/cpu_start.cpp \
    src/cpp/arch/x86_64/hal_x86.cpp \
    src/cpp/arch/x86_64/builtin_pt.cpp \
    src/cpp/drivers/serial.cpp  \
    src/cpp/arch/x86_64/pic.cpp \
    src/cpp/drivers/pit.cpp \
    src/cpp/drivers/hpet.cpp \
    src/cpp/drivers/8042.cpp    \
    src/cpp/drivers/acpi.cpp    \
    src/cpp/arch/x86_64/lapic.cpp   \
    src/cpp/arch/x86_64/ioapic.cpp  \
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
    src/cpp/kernel/interrupt_stats.cpp \
    src/cpp/kernel/task.cpp \
    src/cpp/kernel/test.cpp \
    src/cpp/kernel/main.cpp \
    src/cpp/kernel/trace.cpp \
    src/cpp/kernel/timer.cpp    \
    src/cpp/kernel/panic.cpp    \
    src/cpp/kernel/debug.cpp    \
    src/cpp/kernel/atomic.cpp   \
    src/cpp/arch/x86_64/gdt.cpp  \
    src/cpp/arch/x86_64/gdt_descriptor.cpp   \
    src/cpp/arch/x86_64/idt_descriptor.cpp   \
    src/cpp/arch/x86_64/idt.cpp  \
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
    src/cpp/kernel/irq_balance.cpp \
    src/cpp/arch/x86_64/exception.cpp    \
    src/cpp/kernel/dmesg.cpp    \
    src/cpp/kernel/sched.cpp    \
    src/cpp/kernel/preempt.cpp  \
    src/cpp/kernel/time.cpp \
    src/cpp/arch/x86_64/tsc.cpp \
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

CXX_SRC_aarch64 = \
    src/cpp/arch/arm64/main_arm64.cpp \
    src/cpp/arch/arm64/fdt.cpp \
    src/cpp/arch/arm64/board.cpp \
    src/cpp/arch/arm64/pl011.cpp \
    src/cpp/arch/arm64/stdlib_c.cpp \
    src/cpp/arch/arm64/cpu_arm64.cpp \
    src/cpp/arch/arm64/cpu_start_arm64.cpp \
    src/cpp/arch/arm64/hal_arm64.cpp \
    src/cpp/arch/arm64/time_arm64.cpp \
    src/cpp/arch/arm64/builtin_pt.cpp \
    src/cpp/arch/arm64/gicv3.cpp \
    src/cpp/arch/arm64/exception_arm64.cpp \
    src/cpp/arch/arm64/interrupt_arm64.cpp \
    src/cpp/arch/arm64/generic_timer.cpp \
    src/cpp/arch/arm64/pci_stub.cpp \
    src/cpp/kernel/icxxabi.cpp \
    src/cpp/kernel/trace.cpp \
    src/cpp/kernel/dmesg.cpp \
    src/cpp/kernel/panic.cpp \
    src/cpp/kernel/debug.cpp \
    src/cpp/kernel/stack_trace.cpp \
    src/cpp/kernel/symtab.cpp \
    src/cpp/kernel/atomic.cpp \
    src/cpp/kernel/spin_lock.cpp \
    src/cpp/kernel/raw_spin_lock.cpp \
    src/cpp/kernel/raw_rw_spin_lock.cpp \
    src/cpp/kernel/preempt.cpp \
    src/cpp/kernel/parameters.cpp \
    src/cpp/kernel/watchdog.cpp \
    src/cpp/kernel/task.cpp \
    src/cpp/kernel/sched.cpp \
    src/cpp/kernel/cpu.cpp \
    src/cpp/kernel/timer.cpp \
    src/cpp/kernel/mutex.cpp \
    src/cpp/kernel/wait_group.cpp \
    src/cpp/kernel/object_table.cpp \
    src/cpp/kernel/softirq.cpp \
    src/cpp/kernel/interrupt_stats.cpp \
    src/cpp/block/block_device.cpp \
    src/cpp/block/partition.cpp \
    src/cpp/kernel/test.cpp \
    src/cpp/kernel/cmd.cpp \
    src/cpp/kernel/entropy.cpp \
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
    src/cpp/lib/stdlib.cpp \
    src/cpp/lib/format.cpp \
    src/cpp/lib/error.cpp \
    src/cpp/lib/bitmap.cpp \
    src/cpp/lib/checksum.cpp \
    src/cpp/lib/list_entry.cpp \
    src/cpp/mm/memory_map.cpp \
    src/cpp/mm/new.cpp \
    src/cpp/mm/allocator.cpp \
    src/cpp/mm/page_allocator.cpp \
    src/cpp/mm/va_allocator.cpp \
    src/cpp/mm/pool.cpp \
    src/cpp/mm/page_table.cpp \
    src/cpp/mm/block_allocator.cpp \

CXX_SRC = $(CXX_SRC_$(ARCH))

# NASM sources (x86 only)
ASM_SRC_x86_64 =    \
    src/cpp/arch/x86_64/boot64.asm \
    src/cpp/arch/x86_64/asm.asm \
    src/cpp/arch/x86_64/stdlib_asm.asm
ASM_SRC_aarch64 =
ASM_SRC = $(ASM_SRC_$(ARCH))

# GNU-as sources assembled by clang (arm64 only)
ASM_S_SRC_x86_64 =
ASM_S_SRC_aarch64 = \
    src/cpp/arch/arm64/boot.S \
    src/cpp/arch/arm64/asm.S \
    src/cpp/arch/arm64/vectors.S
ASM_S_SRC = $(ASM_S_SRC_$(ARCH))

OBJS = $(patsubst src/cpp/%.cpp,$(OUT)/%.o,$(CXX_SRC)) $(patsubst src/cpp/%.asm,$(OUT)/%.o,$(ASM_SRC)) $(patsubst src/cpp/%.S,$(OUT)/%.o,$(ASM_S_SRC))
DEPS = $(patsubst src/cpp/%.cpp,$(OUT)/%.d,$(CXX_SRC))

RUST_LIB_x86_64 = src/rust/target/$(RUST_TARGET)/release/libkernel.a
RUST_LIB_aarch64 =
RUST_LIB = $(RUST_LIB_$(ARCH))

.PHONY: all check nocheck clean rust smoke

-include $(DEPS)

ifeq ($(ARCH),x86_64)
all: check nos.iso
nocheck: nos.iso
else
all: check nos-arm64.img
nocheck: nos-arm64.img
endif

check: $(CXX_SRC)
	cppcheck --error-exitcode=22 --inline-suppr -q src/cpp || exit 1

nos.iso: build/grub.cfg $(KERNEL)
	rm -rf iso
	rm -rf bin
	mkdir -p iso/boot/grub
	mkdir -p bin
	cp $(KERNEL) iso/boot/kernel64.elf
	cp $(KERNEL) bin/kernel64.elf
	cp build/grub.cfg iso/boot/grub/grub.cfg
	$(MKRESCUE) -o nos.iso iso
	rm -rf iso

$(OUT)/%.o: src/cpp/%.asm
	@mkdir -p $(dir $@)
	$(ASM) -felf64 $< -o $@
$(OUT)/%.o: src/cpp/%.S
	@mkdir -p $(dir $@)
	$(CC) --target=$(TARGET) $(CPPFLAGS) -c $< -o $@
$(OUT)/%.o: src/cpp/%.cpp
	@mkdir -p $(dir $@)
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -MMD -c $< -o $@

rust:
	cd src/rust && cargo build --release --target $(RUST_TARGET)

src/rust/target/$(RUST_TARGET)/release/libkernel.a:
	cd src/rust && cargo build --release --target $(RUST_TARGET)

nos-arm64.img: $(KERNEL)
	$(OBJCOPY) -O binary $< $@

smoke:
	./scripts/smoke-test.sh

$(OUT)/pass1.elf: $(LDSCRIPT) $(OBJS) $(RUST_LIB)
	$(LD) $(LDFLAGS) -T $< -o $@ $(OBJS) $(RUST_LIB)

$(OUT)/symtab_data.cpp: $(OUT)/pass1.elf
	@echo "Generating symbol table..."
	@printf '#include "kernel/symtab.h"\nnamespace Kernel {\nconst SymEntry SymbolTable::Symbols[] = {\n' > $@
	@$(NM) -Cn $< | awk '/^[0-9a-fA-F]+ [Tt] / { addr=$$1; name=""; for(i=3;i<=NF;i++){name=name (i>3?" ":"") $$i}; sub(/\(.*/, "", name); gsub(/"/, "\\\"", name); printf "    { 0x%s, \"%s\" },\n", addr, name }' >> $@
	@printf '};\nconst size_t SymbolTable::SymbolCount = sizeof(Symbols)/sizeof(Symbols[0]);\n}\n' >> $@

$(OUT)/symtab_data.o: $(OUT)/symtab_data.cpp src/cpp/kernel/symtab.h
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -c $< -o $@

$(KERNEL): $(LDSCRIPT) $(OBJS) $(OUT)/symtab_data.o $(RUST_LIB)
	$(LD) $(LDFLAGS) -T $< -o $@ $(OBJS) $(OUT)/symtab_data.o $(RUST_LIB)

clean:
	rm -rf out kernel64.elf kernel-arm64.elf nos-arm64.img *.bin *.iso iso
	cd src/rust && cargo clean 2>/dev/null || true
