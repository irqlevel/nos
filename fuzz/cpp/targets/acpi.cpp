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

using namespace kernel;
using kernel::mm::PageTable;

namespace
{

typedef unsigned long long ull;

const u64 PAGE_BYTES = consts::PAGE_SIZE;
const u64 MAX_PHYS = mm::MemoryMap::MAX_PHYS_ADDR;
const u64 MAX_TABLE_LENGTH = Acpi::MAX_TABLE_LENGTH;
const size_t WINDOW_SLOTS = PageTable::TMP_MAP_SHARED_COUNT;

/* What of the window the parse may hold at once (acpi.h): the root and a
   table of each kind, each at most MaxTableLength long and so across at
   most that many pages and one more; then a header looked at, across two,
   or the local APIC's page and two IO-APICs', the one kept and the one
   taking its place. */
const size_t MAX_TABLE_PAGES = Acpi::MAX_TABLE_LENGTH / consts::PAGE_SIZE + 1;
const size_t MAX_HELD = (Acpi::MAX_TABLES + 1) * MAX_TABLE_PAGES + 3;

/* ---- physical memory: sparse, zeros where nothing was put ---- */

std::map<u64, std::vector<uint8_t>> phys;

void poke(u64 addr, const uint8_t* bytes, size_t len)
{
    while (len != 0)
    {
        u64 page = addr & ~(PAGE_BYTES - 1);
        size_t at = addr - page;
        size_t chunk = std::min<size_t>(len, PAGE_BYTES - at);
        auto& p = phys[page];
        if (p.empty())
            p.assign(PAGE_BYTES, 0);
        memcpy(p.data() + at, bytes, chunk);
        addr += chunk;
        bytes += chunk;
        len -= chunk;
    }
}

void poke(u64 addr, const std::vector<uint8_t>& bytes)
{
    poke(addr, bytes.data(), bytes.size());
}

uint8_t peek(u64 addr)
{
    auto it = phys.find(addr & ~(PAGE_BYTES - 1));
    return (it == phys.end()) ? 0 : it->second[addr & (PAGE_BYTES - 1)];
}

/* A little-endian word of physical memory */
u64 read(u64 addr, int bytes)
{
    u64 x = 0;
    for (int i = bytes - 1; i >= 0; i--)
        x = (x << 8) | peek(addr + i);
    return x;
}

bool is(u64 addr, const char* signature)
{
    for (size_t i = 0; i < strlen(signature); i++)
    {
        if (peek(addr + i) != static_cast<uint8_t>(signature[i]))
            return false;
    }
    return true;
}

/* ---- the TmpMap window ---- */

struct Mapping
{
    u64 phys_page;
    size_t pages;
    uint8_t* block;
    std::vector<bool> held;
};

/* By the VA of the first page mapped */
std::map<ulong, Mapping> maps;
size_t held;
/* The slots the window has for the parse, and the mapping it refuses (-1:
   none): the input's */
size_t window = WINDOW_SLOTS;
long fail_at = -1;
long map_calls;
bool refused;

void release(std::map<ulong, Mapping>::iterator it)
{
    ASAN_UNPOISON_MEMORY_REGION(it->second.block, (it->second.pages + 2) * PAGE_BYTES);
    free(it->second.block);
    maps.erase(it);
}

/* The mapping a VA is in, and which of its pages */
bool find(ulong va, std::map<ulong, Mapping>::iterator& it, size_t& page)
{
    it = maps.upper_bound(va);
    if (it == maps.begin())
        return false;
    --it;
    if (va >= it->first + it->second.pages * PAGE_BYTES)
        return false;
    page = (va - it->first) / PAGE_BYTES;
    return true;
}

/* The physical address a VA of the window maps, held */
u64 phys_of(const void* va)
{
    std::map<ulong, Mapping>::iterator it;
    size_t page = 0;
    ulong v = reinterpret_cast<ulong>(va);
    INVARIANT(find(v, it, page) && it->second.held[page], "0x%lx is no page of the window held", v);
    return it->second.phys_page + page * PAGE_BYTES + (v & (PAGE_BYTES - 1));
}

/* TmpMapPage, TmpMapAddress and TmpMapRange: len bytes at phys, in the
   window as the kernel's are, one slot a page. The pages are whole for the
   first two, whose callers read a page -- the RSDP's search, the BIOS data
   area -- or hand it to a driver, and for TmpMapRange only the range: what
   ACPI maps that way is a table, and all a parser may read of it. */
ulong map(u64 pa, u64 len, bool exact)
{
    INVARIANT(len <= MAX_TABLE_LENGTH && pa <= MAX_PHYS && len <= MAX_PHYS - pa,
        "a mapping of 0x%llx bytes at 0x%llx: longer than a table, or past the physical address space",
        (ull)len, (ull)pa);
    u64 offset = pa & (PAGE_BYTES - 1);
    size_t pages = (offset + len + PAGE_BYTES - 1) / PAGE_BYTES;
    if (pages == 0)
        pages = 1;
    if (map_calls++ == fail_at || held + pages > window)
    {
        refused = true;
        return 0;
    }

    void* block = nullptr;
    INVARIANT(posix_memalign(&block, PAGE_BYTES, (pages + 2) * PAGE_BYTES) == 0, "no host memory");
    uint8_t* b = static_cast<uint8_t*>(block);
    u64 phys_page = pa - offset;
    for (size_t p = 0; p < pages; p++)
    {
        auto it = phys.find(phys_page + p * PAGE_BYTES);
        if (it == phys.end())
            memset(b + (p + 1) * PAGE_BYTES, 0, PAGE_BYTES);
        else
            memcpy(b + (p + 1) * PAGE_BYTES, it->second.data(), PAGE_BYTES);
    }
    ASAN_POISON_MEMORY_REGION(b, PAGE_BYTES);
    ASAN_POISON_MEMORY_REGION(b + (pages + 1) * PAGE_BYTES, PAGE_BYTES);
    if (exact)
    {
        ASAN_POISON_MEMORY_REGION(b + PAGE_BYTES, offset);
        ASAN_POISON_MEMORY_REGION(b + PAGE_BYTES + offset + len, pages * PAGE_BYTES - offset - len);
    }
    ulong va = reinterpret_cast<ulong>(b + PAGE_BYTES);
    maps[va] = {phys_page, pages, b, std::vector<bool>(pages, true)};
    held += pages;
    INVARIANT(held <= MAX_HELD, "the parse holds %zu slots of the window at once, past the %zu acpi.h bounds",
        held, MAX_HELD);
    return va + offset;
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

ulong PageTable::tmp_map_page(ulong phy_addr)
{
    INVARIANT((phy_addr & (PAGE_BYTES - 1)) == 0, "TmpMapPage(0x%lx), not a page", phy_addr);
    return map(phy_addr, 1, false);
}

ulong PageTable::tmp_map_address(ulong phy_addr)
{
    return map(phy_addr, 1, false);
}

ulong PageTable::tmp_map_range(ulong phy_addr, size_t len)
{
    return map(phy_addr, len, true);
}

ulong PageTable::tmp_unmap_page(ulong virt_addr)
{
    std::map<ulong, Mapping>::iterator it;
    size_t page = 0;
    INVARIANT((virt_addr & (PAGE_BYTES - 1)) == 0 && find(virt_addr, it, page) && it->second.held[page],
        "TmpUnmapPage(0x%lx), which is no slot held", virt_addr);
    it->second.held[page] = false;
    held--;
    u64 pa = it->second.phys_page + page * PAGE_BYTES;
    ASAN_POISON_MEMORY_REGION(reinterpret_cast<void*>(virt_addr), PAGE_BYTES);
    if (std::find(it->second.held.begin(), it->second.held.end(), true) == it->second.held.end())
        release(it);
    return pa;
}

}

/* The CPUs the MADT names, as the CPU table takes them */
std::vector<ulong> inserted;

bool CpuTable::insert_cpu(ulong index)
{
    if (index >= MAX_CPUS || std::find(inserted.begin(), inserted.end(), index) != inserted.end())
        return false;
    inserted.push_back(index);
    return true;
}

namespace grub
{

/* What the Multiboot ACPI tag carried, if the input gives one */
std::vector<uint8_t> rsdp;

const void* get_acpi_rsdp(size_t& size)
{
    size = rsdp.size();
    return size ? rsdp.data() : nullptr;
}

}
}

