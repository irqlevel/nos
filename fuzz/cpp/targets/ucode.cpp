// A file of Intel's microcode updates (arch/x86_64/microcode_file.cpp): what
// GRUB loads from /boot/ucode, every size and checksum in it the file's to
// say, and what the BSP chooses its CPU's update from. A file is built from
// the input update by update, as Intel lays one out -- the original 2048-byte
// format, data and total sizes of their own, an extended signature table
// naming other CPUs and platforms, every checksum made right -- for this CPU
// or another, of revisions above and below what it runs; then one update is
// made unsound in a way the SDM rules out, or bytes follow the last, or bytes
// of anything are written over it. Over a file the generator made, Choose is
// held to a model written from the SDM (Vol. 3A, 10.11.1): the newest update
// for the CPU above its revision, of those before the first unsound one. Over
// any file, a choice is an update inside the file, sound by the model's own
// reading of one, for the CPU and newer than it runs -- and ASan holds every
// read to the file.
#include "host.h"

#include <arch/x86_64/microcode.h>

#include "fuzz.h"

#include <string.h>

#include <utility>
#include <vector>

using namespace Kernel;

namespace
{

const size_t HeaderSize = 48;
const size_t ExtHeaderSize = 20;
const size_t ExtSignatureSize = 12;

void Put32(std::vector<uint8_t>& v, size_t at, u32 x)
{
    memcpy(&v[at], &x, 4);
}

u32 Get32(const uint8_t* p, size_t at)
{
    u32 x;
    memcpy(&x, p + at, 4);
    return x;
}

u32 Sum(const uint8_t* p, size_t bytes)
{
    u32 s = 0;
    for (size_t i = 0; i + 4 <= bytes; i += 4)
        s += Get32(p, i);
    return s;
}

/* A CPU of signature `sig` and platform mask `pf` is one an update, or an
   extended signature, of `s` and `f` is for */
bool Fits(u32 sig, u32 pf, u32 s, u32 f)
{
    return sig == s && ((pf == 0 && f == 0) || (pf & f) != 0);
}

/* ---- the model's reading of one update ---- */

struct Read
{
    bool Sound = false;
    size_t Total = 0;
    u32 Revision = 0;
    bool For = false;
};

/* The update at `p`, `room` bytes left in the file: sound by the SDM's rules
   -- versions 1 and 1, data a whole number of dwords, total a whole number of
   KiB and no more than there is room for, header and data summing to 0, and
   what follows the data an extended table exactly, summing to 0, each of its
   signatures summing as the header does -- and for a CPU of `sig` and `pf` */
Read ReadUpdate(const uint8_t* p, size_t room, u32 sig, u32 pf)
{
    Read r;
    if (room < HeaderSize)
        return r;
    if (Get32(p, 0) != 1 || Get32(p, 20) != 1)
        return r;
    u64 data = Get32(p, 28);
    u64 total = Get32(p, 32);
    if (data == 0)
    {
        data = 2000;
        total = 2048;
    }
    if (data % 4 != 0 || total % 1024 != 0 || total < HeaderSize + data || total > room)
        return r;
    if (Sum(p, HeaderSize + data) != 0)
        return r;
    bool fits = Fits(sig, pf, Get32(p, 12), Get32(p, 24));
    u64 ext = total - HeaderSize - data;
    if (ext != 0)
    {
        if (ext < ExtHeaderSize || (ext - ExtHeaderSize) % ExtSignatureSize != 0)
            return r;
        const uint8_t* t = p + HeaderSize + data;
        u32 count = Get32(t, 0);
        if (count != (ext - ExtHeaderSize) / ExtSignatureSize || Sum(t, ext) != 0)
            return r;
        u32 own = Get32(p, 12) + Get32(p, 24) + Get32(p, 16);
        for (u32 i = 0; i < count; i++)
        {
            const uint8_t* es = t + ExtHeaderSize + i * ExtSignatureSize;
            if (Get32(es, 0) + Get32(es, 4) + Get32(es, 8) != own)
                return r;
            if (Fits(sig, pf, Get32(es, 0), Get32(es, 4)))
                fits = true;
        }
    }
    r.Sound = true;
    r.Total = total;
    r.Revision = Get32(p, 4);
    r.For = fits;
    return r;
}

/* ---- the generator ---- */

struct Spec
{
    u32 Sig;
    u32 Pf;
    u32 Revision;
    bool Original;
    std::vector<std::pair<u32, u32>> Ext;
};

/* One update, appended to `file`, sound: its sizes, its data, its extended
   table and every checksum as Intel makes them */
void Append(std::vector<uint8_t>& file, Fuzz::Input& in, const Spec& s)
{
    size_t ext = s.Ext.empty() ? 0 : ExtHeaderSize + s.Ext.size() * ExtSignatureSize;
    size_t data, total;
    if (s.Original)
    {
        data = 2000;
        total = 2048;
    }
    else
    {
        /* The data what is left of a whole number of KiB once the header
           and the table are in: a whole number of dwords, since both are */
        size_t want = HeaderSize + 4 * (1 + in.Below(700)) + ext;
        total = (want + 1023) / 1024 * 1024;
        data = total - HeaderSize - ext;
    }
    size_t at = file.size();
    file.resize(at + total, 0);
    Put32(file, at + 0, 1);
    Put32(file, at + 4, s.Revision);
    Put32(file, at + 8, in.U32());
    Put32(file, at + 12, s.Sig);
    Put32(file, at + 20, 1);
    Put32(file, at + 24, s.Pf);
    Put32(file, at + 28, s.Original ? 0 : static_cast<u32>(data));
    Put32(file, at + 32, s.Original ? 0 : static_cast<u32>(total));
    std::vector<uint8_t> bytes = Fuzz::Noise(in.U32(), data);
    memcpy(&file[at + HeaderSize], bytes.data(), data);
    Put32(file, at + 16, 0u - Sum(&file[at], HeaderSize + data));
    if (ext != 0)
    {
        size_t t = at + HeaderSize + data;
        Put32(file, t, static_cast<u32>(s.Ext.size()));
        u32 own = s.Sig + s.Pf + Get32(&file[0], at + 16);
        for (size_t i = 0; i < s.Ext.size(); i++)
        {
            size_t es = t + ExtHeaderSize + i * ExtSignatureSize;
            Put32(file, es, s.Ext[i].first);
            Put32(file, es + 4, s.Ext[i].second);
            Put32(file, es + 8, own - s.Ext[i].first - s.Ext[i].second);
        }
        Put32(file, t + 4, 0u - Sum(&file[t], ext));
    }
}

/* Make the update at `at` unsound, one way the SDM rules out, for sure */
void Break(std::vector<uint8_t>& file, Fuzz::Input& in, size_t at, size_t end)
{
    u32 data = Get32(&file[0], at + 28);
    bool original = data == 0;
    size_t dataBytes = original ? 2000 : data;
    size_t extAt = at + HeaderSize + dataBytes;
    bool ext = extAt < end;
    switch (in.U8() % (ext ? 8 : 5))
    {
    case 0:
        Put32(file, at, 2);
        Fuzz::Reached("an update of another header version");
        break;
    case 1:
        Put32(file, at + 20, 0);
        Fuzz::Reached("an update for another loader");
        break;
    case 2:
    {
        /* A byte of the data: the dword it is in, and so the sum, changes */
        size_t b = at + HeaderSize + in.Below(dataBytes);
        file[b] ^= static_cast<uint8_t>(1 + in.Below(255));
        Fuzz::Reached("an update whose checksum is wrong");
        break;
    }
    case 3:
        if (original)
        {
            Put32(file, at, 3);
            break;
        }
        /* A total not a whole number of KiB, the checksum made right again */
        Put32(file, at + 32, Get32(&file[0], at + 32) + 4 * static_cast<u32>(1 + in.Below(255)));
        Put32(file, at + 16, 0);
        Put32(file, at + 16, 0u - Sum(&file[at], HeaderSize + dataBytes));
        Fuzz::Reached("an update of a total size no KiB multiple");
        break;
    case 4:
        if (original)
        {
            Put32(file, at + 20, 2);
            break;
        }
        /* A total past the end of the file, a whole number of KiB, the
           checksum made right again */
        Put32(file, at + 32, Get32(&file[0], at + 32) + 1024 * static_cast<u32>(1 + in.Below(64)) +
            static_cast<u32>((file.size() - at + 1023) / 1024 * 1024));
        Put32(file, at + 16, 0);
        Put32(file, at + 16, 0u - Sum(&file[at], HeaderSize + dataBytes));
        Fuzz::Reached("an update longer than the file");
        break;
    case 5:
    {
        /* The table's count not its size's, its checksum made right again */
        Put32(file, extAt, Get32(&file[0], extAt) + 1 + static_cast<u32>(in.Below(4)));
        Put32(file, extAt + 4, 0);
        Put32(file, extAt + 4, 0u - Sum(&file[extAt], end - extAt));
        Fuzz::Reached("an extended table that lies about its count");
        break;
    }
    case 6:
        Put32(file, extAt + 4, Get32(&file[0], extAt + 4) + 1 + static_cast<u32>(in.Below(255)));
        Fuzz::Reached("an extended table whose checksum is wrong");
        break;
    default:
    {
        /* A signature whose checksum is not the update's, the table's own
           made right again */
        u32 count = Get32(&file[0], extAt);
        size_t es = extAt + ExtHeaderSize + in.Below(count) * ExtSignatureSize;
        Put32(file, es + 8, Get32(&file[0], es + 8) + 1 + static_cast<u32>(in.Below(255)));
        Put32(file, extAt + 4, 0);
        Put32(file, extAt + 4, 0u - Sum(&file[extAt], end - extAt));
        Fuzz::Reached("an extended signature whose checksum is wrong");
        break;
    }
    }
}

void Run(Fuzz::Input& in)
{
    /* The CPU, and the signatures and platforms the file's updates are for:
       this CPU's, its platform's neighbours', another stepping's */
    const u32 sig = in.Chance(32) ? in.U32() : 0xB06F2 + static_cast<u32>(in.Below(4));
    const u32 pf = in.Chance(32) ? 0 : 1u << in.Below(8);
    const u32 current = in.Chance(64) ? in.U32() : 0x30 + static_cast<u32>(in.Below(32));
    auto someSig = [&]() { return in.Chance(160) ? sig : sig ^ (1u + static_cast<u32>(in.Below(15))); };
    auto somePf = [&]() { return in.Chance(32) ? 0u : (in.Chance(160) ? pf : 1u << in.Below(8)); };

    std::vector<uint8_t> file;
    std::vector<size_t> starts;
    std::vector<Spec> specs;
    for (size_t n = in.Below(6); n > 0; n--)
    {
        Spec s;
        s.Sig = someSig();
        s.Pf = somePf();
        s.Revision = in.Chance(32) ? in.U32() : current - 4 + static_cast<u32>(in.Below(12));
        s.Original = in.Chance(32);
        if (!s.Original && in.Chance(64))
            for (size_t k = 1 + in.Below(4); k > 0; k--)
                s.Ext.push_back({someSig(), somePf()});
        starts.push_back(file.size());
        Append(file, in, s);
        specs.push_back(s);
    }
    starts.push_back(file.size());

    /* The first update made unsound, if any: the walk ends there */
    size_t broken = specs.size();
    if (!specs.empty() && in.Chance(96))
    {
        broken = in.Below(specs.size());
        Break(file, in, starts[broken], starts[broken + 1]);
    }
    if (in.Chance(32))
    {
        std::vector<uint8_t> tail = Fuzz::Noise(in.U32(), in.Below(64));
        file.insert(file.end(), tail.begin(), tail.end());
        Fuzz::Reached("bytes after the last update");
    }
    const bool noise = in.Chance(16);
    if (noise)
    {
        for (int n = 1 + static_cast<int>(in.Below(8)); n > 0 && !file.empty(); n--)
            file[in.Below(file.size())] = in.U8();
        if (in.Chance(64) && !file.empty())
            file.resize(in.Below(file.size()));
        Fuzz::Reached("a file with bytes of anything in it");
    }

    /* The file alone in an allocation of its own size, so that ASan sees a
       read past it */
    std::vector<uint8_t> exact(file);
    Microcode::Choice c{};
    bool found = Microcode::Choose(exact.data(), exact.size(), sig, pf, current, &c);

    /* Any file: a choice is a sound update inside it, for the CPU, newer */
    if (found)
    {
        INVARIANT(c.Offset <= exact.size() && c.Size <= exact.size() - c.Offset,
            "the update chosen, %zu bytes at %zu, is past the file's %zu", static_cast<size_t>(c.Size),
            static_cast<size_t>(c.Offset), exact.size());
        Read r = ReadUpdate(exact.data() + c.Offset, exact.size() - c.Offset, sig, pf);
        INVARIANT(r.Sound && r.Total == c.Size && r.For && r.Revision == c.Revision && c.Revision > current,
            "the update chosen at %zu is sound %d, %zu bytes (said %zu), for the CPU %d, revision 0x%x (said 0x%x) "
            "over 0x%x", static_cast<size_t>(c.Offset), r.Sound, r.Total, static_cast<size_t>(c.Size), r.For,
            r.Revision, c.Revision, current);
        Fuzz::Reached("an update chosen");
    }
    if (noise)
        return;

    /* A file the generator made: the newest for the CPU, above its revision,
       of the updates before the first unsound one */
    bool want = false;
    u32 best = 0;
    size_t bestAt = 0;
    for (size_t i = 0; i < broken; i++)
    {
        const Spec& s = specs[i];
        bool fits = Fits(sig, pf, s.Sig, s.Pf);
        for (auto& e : s.Ext)
            fits = fits || Fits(sig, pf, e.first, e.second);
        if (fits && s.Revision > current && (!want || s.Revision > best))
        {
            want = true;
            best = s.Revision;
            bestAt = starts[i];
        }
    }
    INVARIANT(found == want && (!want || (c.Revision == best && c.Offset == bestAt)),
        "chose %s 0x%x at %zu, where the newest for the CPU before the first unsound update is %s 0x%x at %zu",
        found ? "" : "none,", c.Revision, static_cast<size_t>(c.Offset), want ? "" : "none,", best, bestAt);
    if (want && broken < specs.size())
        Fuzz::Reached("an update chosen before an unsound one");
    if (!want && broken == 0)
        Fuzz::Reached("an unsound first update, nothing chosen");
}

void Reset()
{
}

}

const Fuzz::Target Fuzz::TheTarget = {"ucode", Reset, Run, 4096, 100000};
