#include "vfs.h"

#include <block/block_device.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <kernel/trace.h>

/* The VFS itself is Rust (src/rust/fs): the mount table, path resolution and
   the file API. What is left here is the C++ way in -- the same class the
   rest of the kernel has always called -- and the shim that wraps a
   filesystem still written in C++ into the ops table the VFS drives it by.
   Both go when the last filesystem moves. */
extern "C" {

struct RustFsOps
{
    const char* Name;
    void (*Info)(void* ctx, char* buf, unsigned long len);
    Kernel::VNode* (*Root)(void* ctx);
    int (*LoadDir)(void* ctx, Kernel::VNode* dir);
    Kernel::VNode* (*Lookup)(void* ctx, Kernel::VNode* dir, const char* name);
    Kernel::VNode* (*CreateFile)(void* ctx, Kernel::VNode* dir, const char* name);
    Kernel::VNode* (*CreateDir)(void* ctx, Kernel::VNode* dir, const char* name);
    int (*Read)(void* ctx, Kernel::VNode* file, void* buf, unsigned long len, unsigned long off);
    int (*Write)(void* ctx, Kernel::VNode* file, const void* data, unsigned long len, unsigned long off);
    int (*Truncate)(void* ctx, Kernel::VNode* file, unsigned long size);
    int (*Rename)(void* ctx, Kernel::VNode* node, Kernel::VNode* dir, const char* name);
    int (*Remove)(void* ctx, Kernel::VNode* node);
    int (*Sync)(void* ctx);
    unsigned long (*Device)(void* ctx);
    int (*Mount)(void* ctx, int readOnly);
    void (*Unmount)(void* ctx);
    void (*Destroy)(void* ctx);
    void* Ctx;
};

int kernel_vfs_mount(const char* path, unsigned long len, const RustFsOps* ops, int readOnly);
int kernel_vfs_unmount(const char* path, unsigned long len);
void kernel_vfs_unmount_all();
unsigned long kernel_vfs_mount_count();
int kernel_vfs_mount_at(unsigned long index, char* path, unsigned long pathLen,
    const char** name, char* info, unsigned long infoLen);

Kernel::File* kernel_vfs_open(const char* path, unsigned long len, unsigned long flags);
void kernel_vfs_close(Kernel::File* file);
int kernel_vfs_read(Kernel::File* file, void* buf, unsigned long len, unsigned long* out);
int kernel_vfs_write(Kernel::File* file, const void* data, unsigned long len);
int kernel_vfs_seek(Kernel::File* file, unsigned long pos);
unsigned long kernel_vfs_tell(Kernel::File* file);
unsigned long kernel_vfs_size(Kernel::File* file);

int kernel_vfs_stat(const char* path, unsigned long len, Kernel::FileStat* out);
int kernel_vfs_readdir(const char* path, unsigned long len, unsigned long index,
    Kernel::DirEntry* out);
int kernel_vfs_create(const char* path, unsigned long len, int directory);
int kernel_vfs_remove(const char* path, unsigned long len);
int kernel_vfs_truncate(const char* path, unsigned long len, unsigned long size);
int kernel_vfs_rename(const char* from, unsigned long fromLen, const char* to, unsigned long toLen);
int kernel_vfs_sync();
int kernel_vfs_write_file(const char* path, unsigned long len, const void* data, unsigned long dataLen);

}

