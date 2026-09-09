#include "fstest.h"

#include <fs/vfs.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <kernel/trace.h>

namespace Kernel
{

/* The big file is written in chunks of one size and read back in chunks
   of another, so no chunk boundary lines up with a block boundary twice */
static const ulong FsTestWriteChunk = 64 * 1024;
static const ulong FsTestReadChunk = 12345;

static const ulong FsTestPathLen = Vfs::MaxPath;

static void Report(Stdlib::Printer* out, const char* path, const char* what)
{
    Trace(0, "fstest: %s: %s", path, what);
    if (out != nullptr)
        out->Printf("fstest: %s: %s\n", path, what);
}

/* A byte that depends on its offset alone, so any chunking reads it back */
static inline u8 PatternByte(ulong offset, u8 salt)
{
    return (u8)((offset * 7) ^ (offset >> 8) ^ (offset >> 16) ^ salt);
}

static bool Join(char* dst, const char* dir, const char* name)
{
    ulong dirLen = Stdlib::StrLen(dir);
    bool slash = (dirLen > 0 && dir[dirLen - 1] == '/');
    int n = Stdlib::SnPrintf(dst, FsTestPathLen, slash ? "%s%s" : "%s/%s", dir, name);
    return n > 0 && (ulong)n < FsTestPathLen;
}

static bool ReadWhole(Vfs& vfs, const char* path, void* buf, ulong len, ulong& got)
{
    File* f = vfs.Open(path, Vfs::OpenRead);
    if (f == nullptr)
        return false;

    got = 0;
    bool ok = true;
    while (got < len)
    {
        ulong n = 0;
        if (!vfs.Read(f, (u8*)buf + got, len - got, n))
        {
            ok = false;
            break;
        }
        if (n == 0)
            break;
        got += n;
    }
    vfs.Close(f);
    return ok;
}

static bool CheckContent(Vfs& vfs, const char* path, const char* expect, ulong len, Stdlib::Printer* out)
{
    char buf[64];
    if (len > sizeof(buf))
        return false;

    FileStat st;
    if (!vfs.Stat(path, st) || st.Type != VNode::TypeFile || st.Size != len)
    {
        Report(out, path, "size is wrong");
        return false;
    }

    ulong got = 0;
    if (!ReadWhole(vfs, path, buf, sizeof(buf), got) || got != len ||
        Stdlib::MemCmp(buf, expect, len) != 0)
    {
        Report(out, path, "content is wrong");
        return false;
    }
    return true;
}

static bool BigFile(Vfs& vfs, const char* path, ulong size, Stdlib::Printer* out)
{
    u8* wbuf = (u8*)Mm::Alloc(FsTestWriteChunk, 0);
    u8* rbuf = (u8*)Mm::Alloc(FsTestReadChunk, 0);
    if (wbuf == nullptr || rbuf == nullptr)
    {
        Report(out, path, "alloc failed");
        if (wbuf) Mm::Free(wbuf);
        if (rbuf) Mm::Free(rbuf);
        return false;
    }

    bool ok = false;
    File* f = nullptr;
    ulong pos = 0;
    ulong got = 0;
    ulong n = 0;
    FileStat st;
    const u8 salt = 0x5A;
    const u8 salt2 = 0xC3;
    ulong patchOff = 0;
    ulong patchLen = 0;
    ulong cut = 0;

    f = vfs.Open(path, Vfs::OpenWrite | Vfs::OpenCreate | Vfs::OpenTruncate);
    if (f == nullptr)
    {
        Report(out, path, "create failed");
        goto done;
    }

    /* Sequential write in big chunks */
    while (pos < size)
    {
        ulong chunk = (size - pos < FsTestWriteChunk) ? (size - pos) : FsTestWriteChunk;
        for (ulong i = 0; i < chunk; i++)
            wbuf[i] = PatternByte(pos + i, salt);
        if (!vfs.Write(f, wbuf, chunk))
        {
            Report(out, path, "write failed");
            goto done;
        }
        pos += chunk;
    }
    vfs.Close(f);
    f = nullptr;

    if (!vfs.Stat(path, st) || st.Size != size)
    {
        Report(out, path, "size after write is wrong");
        goto done;
    }

    /* Read back in odd-sized chunks */
    f = vfs.Open(path, Vfs::OpenRead);
    if (f == nullptr)
    {
        Report(out, path, "open for read failed");
        goto done;
    }
    pos = 0;
    for (;;)
    {
        if (!vfs.Read(f, rbuf, FsTestReadChunk, n))
        {
            Report(out, path, "read failed");
            goto done;
        }
        if (n == 0)
            break;
        for (ulong i = 0; i < n; i++)
        {
            if (rbuf[i] != PatternByte(pos + i, salt))
            {
                Report(out, path, "content mismatch after write");
                goto done;
            }
        }
        pos += n;
    }
    vfs.Close(f);
    f = nullptr;
    if (pos != size)
    {
        Report(out, path, "short read");
        goto done;
    }

    /* Overwrite a stretch in the middle, straddling block boundaries */
    patchOff = size / 2 - 100;
    patchLen = 4096 + 300;
    if (patchLen > size - patchOff)
        patchLen = size - patchOff;
    f = vfs.Open(path, Vfs::OpenWrite);
    if (f == nullptr || !vfs.Seek(f, patchOff))
    {
        Report(out, path, "open for patch failed");
        goto done;
    }
    for (ulong i = 0; i < patchLen; i++)
        wbuf[i] = PatternByte(patchOff + i, salt2);
    if (!vfs.Write(f, wbuf, patchLen))
    {
        Report(out, path, "patch write failed");
        goto done;
    }
    vfs.Close(f);
    f = nullptr;

    if (!vfs.Stat(path, st) || st.Size != size)
    {
        Report(out, path, "size after patch is wrong");
        goto done;
    }

    f = vfs.Open(path, Vfs::OpenRead);
    if (f == nullptr)
    {
        Report(out, path, "open after patch failed");
        goto done;
    }
    pos = 0;
    for (;;)
    {
        if (!vfs.Read(f, rbuf, FsTestReadChunk, n))
        {
            Report(out, path, "read after patch failed");
            goto done;
        }
        if (n == 0)
            break;
        for (ulong i = 0; i < n; i++)
        {
            ulong off = pos + i;
            u8 want = (off >= patchOff && off < patchOff + patchLen)
                ? PatternByte(off, salt2) : PatternByte(off, salt);
            if (rbuf[i] != want)
            {
                Report(out, path, "content mismatch after patch");
                goto done;
            }
        }
        pos += n;
    }
    vfs.Close(f);
    f = nullptr;

    /* Cut the file short, then check what is left and that it ends there */
    cut = size / 2 + 33;
    if (!vfs.Truncate(path, cut))
    {
        Report(out, path, "truncate failed");
        goto done;
    }
    if (!vfs.Stat(path, st) || st.Size != cut)
    {
        Report(out, path, "size after truncate is wrong");
        goto done;
    }
    f = vfs.Open(path, Vfs::OpenRead);
    if (f == nullptr)
    {
        Report(out, path, "open after truncate failed");
        goto done;
    }
    pos = 0;
    for (;;)
    {
        if (!vfs.Read(f, rbuf, FsTestReadChunk, n))
        {
            Report(out, path, "read after truncate failed");
            goto done;
        }
        if (n == 0)
            break;
        for (ulong i = 0; i < n; i++)
        {
            ulong off = pos + i;
            u8 want = (off >= patchOff && off < patchOff + patchLen)
                ? PatternByte(off, salt2) : PatternByte(off, salt);
            if (rbuf[i] != want)
            {
                Report(out, path, "content mismatch after truncate");
                goto done;
            }
        }
        pos += n;
    }
    vfs.Close(f);
    f = nullptr;
    if (pos != cut)
    {
        Report(out, path, "wrong length after truncate");
        goto done;
    }

    /* Grow it back past the cut: the gap must read as zeros */
    f = vfs.Open(path, Vfs::OpenWrite | Vfs::OpenAppend);
    if (f == nullptr || !vfs.Write(f, "END", 3))
    {
        Report(out, path, "append after truncate failed");
        goto done;
    }
    vfs.Close(f);
    f = nullptr;
    if (!vfs.Truncate(path, cut + 3 + 5000))
    {
        Report(out, path, "truncate to grow failed");
        goto done;
    }
    f = vfs.Open(path, Vfs::OpenRead);
    if (f == nullptr || !vfs.Seek(f, cut) || !vfs.Read(f, rbuf, 3 + 5000, got) || got != 3 + 5000)
    {
        Report(out, path, "read of the grown tail failed");
        goto done;
    }
    vfs.Close(f);
    f = nullptr;
    if (Stdlib::MemCmp(rbuf, "END", 3) != 0)
    {
        Report(out, path, "appended bytes are wrong");
        goto done;
    }
    for (ulong i = 3; i < 3 + 5000; i++)
    {
        if (rbuf[i] != 0)
        {
            Report(out, path, "grown gap is not zero");
            goto done;
        }
    }

    ok = true;

done:
    if (f != nullptr)
        vfs.Close(f);
    Mm::Free(wbuf);
    Mm::Free(rbuf);
    return ok;
}

bool FsSelfTest(const char* dir, ulong bigSize, Stdlib::Printer* out)
{
    auto& vfs = Vfs::GetInstance();
    FileStat st;

    if (dir == nullptr || !vfs.Stat(dir, st) || st.Type != VNode::TypeDir)
    {
        Report(out, dir ? dir : "(null)", "not a directory");
        return false;
    }

    char base[FsTestPathLen];
    char a[FsTestPathLen];
    char b[FsTestPathLen];
    char sub[FsTestPathLen];
    char c[FsTestPathLen];
    char big[FsTestPathLen];
    if (!Join(base, dir, "fstest.tmp") || !Join(a, base, "a.txt") || !Join(b, base, "b.txt") ||
        !Join(sub, base, "sub") || !Join(c, sub, "c.txt") || !Join(big, base, "big.bin"))
    {
        Report(out, dir, "path too long");
        return false;
    }

    /* Leftovers of an interrupted run */
    if (vfs.Stat(base, st) && !vfs.Remove(base))
    {
        Report(out, base, "cannot remove leftovers");
        return false;
    }

    if (!vfs.CreateDir(base))
    {
        Report(out, base, "mkdir failed");
        return false;
    }

    bool ok = false;
    File* f = nullptr;
    DirEntry de;

    if (!vfs.WriteFile(a, "hello world", 11) || !CheckContent(vfs, a, "hello world", 11, out))
    {
        Report(out, a, "write and read back failed");
        goto done;
    }

    /* Append */
    f = vfs.Open(a, Vfs::OpenAppend);
    if (f == nullptr || !vfs.Write(f, " again", 6))
    {
        Report(out, a, "append failed");
        goto done;
    }
    vfs.Close(f);
    f = nullptr;
    if (!CheckContent(vfs, a, "hello world again", 17, out))
        goto done;

    /* Write at an offset */
    f = vfs.Open(a, Vfs::OpenWrite);
    if (f == nullptr || !vfs.Seek(f, 6) || !vfs.Write(f, "WORLD", 5))
    {
        Report(out, a, "write at offset failed");
        goto done;
    }
    vfs.Close(f);
    f = nullptr;
    if (!CheckContent(vfs, a, "hello WORLD again", 17, out))
        goto done;

    /* Shrink, then grow: the gap reads as zeros */
    if (!vfs.Truncate(a, 5) || !CheckContent(vfs, a, "hello", 5, out))
    {
        Report(out, a, "truncate failed");
        goto done;
    }
    if (!vfs.Truncate(a, 8) || !CheckContent(vfs, a, "hello\0\0\0", 8, out))
    {
        Report(out, a, "truncate to grow failed");
        goto done;
    }

    /* Rename in place, then move into a subdirectory */
    if (!vfs.Rename(a, b) || vfs.Stat(a, st) || !CheckContent(vfs, b, "hello\0\0\0", 8, out))
    {
        Report(out, a, "rename failed");
        goto done;
    }
    if (!vfs.CreateDir(sub) || !vfs.Rename(b, c) || vfs.Stat(b, st) ||
        !CheckContent(vfs, c, "hello\0\0\0", 8, out))
    {
        Report(out, b, "move failed");
        goto done;
    }
    if (!vfs.ReadDir(sub, 0, de) || Stdlib::StrCmp(de.Name, "c.txt") != 0 || de.Size != 8 ||
        vfs.ReadDir(sub, 1, de))
    {
        Report(out, sub, "readdir is wrong");
        goto done;
    }

    /* Refusals: a missing file, a directory as a file, a duplicate name */
    if (vfs.Open(b, Vfs::OpenRead) != nullptr || vfs.Open(sub, Vfs::OpenRead) != nullptr ||
        vfs.CreateDir(sub) || vfs.CreateFile(c))
    {
        Report(out, sub, "an operation that should fail succeeded");
        goto done;
    }

    if (bigSize > 0 && !BigFile(vfs, big, bigSize, out))
        goto done;

    /* Everything goes with the directory */
    if (!vfs.Remove(base) || vfs.Stat(base, st) || vfs.Stat(c, st))
    {
        Report(out, base, "recursive remove failed");
        goto done;
    }

    if (!vfs.Sync())
    {
        Report(out, dir, "sync failed");
        goto done;
    }

    ok = true;

done:
    if (f != nullptr)
        vfs.Close(f);
    if (!ok)
        vfs.Remove(base);
    return ok;
}

}
