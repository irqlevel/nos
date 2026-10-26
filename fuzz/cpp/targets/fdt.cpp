// The device tree (arch/arm64/fdt.cpp) and what the arm64 boot makes of it
// (arch/arm64/board.cpp): the first thing the kernel reads, at a point where
// a fault is a boot that dies without a word, and whatever the firmware or
// the bootloader put there -- a tree of QEMU virt's shape, and that damaged.
// Every tree is built node by node from the input, serialized as a DTB, and
// handed to Board::setup as the boot hands it. A tree serialized as it was
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

using kernel::Board;
using kernel::Fdt;

/* ---- the tree ---- */

struct Prop
{
    std::string name;
    std::vector<uint8_t> value;
};

struct Node
{
    std::string name;
    std::vector<Prop> props;
    std::vector<Node> kids;

    const Prop* find(const char* wanted) const
    {
        for (const Prop& p : props)
        {
            if (p.name == wanted)
                return &p;
        }
        return nullptr;
    }
};

void put32(std::vector<uint8_t>& v, uint32_t x)
{
    v.push_back(static_cast<uint8_t>(x >> 24));
    v.push_back(static_cast<uint8_t>(x >> 16));
    v.push_back(static_cast<uint8_t>(x >> 8));
    v.push_back(static_cast<uint8_t>(x));
}

std::vector<uint8_t> cells(std::initializer_list<uint32_t> cells)
{
    std::vector<uint8_t> v;
    for (uint32_t c : cells)
        put32(v, c);
    return v;
}

/* A number in n cells, big-endian: the low 32 bits of each, as a tree
   holds it. */
void put_number(std::vector<uint8_t>& v, uint64_t x, uint32_t n)
{
    for (uint32_t i = n; i > 0; i--)
        put32(v, (i > 2) ? 0 : static_cast<uint32_t>(x >> (32 * (i - 1))));
}

std::vector<uint8_t> str(const std::string& s)
{
    std::vector<uint8_t> v(s.begin(), s.end());
    v.push_back(0);
    return v;
}

/* A compatible list: NUL-separated, NUL-terminated. */
std::vector<uint8_t> strs(std::initializer_list<const char*> list)
{
    std::vector<uint8_t> v;
    for (const char* s : list)
    {
        std::vector<uint8_t> one = str(s);
        v.insert(v.end(), one.begin(), one.end());
    }
    return v;
}

/* ---- the input's choices ---- */

struct Gen
{
    fuzz::Input& in;

    /* A cell count: what trees have, and what they should not. */
    uint32_t cell_count(uint32_t usual)
    {
        switch (in.u8() % 12)
        {
        case 0:
            return 1;
        case 1:
            return 2;
        case 2:
            return static_cast<uint32_t>(in.below(5));
        case 3:
            return in.value32();
        default:
            return usual;
        }
    }

    uint64_t address(uint64_t usual)
    {
        static const uint64_t EDGES[] = {0, 1ULL << 47, (1ULL << 47) - 0x1000, 1ULL << 52, ~0ULL,
                                         0xFFFFFFFFULL, 0x4010000000ULL, 0x8000000000ULL};
        switch (in.u8() % 10)
        {
        case 0:
            return in.pick(EDGES);
        case 1:
            return in.value64();
        default:
            return usual;
        }
    }

    uint64_t size(uint64_t usual)
    {
        static const uint64_t EDGES[] = {0, 1, 0x1000, 1ULL << 46, 1ULL << 47, ~0ULL};
        switch (in.u8() % 10)
        {
        case 0:
            return in.pick(EDGES);
        case 1:
            return in.value64();
        default:
            return usual;
        }
    }

    /* reg = <address size>... in ac/sc cells, sometimes cut short. */
    std::vector<uint8_t> reg(uint32_t ac, uint32_t sc, std::initializer_list<std::pair<uint64_t, uint64_t>> windows)
    {
        std::vector<uint8_t> v;
        for (auto& w : windows)
        {
            if (ac <= 4)
                put_number(v, address(w.first), ac);
            if (sc <= 4)
                put_number(v, size(w.second), sc);
        }
        trim(v);
        return v;
    }

    /* A property's value cut, or grown, now and then. */
    void trim(std::vector<uint8_t>& v)
    {
        if (in.chance(12) && !v.empty())
            v.resize(in.below(v.size()));
        else if (in.chance(6))
            v.push_back(in.u8());
    }

