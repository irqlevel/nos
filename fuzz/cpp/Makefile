# The C++ fuzzers: each target one host binary, of the kernel sources it
# tests -- compiled as they are, under the address and undefined-behaviour
# sanitizers -- the stand-ins for the kernel around them (common/), the
# host's HAL in place of each arch's inline one (host/, first on the include
# path), and the target's own file (targets/). scripts/cpp-fuzz.py builds
# them with this and runs them; `make -C fuzz/cpp` alone builds them all.
#
# The source lists are explicit, as the kernel's are: a kernel file a target
# tests is named here, and nothing else of the kernel is compiled into it.

ROOT := $(abspath ../..)
OUT ?= $(ROOT)/out/cpp-fuzz/$(shell uname -s | tr A-Z a-z)-$(shell uname -m)
# Make's default is g++; the kernel is clang's, and so are its sanitizers.
ifeq ($(origin CXX),default)
CXX := clang++
endif

SAN := -fsanitize=address,undefined -fno-sanitize=function,vptr -fno-sanitize-recover=all
# src/base/lib only for quoted includes, as the kernel's headers reach it
# ("stdlib.h"): as an ordinary path it would stand in for the host's own
# <stdlib.h>.
CPPFLAGS := -I$(ROOT)/fuzz/cpp/host -I$(ROOT)/src/base -iquote $(ROOT)/src/base/lib -I$(ROOT)/fuzz/cpp/common \
            -DNOS_MODULE_ABI=\"fuzz\"
CXXFLAGS := -std=c++20 -g -O1 -fno-omit-frame-pointer -fno-exceptions -fno-rtti $(SAN) \
            -Wall -Wextra -Wno-unused-parameter -Wno-implicit-exception-spec-mismatch
LDFLAGS := $(SAN)

# lib/stdlib.cpp defines memcpy and its kin for the calls the compiler makes
# by itself in a freestanding kernel; on the host those are libc's.
$(OUT)/obj/src/base/lib/stdlib.o: CPPFLAGS += -Dmemcpy=nos_memcpy -Dmemset=nos_memset \
                                             -Dmemcmp=nos_memcmp -Dmemmove=nos_memmove

COMMON := fuzz/cpp/common/runner.cpp fuzz/cpp/common/kernel.cpp \
          src/base/lib/stdlib.cpp src/base/lib/format.cpp src/base/lib/list_entry.cpp \
          src/base/kernel/atomic.cpp
HEAP := fuzz/cpp/common/heap.cpp
# The stand-in CPU table and scheduler; a task and a queue hold what EEVDF
# keeps of them, whose constructors are kernel/eevdf.cpp's
CPU := fuzz/cpp/common/cpu.cpp src/base/kernel/eevdf.cpp

TARGETS := fdt memmap grubenv module pagetable heap cmdline multiboot acpi format ucode eevdf lineedit

SRC_fdt := $(COMMON) fuzz/cpp/targets/fdt.cpp \
           src/base/arch/arm64/fdt.cpp src/base/arch/arm64/board.cpp
SRC_memmap := $(COMMON) fuzz/cpp/targets/memmap.cpp \
              src/base/mm/memory_map.cpp
SRC_grubenv := $(COMMON) fuzz/cpp/targets/grubenv.cpp \
               src/base/lib/grub_env.cpp
# kernel/module.cpp is the target's own file's: it includes it
SRC_module := $(COMMON) $(HEAP) $(CPU) fuzz/cpp/targets/module.cpp \
              src/base/lib/error.cpp
SRC_pagetable := $(COMMON) $(HEAP) $(CPU) fuzz/cpp/targets/pagetable.cpp \
                 src/base/mm/page_table.cpp src/base/mm/memory_map.cpp src/base/lib/error.cpp
SRC_cmdline := $(COMMON) fuzz/cpp/targets/cmdline.cpp src/base/kernel/parameters.cpp
# arch/x86_64/grub.cpp is the target's own file's: it includes it
SRC_multiboot := $(COMMON) fuzz/cpp/targets/multiboot.cpp src/base/mm/memory_map.cpp \
                 src/base/kernel/parameters.cpp
SRC_acpi := $(COMMON) $(CPU) fuzz/cpp/targets/acpi.cpp src/base/drivers/acpi.cpp \
            src/base/lib/error.cpp
# lib/format.cpp is COMMON's
SRC_format := $(COMMON) fuzz/cpp/targets/format.cpp
SRC_ucode := $(COMMON) fuzz/cpp/targets/ucode.cpp src/base/arch/x86_64/microcode_file.cpp
SRC_eevdf := $(COMMON) fuzz/cpp/targets/eevdf.cpp src/base/kernel/eevdf.cpp
SRC_lineedit := $(COMMON) fuzz/cpp/targets/lineedit.cpp src/base/kernel/line_editor.cpp
# The kernel heap itself: no heap.cpp under it
SRC_heap := $(COMMON) $(CPU) fuzz/cpp/targets/heap.cpp \
            src/base/mm/allocator.cpp src/base/mm/pool.cpp src/base/mm/page_allocator.cpp \
            src/base/mm/va_allocator.cpp src/base/lib/bitmap.cpp src/base/lib/error.cpp

all: $(TARGETS:%=$(OUT)/bin/%)

# What scripts/cpp-fuzz.py runs, in this order
print-targets:
	@echo $(TARGETS)

$(OUT)/obj/%.o: $(ROOT)/%.cpp
	@mkdir -p $(dir $@)
	$(CXX) $(CPPFLAGS) $(CXXFLAGS) -MMD -MP -c $< -o $@

define TARGET_RULES
$(OUT)/bin/$(1): $$(SRC_$(1):%.cpp=$(OUT)/obj/%.o)
	@mkdir -p $$(dir $$@)
	$$(CXX) $$(LDFLAGS) $$^ -o $$@
endef
$(foreach t,$(TARGETS),$(eval $(call TARGET_RULES,$(t))))

-include $(shell find $(OUT)/obj -name '*.d' 2>/dev/null)

.PHONY: all print-targets
