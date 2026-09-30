// The Multiboot2 information (arch/x86_64/grub.cpp): what GRUB hands the x86
// kernel before anything else runs -- the firmware's memory map, the command
// line, a copy of the ACPI RSDP, the framebuffer -- in a buffer of tags whose
// every size and count is GRUB's, or the firmware's, to say. An info is built
// from the input tag by tag, as GRUB lays one out -- the tags GRUB gives and
// ones it may, of every size a tag of their kind can be -- and damaged or not:
// a total size that lies, tags that lie about theirs, no end tag, bytes of
// anything. A sound one is held to a reader of the info written from
// grub.h and grub.cpp: the memory map's regions -- reserved ones first, then
// usable RAM while there is room -- the line kept, the RSDP copied (the new
// one over the old), the framebuffer; any one to nothing read past it.
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

using namespace Kernel;
using Kernel::Mm::MemoryMap;

namespace Kernel
{
namespace Ubsan
{

void SetWarnOnly(bool warnOnly)
{
    (void)warnOnly;
}

}
}

namespace
{

/* ---- the info ---- */

void Put(std::vector<uint8_t>& v, size_t at, uint64_t x, int bytes)
{
    if (v.size() < at + bytes)
        v.resize(at + bytes, 0);
    for (int i = 0; i < bytes; i++)
        v[at + i] = static_cast<uint8_t>(x >> (8 * i));
}

struct MmapEntry
{
    u64 Addr;
    u64 Len;
    u32 Type;
};

struct Fb
{
    u64 Addr;
    u32 Pitch, Width, Height;
    u8 Bpp, Type;
    u8 Colors[6];
};

/* What a sound info holds, tag by tag, as the model reads it */
struct Tag
{
    u32 Type;
    std::vector<uint8_t> Body; /* after the 8-byte header */
};

u64 Address(Fuzz::Input& in)
{
    switch (in.U8() % 8)
    {
    case 0:
        return in.Value64();
    case 1:
        return (1ULL << 52) - in.U8() * Const::PageSize;
    default:
        return in.Below(1ULL << 36) & ~0xFFFULL;
    }
}

/* Regions a whole info may carry: fewer than the map holds -- but for an
   info of one long map, which has more, and of them no more reserved ones
   than the map holds with the kernel's own: its usable RAM past that is
   left out, and a sound info never reaches the reserved regions' limit */
const size_t EntryBudget = MemoryMap::MaxRegions - 8;
const size_t LongBudget = MemoryMap::MaxRegions + 64;
const size_t ReservedBudget = MemoryMap::MaxRegions - Grub::KernelCarveOuts;

Tag MmapTag(Fuzz::Input& in, std::vector<MmapEntry>& entries, size_t& budget, size_t& reserved, bool longMap)
{
    u32 entrySize = in.Chance(24) ? 24 + 8 * static_cast<u32>(in.Below(3)) : 24;
    if (in.Chance(8))
        entrySize = static_cast<u32>(in.Below(24));
    Tag t{6, {}};
    Put(t.Body, 0, entrySize, 4);
    Put(t.Body, 4, 0, 4);
    u64 want = longMap ? in.Range(MemoryMap::MaxRegions - 8, LongBudget) : in.Chance(16) ? in.Range(60, 140) : in.Below(24);
    for (u64 n = std::min<u64>(budget, want); n > 0; n--, budget--)
    {
        MmapEntry e{Address(in), in.Chance(16) ? in.Value64() : (1 + in.Below(4096)) * Const::PageSize,
                    in.Chance(128) ? 1u : static_cast<u32>(in.Range(2, 6))};
        if (e.Type != 1 && reserved-- == 0)
        {
            reserved = 0;
            e.Type = 1;
        }
        size_t at = t.Body.size();
        Put(t.Body, at, e.Addr, 8);
        Put(t.Body, at + 8, e.Len, 8);
        Put(t.Body, at + 16, e.Type, 4);
        Put(t.Body, at + 20, 0, 4);
        if (entrySize > 24)
            t.Body.resize(at + entrySize, 0);
        entries.push_back(e);
    }
    return t;
}

Tag CmdlineTag(Fuzz::Input& in)
{
    Tag t{1, {}};
    static const char* const Lines[] = {"root=auto dhcp=auto", "console=serial loglevel=3", "ro root=vda1",
                                        "netconsole=10.0.2.2:6666 nctail=64", ""};
    std::string line = in.Pick(Lines);
    for (int i = in.Below(8); i > 0; i--)
        line += static_cast<char>(in.Chance(16) ? in.Range(1, 255) : 'a' + in.Below(26));
    t.Body.assign(line.begin(), line.end());
    if (!in.Chance(24))
        t.Body.push_back(0);
    return t;
}

Tag RsdpTag(Fuzz::Input& in, bool isNew)
{
    Tag t{isNew ? 15u : 14u, {}};
    size_t len = in.Chance(32) ? in.Below(80) : (isNew ? 36 : 20);
    t.Body = Fuzz::Noise(in.U32(), len);
    return t;
}

Tag FbTag(Fuzz::Input& in)
{
    Tag t{8, {}};
    Put(t.Body, 0, in.Chance(32) ? in.Value64() : 0xFD000000ULL, 8);
    Put(t.Body, 8, in.Chance(32) ? in.Value32() : 4096, 4);
    Put(t.Body, 12, in.Chance(32) ? in.Value32() : 1024, 4);
    Put(t.Body, 16, in.Chance(32) ? in.Value32() : 768, 4);
    Put(t.Body, 20, in.Chance(32) ? in.U8() : 32, 1);
    Put(t.Body, 21, in.Below(4), 1);
    Put(t.Body, 22, 0, 2);
    for (int i = 0; i < 6; i++)
        Put(t.Body, 24 + i, in.U8(), 1);
    /* The common part only, the RGB fields, an indexed palette, or cut */
    switch (in.U8() % 4)
    {
    case 0:
        t.Body.resize(24);
        break;
    case 1:
        t.Body.resize(24 + in.Below(6));
        break;
    default:
        break;
    }
    return t;
}

std::vector<uint8_t> Serialize(const std::vector<Tag>& tags, bool end)
{
    std::vector<uint8_t> info(8, 0);
    for (const Tag& t : tags)
    {
        size_t at = info.size();
        Put(info, at, t.Type, 4);
        Put(info, at + 4, 8 + t.Body.size(), 4);
        info.insert(info.end(), t.Body.begin(), t.Body.end());
        while (info.size() % 8)
            info.push_back(0);
    }
    if (end)
    {
        Put(info, info.size(), 0, 4);
        Put(info, info.size(), 8, 4);
    }
    Put(info, 0, info.size(), 4);
    return info;
}

/* ---- the model ---- */

struct Region
{
    ulong Addr, Len, Type;
};

struct Expected
{
    std::vector<Region> Map;
    bool Line = false;
    std::string Kept;
    std::vector<uint8_t> Rsdp;
    bool RsdpNew = false;
    bool FbPresent = false;
    Fb Framebuffer{};
    bool FbColors = false;
};

/* AddRegion, as memory_map.h says it keeps a region */
void Add(Expected& e, u64 addr, u64 len, u64 type)
{
    const u64 top = MemoryMap::MaxPhysAddr;
    if (len == 0 || addr >= top)
        return;
    if (len > top - addr)
        len = top - addr;
    INVARIANT(e.Map.size() < MemoryMap::MaxRegions, "the model's map overflows: the generator makes too many regions");
    e.Map.push_back({addr, len, type});
}

void ReadMmap(Expected& e, const std::vector<MmapEntry>& entries)
{
    for (int pass = 0; pass < 2; pass++)
    {
        for (const MmapEntry& m : entries)
        {
            bool usable = m.Type == 1;
            if (usable != (pass == 1))
                continue;
            if (usable && e.Map.size() + Grub::KernelCarveOuts >= MemoryMap::MaxRegions)
            {
                Fuzz::Reached("usable RAM past what the map holds");
                continue;
            }
            Add(e, m.Addr, m.Len, m.Type);
        }
    }
}

std::string Kept(const std::string& line)
{
    const size_t keep = Parameters::CmdlineLen - 1;
    std::string kept = line.substr(0, keep);
    if (line.size() > keep && line[keep] != ' ')
    {
        size_t space = kept.rfind(' ');
        kept = (space == std::string::npos) ? std::string() : kept.substr(0, space + 1);
    }
    return kept;
}

void Run(Fuzz::Input& in)
{
    std::vector<Tag> tags;
    std::vector<std::vector<MmapEntry>> mmaps;
    const bool longMap = in.Chance(16);
    size_t budget = longMap ? LongBudget : EntryBudget;
    size_t reserved = ReservedBudget;
    Expected e;
    for (int n = in.Below(10); n > 0; n--)
    {
        switch (in.U8() % 8)
        {
        case 0:
        case 1:
        {
            /* One long map to an info: a second's reserved regions could
               find the table full */
            if (longMap && !mmaps.empty())
                break;
            mmaps.emplace_back();
            tags.push_back(MmapTag(in, mmaps.back(), budget, reserved, longMap));
            break;
        }
        case 2:
            tags.push_back(CmdlineTag(in));
            break;
        case 3:
            tags.push_back(RsdpTag(in, false));
            break;
        case 4:
            tags.push_back(RsdpTag(in, true));
            break;
        case 5:
            tags.push_back(FbTag(in));
            break;
        case 6:
        {
            Tag t{5, {}};
            t.Body = Fuzz::Noise(in.U32(), in.Chance(32) ? in.Below(12) : 12);
            tags.push_back(t);
            break;
        }
        default:
        {
            /* A tag this kernel does not read: a name, a module, ELF sections */
            static const u32 Others[] = {2, 3, 4, 9, 10, 21};
            Tag t{static_cast<u32>(in.Chance(32) ? in.Range(16, 40) : in.Pick(Others)), {}};
            t.Body = Fuzz::Noise(in.U32(), in.Below(64));
            tags.push_back(t);
            break;
        }
        }
    }
    const bool end = !in.Chance(16);
    std::vector<uint8_t> info = Serialize(tags, end);

    const bool damaged = in.Chance(64);
    if (damaged)
    {
        for (int n = 1 + in.Below(3); n > 0; n--)
        {
            switch (in.U8() % 4)
            {
            case 0:
                /* The total size that lies */
                Put(info, 0, in.Chance(32) ? in.Below(16) : in.Below(info.size() + 64), 4);
                break;
            case 1:
            {
                /* A tag size that lies */
                size_t at = 8 + 8 * in.Below((info.size() - 8) / 8);
                Put(info, at + 4, in.Value32(), 4);
                break;
            }
            case 2:
                info[in.Below(info.size())] = in.U8();
                break;
            default:
                info.resize(8 + in.Below(info.size() - 7));
                break;
            }
        }
        Fuzz::Reached("a damaged info");
    }

    /* The info as long as its header says, never less than a header: GRUB's
       buffer is RAM, and what is past the info is there to read */
    u32 said = 0;
    memcpy(&said, info.data(), 4);
    size_t size = std::max<size_t>(std::max<size_t>(info.size(), 8), said);
    std::vector<u64> buffer((size + 7) / 8, 0);
    memcpy(buffer.data(), info.data(), info.size());

    Grub::ParseMultiBootInfo(reinterpret_cast<Grub::MultiBootInfoHeader*>(buffer.data()));

    auto& mmap = MemoryMap::GetInstance();
    INVARIANT(mmap.GetRegionCount() <= MemoryMap::MaxRegions, "a map of %zu regions", mmap.GetRegionCount());
    size_t rsdpSize = 0;
    const void* rsdp = Grub::GetAcpiRsdp(rsdpSize);
    INVARIANT((rsdp == nullptr) == (rsdpSize == 0) && rsdpSize <= sizeof(Grub::AcpiRsdpCopy),
        "an RSDP of %zu bytes", rsdpSize);
    if (damaged)
        return;

    /* The model: the tags in order, as the reader takes each */
    size_t mmapIndex = 0;
    for (const Tag& t : tags)
    {
        switch (t.Type)
        {
        case 6:
        {
            const std::vector<MmapEntry>& entries = mmaps[mmapIndex++];
            u32 entrySize;
            memcpy(&entrySize, t.Body.data(), 4);
            if (entrySize < 24)
                break;
            /* The entries that fit whole, entrySize apart */
            std::vector<MmapEntry> fit;
            for (size_t k = 0; 8 + 8 + (k + 1) * entrySize <= 8 + t.Body.size() && k < entries.size(); k++)
                fit.push_back(entries[k]);
            ReadMmap(e, fit);
            Fuzz::Reached(fit.size() > 60 ? "a long memory map" : "a memory map");
            break;
        }
        case 1:
        {
            if (t.Body.empty() || memchr(t.Body.data(), 0, t.Body.size()) == nullptr)
                break;
            e.Line = true;
            e.Kept = Kept(reinterpret_cast<const char*>(t.Body.data()));
            break;
        }
        case 14:
        case 15:
        {
            bool isNew = t.Type == 15;
            if (!e.Rsdp.empty() && e.RsdpNew && !isNew)
                break;
            if (t.Body.empty())
                break;
            e.Rsdp.assign(t.Body.begin(), t.Body.begin() + std::min<size_t>(t.Body.size(), 64));
            e.RsdpNew = isNew;
            break;
        }
        case 8:
        {
            if (t.Body.size() < 24)
                break;
            e.FbPresent = true;
            memcpy(&e.Framebuffer.Addr, &t.Body[0], 8);
            memcpy(&e.Framebuffer.Pitch, &t.Body[8], 4);
            memcpy(&e.Framebuffer.Width, &t.Body[12], 4);
            memcpy(&e.Framebuffer.Height, &t.Body[16], 4);
            e.Framebuffer.Bpp = t.Body[20];
            e.Framebuffer.Type = t.Body[21];
            if (e.Framebuffer.Type == 1)
            {
                static const u8 Defaults[6] = {16, 8, 8, 8, 0, 8};
                e.FbColors = true;
                if (t.Body.size() >= 30)
                    memcpy(e.Framebuffer.Colors, &t.Body[24], 6);
                else
                    memcpy(e.Framebuffer.Colors, Defaults, 6);
            }
            break;
        }
        default:
            break;
        }
    }

    INVARIANT(mmap.GetRegionCount() == e.Map.size(), "the map holds %zu regions, where the info says %zu",
        mmap.GetRegionCount(), e.Map.size());
    for (size_t i = 0; i < e.Map.size(); i++)
    {
        ulong addr, len, type;
        mmap.GetRegion(i, addr, len, type);
        INVARIANT(addr == e.Map[i].Addr && len == e.Map[i].Len && type == e.Map[i].Type,
            "region %zu is 0x%lx+0x%lx type %lu, not 0x%lx+0x%lx type %lu", i, addr, len, type, e.Map[i].Addr,
            e.Map[i].Len, e.Map[i].Type);
    }
    if (e.Line)
        INVARIANT(e.Kept == Parameters::GetInstance().GetCmdline(), "the line kept is '%s', not '%s'",
            Parameters::GetInstance().GetCmdline(), e.Kept.c_str());
    INVARIANT(rsdpSize == e.Rsdp.size() && (rsdpSize == 0 || memcmp(rsdp, e.Rsdp.data(), rsdpSize) == 0),
        "the RSDP kept is %zu bytes, not the %zu of the tag it should be", rsdpSize, e.Rsdp.size());
    INVARIANT(Grub::HasFramebufferInfo() == e.FbPresent, "a framebuffer %s", e.FbPresent ? "lost" : "from nowhere");
    if (e.FbPresent)
    {
        const Grub::FramebufferInfo* fb = Grub::GetFramebufferInfo();
        INVARIANT(fb->Addr == e.Framebuffer.Addr && fb->Pitch == e.Framebuffer.Pitch && fb->Width == e.Framebuffer.Width &&
            fb->Height == e.Framebuffer.Height && fb->Bpp == e.Framebuffer.Bpp && fb->Type == e.Framebuffer.Type,
            "the framebuffer is not the tag's");
        if (e.FbColors)
            INVARIANT(fb->RedPos == e.Framebuffer.Colors[0] && fb->RedSize == e.Framebuffer.Colors[1] &&
                fb->GreenPos == e.Framebuffer.Colors[2] && fb->GreenSize == e.Framebuffer.Colors[3] &&
                fb->BluePos == e.Framebuffer.Colors[4] && fb->BlueSize == e.Framebuffer.Colors[5],
                "the framebuffer's colours are not the tag's");
        Fuzz::Reached("a framebuffer");
    }
    Fuzz::Reached(end ? "a sound info" : "a sound info with no end tag");
}

void Reset()
{
    MemoryMap& mmap = MemoryMap::GetInstance();
    mmap.~MemoryMap();
    new (&mmap) MemoryMap();
    Parameters& p = Parameters::GetInstance();
    p.~Parameters();
    new (&p) Parameters();
    memset(Grub::AcpiRsdpCopy, 0, sizeof(Grub::AcpiRsdpCopy));
    Grub::AcpiRsdpSize = 0;
    Grub::AcpiRsdpIsNew = false;
    Grub::FramebufferPresent = false;
    Grub::FramebufferType = 0;
    Grub::Framebuffer = Grub::FramebufferInfo();
}

}

const Fuzz::Target Fuzz::TheTarget = {"multiboot", Reset, Run, 4096, 20000};