    /* interrupts = <type number flags>..., mostly one the kernel takes. */
    std::vector<uint8_t> interrupts(uint32_t type, uint32_t num, int count)
    {
        std::vector<uint8_t> v;
        for (int i = 0; i < count; i++)
        {
            uint32_t t = type, n = num + i;
            switch (in.u8() % 12)
            {
            case 0:
                t = static_cast<uint32_t>(in.below(4));
                break;
            case 1:
                n = in.value32();
                break;
            case 2:
                n = static_cast<uint32_t>(in.range(200, 260));
                break;
            default:
                break;
            }
            put32(v, t);
            put32(v, n);
            put32(v, 4);
        }
        trim(v);
        return v;
    }

    std::string text(size_t max)
    {
        size_t n = in.chance(16) ? in.range(400, 700) : in.below(max);
        std::string s;
        for (size_t i = 0; i < n; i++)
            s += static_cast<char>(' ' + in.u8() % 95);
        return s;
    }

    /* A string property, now and then with no NUL at its end. */
    std::vector<uint8_t> string(const std::string& s)
    {
        std::vector<uint8_t> v = str(s);
        if (in.chance(12))
            v.pop_back();
        return v;
    }

    std::vector<uint8_t> compatible(std::initializer_list<const char*> list)
    {
        std::vector<uint8_t> v = strs(list);
        if (in.chance(8))
            v.pop_back();
        return v;
    }
};

Node tree(Gen& g)
{
    fuzz::Input& in = g.in;
    Node root{"", {}, {}};
    uint32_t ac = g.cell_count(2), sc = g.cell_count(2);
    if (!in.chance(8))
        root.props.push_back({"#address-cells", cells({ac})});
    else
        ac = 2; /* the walker's own for a root that says nothing */
    if (!in.chance(8))
        root.props.push_back({"#size-cells", cells({sc})});
    else
        sc = 1;
    root.props.push_back({"compatible", strs({"linux,dummy-virt"})});

    for (int i = in.below(3); i >= 0; i--)
        root.kids.push_back({in.chance(32) ? "memory" : "memory@40000000", {{"device_type", str("memory")},
            {"reg", g.reg(ac, sc, {{0x40000000, 0x40000000}, {0x100000000ULL, 0x40000000}})}}, {}});

    root.kids.push_back({"chosen", {{"bootargs", g.string(g.text(80))}}, {}});

    root.kids.push_back({"psci", {{"compatible", g.compatible({"arm,psci-1.0", "arm,psci-0.2", "arm,psci"})},
        {"method", g.string(in.chance(64) ? "smc" : in.chance(32) ? g.text(6) : "hvc")}}, {}});

    Node intc{"intc@8000000", {{"compatible", g.compatible({"arm,gic-v3"})},
        {"reg", g.reg(ac, sc, {{0x08000000, 0x10000}, {0x080a0000, 0xf60000}})},
        {"#address-cells", cells({2})}, {"#size-cells", cells({2})}}, {}};
    if (!in.chance(32))
        intc.kids.push_back({"its@8080000", {{"compatible", g.compatible({"arm,gic-v3-its"})},
            {"reg", g.reg(2, 2, {{0x08080000, 0x20000}})}}, {}});
    root.kids.push_back(intc);

    {
        std::vector<uint8_t> ranges;
        for (int e = in.below(4); e > 0; e--)
        {
            static const uint32_t SPACES[] = {0x01000000, 0x02000000, 0x03000000, 0x43000000};
            put32(ranges, in.chance(32) ? in.u32() : in.pick(SPACES));
            put32(ranges, 0);
            put32(ranges, 0);
            put_number(ranges, g.address(0x10000000), 2);
            put_number(ranges, g.size(0x2eff0000), 2);
        }
        g.trim(ranges);
        std::vector<uint8_t> bus_range = cells({0, 0xff});
        if (in.chance(24))
            bus_range = cells({in.value32(), in.value32()});
        g.trim(bus_range);
        root.kids.push_back({"pcie@10000000", {{"compatible", g.compatible({"pci-host-ecam-generic"})},
            {"reg", g.reg(ac, sc, {{0x4010000000ULL, 0x10000000}})}, {"bus-range", bus_range},
            {"ranges", ranges}, {"#address-cells", cells({3})}, {"#size-cells", cells({2})}}, {}});
    }

    root.kids.push_back({"pl031@9010000", {{"compatible", g.compatible({"arm,pl031", "arm,primecell"})},
        {"reg", g.reg(ac, sc, {{0x09010000, 0x1000}})}, {"interrupts", g.interrupts(0, 2, 1)}}, {}});
    root.kids.push_back({"pl011@9000000", {{"compatible", g.compatible({"arm,pl011", "arm,primecell"})},
        {"reg", g.reg(ac, sc, {{0x09000000, 0x1000}})}, {"interrupts", g.interrupts(0, 1, 1)}}, {}});

    for (int i = in.chance(16) ? 40 : static_cast<int>(in.below(6)); i > 0; i--)
    {
        Node v{"virtio_mmio@a000000", {{"compatible", g.compatible({"virtio,mmio"})},
            {"reg", g.reg(ac, sc, {{0x0a000000ULL + 0x200ULL * i, 0x200}})}}, {}};
        if (!in.chance(16))
            v.props.push_back({"interrupts", g.interrupts(0, 16 + i, 1)});
        root.kids.push_back(v);
    }

    root.kids.push_back({"timer", {{"compatible", g.compatible({"arm,armv8-timer", "arm,armv7-timer"})},
        {"interrupts", g.interrupts(1, 13, 4)}}, {}});

    {
        uint32_t cac = in.chance(32) ? g.cell_count(1) : 1;
        Node cpus{"cpus", {{"#address-cells", cells({cac})}, {"#size-cells", cells({0})}}, {}};
        for (int i = in.chance(8) ? 70 : static_cast<int>(1 + in.below(4)); i > 0; i--)
        {
            std::vector<uint8_t> reg;
            put_number(reg, in.chance(16) ? in.value64() : static_cast<uint64_t>(i - 1), cac <= 4 ? cac : 2);
            g.trim(reg);
            cpus.kids.push_back({"cpu@" + std::to_string(i - 1), {{"device_type", str("cpu")},
                {"compatible", strs({"arm,cortex-a72"})}, {"reg", reg}}, {}});
        }
        root.kids.push_back(cpus);
    }

    /* Nodes nobody asked for, some nested past what the walker keeps. */
    for (int i = in.below(3); i > 0; i--)
    {
        Node* at = &root;
        for (int d = in.chance(32) ? 20 : static_cast<int>(in.below(3)); d > 0; d--)
        {
            at->kids.push_back({"n" + std::to_string(d), {{"x", g.string("y")}}, {}});
            at = &at->kids.back();
        }
        at->kids.push_back({in.chance(128) ? "memory@0" : "chosen", {{"bootargs", g.string("deep")},
            {"reg", g.reg(ac, sc, {{0x1000, 0x1000}})}}, {}});
    }
    return root;
}

