#include "module.h"
#include "module_exports.h"
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


/* The filesystem layer is Rust (src/crates/fs): a module's file is read whole,
   through the same calls a module itself reads a file by. */
extern "C" {
long kernel_file_size(const char* path, ulong len);
long kernel_file_read(const char* path, ulong len, void* buf, ulong cap);
}

namespace kernel
{

/* Pages of its own at one run of VA -- up to PageTable::MAX_LARGE_MAP_PAGES of
   them, physically scattered, mapped writable: a module's image, or the .ko
   file it is read from */
struct ModulePageRun
{
    ulong base;
    ulong count;
    mm::Page** pages;
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
    ulong offset;       /* from the image's base */
    const char* name;   /* in the image's pages, past the segments */
};

struct LoadedModule
{
    LoadedModule()
        : phase(ModulePhase::Loading)
        , image_size(0)
        , text_end(0)
        , symbols(nullptr)
        , symbol_count(0)
        , imports(0)
        , permanent_by(nullptr)
        , instance(nullptr)
        , exit(nullptr)
    {
        list_entry.init();
        name[0] = '\0';
        image.base = 0;
        image.count = 0;
        image.pages = nullptr;
    }

    stdlib::ListEntry list_entry;
    char name[ModuleTable::NAME_LEN_MAX + 1];
    ModulePhase phase;            /* changed only under ModuleTable::lock_ */
    ModulePageRun image;          /* the segments, then the symbol table */
    ulong image_size;              /* the segments' extent */
    ulong text_end;                /* where the last executable segment ends */
    const ModuleSymbol* symbols;  /* sorted by offset */
    ulong symbol_count;
    ulong imports;                /* kernel functions it binds to */
    const char* permanent_by;      /* what keeps it from being unloaded, if anything */
    void* instance;               /* what its init returned; its exit takes it back */
    void (*exit)(void* instance);
};

namespace
{

const ulong TAG = 'Kmod';

/* kmod::ModuleInfo (src/crates/kmod/src/lib.rs): the header the module! macro
   puts in every module and exports as nos_module_info. Version 2: the init
   is handed the module's parameters, and insmod's output to say why it
   refuses them. */
const u32 INFO_MAGIC = 0x4D534F4E; /* "NOSM" */
const u32 INFO_VERSION = 2;
const ulong INFO_ABI_LEN = 64;
const ulong INFO_NAME_LEN = 32;
const char INFO_SYMBOL[] = "nos_module_info";

struct ModuleInfo
{
    u32 magic;
    u32 version;
    char abi[INFO_ABI_LEN];
    char name[INFO_NAME_LEN];
    /* params: what insmod's line had after the path, params_len bytes of
       it, NUL-terminated; it and out are valid while the init runs */
    void* (*init)(const char* params, ulong params_len, stdlib::Printer* out);
    void (*exit)(void* instance);
};

static_assert(sizeof(ModuleInfo) == 120, "kmod::ModuleInfo layout");
static_assert(INFO_NAME_LEN == ModuleTable::NAME_LEN_MAX + 1, "kmod::NAME_LEN");

const char KERNEL_ABI[] = NOS_MODULE_ABI;
static_assert(sizeof(KERNEL_ABI) - 1 <= INFO_ABI_LEN, "NOS_MODULE_ABI is too long");

/* How much of each digest a refusal for a mismatch quotes */
const ulong ABI_SHOWN = 12;

/* A module's image -- its segments and its symbol table -- and the .ko file
   it comes from are at most the longest run map_large_pages maps: 16 MiB */
const ulong MAX_IMAGE_SIZE = mm::PageTable::MAX_LARGE_MAP_PAGES * consts::PAGE_SIZE;

/* A linked .ko has half a dozen program headers; the segment checks compare
   every pair, so a corrupt count must not make that billions */
const ulong MAX_PROGRAM_HEADERS = 64;

/* Kernel functions that take a callback for good: nothing hands a softirq
   handler back once it is registered. A module that imports one is
   permanent -- rmmod would free code the kernel may still call into -- the
   way a Linux module without an exit is. (A disk's or a NIC's driver
   registers with its layer as a Rust trait object, from inside the kernel
   image: there is no name for a module to import.) */
const char* const PERMANENT_IMPORTS[] = {
    "kernel_softirq_register",
};

/* The section the Makefile adds to every .ko: its functions, a line each,
   "<hex offset> <name>", in address order -- llvm-nm -n -C's, so the Rust
   names come demangled and the kernel needs no demangler */
const char SYMBOL_SECTION[] = ".nos_syms";
const ulong MAX_HEX_DIGITS = 16;

/* insmod and rmmod in a task of their own */
const ulong JOB_OUTPUT_SIZE = 2048;
const ulong JOB_POLL_MS = 10;
const long JOB_RUNNING = 0;
const long JOB_DONE = 1;
const long JOB_ABANDONED = 2;

bool in_file(ulong file_size, ulong offset, ulong len)
{
    return offset <= file_size && len <= file_size - offset;
}

bool aligned8(ulong value)
{
    return (value & (sizeof(u64) - 1)) == 0;
}

/* Up to len characters of src, NUL-terminated, anything unprintable a '?' */
void quote(char* dst, const char* src, ulong len)
{
    ulong i = 0;
    for (; i < len && src[i] != '\0'; i++)
        dst[i] = (src[i] > ' ' && src[i] <= '~') ? src[i] : '?';
    dst[i] = '\0';
}

const char* phase_text(ModulePhase phase)
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

/* Undo alloc_page_run, whatever part of it happened. Unmapping takes the page
   permissions with the PTEs and shoots down every CPU's TLB. */
void free_page_run(ModulePageRun& run)
{
    auto& pt = mm::PageTable::get_instance();

    if (run.base != 0)
        mm::unmap_large_pages(reinterpret_cast<void*>(run.base), run.count);

    for (ulong i = 0; i < run.count; i++)
        pt.free_page(run.pages[i]);

    if (run.pages != nullptr)
        mm::free(run.pages);

    run.base = 0;
    run.count = 0;
    run.pages = nullptr;
}

bool alloc_page_run(ModulePageRun& run, ulong bytes)
{
    auto& pt = mm::PageTable::get_instance();
    const ulong count = stdlib::round_up(bytes, consts::PAGE_SIZE) / consts::PAGE_SIZE;

    run.base = 0;
    run.count = 0;
    run.pages = nullptr;
    if (count == 0 || count > mm::PageTable::MAX_LARGE_MAP_PAGES)
        return false;

    run.pages = static_cast<mm::Page**>(mm::alloc(count * sizeof(mm::Page*), TAG));
    ulong* phys = static_cast<ulong*>(mm::alloc(count * sizeof(ulong), TAG));
    if (run.pages != nullptr && phys != nullptr)
    {
        for (ulong i = 0; i < count; i++)
        {
            mm::Page* page = pt.alloc_page();
            if (page == nullptr)
                break;

            run.pages[i] = page;
            run.count++;
            phys[i] = page->get_phy_address();
        }

        if (run.count == count)
            run.base = reinterpret_cast<ulong>(mm::map_large_pages(count, phys));
    }

    if (phys != nullptr)
        mm::free(phys);

    if (run.base == 0)
    {
        free_page_run(run);
        return false;
    }

    return true;
}

void release_module(LoadedModule* module)
{
    free_page_run(module->image);
    mm::free(module);
}

/* What Load works out about one image, step by step */
struct LoadCtx
{
    LoadCtx(const void* image, ulong size)
        : file(static_cast<const u8*>(image))
        , size(size)
        , header(nullptr)
        , phdrs(nullptr)
        , shdrs(nullptr)
        , syms(nullptr)
        , sym_count(0)
        , strs(nullptr)
        , str_size(0)
        , base(0)
        , image_size(0)
        , run_size(0)
        , text_end(0)
        , imports(0)
        , permanent_by(nullptr)
        , func_text(nullptr)
        , func_text_size(0)
        , func_lines(0)
    {
    }

