// The ACPI tables (drivers/acpi.cpp): what an x86 machine's firmware -- or a
// cloud's hypervisor -- says the machine is: its CPUs, its interrupt
// controllers and how the legacy IRQs reach them, the HPET, the reset
// register, a watchdog of the firmware's own. All of it is read once, at
// boot, through the TmpMap window, before a headless machine has a console,
// and a failure there is a boot that stops. Physical memory is the input's
// -- sparse, zeros where nothing was put -- with an RSDP in the Multiboot
// tag's copy, the EBDA or the BIOS area among decoys, an RSDT or an XSDT,
// and tables below and above 4 GiB, across page ends, over each other: a
// MADT of every entry kind, a FADT, an HPET table and a WDAT of every
// length, the SSDTs and the rest a firmware lists, and seconds of the kinds
// read among them; now and then damaged anywhere. The window is emulated to
// the slot: a mapping is a copy of the pages it maps between poisoned ones,
// each page is let go of by itself, and the window refuses a mapping where
// the input says. The machine is held to a reader of it written from
// acpi.h -- the RSDP the search finds, the root, the first table of each
// kind that can be mapped, the CPUs, the IO-APIC the driver can drive, the
// overrides, the FADT's, HPET's and WDAT's fields, and whether it boots at
// all -- and any machine, the window cut short or not, to this: nothing read
// past a mapping, nothing mapped past the physical address space or longer
// than a table can be, no more of the window held at once than acpi.h
// bounds, and nothing held once the parse is done but the local APIC's and
// the IO-APIC's pages -- and not those if it failed.
#include "host.h"

#define private public
#include <drivers/acpi.h>
#include <kernel/cpu.h>
#include <mm/page_table.h>
#undef private

#include <arch/x86_64/grub.h>
#include <mm/memory_map.h>

#include "fuzz.h"
#include "kernel.h"

#include <sanitizer/asan_interface.h>

using namespace Kernel;
using Kernel::Mm::PageTable;