/* ---- the blob ---- */

struct Blob
{
    std::vector<uint8_t> structure;
    std::string strings;
    /* Where each token starts: for the damage done to them. */
    std::vector<size_t> tokens;

    uint32_t name_off(const std::string& name)
    {
        size_t at = strings.find(name + '\0');
        if (at != std::string::npos && (at == 0 || strings[at - 1] == '\0'))
            return static_cast<uint32_t>(at);
        at = strings.size();
        strings += name;
        strings += '\0';
        return static_cast<uint32_t>(at);
    }

    void pad()
    {
        while (structure.size() % 4 != 0)
            structure.push_back(0);
    }

    void emit(const Node& n)
    {
        tokens.push_back(structure.size());
        put32(structure, 1);
        structure.insert(structure.end(), n.name.begin(), n.name.end());
        structure.push_back(0);
        pad();
        for (const Prop& p : n.props)
        {
            tokens.push_back(structure.size());
            put32(structure, 3);
            put32(structure, static_cast<uint32_t>(p.value.size()));
            put32(structure, name_off(p.name));
            structure.insert(structure.end(), p.value.begin(), p.value.end());
            pad();
        }
        for (const Node& k : n.kids)
            emit(k);
        tokens.push_back(structure.size());
        put32(structure, 2);
    }
};

const size_t HEADER_SIZE = 40;
const size_t RSVMAP_SIZE = 16;

void set32(std::vector<uint8_t>& v, size_t at, uint32_t x)
{
    v[at] = static_cast<uint8_t>(x >> 24);
    v[at + 1] = static_cast<uint8_t>(x >> 16);
    v[at + 2] = static_cast<uint8_t>(x >> 8);
    v[at + 3] = static_cast<uint8_t>(x);
}