    const u8* file;
    ulong size;
    const elf::Ehdr* header;
    const elf::Phdr* phdrs;
    const elf::Shdr* shdrs;
    const elf::Sym* syms;    /* .dynsym */
    ulong sym_count;
    const char* strs;        /* .dynstr */
    ulong str_size;
    ulong base;              /* where the image is mapped */
    ulong image_size;         /* the segments' extent, a whole number of pages */
    ulong run_size;           /* all the image's pages, symbol table included */
    ulong text_end;
    ulong imports;
    const char* permanent_by; /* the first of PERMANENT_IMPORTS it imports */
    const char* func_text;    /* .nos_syms, if it has one */
    ulong func_text_size;
    ulong func_lines;
};

stdlib::Error check_header(LoadCtx& ctx, stdlib::Printer& out)
{
    const elf::Ehdr* eh = reinterpret_cast<const elf::Ehdr*>(ctx.file);

    if (!aligned8(reinterpret_cast<ulong>(ctx.file)))
    {
        out.printf("module: image at 0x%p is not 8-byte aligned\n", ctx.file);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    if (ctx.size < sizeof(*eh) ||
        stdlib::mem_cmp(eh->ident, elf::MAGIC, sizeof(elf::MAGIC)) != 0)
    {
        out.printf("module: not an ELF file\n");
        return make_error(stdlib::Error::BAD_MAGIC);
    }

    if (eh->ident[elf::IDENT_CLASS] != elf::CLASS64 ||
        eh->ident[elf::IDENT_DATA] != elf::DATA2_LSB ||
        eh->ident[elf::IDENT_VERSION] != elf::VERSION_CURRENT)
    {
        out.printf("module: not a little-endian ELF64 file\n");
        return make_error(stdlib::Error::BAD_MAGIC);
    }

    if (eh->machine != hal::module_elf_machine())
    {
        out.printf("module: built for ELF machine %lu, this kernel runs on %lu\n",
            (ulong)eh->machine, (ulong)hal::module_elf_machine());
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    if (eh->type != elf::TYPE_DYN)
    {
        out.printf("module: ELF type %lu, not a shared object\n", (ulong)eh->type);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    if (eh->phentsize != sizeof(elf::Phdr) || eh->phnum == 0 ||
        eh->phnum > MAX_PROGRAM_HEADERS || !aligned8(eh->phoff) ||
        !in_file(ctx.size, eh->phoff, (ulong)eh->phnum * sizeof(elf::Phdr)))
    {
        out.printf("module: program headers corrupt\n");
        return make_error(stdlib::Error::HEADER_CORRUPT);
    }

    if (eh->shentsize != sizeof(elf::Shdr) || eh->shnum == 0 || !aligned8(eh->shoff) ||
        !in_file(ctx.size, eh->shoff, (ulong)eh->shnum * sizeof(elf::Shdr)))
    {
        out.printf("module: section headers corrupt\n");
        return make_error(stdlib::Error::HEADER_CORRUPT);
    }

    ctx.header = eh;
    ctx.phdrs = reinterpret_cast<const elf::Phdr*>(ctx.file + eh->phoff);
    ctx.shdrs = reinterpret_cast<const elf::Shdr*>(ctx.file + eh->shoff);
    return make_success();
}

/* The loadable segments: within the file, page-aligned -- page permissions
   are per page, so no page may hold two segments with different ones -- never
   both writable and executable, and clear of each other. Their extent is the
   image's size. */
stdlib::Error check_segments(LoadCtx& ctx, stdlib::Printer& out)
{
    ulong end = 0;

    for (ulong i = 0; i < ctx.header->phnum; i++)
    {
        const elf::Phdr& ph = ctx.phdrs[i];

        if (ph.type == elf::PT_INTERP || ph.type == elf::PT_TLS)
        {
            out.printf("module: has %s segment, which nothing here provides\n",
                (ph.type == elf::PT_TLS) ? "a thread-local" : "an interpreter");
            return make_error(stdlib::Error::NOT_IMPLEMENTED);
        }

        if (ph.type != elf::PT_LOAD)
            continue;

        if ((ph.flags & elf::PF_W) != 0 && (ph.flags & elf::PF_X) != 0)
        {
            out.printf("module: segment %lu is both writable and executable\n", i);
            return make_error(stdlib::Error::INVALID_VALUE);
        }

        if ((ph.vaddr & (consts::PAGE_SIZE - 1)) != 0 || ph.filesz > ph.memsz ||
            !in_file(ctx.size, ph.offset, ph.filesz) ||
            ph.memsz > MAX_IMAGE_SIZE || ph.vaddr > MAX_IMAGE_SIZE - ph.memsz)
        {
            out.printf("module: segment %lu is corrupt, too big or not page-aligned\n", i);
            return make_error(stdlib::Error::HEADER_CORRUPT);
        }

        const ulong seg_end = stdlib::round_up(ph.vaddr + ph.memsz, consts::PAGE_SIZE);
        for (ulong j = 0; j < i; j++)
        {
            const elf::Phdr& other = ctx.phdrs[j];
            if (other.type != elf::PT_LOAD)
                continue;

            const ulong other_end = stdlib::round_up(other.vaddr + other.memsz, consts::PAGE_SIZE);
            if (ph.vaddr < other_end && other.vaddr < seg_end)
            {
                out.printf("module: segments %lu and %lu share a page\n", j, i);
                return make_error(stdlib::Error::OVERLAP);
            }
        }

        if (seg_end > end)
            end = seg_end;
        if ((ph.flags & elf::PF_X) != 0 && ph.vaddr + ph.memsz > ctx.text_end)
            ctx.text_end = ph.vaddr + ph.memsz;
    }

    if (end == 0)
    {
        out.printf("module: nothing to load\n");
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    if (end > MAX_IMAGE_SIZE)
    {
        out.printf("module: %lu KiB image, more than the %lu KiB a module may take\n",
            end / consts::KB, MAX_IMAGE_SIZE / consts::KB);
        return make_error(stdlib::Error::BAD_SIZE);
    }

    ctx.image_size = end;
    return make_success();
}

stdlib::Error find_symbols(LoadCtx& ctx, stdlib::Printer& out)
{
    const elf::Shdr* dynsym = nullptr;
    for (ulong i = 0; i < ctx.header->shnum && dynsym == nullptr; i++)
    {
        if (ctx.shdrs[i].type == elf::SHT_DYNSYM)
            dynsym = &ctx.shdrs[i];
    }

    if (dynsym == nullptr)
    {
        out.printf("module: no dynamic symbol table\n");
        return make_error(stdlib::Error::NOT_FOUND);
    }

    if (dynsym->entsize != sizeof(elf::Sym) || !aligned8(dynsym->offset) ||
        !in_file(ctx.size, dynsym->offset, dynsym->size) ||
        dynsym->link >= ctx.header->shnum)
    {
        out.printf("module: dynamic symbol table corrupt\n");
        return make_error(stdlib::Error::DATA_CORRUPT);
    }

    const elf::Shdr& strtab = ctx.shdrs[dynsym->link];
    if (strtab.type != elf::SHT_STRTAB || strtab.size == 0 ||
        !in_file(ctx.size, strtab.offset, strtab.size) ||
        ctx.file[strtab.offset + strtab.size - 1] != '\0')
    {
        out.printf("module: dynamic string table corrupt\n");
        return make_error(stdlib::Error::DATA_CORRUPT);
    }

    ctx.syms = reinterpret_cast<const elf::Sym*>(ctx.file + dynsym->offset);
    ctx.sym_count = dynsym->size / sizeof(elf::Sym);
    ctx.strs = reinterpret_cast<const char*>(ctx.file + strtab.offset);
    ctx.str_size = strtab.size;
    return make_success();
}

/* The .nos_syms section, measured: build_functions copies and indexes it once
   the image is mapped. A .ko without one -- linked outside the Makefile --
   loads all the same, and its frames go unnamed. */
void find_functions(LoadCtx& ctx)
{
    const elf::Ehdr* eh = ctx.header;
    if (eh->shstrndx == 0 || eh->shstrndx >= eh->shnum)
        return;

    const elf::Shdr& names = ctx.shdrs[eh->shstrndx];
    if (names.type != elf::SHT_STRTAB || names.size == 0 ||
        !in_file(ctx.size, names.offset, names.size) ||
        ctx.file[names.offset + names.size - 1] != '\0')
        return;

    const char* section_names = reinterpret_cast<const char*>(ctx.file + names.offset);
    for (ulong i = 0; i < eh->shnum; i++)
    {
        const elf::Shdr& sh = ctx.shdrs[i];
        if (sh.name >= names.size || stdlib::str_cmp(section_names + sh.name, SYMBOL_SECTION) != 0)
            continue;

        if (sh.size == 0 || !in_file(ctx.size, sh.offset, sh.size))
            return;

        ulong lines = 0;
        for (ulong j = 0; j < sh.size; j++)
        {
            if (ctx.file[sh.offset + j] == '\n')
                lines++;
        }

        const ulong bytes = lines * sizeof(ModuleSymbol) + sh.size + 1;
        if (lines == 0 || bytes > MAX_IMAGE_SIZE - ctx.image_size)
        {
            trace(MODULE_LL, "module: %lu function names left out, %lu bytes of them", lines, (ulong)sh.size);
            return;
        }

        ctx.func_text = reinterpret_cast<const char*>(ctx.file + sh.offset);
        ctx.func_text_size = sh.size;
        ctx.func_lines = lines;
        return;
    }
}

/* NUL-terminated within the table: find_symbols checked its last byte */
const char* sym_name(const LoadCtx& ctx, const elf::Sym& sym)
{
    return (sym.name < ctx.str_size) ? ctx.strs + sym.name : nullptr;
}

bool sym_defined(const elf::Sym& sym)
{
    return sym.shndx != elf::SHN_UNDEF && sym.shndx < elf::SHN_LO_RESERVE;
}

ulong lookup_kernel(const char* name)
{
    for (ulong i = 0; i < nos_module_export_count; i++)
    {
        if (stdlib::str_cmp(nos_module_exports[i].name, name) == 0)
            return nos_module_exports[i].addr;
    }

    return 0;
}

/* Every function the module imports, looked up before anything is mapped: a
   module this kernel cannot satisfy is refused with the list of everything it
   lacks. A weak import may go unresolved; it binds to 0. Importing one of
   PERMANENT_IMPORTS makes the module permanent. */
stdlib::Error check_imports(LoadCtx& ctx, stdlib::Printer& out)
{
    ulong missing = 0;

    for (ulong i = 1; i < ctx.sym_count; i++)
    {
        const elf::Sym& sym = ctx.syms[i];
        if (sym_defined(sym))
            continue;

        const char* name = sym_name(ctx, sym);
        if (name == nullptr || name[0] == '\0')
        {
            out.printf("module: undefined dynamic symbol %lu has no name\n", i);
            return make_error(stdlib::Error::DATA_CORRUPT);
        }

        const ulong addr = lookup_kernel(name);
        if (addr != 0)
        {
            trace(MODULE_LL, "module: %s -> 0x%lX", name, addr);
            ctx.imports++;

            for (ulong j = 0; j < stdlib::array_size(PERMANENT_IMPORTS); j++)
            {
                if (ctx.permanent_by == nullptr && stdlib::str_cmp(name, PERMANENT_IMPORTS[j]) == 0)
                    ctx.permanent_by = PERMANENT_IMPORTS[j];
            }
        }
        else if (elf::sym_bind(sym.info) != elf::STB_WEAK)
        {
            out.printf("module: needs %s, which this kernel does not export\n", name);
            missing++;
        }
    }

    if (missing != 0)
        return make_error(stdlib::Error::NOT_FOUND);

    return make_success();
}

stdlib::Error apply_reloc(const LoadCtx& ctx, const elf::Rela& rela, stdlib::Printer& out)
{
    const u32 type = elf::rela_type(rela.info);
    const hal::ModuleReloc kind = hal::classify_module_reloc(type);

    if (kind == hal::ModuleReloc::None)
        return make_success();

    if (kind == hal::ModuleReloc::Unsupported)
    {
        out.printf("module: relocation type %lu at 0x%lX is not one a module may have\n",
            (ulong)type, (ulong)rela.offset);
        return make_error(stdlib::Error::NOT_IMPLEMENTED);
    }

    if (rela.offset > ctx.image_size - sizeof(ulong))
    {
        out.printf("module: relocation at 0x%lX is outside the image\n", (ulong)rela.offset);
        return make_error(stdlib::Error::DATA_CORRUPT);
    }

    ulong value;
    if (kind == hal::ModuleReloc::Relative)
    {
        value = ctx.base + rela.addend;
    }
    else
    {
        const u32 index = elf::rela_sym(rela.info);
        if (index == 0 || index >= ctx.sym_count)
        {
            out.printf("module: relocation at 0x%lX names symbol %lu of %lu\n",
                (ulong)rela.offset, (ulong)index, ctx.sym_count);
            return make_error(stdlib::Error::DATA_CORRUPT);
        }

        const elf::Sym& sym = ctx.syms[index];
        if (sym_defined(sym))
        {
            if (sym.value >= ctx.image_size)
            {
                out.printf("module: symbol %lu is outside the image\n", (ulong)index);
                return make_error(stdlib::Error::DATA_CORRUPT);
            }
            value = ctx.base + sym.value + rela.addend;
        }
        else
        {
            /* check_imports vouched for the name, and for a miss being weak:
               an unresolved weak import is 0, plus the addend like any other */
            value = lookup_kernel(sym_name(ctx, sym)) + rela.addend;
        }
    }

    /* A pointer in a packed struct need not be aligned */
    stdlib::mem_cpy(reinterpret_cast<void*>(ctx.base + rela.offset), &value, sizeof(value));
    return make_success();
}

stdlib::Error relocate(const LoadCtx& ctx, stdlib::Printer& out)
{
    for (ulong i = 0; i < ctx.header->shnum; i++)
    {
        const elf::Shdr& sh = ctx.shdrs[i];

        if (sh.type == elf::SHT_REL)
        {
            out.printf("module: has REL relocations; only RELA ones are applied\n");
            return make_error(stdlib::Error::NOT_IMPLEMENTED);
        }

        /* .rela.dyn and .rela.plt: what the linker left for the loader */
        if (sh.type != elf::SHT_RELA || (sh.flags & elf::SHF_ALLOC) == 0)
            continue;

        if (sh.entsize != sizeof(elf::Rela) || !aligned8(sh.offset) ||
            !in_file(ctx.size, sh.offset, sh.size))
        {
            out.printf("module: relocation section %lu corrupt\n", i);
            return make_error(stdlib::Error::DATA_CORRUPT);
        }

        const elf::Rela* rela = reinterpret_cast<const elf::Rela*>(ctx.file + sh.offset);
        for (ulong j = 0; j < sh.size / sizeof(elf::Rela); j++)
        {
            stdlib::Error err = apply_reloc(ctx, rela[j], out);
            if (!err.ok())
                return err;
        }
    }

    return make_success();
}

/* One line of .nos_syms, "<hex offset> <name>", its newline already a NUL */
bool parse_function_line(char* line, ulong& offset, const char*& name)
{
    offset = 0;
    ulong digits = 0;
    char* p = line;
    for (; *p != ' ' && *p != '\0'; p++, digits++)
    {
        const u8 nibble = stdlib::hex_char_to_nibble(*p);
        if (nibble > 0xF || digits == MAX_HEX_DIGITS)
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
void build_functions(const LoadCtx& ctx, LoadedModule& module)
{
    if (ctx.func_text == nullptr)
        return;

    ModuleSymbol* index = reinterpret_cast<ModuleSymbol*>(ctx.base + ctx.image_size);
    char* text = reinterpret_cast<char*>(index + ctx.func_lines);
    stdlib::mem_cpy(text, ctx.func_text, ctx.func_text_size);
    text[ctx.func_text_size] = '\0';

    ulong count = 0;
    char* line = text;
    char* const end = text + ctx.func_text_size;
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
        if (!parse_function_line(line, offset, name) || offset >= ctx.image_size ||
            (count != 0 && offset < index[count - 1].offset))
        {
            trace(0, "module: %s: function table bad at line %lu, left out", module.name, count + 1);
            return;
        }

        index[count].offset = offset;
        index[count].name = name;
        count++;
        line = newline + 1;
    }

    module.symbols = index;
    module.symbol_count = count;
}

/* The function offset lies in: the last one starting at or before it, if
   offset is in code at all */
const ModuleSymbol* find_function(const LoadedModule& module, ulong offset)
{
    if (module.symbol_count == 0 || offset >= module.text_end ||
        offset < module.symbols[0].offset)
        return nullptr;

    ulong lo = 0;
    ulong hi = module.symbol_count;
    while (hi - lo > 1)
    {
        const ulong mid = lo + (hi - lo) / 2;
        if (module.symbols[mid].offset <= offset)
            lo = mid;
        else
            hi = mid;
    }

    return &module.symbols[lo];
}

const ModuleInfo* find_info(const LoadCtx& ctx, stdlib::Printer& out)
{
    for (ulong i = 1; i < ctx.sym_count; i++)
    {
        const elf::Sym& sym = ctx.syms[i];
        const char* name = sym_name(ctx, sym);
        if (name == nullptr || stdlib::str_cmp(name, INFO_SYMBOL) != 0)
            continue;

        if (!sym_defined(sym) || sym.size < sizeof(ModuleInfo) || !aligned8(sym.value) ||
            sym.value > ctx.image_size - sizeof(ModuleInfo))
        {
            out.printf("module: %s is not a module header\n", INFO_SYMBOL);
            return nullptr;
        }

        return reinterpret_cast<const ModuleInfo*>(ctx.base + sym.value);
    }

    out.printf("module: no %s -- not a module built with kmod::module!\n", INFO_SYMBOL);
    return nullptr;
}

bool in_executable_segment(const LoadCtx& ctx, ulong addr)
{
    for (ulong i = 0; i < ctx.header->phnum; i++)
    {
        const elf::Phdr& ph = ctx.phdrs[i];
        if (ph.type == elf::PT_LOAD && (ph.flags & elf::PF_X) != 0 &&
            addr >= ctx.base + ph.vaddr && addr < ctx.base + ph.vaddr + ph.memsz)
            return true;
    }

    return false;
}

/* The header, read once the relocations have made its pointers real: the
   kmod version it was built with, the kernel interface it was built against,
   its name, and whether init and exit point at its code */
stdlib::Error check_info(const LoadCtx& ctx, const ModuleInfo& info, LoadedModule& module,
    stdlib::Printer& out)
{
    if (info.magic != INFO_MAGIC)
    {
        out.printf("module: %s is not a module header\n", INFO_SYMBOL);
        return make_error(stdlib::Error::BAD_MAGIC);
    }

    if (info.version != INFO_VERSION)
    {
        out.printf("module: header version %lu, this kernel reads %lu\n",
            (ulong)info.version, (ulong)INFO_VERSION);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    char abi[INFO_ABI_LEN];
    stdlib::mem_set(abi, 0, sizeof(abi));
    stdlib::mem_cpy(abi, KERNEL_ABI, sizeof(KERNEL_ABI) - 1);
    if (stdlib::mem_cmp(abi, info.abi, sizeof(abi)) != 0)
    {
        char theirs[ABI_SHOWN + 1];
        char ours[ABI_SHOWN + 1];
        quote(theirs, info.abi, ABI_SHOWN);
        quote(ours, KERNEL_ABI, ABI_SHOWN);
        out.printf("module: built against another kernel interface (ffi %s, this kernel %s)"
            " -- rebuild it from this tree\n", theirs, ours);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    ulong len = 0;
    for (; len < INFO_NAME_LEN && info.name[len] != '\0'; len++)
    {
        if (info.name[len] <= ' ' || info.name[len] > '~')
            break;
    }

    if (len == 0 || len == INFO_NAME_LEN || info.name[len] != '\0')
    {
        out.printf("module: the module's name is not a printable word of at most %lu characters\n",
            ModuleTable::NAME_LEN_MAX);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    stdlib::mem_cpy(module.name, info.name, len);
    module.name[len] = '\0';

    if (!in_executable_segment(ctx, reinterpret_cast<ulong>(info.init)) ||
        !in_executable_segment(ctx, reinterpret_cast<ulong>(info.exit)))
    {
        out.printf("module: %s: init or exit is not in the module's code\n", module.name);
        return make_error(stdlib::Error::DATA_CORRUPT);
    }

    return make_success();
}

/* The image's pages -- the segments, and after them the function names, if
   the .ko has any -- mapped writable into one run of kernel VA. Protect gives
   each segment its own permissions once it is filled in. */
stdlib::Error map_image(LoadCtx& ctx, LoadedModule& module, stdlib::Printer& out)
{
    ulong bytes = ctx.image_size;
    if (ctx.func_text != nullptr)
        bytes += ctx.func_lines * sizeof(ModuleSymbol) + ctx.func_text_size + 1;

    if (!alloc_page_run(module.image, bytes))
    {
        out.printf("module: no memory for a %lu KiB image\n", bytes / consts::KB);
        return make_error(stdlib::Error::NO_MEMORY);
    }

    ctx.base = module.image.base;
    ctx.run_size = module.image.count * consts::PAGE_SIZE;
    module.image_size = ctx.image_size;
    module.text_end = ctx.text_end;
    stdlib::mem_set(reinterpret_cast<void*>(ctx.base), 0, ctx.run_size);
    return make_success();
}

/* Each segment's permissions from its program header. Only then is the code
   executable -- and nothing is ever writable and executable at once. */
stdlib::Error protect(const LoadCtx& ctx, stdlib::Printer& out)
{
    auto& pt = mm::PageTable::get_instance();

    /* What no segment claims -- the function names -- is left read-only */
    bool ok = pt.set_range_protection(ctx.base, ctx.run_size, false, false);
    for (ulong i = 0; ok && i < ctx.header->phnum; i++)
    {
        const elf::Phdr& ph = ctx.phdrs[i];
        if (ph.type != elf::PT_LOAD || ph.memsz == 0)
            continue;

        const bool writable = (ph.flags & elf::PF_W) != 0;
        const bool executable = (ph.flags & elf::PF_X) != 0;
        trace(MODULE_LL, "module: [0x%lX, 0x%lX) %s%s", (ulong)(ctx.base + ph.vaddr),
            (ulong)(ctx.base + ph.vaddr + ph.memsz), writable ? "rw" : "r", executable ? "x" : "");
        ok = pt.set_range_protection(ctx.base + ph.vaddr,
            stdlib::round_up(ph.memsz, consts::PAGE_SIZE), writable, executable);
    }

    /* set_range_protection flushes only this CPU's TLB */
    CpuTable::get_instance().invalidate_tlb_range(ctx.base, ctx.run_size / consts::PAGE_SIZE);

    if (!ok)
    {
        out.printf("module: cannot set the image's page permissions\n");
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    for (ulong i = 0; i < ctx.header->phnum; i++)
    {
        const elf::Phdr& ph = ctx.phdrs[i];
        if (ph.type == elf::PT_LOAD && (ph.flags & elf::PF_X) != 0)
            hal::sync_instruction_cache(ctx.base + ph.vaddr, ph.memsz);
    }

    return make_success();
}

/* Everything between the checks on the file and the module's init: the
   image mapped, filled and relocated, its function names indexed, its header
   checked, its permissions set. On failure the caller releases the image. */
stdlib::Error prepare(LoadCtx& ctx, LoadedModule& module, const ModuleInfo*& info,
    stdlib::Printer& out)
{
    stdlib::Error err = map_image(ctx, module, out);
    if (!err.ok())
        return err;

    /* The pages are zeroed, which is what a segment's tail past its file
       contents -- .bss -- has to be */
    for (ulong i = 0; i < ctx.header->phnum; i++)
    {
        const elf::Phdr& ph = ctx.phdrs[i];
        if (ph.type == elf::PT_LOAD && ph.filesz != 0)
            stdlib::mem_cpy(reinterpret_cast<void*>(ctx.base + ph.vaddr), ctx.file + ph.offset,
                ph.filesz);
    }

    err = relocate(ctx, out);
    if (!err.ok())
        return err;

    info = find_info(ctx, out);
    if (info == nullptr)
        return make_error(stdlib::Error::NOT_FOUND);

    err = check_info(ctx, *info, module, out);
    if (!err.ok())
        return err;

    build_functions(ctx, module);
    return protect(ctx, out);
}

/* Everything of a load before the module's name is claimed and its init
   runs: the file checked, the image mapped, filled, relocated and
   protected, and its header found and checked. On success the module is the
   caller's, to list or to release; on failure nothing is left of it. */
stdlib::Error stage(const void* image, ulong size, LoadedModule*& module, const ModuleInfo*& info,
    stdlib::Printer& out)
{
    LoadCtx ctx(image, size);

    module = nullptr;
    info = nullptr;

    stdlib::Error err = check_header(ctx, out);
    if (err.ok())
        err = check_segments(ctx, out);
    if (err.ok())
        err = find_symbols(ctx, out);
    if (err.ok())
        err = check_imports(ctx, out);
    if (!err.ok())
        return err;

    find_functions(ctx);

    module = mm::talloc<LoadedModule, TAG>();
    if (module == nullptr)
    {
        out.printf("module: out of memory\n");
        return make_error(stdlib::Error::NO_MEMORY);
    }
    module->imports = ctx.imports;
    module->permanent_by = ctx.permanent_by;

    err = prepare(ctx, *module, info, out);
    if (!err.ok())
    {
        release_module(module);
        module = nullptr;
        info = nullptr;
        return err;
    }

    return make_success();
}

/* An insmod or rmmod handed to a task of its own, and what the task has to
   say about it. Two references: the task's, and the one of whoever started
   it, who may stop waiting before the task is done. */
struct ModuleJob
{
    ModuleJob(bool unload, const char* arg_text, const char* params_text)
        : refs(2)
        , state(JOB_RUNNING)
        , unload(unload)
        , result(stdlib::Error::SUCCESS)
    {
        /* start_job has seen to it that both fit */
        stdlib::mem_cpy(arg, arg_text, stdlib::str_len(arg_text) + 1);
        stdlib::mem_cpy(params, params_text, stdlib::str_len(params_text) + 1);
        output[0] = '\0';
    }

    Atomic refs;
    Atomic state;   /* JOB_RUNNING, then JOB_DONE or JOB_ABANDONED */
    bool unload;
    int result;
    char arg[ModuleTable::PATH_LEN_MAX + 1];
    /* A load's parameters for its init; "" for an unload */
    char params[ModuleTable::PARAMS_MAX + 1];
    char output[JOB_OUTPUT_SIZE];
};

void put_job(ModuleJob* job)
{
    if (job->refs.dec_and_test())
        mm::free(job);
}

void run_job(void* ctx)
{
    ModuleJob* job = static_cast<ModuleJob*>(ctx);
    auto& modules = ModuleTable::get_instance();

    stdlib::BufferPrinter out(job->output, sizeof(job->output));
    stdlib::Error err = job->unload ? modules.unload(job->arg, out) : modules.load_file(job->arg, job->params, out);
    job->result = err.get_code();

    /* Handed over -- unless whoever started this has stopped waiting, and then
       the kernel log is where it can still be read */
    if (job->state.cmpxchg(JOB_DONE, JOB_RUNNING) != JOB_RUNNING)
    {
        trace(0, "module: %s %s, done in the background, error %lu: %s",
            job->unload ? "rmmod" : "insmod", job->arg, (ulong)job->result, job->output);
    }

    put_job(job);
}

}

ModuleTable::ModuleTable()
    : lock_(false)
{
    list_.init();
}

ModuleTable::~ModuleTable()
{
}

LoadedModule* ModuleTable::find_locked(const char* name)
{
    for (stdlib::ListEntry* entry = list_.flink; entry != &list_; entry = entry->flink)
    {
        LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, list_entry);
        if (stdlib::str_cmp(module->name, name) == 0)
            return module;
    }

    return nullptr;
}

void ModuleTable::remove(LoadedModule* module)
{
    const ulong flags = lock_.lock_irq_save();
    module->list_entry.remove_init();
    lock_.unlock_irq_restore(flags);
}

stdlib::Error ModuleTable::load(const void* image, ulong size, const char* params, stdlib::Printer& out)
{
    const ulong params_len = stdlib::str_len(params);
    if (params_len > PARAMS_MAX)
    {
        out.printf("module: the parameters are longer than %lu\n", PARAMS_MAX);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    LoadedModule* module;
    const ModuleInfo* info;
    stdlib::Error err = stage(image, size, module, info, out);
    if (!err.ok())
        return err;

    /* On the list while its init runs -- a backtrace from inside it can name
       it -- unless the name is taken, whatever that module is doing */
    bool taken;
    ModulePhase phase = ModulePhase::Live;
    {
        const ulong flags = lock_.lock_irq_save();
        LoadedModule* other = find_locked(module->name);
        taken = (other != nullptr);
        if (taken)
            phase = other->phase;
        else
            list_.insert_tail(&module->list_entry);
        lock_.unlock_irq_restore(flags);
    }

    if (taken)
    {
        out.printf("module: %s is %s\n", module->name,
            (phase == ModulePhase::Live) ? "already loaded" : phase_text(phase));
        release_module(module);
        return make_error(stdlib::Error::ALREADY_EXISTS);
    }

    /* Once it is Live another task may unload it at any moment: what is
       said about it here is said from copies */
    char name[NAME_LEN_MAX + 1];
    stdlib::mem_cpy(name, module->name, sizeof(name));
    const ulong base = module->image.base;
    const ulong kib = module->image_size / consts::KB;
    const ulong imports = module->imports;
    const ulong functions = module->symbol_count;

    module->exit = info->exit;
    module->instance = info->init(params, params_len, &out);
    const bool live = (module->instance != nullptr);

    {
        const ulong flags = lock_.lock_irq_save();
        if (live)
            module->phase = ModulePhase::Live;
        else
            module->list_entry.remove_init();
        lock_.unlock_irq_restore(flags);
    }

    if (!live)
    {
        out.printf("module: %s: init failed\n", name);
        release_module(module);
        return make_error(stdlib::Error::UNSUCCESSFUL);
    }

    trace(0, "module: %s loaded at 0x%lX, %lu KiB, %lu kernel imports, %lu functions named",
        name, base, kib, imports, functions);
    out.printf("module: %s loaded at 0x%lX\n", name, base);
    return make_success();
}

stdlib::Error ModuleTable::load_file(const char* path, const char* params, stdlib::Printer& out)
{
    const ulong path_len = stdlib::str_len(path);
    const long got = kernel_file_size(path, path_len);
    if (got < 0)
    {
        out.printf("module: cannot open %s\n", path);
        return make_error(stdlib::Error::NOT_FOUND);
    }

    const ulong size = (ulong)got;
    if (size == 0 || size > MAX_IMAGE_SIZE)
    {
        out.printf("module: %s is %lu bytes; a module file is 1 to %lu\n", path, size, MAX_IMAGE_SIZE);
        return make_error(stdlib::Error::BAD_SIZE);
    }

    /* A run of pages, page-aligned: what Load wants, and as big as a module
       may be, where the heap stops at 512 KiB */
    ModulePageRun buf;
    if (!alloc_page_run(buf, size))
    {
        out.printf("module: no memory to read %s\n", path);
        return make_error(stdlib::Error::NO_MEMORY);
    }

    u8* data = reinterpret_cast<u8*>(buf.base);
    const long done = kernel_file_read(path, path_len, data, size);

    stdlib::Error err;
    if (done == (long)size)
    {
        err = load(data, size, params, out);
    }
    else
    {
        out.printf("module: cannot read %s\n", path);
        err = make_error(stdlib::Error::IO);
    }

    free_page_run(buf);
    return err;
}

stdlib::Error ModuleTable::unload(const char* name, stdlib::Printer& out)
{
    LoadedModule* module;
    ModulePhase phase = ModulePhase::Live;
    const char* permanent_by = nullptr;
    {
        const ulong flags = lock_.lock_irq_save();
        module = find_locked(name);
        if (module != nullptr)
        {
            phase = module->phase;
            permanent_by = module->permanent_by;
            if (phase == ModulePhase::Live && permanent_by == nullptr)
                module->phase = ModulePhase::Unloading;
        }
        lock_.unlock_irq_restore(flags);
    }

    if (module == nullptr)
    {
        out.printf("module: %s is not loaded\n", name);
        return make_error(stdlib::Error::NOT_FOUND);
    }

    if (phase != ModulePhase::Live)
    {
        out.printf("module: %s is %s already\n", name, phase_text(phase));
        return make_error(stdlib::Error::INVALID_STATE);
    }

    if (permanent_by != nullptr)
    {
        out.printf("module: %s is permanent: it imports %s, which nothing undoes\n",
            name, permanent_by);
        return make_error(stdlib::Error::INVALID_STATE);
    }

    /* The module's state goes, and everything the module holds with it: exit
       returns once the commands it registered are gone and no call into it is
       still running (kernel_cmd_unregister waits those out). Only then may its
       code go. Nothing is held meanwhile: this may take a while. */
    module->exit(module->instance);
    remove(module);
    release_module(module);

    trace(0, "module: %s unloaded", name);
    out.printf("module: %s unloaded\n", name);
    return make_success();
}

void ModuleTable::unload_all()
{
    for (;;)
    {
        /* Newest first: the reverse of the order they came in */
        LoadedModule* module = nullptr;
        {
            const ulong flags = lock_.lock_irq_save();
            for (stdlib::ListEntry* entry = list_.blink; entry != &list_; entry = entry->blink)
            {
                LoadedModule* candidate = CONTAINING_RECORD(entry, LoadedModule, list_entry);
                if (candidate->phase == ModulePhase::Live && candidate->permanent_by == nullptr)
                {
                    candidate->phase = ModulePhase::Unloading;
                    module = candidate;
                    break;
                }
            }
            lock_.unlock_irq_restore(flags);
        }

        if (module == nullptr)
            break;

        char name[NAME_LEN_MAX + 1];
        stdlib::mem_cpy(name, module->name, sizeof(name));
        module->exit(module->instance);
        remove(module);
        release_module(module);
        trace(0, "module: %s unloaded", name);
    }

    ulong left = 0;
    {
        const ulong flags = lock_.lock_irq_save();
        for (stdlib::ListEntry* entry = list_.flink; entry != &list_; entry = entry->flink)
            left++;
        lock_.unlock_irq_restore(flags);
    }

    if (left != 0)
        trace(0, "module: %lu left loaded: permanent, or in another task's hands", left);
}

/* Whether c separates insmod's words */
static bool is_blank(char c)
{
    return c == ' ' || c == '\t';
}

ModuleTable::JobOutcome ModuleTable::start_load(const char* args, stdlib::Printer& out)
{
    /* The path is the first word, the parameters everything after it but
       the blanks between: handed to the module's init as they are */
    ulong path_len = 0;
    while (args[path_len] != '\0' && !is_blank(args[path_len]))
        path_len++;
    const char* params = args + path_len;
    while (is_blank(*params))
        params++;

    if (path_len > PATH_LEN_MAX)
    {
        out.printf("insmod: the path is longer than %lu\n", PATH_LEN_MAX);
        return JobOutcome::NotStarted;
    }
    if (stdlib::str_len(params) > PARAMS_MAX)
    {
        out.printf("insmod: the parameters are longer than %lu\n", PARAMS_MAX);
        return JobOutcome::NotStarted;
    }

    char path[PATH_LEN_MAX + 1];
    stdlib::mem_cpy(path, args, path_len);
    path[path_len] = '\0';
    return start_job(false, path, params, out);
}

ModuleTable::JobOutcome ModuleTable::start_unload(const char* name, stdlib::Printer& out)
{
    return start_job(true, name, "", out);
}

ModuleTable::JobOutcome ModuleTable::start_job(bool unload, const char* arg, const char* params,
    stdlib::Printer& out)
{
    const char* what = unload ? "rmmod" : "insmod";
    if (stdlib::str_len(arg) > PATH_LEN_MAX || stdlib::str_len(params) > PARAMS_MAX)
    {
        out.printf("%s: %s is too long\n", what, arg);
        return JobOutcome::NotStarted;
    }

    ModuleJob* job = mm::talloc<ModuleJob, TAG>(unload, arg, params);
    Task* task = (job != nullptr) ? mm::talloc<Task, TAG>(what) : nullptr;
    if (task == nullptr || !task->start(run_job, job))
    {
        /* Never ran: no one else holds the job */
        if (task != nullptr)
            task->put();
        if (job != nullptr)
            mm::free(job);
        out.printf("%s: cannot start a task for it\n", what);
        return JobOutcome::NotStarted;
    }

    /* The run queue keeps a reference of its own until the task has exited */
    task->put();

    for (ulong waited = 0; waited < SHELL_WAIT_MS && job->state.get() == JOB_RUNNING;
         waited += JOB_POLL_MS)
        sleep(JOB_POLL_MS * consts::NANO_SECS_IN_MS);

    JobOutcome outcome = JobOutcome::Done;
    if (job->state.cmpxchg(JOB_ABANDONED, JOB_RUNNING) == JOB_RUNNING)
    {
        out.printf("%s: %s is taking its time and goes on in the background --"
            " lsmod shows where it is, the kernel log will say how it ended\n", what, arg);
        outcome = JobOutcome::Pending;
    }
    else
    {
        out.print_string(job->output);
        if (job->result != stdlib::Error::SUCCESS)
        {
            out.printf("%s: %s failed, error %lu\n", what, arg, (ulong)job->result);
            outcome = JobOutcome::Failed;
        }
    }

    put_job(job);
    return outcome;
}

bool ModuleTable::is_loaded(const char* name)
{
    const ulong flags = lock_.lock_irq_save();
    const bool loaded = (find_locked(name) != nullptr);
    lock_.unlock_irq_restore(flags);
    return loaded;
}

ulong ModuleTable::snapshot(ModuleSnapshot* out, ulong max)
{
    if (out == nullptr)
        return 0;

    ulong n = 0;
    const ulong flags = lock_.lock_irq_save();
    for (stdlib::ListEntry* entry = list_.flink; entry != &list_ && n < max; entry = entry->flink)
    {
        const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, list_entry);
        stdlib::mem_cpy(out[n].name, module->name, sizeof(out[n].name));
        out[n].name[sizeof(out[n].name) - 1] = '\0';
        out[n].size = module->image.count * consts::PAGE_SIZE;
        out[n].imports = module->imports;
        out[n].phase = SNAPSHOT_LIVE;
        if (module->phase == ModulePhase::Loading)
            out[n].phase = SNAPSHOT_LOADING;
        else if (module->phase == ModulePhase::Unloading)
            out[n].phase = SNAPSHOT_UNLOADING;
        out[n].permanent = (module->permanent_by != nullptr) ? 1 : 0;
        n++;
    }
    lock_.unlock_irq_restore(flags);
    return n;
}

void ModuleTable::dump(stdlib::Printer& out)
{
    /* A module at a time, copied under the lock and printed without it: a
       console line can take a while */
    ulong shown = 0;
    for (;; shown++)
    {
        char name[NAME_LEN_MAX + 1];
        ulong base = 0;
        ulong kib = 0;
        ulong imports = 0;
        ModulePhase phase = ModulePhase::Live;
        bool permanent = false;
        bool found = false;

        {
            const ulong flags = lock_.lock_irq_save();
            ulong i = 0;
            for (stdlib::ListEntry* entry = list_.flink; entry != &list_; entry = entry->flink, i++)
            {
                if (i != shown)
                    continue;

                const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, list_entry);
                stdlib::mem_cpy(name, module->name, sizeof(name));
                base = module->image.base;
                kib = (module->image.count * consts::PAGE_SIZE) / consts::KB;
                imports = module->imports;
                phase = module->phase;
                permanent = (module->permanent_by != nullptr);
                found = true;
                break;
            }
            lock_.unlock_irq_restore(flags);
        }

        if (!found)
            break;

        out.printf("%s  %lu KiB at 0x%lX, %lu kernel imports%s%s\n", name, kib, base, imports,
            permanent ? ", permanent" : "",
            (phase == ModulePhase::Loading) ? ", loading" :
            (phase == ModulePhase::Unloading) ? ", unloading" : "");
    }

    if (shown == 0)
        out.printf("no modules loaded\n");
}

bool ModuleTable::describe(ulong addr, bool return_address, char* buf, ulong size)
{
    if (return_address && addr == 0)
        return false;
    /* What is looked up: the call's last byte, for a return address */
    const ulong at = return_address ? addr - 1 : addr;

    bool acquired = false;
    const ulong flags = lock_.try_lock_irq_save(acquired);
    if (!acquired)
    {
        preempt_irq_restore(flags);
        return false;
    }

    bool found = false;
    for (stdlib::ListEntry* entry = list_.flink; entry != &list_; entry = entry->flink)
    {
        const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, list_entry);
        if (at < module->image.base || at >= module->image.base + module->image_size)
            continue;

        const ulong offset = addr - module->image.base;
        const ModuleSymbol* function = find_function(*module, at - module->image.base);
        if (function != nullptr)
            stdlib::sn_printf(buf, size, "%s+0x%lX [%s]", function->name,
                offset - function->offset, module->name);
        else
            stdlib::sn_printf(buf, size, "[%s]+0x%lX", module->name, offset);
        found = true;
        break;
    }

    lock_.unlock_irq_restore(flags);
    return found;
}

bool ModuleTable::describe_all(char* buf, ulong size)
{
    if (size == 0)
        return false;

    bool acquired = false;
    const ulong flags = lock_.try_lock_irq_save(acquired);
    if (!acquired)
    {
        preempt_irq_restore(flags);
        return false;
    }

    buf[0] = '\0';
    ulong pos = 0;
    bool any = false;
    for (stdlib::ListEntry* entry = list_.flink; entry != &list_ && pos + 1 < size;
         entry = entry->flink)
    {
        const LoadedModule* module = CONTAINING_RECORD(entry, LoadedModule, list_entry);
        stdlib::sn_printf(buf + pos, size - pos, "%s%s 0x%lX+0x%lX", any ? ", " : "",
            module->name, module->image.base, module->image_size);
        buf[size - 1] = '\0';
        pos += stdlib::str_len(buf + pos);
        any = true;
    }

    lock_.unlock_irq_restore(flags);
    return any;
}

}
