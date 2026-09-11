#include "module.h"
#include "elf.h"
#include "trace.h"
#include "cpu.h"

#include <hal/mmu.h>
#include <hal/module.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <fs/vfs.h>
#include <include/const.h>

/* The digest of the ffi crate's sources. The Makefile passes it to this file
   alone, and to every module through the kmod crate: a module built against
   other declarations of the kernel's functions would call them with the wrong
   arguments, so it is refused instead. */
#ifndef NOS_MODULE_ABI
#define NOS_MODULE_ABI "unset"
#endif

/* The kernel functions a module may bind to: every function the ffi crate
   declares that this kernel defines, by name. The Makefile generates the table
   from pass1.elf (module_exports.S) and links it into the final kernel only;
   these empty weak ones stand in for it in pass 1, like the symbol table's in
   symtab.cpp. */
struct ModuleExport
{
    const char* Name;
    ulong Addr;
};

extern "C" const ModuleExport nos_module_exports[];
extern "C" const ulong nos_module_export_count;
__attribute__((weak)) const ModuleExport nos_module_exports[] = {};
__attribute__((weak)) const ulong nos_module_export_count = 0;

namespace Kernel
{

struct LoadedModule
{
    LoadedModule()
        : Base(0)
        , PageCount(0)
        , Pages(nullptr)
        , Imports(0)
        , PermanentBy(nullptr)
        , State(nullptr)
        , Exit(nullptr)
    {
        ListEntry.Init();
        Name[0] = '\0';
    }

    Stdlib::ListEntry ListEntry;
    char Name[ModuleTable::NameMax + 1];
    ulong Base;        /* where the image is mapped */
    ulong PageCount;   /* and how many pages it takes */
    Mm::Page** Pages;  /* the frames behind them */
    ulong Imports;     /* kernel functions it binds to */
    const char* PermanentBy; /* what keeps it from being unloaded, if anything */
    void* State;       /* what its init returned, which its exit takes back */
    void (*Exit)(void* state);
};

namespace
{

const ulong Tag = 'Kmod';

/* kmod::ModuleInfo (src/rust/kmod/src/lib.rs): the header the module! macro
   puts in every module and exports as nos_module_info */
const u32 InfoMagic = 0x4D534F4E; /* "NOSM" */
const u32 InfoVersion = 1;
const ulong InfoAbiLen = 64;
const ulong InfoNameLen = 32;
const char InfoSymbol[] = "nos_module_info";

struct ModuleInfo
{
    u32 Magic;
    u32 Version;
    char Abi[InfoAbiLen];
    char Name[InfoNameLen];
    void* (*Init)();
    void (*Exit)(void* state);
};

static_assert(sizeof(ModuleInfo) == 120, "kmod::ModuleInfo layout");
static_assert(InfoNameLen == ModuleTable::NameMax + 1, "kmod::NAME_LEN");

const char KernelAbi[] = NOS_MODULE_ABI;
static_assert(sizeof(KernelAbi) - 1 <= InfoAbiLen, "NOS_MODULE_ABI is too long");

/* How much of each digest a refusal for a mismatch quotes */
const ulong AbiShown = 12;

/* A module's image, and the .ko file it comes from, are at most the largest
   block the page allocator maps in one piece */
const ulong MaxImageSize = Mm::PageTable::MaxContiguousPages * Const::PageSize;

/* A linked .ko has half a dozen program headers; the segment checks compare
   every pair, so a corrupt count must not make that billions */
const ulong MaxProgramHeaders = 64;

/* Kernel functions that take a callback for good: nothing hands a block
   device, a net device or a softirq handler back once it is registered. A
   module that imports one is permanent -- rmmod would free code the kernel
   may still call into -- the way a Linux module without an exit is. */
const char* const PermanentImports[] = {
    "kernel_blockdev_register",
    "kernel_netdev_register",
    "kernel_softirq_register",
};

bool InFile(ulong fileSize, ulong offset, ulong len)
{
    return offset <= fileSize && len <= fileSize - offset;
}

bool Aligned8(ulong value)
{
    return (value & (sizeof(u64) - 1)) == 0;
}

/* Up to len characters of src, NUL-terminated, anything unprintable a '?' */
void Quote(char* dst, const char* src, ulong len)
{
    ulong i = 0;
    for (; i < len && src[i] != '\0'; i++)
        dst[i] = (src[i] > ' ' && src[i] <= '~') ? src[i] : '?';
    dst[i] = '\0';
}

/* What Load works out about one image, step by step */
struct LoadCtx
{
    LoadCtx(const void* image, ulong size)
        : File(static_cast<const u8*>(image))
        , Size(size)
        , Header(nullptr)
        , Phdrs(nullptr)
        , Shdrs(nullptr)
        , Syms(nullptr)
        , SymCount(0)
        , Strs(nullptr)
        , StrSize(0)
        , Base(0)
        , ImageSize(0)
        , Imports(0)
        , PermanentBy(nullptr)
    {
    }

