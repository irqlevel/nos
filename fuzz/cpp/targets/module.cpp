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

using namespace Kernel;

/* ---- the machine the loader runs on ---- */

namespace Fuzz
{

namespace
{

/* The ELF machine of this input's module: the loader is the same for both,
   and so is this program, whichever the host is. */
u16 Machine = Elf::MachineX86_64;

/* Physical pages, as the page allocator hands them out. */
struct HostPages
{
    std::vector<Mm::Page*> All;
    std::set<Mm::Page*> Out;
    long FailAfter = -1;
} Pages;

/* A run of pages mapped at one VA: host memory, a poisoned page either
   side. */
struct Mapping
{
    ulong Count;
    uint8_t* Block;
    std::vector<ulong> Phys;
    /* Per page: what SetRangeProtection last gave it */
    std::vector<uint8_t> Perm;
};

void Unmap(Mapping& m)
{
    ASAN_UNPOISON_MEMORY_REGION(m.Block, (m.Count + 2) * Const::PageSize);
    free(m.Block);
}

std::map<ulong, Mapping> Mappings;
long MapFailAfter = -1;
long ProtectFailAfter = -1;
/* What the loader asked of the CPU: code synced, TLBs shot down */
std::vector<std::pair<ulong, ulong>> Synced;
std::vector<std::pair<ulong, ulong>> Shootdowns;

const uint8_t PermW = 1;
const uint8_t PermX = 2;

bool Take(long& failAfter)
{
    if (failAfter == 0)
        return false;
    if (failAfter > 0)
        failAfter--;
    return true;
}

Mapping* Owning(ulong va, ulong& base)
{
    auto it = Mappings.upper_bound(va);
    if (it == Mappings.begin())
        return nullptr;
    --it;
    if (va >= it->first + it->second.Count * Const::PageSize)
        return nullptr;
    base = it->first;
    return &it->second;
}

}

}

namespace Hal
{

u16 ModuleElfMachine()
{
    return Fuzz::Machine;
}

/* Each ABI's numbering, as hal_x86.cpp and hal_arm64.cpp have it. */
ModuleReloc ClassifyModuleReloc(u32 type)
{
    const bool x86 = Fuzz::Machine == Kernel::Elf::MachineX86_64;
    const u32 relative = x86 ? 8 : 1027;
    const u32 abs64 = x86 ? 1 : 257;
    const u32 globDat = x86 ? 6 : 1025;
    const u32 jumpSlot = x86 ? 7 : 1026;
    if (type == 0)
        return ModuleReloc::None;
    if (type == relative)
        return ModuleReloc::Relative;
    if (type == abs64 || type == globDat || type == jumpSlot)
        return ModuleReloc::Symbol;
    return ModuleReloc::Unsupported;
}

void SyncInstructionCache(ulong va, ulong size)
{
    Fuzz::Synced.push_back({va, size});
}

}

