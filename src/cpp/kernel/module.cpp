#include "module.h"
#include "elf.h"
#include "trace.h"
#include "cpu.h"
#include "atomic.h"
#include "task.h"
#include "sched.h"
#include "preempt.h"

#include <hal/mmu.h>
#include <hal/module.h>
#include <mm/new.h>
#include <mm/page_table.h>
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

/* The filesystem layer is Rust (src/rust/fs): a module's file is read whole,
   through the same calls a module itself reads a file by. */
extern "C" {
long kernel_file_size(const char* path, ulong len);
long kernel_file_read(const char* path, ulong len, void* buf, ulong cap);
}

namespace Kernel
{

/* Pages of its own at one run of VA -- up to PageTable::MaxLargeMapPages of
   them, physically scattered, mapped writable: a module's image, or the .ko
   file it is read from */
struct ModulePageRun
{
    ulong Base;
    ulong Count;
    Mm::Page** Pages;
};

enum class ModulePhase
{
    Loading,    /* mapped and listed, its init running */
    Live,
    Unloading,  /* its exit running */
};

/* One of a module's functions, from the table the build puts in its .ko */
struct ModuleSymbol
{
    ulong Offset;       /* from the image's base */
    const char* Name;   /* in the image's pages, past the segments */
};

struct LoadedModule
{
    LoadedModule()
        : Phase(ModulePhase::Loading)
        , ImageSize(0)
        , TextEnd(0)
        , Symbols(nullptr)
        , SymbolCount(0)
        , Imports(0)
        , PermanentBy(nullptr)
        , Instance(nullptr)
        , Exit(nullptr)
    {
        ListEntry.Init();
        Name[0] = '\0';
        Image.Base = 0;
        Image.Count = 0;
        Image.Pages = nullptr;
    }

