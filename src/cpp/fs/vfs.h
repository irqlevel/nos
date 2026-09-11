#pragma once

#include <fs/filesystem.h>
#include <lib/printer.h>
#include <kernel/mutex.h>

namespace Kernel
{

/* An open file: a position over a vnode. Made by Vfs::Open, released by
   Vfs::Close; every access goes through the Vfs, which holds the vnode in
   place (no remove, rename or unmount under an open handle). */
struct File
{
    FileSystem* Fs;
    VNode* Node;
    ulong Pos;
    ulong Flags;
};

struct FileStat
{
    VNode::Type Type;
    ulong Size;
    ulong Ino;
};

struct DirEntry
{
    char Name[64];
    VNode::Type Type;
    ulong Size;
};

class Vfs
{
public:
    static Vfs& GetInstance()
    {
        static Vfs instance;
        return instance;
    }

    /* Open flags. OpenCreate makes the file if it is missing, OpenTruncate
       empties it, OpenAppend puts every write at the end (and implies
       OpenWrite). */
    static const ulong OpenRead = 1;
    static const ulong OpenWrite = 2;
    static const ulong OpenCreate = 4;
    static const ulong OpenTruncate = 8;
    static const ulong OpenAppend = 16;

    bool Mount(const char* path, FileSystem* fs, bool readOnly = false);
    FileSystem* Unmount(const char* path);

    File* Open(const char* path, ulong flags);
    void Close(File* file);
    /* Read up to len bytes at the current position; bytesRead comes back
       0 at end of file. */
    bool Read(File* file, void* buf, ulong len, ulong& bytesRead);
    bool Write(File* file, const void* data, ulong len);
    bool Seek(File* file, ulong pos);
    ulong Tell(File* file);
    ulong GetSize(File* file);

    bool Stat(const char* path, FileStat& st);
    /* The index-th entry of a directory; false past the end. */
    bool ReadDir(const char* path, ulong index, DirEntry& entry);
    bool Rename(const char* oldPath, const char* newPath);
    bool Truncate(const char* path, ulong size);
    bool Sync();

    bool ListDir(const char* path, Stdlib::Printer& printer);
    bool ReadFile(const char* path, Stdlib::Printer& printer);
    /* Replace the file's content (created if missing). */
    bool WriteFile(const char* path, const void* data, ulong len);
    bool CreateDir(const char* path);
    bool CreateFile(const char* path);
    bool Remove(const char* path);

    void DumpMounts(Stdlib::Printer& printer);
    void UnmountAll();

    static const ulong MaxMounts = 16;
    static const ulong MaxPath = 256;
    static const ulong MaxName = sizeof(VNode::Name);

private:
    Vfs();
    ~Vfs();
    Vfs(const Vfs& other) = delete;
    Vfs(Vfs&& other) = delete;
    Vfs& operator=(const Vfs& other) = delete;
    Vfs& operator=(Vfs&& other) = delete;

    struct MountEntry
    {
        char Path[MaxPath];
        FileSystem* Fs;
        bool ReadOnly;
        ulong Claim;    /* on the filesystem's device, for as long as it is mounted */
    };

    bool ResolvePath(const char* path, FileSystem*& fs, VNode*& node, VNode*& parent, char* lastName, ulong lastNameSize);
    bool FindMount(const char* path, ulong& mountIdx, const char*& remainder);
    bool IsMountReadOnly(const char* path);
    static bool HasOpenFiles(VNode* node);
    static bool IsAncestor(VNode* node, VNode* other);

    MountEntry Mounts[MaxMounts];
    ulong MountCount;
    Mutex Lock;
};

}
