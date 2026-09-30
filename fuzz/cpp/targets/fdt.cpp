// The device tree (arch/arm64/fdt.cpp) and what the arm64 boot makes of it
// (arch/arm64/board.cpp): the first thing the kernel reads, at a point where
// a fault is a boot that dies without a word, and whatever the firmware or
// the bootloader put there -- a tree of QEMU virt's shape, and that damaged.
// Every tree is built node by node from the input, serialized as a DTB, and
// handed to Board::Setup as the boot hands it. A tree serialized as it was
// built is one the model reads too: the board is held to what board.h says
// it takes of each node -- the parent's cells, the node's own property
// lengths, windows the linear map reaches, interrupts the kernel takes, the
// refusals counted -- and to the fallbacks for what it did not. A damaged one
// -- nesting that ends more nodes than it began, lengths and offsets that lie,
// a header that does, a blob cut short -- is held to what any board must be:
// nothing read past the blob (ASan), nothing the kernel acts on out of range.
#include "host.h"

/* The board is a singleton with a private constructor, which Reset runs
   again in place between two inputs. */
#define private public
#include <arch/arm64/board.h>
#include <arch/arm64/fdt.h>
#undef private

#include "fuzz.h"

#include <stdlib.h>
#include <string.h>

#include <algorithm>
#include <new>
#include <string>
#include <vector>

namespace
{

using Kernel::Board;
using Kernel::Fdt;

/* ---- the tree ---- */

struct Prop
{
    std::string Name;
    std::vector<uint8_t> Value;
};

struct Node
{
    std::string Name;
    std::vector<Prop> Props;
    std::vector<Node> Kids;

    const Prop* Find(const char* name) const
    {
        for (const Prop& p : Props)
        {
            if (p.Name == name)
                return &p;
        }
        return nullptr;
    }
};

void Put32(std::vector<uint8_t>& v, uint32_t x)
{
    v.push_back(static_cast<uint8_t>(x >> 24));
    v.push_back(static_cast<uint8_t>(x >> 16));
    v.push_back(static_cast<uint8_t>(x >> 8));
    v.push_back(static_cast<uint8_t>(x));
}

std::vector<uint8_t> Cells(std::initializer_list<uint32_t> cells)
{
    std::vector<uint8_t> v;
    for (uint32_t c : cells)
        Put32(v, c);
    return v;
}

/* A number in n cells, big-endian: the low 32 bits of each, as a tree
   holds it. */
void PutNumber(std::vector<uint8_t>& v, uint64_t x, uint32_t n)
{
    for (uint32_t i = n; i > 0; i--)
        Put32(v, (i > 2) ? 0 : static_cast<uint32_t>(x >> (32 * (i - 1))));
}

std::vector<uint8_t> Str(const std::string& s)
{
    std::vector<uint8_t> v(s.begin(), s.end());
    v.push_back(0);
    return v;
}

/* A compatible list: NUL-separated, NUL-terminated. */
std::vector<uint8_t> Strs(std::initializer_list<const char*> list)
{
    std::vector<uint8_t> v;
    for (const char* s : list)
    {
        std::vector<uint8_t> one = Str(s);
        v.insert(v.end(), one.begin(), one.end());
    }
    return v;
}

/* ---- the input's choices ---- */

struct Gen
{
    Fuzz::Input& In;

    /* A cell count: what trees have, and what they should not. */
    uint32_t CellCount(uint32_t usual)
    {
        switch (In.U8() % 12)
        {
        case 0:
            return 1;
        case 1:
            return 2;
        case 2:
            return static_cast<uint32_t>(In.Below(5));
        case 3:
            return In.Value32();
        default:
            return usual;
        }
    }

    uint64_t Address(uint64_t usual)
    {
        static const uint64_t Edges[] = {0, 1ULL << 47, (1ULL << 47) - 0x1000, 1ULL << 52, ~0ULL,
                                         0xFFFFFFFFULL, 0x4010000000ULL, 0x8000000000ULL};
        switch (In.U8() % 10)
        {
        case 0:
            return In.Pick(Edges);
        case 1:
            return In.Value64();
        default:
            return usual;
        }
    }

    uint64_t Size(uint64_t usual)
    {
        static const uint64_t Edges[] = {0, 1, 0x1000, 1ULL << 46, 1ULL << 47, ~0ULL};
        switch (In.U8() % 10)
        {
        case 0:
            return In.Pick(Edges);
        case 1:
            return In.Value64();
        default:
            return usual;
        }
    }