namespace
{

typedef unsigned long long ull;

const u64 PageBytes = Const::PageSize;
const u64 MaxPhys = Mm::MemoryMap::MaxPhysAddr;
const u64 MaxTableLength = Acpi::MaxTableLength;
const size_t WindowSlots = PageTable::TmpMapSharedCount;

/* What of the window the parse may hold at once (acpi.h): the root and a
   table of each kind, each at most MaxTableLength long and so across at
   most that many pages and one more; then a header looked at, across two,
   or the local APIC's page and two IO-APICs', the one kept and the one
   taking its place. */
const size_t MaxTablePages = Acpi::MaxTableLength / Const::PageSize + 1;
const size_t MaxHeld = (Acpi::MaxTables + 1) * MaxTablePages + 3;

/* ---- physical memory: sparse, zeros where nothing was put ---- */

std::map<u64, std::vector<uint8_t>> Phys;

void Poke(u64 addr, const uint8_t* bytes, size_t len)
{
    while (len != 0)
    {
        u64 page = addr & ~(PageBytes - 1);
        size_t at = addr - page;
        size_t chunk = std::min<size_t>(len, PageBytes - at);
        auto& p = Phys[page];
        if (p.empty())
            p.assign(PageBytes, 0);
        memcpy(p.data() + at, bytes, chunk);
        addr += chunk;
        bytes += chunk;
        len -= chunk;
    }
}

void Poke(u64 addr, const std::vector<uint8_t>& bytes)
{
    Poke(addr, bytes.data(), bytes.size());
}

uint8_t Peek(u64 addr)
{
    auto it = Phys.find(addr & ~(PageBytes - 1));
    return (it == Phys.end()) ? 0 : it->second[addr & (PageBytes - 1)];
}

/* A little-endian word of physical memory */
u64 Read(u64 addr, int bytes)
{
    u64 x = 0;
    for (int i = bytes - 1; i >= 0; i--)
        x = (x << 8) | Peek(addr + i);
    return x;
}

bool Is(u64 addr, const char* signature)
{
    for (size_t i = 0; i < strlen(signature); i++)
    {
        if (Peek(addr + i) != static_cast<uint8_t>(signature[i]))
            return false;
    }
    return true;
}

/* ---- the TmpMap window ---- */

struct Mapping
{
    u64 PhysPage;
    size_t Pages;
    uint8_t* Block;
    std::vector<bool> Held;
};

/* By the VA of the first page mapped */
std::map<ulong, Mapping> Maps;
size_t Held;
/* The slots the window has for the parse, and the mapping it refuses (-1:
   none): the input's */
size_t Window = WindowSlots;
long FailAt = -1;
long MapCalls;
bool Refused;

void Release(std::map<ulong, Mapping>::iterator it)
{
    ASAN_UNPOISON_MEMORY_REGION(it->second.Block, (it->second.Pages + 2) * PageBytes);
    free(it->second.Block);
    Maps.erase(it);
}

/* The mapping a VA is in, and which of its pages */
bool Find(ulong va, std::map<ulong, Mapping>::iterator& it, size_t& page)
{
    it = Maps.upper_bound(va);
    if (it == Maps.begin())
        return false;
    --it;
    if (va >= it->first + it->second.Pages * PageBytes)
        return false;
    page = (va - it->first) / PageBytes;
    return true;
}

/* The physical address a VA of the window maps, held */
u64 PhysOf(const void* va)
{
    std::map<ulong, Mapping>::iterator it;
    size_t page = 0;
    ulong v = reinterpret_cast<ulong>(va);
    INVARIANT(Find(v, it, page) && it->second.Held[page], "0x%lx is no page of the window held", v);
    return it->second.PhysPage + page * PageBytes + (v & (PageBytes - 1));
}

/* TmpMapPage, TmpMapAddress and TmpMapRange: len bytes at phys, in the
   window as the kernel's are, one slot a page. The pages are whole for the
   first two, whose callers read a page -- the RSDP's search, the BIOS data
   area -- or hand it to a driver, and for TmpMapRange only the range: what
   ACPI maps that way is a table, and all a parser may read of it. */
ulong Map(u64 phys, u64 len, bool exact)
{
    INVARIANT(len <= MaxTableLength && phys <= MaxPhys && len <= MaxPhys - phys,
        "a mapping of 0x%llx bytes at 0x%llx: longer than a table, or past the physical address space",
        (ull)len, (ull)phys);
    u64 offset = phys & (PageBytes - 1);
    size_t pages = (offset + len + PageBytes - 1) / PageBytes;
    if (pages == 0)
        pages = 1;
    if (MapCalls++ == FailAt || Held + pages > Window)
    {
        Refused = true;
        return 0;
    }

    void* block = nullptr;
    INVARIANT(posix_memalign(&block, PageBytes, (pages + 2) * PageBytes) == 0, "no host memory");
    uint8_t* b = static_cast<uint8_t*>(block);
    u64 physPage = phys - offset;
    for (size_t p = 0; p < pages; p++)
    {
        auto it = Phys.find(physPage + p * PageBytes);
        if (it == Phys.end())
            memset(b + (p + 1) * PageBytes, 0, PageBytes);
        else
            memcpy(b + (p + 1) * PageBytes, it->second.data(), PageBytes);
    }
    ASAN_POISON_MEMORY_REGION(b, PageBytes);
    ASAN_POISON_MEMORY_REGION(b + (pages + 1) * PageBytes, PageBytes);
    if (exact)
    {
        ASAN_POISON_MEMORY_REGION(b + PageBytes, offset);
        ASAN_POISON_MEMORY_REGION(b + PageBytes + offset + len, pages * PageBytes - offset - len);
    }
    ulong va = reinterpret_cast<ulong>(b + PageBytes);
    Maps[va] = {physPage, pages, b, std::vector<bool>(pages, true)};
    Held += pages;
    INVARIANT(Held <= MaxHeld, "the parse holds %zu slots of the window at once, past the %zu acpi.h bounds",
        Held, MaxHeld);
    return va + offset;
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

ulong PageTable::TmpMapPage(ulong phyAddr)
{
    INVARIANT((phyAddr & (PageBytes - 1)) == 0, "TmpMapPage(0x%lx), not a page", phyAddr);
    return Map(phyAddr, 1, false);
}

ulong PageTable::TmpMapAddress(ulong phyAddr)
{
    return Map(phyAddr, 1, false);
}

ulong PageTable::TmpMapRange(ulong phyAddr, size_t len)
{
    return Map(phyAddr, len, true);
}

ulong PageTable::TmpUnmapPage(ulong virtAddr)
{
    std::map<ulong, Mapping>::iterator it;
    size_t page = 0;
    INVARIANT((virtAddr & (PageBytes - 1)) == 0 && Find(virtAddr, it, page) && it->second.Held[page],
        "TmpUnmapPage(0x%lx), which is no slot held", virtAddr);
    it->second.Held[page] = false;
    Held--;
    u64 phys = it->second.PhysPage + page * PageBytes;
    ASAN_POISON_MEMORY_REGION(reinterpret_cast<void*>(virtAddr), PageBytes);
    if (std::find(it->second.Held.begin(), it->second.Held.end(), true) == it->second.Held.end())
        Release(it);
    return phys;
}

}

/* The CPUs the MADT names, as the CPU table takes them */
std::vector<ulong> Inserted;

bool CpuTable::InsertCpu(ulong index)
{
    if (index >= MaxCpus || std::find(Inserted.begin(), Inserted.end(), index) != Inserted.end())
        return false;
    Inserted.push_back(index);
    return true;
}

namespace Grub
{

/* What the Multiboot ACPI tag carried, if the input gives one */
std::vector<uint8_t> Rsdp;

const void* GetAcpiRsdp(size_t& size)
{
    size = Rsdp.size();
    return size ? Rsdp.data() : nullptr;
}

}
}

