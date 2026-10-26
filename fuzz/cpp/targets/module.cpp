// The module loader (kernel/module.cpp): a .ko is a file on a disk, and what
// is in a file is whoever wrote it's to choose -- a module this tree built, one
// a disk error left half written, one made to take the loader apart. A .ko is
// built from the input as the Makefile's would be -- an ELF shared object
// for x86-64 or for arm64, its loadable segments, its dynamic symbols and
// relocations, the function table the build adds, the kmod header -- and
// damaged or not, and handed to the loader's own Stage: everything Load does
// up to the module's init, which a fuzzer does not run. A sound one is held
// to a model of what the loader promises: the image the segments and the
// relocations make, byte for byte; each segment's pages with its own
// permissions and nothing both writable and executable; the imports, the
// function table, the header's name; the lookups a backtrace makes through
// it. Any one, sound or not: nothing written past the image's pages (a
// page either side of each run is poisoned for ASan -- a host's own pages
// may be bigger than the kernel's, so guard pages and mprotect cannot be
// placed to the kernel's page), nothing asked for that is both writable and
// executable,
// and -- when the loader says no, when an allocation it makes fails, and
// once the module is released -- every page, mapping and block it took
// given back.
#include "host.h"

/* The loader's own file, so that its internals -- Stage, LoadedModule,
   FindFunction -- are this target's to call; and the table's list, which
   Load puts a module on, reachable to do the same. */
#define private public
#include <kernel/module.cpp>
#undef private

#include <kernel/cpu.h>
#include <kernel/sched.h>
#include <kernel/task.h>
#include <lib/printer.h>

#include "fuzz.h"
#include "kernel.h"

#include <sanitizer/asan_interface.h>

#include <algorithm>
#include <map>
#include <set>
#include <string>
#include <vector>

using namespace kernel;

/* ---- the machine the loader runs on ---- */

namespace fuzz
{

namespace
{

/* The ELF machine of this input's module: the loader is the same for both,
   and so is this program, whichever the host is. */
u16 machine = elf::MACHINE_X86_64;

/* Physical pages, as the page allocator hands them out. */
struct HostPages
{
    std::vector<mm::Page*> all;
    std::set<mm::Page*> out;
    long fail_after = -1;
} pages;

/* A run of pages mapped at one VA: host memory, a poisoned page either
   side. */
struct Mapping
{
    ulong count;
    uint8_t* block;
    std::vector<ulong> phys;
    /* Per page: what SetRangeProtection last gave it */
    std::vector<uint8_t> perm;
};

void unmap(Mapping& m)
{
    ASAN_UNPOISON_MEMORY_REGION(m.block, (m.count + 2) * consts::PAGE_SIZE);
    free(m.block);
}

std::map<ulong, Mapping> mappings;
long map_fail_after = -1;
long protect_fail_after = -1;
/* What the loader asked of the CPU: code synced, TLBs shot down */
std::vector<std::pair<ulong, ulong>> synced;
std::vector<std::pair<ulong, ulong>> shootdowns;

const uint8_t PERM_W = 1;
const uint8_t PERM_X = 2;

bool take(long& fail_after)
{
    if (fail_after == 0)
        return false;
    if (fail_after > 0)
        fail_after--;
    return true;
}

Mapping* owning(ulong va, ulong& base)
{
    auto it = mappings.upper_bound(va);
    if (it == mappings.begin())
        return nullptr;
    --it;
    if (va >= it->first + it->second.count * consts::PAGE_SIZE)
        return nullptr;
    base = it->first;
    return &it->second;
}

}

}

namespace hal
{

u16 module_elf_machine()
{
    return fuzz::machine;
}

/* Each ABI's numbering, as hal_x86.cpp and hal_arm64.cpp have it. */
ModuleReloc classify_module_reloc(u32 type)
{
    const bool x86 = fuzz::machine == kernel::elf::MACHINE_X86_64;
    const u32 relative = x86 ? 8 : 1027;
    const u32 abs64 = x86 ? 1 : 257;
    const u32 glob_dat = x86 ? 6 : 1025;
    const u32 jump_slot = x86 ? 7 : 1026;
    if (type == 0)
        return ModuleReloc::None;
    if (type == relative)
        return ModuleReloc::Relative;
    if (type == abs64 || type == glob_dat || type == jump_slot)
        return ModuleReloc::Symbol;
    return ModuleReloc::Unsupported;
}

void sync_instruction_cache(ulong va, ulong size)
{
    fuzz::synced.push_back({va, size});
}

}