namespace
{

/* ---- the machine ---- */

void put(std::vector<uint8_t>& v, size_t at, u64 x, int bytes)
{
    if (v.size() < at + bytes)
        v.resize(at + bytes, 0);
    for (int i = 0; i < bytes; i++)
        v[at + i] = static_cast<uint8_t>(x >> (8 * i));
}

/* An SDT: its header, then body */
std::vector<uint8_t> sdt(const char* signature, const std::vector<uint8_t>& body)
{
    std::vector<uint8_t> t(sizeof(Acpi::ACPISDTHeader), 0);
    memcpy(t.data(), signature, 4);
    put(t, 4, t.size() + body.size(), 4);
    t[8] = 1;
    memcpy(t.data() + 10, "NOSFUZ", 6);
    t.insert(t.end(), body.begin(), body.end());
    return t;
}

void checksum(std::vector<uint8_t>& v, size_t len, size_t at)
{
    uint8_t sum = 0;
    v[at] = 0;
    for (size_t i = 0; i < len; i++)
        sum = static_cast<uint8_t>(sum + v[i]);
    v[at] = static_cast<uint8_t>(-sum);
}

std::vector<uint8_t> make_rsdp(uint8_t revision, u32 rsdt, u64 xsdt)
{
    std::vector<uint8_t> r(revision >= 2 ? 36 : 20, 0);
    memcpy(r.data(), "RSD PTR ", 8);
    memcpy(r.data() + 9, "NOSFUZ", 6);
    r[15] = revision;
    put(r, 16, rsdt, 4);
    if (revision >= 2)
    {
        put(r, 20, 36, 4);
        put(r, 24, xsdt, 8);
    }
    checksum(r, 20, 8);
    if (revision >= 2)
        checksum(r, 36, 32);
    return r;
}

void shuffle(fuzz::Input& in, std::vector<std::vector<uint8_t>>& v)
{
    for (size_t i = v.size(); i > 1; i--)
        std::swap(v[i - 1], v[in.below(i)]);
}

std::vector<uint8_t> madt_body(fuzz::Input& in)
{
    std::vector<std::vector<uint8_t>> entries;

    /* The CPUs, some disabled, now and then with ids past MaxCpus or twice */
    size_t cpus = in.chance(16) ? in.below(100) : in.range(1, 16);
    for (size_t i = 0; i < cpus; i++)
    {
        std::vector<uint8_t> e = {0, 8, static_cast<uint8_t>(i), static_cast<uint8_t>(in.chance(16) ? in.u8() : i)};
        put(e, 4, in.chance(24) ? in.u32() & ~1u : 1, 4);
        entries.push_back(e);
    }

    /* The IO-APICs: one, or AMD's two, the one at GSI 0 first -- or not,
       or none, or one whose registers run across its page's end */
    size_t io_apics = in.chance(16) ? in.below(5) : in.range(1, 2);
    const bool reversed = in.chance(48);
    for (size_t i = 0; i < io_apics; i++)
    {
        u32 addr = 0xFEC00000 + 0x1000 * static_cast<u32>(i);
        if (in.chance(16))
            addr += static_cast<u32>(in.boolean() ? in.below(0x1000) : 0xFC0 + in.below(0x40));
        u32 base = 24 * static_cast<u32>(reversed ? io_apics - 1 - i : i);
        if (in.chance(32))
            base = in.chance(128) ? 0 : in.value32();
        std::vector<uint8_t> e = {1, 12, static_cast<uint8_t>(cpus + i), 0};
        put(e, 4, addr, 4);
        put(e, 8, base, 4);
        entries.push_back(e);
    }

    /* The overrides a PC has -- the PIT on pin 2, the SCI level and low --
       and others: IRQs twice, GSIs past what the interrupt layer takes */
    if (!in.chance(32))
        entries.push_back({2, 10, 0, 0, 2, 0, 0, 0, 0, 0});
    if (!in.chance(32))
        entries.push_back({2, 10, 0, 9, 9, 0, 0, 0, 0xD, 0});
    for (size_t n = in.chance(16) ? in.below(80) : in.below(3); n > 0; n--)
    {
        std::vector<uint8_t> e = {2, 10, 0, static_cast<uint8_t>(in.chance(32) ? in.u8() : in.below(24))};
        put(e, 4, in.chance(32) ? in.value32() : in.below(64), 4);
        put(e, 8, in.u16(), 2);
        entries.push_back(e);
    }

    /* What this kernel does not read: NMIs, the local APIC's 64-bit
       address, x2APICs, and kinds past those */
    static const uint8_t OTHERS[][2] = {{4, 6}, {5, 12}, {9, 16}, {10, 12}, {11, 80}, {13, 16}, {0x7F, 2}};
    for (size_t n = in.below(4); n > 0; n--)
    {
        const auto& o = OTHERS[in.below(sizeof(OTHERS) / sizeof(OTHERS[0]))];
        std::vector<uint8_t> e = fuzz::noise(in.u32(), o[1]);
        e[0] = o[0];
        e[1] = o[1];
        entries.push_back(e);
    }

    if (in.chance(32))
    {
        shuffle(in, entries);
        fuzz::reached("a MADT in any order");
    }

    /* An entry's length that is not its kind's: short of it, 0, past it */
    if (!entries.empty() && in.chance(16))
    {
        auto& e = entries[in.below(entries.size())];
        e[1] = static_cast<uint8_t>(in.boolean() ? in.below(e.size()) : e.size() + in.range(1, 16));
        fuzz::reached("a MADT entry of the wrong length");
    }

    std::vector<uint8_t> body;
    u32 lapic = 0xFEE00000;
    if (in.chance(16))
        lapic = in.boolean() ? in.u32() : lapic + static_cast<u32>(in.range(1, 0xFFF));
    put(body, 0, lapic, 4);
    put(body, 4, 1, 4);
    for (auto& e : entries)
        body.insert(body.end(), e.begin(), e.end());
    if (in.chance(16))
        body.resize(in.below(body.size() + 1));
    return body;
}

std::vector<uint8_t> fadt_body(fuzz::Input& in)
{
    /* ACPI 1.0's, 2.0's, 5.0's and 6's, less the header; and each side of
       where a field the kernel reads ends */
    static const size_t LENGTHS[] = {80, 208, 232, 240, 80, 208, 232, 240, 31, 32, 72, 73, 92, 93};
    size_t len = in.chance(32) ? in.below(300) : in.pick(LENGTHS);
    std::vector<uint8_t> body = fuzz::noise(in.u32(), len);
    if (len >= 32 && !in.chance(64))
        put(body, 28, 0x604, 4);
    if (len > 80 && !in.chance(64))
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
    if (len >= 73 && in.boolean())
        body[72] = 0x32;
    return body;
}

std::vector<uint8_t> hpet_body(fuzz::Input& in)
{
    size_t len = in.chance(32) ? in.below(24) : in.chance(32) ? 19 : 20;
    std::vector<uint8_t> body = fuzz::noise(in.u32(), len);
    if (len >= 16 && !in.chance(48))
    {
        body[4] = 0;
        put(body, 8, 0xFED00000, 8);
    }
    return body;
}

std::vector<uint8_t> wdat_body(fuzz::Input& in)
{
    std::vector<uint8_t> body = fuzz::noise(in.u32(), 32);
    size_t entries = in.below(8);
    put(body, 28, in.chance(32) ? in.u32() : entries, 4);
    for (size_t i = 0; i < entries; i++)
    {
        std::vector<uint8_t> entry = fuzz::noise(in.u32(), 24);
        if (in.chance(32))
        {
            /* An instruction on the RTC's ports */
            entry[4] = 1;
            put(entry, 8, 0x70, 8);
        }
        body.insert(body.end(), entry.begin(), entry.end());
    }
    if (in.chance(32))
        body.resize(in.chance(128) ? in.below(body.size() + 1) : body.size() - in.range(1, std::min<size_t>(body.size(), 24)));
    return body;
}

/* Where a table goes: below 4 GiB, or for an XSDT's anywhere; now and then
   with its header across a page's end, or over another table */
struct Placer
{
    fuzz::Input& in;
    bool high;
    u64 next;
    std::vector<u64> placed;