std::vector<uint8_t> serialize(Blob& b)
{
    b.tokens.push_back(b.structure.size());
    put32(b.structure, 9);
    std::vector<uint8_t> out(HEADER_SIZE + RSVMAP_SIZE, 0);
    size_t struct_off = out.size();
    out.insert(out.end(), b.structure.begin(), b.structure.end());
    size_t strings_off = out.size();
    out.insert(out.end(), b.strings.begin(), b.strings.end());
    set32(out, 0, 0xD00DFEED);
    set32(out, 4, static_cast<uint32_t>(out.size()));
    set32(out, 8, static_cast<uint32_t>(struct_off));
    set32(out, 12, static_cast<uint32_t>(strings_off));
    set32(out, 16, static_cast<uint32_t>(HEADER_SIZE));
    set32(out, 20, 17);
    set32(out, 24, 16);
    set32(out, 28, 0);
    set32(out, 32, static_cast<uint32_t>(b.strings.size()));
    set32(out, 36, static_cast<uint32_t>(b.structure.size()));
    return out;
}

/* Damage of the kinds a blob has: tokens that lie, a header that does. */
void damage(fuzz::Input& in, std::vector<uint8_t>& blob, const Blob& b)
{
    size_t struct_off = HEADER_SIZE + RSVMAP_SIZE;
    for (int n = 1 + in.below(3); n > 0; n--)
    {
        /* Where a token was, as long as the blob still has it */
        size_t tok = struct_off + b.tokens[in.below(b.tokens.size())];
        if (tok + 12 > blob.size())
            continue;
        switch (in.u8() % 9)
        {
        case 0:
        case 1:
        {
            /* An END_NODE more, or a NOP, where a token was */
            uint8_t word[4] = {0, 0, 0, static_cast<uint8_t>(in.chance(200) ? 2 : 4)};
            blob.insert(blob.begin() + tok, word, word + 4);
            fuzz::reached("a token inserted");
            break;
        }
        case 2:
            /* A token of anything */
            set32(blob, tok, in.value32());
            break;
        case 3:
            /* A length or a name offset that lies */
            set32(blob, tok + 4 + 4 * in.below(2), in.value32());
            break;
        case 4:
            /* A byte of anything */
            blob[tok + in.below(blob.size() - tok)] = in.u8();
            break;
        case 5:
            /* The header's sizes and offsets */
            set32(blob, 4 + 4 * in.below(9), in.value32());
            fuzz::reached("a header that lies");
            break;
        case 6:
            /* Cut short */
            blob.resize(HEADER_SIZE + in.below(blob.size() - HEADER_SIZE));
            break;
        case 7:
            /* The strings block unterminated */
            if (!blob.empty() && blob.back() == 0)
                blob.back() = 'x';
            break;
        default:
            /* An END_NODE fewer */
            set32(blob, tok, 4);
            break;
        }
    }
}

/* ---- the model: board.h's reading of a sound tree ---- */

struct Expected
{
    std::vector<std::pair<uint64_t, uint64_t>> mem;
    std::string boot_args;
    bool psci_use_hvc = true;
    uint64_t gicd_base = 0, gicd_size = 0, gicr_base = 0, gicr_size = 0;
    uint64_t pl011_base = 0, pl031_base = 0;
    uint32_t pl011_int_id = 0, pl031_int_id = 0, timer_int_id = 0;
    uint64_t its_base = 0;
    uint64_t ecam_base = 0, ecam_size = 0;
    uint64_t mmio32_base = 0, mmio32_size = 0, mmio64_base = 0, mmio64_size = 0;
    uint32_t bus_start = 0, bus_end = 0;
    struct Virtio
    {
        uint64_t base, size;
        uint32_t int_id;
    };
    std::vector<Virtio> virtio_mmio;
    std::vector<uint64_t> cpus;
    uint64_t refused = 0;
};

const uint64_t REACH = 1ULL << 47;

uint32_t be32_at(const std::vector<uint8_t>& v, size_t cell)
{
    size_t at = cell * 4;
    return (static_cast<uint32_t>(v[at]) << 24) | (v[at + 1] << 16) | (v[at + 2] << 8) | v[at + 3];
}

/* n cells from cell i on, as one number: two at most, all inside. */
bool number(const Prop* p, uint64_t i, uint32_t n, uint64_t& x)
{
    if (p == nullptr || n > 2 || i > p->value.size() / 4 || n > p->value.size() / 4 - i)
        return false;
    x = 0;
    for (uint32_t k = 0; k < n; k++)
        x = (x << 32) | be32_at(p->value, i + k);
    return true;
}

bool is_string(const Prop* p)
{
    return p != nullptr && !p->value.empty() && p->value.back() == 0;
}