namespace kernel
{
namespace mm
{

PageTable::PageTable()
{
}

PageTable::~PageTable()
{
}

Page* PageTable::alloc_page()
{
    if (!fuzz::take(fuzz::pages.fail_after))
        return nullptr;
    Page* page = new Page();
    page->init(0x100000 + fuzz::pages.all.size() * consts::PAGE_SIZE);
    fuzz::pages.all.push_back(page);
    fuzz::pages.out.insert(page);
    return page;
}

void PageTable::free_page(Page* page)
{
    INVARIANT(fuzz::pages.out.erase(page) == 1, "a page freed that is not out: %p", page);
}

bool PageTable::set_range_protection(ulong virt_addr, ulong size_bytes, bool writable, bool executable)
{
    INVARIANT(!(writable && executable), "pages asked to be writable and executable at once");
    INVARIANT(virt_addr % consts::PAGE_SIZE == 0 && size_bytes % consts::PAGE_SIZE == 0,
        "a protection change of [0x%lx, +0x%lx) not in pages", virt_addr, size_bytes);
    ulong base;
    fuzz::Mapping* m = fuzz::owning(virt_addr, base);
    INVARIANT(m != nullptr && virt_addr + size_bytes <= base + m->count * consts::PAGE_SIZE,
        "a protection change of [0x%lx, +0x%lx) outside what is mapped", virt_addr, size_bytes);
    if (!fuzz::take(fuzz::protect_fail_after))
        return false;
    for (ulong va = virt_addr; va < virt_addr + size_bytes; va += consts::PAGE_SIZE)
        m->perm[(va - base) / consts::PAGE_SIZE] = (writable ? fuzz::PERM_W : 0) | (executable ? fuzz::PERM_X : 0);
    return true;
}

void* map_large_pages(size_t num_pages, ulong* phys_addrs)
{
    INVARIANT(num_pages != 0 && num_pages <= PageTable::MAX_LARGE_MAP_PAGES, "a run of %zu pages mapped", num_pages);
    INVARIANT(fuzz::held_spin_locks() == 0, "pages mapped with a spin lock held");
    if (!fuzz::take(fuzz::map_fail_after))
        return nullptr;
    const size_t page = consts::PAGE_SIZE;
    void* block = nullptr;
    INVARIANT(posix_memalign(&block, page, (num_pages + 2) * page) == 0, "the host has no room for %zu pages",
        num_pages);
    uint8_t* at = static_cast<uint8_t*>(block);
    uint8_t* base = at + page;
    ASAN_POISON_MEMORY_REGION(at, page);
    ASAN_POISON_MEMORY_REGION(base + num_pages * page, page);
    fuzz::Mapping m;
    m.count = num_pages;
    m.block = at;
    m.phys.assign(phys_addrs, phys_addrs + num_pages);
    m.perm.assign(num_pages, fuzz::PERM_W);
    fuzz::mappings[reinterpret_cast<ulong>(base)] = m;
    return base;
}

void unmap_large_pages(void* ptr, size_t num_pages)
{
    auto it = fuzz::mappings.find(reinterpret_cast<ulong>(ptr));
    INVARIANT(it != fuzz::mappings.end() && it->second.count == num_pages,
        "an unmap of %zu pages at %p, which is not a run that was mapped", num_pages, ptr);
    fuzz::unmap(it->second);
    fuzz::mappings.erase(it);
}

}

void CpuTable::invalidate_tlb_range(ulong virt_addr, ulong count)
{
    fuzz::shootdowns.push_back({virt_addr, count});
}

}

/* The export table the Makefile generates from the ffi declarations: a few
   names at made-up addresses, and the one that makes a module permanent. */
extern "C" const ModuleExport nos_module_exports[] = {
    {"kernel_trace", 0x10000},
    {"kernel_alloc", 0x20000},
    {"kernel_free", 0x30000},
    {"kernel_panic", 0x40000},
    {"kernel_softirq_register", 0x50000},
    {"kernel_get_boot_time", 0x60000},
};
extern "C" const ulong nos_module_export_count = sizeof(nos_module_exports) / sizeof(nos_module_exports[0]);

/* The file system's side of LoadFile, which this target does not use. */
extern "C" long kernel_file_size(const char*, ulong)
{
    fuzz::host_hal_unreachable("kernel_file_size");
}

extern "C" long kernel_file_read(const char*, ulong, void*, ulong)
{
    fuzz::host_hal_unreachable("kernel_file_read");
}

