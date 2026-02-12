#pragma once

#include <fs/filesystem.h>
#include <lib/printer.h>
#include <kernel/mutex.h>

namespace Kernel
{

class Vfs
{
public:
    static Vfs& GetInstance()
    {
        static Vfs instance;
        return instance;
    }

    bool Mount(const char* path, FileSystem* fs);
    FileSystem* Unmount(const char* path);

    bool ListDir(const char* path, Stdlib::Printer& printer);
    bool ReadFile(const char* path, Stdlib::Printer& printer);
    bool WriteFile(const char* path, const void* data, ulong len);
    bool CreateDir(const char* path);
    bool CreateFile(const char* path);
    bool Remove(const char* path);

    void DumpMounts(Stdlib::Printer& printer);

    static const ulong MaxMounts = 16;
    static const ulong MaxPath = 256;

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
    };

    bool ResolvePath(const char* path, FileSystem*& fs, VNode*& node, VNode*& parent, char* lastName, ulong lastNameSize);
    bool FindMount(const char* path, ulong& mountIdx, const char*& remainder);

    MountEntry Mounts[MaxMounts];
    ulong MountCount;
    Mutex Lock;
};

}