namespace Kernel
{

/* ReadFile streams a file to a printer through a buffer of this size, so
   the file itself never has to fit in one allocation. */
static const ulong ReadFileChunk = 4096;

namespace
{

/* The shim: a C++ FileSystem seen as the ops table the VFS drives. Every
   call arrives with the VFS lock held, which is the contract FileSystem is
   written to. */

FileSystem* Fs(void* ctx)
{
    return static_cast<FileSystem*>(ctx);
}

void ShimInfo(void* ctx, char* buf, unsigned long len)
{
    Fs(ctx)->GetInfo(buf, len);
}

VNode* ShimRoot(void* ctx)
{
    return Fs(ctx)->GetRoot();
}

int ShimLoadDir(void* ctx, VNode* dir)
{
    return Fs(ctx)->LoadDir(dir) ? 0 : -1;
}

VNode* ShimLookup(void* ctx, VNode* dir, const char* name)
{
    return Fs(ctx)->Lookup(dir, name);
}

VNode* ShimCreateFile(void* ctx, VNode* dir, const char* name)
{
    return Fs(ctx)->CreateFile(dir, name);
}

VNode* ShimCreateDir(void* ctx, VNode* dir, const char* name)
{
    return Fs(ctx)->CreateDir(dir, name);
}

int ShimRead(void* ctx, VNode* file, void* buf, unsigned long len, unsigned long off)
{
    return Fs(ctx)->Read(file, buf, len, off) ? 0 : -1;
}

int ShimWrite(void* ctx, VNode* file, const void* data, unsigned long len, unsigned long off)
{
    return Fs(ctx)->Write(file, data, len, off) ? 0 : -1;
}

int ShimTruncate(void* ctx, VNode* file, unsigned long size)
{
    return Fs(ctx)->Truncate(file, size) ? 0 : -1;
}

int ShimRename(void* ctx, VNode* node, VNode* dir, const char* name)
{
    return Fs(ctx)->Rename(node, dir, name) ? 0 : -1;
}

int ShimRemove(void* ctx, VNode* node)
{
    return Fs(ctx)->Remove(node) ? 0 : -1;
}

int ShimSync(void* ctx)
{
    return Fs(ctx)->Sync() ? 0 : -1;
}

unsigned long ShimDevice(void* ctx)
{
    BlockDevice* dev = Fs(ctx)->GetDevice();
    return dev != nullptr ? dev->GetHandle() : 0;
}

int ShimMount(void* ctx, int readOnly)
{
    FileSystem* fs = Fs(ctx);
    fs->ReadOnly = readOnly != 0;
    if (!fs->Mount())
        return -1;

    /* The filesystem may have found an image it can read but must not
       write; the VFS takes that answer as the mount's. */
    return fs->ReadOnly ? 1 : 0;
}

void ShimUnmount(void* ctx)
{
    Fs(ctx)->Unmount();
}

void ShimDestroy(void* ctx)
{
    delete Fs(ctx);
}

RustFsOps ShimOps(FileSystem* fs)
{
    RustFsOps ops = {};
    ops.Name = fs->GetName();
    ops.Info = ShimInfo;
    ops.Root = ShimRoot;
    ops.LoadDir = ShimLoadDir;
    ops.Lookup = ShimLookup;
    ops.CreateFile = ShimCreateFile;
    ops.CreateDir = ShimCreateDir;
    ops.Read = ShimRead;
    ops.Write = ShimWrite;
    ops.Truncate = ShimTruncate;
    ops.Rename = ShimRename;
    ops.Remove = ShimRemove;
    ops.Sync = ShimSync;
    ops.Device = ShimDevice;
    ops.Mount = ShimMount;
    ops.Unmount = ShimUnmount;
    ops.Destroy = ShimDestroy;
    ops.Ctx = fs;
    return ops;
}

}

Vfs::Vfs()
{
}

Vfs::~Vfs()
{
}

bool Vfs::Mount(const char* path, FileSystem* fs, bool readOnly)
{
    if (path == nullptr || fs == nullptr)
        return false;

    RustFsOps ops = ShimOps(fs);
    return kernel_vfs_mount(path, Stdlib::StrLen(path), &ops, readOnly ? 1 : 0) == 0;
}

bool Vfs::Unmount(const char* path)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_unmount(path, Stdlib::StrLen(path)) == 0;
}

File* Vfs::Open(const char* path, ulong flags)
{
    if (path == nullptr)
        return nullptr;

    return kernel_vfs_open(path, Stdlib::StrLen(path), flags);
}

void Vfs::Close(File* file)
{
    kernel_vfs_close(file);
}

bool Vfs::Read(File* file, void* buf, ulong len, ulong& bytesRead)
{
    bytesRead = 0;
    return kernel_vfs_read(file, buf, len, &bytesRead) == 0;
}

bool Vfs::Write(File* file, const void* data, ulong len)
{
    return kernel_vfs_write(file, data, len) == 0;
}

bool Vfs::Seek(File* file, ulong pos)
{
    return kernel_vfs_seek(file, pos) == 0;
}

ulong Vfs::Tell(File* file)
{
    return kernel_vfs_tell(file);
}

ulong Vfs::GetSize(File* file)
{
    return kernel_vfs_size(file);
}

bool Vfs::Stat(const char* path, FileStat& st)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_stat(path, Stdlib::StrLen(path), &st) == 0;
}

bool Vfs::ReadDir(const char* path, ulong index, DirEntry& entry)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_readdir(path, Stdlib::StrLen(path), index, &entry) == 0;
}

bool Vfs::CreateDir(const char* path)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_create(path, Stdlib::StrLen(path), 1) == 0;
}

bool Vfs::CreateFile(const char* path)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_create(path, Stdlib::StrLen(path), 0) == 0;
}

bool Vfs::Remove(const char* path)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_remove(path, Stdlib::StrLen(path)) == 0;
}

bool Vfs::Truncate(const char* path, ulong size)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_truncate(path, Stdlib::StrLen(path), size) == 0;
}