    u64 place(size_t len)
    {
        u64 at;
        if (!placed.empty() && in.chance(8))
        {
            at = placed[in.below(placed.size())] + 4 * in.below(16);
            fuzz::reached("a table over another");
        }
        else if (high && in.chance(64))
        {
            at = 0x100000000ULL + (in.below(1ULL << 20) << 12) + 4 * in.below(1024);
            fuzz::reached("a table above 4 GiB");
        }
        else
        {
            at = next + 4 * in.below(64);
            if (in.chance(32))
                at = ((at + PAGE_BYTES) & ~(PAGE_BYTES - 1)) - 4 * in.range(1, 8);
            next = (at + len + 4) & ~3ULL;
        }
        placed.push_back(at);
        return at;
    }
};

/* ---- the reader: acpi.h's account of a machine ---- */

struct Override
{
    u8 irq;
    u32 gsi;
    u16 flags;
};

struct Expected
{
    bool ok = false;
    u64 lapic = 0;
    u64 io_apic = 0;
    std::vector<ulong> cpus;
    std::vector<Override> overrides;
    ulong pm1a = 0;
    bool reset = false;
    ulong reset_port = 0;
    u8 reset_value = 0;
    u8 century = 0;
    ulong hpet_base = 0;
    u16 hpet_tick = 0;
    bool wdat = false;
};

const u64 HDR = sizeof(Acpi::ACPISDTHeader);

/* An RSDP's 36 bytes, zeros past what there is: the root it names */
bool rsdp_names(const std::vector<uint8_t>& r, u64& root, bool& xsdt)
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
bool scan(u64 start, u64 end, u64& root, bool& xsdt)
{
    for (u64 a = start; a < end; a += 16)
    {
        u64 limit = std::min<u64>((a & ~(PAGE_BYTES - 1)) + PAGE_BYTES, end);
        if (a + 20 > limit || !is(a, "RSD PTR "))
            continue;
        if (peek(a + 15) >= 2 && a + 36 > limit)
            continue;
        std::vector<uint8_t> r(36);
        for (size_t i = 0; i < r.size(); i++)
            r[i] = peek(a + i);
        if (rsdp_names(r, root, xsdt))
            return true;
    }
    return false;
}

bool find_root(u64& root, bool& xsdt)
{
    const auto& tag = kernel::grub::rsdp;
    if (tag.size() >= 20)
    {
        std::vector<uint8_t> r(36, 0);
        std::copy(tag.begin(), tag.begin() + std::min<size_t>(tag.size(), 36), r.begin());
        if (rsdp_names(r, root, xsdt))
            return true;
    }
    u64 ebda = read(0x40E, 2) << 4;
    if (ebda >= 0x80000 && ebda < 0xA0000 && scan(ebda, ebda + 1024, root, xsdt))
        return true;
    return scan(0xE0000, 0x100000, root, xsdt);
}

/* The table at phys can be mapped whole: its length, or 0 */
u64 mappable(u64 pa)
{
    if (pa > MAX_PHYS - HDR)
        return 0;
    u64 len = read(pa + 4, 4);
    if (len < HDR || len > MAX_TABLE_LENGTH || len > MAX_PHYS - pa)
        return 0;
    return len;
}

bool read_madt(u64 madt, u64 len, Expected& e)
{
    const u64 body = madt + HDR;
    if (len < HDR + 8)
        return false;
    e.lapic = read(body, 4);
    if (e.lapic & (PAGE_BYTES - 1))
        return false;

    bool io_apic = false;
    u32 io_apic_base = 0;
    const u64 end = madt + len;
    for (u64 p = body + 8; p + 2 <= end && p + peek(p + 1) <= end; p += peek(p + 1))
    {
        const uint8_t type = peek(p), l = peek(p + 1);
        if (l == 0)
            break;
        if (type == 0)
        {
            if (l < 8)
                return false;
            ulong id = peek(p + 3);
            if ((read(p + 4, 4) & 1) && id < MAX_CPUS && std::find(e.cpus.begin(), e.cpus.end(), id) == e.cpus.end())
                e.cpus.push_back(id);
        }
        else if (type == 1)
        {
            if (l < 12)
                return false;
            u64 addr = read(p + 4, 4);
            u32 base = static_cast<u32>(read(p + 8, 4));
            if ((addr & (PAGE_BYTES - 1)) > PAGE_BYTES - Acpi::IO_APIC_REGISTER_BYTES)
            {
                fuzz::reached("an IO-APIC across its page");
                continue;
            }
            /* The one at GSI 0 the driver can drive, else the first */
            if (io_apic && (io_apic_base == 0 || base != 0))
                continue;
            if (io_apic)
                fuzz::reached("the IO-APIC at GSI 0 after another");
            io_apic = true;
            io_apic_base = base;
            e.io_apic = addr;
        }
        else if (type == 2)
        {
            if (l < 10)
                return false;
            Override o = {peek(p + 3), static_cast<u32>(read(p + 4, 4)), static_cast<u16>(read(p + 8, 2))};
            bool again = false;
            for (auto& x : e.overrides)
                again |= x.irq == o.irq;
            if (!again && o.gsi <= 0xFF && e.overrides.size() < 64)
                e.overrides.push_back(o);
        }
    }
    return io_apic;
}

void read_fadt(u64 fadt, u64 len, Expected& e)
{
    const u64 body = fadt + HDR, body_len = len - HDR;
    if (body_len >= 32)
        e.pm1a = read(body + 28, 4);
    if (body_len >= 93 && (read(body + 76, 4) & (1u << 10)) && peek(body + 80) == 1)
    {
        e.reset = true;
        e.reset_port = read(body + 84, 8);
        e.reset_value = peek(body + 92);
    }
    if (body_len >= 73)
        e.century = peek(body + 72);
}

void read_hpet(u64 hpet, u64 len, Expected& e)
{
    const u64 body = hpet + HDR;
    if (len < HDR + 20 || peek(body + 4) != 0)
        return;
    e.hpet_base = read(body + 8, 8);
    e.hpet_tick = static_cast<u16>(read(body + 17, 2));
}

void read_wdat(u64 wdat, u64 len, Expected& e)
{
    const u64 body = wdat + HDR;
    if (len < HDR + 32)
        return;
    u64 entries = std::min<u64>(read(body + 28, 4), (len - HDR - 32) / 24);
    for (u64 i = 0; i < entries; i++)
    {
        u64 entry = body + 32 + 24 * i;
        if (peek(entry + 4) == 1 && read(entry + 8, 8) == 0x70)
            return;
    }
    e.wdat = true;
}

Expected read_machine()
{
    Expected e;
    u64 root = 0;
    bool xsdt = false;
    if (!find_root(root, xsdt))
        return e;
    u64 root_len = mappable(root);
    if (root_len <= HDR || !is(root, xsdt ? "XSDT" : "RSDT"))
        return e;

    /* The first of each kind read that can be mapped */
    static const char* const KINDS[] = {"APIC", "FACP", "HPET", "WDAT"};
    static_assert(sizeof(KINDS) / sizeof(KINDS[0]) == Acpi::MAX_TABLES, "a kind for each table read");
    u64 table[Acpi::MAX_TABLES] = {}, len[Acpi::MAX_TABLES] = {};
    const int size = xsdt ? 8 : 4;
    for (u64 i = 0; i < (root_len - HDR) / size; i++)
    {
        u64 at = read(root + HDR + i * size, size);
        if (at > MAX_PHYS - HDR)
            continue;
        for (size_t k = 0; k < Acpi::MAX_TABLES; k++)
        {
            if (len[k] == 0 && is(at, KINDS[k]) && mappable(at) != 0)
            {
                table[k] = at;
                len[k] = mappable(at);
            }
        }
    }

    if (len[0] == 0 || !read_madt(table[0], len[0], e))
        return e;
    if (len[1] != 0)
        read_fadt(table[1], len[1], e);
    if (len[2] != 0)
        read_hpet(table[2], len[2], e);
    if (len[3] != 0)
        read_wdat(table[3], len[3], e);
    e.ok = true;
    return e;
}

/* ---- the run ---- */

void run(fuzz::Input& in)
{
    const bool xsdt = in.boolean();
    Placer place = {in, xsdt, 0x7FE00000, {}};

    /* The tables: the four this kernel reads, each now and then as long as
       a table may be, and what else a firmware lists -- seconds of the four
       among them, mostly after the first */
    struct Table
    {
        std::string name;
        std::vector<uint8_t> bytes;
    };
    std::vector<Table> tables;
    if (!in.chance(8))
        tables.push_back({"APIC", sdt("APIC", madt_body(in))});
    if (!in.chance(64))
        tables.push_back({"FACP", sdt("FACP", fadt_body(in))});
    if (in.boolean())
        tables.push_back({"HPET", sdt("HPET", hpet_body(in))});
    if (in.chance(96))
        tables.push_back({"WDAT", sdt("WDAT", wdat_body(in))});
    for (auto& t : tables)
    {
        if (in.chance(8))
        {
            t.bytes.resize(MAX_TABLE_LENGTH - (in.boolean() ? 0 : in.below(PAGE_BYTES)), 0);
            put(t.bytes, 4, t.bytes.size(), 4);
            fuzz::reached("a table as long as one may be");
        }
    }
    static const char* const OTHERS[] = {"SSDT", "SSDT", "SSDT", "MCFG", "BGRT", "WAET", "SRAT", "DMAR",
                                         "APIC", "FACP", "HPET", "WDAT"};
    for (size_t n = in.chance(16) ? in.below(40) : in.below(6); n > 0; n--)
    {
        const char* name = in.pick(OTHERS);
        size_t len = in.chance(16) ? in.below(3 * PAGE_BYTES) : in.below(512);
        size_t at = in.chance(48) ? in.below(tables.size() + 1) : tables.size();
        tables.insert(tables.begin() + at, {name, sdt(name, fuzz::noise(in.u32(), len))});
    }
    if (in.chance(16))
    {
        /* One whose length is not a table's: short of its header, or longer
           than the kernel maps */
        const char* name = in.pick(OTHERS);
        std::vector<uint8_t> bad = sdt(name, {});
        put(bad, 4, in.boolean() ? in.below(HDR) : MAX_TABLE_LENGTH + 1 + in.below(in.chance(32) ? 1ULL << 31 : 64), 4);
        tables.insert(tables.begin() + in.below(tables.size() + 1), {name, bad});
        fuzz::reached("a table whose length is not a table's");
    }

    std::vector<u64> entries;
    for (auto& t : tables)
    {
        u64 at = place.place(t.bytes.size());
        poke(at, t.bytes);
        entries.push_back(at);
        fuzz::say("%s at 0x%llx, %zu bytes", t.name.c_str(), (ull)at, t.bytes.size());
    }
    for (size_t n = in.chance(16) ? in.range(1, 4) : 0; n > 0; n--)
    {
        /* An entry naming nothing, a table twice, or past the address space */
        u64 at = in.chance(64) ? MAX_PHYS - in.below(64) : in.chance(128) ? 0 : in.u64() >> in.below(64);
        if (!entries.empty() && in.boolean())
            at = entries[in.below(entries.size())];
        entries.insert(entries.begin() + in.below(entries.size() + 1), xsdt ? at : static_cast<u32>(at));
    }

    /* The root */
    std::vector<uint8_t> list;
    for (u64 at : entries)
        put(list, list.size(), at, xsdt ? 8 : 4);
    if (in.chance(4))
    {
        /* As long as a root may be: the rest of it names address 0 */
        list.resize(MAX_TABLE_LENGTH - HDR, 0);
        fuzz::reached("a root as long as one may be");
    }
    std::vector<uint8_t> root = sdt(in.chance(8) ? "RSDT" : xsdt ? "XSDT" : "RSDT", list);
    if (in.chance(8))
        root[0] = 'X';
    const u64 root_at = place.place(root.size());
    poke(root_at, root);

    /* The RSDP: where the search looks -- the Multiboot tag's copy, the
       EBDA, the BIOS area -- or where it does not, behind decoys */
    u8 revision = xsdt ? 2 : static_cast<u8>(in.chance(64) ? 2 : 0);
    u32 rsdt = xsdt ? (in.boolean() ? 0 : in.u32()) : static_cast<u32>(root_at);
    std::vector<uint8_t> rsdp = make_rsdp(revision, rsdt, xsdt ? root_at : 0);
    const u16 segment = in.chance(16) ? in.u16() : static_cast<u16>(0x9FC0 - 0x40 * in.below(0x20));
    const uint8_t bda[] = {static_cast<uint8_t>(segment), static_cast<uint8_t>(segment >> 8)};
    poke(0x40E, bda, sizeof(bda));
    const u64 ebda = static_cast<u64>(segment) << 4;
    for (size_t n = in.chance(32) ? in.range(1, 3) : 0; n > 0; n--)
    {
        /* A checksum that does not hold, or an RSDP naming no root */
        std::vector<uint8_t> decoy = in.boolean() ? make_rsdp(revision, 0, 0) : rsdp;
        if (decoy == rsdp || in.boolean())
            decoy[8 + in.below(decoy.size() - 8)] ^= static_cast<uint8_t>(in.range(1, 255));
        poke(in.boolean() ? ebda + 16 * in.below(8) : 0xE0000 + 16 * in.below(64), decoy);
        fuzz::reached("a decoy RSDP");
    }
    u64 where = in.below(8);
    if (where <= 2)
    {
        /* ACPI_OLD's copy is 20 bytes, ACPI_NEW's 36; a tag of another size,
           or a copy that does not hold -- where the search then looks for the
           RSDP in memory, as a firmware has it there too */
        static const size_t SIZES[] = {20, 36, 36, 36, 8, 19, 24, 40};
        size_t size = in.pick(SIZES);
        kernel::grub::rsdp = rsdp;
        kernel::grub::rsdp.resize(size, 0);
        if (in.chance(64))
            kernel::grub::rsdp[in.below(size)] ^= 0x40;
        std::vector<uint8_t> copy(36, 0);
        std::copy(rsdp.begin(), rsdp.begin() + std::min<size_t>(size, rsdp.size()), copy.begin());
        u64 named = 0;
        bool x = false;
        if (size >= 20 && rsdp_names(copy, named, x) && kernel::grub::rsdp == std::vector<uint8_t>(copy.begin(),
            copy.begin() + std::min<size_t>(size, 36)))
            fuzz::reached("an RSDP in the Multiboot tag");
        else
            where = in.chance(64) ? 7 : in.range(3, 6);
    }
    switch (where)
    {
    case 0:
    case 1:
    case 2:
        break;
    case 3:
    case 4:
        poke(ebda + 16 * in.below(66), rsdp);
        fuzz::reached("an RSDP in the EBDA");
        break;
    case 5:
    case 6:
        poke(0xE0000 + 16 * in.below(0x2000), rsdp);
        fuzz::reached("an RSDP in the BIOS area");
        break;
    default:
        poke(0xC0000 + 16 * in.below(0x2000), rsdp);
        break;
    }

    /* Damage, anywhere in what the parse reads */
    if (in.chance(32))
    {
        for (size_t n = in.range(1, 8); n > 0; n--)
        {
            u64 at = (in.chance(32) || entries.empty()) ? root_at : entries[in.below(entries.size())];
            uint8_t b = in.u8();
            poke((at + in.below(256)) & (MAX_PHYS - 1), &b, 1);
        }
        fuzz::reached("damage");
    }

    /* The window: whole, or refusing a mapping, or with little room */
    bool faulty = false;
    if (in.chance(32))
    {
        fail_at = static_cast<long>(in.below(64));
        faulty = true;
    }
    if (in.chance(16))
    {
        window = in.range(1, 40);
        faulty = true;
    }

    const Expected e = read_machine();
    fuzz::say("root 0x%llx, the reader says %s", (ull)root_at, e.ok ? "a machine" : "no machine");

    auto& acpi = Acpi::get_instance();
    stdlib::Error err = acpi.parse();
    fuzz::check_no_locks_held("the parse");

    /* Whichever way it ended: the tables let go of, and nothing of the
       window held but the APICs' pages -- and not those after a failure */
    bool table_kept = acpi.root_ != nullptr;
    for (auto* t : acpi.table_)
        table_kept |= t != nullptr;
    INVARIANT(!table_kept, "a table still mapped when the parse is done");
    if (err.ok())
    {
        INVARIANT(acpi.get_lapic_address() != nullptr && acpi.get_io_apic_address() != nullptr,
            "a parse that succeeds with no %s", acpi.get_lapic_address() ? "IO-APIC" : "local APIC");
        phys_of(acpi.get_lapic_address());
        phys_of(acpi.get_io_apic_address());
    }
    else
    {
        INVARIANT(acpi.get_lapic_address() == nullptr && acpi.get_io_apic_address() == nullptr,
            "an APIC's page kept by a parse that failed");
    }
    INVARIANT(held == (err.ok() ? 2u : 0u), "%zu slots of the window held after the parse, where %u are the APICs'",
        held, err.ok() ? 2u : 0u);

    if (faulty)
    {
        fuzz::reached(refused ? "the window refused" : "a window that had room after all");
        return;
    }

    INVARIANT(err.ok() == e.ok, "the parse says %s where the machine is %s", err.ok() ? "yes" : "no",
        e.ok ? "whole" : "not one to boot");
    if (!e.ok)
    {
        fuzz::reached("no machine");
        return;
    }

    INVARIANT(phys_of(acpi.get_lapic_address()) == e.lapic, "the local APIC mapped at 0x%llx, not 0x%llx",
        (ull)phys_of(acpi.get_lapic_address()), (ull)e.lapic);
    INVARIANT(phys_of(acpi.get_io_apic_address()) == e.io_apic, "the IO-APIC mapped at 0x%llx, where the one to "
        "drive is at 0x%llx", (ull)phys_of(acpi.get_io_apic_address()), (ull)e.io_apic);
    INVARIANT(kernel::inserted == e.cpus, "%zu CPUs taken, where the MADT enables %zu", kernel::inserted.size(),
        e.cpus.size());
    for (u32 irq = 0; irq < 256; irq++)
    {
        u32 gsi = irq;
        u16 flags = 0;
        for (auto& o : e.overrides)
        {
            if (o.irq == irq)
            {
                gsi = o.gsi;
                flags = o.flags;
                break;
            }
        }
        const u8 i = static_cast<u8>(irq);
        INVARIANT(acpi.get_gsi_by_irq(i) == gsi && acpi.get_irq_flags(i) == flags,
            "irq %u goes to gsi %u flags 0x%x, not %u 0x%x", irq, acpi.get_gsi_by_irq(i), acpi.get_irq_flags(i), gsi, flags);
    }
    INVARIANT(acpi.get_pm1a_cnt_port() == e.pm1a, "PM1a at 0x%lx, not 0x%lx", acpi.get_pm1a_cnt_port(), e.pm1a);
    INVARIANT(acpi.has_reset_reg() == e.reset && acpi.get_reset_reg_port() == e.reset_port &&
        acpi.get_reset_value() == e.reset_value, "the reset register is not the FADT's");
    INVARIANT(acpi.get_century_register() == e.century, "the century register is %u, not %u",
        acpi.get_century_register(), e.century);
    INVARIANT(acpi.get_hpet_base_phys() == e.hpet_base && acpi.get_hpet_min_tick() == e.hpet_tick,
        "the HPET at 0x%lx tick %u, not 0x%lx %u", acpi.get_hpet_base_phys(), acpi.get_hpet_min_tick(), e.hpet_base,
        e.hpet_tick);
    INVARIANT(acpi.has_firmware_watchdog() == e.wdat, "a firmware watchdog %s", e.wdat ? "missed" : "from nowhere");

    fuzz::reached("a machine");
    if (e.cpus.size() > 1)
        fuzz::reached("a machine of several CPUs");
    if (e.reset && e.wdat && e.hpet_base != 0)
        fuzz::reached("a machine with every table read");
    if (tables.size() > 20)
        fuzz::reached("a firmware of many tables");
}

void reset()
{
    while (!maps.empty())
        release(maps.begin());
    held = 0;
    window = WINDOW_SLOTS;
    fail_at = -1;
    map_calls = 0;
    refused = false;
    phys.clear();
    kernel::inserted.clear();
    kernel::grub::rsdp.clear();
    auto& acpi = Acpi::get_instance();
    acpi.~Acpi();
    new (&acpi) Acpi();
}

}

const fuzz::Target fuzz::THE_TARGET = {"acpi", reset, run, 4096, 20000};