    const u8* File;
    ulong Size;
    const Elf::Ehdr* Header;
    const Elf::Phdr* Phdrs;
    const Elf::Shdr* Shdrs;
    const Elf::Sym* Syms;   /* .dynsym */
    ulong SymCount;
    const char* Strs;       /* .dynstr */
    ulong StrSize;
    ulong Base;             /* where the image is mapped */
    ulong ImageSize;        /* bytes, a whole number of pages */
    ulong Imports;
    const char* PermanentBy; /* the first of PermanentImports it imports */
};

Stdlib::Error CheckHeader(LoadCtx& ctx, Stdlib::Printer& out)
{
    const Elf::Ehdr* eh = reinterpret_cast<const Elf::Ehdr*>(ctx.File);

    if (!Aligned8(reinterpret_cast<ulong>(ctx.File)))
    {
        out.Printf("module: image at 0x%p is not 8-byte aligned\n", ctx.File);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    if (ctx.Size < sizeof(*eh) ||
        Stdlib::MemCmp(eh->Ident, Elf::Magic, sizeof(Elf::Magic)) != 0)
    {
        out.Printf("module: not an ELF file\n");
        return MakeError(Stdlib::Error::BadMagic);
    }

    if (eh->Ident[Elf::IdentClass] != Elf::Class64 ||
        eh->Ident[Elf::IdentData] != Elf::Data2Lsb ||
        eh->Ident[Elf::IdentVersion] != Elf::VersionCurrent)
    {
        out.Printf("module: not a little-endian ELF64 file\n");
        return MakeError(Stdlib::Error::BadMagic);
    }

    if (eh->Machine != Hal::ModuleElfMachine())
    {
        out.Printf("module: built for ELF machine %u, this kernel runs on %u\n",
            (ulong)eh->Machine, (ulong)Hal::ModuleElfMachine());
        return MakeError(Stdlib::Error::InvalidValue);
    }

    if (eh->Type != Elf::TypeDyn)
    {
        out.Printf("module: ELF type %u, not a shared object\n", (ulong)eh->Type);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    if (eh->Phentsize != sizeof(Elf::Phdr) || eh->Phnum == 0 ||
        eh->Phnum > MaxProgramHeaders || !Aligned8(eh->Phoff) ||
        !InFile(ctx.Size, eh->Phoff, (ulong)eh->Phnum * sizeof(Elf::Phdr)))
    {
        out.Printf("module: program headers corrupt\n");
        return MakeError(Stdlib::Error::HeaderCorrupt);
    }

    if (eh->Shentsize != sizeof(Elf::Shdr) || eh->Shnum == 0 || !Aligned8(eh->Shoff) ||
        !InFile(ctx.Size, eh->Shoff, (ulong)eh->Shnum * sizeof(Elf::Shdr)))
    {
        out.Printf("module: section headers corrupt\n");
        return MakeError(Stdlib::Error::HeaderCorrupt);
    }

    ctx.Header = eh;
    ctx.Phdrs = reinterpret_cast<const Elf::Phdr*>(ctx.File + eh->Phoff);
    ctx.Shdrs = reinterpret_cast<const Elf::Shdr*>(ctx.File + eh->Shoff);
    return MakeSuccess();
}

/* The loadable segments: within the file, page-aligned -- page permissions
   are per page, so no page may hold two segments with different ones -- never
   both writable and executable, and clear of each other. Their extent is the
   image's size. */
Stdlib::Error CheckSegments(LoadCtx& ctx, Stdlib::Printer& out)
{
    ulong end = 0;

    for (ulong i = 0; i < ctx.Header->Phnum; i++)
    {
        const Elf::Phdr& ph = ctx.Phdrs[i];

        if (ph.Type == Elf::PtInterp || ph.Type == Elf::PtTls)
        {
            out.Printf("module: has %s segment, which nothing here provides\n",
                (ph.Type == Elf::PtTls) ? "a thread-local" : "an interpreter");
            return MakeError(Stdlib::Error::NotImplemented);
        }

        if (ph.Type != Elf::PtLoad)
            continue;

        if ((ph.Flags & Elf::PfW) != 0 && (ph.Flags & Elf::PfX) != 0)
        {
            out.Printf("module: segment %u is both writable and executable\n", i);
            return MakeError(Stdlib::Error::InvalidValue);
        }

        if ((ph.Vaddr & (Const::PageSize - 1)) != 0 || ph.Filesz > ph.Memsz ||
            !InFile(ctx.Size, ph.Offset, ph.Filesz) ||
            ph.Memsz > MaxImageSize || ph.Vaddr > MaxImageSize - ph.Memsz)
        {
            out.Printf("module: segment %u is corrupt, too big or not page-aligned\n", i);
            return MakeError(Stdlib::Error::HeaderCorrupt);
        }

        const ulong segEnd = Stdlib::RoundUp(ph.Vaddr + ph.Memsz, Const::PageSize);
        for (ulong j = 0; j < i; j++)
        {
            const Elf::Phdr& other = ctx.Phdrs[j];
            if (other.Type != Elf::PtLoad)
                continue;

            const ulong otherEnd = Stdlib::RoundUp(other.Vaddr + other.Memsz, Const::PageSize);
            if (ph.Vaddr < otherEnd && other.Vaddr < segEnd)
            {
                out.Printf("module: segments %u and %u share a page\n", j, i);
                return MakeError(Stdlib::Error::Overlap);
            }
        }

        if (segEnd > end)
            end = segEnd;
    }

    if (end == 0)
    {
        out.Printf("module: nothing to load\n");
        return MakeError(Stdlib::Error::InvalidValue);
    }

    if (end > MaxImageSize)
    {
        out.Printf("module: %u KiB image, more than the %u KiB a module may take\n",
            end / Const::KB, MaxImageSize / Const::KB);
        return MakeError(Stdlib::Error::BadSize);
    }

    ctx.ImageSize = end;
    return MakeSuccess();
}

Stdlib::Error FindSymbols(LoadCtx& ctx, Stdlib::Printer& out)
{
    const Elf::Shdr* dynsym = nullptr;
    for (ulong i = 0; i < ctx.Header->Shnum && dynsym == nullptr; i++)
    {
        if (ctx.Shdrs[i].Type == Elf::ShtDynsym)
            dynsym = &ctx.Shdrs[i];
    }

    if (dynsym == nullptr)
    {
        out.Printf("module: no dynamic symbol table\n");
        return MakeError(Stdlib::Error::NotFound);
    }

    if (dynsym->Entsize != sizeof(Elf::Sym) || !Aligned8(dynsym->Offset) ||
        !InFile(ctx.Size, dynsym->Offset, dynsym->Size) ||
        dynsym->Link >= ctx.Header->Shnum)
    {
        out.Printf("module: dynamic symbol table corrupt\n");
        return MakeError(Stdlib::Error::DataCorrupt);
    }

    const Elf::Shdr& strtab = ctx.Shdrs[dynsym->Link];
    if (strtab.Type != Elf::ShtStrtab || strtab.Size == 0 ||
        !InFile(ctx.Size, strtab.Offset, strtab.Size) ||
        ctx.File[strtab.Offset + strtab.Size - 1] != '\0')
    {
        out.Printf("module: dynamic string table corrupt\n");
        return MakeError(Stdlib::Error::DataCorrupt);
    }

    ctx.Syms = reinterpret_cast<const Elf::Sym*>(ctx.File + dynsym->Offset);
    ctx.SymCount = dynsym->Size / sizeof(Elf::Sym);
    ctx.Strs = reinterpret_cast<const char*>(ctx.File + strtab.Offset);
    ctx.StrSize = strtab.Size;
    return MakeSuccess();
}

/* NUL-terminated within the table: FindSymbols checked its last byte */
const char* SymName(const LoadCtx& ctx, const Elf::Sym& sym)
{
    return (sym.Name < ctx.StrSize) ? ctx.Strs + sym.Name : nullptr;
}

bool SymDefined(const Elf::Sym& sym)
{
    return sym.Shndx != Elf::ShnUndef && sym.Shndx < Elf::ShnLoReserve;
}

ulong LookupKernel(const char* name)
{
    for (ulong i = 0; i < nos_module_export_count; i++)
    {
        if (Stdlib::StrCmp(nos_module_exports[i].Name, name) == 0)
            return nos_module_exports[i].Addr;
    }

    return 0;
}

/* Every function the module imports, looked up before anything is mapped: a
   module this kernel cannot satisfy is refused with the list of everything it
   lacks. A weak import may go unresolved; it binds to 0. Importing one of
   PermanentImports makes the module permanent. */
Stdlib::Error CheckImports(LoadCtx& ctx, Stdlib::Printer& out)
{
    ulong missing = 0;

    for (ulong i = 1; i < ctx.SymCount; i++)
    {
        const Elf::Sym& sym = ctx.Syms[i];
        if (SymDefined(sym))
            continue;

        const char* name = SymName(ctx, sym);
        if (name == nullptr || name[0] == '\0')
        {
            out.Printf("module: undefined dynamic symbol %u has no name\n", i);
            return MakeError(Stdlib::Error::DataCorrupt);
        }

        const ulong addr = LookupKernel(name);
        if (addr != 0)
        {
            Trace(ModuleLL, "module: %s -> 0x%p", name, addr);
            ctx.Imports++;

            for (ulong j = 0; j < Stdlib::ArraySize(PermanentImports); j++)
            {
                if (ctx.PermanentBy == nullptr && Stdlib::StrCmp(name, PermanentImports[j]) == 0)
                    ctx.PermanentBy = PermanentImports[j];
            }
        }
        else if (Elf::SymBind(sym.Info) != Elf::StbWeak)
        {
            out.Printf("module: needs %s, which this kernel does not export\n", name);
            missing++;
        }
    }

    if (missing != 0)
        return MakeError(Stdlib::Error::NotFound);

    return MakeSuccess();
}

Stdlib::Error ApplyReloc(const LoadCtx& ctx, const Elf::Rela& rela, Stdlib::Printer& out)
{
    const u32 type = Elf::RelaType(rela.Info);
    const Hal::ModuleReloc kind = Hal::ClassifyModuleReloc(type);

    if (kind == Hal::ModuleReloc::None)
        return MakeSuccess();

    if (kind == Hal::ModuleReloc::Unsupported)
    {
        out.Printf("module: relocation type %u at 0x%p is not one a module may have\n",
            (ulong)type, rela.Offset);
        return MakeError(Stdlib::Error::NotImplemented);
    }

    if (rela.Offset > ctx.ImageSize - sizeof(ulong))
    {
        out.Printf("module: relocation at 0x%p is outside the image\n", rela.Offset);
        return MakeError(Stdlib::Error::DataCorrupt);
    }

    ulong value;
    if (kind == Hal::ModuleReloc::Relative)
    {
        value = ctx.Base + rela.Addend;
    }
    else
    {
        const u32 index = Elf::RelaSym(rela.Info);
        if (index == 0 || index >= ctx.SymCount)
        {
            out.Printf("module: relocation at 0x%p names symbol %u of %u\n",
                rela.Offset, (ulong)index, ctx.SymCount);
            return MakeError(Stdlib::Error::DataCorrupt);
        }

        const Elf::Sym& sym = ctx.Syms[index];
        if (SymDefined(sym))
        {
            if (sym.Value >= ctx.ImageSize)
            {
                out.Printf("module: symbol %u is outside the image\n", (ulong)index);
                return MakeError(Stdlib::Error::DataCorrupt);
            }
            value = ctx.Base + sym.Value + rela.Addend;
        }
        else
        {
            /* CheckImports vouched for the name, and for a miss being weak:
               an unresolved weak import is 0, plus the addend like any other */
            value = LookupKernel(SymName(ctx, sym)) + rela.Addend;
        }
    }

    /* A pointer in a packed struct need not be aligned */
    Stdlib::MemCpy(reinterpret_cast<void*>(ctx.Base + rela.Offset), &value, sizeof(value));
    return MakeSuccess();
}

Stdlib::Error Relocate(const LoadCtx& ctx, Stdlib::Printer& out)
{
    for (ulong i = 0; i < ctx.Header->Shnum; i++)
    {
        const Elf::Shdr& sh = ctx.Shdrs[i];

        if (sh.Type == Elf::ShtRel)
        {
            out.Printf("module: has REL relocations; only RELA ones are applied\n");
            return MakeError(Stdlib::Error::NotImplemented);
        }

        /* .rela.dyn and .rela.plt: what the linker left for the loader */
        if (sh.Type != Elf::ShtRela || (sh.Flags & Elf::ShfAlloc) == 0)
            continue;

        if (sh.Entsize != sizeof(Elf::Rela) || !Aligned8(sh.Offset) ||
            !InFile(ctx.Size, sh.Offset, sh.Size))
        {
            out.Printf("module: relocation section %u corrupt\n", i);
            return MakeError(Stdlib::Error::DataCorrupt);
        }

        const Elf::Rela* rela = reinterpret_cast<const Elf::Rela*>(ctx.File + sh.Offset);
        for (ulong j = 0; j < sh.Size / sizeof(Elf::Rela); j++)
        {
            Stdlib::Error err = ApplyReloc(ctx, rela[j], out);
            if (!err.Ok())
                return err;
        }
    }

    return MakeSuccess();
}

const ModuleInfo* FindInfo(const LoadCtx& ctx, Stdlib::Printer& out)
{
    for (ulong i = 1; i < ctx.SymCount; i++)
    {
        const Elf::Sym& sym = ctx.Syms[i];
        const char* name = SymName(ctx, sym);
        if (name == nullptr || Stdlib::StrCmp(name, InfoSymbol) != 0)
            continue;

        if (!SymDefined(sym) || sym.Size < sizeof(ModuleInfo) || !Aligned8(sym.Value) ||
            sym.Value > ctx.ImageSize - sizeof(ModuleInfo))
        {
            out.Printf("module: %s is not a module header\n", InfoSymbol);
            return nullptr;
        }

        return reinterpret_cast<const ModuleInfo*>(ctx.Base + sym.Value);
    }

    out.Printf("module: no %s -- not a module built with kmod::module!\n", InfoSymbol);
    return nullptr;
}

bool InExecutableSegment(const LoadCtx& ctx, ulong addr)
{
    for (ulong i = 0; i < ctx.Header->Phnum; i++)
    {
        const Elf::Phdr& ph = ctx.Phdrs[i];
        if (ph.Type == Elf::PtLoad && (ph.Flags & Elf::PfX) != 0 &&
            addr >= ctx.Base + ph.Vaddr && addr < ctx.Base + ph.Vaddr + ph.Memsz)
            return true;
    }

    return false;
}

/* The header, read once the relocations have made its pointers real: the
   kmod version it was built with, the kernel interface it was built against,
   its name, and whether init and exit point at its code */
Stdlib::Error CheckInfo(const LoadCtx& ctx, const ModuleInfo& info, LoadedModule& module,
    Stdlib::Printer& out)
{
    if (info.Magic != InfoMagic)
    {
        out.Printf("module: %s is not a module header\n", InfoSymbol);
        return MakeError(Stdlib::Error::BadMagic);
    }

    if (info.Version != InfoVersion)
    {
        out.Printf("module: header version %u, this kernel reads %u\n",
            (ulong)info.Version, (ulong)InfoVersion);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    char abi[InfoAbiLen];
    Stdlib::MemSet(abi, 0, sizeof(abi));
    Stdlib::MemCpy(abi, KernelAbi, sizeof(KernelAbi) - 1);
    if (Stdlib::MemCmp(abi, info.Abi, sizeof(abi)) != 0)
    {
        char theirs[AbiShown + 1];
        char ours[AbiShown + 1];
        Quote(theirs, info.Abi, AbiShown);
        Quote(ours, KernelAbi, AbiShown);
        out.Printf("module: built against another kernel interface (ffi %s, this kernel %s)"
            " -- rebuild it from this tree\n", theirs, ours);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    ulong len = 0;
    for (; len < InfoNameLen && info.Name[len] != '\0'; len++)
    {
        if (info.Name[len] <= ' ' || info.Name[len] > '~')
            break;
    }

    if (len == 0 || len == InfoNameLen || info.Name[len] != '\0')
    {
        out.Printf("module: the module's name is not a printable word of at most %u characters\n",
            ModuleTable::NameMax);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    Stdlib::MemCpy(module.Name, info.Name, len);
    module.Name[len] = '\0';

    if (!InExecutableSegment(ctx, reinterpret_cast<ulong>(info.Init)) ||
        !InExecutableSegment(ctx, reinterpret_cast<ulong>(info.Exit)))
    {
        out.Printf("module: %s: init or exit is not in the module's code\n", module.Name);
        return MakeError(Stdlib::Error::DataCorrupt);
    }

    return MakeSuccess();
}

/* Pages of its own for the image, mapped writable into one run of kernel VA
   -- Protect gives each segment its own permissions once it is filled in.
   They need not be physically contiguous. */
Stdlib::Error MapImage(LoadCtx& ctx, LoadedModule& module, Stdlib::Printer& out)
{
    auto& pt = Mm::PageTable::GetInstance();
    const ulong count = ctx.ImageSize / Const::PageSize;

    module.Pages = static_cast<Mm::Page**>(Mm::Alloc(count * sizeof(Mm::Page*), Tag));
    ulong* phys = static_cast<ulong*>(Mm::Alloc(count * sizeof(ulong), Tag));
    if (module.Pages == nullptr || phys == nullptr)
    {
        if (phys != nullptr)
            Mm::Free(phys);
        out.Printf("module: out of memory\n");
        return MakeError(Stdlib::Error::NoMemory);
    }

    for (ulong i = 0; i < count; i++)
    {
        Mm::Page* page = pt.AllocPage();
        if (page == nullptr)
            break;

        module.Pages[i] = page;
        module.PageCount++;
        phys[i] = page->GetPhyAddress();
    }

    void* va = nullptr;
    if (module.PageCount == count)
        va = Mm::MapPages(count, phys);
    Mm::Free(phys);

    if (va == nullptr)
    {
        out.Printf("module: no memory for a %u KiB image\n", ctx.ImageSize / Const::KB);
        return MakeError(Stdlib::Error::NoMemory);
    }

    module.Base = reinterpret_cast<ulong>(va);
    ctx.Base = module.Base;
    Stdlib::MemSet(va, 0, ctx.ImageSize);
    return MakeSuccess();
}

/* Undo MapImage, whatever part of it happened. Unmapping takes the page
   permissions with the PTEs and shoots down every CPU's TLB. */
void ReleaseImage(LoadedModule& module)
{
    auto& pt = Mm::PageTable::GetInstance();

    if (module.Base != 0)
        Mm::UnmapPages(reinterpret_cast<void*>(module.Base), module.PageCount);

    for (ulong i = 0; i < module.PageCount; i++)
        pt.FreePage(module.Pages[i]);

    if (module.Pages != nullptr)
        Mm::Free(module.Pages);

    module.Base = 0;
    module.PageCount = 0;
    module.Pages = nullptr;
}

/* Each segment's permissions from its program header. Only then is the code
   executable -- and nothing is ever writable and executable at once. */
Stdlib::Error Protect(const LoadCtx& ctx, Stdlib::Printer& out)
{
    auto& pt = Mm::PageTable::GetInstance();

    /* A page no segment claims (there are none, the way the Makefile links)
       is left read-only */
    bool ok = pt.SetRangeProtection(ctx.Base, ctx.ImageSize, false, false);
    for (ulong i = 0; ok && i < ctx.Header->Phnum; i++)
    {
        const Elf::Phdr& ph = ctx.Phdrs[i];
        if (ph.Type != Elf::PtLoad || ph.Memsz == 0)
            continue;

        const bool writable = (ph.Flags & Elf::PfW) != 0;
        const bool executable = (ph.Flags & Elf::PfX) != 0;
        Trace(ModuleLL, "module: [0x%p, 0x%p) %s%s", ctx.Base + ph.Vaddr,
            ctx.Base + ph.Vaddr + ph.Memsz, writable ? "rw" : "r", executable ? "x" : "");
        ok = pt.SetRangeProtection(ctx.Base + ph.Vaddr,
            Stdlib::RoundUp(ph.Memsz, Const::PageSize), writable, executable);
    }

    /* SetRangeProtection flushes only this CPU's TLB */
    CpuTable::GetInstance().InvalidateTlbRange(ctx.Base, ctx.ImageSize / Const::PageSize);

    if (!ok)
    {
        out.Printf("module: cannot set the image's page permissions\n");
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    for (ulong i = 0; i < ctx.Header->Phnum; i++)
    {
        const Elf::Phdr& ph = ctx.Phdrs[i];
        if (ph.Type == Elf::PtLoad && (ph.Flags & Elf::PfX) != 0)
            Hal::SyncInstructionCache(ctx.Base + ph.Vaddr, ph.Memsz);
    }

    return MakeSuccess();
}

/* Everything between the checks on the file and the module's init: the
   image mapped, filled and relocated, its header checked, its permissions
   set. On failure the caller releases the image. */
Stdlib::Error Prepare(LoadCtx& ctx, LoadedModule& module, const ModuleInfo*& info,
    Stdlib::Printer& out)
{
    Stdlib::Error err = MapImage(ctx, module, out);
    if (!err.Ok())
        return err;

    /* The pages came zeroed, which is what a segment's tail past its file
       contents -- .bss -- has to be */
    for (ulong i = 0; i < ctx.Header->Phnum; i++)
    {
        const Elf::Phdr& ph = ctx.Phdrs[i];
        if (ph.Type == Elf::PtLoad && ph.Filesz != 0)
            Stdlib::MemCpy(reinterpret_cast<void*>(ctx.Base + ph.Vaddr), ctx.File + ph.Offset,
                ph.Filesz);
    }

    err = Relocate(ctx, out);
    if (!err.Ok())
        return err;

    info = FindInfo(ctx, out);
    if (info == nullptr)
        return MakeError(Stdlib::Error::NotFound);

    err = CheckInfo(ctx, *info, module, out);
    if (!err.Ok())
        return err;

    return Protect(ctx, out);
}

}

ModuleTable::ModuleTable()
{
    List.Init();
}

ModuleTable::~ModuleTable()
{
}

LoadedModule* ModuleTable::FindLocked(const char* name)
{
    for (Stdlib::ListEntry* entry = List.Flink; entry != &List; entry = entry->Flink)
    {
        LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
        if (Stdlib::StrCmp(module->Name, name) == 0)
            return module;
    }

    return nullptr;
}

Stdlib::Error ModuleTable::Load(const void* image, ulong size, Stdlib::Printer& out)
{
    LoadCtx ctx(image, size);

    Stdlib::Error err = CheckHeader(ctx, out);
    if (err.Ok())
        err = CheckSegments(ctx, out);
    if (err.Ok())
        err = FindSymbols(ctx, out);
    if (err.Ok())
        err = CheckImports(ctx, out);
    if (!err.Ok())
        return err;

    LoadedModule* module = Mm::TAlloc<LoadedModule, Tag>();
    if (module == nullptr)
    {
        out.Printf("module: out of memory\n");
        return MakeError(Stdlib::Error::NoMemory);
    }
    module->Imports = ctx.Imports;
    module->PermanentBy = ctx.PermanentBy;

    Stdlib::AutoLock lock(Lock);

    const ModuleInfo* info = nullptr;
    err = Prepare(ctx, *module, info, out);
    if (err.Ok() && FindLocked(module->Name) != nullptr)
    {
        out.Printf("module: %s is already loaded\n", module->Name);
        err = MakeError(Stdlib::Error::AlreadyExists);
    }

    if (err.Ok())
    {
        module->State = info->Init();
        if (module->State == nullptr)
        {
            out.Printf("module: %s: init failed\n", module->Name);
            err = MakeError(Stdlib::Error::Unsuccessful);
        }
    }

    if (!err.Ok())
    {
        ReleaseImage(*module);
        Mm::Free(module);
        return err;
    }

    module->Exit = info->Exit;
    List.InsertTail(&module->ListEntry);

    Trace(0, "module: %s loaded at 0x%p, %u KiB, %u kernel imports", module->Name,
        module->Base, (module->PageCount * Const::PageSize) / Const::KB, module->Imports);
    out.Printf("module: %s loaded at 0x%p\n", module->Name, module->Base);
    return MakeSuccess();
}

Stdlib::Error ModuleTable::LoadFile(const char* path, Stdlib::Printer& out)
{
    auto& vfs = Vfs::GetInstance();

    File* file = vfs.Open(path, Vfs::OpenRead);
    if (file == nullptr)
    {
        out.Printf("module: cannot open %s\n", path);
        return MakeError(Stdlib::Error::NotFound);
    }

    const ulong size = vfs.GetSize(file);
    if (size == 0 || size > MaxImageSize)
    {
        vfs.Close(file);
        out.Printf("module: %s is %u bytes; a module file is 1 to %u\n", path, size, MaxImageSize);
        return MakeError(Stdlib::Error::BadSize);
    }

    /* Mm::Alloc hands out a block this size page-aligned, and anything
       smaller 8-byte aligned: either suits Load */
    u8* buf = static_cast<u8*>(Mm::Alloc(size, Tag));
    if (buf == nullptr)
    {
        vfs.Close(file);
        out.Printf("module: no memory to read %s\n", path);
        return MakeError(Stdlib::Error::NoMemory);
    }

    ulong done = 0;
    while (done < size)
    {
        ulong got = 0;
        if (!vfs.Read(file, buf + done, size - done, got) || got == 0)
            break;
        done += got;
    }
    vfs.Close(file);

    Stdlib::Error err;
    if (done == size)
    {
        err = Load(buf, size, out);
    }
    else
    {
        out.Printf("module: cannot read %s\n", path);
        err = MakeError(Stdlib::Error::IO);
    }

    Mm::Free(buf);
    return err;
}

void ModuleTable::UnloadLocked(LoadedModule* module)
{
    /* The module's state goes, and everything the module holds with it: exit
       returns once the commands it registered are gone and no call into it is
       still running (kernel_cmd_unregister waits those out). Only then may its
       code go. */
    module->ListEntry.RemoveInit();
    module->Exit(module->State);

    Trace(0, "module: %s unloaded", module->Name);

    ReleaseImage(*module);
    Mm::Free(module);
}

Stdlib::Error ModuleTable::Unload(const char* name, Stdlib::Printer& out)
{
    Stdlib::AutoLock lock(Lock);

    LoadedModule* module = FindLocked(name);
    if (module == nullptr)
    {
        out.Printf("module: %s is not loaded\n", name);
        return MakeError(Stdlib::Error::NotFound);
    }

    if (module->PermanentBy != nullptr)
    {
        out.Printf("module: %s is permanent: it imports %s, which nothing undoes\n",
            module->Name, module->PermanentBy);
        return MakeError(Stdlib::Error::InvalidState);
    }

    UnloadLocked(module);
    out.Printf("module: %s unloaded\n", name);
    return MakeSuccess();
}

void ModuleTable::UnloadAll()
{
    Stdlib::AutoLock lock(Lock);

    /* Newest first: the reverse of the order they came in */
    Stdlib::ListEntry* entry = List.Blink;
    while (entry != &List)
    {
        LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
        entry = entry->Blink;

        if (module->PermanentBy != nullptr)
            Trace(0, "module: %s is permanent, left loaded", module->Name);
        else
            UnloadLocked(module);
    }
}

bool ModuleTable::IsLoaded(const char* name)
{
    Stdlib::AutoLock lock(Lock);

    return FindLocked(name) != nullptr;
}

void ModuleTable::Dump(Stdlib::Printer& out)
{
    Stdlib::AutoLock lock(Lock);

    if (List.IsEmpty())
    {
        out.Printf("no modules loaded\n");
        return;
    }

    for (Stdlib::ListEntry* entry = List.Flink; entry != &List; entry = entry->Flink)
    {
        LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
        out.Printf("%s  %u KiB at 0x%p, %u kernel imports%s\n", module->Name,
            (module->PageCount * Const::PageSize) / Const::KB, module->Base, module->Imports,
            (module->PermanentBy != nullptr) ? ", permanent" : "");
    }
}

}