    /* reg = <address size>... in ac/sc cells, sometimes cut short. */
    std::vector<uint8_t> Reg(uint32_t ac, uint32_t sc, std::initializer_list<std::pair<uint64_t, uint64_t>> windows)
    {
        std::vector<uint8_t> v;
        for (auto& w : windows)
        {
            if (ac <= 4)
                PutNumber(v, Address(w.first), ac);
            if (sc <= 4)
                PutNumber(v, Size(w.second), sc);
        }
        Trim(v);
        return v;
    }

    /* A property's value cut, or grown, now and then. */
    void Trim(std::vector<uint8_t>& v)
    {
        if (In.Chance(12) && !v.empty())
            v.resize(In.Below(v.size()));
        else if (In.Chance(6))
            v.push_back(In.U8());
    }

    /* interrupts = <type number flags>..., mostly one the kernel takes. */
    std::vector<uint8_t> Interrupts(uint32_t type, uint32_t num, int count)
    {
        std::vector<uint8_t> v;
        for (int i = 0; i < count; i++)
        {
            uint32_t t = type, n = num + i;
            switch (In.U8() % 12)
            {
            case 0:
                t = static_cast<uint32_t>(In.Below(4));
                break;
            case 1:
                n = In.Value32();
                break;
            case 2:
                n = static_cast<uint32_t>(In.Range(200, 260));
                break;
            default:
                break;
            }
            Put32(v, t);
            Put32(v, n);
            Put32(v, 4);
        }
        Trim(v);
        return v;
    }

    std::string Text(size_t max)
    {
        size_t n = In.Chance(16) ? In.Range(400, 700) : In.Below(max);
        std::string s;
        for (size_t i = 0; i < n; i++)
            s += static_cast<char>(' ' + In.U8() % 95);
        return s;
    }

    /* A string property, now and then with no NUL at its end. */
    std::vector<uint8_t> String(const std::string& s)
    {
        std::vector<uint8_t> v = Str(s);
        if (In.Chance(12))
            v.pop_back();
        return v;
    }