bool compatible(const Node& n, const char* what)
{
    const Prop* p = n.find("compatible");
    if (!is_string(p))
        return false;
    size_t at = 0;
    while (at < p->value.size())
    {
        const char* s = reinterpret_cast<const char*>(p->value.data() + at);
        if (strcmp(s, what) == 0)
            return true;
        at += strlen(s) + 1;
    }
    return false;
}

bool window(Expected& e, const Prop* reg, uint64_t i, uint32_t ac, uint32_t sc, uint64_t& base, uint64_t& size)
{
    if (reg == nullptr)
        return false;
    if (!number(reg, i, ac, base) || !number(reg, i + ac, sc, size))
    {
        e.refused++;
        return false;
    }
    if (base < REACH && size <= REACH - base)
        return true;
    e.refused++;
    return false;
}

bool int_id(const Prop* p, uint64_t index, uint32_t& id)
{
    if (p == nullptr || (index + 1) * 3 > p->value.size() / 4)
        return false;
    uint32_t type = be32_at(p->value, index * 3), num = be32_at(p->value, index * 3 + 1);
    if (type == 0 && num < 256 - 32)
        id = 32 + num;
    else if (type == 1 && num < 16)
        id = 16 + num;
    else
        return false;
    return true;
}

bool starts_with(const std::string& s, const char* prefix)
{
    return s.compare(0, strlen(prefix), prefix) == 0;
}

/* One node, as board.cpp takes it: the first kind it is, in board.cpp's
   order, with its parent's cells. */
void take(Expected& e, const Node& n, int depth, uint32_t ac, uint32_t sc)
{
    if (starts_with(n.name, "memory@") || n.name == "memory")
    {
        const Prop* reg = n.find("reg");
        uint64_t entry = static_cast<uint64_t>(ac) + sc;
        if (reg != nullptr && (entry == 0 || reg->value.size() % (entry * 4) != 0))
            e.refused++;
        for (uint64_t i = 0; entry != 0 && e.mem.size() < Board::MAX_MEM_REGIONS && reg != nullptr &&
             i < reg->value.size() / 4 / entry; i++)
        {
            uint64_t addr, size;
            if (!number(reg, i * entry, ac, addr) || !number(reg, i * entry + ac, sc, size))
            {
                e.refused++;
                break;
            }
            e.mem.push_back({addr, size});
        }
    }
    else if (n.name == "chosen")
    {
        const Prop* p = n.find("bootargs");
        if (is_string(p))
            e.boot_args = std::string(reinterpret_cast<const char*>(p->value.data())).substr(0, 511);
        else if (p != nullptr)
            e.refused++;
    }
    else if (compatible(n, "arm,psci-1.0") || compatible(n, "arm,psci-0.2"))
    {
        const Prop* p = n.find("method");
        if (is_string(p))
            e.psci_use_hvc = strcmp(reinterpret_cast<const char*>(p->value.data()), "hvc") == 0;
    }
    else if (compatible(n, "arm,gic-v3-its"))
    {
        uint64_t base, size;
        if (window(e, n.find("reg"), 0, ac, sc, base, size))
            e.its_base = base;
    }
    else if (compatible(n, "pci-host-ecam-generic"))
    {
        uint64_t base, size;
        if (window(e, n.find("reg"), 0, ac, sc, base, size))
        {
            e.ecam_base = base;
            e.ecam_size = size;
        }
        const Prop* br = n.find("bus-range");
        if (br != nullptr && br->value.size() >= 8)
        {
            uint32_t start = be32_at(br->value, 0), end = be32_at(br->value, 1);
            if (start <= end && end <= 0xFF)
            {
                e.bus_start = start;
                e.bus_end = end;
            }
            else
            {
                e.refused++;
            }
        }
        const Prop* ranges = n.find("ranges");
        for (uint64_t k = 0; ranges != nullptr && k < ranges->value.size() / 4 / 7; k++)
        {
            uint32_t space = (be32_at(ranges->value, k * 7) >> 24) & 3;
            uint64_t cpu, size;
            number(ranges, k * 7 + 3, 2, cpu);
            number(ranges, k * 7 + 5, 2, size);
            if (space != 2 && space != 3)
                continue;
            if (!(cpu < REACH && size <= REACH - cpu))
            {
                e.refused++;
                continue;
            }
            (space == 2 ? e.mmio32_base : e.mmio64_base) = cpu;
            (space == 2 ? e.mmio32_size : e.mmio64_size) = size;
        }
    }
    else if (compatible(n, "arm,gic-v3"))
    {
        const Prop* reg = n.find("reg");
        uint64_t d, ds, r, rs;
        if (window(e, reg, 0, ac, sc, d, ds) && window(e, reg, static_cast<uint64_t>(ac) + sc, ac, sc, r, rs))
        {
            e.gicd_base = d;
            e.gicd_size = ds;
            e.gicr_base = r;
            e.gicr_size = rs;
        }
    }
    else if (compatible(n, "arm,pl011") || compatible(n, "arm,pl031"))
    {
        bool uart = compatible(n, "arm,pl011");
        uint64_t base, size;
        if (window(e, n.find("reg"), 0, ac, sc, base, size))
            (uart ? e.pl011_base : e.pl031_base) = base;
        const Prop* irq = n.find("interrupts");
        uint32_t id;
        if (irq != nullptr)
        {
            if (int_id(irq, 0, id))
                (uart ? e.pl011_int_id : e.pl031_int_id) = id;
            else
                e.refused++;
        }
    }
    else if (compatible(n, "virtio,mmio"))
    {
        const Prop* irq = n.find("interrupts");
        uint32_t id = 0;
        bool usable = irq == nullptr || int_id(irq, 0, id);
        if (!usable)
            e.refused++;
        uint64_t base, size;
        if (usable && e.virtio_mmio.size() < Board::MAX_VIRTIO_MMIO && window(e, n.find("reg"), 0, ac, sc, base, size))
            e.virtio_mmio.push_back({base, size, id});
    }
    else if (compatible(n, "arm,armv8-timer") || compatible(n, "arm,armv7-timer"))
    {
        const Prop* irq = n.find("interrupts");
        uint32_t id;
        if (irq != nullptr)
        {
            if (int_id(irq, 2, id))
                e.timer_int_id = id;
            else
                e.refused++;
        }
    }
    else if (starts_with(n.name, "cpu@") && depth == 2)
    {
        const Prop* reg = n.find("reg");
        uint64_t mpidr;
        if (number(reg, 0, ac, mpidr))
        {
            if (e.cpus.size() < Board::MAX_BOARD_CPUS)
                e.cpus.push_back(mpidr);
        }
        else if (reg != nullptr)
        {
            e.refused++;
        }
    }
}

