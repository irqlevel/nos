#pragma once

#include <fs/vnode.h>

namespace Kernel
{

class BlockDevice;

/* The contract between the Vfs and a filesystem. Every call arrives under
   the Vfs lock, so an implementation needs no locking of its own: two calls
   never overlap on the same filesystem. Paths are the Vfs's business; a
   filesystem sees vnodes and single names. */
class FileSystem
{
public:
    FileSystem()
        : ReadOnly(false)
        , OpenFiles(0)
    {
    }

    virtual ~FileSystem() {}
    virtual const char* GetName() = 0;
    virtual void GetInfo(char* buf, ulong bufSize) { if (buf && bufSize) buf[0] = '\0'; }
    virtual bool Format(BlockDevice* dev) { (void)dev; return false; }
    virtual bool Mount() { return true; }
    virtual void Unmount() {}
    virtual VNode* GetRoot() = 0;

    /* Make dir->Children complete. A filesystem that reads directories on
       demand loads them here; the Vfs calls it before walking Children. */
    virtual bool LoadDir(VNode* dir) { (void)dir; return true; }
    virtual VNode* Lookup(VNode* dir, const char* name) = 0;
    virtual VNode* CreateFile(VNode* dir, const char* name) = 0;
    virtual VNode* CreateDir(VNode* dir, const char* name) = 0;

    /* Write len bytes at offset. The file grows as needed; a gap between
       the old end and offset reads back as zeros. */
    virtual bool Write(VNode* file, const void* data, ulong len, ulong offset) = 0;

    /* Read len bytes at offset. The caller keeps offset + len within
       file->Size, which the filesystem maintains. */
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) = 0;

    virtual bool Truncate(VNode* file, ulong size) = 0;

    /* Move node under newDir as newName; newDir may be its current parent. */
    virtual bool Rename(VNode* node, VNode* newDir, const char* newName) = 0;

    /* Remove a file, or a directory with everything under it. */
    virtual bool Remove(VNode* node) = 0;

    /* Push everything written so far to stable storage. */
    virtual bool Sync() { return true; }

    virtual BlockDevice* GetDevice() { return nullptr; }

    /* Set by the Vfs before Mount() from the mount's flag. A filesystem that
       finds an image it can read but must not write (ext2 with features this
       driver does not maintain) sets it itself in Mount(), and the Vfs then
       mounts read-only. */
    bool ReadOnly;

    /* File handles open on this filesystem; Unmount refuses while non-zero. */
    ulong OpenFiles;
};

}