bool Vfs::Rename(const char* oldPath, const char* newPath)
{
    if (oldPath == nullptr || newPath == nullptr)
        return false;

    return kernel_vfs_rename(oldPath, Stdlib::StrLen(oldPath),
        newPath, Stdlib::StrLen(newPath)) == 0;
}

bool Vfs::Sync()
{
    return kernel_vfs_sync() == 0;
}

bool Vfs::WriteFile(const char* path, const void* data, ulong len)
{
    if (path == nullptr)
        return false;

    return kernel_vfs_write_file(path, Stdlib::StrLen(path), data, len) == 0;
}

void Vfs::UnmountAll()
{
    kernel_vfs_unmount_all();
}

/* What follows is made of the calls above and nothing else: presentation,
   and the two-step replace. */

bool Vfs::ListDir(const char* path, Stdlib::Printer& printer)
{
    FileStat st;
    if (!Stat(path, st))
    {
        printer.Printf("path not found\n");
        return false;
    }

    if (st.Type != VNode::TypeDir)
    {
        printer.Printf("not a directory\n");
        return false;
    }

    for (ulong i = 0; ; i++)
    {
        DirEntry entry;
        if (!ReadDir(path, i, entry))
            break;

        if (entry.Type == VNode::TypeFile)
            printer.Printf("f %u %s\n", entry.Size, entry.Name);
        else
            printer.Printf("d   %s\n", entry.Name);
    }

    return true;
}

bool Vfs::ReadFile(const char* path, Stdlib::Printer& printer)
{
    File* file = Open(path, OpenRead);
    if (file == nullptr)
    {
        printer.Printf("file not found\n");
        return false;
    }

    u8* buf = (u8*)Mm::Alloc(ReadFileChunk, 0);
    if (buf == nullptr)
    {
        Trace(0, "Vfs::ReadFile: alloc %u bytes failed", ReadFileChunk);
        printer.Printf("read failed\n");
        Close(file);
        return false;
    }

    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (!Read(file, buf, ReadFileChunk, got))
        {
            printer.Printf("read failed\n");
            ok = false;
            break;
        }
        if (got == 0)
            break;

        /* Character by character: the data is not NUL-terminated. */
        for (ulong i = 0; i < got; i++)
        {
            char tmp[2] = { (char)buf[i], '\0' };
            printer.PrintString(tmp);
        }
    }

    Mm::Free(buf);
    Close(file);
    if (ok)
        printer.Printf("\n");
    return ok;
}

/* <path>.new: where ReplaceFile puts a file's next content */
static bool ReplacementPath(const char* path, char* out, ulong size)
{
    static const char Suffix[] = ".new";
    ulong len = Stdlib::StrLen(path);
    if (len + sizeof(Suffix) > size)
        return false;
    Stdlib::MemCpy(out, path, len);
    Stdlib::MemCpy(out + len, Suffix, sizeof(Suffix));
    return true;
}

bool Vfs::ReplaceFile(const char* path, const void* data, ulong len)
{
    char next[MaxPath];
    if (!ReplacementPath(path, next, sizeof(next)))
        return false;

    FileStat st;
    if (Stat(path, st) && st.Type != VNode::TypeFile)
    {
        Trace(0, "Vfs::ReplaceFile: %s is not a file", path);
        return false;
    }

    /* The old content stays where it is until all of the new is on disk */
    if (!WriteFile(next, data, len) || !Sync())
    {
        Remove(next);
        return false;
    }
    if (Stat(path, st) && !Remove(path))
        return false;
    if (!Rename(next, path))
        return false;
    return Sync();
}

bool Vfs::Locate(const char* path, char* out, ulong outSize)
{
    FileStat st;
    if (Stat(path, st))
    {
        ulong len = Stdlib::StrLen(path);
        if (len >= outSize)
            return false;
        Stdlib::MemCpy(out, path, len + 1);
        return true;
    }
    return ReplacementPath(path, out, outSize) && Stat(out, st);
}

void Vfs::DumpMounts(Stdlib::Printer& printer)
{
    ulong count = kernel_vfs_mount_count();
    for (ulong i = 0; i < count; i++)
    {
        char path[MaxPath];
        char info[64];
        const char* name = nullptr;

        int readOnly = kernel_vfs_mount_at(i, path, sizeof(path), &name, info, sizeof(info));
        if (readOnly < 0)
            continue;

        const char* rwStr = (readOnly != 0) ? "ro" : "rw";
        if (info[0] != '\0')
            printer.Printf("%s on %s  %s  %s\n", name != nullptr ? name : "?", path, info, rwStr);
        else
            printer.Printf("%s on %s  %s\n", name != nullptr ? name : "?", path, rwStr);
    }
}

}