/* The walk: nodes in order, each with the cells its parent passes on --
   its own #address-cells and #size-cells if it says them in one cell, what
   it inherited if not -- until a node deeper than the walker keeps, where
   the walk ends. False when it ended there. */
bool walk(Expected& e, const Node& n, int depth, uint32_t ac, uint32_t sc)
{
    static const int MAX_DEPTH = 16;
    if (depth >= MAX_DEPTH)
        return false;
    take(e, n, depth, ac, sc);
    uint32_t kac = ac, ksc = sc;
    const Prop* p = n.find("#address-cells");
    if (p != nullptr && p->value.size() == 4)
        kac = be32_at(p->value, 0);
    p = n.find("#size-cells");
    if (p != nullptr && p->value.size() == 4)
        ksc = be32_at(p->value, 0);
    for (const Node& k : n.kids)
    {
        if (!walk(e, k, depth + 1, kac, ksc))
            return false;
    }
    return true;
}

void fallbacks(Expected& e)
{
    if (e.mem.empty())
        e.mem.push_back({0x40000000, 128ULL << 20});
    if (e.gicd_base == 0)
    {
        e.gicd_base = 0x08000000;
        e.gicd_size = 0x10000;
        e.gicr_base = 0x080A0000;
        e.gicr_size = 0xF60000;
    }
    if (e.pl011_base == 0)
        e.pl011_base = 0x09000000;
    if (e.pl011_int_id == 0)
        e.pl011_int_id = 33;
    if (e.pl031_base == 0)
        e.pl031_base = 0x09010000;
    if (e.pl031_int_id == 0)
        e.pl031_int_id = 34;
    if (e.timer_int_id == 0)
        e.timer_int_id = 27;
    if (e.its_base == 0)
        e.its_base = 0x08080000;
    if (e.ecam_base == 0)
    {
        e.ecam_base = 0x4010000000ULL;
        e.ecam_size = 0x10000000;
        e.mmio32_base = 0x10000000;
        e.mmio32_size = 0x2eff0000;
        e.mmio64_base = 0x8000000000ULL;
        e.mmio64_size = 0x8000000000ULL;
        e.bus_start = 0;
        e.bus_end = 0xff;
    }
    if (e.cpus.empty())
        e.cpus.push_back(0);
}

