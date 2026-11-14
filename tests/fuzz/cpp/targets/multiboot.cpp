// The Multiboot2 information (arch/x86_64/grub.cpp): what GRUB hands the x86
// kernel before anything else runs -- the firmware's memory map, the command
// line, a copy of the ACPI RSDP, the framebuffer, the microcode module -- in
// a buffer of tags whose every size and count is GRUB's, or the firmware's,
// to say. An info is built from the input tag by tag, as GRUB lays one out --
// the tags GRUB gives and ones it may, of every size a tag of their kind can
// be -- and damaged or not: a total size that lies, tags that lie about
// theirs, no end tag, bytes of anything. A sound one is held to a reader of
// the info written from grub.h and grub.cpp: the memory map's regions --
// reserved ones first, then usable RAM while there is room -- the line kept,
// the RSDP copied (the new one over the old), the framebuffer, the last
// `ucode` module whose name ends inside its tag and whose range is not empty;
// any one to nothing read past it.
#include "host.h"

/* The loader's own file, for its statics, which Reset puts back */
#define private public
#include <arch/x86_64/grub.cpp>
#include <mm/memory_map.h>
#undef private

#include <kernel/parameters.h>
#include <kernel/ubsan.h>

#include "fuzz.h"

#include <new>

using namespace kernel;
using kernel::mm::MemoryMap;

namespace kernel
{
namespace ubsan
{

void set_warn_only(bool warn_only)
{
    (void)warn_only;
}

}
}