namespace
{

/* ---- the machine ---- */

void Put(std::vector<uint8_t>& v, size_t at, u64 x, int bytes)
{
    if (v.size() < at + bytes)
        v.resize(at + bytes, 0);
    for (int i = 0; i < bytes; i++)
        v[at + i] = static_cast<uint8_t>(x >> (8 * i));
}

/* An SDT: its header, then body */
std::vector<uint8_t> Sdt(const char* signature, const std::vector<uint8_t>& body)
{
    std::vector<uint8_t> t(sizeof(Acpi::ACPISDTHeader), 0);
    memcpy(t.data(), signature, 4);
    Put(t, 4, t.size() + body.size(), 4);
    t[8] = 1;
    memcpy(t.data() + 10, "NOSFUZ", 6);
    t.insert(t.end(), body.begin(), body.end());
    return t;
}

void Checksum(std::vector<uint8_t>& v, size_t len, size_t at)
{
    uint8_t sum = 0;
    v[at] = 0;
    for (size_t i = 0; i < len; i++)
        sum = static_cast<uint8_t>(sum + v[i]);
    v[at] = static_cast<uint8_t>(-sum);
}

std::vector<uint8_t> MakeRsdp(uint8_t revision, u32 rsdt, u64 xsdt)
{
    std::vector<uint8_t> r(revision >= 2 ? 36 : 20, 0);
    memcpy(r.data(), "RSD PTR ", 8);
    memcpy(r.data() + 9, "NOSFUZ", 6);
    r[15] = revision;
    Put(r, 16, rsdt, 4);
    if (revision >= 2)
    {
        Put(r, 20, 36, 4);
        Put(r, 24, xsdt, 8);
    }
    Checksum(r, 20, 8);
    if (revision >= 2)
        Checksum(r, 36, 32);
    return r;
}

void Shuffle(Fuzz::Input& in, std::vector<std::vector<uint8_t>>& v)
{
    for (size_t i = v.size(); i > 1; i--)
        std::swap(v[i - 1], v[in.Below(i)]);
}

std::vector<uint8_t> MadtBody(Fuzz::Input& in)
{
    std::vector<std::vector<uint8_t>> entries;

    /* The CPUs, some disabled, now and then with ids past MaxCpus or twice */
    size_t cpus = in.Chance(16) ? in.Below(100) : in.Range(1, 16);
    for (size_t i = 0; i < cpus; i++)
    {
        std::vector<uint8_t> e = {0, 8, static_cast<uint8_t>(i), static_cast<uint8_t>(in.Chance(16) ? in.U8() : i)};
        Put(e, 4, in.Chance(24) ? in.U32() & ~1u : 1, 4);
        entries.push_back(e);
    }

    /* The IO-APICs: one, or AMD's two, the one at GSI 0 first -- or not,
       or none, or one whose registers run across its page's end */
    size_t ioApics = in.Chance(16) ? in.Below(5) : in.Range(1, 2);
    const bool reversed = in.Chance(48);
    for (size_t i = 0; i < ioApics; i++)
    {
        u32 addr = 0xFEC00000 + 0x1000 * static_cast<u32>(i);
        if (in.Chance(16))
            addr += static_cast<u32>(in.Bool() ? in.Below(0x1000) : 0xFC0 + in.Below(0x40));
        u32 base = 24 * static_cast<u32>(reversed ? ioApics - 1 - i : i);
        if (in.Chance(32))
            base = in.Chance(128) ? 0 : in.Value32();
        std::vector<uint8_t> e = {1, 12, static_cast<uint8_t>(cpus + i), 0};
        Put(e, 4, addr, 4);
        Put(e, 8, base, 4);
        entries.push_back(e);
    }

    /* The overrides a PC has -- the PIT on pin 2, the SCI level and low --
       and others: IRQs twice, GSIs past what the interrupt layer takes */
    if (!in.Chance(32))
        entries.push_back({2, 10, 0, 0, 2, 0, 0, 0, 0, 0});
    if (!in.Chance(32))
        entries.push_back({2, 10, 0, 9, 9, 0, 0, 0, 0xD, 0});
    for (size_t n = in.Chance(16) ? in.Below(80) : in.Below(3); n > 0; n--)
    {
        std::vector<uint8_t> e = {2, 10, 0, static_cast<uint8_t>(in.Chance(32) ? in.U8() : in.Below(24))};
        Put(e, 4, in.Chance(32) ? in.Value32() : in.Below(64), 4);
        Put(e, 8, in.U16(), 2);
        entries.push_back(e);
    }

    /* What this kernel does not read: NMIs, the local APIC's 64-bit
       address, x2APICs, and kinds past those */
    static const uint8_t Others[][2] = {{4, 6}, {5, 12}, {9, 16}, {10, 12}, {11, 80}, {13, 16}, {0x7F, 2}};
    for (size_t n = in.Below(4); n > 0; n--)
    {
        const auto& o = Others[in.Below(sizeof(Others) / sizeof(Others[0]))];
        std::vector<uint8_t> e = Fuzz::Noise(in.U32(), o[1]);
        e[0] = o[0];
        e[1] = o[1];
        entries.push_back(e);
    }

    if (in.Chance(32))
    {
        Shuffle(in, entries);
        Fuzz::Reached("a MADT in any order");
    }

    /* An entry's length that is not its kind's: short of it, 0, past it */
    if (!entries.empty() && in.Chance(16))
    {
        auto& e = entries[in.Below(entries.size())];
        e[1] = static_cast<uint8_t>(in.Bool() ? in.Below(e.size()) : e.size() + in.Range(1, 16));
        Fuzz::Reached("a MADT entry of the wrong length");
    }

    std::vector<uint8_t> body;
    u32 lapic = 0xFEE00000;
    if (in.Chance(16))
        lapic = in.Bool() ? in.U32() : lapic + static_cast<u32>(in.Range(1, 0xFFF));
    Put(body, 0, lapic, 4);
    Put(body, 4, 1, 4);
    for (auto& e : entries)
        body.insert(body.end(), e.begin(), e.end());
    if (in.Chance(16))
        body.resize(in.Below(body.size() + 1));
    return body;
}

std::vector<uint8_t> FadtBody(Fuzz::Input& in)
{
    /* ACPI 1.0's, 2.0's, 5.0's and 6's, less the header; and each side of
       where a field the kernel reads ends */
    static const size_t Lengths[] = {80, 208, 232, 240, 80, 208, 232, 240, 31, 32, 72, 73, 92, 93};
    size_t len = in.Chance(32) ? in.Below(300) : in.Pick(Lengths);
    std::vector<uint8_t> body = Fuzz::Noise(in.U32(), len);
    if (len >= 32 && !in.Chance(64))
        Put(body, 28, 0x604, 4);
    if (len > 80 && !in.Chance(64))
    {
        /* RESET_REG_SUP, and the reset register in I/O space: 0xCF9, the
           value 6 -- as much of it as the table holds */
        body[77] = static_cast<uint8_t>(body[77] | 0x04);
        body[80] = 1;
        for (size_t i = 0; i < 8 && 84 + i < len; i++)
            body[84 + i] = static_cast<uint8_t>(0xCF9ULL >> (8 * i));
        if (len > 92)
            body[92] = 6;
    }
    if (len >= 73 && in.Bool())
        body[72] = 0x32;
    return body;
}

std::vector<uint8_t> HpetBody(Fuzz::Input& in)
{
    size_t len = in.Chance(32) ? in.Below(24) : in.Chance(32) ? 19 : 20;
    std::vector<uint8_t> body = Fuzz::Noise(in.U32(), len);
    if (len >= 16 && !in.Chance(48))
    {
        body[4] = 0;
        Put(body, 8, 0xFED00000, 8);
    }
    return body;
}

std::vector<uint8_t> WdatBody(Fuzz::Input& in)
{
    std::vector<uint8_t> body = Fuzz::Noise(in.U32(), 32);
    size_t entries = in.Below(8);
    Put(body, 28, in.Chance(32) ? in.U32() : entries, 4);
    for (size_t i = 0; i < entries; i++)
    {
        std::vector<uint8_t> entry = Fuzz::Noise(in.U32(), 24);
        if (in.Chance(32))
        {
            /* An instruction on the RTC's ports */
            entry[4] = 1;
            Put(entry, 8, 0x70, 8);
        }
        body.insert(body.end(), entry.begin(), entry.end());
    }
    if (in.Chance(32))
        body.resize(in.Chance(128) ? in.Below(body.size() + 1) : body.size() - in.Range(1, std::min<size_t>(body.size(), 24)));
    return body;
}

/* Where a table goes: below 4 GiB, or for an XSDT's anywhere; now and then
   with its header across a page's end, or over another table */
struct Placer
{
    Fuzz::Input& In;
    bool High;
    u64 Next;
    std::vector<u64> Placed;