namespace
{

/* ---- the .ko ---- */

struct Seg
{
    u32 flags;
    ulong vaddr;
    ulong filesz;
    ulong memsz;
    ulong offset;
    std::vector<uint8_t> bytes;
};

struct Sym
{
    std::string name;
    u8 info;
    u16 shndx;
    u64 value;
    u64 size;
};

struct Rela
{
    u64 offset;
    u32 type;
    u32 sym_index;
    long addend;
};

struct Ko
{
    std::vector<Seg> segs;
    std::vector<Sym> syms;
    std::vector<Rela> relas;
    std::vector<Rela> plt;
    std::string func_text;
    bool has_funcs = false;
    std::string name;
    ulong info_vaddr = 0;
    /* What the model says of it */
    ulong image_size = 0;
    ulong text_end = 0;
    ulong imports = 0;
    bool permanent = false;
    std::vector<std::pair<ulong, std::string>> funcs;
};

const char* const EXPORTED[] = {"kernel_trace", "kernel_alloc", "kernel_free", "kernel_panic",
                                "kernel_softirq_register", "kernel_get_boot_time"};

ulong export_addr(const std::string& name)
{
    for (ulong i = 0; i < nos_module_export_count; i++)
    {
        if (name == nos_module_exports[i].name)
            return nos_module_exports[i].addr;
    }
    return 0;
}

u32 rel_type(char kind)
{
    const bool x86 = fuzz::machine == elf::MACHINE_X86_64;
    switch (kind)
    {
    case 'R':
        return x86 ? 8 : 1027;
    case 'A':
        return x86 ? 1 : 257;
    case 'G':
        return x86 ? 6 : 1025;
    default:
        return x86 ? 7 : 1026;
    }
}

void put_le(std::vector<uint8_t>& v, size_t at, u64 x, int bytes)
{
    if (v.size() < at + bytes)
        v.resize(at + bytes, 0);
    for (int i = 0; i < bytes; i++)
        v[at + i] = static_cast<uint8_t>(x >> (8 * i));
}

u64 get_le(const uint8_t* p, int bytes)
{
    u64 x = 0;
    for (int i = bytes - 1; i >= 0; i--)
        x = (x << 8) | p[i];
    return x;
}

const ulong PAGE = consts::PAGE_SIZE;
const ulong PAGE_TABLE_MAX_IMAGE = mm::PageTable::MAX_LARGE_MAP_PAGES * consts::PAGE_SIZE;

/* A module as the build makes one. */
Ko build(fuzz::Input& in)
{
    Ko ko;
    const char* const names[] = {"modtest", "sshd", "netload", "blkload", "hv", "x", "a_31_characters_long_name_here"};
    ko.name = in.pick(names);

    /* The segments: read-only data, code, data with bss. */
    ulong va = 0;
    const u32 flags[] = {elf::PF_R, elf::PF_R | elf::PF_X, elf::PF_R | elf::PF_W};
    for (u32 f : flags)
    {
        Seg s;
        s.flags = f;
        s.vaddr = va;
        s.memsz = (1 + in.below(3)) * PAGE - in.below(64) * 8;
        s.filesz = (f & elf::PF_W) ? s.memsz - in.below(s.memsz / 2) : s.memsz;
        s.bytes = fuzz::noise(in.u32(), s.filesz);
        ko.segs.push_back(s);
        va = stdlib::round_up(s.vaddr + s.memsz, PAGE) + (in.chance(32) ? PAGE : 0);
    }
    ko.image_size = stdlib::round_up(ko.segs.back().vaddr + ko.segs.back().memsz, PAGE);
    Seg& text = ko.segs[1];
    Seg& data = ko.segs[2];
    ko.text_end = text.vaddr + text.memsz;

    /* Functions in the code, and the table the build lists them in. */
    std::vector<ulong> funcs;
    for (int i = 2 + in.below(6); i > 0; i--)
        funcs.push_back(text.vaddr + in.below(text.memsz / 16) * 16);
    std::sort(funcs.begin(), funcs.end());
    ko.has_funcs = !in.chance(24);
    for (size_t i = 0; i < funcs.size(); i++)
    {
        char line[64];
        std::string name = "fn" + std::to_string(i) + (in.chance(32) ? "::<impl kcore::Thing>" : "");
        snprintf(line, sizeof(line), "%lx %s\n", funcs[i], name.c_str());
        ko.func_text += line;
        ko.funcs.push_back({funcs[i], name});
    }

    /* The header, in the data, and what points at it. */
    const ulong info_size = 120;
    ulong info_off = in.below((data.filesz - info_size) / 8) * 8;
    ko.info_vaddr = data.vaddr + info_off;
    std::vector<uint8_t> info(info_size, 0);
    put_le(info, 0, 0x4D534F4E, 4);
    put_le(info, 4, 2, 4);
    memcpy(info.data() + 8, "fuzz", 4);
    memcpy(info.data() + 72, ko.name.data(), ko.name.size());
    memcpy(data.bytes.data() + info_off, info.data(), info_size);

    ko.syms.push_back({"", 0, 0, 0, 0});
    ko.syms.push_back({"nos_module_info", 0x11, 5, ko.info_vaddr, info_size});
    for (size_t i = 0; i < funcs.size(); i++)
        ko.syms.push_back({"fn" + std::to_string(i), 0x12, 6, funcs[i], 16});
    ulong init = funcs[0], exit = funcs[1];
    ko.relas.push_back({ko.info_vaddr + 104, rel_type('R'), 0, static_cast<long>(init)});
    ko.relas.push_back({ko.info_vaddr + 112, rel_type('R'), 0, static_cast<long>(exit)});

    /* A slot in the data for a pointer, clear of the header, whose fields
       CheckInfo reads */
    auto slot = [&]() {
        for (;;)
        {
            ulong at = data.vaddr + in.below((data.memsz - 8) / 8) * 8;
            if (at + 8 <= ko.info_vaddr || at >= ko.info_vaddr + info_size || !in.more())
                return (at + 8 <= ko.info_vaddr || at >= ko.info_vaddr + info_size) ? at : ko.info_vaddr + info_size;
        }
    };
    const char kinds[] = {'A', 'G', 'J'};

    /* Imports: the kernel's, some weak ones nobody exports, each bound
       through a slot in the data. */
    std::set<std::string> imported;
    for (int i = in.below(6); i > 0; i--)
    {
        bool weak = in.chance(48);
        std::string name = weak && in.boolean() ? "kernel_nothing_" + std::to_string(i) : in.pick(EXPORTED);
        u32 index = static_cast<u32>(ko.syms.size());
        ko.syms.push_back({name, static_cast<u8>(((weak ? 2 : 1) << 4) | 2), 0, 0, 0});
        if (export_addr(name) != 0)
        {
            ko.imports++;
            if (name == "kernel_softirq_register")
                ko.permanent = true;
        }
        imported.insert(name);
        Rela r{slot(), rel_type(in.pick(kinds)), index, static_cast<long>(in.chance(32) ? in.below(64) : 0)};
        (in.boolean() ? ko.plt : ko.relas).push_back(r);
    }
    /* Pointers in the data to the module's own code and data. */
    for (int i = in.below(6); i > 0; i--)
    {
        ulong at = slot();
        if (in.boolean())
            ko.relas.push_back({at, rel_type('R'), 0, static_cast<long>(in.below(ko.image_size))});
        else
            ko.relas.push_back({at, rel_type('A'), static_cast<u32>(2 + in.below(funcs.size())), static_cast<long>(in.below(16))});
    }
    if (in.chance(32))
        ko.relas.push_back({0, 0, 0, 0});
    /* The header's own slots last, whatever else points there: they are
       what CheckInfo reads */
    std::rotate(ko.relas.begin(), ko.relas.begin() + 2, ko.relas.end());
    return ko;
}

/* Function tables .nos_syms should not hold */
const char* const BAD_TABLES[] = {"10 a\n8 b\n", "fffffffff c\n", "zz d\n", "12\n", " e\n",
                                 "11111111111111111 f\n"};

/* Damage that keeps the file a file: a module that says something the
   loader must refuse, or take with care, in a field it reads on the way. */
void twist(fuzz::Input& in, Ko& ko)
{
    Seg& data = ko.segs[2];
    switch (in.u8() % 10)
    {
    case 0:
        /* Two segments on one page, or overlapping */
        ko.segs[1].vaddr = ko.segs[0].vaddr + in.below(2) * PAGE;
        break;
    case 1:
        /* Writable and executable */
        ko.segs[in.below(3)].flags = elf::PF_R | elf::PF_W | elf::PF_X;
        break;
    case 2:
        /* A relocation at the image's end, or past it */
        ko.relas.push_back({ko.image_size - 8 + in.below(3) * 4 - 4, rel_type('R'), 0, 0});
        break;
    case 3:
        /* A relocation naming a symbol there is none of */
        ko.relas.push_back({data.vaddr, rel_type('A'), static_cast<u32>(ko.syms.size() + in.below(2)), 0});
        break;
    case 4:
        /* A defined symbol past the image */
        ko.syms.push_back({"far", 0x12, 6, ko.image_size + in.below(2) - 1, 0});
        ko.relas.push_back({data.vaddr, rel_type('A'), static_cast<u32>(ko.syms.size() - 1), 0});
        break;
    case 5:
        /* The header past the image's end, or unaligned */
        ko.syms[1].value = in.boolean() ? ko.image_size - 120 + 8 : ko.syms[1].value + 4;
        break;
    case 6:
        /* Function table lines out of order, past the image, not hex */
        ko.func_text = in.pick(BAD_TABLES);
        ko.has_funcs = true;
        break;
    case 7:
        /* A global import nobody exports */
        ko.syms.push_back({"kernel_nothing", 0x12, 0, 0, 0});
        break;
    case 8:
        /* A relocation type no module may have */
        ko.relas.push_back({data.vaddr, 42, 0, 0});
        break;
    default:
        /* An image bigger than a module may be */
        data.memsz = PAGE_TABLE_MAX_IMAGE + in.below(2) * PAGE;
        break;
    }
}

/* The file: headers, the segments' bytes, the sections, the section
   headers. */
std::vector<uint8_t> serialize(const Ko& ko)
{
    std::vector<uint8_t> f(64, 0);
    auto align = [&](size_t a) {
        while (f.size() % a)
            f.push_back(0);
    };
    const size_t phnum = ko.segs.size() + 1;
    size_t phoff = f.size();
    f.resize(phoff + phnum * 56, 0);

    std::vector<ulong> seg_off;
    for (const Seg& s : ko.segs)
    {
        align(16);
        seg_off.push_back(f.size());
        f.insert(f.end(), s.bytes.begin(), s.bytes.end());
    }

    std::string dynstr(1, '\0');
    std::vector<u32> name_off;
    for (const Sym& s : ko.syms)
    {
        if (s.name.empty())
        {
            name_off.push_back(0);
            continue;
        }
        name_off.push_back(static_cast<u32>(dynstr.size()));
        dynstr += s.name;
        dynstr += '\0';
    }

    align(8);
    size_t dynsym_off = f.size();
    for (size_t i = 0; i < ko.syms.size(); i++)
    {
        std::vector<uint8_t> e(24, 0);
        put_le(e, 0, name_off[i], 4);
        e[4] = ko.syms[i].info;
        put_le(e, 6, ko.syms[i].shndx, 2);
        put_le(e, 8, ko.syms[i].value, 8);
        put_le(e, 16, ko.syms[i].size, 8);
        f.insert(f.end(), e.begin(), e.end());
    }
    size_t dynstr_off = f.size();
    f.insert(f.end(), dynstr.begin(), dynstr.end());

    auto relas = [&](const std::vector<Rela>& list) {
        align(8);
        size_t at = f.size();
        for (const Rela& r : list)
        {
            std::vector<uint8_t> e(24, 0);
            put_le(e, 0, r.offset, 8);
            put_le(e, 8, (static_cast<u64>(r.sym_index) << 32) | r.type, 8);
            put_le(e, 16, static_cast<u64>(r.addend), 8);
            f.insert(f.end(), e.begin(), e.end());
        }
        return at;
    };
    size_t rela_off = relas(ko.relas);
    size_t plt_off = relas(ko.plt);

    size_t func_off = f.size();
    f.insert(f.end(), ko.func_text.begin(), ko.func_text.end());

    const char shstr[] = "\0.dynsym\0.dynstr\0.rela.dyn\0.rela.plt\0.nos_syms\0.shstrtab\0.text\0";
    size_t shstr_off = f.size();
    f.insert(f.end(), shstr, shstr + sizeof(shstr));

    align(8);
    size_t shoff = f.size();
    struct Sh
    {
        u32 name, type;
        u64 flags, offset, size;
        u32 link;
        u64 entsize;
    };
    std::vector<Sh> shs = {
        {0, 0, 0, 0, 0, 0, 0},
        {1, elf::SHT_DYNSYM, elf::SHF_ALLOC, dynsym_off, ko.syms.size() * 24, 2, 24},
        {9, elf::SHT_STRTAB, elf::SHF_ALLOC, dynstr_off, dynstr.size(), 0, 0},
        {17, elf::SHT_RELA, elf::SHF_ALLOC, rela_off, ko.relas.size() * 24, 1, 24},
        {27, elf::SHT_RELA, elf::SHF_ALLOC, plt_off, ko.plt.size() * 24, 1, 24},
        {37, 1, 0, func_off, ko.has_funcs ? ko.func_text.size() : 0, 0, 0},
        {47, elf::SHT_STRTAB, 0, shstr_off, sizeof(shstr), 0, 0},
        {57, 1, elf::SHF_ALLOC, seg_off[1], ko.segs[1].filesz, 0, 0},
    };
    for (const Sh& s : shs)
    {
        std::vector<uint8_t> e(64, 0);
        put_le(e, 0, s.name, 4);
        put_le(e, 4, s.type, 4);
        put_le(e, 8, s.flags, 8);
        put_le(e, 24, s.offset, 8);
        put_le(e, 32, s.size, 8);
        put_le(e, 40, s.link, 4);
        put_le(e, 56, s.entsize, 8);
        f.insert(f.end(), e.begin(), e.end());
    }

    /* The ELF header and the program headers */
    memcpy(f.data(), elf::MAGIC, 4);
    f[elf::IDENT_CLASS] = elf::CLASS64;
    f[elf::IDENT_DATA] = elf::DATA2_LSB;
    f[elf::IDENT_VERSION] = elf::VERSION_CURRENT;
    put_le(f, 16, elf::TYPE_DYN, 2);
    put_le(f, 18, fuzz::machine, 2);
    put_le(f, 20, 1, 4);
    put_le(f, 32, phoff, 8);
    put_le(f, 40, shoff, 8);
    put_le(f, 52, 64, 2);
    put_le(f, 54, 56, 2);
    put_le(f, 56, phnum, 2);
    put_le(f, 58, 64, 2);
    put_le(f, 60, shs.size(), 2);
    put_le(f, 62, 6, 2);
    for (size_t i = 0; i < ko.segs.size(); i++)
    {
        size_t at = phoff + i * 56;
        const Seg& s = ko.segs[i];
        put_le(f, at, elf::PT_LOAD, 4);
        put_le(f, at + 4, s.flags, 4);
        put_le(f, at + 8, seg_off[i], 8);
        put_le(f, at + 16, s.vaddr, 8);
        put_le(f, at + 24, s.vaddr, 8);
        put_le(f, at + 32, s.filesz, 8);
        put_le(f, at + 40, s.memsz, 8);
        put_le(f, at + 48, PAGE, 8);
    }
    /* A PT_DYNAMIC the loader passes over */
    put_le(f, phoff + ko.segs.size() * 56, elf::PT_DYNAMIC, 4);
    return f;
}

/* Damage of the kinds a file has: a byte of anything, a field of the
   header's or a program header's or a section header's that lies, the file
   cut short. */
void damage(fuzz::Input& in, std::vector<uint8_t>& f)
{
    for (int n = 1 + in.below(3); n > 0; n--)
    {
        switch (in.u8() % 6)
        {
        case 0:
            f[in.below(f.size())] = in.u8();
            break;
        case 1:
        {
            /* A field of the ELF header's */
            static const u8 AT[] = {16, 18, 32, 40, 54, 56, 58, 60, 62};
            u8 where = in.pick(AT);
            put_le(f, where, in.value64(), (where >= 32 && where < 48) ? 8 : 2);
            break;
        }
        case 2:
        {
            /* A field of a program header's, while the file still has a
               header to say where they are */
            if (f.size() < 64)
                break;
            u64 phoff = get_le(f.data() + 32, 8);
            u64 which = phoff + in.below(4) * 56 + in.below(7) * 8;
            if (which < f.size() && f.size() - which >= 8)
                put_le(f, which, in.value64(), in.boolean() ? 8 : 4);
            break;
        }
        case 3:
        {
            /* A field of a section header's, the same */
            if (f.size() < 64)
                break;
            u64 shoff = get_le(f.data() + 40, 8);
            u64 which = shoff + in.below(8) * 64 + in.below(8) * 8;
            if (which < f.size() && f.size() - which >= 8)
                put_le(f, which, in.value64(), in.boolean() ? 8 : 4);
            break;
        }
        case 4:
            f.resize(in.below(f.size()));
            if (f.empty())
                f.push_back(0x7F);
            break;
        default:
        {
            /* Eight bytes of anything in the middle: a symbol, a relocation */
            size_t at = in.below(f.size());
            put_le(f, at, in.value64(), 8);
            break;
        }
        }
    }
}

/* ---- what the loader made of it ---- */

void check_sound(const Ko& ko, const std::vector<uint8_t>& file, LoadedModule& m, const ModuleInfo* info)
{
    const ulong base = m.image.base;
    INVARIANT(base != 0 && m.image_size == ko.image_size, "an image of %lu bytes, where the file's is %lu",
        m.image_size, ko.image_size);
    INVARIANT(m.text_end == ko.text_end, "text ends at 0x%lx, not 0x%lx", m.text_end, ko.text_end);
    INVARIANT(ko.name == m.name, "the module is called '%s', not '%s'", m.name, ko.name.c_str());
    INVARIANT(m.imports == ko.imports, "%lu imports, where the file has %lu", m.imports, ko.imports);
    INVARIANT((m.permanent_by != nullptr) == ko.permanent, "permanent %d, where the file's imports say %d",
        m.permanent_by != nullptr, ko.permanent);
    INVARIANT(reinterpret_cast<ulong>(info) == base + ko.info_vaddr, "the header found at +0x%lx, not +0x%lx",
        reinterpret_cast<ulong>(info) - base, ko.info_vaddr);

    /* The image: the segments' bytes, zeros past them, the relocations over
       both, in the order the sections give them */
    std::vector<uint8_t> want(ko.image_size, 0);
    for (const Seg& s : ko.segs)
        memcpy(want.data() + s.vaddr, s.bytes.data(), s.filesz);
    for (const std::vector<Rela>* list : {&ko.relas, &ko.plt})
    {
        for (const Rela& r : *list)
        {
            ulong value;
            hal::ModuleReloc kind = hal::classify_module_reloc(r.type);
            if (kind == hal::ModuleReloc::None)
                continue;
            if (kind == hal::ModuleReloc::Relative)
                value = base + r.addend;
            else if (ko.syms[r.sym_index].shndx != 0)
                value = base + ko.syms[r.sym_index].value + r.addend;
            else
                value = export_addr(ko.syms[r.sym_index].name) + r.addend;
            put_le(want, r.offset, value, 8);
        }
    }
    const uint8_t* image = reinterpret_cast<const uint8_t*>(base);
    for (ulong i = 0; i < ko.image_size; i++)
        INVARIANT(image[i] == want[i], "the image's byte at +0x%lx is 0x%02x, where the file and its relocations "
            "make 0x%02x", i, image[i], want[i]);

    /* Each segment's pages with its own permissions, the rest read-only */
    ulong mbase;
    fuzz::Mapping* map = fuzz::owning(base, mbase);
    INVARIANT(map != nullptr && mbase == base, "the image is not a run that was mapped");
    std::vector<uint8_t> perm(map->count, 0);
    for (const Seg& s : ko.segs)
    {
        for (ulong va = s.vaddr; va < s.vaddr + s.memsz; va += PAGE)
            perm[va / PAGE] = ((s.flags & elf::PF_W) ? fuzz::PERM_W : 0) | ((s.flags & elf::PF_X) ? fuzz::PERM_X : 0);
    }
    for (ulong p = 0; p < map->count; p++)
        INVARIANT(map->perm[p] == perm[p], "page %lu of the image is %s%s, not %s%s", p,
            (map->perm[p] & fuzz::PERM_W) ? "w" : "", (map->perm[p] & fuzz::PERM_X) ? "x" : "",
            (perm[p] & fuzz::PERM_W) ? "w" : "", (perm[p] & fuzz::PERM_X) ? "x" : "");
    bool synced = false;
    for (auto& s : fuzz::synced)
        synced |= s.first == base + ko.segs[1].vaddr && s.second == ko.segs[1].memsz;
    INVARIANT(synced, "the code was never synced with the instruction cache");
    bool shot = false;
    for (auto& s : fuzz::shootdowns)
        shot |= s.first == base && s.second == map->count;
    INVARIANT(shot, "no TLB shootdown for the image's new permissions");

    /* The function table */
    if (ko.has_funcs)
    {
        INVARIANT(m.symbol_count == ko.funcs.size(), "%lu functions named, where the table has %zu", m.symbol_count,
            ko.funcs.size());
        for (size_t i = 0; i < ko.funcs.size(); i++)
            INVARIANT(m.symbols[i].offset == ko.funcs[i].first && ko.funcs[i].second == m.symbols[i].name,
                "function %zu is %s at 0x%lx", i, m.symbols[i].name, m.symbols[i].offset);
    }
    else
    {
        INVARIANT(m.symbol_count == 0, "%lu functions named, from no table", m.symbol_count);
    }
    (void)file;
}

/* A backtrace's questions of a loaded module: any address, inside or not. */
void lookups(fuzz::Input& in, LoadedModule& m)
{
    auto& table = ModuleTable::get_instance();
    for (int i = 0; i < 8 && in.more(); i++)
    {
        ulong addr = in.chance(64) ? in.u64() : m.image.base + in.below(m.image.count * PAGE + PAGE) - 8;
        char buf[96];
        size_t size = 1 + in.below(sizeof(buf));
        memset(buf, 'x', sizeof(buf));
        /* A return address is looked up a byte back, at its call */
        const bool ret = in.boolean();
        bool named = table.describe(addr, ret, buf, size);
        INVARIANT(!named || memchr(buf, 0, size) != nullptr, "Describe left its buffer unterminated");
        const ulong at = ret ? addr - 1 : addr;
        if (!(ret && addr == 0) && at - m.image.base < m.image_size)
            INVARIANT(named, "Describe does not know 0x%lx%s, in the module", addr, ret ? ", a return address" : "");
        const ModuleSymbol* f = find_function(m, addr - m.image.base);
        if (f != nullptr)
            INVARIANT(f->offset <= addr - m.image.base && addr - m.image.base < m.text_end,
                "FindFunction(+0x%lx) names a function at +0x%lx", addr - m.image.base, f->offset);
    }
    char all[256];
    table.describe_all(all, 1 + in.below(sizeof(all)));
    INVARIANT(table.is_loaded(m.name), "IsLoaded does not know %s", m.name);
}

void run(fuzz::Input& in)
{
    fuzz::machine = in.boolean() ? elf::MACHINE_AARCH64 : elf::MACHINE_X86_64;
    Ko ko = build(in);
    const bool twisted = in.chance(64);
    if (twisted)
    {
        twist(in, ko);
        fuzz::reached("a twisted module");
    }
    std::vector<uint8_t> file = serialize(ko);
    const bool damaged = twisted || in.chance(96);
    if (damaged && !twisted)
    {
        damage(in, file);
        fuzz::reached("a damaged module");
    }

    /* Allocations that fail: the error paths */
    bool failing = in.chance(40);
    if (failing)
    {
        switch (in.u8() % 4)
        {
        case 0:
            fuzz::heap_fail_after(in.below(4));
            break;
        case 1:
            fuzz::pages.fail_after = in.below(8);
            break;
        case 2:
            fuzz::map_fail_after = 0;
            break;
        default:
            fuzz::protect_fail_after = in.below(4);
            break;
        }
    }

    /* The file 8-byte aligned, as LoadFile's page run is, and alone */
    std::vector<u64> aligned((file.size() + 7) / 8 + 1);
    memcpy(aligned.data(), file.data(), file.size());

    const size_t blocks = fuzz::heap_blocks();
    char said[1024];
    stdlib::BufferPrinter out(said, sizeof(said));
    LoadedModule* module = nullptr;
    const ModuleInfo* info = nullptr;
    stdlib::Error err = stage(aligned.data(), file.size(), module, info, out);
    fuzz::check_no_locks_held("after the load");
    fuzz::say("Stage: %s", err.ok() ? "loaded" : said);
    if (err.ok())
    {
        INVARIANT(module != nullptr && info != nullptr, "a load that says yes with no module");
        fuzz::reached(damaged ? "a damaged module loaded" : "a module loaded");
        if (!damaged)
            check_sound(ko, file, *module, info);

        /* On the list, as Load puts it there, for the lookups */
        auto& table = ModuleTable::get_instance();
        ulong flags = table.lock_.lock_irq_save();
        table.list_.insert_tail(&module->list_entry);
        table.lock_.unlock_irq_restore(flags);
        lookups(in, *module);
        flags = table.lock_.lock_irq_save();
        module->list_entry.remove_init();
        table.lock_.unlock_irq_restore(flags);

        release_module(module);
    }
    else
    {
        INVARIANT(module == nullptr, "a load that says no, with a module left");
        INVARIANT(damaged || failing, "a sound module refused: %s", said);
        fuzz::reached(failing && !damaged ? "an allocation failed" : "a module refused");
    }
    INVARIANT(fuzz::pages.out.empty(), "%zu pages still out", fuzz::pages.out.size());
    INVARIANT(fuzz::mappings.empty(), "%zu runs still mapped", fuzz::mappings.size());
    INVARIANT(fuzz::heap_blocks() == blocks, "%zu heap blocks left", fuzz::heap_blocks() - blocks);
}

void reset()
{
    fuzz::pages.out.clear();
    fuzz::pages.fail_after = -1;
    for (auto& m : fuzz::mappings)
        fuzz::unmap(m.second);
    fuzz::mappings.clear();
    fuzz::map_fail_after = -1;
    fuzz::protect_fail_after = -1;
    fuzz::synced.clear();
    fuzz::shootdowns.clear();
    auto& table = ModuleTable::get_instance();
    table.list_.init();
    table.lock_.~RawSpinLock();
    new (&table.lock_) RawSpinLock(false);
}

}

const fuzz::Target fuzz::THE_TARGET = {"module", reset, run, 4096, 20000};