    Stdlib::ListEntry ListEntry;
    char Name[ModuleTable::NameMax + 1];
    ModulePhase Phase;            /* changed only under ModuleTable::Lock */
    ModulePageRun Image;          /* the segments, then the symbol table */
    ulong ImageSize;              /* the segments' extent */
    ulong TextEnd;                /* where the last executable segment ends */
    const ModuleSymbol* Symbols;  /* sorted by offset */
    ulong SymbolCount;
    ulong Imports;                /* kernel functions it binds to */
    const char* PermanentBy;      /* what keeps it from being unloaded, if anything */
    void* Instance;               /* what its init returned; its exit takes it back */
    void (*Exit)(void* instance);
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
    void (*Exit)(void* instance);
};

static_assert(sizeof(ModuleInfo) == 120, "kmod::ModuleInfo layout");
static_assert(InfoNameLen == ModuleTable::NameMax + 1, "kmod::NAME_LEN");

const char KernelAbi[] = NOS_MODULE_ABI;
static_assert(sizeof(KernelAbi) - 1 <= InfoAbiLen, "NOS_MODULE_ABI is too long");

/* How much of each digest a refusal for a mismatch quotes */
const ulong AbiShown = 12;

/* A module's image -- its segments and its symbol table -- and the .ko file
   it comes from are at most the longest run MapLargePages maps: 16 MiB */
const ulong MaxImageSize = Mm::PageTable::MaxLargeMapPages * Const::PageSize;

/* A linked .ko has half a dozen program headers; the segment checks compare
   every pair, so a corrupt count must not make that billions */
const ulong MaxProgramHeaders = 64;

/* Kernel functions that take a callback for good: nothing hands a net
   device or a softirq handler back once it is registered. A module that
   imports one is permanent -- rmmod would free code the kernel may still
   call into -- the way a Linux module without an exit is. (A disk's driver
   registers with the block layer as a Rust trait object, from inside the
   kernel image: there is no name for a module to import.) */
const char* const PermanentImports[] = {
    "kernel_netdev_register",
    "kernel_softirq_register",
};

/* The section the Makefile adds to every .ko: its functions, a line each,
   "<hex offset> <name>", in address order -- llvm-nm -n -C's, so the Rust
   names come demangled and the kernel needs no demangler */
const char SymbolSection[] = ".nos_syms";
const ulong MaxHexDigits = 16;

/* insmod and rmmod in a task of their own */
const ulong JobOutputSize = 2048;
const ulong JobPollMs = 10;
const long JobRunning = 0;
const long JobDone = 1;
const long JobAbandoned = 2;

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

const char* PhaseText(ModulePhase phase)
{
    switch (phase)
    {
    case ModulePhase::Loading:
        return "being loaded";
    case ModulePhase::Unloading:
        return "being unloaded";
    default:
        return "loaded";
    }
}

/* Undo AllocPageRun, whatever part of it happened. Unmapping takes the page
   permissions with the PTEs and shoots down every CPU's TLB. */
void FreePageRun(ModulePageRun& run)
{
    auto& pt = Mm::PageTable::GetInstance();

    if (run.Base != 0)
        Mm::UnmapLargePages(reinterpret_cast<void*>(run.Base), run.Count);

    for (ulong i = 0; i < run.Count; i++)
        pt.FreePage(run.Pages[i]);

    if (run.Pages != nullptr)
        Mm::Free(run.Pages);

    run.Base = 0;
    run.Count = 0;
    run.Pages = nullptr;
}

bool AllocPageRun(ModulePageRun& run, ulong bytes)
{
    auto& pt = Mm::PageTable::GetInstance();
    const ulong count = Stdlib::RoundUp(bytes, Const::PageSize) / Const::PageSize;

    run.Base = 0;
    run.Count = 0;
    run.Pages = nullptr;
    if (count == 0 || count > Mm::PageTable::MaxLargeMapPages)
        return false;

    run.Pages = static_cast<Mm::Page**>(Mm::Alloc(count * sizeof(Mm::Page*), Tag));
    ulong* phys = static_cast<ulong*>(Mm::Alloc(count * sizeof(ulong), Tag));
    if (run.Pages != nullptr && phys != nullptr)
    {
        for (ulong i = 0; i < count; i++)
        {
            Mm::Page* page = pt.AllocPage();
            if (page == nullptr)
                break;

            run.Pages[i] = page;
            run.Count++;
            phys[i] = page->GetPhyAddress();
        }

        if (run.Count == count)
            run.Base = reinterpret_cast<ulong>(Mm::MapLargePages(count, phys));
    }

    if (phys != nullptr)
        Mm::Free(phys);

    if (run.Base == 0)
    {
        FreePageRun(run);
        return false;
    }

    return true;
}

void ReleaseModule(LoadedModule* module)
{
    FreePageRun(module->Image);
    Mm::Free(module);
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
        , RunSize(0)
        , TextEnd(0)
        , Imports(0)
        , PermanentBy(nullptr)
        , FuncText(nullptr)
        , FuncTextSize(0)
        , FuncLines(0)
    {
    }

    const u8* File;
    ulong Size;
    const Elf::Ehdr* Header;
    const Elf::Phdr* Phdrs;
    const Elf::Shdr* Shdrs;
    const Elf::Sym* Syms;    /* .dynsym */
    ulong SymCount;
    const char* Strs;        /* .dynstr */
    ulong StrSize;
    ulong Base;              /* where the image is mapped */
    ulong ImageSize;         /* the segments' extent, a whole number of pages */
    ulong RunSize;           /* all the image's pages, symbol table included */
    ulong TextEnd;
    ulong Imports;
    const char* PermanentBy; /* the first of PermanentImports it imports */
    const char* FuncText;    /* .nos_syms, if it has one */
    ulong FuncTextSize;
    ulong FuncLines;
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
        if ((ph.Flags & Elf::PfX) != 0 && ph.Vaddr + ph.Memsz > ctx.TextEnd)
            ctx.TextEnd = ph.Vaddr + ph.Memsz;
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

/* The .nos_syms section, measured: BuildFunctions copies and indexes it once
   the image is mapped. A .ko without one -- linked outside the Makefile --
   loads all the same, and its frames go unnamed. */
void FindFunctions(LoadCtx& ctx)
{
    const Elf::Ehdr* eh = ctx.Header;
    if (eh->Shstrndx == 0 || eh->Shstrndx >= eh->Shnum)
        return;

    const Elf::Shdr& names = ctx.Shdrs[eh->Shstrndx];
    if (names.Type != Elf::ShtStrtab || names.Size == 0 ||
        !InFile(ctx.Size, names.Offset, names.Size) ||
        ctx.File[names.Offset + names.Size - 1] != '\0')
        return;

    const char* sectionNames = reinterpret_cast<const char*>(ctx.File + names.Offset);
    for (ulong i = 0; i < eh->Shnum; i++)
    {
        const Elf::Shdr& sh = ctx.Shdrs[i];
        if (sh.Name >= names.Size || Stdlib::StrCmp(sectionNames + sh.Name, SymbolSection) != 0)
            continue;

        if (sh.Size == 0 || !InFile(ctx.Size, sh.Offset, sh.Size))
            return;

        ulong lines = 0;
        for (ulong j = 0; j < sh.Size; j++)
        {
            if (ctx.File[sh.Offset + j] == '\n')
                lines++;
        }

        const ulong bytes = lines * sizeof(ModuleSymbol) + sh.Size + 1;
        if (lines == 0 || bytes > MaxImageSize - ctx.ImageSize)
        {
            Trace(ModuleLL, "module: %u function names left out, %u bytes of them", lines, sh.Size);
            return;
        }

        ctx.FuncText = reinterpret_cast<const char*>(ctx.File + sh.Offset);
        ctx.FuncTextSize = sh.Size;
        ctx.FuncLines = lines;
        return;
    }
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

/* One line of .nos_syms, "<hex offset> <name>", its newline already a NUL */
bool ParseFunctionLine(char* line, ulong& offset, const char*& name)
{
    offset = 0;
    ulong digits = 0;
    char* p = line;
    for (; *p != ' ' && *p != '\0'; p++, digits++)
    {
        const u8 nibble = Stdlib::HexCharToNibble(*p);
        if (nibble > 0xF || digits == MaxHexDigits)
            return false;
        offset = (offset << 4) | nibble;
    }

    if (digits == 0 || *p != ' ' || p[1] == '\0')
        return false;

    name = p + 1;
    return true;
}

/* The function names, into the image's own pages past the segments: an
   index of offsets there, then the names it points at. Protect leaves them
   read-only. A table that does not parse -- out of order, or naming offsets
   outside the image -- is dropped whole, and the module loads nameless. */
void BuildFunctions(const LoadCtx& ctx, LoadedModule& module)
{
    if (ctx.FuncText == nullptr)
        return;

    ModuleSymbol* index = reinterpret_cast<ModuleSymbol*>(ctx.Base + ctx.ImageSize);
    char* text = reinterpret_cast<char*>(index + ctx.FuncLines);
    Stdlib::MemCpy(text, ctx.FuncText, ctx.FuncTextSize);
    text[ctx.FuncTextSize] = '\0';

    ulong count = 0;
    char* line = text;
    char* const end = text + ctx.FuncTextSize;
    while (line < end)
    {
        char* newline = line;
        while (newline < end && *newline != '\n')
            newline++;
        if (newline == end)
            break;
        *newline = '\0';

        ulong offset;
        const char* name;
        if (!ParseFunctionLine(line, offset, name) || offset >= ctx.ImageSize ||
            (count != 0 && offset < index[count - 1].Offset))
        {
            Trace(0, "module: %s: function table bad at line %u, left out", module.Name, count + 1);
            return;
        }

        index[count].Offset = offset;
        index[count].Name = name;
        count++;
        line = newline + 1;
    }

    module.Symbols = index;
    module.SymbolCount = count;
}

/* The function offset lies in: the last one starting at or before it, if
   offset is in code at all */
const ModuleSymbol* FindFunction(const LoadedModule& module, ulong offset)
{
    if (module.SymbolCount == 0 || offset >= module.TextEnd ||
        offset < module.Symbols[0].Offset)
        return nullptr;

    ulong lo = 0;
    ulong hi = module.SymbolCount;
    while (hi - lo > 1)
    {
        const ulong mid = lo + (hi - lo) / 2;
        if (module.Symbols[mid].Offset <= offset)
            lo = mid;
        else
            hi = mid;
    }

    return &module.Symbols[lo];
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

/* The image's pages -- the segments, and after them the function names, if
   the .ko has any -- mapped writable into one run of kernel VA. Protect gives
   each segment its own permissions once it is filled in. */
Stdlib::Error MapImage(LoadCtx& ctx, LoadedModule& module, Stdlib::Printer& out)
{
    ulong bytes = ctx.ImageSize;
    if (ctx.FuncText != nullptr)
        bytes += ctx.FuncLines * sizeof(ModuleSymbol) + ctx.FuncTextSize + 1;

    if (!AllocPageRun(module.Image, bytes))
    {
        out.Printf("module: no memory for a %u KiB image\n", bytes / Const::KB);
        return MakeError(Stdlib::Error::NoMemory);
    }

    ctx.Base = module.Image.Base;
    ctx.RunSize = module.Image.Count * Const::PageSize;
    module.ImageSize = ctx.ImageSize;
    module.TextEnd = ctx.TextEnd;
    Stdlib::MemSet(reinterpret_cast<void*>(ctx.Base), 0, ctx.RunSize);
    return MakeSuccess();
}

/* Each segment's permissions from its program header. Only then is the code
   executable -- and nothing is ever writable and executable at once. */
Stdlib::Error Protect(const LoadCtx& ctx, Stdlib::Printer& out)
{
    auto& pt = Mm::PageTable::GetInstance();

    /* What no segment claims -- the function names -- is left read-only */
    bool ok = pt.SetRangeProtection(ctx.Base, ctx.RunSize, false, false);
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
    CpuTable::GetInstance().InvalidateTlbRange(ctx.Base, ctx.RunSize / Const::PageSize);

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
   image mapped, filled and relocated, its function names indexed, its header
   checked, its permissions set. On failure the caller releases the image. */
Stdlib::Error Prepare(LoadCtx& ctx, LoadedModule& module, const ModuleInfo*& info,
    Stdlib::Printer& out)
{
    Stdlib::Error err = MapImage(ctx, module, out);
    if (!err.Ok())
        return err;

    /* The pages are zeroed, which is what a segment's tail past its file
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

    BuildFunctions(ctx, module);
    return Protect(ctx, out);
}

/* An insmod or rmmod handed to a task of its own, and what the task has to
   say about it. Two references: the task's, and the one of whoever started
   it, who may stop waiting before the task is done. */
struct ModuleJob
{
    ModuleJob(bool unload, const char* arg)
        : Refs(2)
        , State(JobRunning)
        , Unload(unload)
        , Result(Stdlib::Error::Success)
    {
        /* StartJob has seen to it that arg fits */
        Stdlib::MemCpy(Arg, arg, Stdlib::StrLen(arg) + 1);
        Output[0] = '\0';
    }

    Atomic Refs;
    Atomic State;   /* JobRunning, then JobDone or JobAbandoned */
    bool Unload;
    int Result;
    char Arg[ModuleTable::PathMax + 1];
    char Output[JobOutputSize];
};

void PutJob(ModuleJob* job)
{
    if (job->Refs.DecAndTest())
        Mm::Free(job);
}

void RunJob(void* ctx)
{
    ModuleJob* job = static_cast<ModuleJob*>(ctx);
    auto& modules = ModuleTable::GetInstance();

    Stdlib::BufferPrinter out(job->Output, sizeof(job->Output));
    Stdlib::Error err = job->Unload ? modules.Unload(job->Arg, out) : modules.LoadFile(job->Arg, out);
    job->Result = err.GetCode();

    /* Handed over -- unless whoever started this has stopped waiting, and then
       the kernel log is where it can still be read */
    if (job->State.Cmpxchg(JobDone, JobRunning) != JobRunning)
    {
        Trace(0, "module: %s %s, done in the background, error %u: %s",
            job->Unload ? "rmmod" : "insmod", job->Arg, (ulong)job->Result, job->Output);
    }

    PutJob(job);
}

}

ModuleTable::ModuleTable()
    : Lock(false)
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

void ModuleTable::Remove(LoadedModule* module)
{
    const ulong flags = Lock.LockIrqSave();
    module->ListEntry.RemoveInit();
    Lock.UnlockIrqRestore(flags);
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

    FindFunctions(ctx);

    LoadedModule* module = Mm::TAlloc<LoadedModule, Tag>();
    if (module == nullptr)
    {
        out.Printf("module: out of memory\n");
        return MakeError(Stdlib::Error::NoMemory);
    }
    module->Imports = ctx.Imports;
    module->PermanentBy = ctx.PermanentBy;

    const ModuleInfo* info = nullptr;
    err = Prepare(ctx, *module, info, out);
    if (!err.Ok())
    {
        ReleaseModule(module);
        return err;
    }

    /* On the list while its init runs -- a backtrace from inside it can name
       it -- unless the name is taken, whatever that module is doing */
    bool taken;
    ModulePhase phase = ModulePhase::Live;
    {
        const ulong flags = Lock.LockIrqSave();
        LoadedModule* other = FindLocked(module->Name);
        taken = (other != nullptr);
        if (taken)
            phase = other->Phase;
        else
            List.InsertTail(&module->ListEntry);
        Lock.UnlockIrqRestore(flags);
    }

    if (taken)
    {
        out.Printf("module: %s is %s\n", module->Name,
            (phase == ModulePhase::Live) ? "already loaded" : PhaseText(phase));
        ReleaseModule(module);
        return MakeError(Stdlib::Error::AlreadyExists);
    }

    /* Once it is Live another task may unload it at any moment: what is
       said about it here is said from copies */
    char name[NameMax + 1];
    Stdlib::MemCpy(name, module->Name, sizeof(name));
    const ulong base = module->Image.Base;
    const ulong kib = module->ImageSize / Const::KB;
    const ulong imports = module->Imports;
    const ulong functions = module->SymbolCount;

    module->Exit = info->Exit;
    module->Instance = info->Init();
    const bool live = (module->Instance != nullptr);

    {
        const ulong flags = Lock.LockIrqSave();
        if (live)
            module->Phase = ModulePhase::Live;
        else
            module->ListEntry.RemoveInit();
        Lock.UnlockIrqRestore(flags);
    }

    if (!live)
    {
        out.Printf("module: %s: init failed\n", name);
        ReleaseModule(module);
        return MakeError(Stdlib::Error::Unsuccessful);
    }

    Trace(0, "module: %s loaded at 0x%p, %u KiB, %u kernel imports, %u functions named",
        name, base, kib, imports, functions);
    out.Printf("module: %s loaded at 0x%p\n", name, base);
    return MakeSuccess();
}

Stdlib::Error ModuleTable::LoadFile(const char* path, Stdlib::Printer& out)
{
    const ulong pathLen = Stdlib::StrLen(path);
    const long got = kernel_file_size(path, pathLen);
    if (got < 0)
    {
        out.Printf("module: cannot open %s\n", path);
        return MakeError(Stdlib::Error::NotFound);
    }

    const ulong size = (ulong)got;
    if (size == 0 || size > MaxImageSize)
    {
        out.Printf("module: %s is %u bytes; a module file is 1 to %u\n", path, size, MaxImageSize);
        return MakeError(Stdlib::Error::BadSize);
    }

    /* A run of pages, page-aligned: what Load wants, and as big as a module
       may be, where the heap stops at 512 KiB */
    ModulePageRun buf;
    if (!AllocPageRun(buf, size))
    {
        out.Printf("module: no memory to read %s\n", path);
        return MakeError(Stdlib::Error::NoMemory);
    }

    u8* data = reinterpret_cast<u8*>(buf.Base);
    const long done = kernel_file_read(path, pathLen, data, size);

    Stdlib::Error err;
    if (done == (long)size)
    {
        err = Load(data, size, out);
    }
    else
    {
        out.Printf("module: cannot read %s\n", path);
        err = MakeError(Stdlib::Error::IO);
    }

    FreePageRun(buf);
    return err;
}

Stdlib::Error ModuleTable::Unload(const char* name, Stdlib::Printer& out)
{
    LoadedModule* module;
    ModulePhase phase = ModulePhase::Live;
    const char* permanentBy = nullptr;
    {
        const ulong flags = Lock.LockIrqSave();
        module = FindLocked(name);
        if (module != nullptr)
        {
            phase = module->Phase;
            permanentBy = module->PermanentBy;
            if (phase == ModulePhase::Live && permanentBy == nullptr)
                module->Phase = ModulePhase::Unloading;
        }
        Lock.UnlockIrqRestore(flags);
    }

    if (module == nullptr)
    {
        out.Printf("module: %s is not loaded\n", name);
        return MakeError(Stdlib::Error::NotFound);
    }

    if (phase != ModulePhase::Live)
    {
        out.Printf("module: %s is %s already\n", name, PhaseText(phase));
        return MakeError(Stdlib::Error::InvalidState);
    }

    if (permanentBy != nullptr)
    {
        out.Printf("module: %s is permanent: it imports %s, which nothing undoes\n",
            name, permanentBy);
        return MakeError(Stdlib::Error::InvalidState);
    }

    /* The module's state goes, and everything the module holds with it: exit
       returns once the commands it registered are gone and no call into it is
       still running (kernel_cmd_unregister waits those out). Only then may its
       code go. Nothing is held meanwhile: this may take a while. */
    module->Exit(module->Instance);
    Remove(module);
    ReleaseModule(module);

    Trace(0, "module: %s unloaded", name);
    out.Printf("module: %s unloaded\n", name);
    return MakeSuccess();
}

void ModuleTable::UnloadAll()
{
    for (;;)
    {
        /* Newest first: the reverse of the order they came in */
        LoadedModule* module = nullptr;
        {
            const ulong flags = Lock.LockIrqSave();
            for (Stdlib::ListEntry* entry = List.Blink; entry != &List; entry = entry->Blink)
            {
                LoadedModule* candidate = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
                if (candidate->Phase == ModulePhase::Live && candidate->PermanentBy == nullptr)
                {
                    candidate->Phase = ModulePhase::Unloading;
                    module = candidate;
                    break;
                }
            }
            Lock.UnlockIrqRestore(flags);
        }

        if (module == nullptr)
            break;

        char name[NameMax + 1];
        Stdlib::MemCpy(name, module->Name, sizeof(name));
        module->Exit(module->Instance);
        Remove(module);
        ReleaseModule(module);
        Trace(0, "module: %s unloaded", name);
    }

    ulong left = 0;
    {
        const ulong flags = Lock.LockIrqSave();
        for (Stdlib::ListEntry* entry = List.Flink; entry != &List; entry = entry->Flink)
            left++;
        Lock.UnlockIrqRestore(flags);
    }

    if (left != 0)
        Trace(0, "module: %u left loaded: permanent, or in another task's hands", left);
}

void ModuleTable::StartLoad(const char* path, Stdlib::Printer& out)
{
    StartJob(false, path, out);
}

void ModuleTable::StartUnload(const char* name, Stdlib::Printer& out)
{
    StartJob(true, name, out);
}

void ModuleTable::StartJob(bool unload, const char* arg, Stdlib::Printer& out)
{
    const char* what = unload ? "rmmod" : "insmod";
    if (Stdlib::StrLen(arg) > PathMax)
    {
        out.Printf("%s: %s is too long\n", what, arg);
        return;
    }

    ModuleJob* job = Mm::TAlloc<ModuleJob, Tag>(unload, arg);
    Task* task = (job != nullptr) ? Mm::TAlloc<Task, Tag>("%s", what) : nullptr;
    if (task == nullptr || !task->Start(RunJob, job))
    {
        /* Never ran: no one else holds the job */
        if (task != nullptr)
            task->Put();
        if (job != nullptr)
            Mm::Free(job);
        out.Printf("%s: cannot start a task for it\n", what);
        return;
    }

    /* The run queue keeps a reference of its own until the task has exited */
    task->Put();

    for (ulong waited = 0; waited < ShellWaitMs && job->State.Get() == JobRunning;
         waited += JobPollMs)
        Sleep(JobPollMs * Const::NanoSecsInMs);

    if (job->State.Cmpxchg(JobAbandoned, JobRunning) == JobRunning)
    {
        out.Printf("%s: %s is taking its time and goes on in the background --"
            " lsmod shows where it is, the kernel log will say how it ended\n", what, arg);
    }
    else
    {
        out.PrintString(job->Output);
        if (job->Result != Stdlib::Error::Success)
            out.Printf("%s: %s failed, error %u\n", what, arg, (ulong)job->Result);
    }

    PutJob(job);
}

bool ModuleTable::IsLoaded(const char* name)
{
    const ulong flags = Lock.LockIrqSave();
    const bool loaded = (FindLocked(name) != nullptr);
    Lock.UnlockIrqRestore(flags);
    return loaded;
}

void ModuleTable::Dump(Stdlib::Printer& out)
{
    /* A module at a time, copied under the lock and printed without it: a
       console line can take a while */
    ulong shown = 0;
    for (;; shown++)
    {
        char name[NameMax + 1];
        ulong base = 0;
        ulong kib = 0;
        ulong imports = 0;
        ModulePhase phase = ModulePhase::Live;
        bool permanent = false;
        bool found = false;

        {
            const ulong flags = Lock.LockIrqSave();
            ulong i = 0;
            for (Stdlib::ListEntry* entry = List.Flink; entry != &List; entry = entry->Flink, i++)
            {
                if (i != shown)
                    continue;

                const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
                Stdlib::MemCpy(name, module->Name, sizeof(name));
                base = module->Image.Base;
                kib = (module->Image.Count * Const::PageSize) / Const::KB;
                imports = module->Imports;
                phase = module->Phase;
                permanent = (module->PermanentBy != nullptr);
                found = true;
                break;
            }
            Lock.UnlockIrqRestore(flags);
        }

        if (!found)
            break;

        out.Printf("%s  %u KiB at 0x%p, %u kernel imports%s%s\n", name, kib, base, imports,
            permanent ? ", permanent" : "",
            (phase == ModulePhase::Loading) ? ", loading" :
            (phase == ModulePhase::Unloading) ? ", unloading" : "");
    }

    if (shown == 0)
        out.Printf("no modules loaded\n");
}

bool ModuleTable::Describe(ulong addr, char* buf, ulong size)
{
    bool acquired = false;
    const ulong flags = Lock.TryLockIrqSave(acquired);
    if (!acquired)
    {
        PreemptIrqRestore(flags);
        return false;
    }

    bool found = false;
    for (Stdlib::ListEntry* entry = List.Flink; entry != &List; entry = entry->Flink)
    {
        const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
        if (addr < module->Image.Base || addr >= module->Image.Base + module->ImageSize)
            continue;

        const ulong offset = addr - module->Image.Base;
        const ModuleSymbol* function = FindFunction(*module, offset);
        if (function != nullptr)
            Stdlib::SnPrintf(buf, size, "%s+0x%p [%s]", function->Name,
                offset - function->Offset, module->Name);
        else
            Stdlib::SnPrintf(buf, size, "[%s]+0x%p", module->Name, offset);
        found = true;
        break;
    }

    Lock.UnlockIrqRestore(flags);
    return found;
}

bool ModuleTable::DescribeAll(char* buf, ulong size)
{
    if (size == 0)
        return false;

    bool acquired = false;
    const ulong flags = Lock.TryLockIrqSave(acquired);
    if (!acquired)
    {
        PreemptIrqRestore(flags);
        return false;
    }

    buf[0] = '\0';
    ulong pos = 0;
    bool any = false;
    for (Stdlib::ListEntry* entry = List.Flink; entry != &List && pos + 1 < size;
         entry = entry->Flink)
    {
        const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, ListEntry);
        Stdlib::SnPrintf(buf + pos, size - pos, "%s%s 0x%p+0x%p", any ? ", " : "",
            module->Name, module->Image.Base, module->ImageSize);
        buf[size - 1] = '\0';
        pos += Stdlib::StrLen(buf + pos);
        any = true;
    }

    Lock.UnlockIrqRestore(flags);
    return any;
}

}