/* What any board must be, whatever the tree said. */
void check_sane(const Board& b)
{
    INVARIANT(b.mem_region_count_ <= Board::MAX_MEM_REGIONS, "%lu memory regions", b.mem_region_count_);
    INVARIANT(b.virtio_mmio_count_ <= Board::MAX_VIRTIO_MMIO, "%lu virtio-mmio windows", b.virtio_mmio_count_);
    INVARIANT(b.cpu_count_ >= 1 && b.cpu_count_ <= Board::MAX_BOARD_CPUS, "%lu CPUs", b.cpu_count_);
    INVARIANT(memchr(b.boot_args_, 0, sizeof(b.boot_args_)) != nullptr, "bootargs unterminated");
    INVARIANT(b.dtb_region_.size <= Fdt::MAX_SIZE, "a DTB of %lu bytes reserved", b.dtb_region_.size);
    const uint32_t ids[] = {b.pl011_int_id_, b.pl031_int_id_, b.timer_int_id_};
    for (uint32_t id : ids)
        INVARIANT(Board::is_taken_int_id(id), "INTID %u, which the kernel cannot take", id);
    const std::pair<uint64_t, uint64_t> windows[] = {{b.gicd_base_, b.gicd_size_}, {b.gicr_base_, b.gicr_size_},
        {b.its_base_, 0}, {b.ecam_base_, b.ecam_size_}, {b.pl011_base_, 0}, {b.pl031_base_, 0},
        {b.pci_mmio32_base_, b.pci_mmio32_size_}, {b.pci_mmio64_base_, b.pci_mmio64_size_}};
    for (auto& w : windows)
        INVARIANT(w.first < REACH && w.second <= REACH - w.first, "a window at 0x%llx of 0x%llx out of reach",
            static_cast<unsigned long long>(w.first), static_cast<unsigned long long>(w.second));
    for (ulong i = 0; i < b.virtio_mmio_count_; i++)
    {
        const auto& v = b.virtio_mmio_[i];
        INVARIANT(v.base < REACH && v.size <= REACH - v.base, "virtio-mmio %lu out of reach", i);
        INVARIANT(v.int_id == 0 || Board::is_taken_int_id(v.int_id), "virtio-mmio %lu on INTID %u", i, v.int_id);
    }
    INVARIANT(b.pci_bus_start_ <= b.pci_bus_end_, "bus range %u..%u", b.pci_bus_start_, b.pci_bus_end_);
}

