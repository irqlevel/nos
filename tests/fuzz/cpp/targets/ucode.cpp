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

using namespace kernel;

namespace
{

const size_t HEADER_SIZE = 48;
const size_t EXT_HEADER_SIZE = 20;
const size_t EXT_SIGNATURE_SIZE = 12;

void put32(std::vector<uint8_t>& v, size_t at, u32 x)
{
    memcpy(&v[at], &x, 4);
}

u32 get32(const uint8_t* p, size_t at)
{
    u32 x;
    memcpy(&x, p + at, 4);
    return x;
}

u32 sum(const uint8_t* p, size_t bytes)
{
    u32 s = 0;
    for (size_t i = 0; i + 4 <= bytes; i += 4)
        s += get32(p, i);
    return s;
}

/* A CPU of signature `sig` and platform mask `pf` is one an update, or an
   extended signature, of `s` and `f` is for */
bool fits_cpu(u32 sig, u32 pf, u32 s, u32 f)
{
    return sig == s && ((pf == 0 && f == 0) || (pf & f) != 0);
}

/* ---- the model's reading of one update ---- */

struct Read
{
    bool sound = false;
    size_t total = 0;
    u32 revision = 0;
    bool for_cpu = false;
};

/* The update at `p`, `room` bytes left in the file: sound by the SDM's rules
   -- versions 1 and 1, data a whole number of dwords, total a whole number of
   KiB and no more than there is room for, header and data summing to 0, and
   what follows the data an extended table exactly, summing to 0, each of its
   signatures summing as the header does -- and for a CPU of `sig` and `pf` */
Read read_update(const uint8_t* p, size_t room, u32 sig, u32 pf)
{
    Read r;
    if (room < HEADER_SIZE)
        return r;
    if (get32(p, 0) != 1 || get32(p, 20) != 1)
        return r;
    u64 data = get32(p, 28);
    u64 total = get32(p, 32);
    if (data == 0)
    {
        data = 2000;
        total = 2048;
    }
    if (data % 4 != 0 || total % 1024 != 0 || total < HEADER_SIZE + data || total > room)
        return r;
    if (sum(p, HEADER_SIZE + data) != 0)
        return r;
    bool fits = fits_cpu(sig, pf, get32(p, 12), get32(p, 24));
    u64 ext = total - HEADER_SIZE - data;
    if (ext != 0)
    {
        if (ext < EXT_HEADER_SIZE || (ext - EXT_HEADER_SIZE) % EXT_SIGNATURE_SIZE != 0)
            return r;
        const uint8_t* t = p + HEADER_SIZE + data;
        u32 count = get32(t, 0);
        if (count != (ext - EXT_HEADER_SIZE) / EXT_SIGNATURE_SIZE || sum(t, ext) != 0)
            return r;
        u32 own = get32(p, 12) + get32(p, 24) + get32(p, 16);
        for (u32 i = 0; i < count; i++)
        {
            const uint8_t* es = t + EXT_HEADER_SIZE + i * EXT_SIGNATURE_SIZE;
            if (get32(es, 0) + get32(es, 4) + get32(es, 8) != own)
                return r;
            if (fits_cpu(sig, pf, get32(es, 0), get32(es, 4)))
                fits = true;
        }
    }
    r.sound = true;
    r.total = total;
    r.revision = get32(p, 4);
    r.for_cpu = fits;
    return r;
}

/* ---- the generator ---- */

struct Spec
{
    u32 sig;
    u32 pf;
    u32 revision;
    bool original;
    std::vector<std::pair<u32, u32>> ext;
};

/* One update, appended to `file`, sound: its sizes, its data, its extended
   table and every checksum as Intel makes them */
void append(std::vector<uint8_t>& file, fuzz::Input& in, const Spec& s)
{
    size_t ext = s.ext.empty() ? 0 : EXT_HEADER_SIZE + s.ext.size() * EXT_SIGNATURE_SIZE;
    size_t data, total;
    if (s.original)
    {
        data = 2000;
        total = 2048;
    }
    else
    {
        /* The data what is left of a whole number of KiB once the header
           and the table are in: a whole number of dwords, since both are */
        size_t want = HEADER_SIZE + 4 * (1 + in.below(700)) + ext;
        total = (want + 1023) / 1024 * 1024;
        data = total - HEADER_SIZE - ext;
    }
    size_t at = file.size();
    file.resize(at + total, 0);
    put32(file, at + 0, 1);
    put32(file, at + 4, s.revision);
    put32(file, at + 8, in.u32());
    put32(file, at + 12, s.sig);
    put32(file, at + 20, 1);
    put32(file, at + 24, s.pf);
    put32(file, at + 28, s.original ? 0 : static_cast<u32>(data));
    put32(file, at + 32, s.original ? 0 : static_cast<u32>(total));
    std::vector<uint8_t> bytes = fuzz::noise(in.u32(), data);
    memcpy(&file[at + HEADER_SIZE], bytes.data(), data);
    put32(file, at + 16, 0u - sum(&file[at], HEADER_SIZE + data));
    if (ext != 0)
    {
        size_t t = at + HEADER_SIZE + data;
        put32(file, t, static_cast<u32>(s.ext.size()));
        u32 own = s.sig + s.pf + get32(&file[0], at + 16);
        for (size_t i = 0; i < s.ext.size(); i++)
        {
            size_t es = t + EXT_HEADER_SIZE + i * EXT_SIGNATURE_SIZE;
            put32(file, es, s.ext[i].first);
            put32(file, es + 4, s.ext[i].second);
            put32(file, es + 8, own - s.ext[i].first - s.ext[i].second);
        }
        put32(file, t + 4, 0u - sum(&file[t], ext));
    }
}

/* Make the update at `at` unsound, one way the SDM rules out, for sure */
void break_update(std::vector<uint8_t>& file, fuzz::Input& in, size_t at, size_t end)
{
    u32 data = get32(&file[0], at + 28);
    bool original = data == 0;
    size_t data_bytes = original ? 2000 : data;
    size_t ext_at = at + HEADER_SIZE + data_bytes;
    bool ext = ext_at < end;
    switch (in.u8() % (ext ? 8 : 5))
    {
    case 0:
        put32(file, at, 2);
        fuzz::reached("an update of another header version");
        break;
    case 1:
        put32(file, at + 20, 0);
        fuzz::reached("an update for another loader");
        break;
    case 2:
    {
        /* A byte of the data: the dword it is in, and so the sum, changes */
        size_t b = at + HEADER_SIZE + in.below(data_bytes);
        file[b] ^= static_cast<uint8_t>(1 + in.below(255));
        fuzz::reached("an update whose checksum is wrong");
        break;
    }
    case 3:
        if (original)
        {
            put32(file, at, 3);
            break;
        }
        /* A total not a whole number of KiB, the checksum made right again */
        put32(file, at + 32, get32(&file[0], at + 32) + 4 * static_cast<u32>(1 + in.below(255)));
        put32(file, at + 16, 0);
        put32(file, at + 16, 0u - sum(&file[at], HEADER_SIZE + data_bytes));
        fuzz::reached("an update of a total size no KiB multiple");
        break;
    case 4:
        if (original)
        {
            put32(file, at + 20, 2);
            break;
        }
        /* A total past the end of the file, a whole number of KiB, the
           checksum made right again */
        put32(file, at + 32, get32(&file[0], at + 32) + 1024 * static_cast<u32>(1 + in.below(64)) +
            static_cast<u32>((file.size() - at + 1023) / 1024 * 1024));
        put32(file, at + 16, 0);
        put32(file, at + 16, 0u - sum(&file[at], HEADER_SIZE + data_bytes));
        fuzz::reached("an update longer than the file");
        break;
    case 5:
    {
        /* The table's count not its size's, its checksum made right again */
        put32(file, ext_at, get32(&file[0], ext_at) + 1 + static_cast<u32>(in.below(4)));
        put32(file, ext_at + 4, 0);
        put32(file, ext_at + 4, 0u - sum(&file[ext_at], end - ext_at));
        fuzz::reached("an extended table that lies about its count");
        break;
    }
    case 6:
        put32(file, ext_at + 4, get32(&file[0], ext_at + 4) + 1 + static_cast<u32>(in.below(255)));
        fuzz::reached("an extended table whose checksum is wrong");
        break;
    default:
    {
        /* A signature whose checksum is not the update's, the table's own
           made right again */
        u32 count = get32(&file[0], ext_at);
        size_t es = ext_at + EXT_HEADER_SIZE + in.below(count) * EXT_SIGNATURE_SIZE;
        put32(file, es + 8, get32(&file[0], es + 8) + 1 + static_cast<u32>(in.below(255)));
        put32(file, ext_at + 4, 0);
        put32(file, ext_at + 4, 0u - sum(&file[ext_at], end - ext_at));
        fuzz::reached("an extended signature whose checksum is wrong");
        break;
    }
    }
}

void run(fuzz::Input& in)
{
    /* The CPU, and the signatures and platforms the file's updates are for:
       this CPU's, its platform's neighbours', another stepping's */
    const u32 sig = in.chance(32) ? in.u32() : 0xB06F2 + static_cast<u32>(in.below(4));
    const u32 pf = in.chance(32) ? 0 : 1u << in.below(8);
    const u32 current = in.chance(64) ? in.u32() : 0x30 + static_cast<u32>(in.below(32));
    auto some_sig = [&]() { return in.chance(160) ? sig : sig ^ (1u + static_cast<u32>(in.below(15))); };
    auto some_pf = [&]() { return in.chance(32) ? 0u : (in.chance(160) ? pf : 1u << in.below(8)); };

    std::vector<uint8_t> file;
    std::vector<size_t> starts;
    std::vector<Spec> specs;
    for (size_t n = in.below(6); n > 0; n--)
    {
        Spec s;
        s.sig = some_sig();
        s.pf = some_pf();
        s.revision = in.chance(32) ? in.u32() : current - 4 + static_cast<u32>(in.below(12));
        s.original = in.chance(32);
        if (!s.original && in.chance(64))
            for (size_t k = 1 + in.below(4); k > 0; k--)
                s.ext.push_back({some_sig(), some_pf()});
        starts.push_back(file.size());
        append(file, in, s);
        specs.push_back(s);
    }
    starts.push_back(file.size());

    /* The first update made unsound, if any: the walk ends there */
    size_t broken = specs.size();
    if (!specs.empty() && in.chance(96))
    {
        broken = in.below(specs.size());
        break_update(file, in, starts[broken], starts[broken + 1]);
    }
    if (in.chance(32))
    {
        std::vector<uint8_t> tail = fuzz::noise(in.u32(), in.below(64));
        file.insert(file.end(), tail.begin(), tail.end());
        fuzz::reached("bytes after the last update");
    }
    const bool noise = in.chance(16);
    if (noise)
    {
        for (int n = 1 + static_cast<int>(in.below(8)); n > 0 && !file.empty(); n--)
            file[in.below(file.size())] = in.u8();
        if (in.chance(64) && !file.empty())
            file.resize(in.below(file.size()));
        fuzz::reached("a file with bytes of anything in it");
    }

    /* The file alone in an allocation of its own size, so that ASan sees a
       read past it */
    std::vector<uint8_t> exact(file);
    microcode::Choice c{};
    bool found = microcode::choose(exact.data(), exact.size(), sig, pf, current, &c);

    /* Any file: a choice is a sound update inside it, for the CPU, newer */
    if (found)
    {
        INVARIANT(c.offset <= exact.size() && c.size <= exact.size() - c.offset,
            "the update chosen, %zu bytes at %zu, is past the file's %zu", static_cast<size_t>(c.size),
            static_cast<size_t>(c.offset), exact.size());
        Read r = read_update(exact.data() + c.offset, exact.size() - c.offset, sig, pf);
        INVARIANT(r.sound && r.total == c.size && r.for_cpu && r.revision == c.revision && c.revision > current,
            "the update chosen at %zu is sound %d, %zu bytes (said %zu), for the CPU %d, revision 0x%x (said 0x%x) "
            "over 0x%x", static_cast<size_t>(c.offset), r.sound, r.total, static_cast<size_t>(c.size), r.for_cpu,
            r.revision, c.revision, current);
        fuzz::reached("an update chosen");
    }
    if (noise)
        return;

    /* A file the generator made: the newest for the CPU, above its revision,
       of the updates before the first unsound one */
    bool want = false;
    u32 best = 0;
    size_t best_at = 0;
    for (size_t i = 0; i < broken; i++)
    {
        const Spec& s = specs[i];
        bool fits = fits_cpu(sig, pf, s.sig, s.pf);
        for (auto& e : s.ext)
            fits = fits || fits_cpu(sig, pf, e.first, e.second);
        if (fits && s.revision > current && (!want || s.revision > best))
        {
            want = true;
            best = s.revision;
            best_at = starts[i];
        }
    }
    INVARIANT(found == want && (!want || (c.revision == best && c.offset == best_at)),
        "chose %s 0x%x at %zu, where the newest for the CPU before the first unsound update is %s 0x%x at %zu",
        found ? "" : "none,", c.revision, static_cast<size_t>(c.offset), want ? "" : "none,", best, best_at);
    if (want && broken < specs.size())
        fuzz::reached("an update chosen before an unsound one");
    if (!want && broken == 0)
        fuzz::reached("an unsound first update, nothing chosen");
}

void reset()
{
}

}

const fuzz::Target fuzz::THE_TARGET = {"ucode", reset, run, 4096, 100000};