namespace
{

/* ---- the info ---- */

void put(std::vector<uint8_t>& v, size_t at, uint64_t x, int bytes)
{
    if (v.size() < at + bytes)
        v.resize(at + bytes, 0);
    for (int i = 0; i < bytes; i++)
        v[at + i] = static_cast<uint8_t>(x >> (8 * i));
}

struct MmapEntry
{
    u64 addr;
    u64 len;
    u32 type;
};

struct Fb
{
    u64 addr;
    u32 pitch, width, height;
    u8 bpp, type;
    u8 colors[6];
};

/* What a sound info holds, tag by tag, as the model reads it */
struct Tag
{
    u32 type;
    std::vector<uint8_t> body; /* after the 8-byte header */
};

u64 address(fuzz::Input& in)
{
    switch (in.u8() % 8)
    {
    case 0:
        return in.value64();
    case 1:
        return (1ULL << 52) - in.u8() * consts::PAGE_SIZE;
    default:
        return in.below(1ULL << 36) & ~0xFFFULL;
    }
}

/* Regions a whole info may carry: fewer than the map holds -- but for an
   info of one long map, which has more, and of them no more reserved ones
   than the map holds with the kernel's own: its usable RAM past that is
   left out, and a sound info never reaches the reserved regions' limit */
const size_t ENTRY_BUDGET = MemoryMap::MAX_REGIONS - 8;
const size_t LONG_BUDGET = MemoryMap::MAX_REGIONS + 64;
const size_t RESERVED_BUDGET = MemoryMap::MAX_REGIONS - grub::KERNEL_CARVE_OUTS;

Tag mmap_tag(fuzz::Input& in, std::vector<MmapEntry>& entries, size_t& budget, size_t& reserved, bool long_map)
{
    u32 entry_size = in.chance(24) ? 24 + 8 * static_cast<u32>(in.below(3)) : 24;
    if (in.chance(8))
        entry_size = static_cast<u32>(in.below(24));
    Tag t{6, {}};
    put(t.body, 0, entry_size, 4);
    put(t.body, 4, 0, 4);
    u64 want = long_map ? in.range(MemoryMap::MAX_REGIONS - 8, LONG_BUDGET) : in.chance(16) ? in.range(60, 140) : in.below(24);
    for (u64 n = std::min<u64>(budget, want); n > 0; n--, budget--)
    {
        MmapEntry e{address(in), in.chance(16) ? in.value64() : (1 + in.below(4096)) * consts::PAGE_SIZE,
                    in.chance(128) ? 1u : static_cast<u32>(in.range(2, 6))};
        if (e.type != 1 && reserved-- == 0)
        {
            reserved = 0;
            e.type = 1;
        }
        size_t at = t.body.size();
        put(t.body, at, e.addr, 8);
        put(t.body, at + 8, e.len, 8);
        put(t.body, at + 16, e.type, 4);
        put(t.body, at + 20, 0, 4);
        if (entry_size > 24)
            t.body.resize(at + entry_size, 0);
        entries.push_back(e);
    }
    return t;
}

Tag cmdline_tag(fuzz::Input& in)
{
    Tag t{1, {}};
    static const char* const LINES[] = {"root=auto dhcp=auto", "console=serial loglevel=3", "ro root=vda1",
                                        "netconsole=10.0.2.2:6666 nctail=64", ""};
    std::string line = in.pick(LINES);
    for (int i = in.below(8); i > 0; i--)
        line += static_cast<char>(in.chance(16) ? in.range(1, 255) : 'a' + in.below(26));
    t.body.assign(line.begin(), line.end());
    if (!in.chance(24))
        t.body.push_back(0);
    return t;
}

Tag rsdp_tag(fuzz::Input& in, bool is_new)
{
    Tag t{is_new ? 15u : 14u, {}};
    size_t len = in.chance(32) ? in.below(80) : (is_new ? 36 : 20);
    t.body = fuzz::noise(in.u32(), len);
    return t;
}

/* A module: its range, of any extent or none, and its command line --
   `ucode`, what the kernel takes, another name, or one cut short with no
   NUL inside the tag */
Tag module_tag(fuzz::Input& in)
{
    Tag t{3, {}};
    u32 start = in.chance(8) ? in.u32() : static_cast<u32>(in.below(1ULL << 32) & ~0xFFFULL);
    u32 end = in.chance(8) ? start - static_cast<u32>(in.below(2)) : start + static_cast<u32>(in.below(1 << 20));
    put(t.body, 0, start, 4);
    put(t.body, 4, end, 4);
    static const char* const NAMES[] = {"ucode", "ucode", "ucode", "", "ucod", "ucode2", "initrd"};
    std::string name = in.pick(NAMES);
    for (char c : name)
        t.body.push_back(static_cast<uint8_t>(c));
    if (!in.chance(16))
        t.body.push_back(0);
    return t;
}

Tag fb_tag(fuzz::Input& in)
{
    Tag t{8, {}};
    put(t.body, 0, in.chance(32) ? in.value64() : 0xFD000000ULL, 8);
    put(t.body, 8, in.chance(32) ? in.value32() : 4096, 4);
    put(t.body, 12, in.chance(32) ? in.value32() : 1024, 4);
    put(t.body, 16, in.chance(32) ? in.value32() : 768, 4);
    put(t.body, 20, in.chance(32) ? in.u8() : 32, 1);
    put(t.body, 21, in.below(4), 1);
    put(t.body, 22, 0, 2);
    for (int i = 0; i < 6; i++)
        put(t.body, 24 + i, in.u8(), 1);
    /* The common part only, the RGB fields, an indexed palette, or cut */
    switch (in.u8() % 4)
    {
    case 0:
        t.body.resize(24);
        break;
    case 1:
        t.body.resize(24 + in.below(6));
        break;
    default:
        break;
    }
    return t;
}

std::vector<uint8_t> serialize(const std::vector<Tag>& tags, bool end)
{
    std::vector<uint8_t> info(8, 0);
    for (const Tag& t : tags)
    {
        size_t at = info.size();
        put(info, at, t.type, 4);
        put(info, at + 4, 8 + t.body.size(), 4);
        info.insert(info.end(), t.body.begin(), t.body.end());
        while (info.size() % 8)
            info.push_back(0);
    }
    if (end)
    {
        put(info, info.size(), 0, 4);
        put(info, info.size(), 8, 4);
    }
    put(info, 0, info.size(), 4);
    return info;
}

/* ---- the model ---- */

struct Region
{
    ulong addr, len, type;
};

struct Expected
{
    std::vector<Region> map;
    bool line = false;
    std::string kept;
    std::vector<uint8_t> rsdp;
    bool rsdp_new = false;
    bool fb_present = false;
    Fb framebuffer{};
    bool fb_colors = false;
    bool ucode = false;
    u32 ucode_start = 0, ucode_end = 0;
};

/* add_region, as memory_map.h says it keeps a region */
void add(Expected& e, u64 addr, u64 len, u64 type)
{
    const u64 top = MemoryMap::MAX_PHYS_ADDR;
    if (len == 0 || addr >= top)
        return;
    if (len > top - addr)
        len = top - addr;
    INVARIANT(e.map.size() < MemoryMap::MAX_REGIONS, "the model's map overflows: the generator makes too many regions");
    e.map.push_back({addr, len, type});
}

void read_mmap(Expected& e, const std::vector<MmapEntry>& entries)
{
    for (int pass = 0; pass < 2; pass++)
    {
        for (const MmapEntry& m : entries)
        {
            bool usable = m.type == 1;
            if (usable != (pass == 1))
                continue;
            if (usable && e.map.size() + grub::KERNEL_CARVE_OUTS >= MemoryMap::MAX_REGIONS)
            {
                fuzz::reached("usable RAM past what the map holds");
                continue;
            }
            add(e, m.addr, m.len, m.type);
        }
    }
}

std::string kept(const std::string& line)
{
    const size_t keep = Parameters::CMDLINE_LEN - 1;
    std::string kept = line.substr(0, keep);
    if (line.size() > keep && line[keep] != ' ')
    {
        size_t space = kept.rfind(' ');
        kept = (space == std::string::npos) ? std::string() : kept.substr(0, space + 1);
    }
    return kept;
}

void run(fuzz::Input& in)
{
    std::vector<Tag> tags;
    std::vector<std::vector<MmapEntry>> mmaps;
    const bool long_map = in.chance(16);
    size_t budget = long_map ? LONG_BUDGET : ENTRY_BUDGET;
    size_t reserved = RESERVED_BUDGET;
    Expected e;
    for (int n = in.below(10); n > 0; n--)
    {
        switch (in.u8() % 9)
        {
        case 0:
        case 1:
        {
            /* One long map to an info: a second's reserved regions could
               find the table full */
            if (long_map && !mmaps.empty())
                break;
            mmaps.emplace_back();
            tags.push_back(mmap_tag(in, mmaps.back(), budget, reserved, long_map));
            break;
        }
        case 2:
            tags.push_back(cmdline_tag(in));
            break;
        case 3:
            tags.push_back(rsdp_tag(in, false));
            break;
        case 4:
            tags.push_back(rsdp_tag(in, true));
            break;
        case 5:
            tags.push_back(fb_tag(in));
            break;
        case 6:
        {
            Tag t{5, {}};
            t.body = fuzz::noise(in.u32(), in.chance(32) ? in.below(12) : 12);
            tags.push_back(t);
            break;
        }
        case 7:
            tags.push_back(module_tag(in));
            break;
        default:
        {
            /* A tag this kernel does not read: a name, ELF sections -- or a
               module whose body is anything */
            static const u32 OTHERS[] = {2, 3, 4, 9, 10, 21};
            Tag t{static_cast<u32>(in.chance(32) ? in.range(16, 40) : in.pick(OTHERS)), {}};
            t.body = fuzz::noise(in.u32(), in.below(64));
            tags.push_back(t);
            break;
        }
        }
    }
    const bool end = !in.chance(16);
    std::vector<uint8_t> info = serialize(tags, end);

    const bool damaged = in.chance(64);
    if (damaged)
    {
        for (int n = 1 + in.below(3); n > 0; n--)
        {
            switch (in.u8() % 4)
            {
            case 0:
                /* The total size that lies */
                put(info, 0, in.chance(32) ? in.below(16) : in.below(info.size() + 64), 4);
                break;
            case 1:
            {
                /* A tag size that lies */
                size_t at = 8 + 8 * in.below((info.size() - 8) / 8);
                put(info, at + 4, in.value32(), 4);
                break;
            }
            case 2:
                info[in.below(info.size())] = in.u8();
                break;
            default:
                info.resize(8 + in.below(info.size() - 7));
                break;
            }
        }
        fuzz::reached("a damaged info");
    }

    /* The info as long as its header says, never less than a header: GRUB's
       buffer is RAM, and what is past the info is there to read */
    u32 said = 0;
    memcpy(&said, info.data(), 4);
    size_t size = std::max<size_t>(std::max<size_t>(info.size(), 8), said);
    std::vector<u64> buffer((size + 7) / 8, 0);
    memcpy(buffer.data(), info.data(), info.size());

    grub::parse_multi_boot_info(reinterpret_cast<grub::MultiBootInfoHeader*>(buffer.data()));

    auto& mmap = MemoryMap::get_instance();
    INVARIANT(mmap.get_region_count() <= MemoryMap::MAX_REGIONS, "a map of %zu regions", mmap.get_region_count());
    size_t rsdp_size = 0;
    const void* rsdp = grub::get_acpi_rsdp(rsdp_size);
    INVARIANT((rsdp == nullptr) == (rsdp_size == 0) && rsdp_size <= sizeof(grub::acpi_rsdp_copy),
        "an RSDP of %zu bytes", rsdp_size);
    if (damaged)
        return;

    /* The model: the tags in order, as the reader takes each */
    size_t mmap_index = 0;
    for (const Tag& t : tags)
    {
        switch (t.type)
        {
        case 6:
        {
            const std::vector<MmapEntry>& entries = mmaps[mmap_index++];
            u32 entry_size;
            memcpy(&entry_size, t.body.data(), 4);
            if (entry_size < 24)
                break;
            /* The entries that fit whole, entry_size apart */
            std::vector<MmapEntry> fit;
            for (size_t k = 0; 8 + 8 + (k + 1) * entry_size <= 8 + t.body.size() && k < entries.size(); k++)
                fit.push_back(entries[k]);
            read_mmap(e, fit);
            fuzz::reached(fit.size() > 60 ? "a long memory map" : "a memory map");
            break;
        }
        case 1:
        {
            if (t.body.empty() || memchr(t.body.data(), 0, t.body.size()) == nullptr)
                break;
            e.line = true;
            e.kept = kept(reinterpret_cast<const char*>(t.body.data()));
            break;
        }
        case 14:
        case 15:
        {
            bool is_new = t.type == 15;
            if (!e.rsdp.empty() && e.rsdp_new && !is_new)
                break;
            if (t.body.empty())
                break;
            e.rsdp.assign(t.body.begin(), t.body.begin() + std::min<size_t>(t.body.size(), 64));
            e.rsdp_new = is_new;
            break;
        }
        case 3:
        {
            /* The module's range, then its name, which must end inside the
               tag; `ucode` with a range that is not empty is kept, the last
               such over any before */
            if (t.body.size() <= 8)
                break;
            const uint8_t* name = t.body.data() + 8;
            size_t room = t.body.size() - 8;
            const void* nul = memchr(name, 0, room);
            if (nul == nullptr)
                break;
            if (strcmp(reinterpret_cast<const char*>(name), "ucode") != 0)
                break;
            u32 start, end;
            memcpy(&start, t.body.data(), 4);
            memcpy(&end, t.body.data() + 4, 4);
            if (end <= start)
                break;
            e.ucode = true;
            e.ucode_start = start;
            e.ucode_end = end;
            fuzz::reached("a ucode module");
            break;
        }
        case 8:
        {
            if (t.body.size() < 24)
                break;
            e.fb_present = true;
            memcpy(&e.framebuffer.addr, &t.body[0], 8);
            memcpy(&e.framebuffer.pitch, &t.body[8], 4);
            memcpy(&e.framebuffer.width, &t.body[12], 4);
            memcpy(&e.framebuffer.height, &t.body[16], 4);
            e.framebuffer.bpp = t.body[20];
            e.framebuffer.type = t.body[21];
            if (e.framebuffer.type == 1)
            {
                static const u8 DEFAULTS[6] = {16, 8, 8, 8, 0, 8};
                e.fb_colors = true;
                if (t.body.size() >= 30)
                    memcpy(e.framebuffer.colors, &t.body[24], 6);
                else
                    memcpy(e.framebuffer.colors, DEFAULTS, 6);
            }
            break;
        }
        default:
            break;
        }
    }

    INVARIANT(mmap.get_region_count() == e.map.size(), "the map holds %zu regions, where the info says %zu",
        mmap.get_region_count(), e.map.size());
    for (size_t i = 0; i < e.map.size(); i++)
    {
        ulong addr, len, type;
        mmap.get_region(i, addr, len, type);
        INVARIANT(addr == e.map[i].addr && len == e.map[i].len && type == e.map[i].type,
            "region %zu is 0x%lx+0x%lx type %lu, not 0x%lx+0x%lx type %lu", i, addr, len, type, e.map[i].addr,
            e.map[i].len, e.map[i].type);
    }
    if (e.line)
        INVARIANT(e.kept == Parameters::get_instance().get_cmdline(), "the line kept is '%s', not '%s'",
            Parameters::get_instance().get_cmdline(), e.kept.c_str());
    INVARIANT(rsdp_size == e.rsdp.size() && (rsdp_size == 0 || memcmp(rsdp, e.rsdp.data(), rsdp_size) == 0),
        "the RSDP kept is %zu bytes, not the %zu of the tag it should be", rsdp_size, e.rsdp.size());
    ulong ucode_phys = 0, ucode_len = 0;
    bool ucode = grub::get_ucode_module(ucode_phys, ucode_len);
    INVARIANT(ucode == e.ucode && (!ucode || (ucode_phys == e.ucode_start && ucode_len == u64(e.ucode_end) - e.ucode_start)),
        "the ucode module kept is %s 0x%lx+0x%lx, where the info's is %s 0x%x-0x%x", ucode ? "" : "none,", ucode_phys,
        ucode_len, e.ucode ? "" : "none,", e.ucode_start, e.ucode_end);
    INVARIANT(grub::has_framebuffer_info() == e.fb_present, "a framebuffer %s", e.fb_present ? "lost" : "from nowhere");
    if (e.fb_present)
    {
        const grub::FramebufferInfo* fb = grub::get_framebuffer_info();
        INVARIANT(fb->addr == e.framebuffer.addr && fb->pitch == e.framebuffer.pitch && fb->width == e.framebuffer.width &&
            fb->height == e.framebuffer.height && fb->bpp == e.framebuffer.bpp && fb->type == e.framebuffer.type,
            "the framebuffer is not the tag's");
        if (e.fb_colors)
            INVARIANT(fb->red_pos == e.framebuffer.colors[0] && fb->red_size == e.framebuffer.colors[1] &&
                fb->green_pos == e.framebuffer.colors[2] && fb->green_size == e.framebuffer.colors[3] &&
                fb->blue_pos == e.framebuffer.colors[4] && fb->blue_size == e.framebuffer.colors[5],
                "the framebuffer's colours are not the tag's");
        fuzz::reached("a framebuffer");
    }
    fuzz::reached(end ? "a sound info" : "a sound info with no end tag");
}

void reset()
{
    MemoryMap& mmap = MemoryMap::get_instance();
    mmap.~MemoryMap();
    new (&mmap) MemoryMap();
    Parameters& p = Parameters::get_instance();
    p.~Parameters();
    new (&p) Parameters();
    memset(grub::acpi_rsdp_copy, 0, sizeof(grub::acpi_rsdp_copy));
    grub::acpi_rsdp_size = 0;
    grub::acpi_rsdp_is_new = false;
    grub::framebuffer_present = false;
    grub::framebuffer_type = 0;
    grub::framebuffer = grub::FramebufferInfo();
    grub::ucode_start = 0;
    grub::ucode_end = 0;
}

}

const fuzz::Target fuzz::THE_TARGET = {"multiboot", reset, run, 4096, 20000};