    std::vector<uint8_t> Compatible(std::initializer_list<const char*> list)
    {
        std::vector<uint8_t> v = Strs(list);
        if (In.Chance(8))
            v.pop_back();
        return v;
    }
};

Node Tree(Gen& g)
{
    Fuzz::Input& in = g.In;
    Node root{"", {}, {}};
    uint32_t ac = g.CellCount(2), sc = g.CellCount(2);
    if (!in.Chance(8))
        root.Props.push_back({"#address-cells", Cells({ac})});
    else
        ac = 2; /* the walker's own for a root that says nothing */
    if (!in.Chance(8))
        root.Props.push_back({"#size-cells", Cells({sc})});
    else
        sc = 1;
    root.Props.push_back({"compatible", Strs({"linux,dummy-virt"})});

    for (int i = in.Below(3); i >= 0; i--)
        root.Kids.push_back({in.Chance(32) ? "memory" : "memory@40000000", {{"device_type", Str("memory")},
            {"reg", g.Reg(ac, sc, {{0x40000000, 0x40000000}, {0x100000000ULL, 0x40000000}})}}, {}});

    root.Kids.push_back({"chosen", {{"bootargs", g.String(g.Text(80))}}, {}});

    root.Kids.push_back({"psci", {{"compatible", g.Compatible({"arm,psci-1.0", "arm,psci-0.2", "arm,psci"})},
        {"method", g.String(in.Chance(64) ? "smc" : in.Chance(32) ? g.Text(6) : "hvc")}}, {}});

    Node intc{"intc@8000000", {{"compatible", g.Compatible({"arm,gic-v3"})},
        {"reg", g.Reg(ac, sc, {{0x08000000, 0x10000}, {0x080a0000, 0xf60000}})},
        {"#address-cells", Cells({2})}, {"#size-cells", Cells({2})}}, {}};
    if (!in.Chance(32))
        intc.Kids.push_back({"its@8080000", {{"compatible", g.Compatible({"arm,gic-v3-its"})},
            {"reg", g.Reg(2, 2, {{0x08080000, 0x20000}})}}, {}});
    root.Kids.push_back(intc);

    {
        std::vector<uint8_t> ranges;
        for (int e = in.Below(4); e > 0; e--)
        {
            static const uint32_t Spaces[] = {0x01000000, 0x02000000, 0x03000000, 0x43000000};
            Put32(ranges, in.Chance(32) ? in.U32() : in.Pick(Spaces));
            Put32(ranges, 0);
            Put32(ranges, 0);
            PutNumber(ranges, g.Address(0x10000000), 2);
            PutNumber(ranges, g.Size(0x2eff0000), 2);
        }
        g.Trim(ranges);
        std::vector<uint8_t> busRange = Cells({0, 0xff});
        if (in.Chance(24))
            busRange = Cells({in.Value32(), in.Value32()});
        g.Trim(busRange);
        root.Kids.push_back({"pcie@10000000", {{"compatible", g.Compatible({"pci-host-ecam-generic"})},
            {"reg", g.Reg(ac, sc, {{0x4010000000ULL, 0x10000000}})}, {"bus-range", busRange},
            {"ranges", ranges}, {"#address-cells", Cells({3})}, {"#size-cells", Cells({2})}}, {}});
    }

    root.Kids.push_back({"pl031@9010000", {{"compatible", g.Compatible({"arm,pl031", "arm,primecell"})},
        {"reg", g.Reg(ac, sc, {{0x09010000, 0x1000}})}, {"interrupts", g.Interrupts(0, 2, 1)}}, {}});
    root.Kids.push_back({"pl011@9000000", {{"compatible", g.Compatible({"arm,pl011", "arm,primecell"})},
        {"reg", g.Reg(ac, sc, {{0x09000000, 0x1000}})}, {"interrupts", g.Interrupts(0, 1, 1)}}, {}});

    for (int i = in.Chance(16) ? 40 : static_cast<int>(in.Below(6)); i > 0; i--)
    {
        Node v{"virtio_mmio@a000000", {{"compatible", g.Compatible({"virtio,mmio"})},
            {"reg", g.Reg(ac, sc, {{0x0a000000ULL + 0x200ULL * i, 0x200}})}}, {}};
        if (!in.Chance(16))
            v.Props.push_back({"interrupts", g.Interrupts(0, 16 + i, 1)});
        root.Kids.push_back(v);
    }

    root.Kids.push_back({"timer", {{"compatible", g.Compatible({"arm,armv8-timer", "arm,armv7-timer"})},
        {"interrupts", g.Interrupts(1, 13, 4)}}, {}});

    {
        uint32_t cac = in.Chance(32) ? g.CellCount(1) : 1;
        Node cpus{"cpus", {{"#address-cells", Cells({cac})}, {"#size-cells", Cells({0})}}, {}};
        for (int i = in.Chance(8) ? 70 : static_cast<int>(1 + in.Below(4)); i > 0; i--)
        {
            std::vector<uint8_t> reg;
            PutNumber(reg, in.Chance(16) ? in.Value64() : static_cast<uint64_t>(i - 1), cac <= 4 ? cac : 2);
            g.Trim(reg);
            cpus.Kids.push_back({"cpu@" + std::to_string(i - 1), {{"device_type", Str("cpu")},
                {"compatible", Strs({"arm,cortex-a72"})}, {"reg", reg}}, {}});
        }
        root.Kids.push_back(cpus);
    }

    /* Nodes nobody asked for, some nested past what the walker keeps. */
    for (int i = in.Below(3); i > 0; i--)
    {
        Node* at = &root;
        for (int d = in.Chance(32) ? 20 : static_cast<int>(in.Below(3)); d > 0; d--)
        {
            at->Kids.push_back({"n" + std::to_string(d), {{"x", g.String("y")}}, {}});
            at = &at->Kids.back();
        }
        at->Kids.push_back({in.Chance(128) ? "memory@0" : "chosen", {{"bootargs", g.String("deep")},
            {"reg", g.Reg(ac, sc, {{0x1000, 0x1000}})}}, {}});
    }
    return root;
}

/* ---- the blob ---- */

struct Blob
{
    std::vector<uint8_t> Struct;
    std::string Strings;
    /* Where each token starts: for the damage done to them. */
    std::vector<size_t> Tokens;

    uint32_t NameOff(const std::string& name)
    {
        size_t at = Strings.find(name + '\0');
        if (at != std::string::npos && (at == 0 || Strings[at - 1] == '\0'))
            return static_cast<uint32_t>(at);
        at = Strings.size();
        Strings += name;
        Strings += '\0';
        return static_cast<uint32_t>(at);
    }

    void Pad()
    {
        while (Struct.size() % 4 != 0)
            Struct.push_back(0);
    }