namespace Kernel
{
namespace Mm
{

PageTable::PageTable()
{
}

PageTable::~PageTable()
{
}

Page* PageTable::AllocPage()
{
    if (!Fuzz::Take(Fuzz::Pages.FailAfter))
        return nullptr;
    Page* page = new Page();
    page->Init(0x100000 + Fuzz::Pages.All.size() * Const::PageSize);
    Fuzz::Pages.All.push_back(page);
    Fuzz::Pages.Out.insert(page);
    return page;
}

void PageTable::FreePage(Page* page)
{
    INVARIANT(Fuzz::Pages.Out.erase(page) == 1, "a page freed that is not out: %p", page);
}

bool PageTable::SetRangeProtection(ulong virtAddr, ulong sizeBytes, bool writable, bool executable)
{
    INVARIANT(!(writable && executable), "pages asked to be writable and executable at once");
    INVARIANT(virtAddr % Const::PageSize == 0 && sizeBytes % Const::PageSize == 0,
        "a protection change of [0x%lx, +0x%lx) not in pages", virtAddr, sizeBytes);
    ulong base;
    Fuzz::Mapping* m = Fuzz::Owning(virtAddr, base);
    INVARIANT(m != nullptr && virtAddr + sizeBytes <= base + m->Count * Const::PageSize,
        "a protection change of [0x%lx, +0x%lx) outside what is mapped", virtAddr, sizeBytes);
    if (!Fuzz::Take(Fuzz::ProtectFailAfter))
        return false;
    for (ulong va = virtAddr; va < virtAddr + sizeBytes; va += Const::PageSize)
        m->Perm[(va - base) / Const::PageSize] = (writable ? Fuzz::PermW : 0) | (executable ? Fuzz::PermX : 0);
    return true;
}

void* MapLargePages(size_t numPages, ulong* physAddrs)
{
    INVARIANT(numPages != 0 && numPages <= PageTable::MaxLargeMapPages, "a run of %zu pages mapped", numPages);
    INVARIANT(Fuzz::HeldSpinLocks() == 0, "pages mapped with a spin lock held");
    if (!Fuzz::Take(Fuzz::MapFailAfter))
        return nullptr;
    const size_t page = Const::PageSize;
    void* block = nullptr;
    INVARIANT(posix_memalign(&block, page, (numPages + 2) * page) == 0, "the host has no room for %zu pages",
        numPages);
    uint8_t* at = static_cast<uint8_t*>(block);
    uint8_t* base = at + page;
    ASAN_POISON_MEMORY_REGION(at, page);
    ASAN_POISON_MEMORY_REGION(base + numPages * page, page);
    Fuzz::Mapping m;
    m.Count = numPages;
    m.Block = at;
    m.Phys.assign(physAddrs, physAddrs + numPages);
    m.Perm.assign(numPages, Fuzz::PermW);
    Fuzz::Mappings[reinterpret_cast<ulong>(base)] = m;
    return base;
}

void UnmapLargePages(void* ptr, size_t numPages)
{
    auto it = Fuzz::Mappings.find(reinterpret_cast<ulong>(ptr));
    INVARIANT(it != Fuzz::Mappings.end() && it->second.Count == numPages,
        "an unmap of %zu pages at %p, which is not a run that was mapped", numPages, ptr);
    Fuzz::Unmap(it->second);
    Fuzz::Mappings.erase(it);
}

}

void CpuTable::InvalidateTlbRange(ulong virtAddr, ulong count)
{
    Fuzz::Shootdowns.push_back({virtAddr, count});
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
    Fuzz::HostHalUnreachable("kernel_file_size");
}

extern "C" long kernel_file_read(const char*, ulong, void*, ulong)
{
    Fuzz::HostHalUnreachable("kernel_file_read");
}

namespace
{

/* ---- the .ko ---- */

struct Seg
{
    u32 Flags;
    ulong Vaddr;
    ulong Filesz;
    ulong Memsz;
    ulong Offset;
    std::vector<uint8_t> Bytes;
};

struct Sym
{
    std::string Name;
    u8 Info;
    u16 Shndx;
    u64 Value;
    u64 Size;
};

struct Rela
{
    u64 Offset;
    u32 Type;
    u32 SymIndex;
    long Addend;
};

struct Ko
{
    std::vector<Seg> Segs;
    std::vector<Sym> Syms;
    std::vector<Rela> Relas;
    std::vector<Rela> Plt;
    std::string FuncText;
    bool HasFuncs = false;
    std::string Name;
    ulong InfoVaddr = 0;
    /* What the model says of it */
    ulong ImageSize = 0;
    ulong TextEnd = 0;
    ulong Imports = 0;
    bool Permanent = false;
    std::vector<std::pair<ulong, std::string>> Funcs;
};

const char* const Exported[] = {"kernel_trace", "kernel_alloc", "kernel_free", "kernel_panic",
                                "kernel_softirq_register", "kernel_get_boot_time"};

ulong ExportAddr(const std::string& name)
{
    for (ulong i = 0; i < nos_module_export_count; i++)
    {
        if (name == nos_module_exports[i].Name)
            return nos_module_exports[i].Addr;
    }
    return 0;
}

u32 RelType(char kind)
{
    const bool x86 = Fuzz::Machine == Elf::MachineX86_64;
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

void PutLe(std::vector<uint8_t>& v, size_t at, u64 x, int bytes)
{
    if (v.size() < at + bytes)
        v.resize(at + bytes, 0);
    for (int i = 0; i < bytes; i++)
        v[at + i] = static_cast<uint8_t>(x >> (8 * i));
}

u64 GetLe(const uint8_t* p, int bytes)
{
    u64 x = 0;
    for (int i = bytes - 1; i >= 0; i--)
        x = (x << 8) | p[i];
    return x;
}

const ulong Page = Const::PageSize;
const ulong PageTable_MaxImage = Mm::PageTable::MaxLargeMapPages * Const::PageSize;

/* A module as the build makes one. */
Ko Build(Fuzz::Input& in)
{
    Ko ko;
    const char* const Names[] = {"modtest", "sshd", "netload", "blkload", "hv", "x", "a_31_characters_long_name_here"};
    ko.Name = in.Pick(Names);

    /* The segments: read-only data, code, data with bss. */
    ulong va = 0;
    const u32 flags[] = {Elf::PfR, Elf::PfR | Elf::PfX, Elf::PfR | Elf::PfW};
    for (u32 f : flags)
    {
        Seg s;
        s.Flags = f;
        s.Vaddr = va;
        s.Memsz = (1 + in.Below(3)) * Page - in.Below(64) * 8;
        s.Filesz = (f & Elf::PfW) ? s.Memsz - in.Below(s.Memsz / 2) : s.Memsz;
        s.Bytes = Fuzz::Noise(in.U32(), s.Filesz);
        ko.Segs.push_back(s);
        va = Stdlib::RoundUp(s.Vaddr + s.Memsz, Page) + (in.Chance(32) ? Page : 0);
    }
    ko.ImageSize = Stdlib::RoundUp(ko.Segs.back().Vaddr + ko.Segs.back().Memsz, Page);
    Seg& text = ko.Segs[1];
    Seg& data = ko.Segs[2];
    ko.TextEnd = text.Vaddr + text.Memsz;

    /* Functions in the code, and the table the build lists them in. */
    std::vector<ulong> funcs;
    for (int i = 2 + in.Below(6); i > 0; i--)
        funcs.push_back(text.Vaddr + in.Below(text.Memsz / 16) * 16);
    std::sort(funcs.begin(), funcs.end());
    ko.HasFuncs = !in.Chance(24);
    for (size_t i = 0; i < funcs.size(); i++)
    {
        char line[64];
        std::string name = "fn" + std::to_string(i) + (in.Chance(32) ? "::<impl kcore::Thing>" : "");
        snprintf(line, sizeof(line), "%lx %s\n", funcs[i], name.c_str());
        ko.FuncText += line;
        ko.Funcs.push_back({funcs[i], name});
    }

    /* The header, in the data, and what points at it. */
    const ulong infoSize = 120;
    ulong infoOff = in.Below((data.Filesz - infoSize) / 8) * 8;
    ko.InfoVaddr = data.Vaddr + infoOff;
    std::vector<uint8_t> info(infoSize, 0);
    PutLe(info, 0, 0x4D534F4E, 4);
    PutLe(info, 4, 1, 4);
    memcpy(info.data() + 8, "fuzz", 4);
    memcpy(info.data() + 72, ko.Name.data(), ko.Name.size());
    memcpy(data.Bytes.data() + infoOff, info.data(), infoSize);

    ko.Syms.push_back({"", 0, 0, 0, 0});
    ko.Syms.push_back({"nos_module_info", 0x11, 5, ko.InfoVaddr, infoSize});
    for (size_t i = 0; i < funcs.size(); i++)
        ko.Syms.push_back({"fn" + std::to_string(i), 0x12, 6, funcs[i], 16});
    ulong init = funcs[0], exit = funcs[1];
    ko.Relas.push_back({ko.InfoVaddr + 104, RelType('R'), 0, static_cast<long>(init)});
    ko.Relas.push_back({ko.InfoVaddr + 112, RelType('R'), 0, static_cast<long>(exit)});

    /* A slot in the data for a pointer, clear of the header, whose fields
       CheckInfo reads */
    auto slot = [&]() {
        for (;;)
        {
            ulong at = data.Vaddr + in.Below((data.Memsz - 8) / 8) * 8;
            if (at + 8 <= ko.InfoVaddr || at >= ko.InfoVaddr + infoSize || !in.More())
                return (at + 8 <= ko.InfoVaddr || at >= ko.InfoVaddr + infoSize) ? at : ko.InfoVaddr + infoSize;
        }
    };
    const char kinds[] = {'A', 'G', 'J'};

    /* Imports: the kernel's, some weak ones nobody exports, each bound
       through a slot in the data. */
    std::set<std::string> imported;
    for (int i = in.Below(6); i > 0; i--)
    {
        bool weak = in.Chance(48);
        std::string name = weak && in.Bool() ? "kernel_nothing_" + std::to_string(i) : in.Pick(Exported);
        u32 index = static_cast<u32>(ko.Syms.size());
        ko.Syms.push_back({name, static_cast<u8>(((weak ? 2 : 1) << 4) | 2), 0, 0, 0});
        if (ExportAddr(name) != 0)
        {
            ko.Imports++;
            if (name == "kernel_softirq_register")
                ko.Permanent = true;
        }
        imported.insert(name);
        Rela r{slot(), RelType(in.Pick(kinds)), index, static_cast<long>(in.Chance(32) ? in.Below(64) : 0)};
        (in.Bool() ? ko.Plt : ko.Relas).push_back(r);
    }
    /* Pointers in the data to the module's own code and data. */
    for (int i = in.Below(6); i > 0; i--)
    {
        ulong at = slot();
        if (in.Bool())
            ko.Relas.push_back({at, RelType('R'), 0, static_cast<long>(in.Below(ko.ImageSize))});
        else
            ko.Relas.push_back({at, RelType('A'), static_cast<u32>(2 + in.Below(funcs.size())), static_cast<long>(in.Below(16))});
    }
    if (in.Chance(32))
        ko.Relas.push_back({0, 0, 0, 0});
    /* The header's own slots last, whatever else points there: they are
       what CheckInfo reads */
    std::rotate(ko.Relas.begin(), ko.Relas.begin() + 2, ko.Relas.end());
    return ko;
}

/* Function tables .nos_syms should not hold */
const char* const BadTables[] = {"10 a\n8 b\n", "fffffffff c\n", "zz d\n", "12\n", " e\n",
                                 "11111111111111111 f\n"};

/* Damage that keeps the file a file: a module that says something the
   loader must refuse, or take with care, in a field it reads on the way. */
void Twist(Fuzz::Input& in, Ko& ko)
{
    Seg& data = ko.Segs[2];
    switch (in.U8() % 10)
    {
    case 0:
        /* Two segments on one page, or overlapping */
        ko.Segs[1].Vaddr = ko.Segs[0].Vaddr + in.Below(2) * Page;
        break;
    case 1:
        /* Writable and executable */
        ko.Segs[in.Below(3)].Flags = Elf::PfR | Elf::PfW | Elf::PfX;
        break;
    case 2:
        /* A relocation at the image's end, or past it */
        ko.Relas.push_back({ko.ImageSize - 8 + in.Below(3) * 4 - 4, RelType('R'), 0, 0});
        break;
    case 3:
        /* A relocation naming a symbol there is none of */
        ko.Relas.push_back({data.Vaddr, RelType('A'), static_cast<u32>(ko.Syms.size() + in.Below(2)), 0});
        break;
    case 4:
        /* A defined symbol past the image */
        ko.Syms.push_back({"far", 0x12, 6, ko.ImageSize + in.Below(2) - 1, 0});
        ko.Relas.push_back({data.Vaddr, RelType('A'), static_cast<u32>(ko.Syms.size() - 1), 0});
        break;
    case 5:
        /* The header past the image's end, or unaligned */
        ko.Syms[1].Value = in.Bool() ? ko.ImageSize - 120 + 8 : ko.Syms[1].Value + 4;
        break;
    case 6:
        /* Function table lines out of order, past the image, not hex */
        ko.FuncText = in.Pick(BadTables);
        ko.HasFuncs = true;
        break;
    case 7:
        /* A global import nobody exports */
        ko.Syms.push_back({"kernel_nothing", 0x12, 0, 0, 0});
        break;
    case 8:
        /* A relocation type no module may have */
        ko.Relas.push_back({data.Vaddr, 42, 0, 0});
        break;
    default:
        /* An image bigger than a module may be */
        data.Memsz = PageTable_MaxImage + in.Below(2) * Page;
        break;
    }
}

/* The file: headers, the segments' bytes, the sections, the section
   headers. */
std::vector<uint8_t> Serialize(const Ko& ko)
{
    std::vector<uint8_t> f(64, 0);
    auto align = [&](size_t a) {
        while (f.size() % a)
            f.push_back(0);
    };
    const size_t phnum = ko.Segs.size() + 1;
    size_t phoff = f.size();
    f.resize(phoff + phnum * 56, 0);

    std::vector<ulong> segOff;
    for (const Seg& s : ko.Segs)
    {
        align(16);
        segOff.push_back(f.size());
        f.insert(f.end(), s.Bytes.begin(), s.Bytes.end());
    }

    std::string dynstr(1, '\0');
    std::vector<u32> nameOff;
    for (const Sym& s : ko.Syms)
    {
        if (s.Name.empty())
        {
            nameOff.push_back(0);
            continue;
        }
        nameOff.push_back(static_cast<u32>(dynstr.size()));
        dynstr += s.Name;
        dynstr += '\0';
    }

    align(8);
    size_t dynsymOff = f.size();
    for (size_t i = 0; i < ko.Syms.size(); i++)
    {
        std::vector<uint8_t> e(24, 0);
        PutLe(e, 0, nameOff[i], 4);
        e[4] = ko.Syms[i].Info;
        PutLe(e, 6, ko.Syms[i].Shndx, 2);
        PutLe(e, 8, ko.Syms[i].Value, 8);
        PutLe(e, 16, ko.Syms[i].Size, 8);
        f.insert(f.end(), e.begin(), e.end());
    }
    size_t dynstrOff = f.size();
    f.insert(f.end(), dynstr.begin(), dynstr.end());

    auto relas = [&](const std::vector<Rela>& list) {
        align(8);
        size_t at = f.size();
        for (const Rela& r : list)
        {
            std::vector<uint8_t> e(24, 0);
            PutLe(e, 0, r.Offset, 8);
            PutLe(e, 8, (static_cast<u64>(r.SymIndex) << 32) | r.Type, 8);
            PutLe(e, 16, static_cast<u64>(r.Addend), 8);
            f.insert(f.end(), e.begin(), e.end());
        }
        return at;
    };
    size_t relaOff = relas(ko.Relas);
    size_t pltOff = relas(ko.Plt);

    size_t funcOff = f.size();
    f.insert(f.end(), ko.FuncText.begin(), ko.FuncText.end());

    const char shstr[] = "\0.dynsym\0.dynstr\0.rela.dyn\0.rela.plt\0.nos_syms\0.shstrtab\0.text\0";
    size_t shstrOff = f.size();
    f.insert(f.end(), shstr, shstr + sizeof(shstr));

    align(8);
    size_t shoff = f.size();
    struct Sh
    {
        u32 Name, Type;
        u64 Flags, Offset, Size;
        u32 Link;
        u64 Entsize;
    };
    std::vector<Sh> shs = {
        {0, 0, 0, 0, 0, 0, 0},
        {1, Elf::ShtDynsym, Elf::ShfAlloc, dynsymOff, ko.Syms.size() * 24, 2, 24},
        {9, Elf::ShtStrtab, Elf::ShfAlloc, dynstrOff, dynstr.size(), 0, 0},
        {17, Elf::ShtRela, Elf::ShfAlloc, relaOff, ko.Relas.size() * 24, 1, 24},
        {27, Elf::ShtRela, Elf::ShfAlloc, pltOff, ko.Plt.size() * 24, 1, 24},
        {37, 1, 0, funcOff, ko.HasFuncs ? ko.FuncText.size() : 0, 0, 0},
        {47, Elf::ShtStrtab, 0, shstrOff, sizeof(shstr), 0, 0},
        {57, 1, Elf::ShfAlloc, segOff[1], ko.Segs[1].Filesz, 0, 0},
    };
    for (const Sh& s : shs)
    {
        std::vector<uint8_t> e(64, 0);
        PutLe(e, 0, s.Name, 4);
        PutLe(e, 4, s.Type, 4);
        PutLe(e, 8, s.Flags, 8);
        PutLe(e, 24, s.Offset, 8);
        PutLe(e, 32, s.Size, 8);
        PutLe(e, 40, s.Link, 4);
        PutLe(e, 56, s.Entsize, 8);
        f.insert(f.end(), e.begin(), e.end());
    }

    /* The ELF header and the program headers */
    memcpy(f.data(), Elf::Magic, 4);
    f[Elf::IdentClass] = Elf::Class64;
    f[Elf::IdentData] = Elf::Data2Lsb;
    f[Elf::IdentVersion] = Elf::VersionCurrent;
    PutLe(f, 16, Elf::TypeDyn, 2);
    PutLe(f, 18, Fuzz::Machine, 2);
    PutLe(f, 20, 1, 4);
    PutLe(f, 32, phoff, 8);
    PutLe(f, 40, shoff, 8);
    PutLe(f, 52, 64, 2);
    PutLe(f, 54, 56, 2);
    PutLe(f, 56, phnum, 2);
    PutLe(f, 58, 64, 2);
    PutLe(f, 60, shs.size(), 2);
    PutLe(f, 62, 6, 2);
    for (size_t i = 0; i < ko.Segs.size(); i++)
    {
        size_t at = phoff + i * 56;
        const Seg& s = ko.Segs[i];
        PutLe(f, at, Elf::PtLoad, 4);
        PutLe(f, at + 4, s.Flags, 4);
        PutLe(f, at + 8, segOff[i], 8);
        PutLe(f, at + 16, s.Vaddr, 8);
        PutLe(f, at + 24, s.Vaddr, 8);
        PutLe(f, at + 32, s.Filesz, 8);
        PutLe(f, at + 40, s.Memsz, 8);
        PutLe(f, at + 48, Page, 8);
    }
    /* A PT_DYNAMIC the loader passes over */
    PutLe(f, phoff + ko.Segs.size() * 56, Elf::PtDynamic, 4);
    return f;
}

/* Damage of the kinds a file has: a byte of anything, a field of the
   header's or a program header's or a section header's that lies, the file
   cut short. */
void Damage(Fuzz::Input& in, std::vector<uint8_t>& f)
{
    for (int n = 1 + in.Below(3); n > 0; n--)
    {
        switch (in.U8() % 6)
        {
        case 0:
            f[in.Below(f.size())] = in.U8();
            break;
        case 1:
        {
            /* A field of the ELF header's */
            static const u8 at[] = {16, 18, 32, 40, 54, 56, 58, 60, 62};
            u8 where = in.Pick(at);
            PutLe(f, where, in.Value64(), (where >= 32 && where < 48) ? 8 : 2);
            break;
        }
        case 2:
        {
            /* A field of a program header's, while the file still has a
               header to say where they are */
            if (f.size() < 64)
                break;
            u64 phoff = GetLe(f.data() + 32, 8);
            u64 which = phoff + in.Below(4) * 56 + in.Below(7) * 8;
            if (which < f.size() && f.size() - which >= 8)
                PutLe(f, which, in.Value64(), in.Bool() ? 8 : 4);
            break;
        }
        case 3:
        {
            /* A field of a section header's, the same */
            if (f.size() < 64)
                break;
            u64 shoff = GetLe(f.data() + 40, 8);
            u64 which = shoff + in.Below(8) * 64 + in.Below(8) * 8;
            if (which < f.size() && f.size() - which >= 8)
                PutLe(f, which, in.Value64(), in.Bool() ? 8 : 4);
            break;
        }
        case 4:
            f.resize(in.Below(f.size()));
            if (f.empty())
                f.push_back(0x7F);
            break;
        default:
        {
            /* Eight bytes of anything in the middle: a symbol, a relocation */
            size_t at = in.Below(f.size());
            PutLe(f, at, in.Value64(), 8);
            break;
        }
        }
    }
}

/* ---- what the loader made of it ---- */

void CheckSound(const Ko& ko, const std::vector<uint8_t>& file, LoadedModule& m, const ModuleInfo* info)
{
    const ulong base = m.Image.Base;
    INVARIANT(base != 0 && m.ImageSize == ko.ImageSize, "an image of %lu bytes, where the file's is %lu",
        m.ImageSize, ko.ImageSize);
    INVARIANT(m.TextEnd == ko.TextEnd, "text ends at 0x%lx, not 0x%lx", m.TextEnd, ko.TextEnd);
    INVARIANT(ko.Name == m.Name, "the module is called '%s', not '%s'", m.Name, ko.Name.c_str());
    INVARIANT(m.Imports == ko.Imports, "%lu imports, where the file has %lu", m.Imports, ko.Imports);
    INVARIANT((m.PermanentBy != nullptr) == ko.Permanent, "permanent %d, where the file's imports say %d",
        m.PermanentBy != nullptr, ko.Permanent);
    INVARIANT(reinterpret_cast<ulong>(info) == base + ko.InfoVaddr, "the header found at +0x%lx, not +0x%lx",
        reinterpret_cast<ulong>(info) - base, ko.InfoVaddr);

    /* The image: the segments' bytes, zeros past them, the relocations over
       both, in the order the sections give them */
    std::vector<uint8_t> want(ko.ImageSize, 0);
    for (const Seg& s : ko.Segs)
        memcpy(want.data() + s.Vaddr, s.Bytes.data(), s.Filesz);
    for (const std::vector<Rela>* list : {&ko.Relas, &ko.Plt})
    {
        for (const Rela& r : *list)
        {
            ulong value;
            Hal::ModuleReloc kind = Hal::ClassifyModuleReloc(r.Type);
            if (kind == Hal::ModuleReloc::None)
                continue;
            if (kind == Hal::ModuleReloc::Relative)
                value = base + r.Addend;
            else if (ko.Syms[r.SymIndex].Shndx != 0)
                value = base + ko.Syms[r.SymIndex].Value + r.Addend;
            else
                value = ExportAddr(ko.Syms[r.SymIndex].Name) + r.Addend;
            PutLe(want, r.Offset, value, 8);
        }
    }
    const uint8_t* image = reinterpret_cast<const uint8_t*>(base);
    for (ulong i = 0; i < ko.ImageSize; i++)
        INVARIANT(image[i] == want[i], "the image's byte at +0x%lx is 0x%02x, where the file and its relocations "
            "make 0x%02x", i, image[i], want[i]);

    /* Each segment's pages with its own permissions, the rest read-only */
    ulong mbase;
    Fuzz::Mapping* map = Fuzz::Owning(base, mbase);
    INVARIANT(map != nullptr && mbase == base, "the image is not a run that was mapped");
    std::vector<uint8_t> perm(map->Count, 0);
    for (const Seg& s : ko.Segs)
    {
        for (ulong va = s.Vaddr; va < s.Vaddr + s.Memsz; va += Page)
            perm[va / Page] = ((s.Flags & Elf::PfW) ? Fuzz::PermW : 0) | ((s.Flags & Elf::PfX) ? Fuzz::PermX : 0);
    }
    for (ulong p = 0; p < map->Count; p++)
        INVARIANT(map->Perm[p] == perm[p], "page %lu of the image is %s%s, not %s%s", p,
            (map->Perm[p] & Fuzz::PermW) ? "w" : "", (map->Perm[p] & Fuzz::PermX) ? "x" : "",
            (perm[p] & Fuzz::PermW) ? "w" : "", (perm[p] & Fuzz::PermX) ? "x" : "");
    bool synced = false;
    for (auto& s : Fuzz::Synced)
        synced |= s.first == base + ko.Segs[1].Vaddr && s.second == ko.Segs[1].Memsz;
    INVARIANT(synced, "the code was never synced with the instruction cache");
    bool shot = false;
    for (auto& s : Fuzz::Shootdowns)
        shot |= s.first == base && s.second == map->Count;
    INVARIANT(shot, "no TLB shootdown for the image's new permissions");

    /* The function table */
    if (ko.HasFuncs)
    {
        INVARIANT(m.SymbolCount == ko.Funcs.size(), "%lu functions named, where the table has %zu", m.SymbolCount,
            ko.Funcs.size());
        for (size_t i = 0; i < ko.Funcs.size(); i++)
            INVARIANT(m.Symbols[i].Offset == ko.Funcs[i].first && ko.Funcs[i].second == m.Symbols[i].Name,
                "function %zu is %s at 0x%lx", i, m.Symbols[i].Name, m.Symbols[i].Offset);
    }
    else
    {
        INVARIANT(m.SymbolCount == 0, "%lu functions named, from no table", m.SymbolCount);
    }
    (void)file;
}

/* A backtrace's questions of a loaded module: any address, inside or not. */
void Lookups(Fuzz::Input& in, LoadedModule& m)
{
    auto& table = ModuleTable::GetInstance();
    for (int i = 0; i < 8 && in.More(); i++)
    {
        ulong addr = in.Chance(64) ? in.U64() : m.Image.Base + in.Below(m.Image.Count * Page + Page) - 8;
        char buf[96];
        size_t size = 1 + in.Below(sizeof(buf));
        memset(buf, 'x', sizeof(buf));
        /* A return address is looked up a byte back, at its call */
        const bool ret = in.Bool();
        bool named = table.Describe(addr, ret, buf, size);
        INVARIANT(!named || memchr(buf, 0, size) != nullptr, "Describe left its buffer unterminated");
        const ulong at = ret ? addr - 1 : addr;
        if (!(ret && addr == 0) && at - m.Image.Base < m.ImageSize)
            INVARIANT(named, "Describe does not know 0x%lx%s, in the module", addr, ret ? ", a return address" : "");
        const ModuleSymbol* f = FindFunction(m, addr - m.Image.Base);
        if (f != nullptr)
            INVARIANT(f->Offset <= addr - m.Image.Base && addr - m.Image.Base < m.TextEnd,
                "FindFunction(+0x%lx) names a function at +0x%lx", addr - m.Image.Base, f->Offset);
    }
    char all[256];
    table.DescribeAll(all, 1 + in.Below(sizeof(all)));
    INVARIANT(table.IsLoaded(m.Name), "IsLoaded does not know %s", m.Name);
}

void Run(Fuzz::Input& in)
{
    Fuzz::Machine = in.Bool() ? Elf::MachineAarch64 : Elf::MachineX86_64;
    Ko ko = Build(in);
    const bool twisted = in.Chance(64);
    if (twisted)
    {
        Twist(in, ko);
        Fuzz::Reached("a twisted module");
    }
    std::vector<uint8_t> file = Serialize(ko);
    const bool damaged = twisted || in.Chance(96);
    if (damaged && !twisted)
    {
        Damage(in, file);
        Fuzz::Reached("a damaged module");
    }

    /* Allocations that fail: the error paths */
    bool failing = in.Chance(40);
    if (failing)
    {
        switch (in.U8() % 4)
        {
        case 0:
            Fuzz::HeapFailAfter(in.Below(4));
            break;
        case 1:
            Fuzz::Pages.FailAfter = in.Below(8);
            break;
        case 2:
            Fuzz::MapFailAfter = 0;
            break;
        default:
            Fuzz::ProtectFailAfter = in.Below(4);
            break;
        }
    }

    /* The file 8-byte aligned, as LoadFile's page run is, and alone */
    std::vector<u64> aligned((file.size() + 7) / 8 + 1);
    memcpy(aligned.data(), file.data(), file.size());

    const size_t blocks = Fuzz::HeapBlocks();
    char said[1024];
    Stdlib::BufferPrinter out(said, sizeof(said));
    LoadedModule* module = nullptr;
    const ModuleInfo* info = nullptr;
    Stdlib::Error err = Stage(aligned.data(), file.size(), module, info, out);
    Fuzz::CheckNoLocksHeld("after the load");
    Fuzz::Say("Stage: %s", err.Ok() ? "loaded" : said);
    if (err.Ok())
    {
        INVARIANT(module != nullptr && info != nullptr, "a load that says yes with no module");
        Fuzz::Reached(damaged ? "a damaged module loaded" : "a module loaded");
        if (!damaged)
            CheckSound(ko, file, *module, info);

        /* On the list, as Load puts it there, for the lookups */
        auto& table = ModuleTable::GetInstance();
        ulong flags = table.Lock.LockIrqSave();
        table.List.InsertTail(&module->ListEntry);
        table.Lock.UnlockIrqRestore(flags);
        Lookups(in, *module);
        flags = table.Lock.LockIrqSave();
        module->ListEntry.RemoveInit();
        table.Lock.UnlockIrqRestore(flags);

        ReleaseModule(module);
    }
    else
    {
        INVARIANT(module == nullptr, "a load that says no, with a module left");
        INVARIANT(damaged || failing, "a sound module refused: %s", said);
        Fuzz::Reached(failing && !damaged ? "an allocation failed" : "a module refused");
    }
    INVARIANT(Fuzz::Pages.Out.empty(), "%zu pages still out", Fuzz::Pages.Out.size());
    INVARIANT(Fuzz::Mappings.empty(), "%zu runs still mapped", Fuzz::Mappings.size());
    INVARIANT(Fuzz::HeapBlocks() == blocks, "%zu heap blocks left", Fuzz::HeapBlocks() - blocks);
}

void Reset()
{
    Fuzz::Pages.Out.clear();
    Fuzz::Pages.FailAfter = -1;
    for (auto& m : Fuzz::Mappings)
        Fuzz::Unmap(m.second);
    Fuzz::Mappings.clear();
    Fuzz::MapFailAfter = -1;
    Fuzz::ProtectFailAfter = -1;
    Fuzz::Synced.clear();
    Fuzz::Shootdowns.clear();
    auto& table = ModuleTable::GetInstance();
    table.List.Init();
    table.Lock.~RawSpinLock();
    new (&table.Lock) RawSpinLock(false);
}

}

const Fuzz::Target Fuzz::TheTarget = {"module", Reset, Run, 4096, 20000};