void check_model(const Board& b, const Expected& e, bool setup)
{
    INVARIANT(b.mem_region_count_ == e.mem.size(), "%lu memory regions, where the tree has %zu", b.mem_region_count_,
        e.mem.size());
    for (size_t i = 0; i < e.mem.size(); i++)
        INVARIANT(b.mem_regions_[i].addr == e.mem[i].first && b.mem_regions_[i].size == e.mem[i].second,
            "memory region %zu is 0x%lx+0x%lx, where the tree has 0x%llx+0x%llx", i, b.mem_regions_[i].addr,
            b.mem_regions_[i].size, static_cast<unsigned long long>(e.mem[i].first),
            static_cast<unsigned long long>(e.mem[i].second));
    INVARIANT(e.boot_args == b.boot_args_, "bootargs '%s', where the tree has '%s'", b.boot_args_, e.boot_args.c_str());
    INVARIANT(b.psci_use_hvc_ == e.psci_use_hvc, "PSCI by %s", b.psci_use_hvc_ ? "hvc" : "smc");
    INVARIANT(b.gicd_base_ == e.gicd_base && b.gicd_size_ == e.gicd_size && b.gicr_base_ == e.gicr_base &&
        b.gicr_size_ == e.gicr_size, "the GIC at 0x%lx/0x%lx, where the tree has 0x%llx/0x%llx", b.gicd_base_,
        b.gicr_base_, static_cast<unsigned long long>(e.gicd_base), static_cast<unsigned long long>(e.gicr_base));
    INVARIANT(b.its_base_ == e.its_base, "the ITS at 0x%lx, where the tree has 0x%llx", b.its_base_,
        static_cast<unsigned long long>(e.its_base));
    INVARIANT(b.pl011_base_ == e.pl011_base && b.pl011_int_id_ == e.pl011_int_id, "the PL011 at 0x%lx on %u, not 0x%llx on %u",
        b.pl011_base_, b.pl011_int_id_, static_cast<unsigned long long>(e.pl011_base), e.pl011_int_id);
    INVARIANT(b.pl031_base_ == e.pl031_base && b.pl031_int_id_ == e.pl031_int_id, "the PL031 at 0x%lx on %u, not 0x%llx on %u",
        b.pl031_base_, b.pl031_int_id_, static_cast<unsigned long long>(e.pl031_base), e.pl031_int_id);
    INVARIANT(b.timer_int_id_ == e.timer_int_id, "the timer on %u, where the tree has %u", b.timer_int_id_, e.timer_int_id);
    INVARIANT(b.ecam_base_ == e.ecam_base && b.ecam_size_ == e.ecam_size, "ECAM at 0x%lx+0x%lx, not 0x%llx+0x%llx",
        b.ecam_base_, b.ecam_size_, static_cast<unsigned long long>(e.ecam_base), static_cast<unsigned long long>(e.ecam_size));
    INVARIANT(b.pci_mmio32_base_ == e.mmio32_base && b.pci_mmio32_size_ == e.mmio32_size && b.pci_mmio64_base_ == e.mmio64_base &&
        b.pci_mmio64_size_ == e.mmio64_size, "the PCI windows are not the tree's");
    INVARIANT(b.pci_bus_start_ == e.bus_start && b.pci_bus_end_ == e.bus_end, "buses %u..%u, where the tree has %u..%u",
        b.pci_bus_start_, b.pci_bus_end_, e.bus_start, e.bus_end);
    INVARIANT(b.virtio_mmio_count_ == e.virtio_mmio.size(), "%lu virtio-mmio windows, where the tree has %zu",
        b.virtio_mmio_count_, e.virtio_mmio.size());
    for (size_t i = 0; i < e.virtio_mmio.size(); i++)
        INVARIANT(b.virtio_mmio_[i].base == e.virtio_mmio[i].base && b.virtio_mmio_[i].size == e.virtio_mmio[i].size &&
            b.virtio_mmio_[i].int_id == e.virtio_mmio[i].int_id, "virtio-mmio %zu is not the tree's", i);
    INVARIANT(b.cpu_count_ == e.cpus.size(), "%lu CPUs, where the tree has %zu", b.cpu_count_, e.cpus.size());
    for (size_t i = 0; i < e.cpus.size(); i++)
        INVARIANT(b.cpu_mpidr_[i] == e.cpus[i], "CPU %zu's MPIDR is 0x%lx, not 0x%llx", i, b.cpu_mpidr_[i],
            static_cast<unsigned long long>(e.cpus[i]));
    INVARIANT(b.refused_ == e.refused, "%lu values refused, where the tree has %llu to refuse", b.refused_,
        static_cast<unsigned long long>(e.refused));
    INVARIANT(setup == (b.dtb_region_.size != 0), "the DTB region is %lu bytes", b.dtb_region_.size);
}

void run(fuzz::Input& in)
{
    Gen g{in};
    Node root = tree(g);
    Blob b;
    b.emit(root);
    std::vector<uint8_t> blob = serialize(b);
    bool damaged = in.chance(80);
    if (damaged)
    {
        damage(in, blob, b);
        fuzz::reached("a damaged tree");
    }
    else
    {
        fuzz::reached("a sound tree");
    }

    /* The blob alone in its allocation, as long as its header says -- what
       is past a DTB in memory is not the DTB's, but it is there to read --
       and never less than a header. */
    size_t said = 0;
    if (blob.size() >= 8)
        said = be32_at(blob, 1);
    size_t size = std::max<size_t>(std::max(blob.size(), HEADER_SIZE), std::min<size_t>(said, Fdt::MAX_SIZE));
    uint8_t* mem = static_cast<uint8_t*>(calloc(1, size));
    memcpy(mem, blob.data(), blob.size());

    Board& board = Board::get_instance();
    board.setup(mem);
    check_sane(board);
    if (!damaged)
    {
        Expected e;
        walk(e, root, 0, 2, 1);
        fallbacks(e);
        check_model(board, e, true);
        if (e.refused != 0)
            fuzz::reached("a sound tree with values refused");
    }
    else if (board.dtb_region_.size != 0)
    {
        fuzz::reached("a damaged tree read");
    }
    free(mem);
}

void reset()
{
    Board& board = Board::get_instance();
    board.~Board();
    new (&board) Board();
}

}

const fuzz::Target fuzz::THE_TARGET = {"fdt", reset, run, 4096, 50000};