    u64 Place(size_t len)
    {
        u64 at;
        if (!Placed.empty() && In.Chance(8))
        {
            at = Placed[In.Below(Placed.size())] + 4 * In.Below(16);
            Fuzz::Reached("a table over another");
        }
        else if (High && In.Chance(64))
        {
            at = 0x100000000ULL + (In.Below(1ULL << 20) << 12) + 4 * In.Below(1024);
            Fuzz::Reached("a table above 4 GiB");
        }
        else
        {
            at = Next + 4 * In.Below(64);
            if (In.Chance(32))
                at = ((at + PageBytes) & ~(PageBytes - 1)) - 4 * In.Range(1, 8);
            Next = (at + len + 4) & ~3ULL;
        }
        Placed.push_back(at);
        return at;
    }
};

/* ---- the reader: acpi.h's account of a machine ---- */

struct Override
{
    u8 Irq;
    u32 Gsi;
    u16 Flags;
};

struct Expected
{
    bool Ok = false;
    u64 Lapic = 0;
    u64 IoApic = 0;
    std::vector<ulong> Cpus;
    std::vector<Override> Overrides;
    ulong Pm1a = 0;
    bool Reset = false;
    ulong ResetPort = 0;
    u8 ResetValue = 0;
    u8 Century = 0;
    ulong HpetBase = 0;
    u16 HpetTick = 0;
    bool Wdat = false;
};

const u64 Hdr = sizeof(Acpi::ACPISDTHeader);

/* An RSDP's 36 bytes, zeros past what there is: the root it names */
bool RsdpNames(const std::vector<uint8_t>& r, u64& root, bool& xsdt)
{
    if (memcmp(r.data(), "RSD PTR ", 8) != 0)
        return false;
    uint8_t sum = 0;
    for (size_t i = 0; i < 20; i++)
        sum = static_cast<uint8_t>(sum + r[i]);
    if (sum != 0)
        return false;
    if (r[15] >= 2)
    {
        for (size_t i = 20; i < 36; i++)
            sum = static_cast<uint8_t>(sum + r[i]);
        if (sum != 0)
            return false;
    }
    u64 x = 0;
    for (int i = 7; i >= 0; i--)
        x = (x << 8) | r[24 + i];
    if (r[15] >= 2 && x != 0)
    {
        root = x;
        xsdt = true;
    }
    else
    {
        root = r[16] | (r[17] << 8) | (r[18] << 16) | (static_cast<u64>(r[19]) << 24);
        xsdt = false;
    }
    return root != 0;
}

/* The legacy search: at 16-byte steps, an RSDP whole within the range and
   within one page -- the kernel maps a page at a time */
bool Scan(u64 start, u64 end, u64& root, bool& xsdt)
{
    for (u64 a = start; a < end; a += 16)
    {
        u64 limit = std::min<u64>((a & ~(PageBytes - 1)) + PageBytes, end);
        if (a + 20 > limit || !Is(a, "RSD PTR "))
            continue;
        if (Peek(a + 15) >= 2 && a + 36 > limit)
            continue;
        std::vector<uint8_t> r(36);
        for (size_t i = 0; i < r.size(); i++)
            r[i] = Peek(a + i);
        if (RsdpNames(r, root, xsdt))
            return true;
    }
    return false;
}

bool FindRoot(u64& root, bool& xsdt)
{
    const auto& tag = Kernel::Grub::Rsdp;
    if (tag.size() >= 20)
    {
        std::vector<uint8_t> r(36, 0);
        std::copy(tag.begin(), tag.begin() + std::min<size_t>(tag.size(), 36), r.begin());
        if (RsdpNames(r, root, xsdt))
            return true;
    }
    u64 ebda = Read(0x40E, 2) << 4;
    if (ebda >= 0x80000 && ebda < 0xA0000 && Scan(ebda, ebda + 1024, root, xsdt))
        return true;
    return Scan(0xE0000, 0x100000, root, xsdt);
}

/* The table at phys can be mapped whole: its length, or 0 */
u64 Mappable(u64 phys)
{
    if (phys > MaxPhys - Hdr)
        return 0;
    u64 len = Read(phys + 4, 4);
    if (len < Hdr || len > MaxTableLength || len > MaxPhys - phys)
        return 0;
    return len;
}

bool ReadMadt(u64 madt, u64 len, Expected& e)
{
    const u64 body = madt + Hdr;
    if (len < Hdr + 8)
        return false;
    e.Lapic = Read(body, 4);
    if (e.Lapic & (PageBytes - 1))
        return false;

    bool ioApic = false;
    u32 ioApicBase = 0;
    const u64 end = madt + len;
    for (u64 p = body + 8; p + 2 <= end && p + Peek(p + 1) <= end; p += Peek(p + 1))
    {
        const uint8_t type = Peek(p), l = Peek(p + 1);
        if (l == 0)
            break;
        if (type == 0)
        {
            if (l < 8)
                return false;
            ulong id = Peek(p + 3);
            if ((Read(p + 4, 4) & 1) && id < MaxCpus && std::find(e.Cpus.begin(), e.Cpus.end(), id) == e.Cpus.end())
                e.Cpus.push_back(id);
        }
        else if (type == 1)
        {
            if (l < 12)
                return false;
            u64 addr = Read(p + 4, 4);
            u32 base = static_cast<u32>(Read(p + 8, 4));
            if ((addr & (PageBytes - 1)) > PageBytes - Acpi::IoApicRegisterBytes)
            {
                Fuzz::Reached("an IO-APIC across its page");
                continue;
            }
            /* The one at GSI 0 the driver can drive, else the first */
            if (ioApic && (ioApicBase == 0 || base != 0))
                continue;
            if (ioApic)
                Fuzz::Reached("the IO-APIC at GSI 0 after another");
            ioApic = true;
            ioApicBase = base;
            e.IoApic = addr;
        }
        else if (type == 2)
        {
            if (l < 10)
                return false;
            Override o = {Peek(p + 3), static_cast<u32>(Read(p + 4, 4)), static_cast<u16>(Read(p + 8, 2))};
            bool again = false;
            for (auto& x : e.Overrides)
                again |= x.Irq == o.Irq;
            if (!again && o.Gsi <= 0xFF && e.Overrides.size() < 64)
                e.Overrides.push_back(o);
        }
    }
    return ioApic;
}

void ReadFadt(u64 fadt, u64 len, Expected& e)
{
    const u64 body = fadt + Hdr, bodyLen = len - Hdr;
    if (bodyLen >= 32)
        e.Pm1a = Read(body + 28, 4);
    if (bodyLen >= 93 && (Read(body + 76, 4) & (1u << 10)) && Peek(body + 80) == 1)
    {
        e.Reset = true;
        e.ResetPort = Read(body + 84, 8);
        e.ResetValue = Peek(body + 92);
    }
    if (bodyLen >= 73)
        e.Century = Peek(body + 72);
}

void ReadHpet(u64 hpet, u64 len, Expected& e)
{
    const u64 body = hpet + Hdr;
    if (len < Hdr + 20 || Peek(body + 4) != 0)
        return;
    e.HpetBase = Read(body + 8, 8);
    e.HpetTick = static_cast<u16>(Read(body + 17, 2));
}

void ReadWdat(u64 wdat, u64 len, Expected& e)
{
    const u64 body = wdat + Hdr;
    if (len < Hdr + 32)
        return;
    u64 entries = std::min<u64>(Read(body + 28, 4), (len - Hdr - 32) / 24);
    for (u64 i = 0; i < entries; i++)
    {
        u64 entry = body + 32 + 24 * i;
        if (Peek(entry + 4) == 1 && Read(entry + 8, 8) == 0x70)
            return;
    }
    e.Wdat = true;
}

Expected ReadMachine()
{
    Expected e;
    u64 root = 0;
    bool xsdt = false;
    if (!FindRoot(root, xsdt))
        return e;
    u64 rootLen = Mappable(root);
    if (rootLen <= Hdr || !Is(root, xsdt ? "XSDT" : "RSDT"))
        return e;

    /* The first of each kind read that can be mapped */
    static const char* const Kinds[] = {"APIC", "FACP", "HPET", "WDAT"};
    static_assert(sizeof(Kinds) / sizeof(Kinds[0]) == Acpi::MaxTables, "a kind for each table read");
    u64 table[Acpi::MaxTables] = {}, len[Acpi::MaxTables] = {};
    const int size = xsdt ? 8 : 4;
    for (u64 i = 0; i < (rootLen - Hdr) / size; i++)
    {
        u64 at = Read(root + Hdr + i * size, size);
        if (at > MaxPhys - Hdr)
            continue;
        for (size_t k = 0; k < Acpi::MaxTables; k++)
        {
            if (len[k] == 0 && Is(at, Kinds[k]) && Mappable(at) != 0)
            {
                table[k] = at;
                len[k] = Mappable(at);
            }
        }
    }

    if (len[0] == 0 || !ReadMadt(table[0], len[0], e))
        return e;
    if (len[1] != 0)
        ReadFadt(table[1], len[1], e);
    if (len[2] != 0)
        ReadHpet(table[2], len[2], e);
    if (len[3] != 0)
        ReadWdat(table[3], len[3], e);
    e.Ok = true;
    return e;
}

/* ---- the run ---- */

void Run(Fuzz::Input& in)
{
    const bool xsdt = in.Bool();
    Placer place = {in, xsdt, 0x7FE00000, {}};

    /* The tables: the four this kernel reads, each now and then as long as
       a table may be, and what else a firmware lists -- seconds of the four
       among them, mostly after the first */
    struct Table
    {
        std::string Name;
        std::vector<uint8_t> Bytes;
    };
    std::vector<Table> tables;
    if (!in.Chance(8))
        tables.push_back({"APIC", Sdt("APIC", MadtBody(in))});
    if (!in.Chance(64))
        tables.push_back({"FACP", Sdt("FACP", FadtBody(in))});
    if (in.Bool())
        tables.push_back({"HPET", Sdt("HPET", HpetBody(in))});
    if (in.Chance(96))
        tables.push_back({"WDAT", Sdt("WDAT", WdatBody(in))});
    for (auto& t : tables)
    {
        if (in.Chance(8))
        {
            t.Bytes.resize(MaxTableLength - (in.Bool() ? 0 : in.Below(PageBytes)), 0);
            Put(t.Bytes, 4, t.Bytes.size(), 4);
            Fuzz::Reached("a table as long as one may be");
        }
    }
    static const char* const Others[] = {"SSDT", "SSDT", "SSDT", "MCFG", "BGRT", "WAET", "SRAT", "DMAR",
                                         "APIC", "FACP", "HPET", "WDAT"};
    for (size_t n = in.Chance(16) ? in.Below(40) : in.Below(6); n > 0; n--)
    {
        const char* name = in.Pick(Others);
        size_t len = in.Chance(16) ? in.Below(3 * PageBytes) : in.Below(512);
        size_t at = in.Chance(48) ? in.Below(tables.size() + 1) : tables.size();
        tables.insert(tables.begin() + at, {name, Sdt(name, Fuzz::Noise(in.U32(), len))});
    }
    if (in.Chance(16))
    {
        /* One whose length is not a table's: short of its header, or longer
           than the kernel maps */
        const char* name = in.Pick(Others);
        std::vector<uint8_t> bad = Sdt(name, {});
        Put(bad, 4, in.Bool() ? in.Below(Hdr) : MaxTableLength + 1 + in.Below(in.Chance(32) ? 1ULL << 31 : 64), 4);
        tables.insert(tables.begin() + in.Below(tables.size() + 1), {name, bad});
        Fuzz::Reached("a table whose length is not a table's");
    }

    std::vector<u64> entries;
    for (auto& t : tables)
    {
        u64 at = place.Place(t.Bytes.size());
        Poke(at, t.Bytes);
        entries.push_back(at);
        Fuzz::Say("%s at 0x%llx, %zu bytes", t.Name.c_str(), (ull)at, t.Bytes.size());
    }
    for (size_t n = in.Chance(16) ? in.Range(1, 4) : 0; n > 0; n--)
    {
        /* An entry naming nothing, a table twice, or past the address space */
        u64 at = in.Chance(64) ? MaxPhys - in.Below(64) : in.Chance(128) ? 0 : in.U64() >> in.Below(64);
        if (!entries.empty() && in.Bool())
            at = entries[in.Below(entries.size())];
        entries.insert(entries.begin() + in.Below(entries.size() + 1), xsdt ? at : static_cast<u32>(at));
    }

    /* The root */
    std::vector<uint8_t> list;
    for (u64 at : entries)
        Put(list, list.size(), at, xsdt ? 8 : 4);
    if (in.Chance(4))
    {
        /* As long as a root may be: the rest of it names address 0 */
        list.resize(MaxTableLength - Hdr, 0);
        Fuzz::Reached("a root as long as one may be");
    }
    std::vector<uint8_t> root = Sdt(in.Chance(8) ? "RSDT" : xsdt ? "XSDT" : "RSDT", list);
    if (in.Chance(8))
        root[0] = 'X';
    const u64 rootAt = place.Place(root.size());
    Poke(rootAt, root);

    /* The RSDP: where the search looks -- the Multiboot tag's copy, the
       EBDA, the BIOS area -- or where it does not, behind decoys */
    u8 revision = xsdt ? 2 : static_cast<u8>(in.Chance(64) ? 2 : 0);
    u32 rsdt = xsdt ? (in.Bool() ? 0 : in.U32()) : static_cast<u32>(rootAt);
    std::vector<uint8_t> rsdp = MakeRsdp(revision, rsdt, xsdt ? rootAt : 0);
    const u16 segment = in.Chance(16) ? in.U16() : static_cast<u16>(0x9FC0 - 0x40 * in.Below(0x20));
    const uint8_t bda[] = {static_cast<uint8_t>(segment), static_cast<uint8_t>(segment >> 8)};
    Poke(0x40E, bda, sizeof(bda));
    const u64 ebda = static_cast<u64>(segment) << 4;
    for (size_t n = in.Chance(32) ? in.Range(1, 3) : 0; n > 0; n--)
    {
        /* A checksum that does not hold, or an RSDP naming no root */
        std::vector<uint8_t> decoy = in.Bool() ? MakeRsdp(revision, 0, 0) : rsdp;
        if (decoy == rsdp || in.Bool())
            decoy[8 + in.Below(decoy.size() - 8)] ^= static_cast<uint8_t>(in.Range(1, 255));
        Poke(in.Bool() ? ebda + 16 * in.Below(8) : 0xE0000 + 16 * in.Below(64), decoy);
        Fuzz::Reached("a decoy RSDP");
    }
    u64 where = in.Below(8);
    if (where <= 2)
    {
        /* ACPI_OLD's copy is 20 bytes, ACPI_NEW's 36; a tag of another size,
           or a copy that does not hold -- where the search then looks for the
           RSDP in memory, as a firmware has it there too */
        static const size_t Sizes[] = {20, 36, 36, 36, 8, 19, 24, 40};
        size_t size = in.Pick(Sizes);
        Kernel::Grub::Rsdp = rsdp;
        Kernel::Grub::Rsdp.resize(size, 0);
        if (in.Chance(64))
            Kernel::Grub::Rsdp[in.Below(size)] ^= 0x40;
        std::vector<uint8_t> copy(36, 0);
        std::copy(rsdp.begin(), rsdp.begin() + std::min<size_t>(size, rsdp.size()), copy.begin());
        u64 named = 0;
        bool x = false;
        if (size >= 20 && RsdpNames(copy, named, x) && Kernel::Grub::Rsdp == std::vector<uint8_t>(copy.begin(),
            copy.begin() + std::min<size_t>(size, 36)))
            Fuzz::Reached("an RSDP in the Multiboot tag");
        else
            where = in.Chance(64) ? 7 : in.Range(3, 6);
    }
    switch (where)
    {
    case 0:
    case 1:
    case 2:
        break;
    case 3:
    case 4:
        Poke(ebda + 16 * in.Below(66), rsdp);
        Fuzz::Reached("an RSDP in the EBDA");
        break;
    case 5:
    case 6:
        Poke(0xE0000 + 16 * in.Below(0x2000), rsdp);
        Fuzz::Reached("an RSDP in the BIOS area");
        break;
    default:
        Poke(0xC0000 + 16 * in.Below(0x2000), rsdp);
        break;
    }

    /* Damage, anywhere in what the parse reads */
    if (in.Chance(32))
    {
        for (size_t n = in.Range(1, 8); n > 0; n--)
        {
            u64 at = (in.Chance(32) || entries.empty()) ? rootAt : entries[in.Below(entries.size())];
            uint8_t b = in.U8();
            Poke((at + in.Below(256)) & (MaxPhys - 1), &b, 1);
        }
        Fuzz::Reached("damage");
    }

    /* The window: whole, or refusing a mapping, or with little room */
    bool faulty = false;
    if (in.Chance(32))
    {
        FailAt = static_cast<long>(in.Below(64));
        faulty = true;
    }
    if (in.Chance(16))
    {
        Window = in.Range(1, 40);
        faulty = true;
    }

    const Expected e = ReadMachine();
    Fuzz::Say("root 0x%llx, the reader says %s", (ull)rootAt, e.Ok ? "a machine" : "no machine");

    auto& acpi = Acpi::GetInstance();
    Stdlib::Error err = acpi.Parse();
    Fuzz::CheckNoLocksHeld("the parse");

    /* Whichever way it ended: the tables let go of, and nothing of the
       window held but the APICs' pages -- and not those after a failure */
    bool tableKept = acpi.Root != nullptr;
    for (auto* t : acpi.Table)
        tableKept |= t != nullptr;
    INVARIANT(!tableKept, "a table still mapped when the parse is done");
    if (err.Ok())
    {
        INVARIANT(acpi.GetLapicAddress() != nullptr && acpi.GetIoApicAddress() != nullptr,
            "a parse that succeeds with no %s", acpi.GetLapicAddress() ? "IO-APIC" : "local APIC");
        PhysOf(acpi.GetLapicAddress());
        PhysOf(acpi.GetIoApicAddress());
    }
    else
    {
        INVARIANT(acpi.GetLapicAddress() == nullptr && acpi.GetIoApicAddress() == nullptr,
            "an APIC's page kept by a parse that failed");
    }
    INVARIANT(Held == (err.Ok() ? 2u : 0u), "%zu slots of the window held after the parse, where %u are the APICs'",
        Held, err.Ok() ? 2u : 0u);

    if (faulty)
    {
        Fuzz::Reached(Refused ? "the window refused" : "a window that had room after all");
        return;
    }

    INVARIANT(err.Ok() == e.Ok, "the parse says %s where the machine is %s", err.Ok() ? "yes" : "no",
        e.Ok ? "whole" : "not one to boot");
    if (!e.Ok)
    {
        Fuzz::Reached("no machine");
        return;
    }

    INVARIANT(PhysOf(acpi.GetLapicAddress()) == e.Lapic, "the local APIC mapped at 0x%llx, not 0x%llx",
        (ull)PhysOf(acpi.GetLapicAddress()), (ull)e.Lapic);
    INVARIANT(PhysOf(acpi.GetIoApicAddress()) == e.IoApic, "the IO-APIC mapped at 0x%llx, where the one to "
        "drive is at 0x%llx", (ull)PhysOf(acpi.GetIoApicAddress()), (ull)e.IoApic);
    INVARIANT(Kernel::Inserted == e.Cpus, "%zu CPUs taken, where the MADT enables %zu", Kernel::Inserted.size(),
        e.Cpus.size());
    for (u32 irq = 0; irq < 256; irq++)
    {
        u32 gsi = irq;
        u16 flags = 0;
        for (auto& o : e.Overrides)
        {
            if (o.Irq == irq)
            {
                gsi = o.Gsi;
                flags = o.Flags;
                break;
            }
        }
        const u8 i = static_cast<u8>(irq);
        INVARIANT(acpi.GetGsiByIrq(i) == gsi && acpi.GetIrqFlags(i) == flags,
            "irq %u goes to gsi %u flags 0x%x, not %u 0x%x", irq, acpi.GetGsiByIrq(i), acpi.GetIrqFlags(i), gsi, flags);
    }
    INVARIANT(acpi.GetPm1aCntPort() == e.Pm1a, "PM1a at 0x%lx, not 0x%lx", acpi.GetPm1aCntPort(), e.Pm1a);
    INVARIANT(acpi.HasResetReg() == e.Reset && acpi.GetResetRegPort() == e.ResetPort &&
        acpi.GetResetValue() == e.ResetValue, "the reset register is not the FADT's");
    INVARIANT(acpi.GetCenturyRegister() == e.Century, "the century register is %u, not %u",
        acpi.GetCenturyRegister(), e.Century);
    INVARIANT(acpi.GetHpetBasePhys() == e.HpetBase && acpi.GetHpetMinTick() == e.HpetTick,
        "the HPET at 0x%lx tick %u, not 0x%lx %u", acpi.GetHpetBasePhys(), acpi.GetHpetMinTick(), e.HpetBase,
        e.HpetTick);
    INVARIANT(acpi.HasFirmwareWatchdog() == e.Wdat, "a firmware watchdog %s", e.Wdat ? "missed" : "from nowhere");

    Fuzz::Reached("a machine");
    if (e.Cpus.size() > 1)
        Fuzz::Reached("a machine of several CPUs");
    if (e.Reset && e.Wdat && e.HpetBase != 0)
        Fuzz::Reached("a machine with every table read");
    if (tables.size() > 20)
        Fuzz::Reached("a firmware of many tables");
}

void Reset()
{
    while (!Maps.empty())
        Release(Maps.begin());
    Held = 0;
    Window = WindowSlots;
    FailAt = -1;
    MapCalls = 0;
    Refused = false;
    Phys.clear();
    Kernel::Inserted.clear();
    Kernel::Grub::Rsdp.clear();
    auto& acpi = Acpi::GetInstance();
    acpi.~Acpi();
    new (&acpi) Acpi();
}

}

const Fuzz::Target Fuzz::TheTarget = {"acpi", Reset, Run, 4096, 20000};