    void Emit(const Node& n)
    {
        Tokens.push_back(Struct.size());
        Put32(Struct, 1);
        Struct.insert(Struct.end(), n.Name.begin(), n.Name.end());
        Struct.push_back(0);
        Pad();
        for (const Prop& p : n.Props)
        {
            Tokens.push_back(Struct.size());
            Put32(Struct, 3);
            Put32(Struct, static_cast<uint32_t>(p.Value.size()));
            Put32(Struct, NameOff(p.Name));
            Struct.insert(Struct.end(), p.Value.begin(), p.Value.end());
            Pad();
        }
        for (const Node& k : n.Kids)
            Emit(k);
        Tokens.push_back(Struct.size());
        Put32(Struct, 2);
    }
};

const size_t HeaderSize = 40;
const size_t RsvmapSize = 16;

void Set32(std::vector<uint8_t>& v, size_t at, uint32_t x)
{
    v[at] = static_cast<uint8_t>(x >> 24);
    v[at + 1] = static_cast<uint8_t>(x >> 16);
    v[at + 2] = static_cast<uint8_t>(x >> 8);
    v[at + 3] = static_cast<uint8_t>(x);
}

std::vector<uint8_t> Serialize(Blob& b)
{
    b.Tokens.push_back(b.Struct.size());
    Put32(b.Struct, 9);
    std::vector<uint8_t> out(HeaderSize + RsvmapSize, 0);
    size_t structOff = out.size();
    out.insert(out.end(), b.Struct.begin(), b.Struct.end());
    size_t stringsOff = out.size();
    out.insert(out.end(), b.Strings.begin(), b.Strings.end());
    Set32(out, 0, 0xD00DFEED);
    Set32(out, 4, static_cast<uint32_t>(out.size()));
    Set32(out, 8, static_cast<uint32_t>(structOff));
    Set32(out, 12, static_cast<uint32_t>(stringsOff));
    Set32(out, 16, static_cast<uint32_t>(HeaderSize));
    Set32(out, 20, 17);
    Set32(out, 24, 16);
    Set32(out, 28, 0);
    Set32(out, 32, static_cast<uint32_t>(b.Strings.size()));
    Set32(out, 36, static_cast<uint32_t>(b.Struct.size()));
    return out;
}

/* Damage of the kinds a blob has: tokens that lie, a header that does. */
void Damage(Fuzz::Input& in, std::vector<uint8_t>& blob, const Blob& b)
{
    size_t structOff = HeaderSize + RsvmapSize;
    for (int n = 1 + in.Below(3); n > 0; n--)
    {
        /* Where a token was, as long as the blob still has it */
        size_t tok = structOff + b.Tokens[in.Below(b.Tokens.size())];
        if (tok + 12 > blob.size())
            continue;
        switch (in.U8() % 9)
        {
        case 0:
        case 1:
        {
            /* An END_NODE more, or a NOP, where a token was */
            uint8_t word[4] = {0, 0, 0, static_cast<uint8_t>(in.Chance(200) ? 2 : 4)};
            blob.insert(blob.begin() + tok, word, word + 4);
            Fuzz::Reached("a token inserted");
            break;
        }
        case 2:
            /* A token of anything */
            Set32(blob, tok, in.Value32());
            break;
        case 3:
            /* A length or a name offset that lies */
            Set32(blob, tok + 4 + 4 * in.Below(2), in.Value32());
            break;
        case 4:
            /* A byte of anything */
            blob[tok + in.Below(blob.size() - tok)] = in.U8();
            break;
        case 5:
            /* The header's sizes and offsets */
            Set32(blob, 4 + 4 * in.Below(9), in.Value32());
            Fuzz::Reached("a header that lies");
            break;
        case 6:
            /* Cut short */
            blob.resize(HeaderSize + in.Below(blob.size() - HeaderSize));
            break;
        case 7:
            /* The strings block unterminated */
            if (!blob.empty() && blob.back() == 0)
                blob.back() = 'x';
            break;
        default:
            /* An END_NODE fewer */
            Set32(blob, tok, 4);
            break;
        }
    }
}

/* ---- the model: board.h's reading of a sound tree ---- */

struct Expected
{
    std::vector<std::pair<uint64_t, uint64_t>> Mem;
    std::string BootArgs;
    bool PsciUseHvc = true;
    uint64_t GicdBase = 0, GicdSize = 0, GicrBase = 0, GicrSize = 0;
    uint64_t Pl011Base = 0, Pl031Base = 0;
    uint32_t Pl011IntId = 0, Pl031IntId = 0, TimerIntId = 0;
    uint64_t ItsBase = 0;
    uint64_t EcamBase = 0, EcamSize = 0;
    uint64_t Mmio32Base = 0, Mmio32Size = 0, Mmio64Base = 0, Mmio64Size = 0;
    uint32_t BusStart = 0, BusEnd = 0;
    struct Virtio
    {
        uint64_t Base, Size;
        uint32_t IntId;
    };
    std::vector<Virtio> VirtioMmio;
    std::vector<uint64_t> Cpus;
    uint64_t Refused = 0;
};

const uint64_t Reach = 1ULL << 47;

uint32_t Be32At(const std::vector<uint8_t>& v, size_t cell)
{
    size_t at = cell * 4;
    return (static_cast<uint32_t>(v[at]) << 24) | (v[at + 1] << 16) | (v[at + 2] << 8) | v[at + 3];
}

/* n cells from cell i on, as one number: two at most, all inside. */
bool Number(const Prop* p, uint64_t i, uint32_t n, uint64_t& x)
{
    if (p == nullptr || n > 2 || i > p->Value.size() / 4 || n > p->Value.size() / 4 - i)
        return false;
    x = 0;
    for (uint32_t k = 0; k < n; k++)
        x = (x << 32) | Be32At(p->Value, i + k);
    return true;
}

bool IsString(const Prop* p)
{
    return p != nullptr && !p->Value.empty() && p->Value.back() == 0;
}

bool Compatible(const Node& n, const char* what)
{
    const Prop* p = n.Find("compatible");
    if (!IsString(p))
        return false;
    size_t at = 0;
    while (at < p->Value.size())
    {
        const char* s = reinterpret_cast<const char*>(p->Value.data() + at);
        if (strcmp(s, what) == 0)
            return true;
        at += strlen(s) + 1;
    }
    return false;
}

bool Window(Expected& e, const Prop* reg, uint64_t i, uint32_t ac, uint32_t sc, uint64_t& base, uint64_t& size)
{
    if (reg == nullptr)
        return false;
    if (!Number(reg, i, ac, base) || !Number(reg, i + ac, sc, size))
    {
        e.Refused++;
        return false;
    }
    if (base < Reach && size <= Reach - base)
        return true;
    e.Refused++;
    return false;
}

bool IntId(const Prop* p, uint64_t index, uint32_t& id)
{
    if (p == nullptr || (index + 1) * 3 > p->Value.size() / 4)
        return false;
    uint32_t type = Be32At(p->Value, index * 3), num = Be32At(p->Value, index * 3 + 1);
    if (type == 0 && num < 256 - 32)
        id = 32 + num;
    else if (type == 1 && num < 16)
        id = 16 + num;
    else
        return false;
    return true;
}

bool StartsWith(const std::string& s, const char* prefix)
{
    return s.compare(0, strlen(prefix), prefix) == 0;
}

/* One node, as board.cpp takes it: the first kind it is, in board.cpp's
   order, with its parent's cells. */
void Take(Expected& e, const Node& n, int depth, uint32_t ac, uint32_t sc)
{
    if (StartsWith(n.Name, "memory@") || n.Name == "memory")
    {
        const Prop* reg = n.Find("reg");
        uint64_t entry = static_cast<uint64_t>(ac) + sc;
        if (reg != nullptr && (entry == 0 || reg->Value.size() % (entry * 4) != 0))
            e.Refused++;
        for (uint64_t i = 0; entry != 0 && e.Mem.size() < Board::MaxMemRegions && reg != nullptr &&
             i < reg->Value.size() / 4 / entry; i++)
        {
            uint64_t addr, size;
            if (!Number(reg, i * entry, ac, addr) || !Number(reg, i * entry + ac, sc, size))
            {
                e.Refused++;
                break;
            }
            e.Mem.push_back({addr, size});
        }
    }
    else if (n.Name == "chosen")
    {
        const Prop* p = n.Find("bootargs");
        if (IsString(p))
            e.BootArgs = std::string(reinterpret_cast<const char*>(p->Value.data())).substr(0, 511);
        else if (p != nullptr)
            e.Refused++;
    }
    else if (Compatible(n, "arm,psci-1.0") || Compatible(n, "arm,psci-0.2"))
    {
        const Prop* p = n.Find("method");
        if (IsString(p))
            e.PsciUseHvc = strcmp(reinterpret_cast<const char*>(p->Value.data()), "hvc") == 0;
    }
    else if (Compatible(n, "arm,gic-v3-its"))
    {
        uint64_t base, size;
        if (Window(e, n.Find("reg"), 0, ac, sc, base, size))
            e.ItsBase = base;
    }
    else if (Compatible(n, "pci-host-ecam-generic"))
    {
        uint64_t base, size;
        if (Window(e, n.Find("reg"), 0, ac, sc, base, size))
        {
            e.EcamBase = base;
            e.EcamSize = size;
        }
        const Prop* br = n.Find("bus-range");
        if (br != nullptr && br->Value.size() >= 8)
        {
            uint32_t start = Be32At(br->Value, 0), end = Be32At(br->Value, 1);
            if (start <= end && end <= 0xFF)
            {
                e.BusStart = start;
                e.BusEnd = end;
            }
            else
            {
                e.Refused++;
            }
        }
        const Prop* ranges = n.Find("ranges");
        for (uint64_t k = 0; ranges != nullptr && k < ranges->Value.size() / 4 / 7; k++)
        {
            uint32_t space = (Be32At(ranges->Value, k * 7) >> 24) & 3;
            uint64_t cpu, size;
            Number(ranges, k * 7 + 3, 2, cpu);
            Number(ranges, k * 7 + 5, 2, size);
            if (space != 2 && space != 3)
                continue;
            if (!(cpu < Reach && size <= Reach - cpu))
            {
                e.Refused++;
                continue;
            }
            (space == 2 ? e.Mmio32Base : e.Mmio64Base) = cpu;
            (space == 2 ? e.Mmio32Size : e.Mmio64Size) = size;
        }
    }
    else if (Compatible(n, "arm,gic-v3"))
    {
        const Prop* reg = n.Find("reg");
        uint64_t d, ds, r, rs;
        if (Window(e, reg, 0, ac, sc, d, ds) && Window(e, reg, static_cast<uint64_t>(ac) + sc, ac, sc, r, rs))
        {
            e.GicdBase = d;
            e.GicdSize = ds;
            e.GicrBase = r;
            e.GicrSize = rs;
        }
    }
    else if (Compatible(n, "arm,pl011") || Compatible(n, "arm,pl031"))
    {
        bool uart = Compatible(n, "arm,pl011");
        uint64_t base, size;
        if (Window(e, n.Find("reg"), 0, ac, sc, base, size))
            (uart ? e.Pl011Base : e.Pl031Base) = base;
        const Prop* irq = n.Find("interrupts");
        uint32_t id;
        if (irq != nullptr)
        {
            if (IntId(irq, 0, id))
                (uart ? e.Pl011IntId : e.Pl031IntId) = id;
            else
                e.Refused++;
        }
    }
    else if (Compatible(n, "virtio,mmio"))
    {
        const Prop* irq = n.Find("interrupts");
        uint32_t id = 0;
        bool usable = irq == nullptr || IntId(irq, 0, id);
        if (!usable)
            e.Refused++;
        uint64_t base, size;
        if (usable && e.VirtioMmio.size() < Board::MaxVirtioMmio && Window(e, n.Find("reg"), 0, ac, sc, base, size))
            e.VirtioMmio.push_back({base, size, id});
    }
    else if (Compatible(n, "arm,armv8-timer") || Compatible(n, "arm,armv7-timer"))
    {
        const Prop* irq = n.Find("interrupts");
        uint32_t id;
        if (irq != nullptr)
        {
            if (IntId(irq, 2, id))
                e.TimerIntId = id;
            else
                e.Refused++;
        }
    }
    else if (StartsWith(n.Name, "cpu@") && depth == 2)
    {
        const Prop* reg = n.Find("reg");
        uint64_t mpidr;
        if (Number(reg, 0, ac, mpidr))
        {
            if (e.Cpus.size() < Board::MaxBoardCpus)
                e.Cpus.push_back(mpidr);
        }
        else if (reg != nullptr)
        {
            e.Refused++;
        }
    }
}

/* The walk: nodes in order, each with the cells its parent passes on --
   its own #address-cells and #size-cells if it says them in one cell, what
   it inherited if not -- until a node deeper than the walker keeps, where
   the walk ends. False when it ended there. */
bool Walk(Expected& e, const Node& n, int depth, uint32_t ac, uint32_t sc)
{
    static const int MaxDepth = 16;
    if (depth >= MaxDepth)
        return false;
    Take(e, n, depth, ac, sc);
    uint32_t kac = ac, ksc = sc;
    const Prop* p = n.Find("#address-cells");
    if (p != nullptr && p->Value.size() == 4)
        kac = Be32At(p->Value, 0);
    p = n.Find("#size-cells");
    if (p != nullptr && p->Value.size() == 4)
        ksc = Be32At(p->Value, 0);
    for (const Node& k : n.Kids)
    {
        if (!Walk(e, k, depth + 1, kac, ksc))
            return false;
    }
    return true;
}

void Fallbacks(Expected& e)
{
    if (e.Mem.empty())
        e.Mem.push_back({0x40000000, 128ULL << 20});
    if (e.GicdBase == 0)
    {
        e.GicdBase = 0x08000000;
        e.GicdSize = 0x10000;
        e.GicrBase = 0x080A0000;
        e.GicrSize = 0xF60000;
    }
    if (e.Pl011Base == 0)
        e.Pl011Base = 0x09000000;
    if (e.Pl011IntId == 0)
        e.Pl011IntId = 33;
    if (e.Pl031Base == 0)
        e.Pl031Base = 0x09010000;
    if (e.Pl031IntId == 0)
        e.Pl031IntId = 34;
    if (e.TimerIntId == 0)
        e.TimerIntId = 27;
    if (e.ItsBase == 0)
        e.ItsBase = 0x08080000;
    if (e.EcamBase == 0)
    {
        e.EcamBase = 0x4010000000ULL;
        e.EcamSize = 0x10000000;
        e.Mmio32Base = 0x10000000;
        e.Mmio32Size = 0x2eff0000;
        e.Mmio64Base = 0x8000000000ULL;
        e.Mmio64Size = 0x8000000000ULL;
        e.BusStart = 0;
        e.BusEnd = 0xff;
    }
    if (e.Cpus.empty())
        e.Cpus.push_back(0);
}

/* What any board must be, whatever the tree said. */
void CheckSane(const Board& b)
{
    INVARIANT(b.MemRegionCount <= Board::MaxMemRegions, "%lu memory regions", b.MemRegionCount);
    INVARIANT(b.VirtioMmioCount <= Board::MaxVirtioMmio, "%lu virtio-mmio windows", b.VirtioMmioCount);
    INVARIANT(b.CpuCount >= 1 && b.CpuCount <= Board::MaxBoardCpus, "%lu CPUs", b.CpuCount);
    INVARIANT(memchr(b.BootArgs, 0, sizeof(b.BootArgs)) != nullptr, "bootargs unterminated");
    INVARIANT(b.DtbRegion.Size <= Fdt::MaxSize, "a DTB of %lu bytes reserved", b.DtbRegion.Size);
    const uint32_t ids[] = {b.Pl011IntId, b.Pl031IntId, b.TimerIntId};
    for (uint32_t id : ids)
        INVARIANT(Board::IsTakenIntId(id), "INTID %u, which the kernel cannot take", id);
    const std::pair<uint64_t, uint64_t> windows[] = {{b.GicdBase, b.GicdSize}, {b.GicrBase, b.GicrSize},
        {b.ItsBase, 0}, {b.EcamBase, b.EcamSize}, {b.Pl011Base, 0}, {b.Pl031Base, 0},
        {b.PciMmio32Base, b.PciMmio32Size}, {b.PciMmio64Base, b.PciMmio64Size}};
    for (auto& w : windows)
        INVARIANT(w.first < Reach && w.second <= Reach - w.first, "a window at 0x%llx of 0x%llx out of reach",
            static_cast<unsigned long long>(w.first), static_cast<unsigned long long>(w.second));
    for (ulong i = 0; i < b.VirtioMmioCount; i++)
    {
        const auto& v = b.VirtioMmio[i];
        INVARIANT(v.Base < Reach && v.Size <= Reach - v.Base, "virtio-mmio %lu out of reach", i);
        INVARIANT(v.IntId == 0 || Board::IsTakenIntId(v.IntId), "virtio-mmio %lu on INTID %u", i, v.IntId);
    }
    INVARIANT(b.PciBusStart <= b.PciBusEnd, "bus range %u..%u", b.PciBusStart, b.PciBusEnd);
}

void CheckModel(const Board& b, const Expected& e, bool setup)
{
    INVARIANT(b.MemRegionCount == e.Mem.size(), "%lu memory regions, where the tree has %zu", b.MemRegionCount,
        e.Mem.size());
    for (size_t i = 0; i < e.Mem.size(); i++)
        INVARIANT(b.MemRegions[i].Addr == e.Mem[i].first && b.MemRegions[i].Size == e.Mem[i].second,
            "memory region %zu is 0x%lx+0x%lx, where the tree has 0x%llx+0x%llx", i, b.MemRegions[i].Addr,
            b.MemRegions[i].Size, static_cast<unsigned long long>(e.Mem[i].first),
            static_cast<unsigned long long>(e.Mem[i].second));
    INVARIANT(e.BootArgs == b.BootArgs, "bootargs '%s', where the tree has '%s'", b.BootArgs, e.BootArgs.c_str());
    INVARIANT(b.PsciUseHvc == e.PsciUseHvc, "PSCI by %s", b.PsciUseHvc ? "hvc" : "smc");
    INVARIANT(b.GicdBase == e.GicdBase && b.GicdSize == e.GicdSize && b.GicrBase == e.GicrBase &&
        b.GicrSize == e.GicrSize, "the GIC at 0x%lx/0x%lx, where the tree has 0x%llx/0x%llx", b.GicdBase,
        b.GicrBase, static_cast<unsigned long long>(e.GicdBase), static_cast<unsigned long long>(e.GicrBase));
    INVARIANT(b.ItsBase == e.ItsBase, "the ITS at 0x%lx, where the tree has 0x%llx", b.ItsBase,
        static_cast<unsigned long long>(e.ItsBase));
    INVARIANT(b.Pl011Base == e.Pl011Base && b.Pl011IntId == e.Pl011IntId, "the PL011 at 0x%lx on %u, not 0x%llx on %u",
        b.Pl011Base, b.Pl011IntId, static_cast<unsigned long long>(e.Pl011Base), e.Pl011IntId);
    INVARIANT(b.Pl031Base == e.Pl031Base && b.Pl031IntId == e.Pl031IntId, "the PL031 at 0x%lx on %u, not 0x%llx on %u",
        b.Pl031Base, b.Pl031IntId, static_cast<unsigned long long>(e.Pl031Base), e.Pl031IntId);
    INVARIANT(b.TimerIntId == e.TimerIntId, "the timer on %u, where the tree has %u", b.TimerIntId, e.TimerIntId);
    INVARIANT(b.EcamBase == e.EcamBase && b.EcamSize == e.EcamSize, "ECAM at 0x%lx+0x%lx, not 0x%llx+0x%llx",
        b.EcamBase, b.EcamSize, static_cast<unsigned long long>(e.EcamBase), static_cast<unsigned long long>(e.EcamSize));
    INVARIANT(b.PciMmio32Base == e.Mmio32Base && b.PciMmio32Size == e.Mmio32Size && b.PciMmio64Base == e.Mmio64Base &&
        b.PciMmio64Size == e.Mmio64Size, "the PCI windows are not the tree's");
    INVARIANT(b.PciBusStart == e.BusStart && b.PciBusEnd == e.BusEnd, "buses %u..%u, where the tree has %u..%u",
        b.PciBusStart, b.PciBusEnd, e.BusStart, e.BusEnd);
    INVARIANT(b.VirtioMmioCount == e.VirtioMmio.size(), "%lu virtio-mmio windows, where the tree has %zu",
        b.VirtioMmioCount, e.VirtioMmio.size());
    for (size_t i = 0; i < e.VirtioMmio.size(); i++)
        INVARIANT(b.VirtioMmio[i].Base == e.VirtioMmio[i].Base && b.VirtioMmio[i].Size == e.VirtioMmio[i].Size &&
            b.VirtioMmio[i].IntId == e.VirtioMmio[i].IntId, "virtio-mmio %zu is not the tree's", i);
    INVARIANT(b.CpuCount == e.Cpus.size(), "%lu CPUs, where the tree has %zu", b.CpuCount, e.Cpus.size());
    for (size_t i = 0; i < e.Cpus.size(); i++)
        INVARIANT(b.CpuMpidr[i] == e.Cpus[i], "CPU %zu's MPIDR is 0x%lx, not 0x%llx", i, b.CpuMpidr[i],
            static_cast<unsigned long long>(e.Cpus[i]));
    INVARIANT(b.Refused == e.Refused, "%lu values refused, where the tree has %llu to refuse", b.Refused,
        static_cast<unsigned long long>(e.Refused));
    INVARIANT(setup == (b.DtbRegion.Size != 0), "the DTB region is %lu bytes", b.DtbRegion.Size);
}

void Run(Fuzz::Input& in)
{
    Gen g{in};
    Node root = Tree(g);
    Blob b;
    b.Emit(root);
    std::vector<uint8_t> blob = Serialize(b);
    bool damaged = in.Chance(80);
    if (damaged)
    {
        Damage(in, blob, b);
        Fuzz::Reached("a damaged tree");
    }
    else
    {
        Fuzz::Reached("a sound tree");
    }

    /* The blob alone in its allocation, as long as its header says -- what
       is past a DTB in memory is not the DTB's, but it is there to read --
       and never less than a header. */
    size_t said = 0;
    if (blob.size() >= 8)
        said = Be32At(blob, 1);
    size_t size = std::max<size_t>(std::max(blob.size(), HeaderSize), std::min<size_t>(said, Fdt::MaxSize));
    uint8_t* mem = static_cast<uint8_t*>(calloc(1, size));
    memcpy(mem, blob.data(), blob.size());

    Board& board = Board::GetInstance();
    board.Setup(mem);
    CheckSane(board);
    if (!damaged)
    {
        Expected e;
        Walk(e, root, 0, 2, 1);
        Fallbacks(e);
        CheckModel(board, e, true);
        if (e.Refused != 0)
            Fuzz::Reached("a sound tree with values refused");
    }
    else if (board.DtbRegion.Size != 0)
    {
        Fuzz::Reached("a damaged tree read");
    }
    free(mem);
}

void Reset()
{
    Board& board = Board::GetInstance();
    board.~Board();
    new (&board) Board();
}

}

const Fuzz::Target Fuzz::TheTarget = {"fdt", Reset, Run, 4096, 50000};
