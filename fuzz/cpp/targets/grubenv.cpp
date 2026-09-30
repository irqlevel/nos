// The GRUB environment block (lib/grub_env.cpp): the file `grubenv` reads
// off /boot and writes back for GRUB's next boot. What is on a disk is
// whoever wrote it's to choose, so the block is anything: what grub-editenv
// writes, that damaged, bytes. Over a block the model can read -- comments,
// blank lines and name=value lines, then '#' to the end -- every call is held
// to what grub_env.h says it does: Set and Unset change exactly the line the
// model says, or say no and change nothing, room counted to the byte; Get
// and ForEach give back what the model holds. Over any block, a Set that
// says yes is what Get gives back, one that says no leaves every byte as it
// was, and a block Set or Unset wrote is one ForEach reads to its end, with
// the newline and the '#' padding GRUB's own writer looks for.
#include "host.h"

#include <lib/grub_env.h>

#include "fuzz.h"

#include <string.h>

#include <algorithm>
#include <string>
#include <vector>

namespace
{

using Stdlib::GrubEnvBlock;

struct Var
{
    std::string Name;
    std::string Value;
};

const char* const Names[] = {"a", "b", "saved_entry", "next_entry", "nos_next", "x_1", "A", "aa", "boot_once"};

/* Names GRUB would not take. */
const char* const BadNames[] = {"", "a b", "a=b", "a\\b", "a\nb", "-x", "a.b"};

/* Names a block may have all the same. */
const char* const OddNames[] = {"a-b", "x.y", "with space",
                                "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn"};

std::string Escape(const std::string& v)
{
    std::string e;
    for (char c : v)
    {
        if (c == '\\' || c == '\n')
            e += '\\';
        e += c;
    }
    return e;
}

bool ValidName(const std::string& n)
{
    if (n.empty() || n.size() > GrubEnvBlock::MaxNameLen)
        return false;
    for (char c : n)
    {
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_'))
            return false;
    }
    return true;
}

/* A name to ask about: one GRUB takes, one it does not, one the block has. */
std::string Name(Fuzz::Input& in, const std::vector<Var>& vars)
{
    switch (in.U8() % 8)
    {
    case 0:
        return std::string(in.Range(60, 70), 'a');
    case 1:
        return in.Pick(BadNames);
    case 2:
    case 3:
        if (!vars.empty())
            return vars[in.Below(vars.size())].Name;
        [[fallthrough]];
    default:
        return Names[in.Below(sizeof(Names) / sizeof(Names[0]))];
    }
}

/* A name a line of a block may have: one GRUB takes, mostly. */
std::string LineName(Fuzz::Input& in)
{
    if (in.Chance(20))
        return in.Pick(OddNames);
    return Names[in.Below(sizeof(Names) / sizeof(Names[0]))];
}

std::string Value(Fuzz::Input& in)
{
    size_t n = in.Chance(16) ? in.Range(250, 600) : in.Below(40);
    std::string v;
    for (size_t i = 0; i < n; i++)
    {
        uint8_t c = in.U8();
        v += (c < 48) ? "\\\n=#\r "[c % 6] : static_cast<char>('a' + c % 26);
    }
    return v;
}

std::string Cut(const std::string& v, size_t n)
{
    return v.substr(0, std::min(n, v.size()));
}

/* ForEach's visit, kept. */
struct Visits
{
    std::vector<Var> Seen;
    size_t StopAfter;
};

bool Visit(const char* name, const char* value, void* ctx)
{
    auto* v = static_cast<Visits*>(ctx);
    v->Seen.push_back({name, value});
    return v->Seen.size() < v->StopAfter;
}

bool Tail(const char* b, size_t size)
{
    size_t pos = size;
    while (pos > GrubEnvBlock::SignatureLen && b[pos - 1] == '#')
        pos--;
    return b[pos - 1] == '\n';
}

void CheckReadsAsModel(GrubEnvBlock& env, const std::vector<Var>& vars, const char* after)
{
    Visits v{{}, ~0UL};
    INVARIANT(env.ForEach(Visit, &v), "ForEach calls a block the model reads malformed, after %s", after);
    INVARIANT(v.Seen.size() == vars.size(), "ForEach visits %zu variables, not %zu, after %s", v.Seen.size(),
        vars.size(), after);
    for (size_t i = 0; i < vars.size(); i++)
    {
        INVARIANT(v.Seen[i].Name == vars[i].Name && v.Seen[i].Value == Cut(vars[i].Value, GrubEnvBlock::MaxValueLen),
            "ForEach's variable %zu is '%s', not '%s', after %s", i, v.Seen[i].Name.c_str(), vars[i].Name.c_str(),
            after);
    }
}

void Run(Fuzz::Input& in)
{
    size_t size;
    switch (in.U8() % 4)
    {
    case 0:
        size = GrubEnvBlock::MinSize + in.Below(64);
        break;
    case 1:
        size = GrubEnvBlock::MinSize + in.Below(4096);
        break;
    default:
        size = GrubEnvBlock::DefaultSize;
        break;
    }
    /* The block alone in its allocation: a step past either end is ASan's. */
    char* b = static_cast<char*>(malloc(size));
    GrubEnvBlock::Format(b, size);
    GrubEnvBlock env(b, size);

    /* The model holds while the block is one it can read. */
    std::vector<Var> vars;
    size_t used = GrubEnvBlock::SignatureLen;
    bool modelled = true;
    switch (in.U8() % 8)
    {
    case 0:
        /* Bytes: whatever a disk holds past the signature. */
        for (size_t i = GrubEnvBlock::SignatureLen; i < size && in.More(); i++)
            b[i] = static_cast<char>(in.U8());
        modelled = false;
        Fuzz::Reached("a block of bytes");
        break;
    default:
    {
        /* Lines grub-editenv could have written. */
        for (size_t lines = in.Below(10); lines > 0; lines--)
        {
            std::string line;
            Var var;
            bool isVar = false;
            switch (in.U8() % 8)
            {
            case 0:
                line = "# a comment, a\\b in it\n";
                break;
            case 1:
                line = in.Chance(128) ? "\n" : "\r\n";
                break;
            default:
                var = {LineName(in), Value(in)};
                line = var.Name + "=" + Escape(var.Value) + "\n";
                isVar = true;
                break;
            }
            if (used + line.size() > size)
                break;
            memcpy(b + used, line.data(), line.size());
            used += line.size();
            if (isVar)
                vars.push_back(var);
        }
        if (in.Chance(40))
        {
            /* And then damaged: a line with no '=', a byte of anything, an
               escape where the last newline was. */
            size_t at = GrubEnvBlock::SignatureLen + in.Below(size - GrubEnvBlock::SignatureLen);
            switch (in.U8() % 3)
            {
            case 0:
                if (used + 2 <= size)
                    memcpy(b + GrubEnvBlock::SignatureLen, "b\n", 2);
                break;
            case 1:
                b[at] = static_cast<char>(in.U8());
                break;
            default:
                if (used > GrubEnvBlock::SignatureLen + 1)
                    b[used - 2] = '\\';
                break;
            }
            modelled = false;
            Fuzz::Reached("a block damaged");
        }
        break;
    }
    }
    if (modelled)
    {
        CheckReadsAsModel(env, vars, "the block was written");
        Fuzz::Reached("a block the model reads");
    }

    std::vector<char> before(size);
    for (int ops = 0; ops < 24 && in.More(); ops++)
    {
        std::string name = Name(in, vars);
        memcpy(before.data(), b, size);
        switch (in.U8() % 4)
        {
        case 0:
        {
            std::string value = Value(in);
            bool said = env.Set(name.c_str(), value.c_str());
            Fuzz::Say("Set('%s', '%s') = %d", name.c_str(), value.c_str(), said);
            if (modelled)
            {
                /* The model's answer, room counted to the byte. */
                bool want = false;
                auto it = std::find_if(vars.begin(), vars.end(), [&](const Var& v) { return v.Name == name; });
                if (ValidName(name))
                {
                    size_t newLen = Escape(value).size();
                    if (it != vars.end())
                    {
                        size_t oldLen = Escape(it->Value).size();
                        want = newLen <= oldLen || newLen - oldLen <= size - used;
                        if (want)
                        {
                            used = used + newLen - oldLen;
                            it->Value = value;
                        }
                    }
                    else
                    {
                        want = name.size() + 1 + newLen + 1 <= size - used;
                        if (want)
                        {
                            used += name.size() + 1 + newLen + 1;
                            vars.push_back({name, value});
                        }
                    }
                }
                INVARIANT(said == want, "Set('%s') of %zu bytes said %d where the model says %d", name.c_str(),
                    value.size(), said, want);
                if (said)
                    CheckReadsAsModel(env, vars, "a Set");
            }
            if (said)
            {
                char got[GrubEnvBlock::MaxValueLen + 1];
                INVARIANT(env.Get(name.c_str(), got, sizeof(got)), "a Set of '%s' that said yes is not there to Get",
                    name.c_str());
                INVARIANT(Cut(value, GrubEnvBlock::MaxValueLen) == got, "Get('%s') gives back other than Set put",
                    name.c_str());
                Visits v{{}, ~0UL};
                INVARIANT(env.ForEach(Visit, &v), "a block Set wrote is one ForEach calls malformed");
                INVARIANT(Tail(b, size), "a Set left no newline before the padding");
                Fuzz::Reached("a Set");
            }
            else
            {
                INVARIANT(memcmp(before.data(), b, size) == 0, "a Set of '%s' that said no changed the block",
                    name.c_str());
            }
            break;
        }
        case 1:
        {
            bool said = env.Unset(name.c_str());
            Fuzz::Say("Unset('%s') = %d", name.c_str(), said);
            if (modelled)
            {
                auto it = std::find_if(vars.begin(), vars.end(), [&](const Var& v) { return v.Name == name; });
                bool want = ValidName(name) && it != vars.end();
                INVARIANT(said == want, "Unset('%s') said %d where the model says %d", name.c_str(), said, want);
                if (want)
                {
                    used -= name.size() + 1 + Escape(it->Value).size() + 1;
                    vars.erase(it);
                    CheckReadsAsModel(env, vars, "an Unset");
                }
            }
            if (said)
            {
                Visits v{{}, ~0UL};
                INVARIANT(env.ForEach(Visit, &v), "a block Unset wrote is one ForEach calls malformed");
                INVARIANT(Tail(b, size), "an Unset left no newline before the padding");
                Fuzz::Reached("an Unset");
            }
            else
            {
                INVARIANT(memcmp(before.data(), b, size) == 0, "an Unset of '%s' that said no changed the block",
                    name.c_str());
            }
            break;
        }
        case 2:
        {
            size_t cap = 1 + in.Below(GrubEnvBlock::MaxValueLen + 1);
            std::vector<char> got(cap);
            bool found = env.Get(name.c_str(), got.data(), cap);
            if (modelled)
            {
                auto it = std::find_if(vars.begin(), vars.end(), [&](const Var& v) { return v.Name == name; });
                bool want = ValidName(name) && it != vars.end();
                INVARIANT(found == want, "Get('%s') found %d where the model says %d", name.c_str(), found, want);
                if (found)
                    INVARIANT(Cut(Cut(it->Value, GrubEnvBlock::MaxValueLen), cap - 1) == got.data(),
                        "Get('%s') into %zu bytes gives back other than the model's value", name.c_str(), cap);
            }
            if (found)
                INVARIANT(strlen(got.data()) < cap, "Get wrote past what it was given");
            break;
        }
        default:
        {
            Visits v{{}, 1 + in.Below(8)};
            bool whole = env.ForEach(Visit, &v);
            if (modelled)
            {
                INVARIANT(whole, "ForEach calls a block the model reads malformed");
                for (size_t i = 0; i < v.Seen.size(); i++)
                    INVARIANT(i < vars.size() && v.Seen[i].Name == vars[i].Name, "ForEach visits past the model");
            }
            break;
        }
        }
        memcpy(before.data(), b, size);
    }
    free(b);
}

void Reset()
{
}

}

const Fuzz::Target Fuzz::TheTarget = {"grubenv", Reset, Run, 2048, 200000};
