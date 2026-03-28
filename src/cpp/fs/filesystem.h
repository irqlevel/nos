#pragma once

#include <fs/vnode.h>

namespace Kernel
{

class BlockDevice;

class FileSystem
{
public:
    virtual ~FileSystem() {}
    virtual const char* GetName() = 0;
    virtual void GetInfo(char* buf, ulong bufSize) { if (buf && bufSize) buf[0] = '\0'; }
    virtual bool Format(BlockDevice* dev) { (void)dev; return false; }
    virtual bool Mount() { return true; }
    virtual void Unmount() {}
    virtual VNode* GetRoot() = 0;
    virtual VNode* Lookup(VNode* dir, const char* name) = 0;
    virtual VNode* CreateFile(VNode* dir, const char* name) = 0;
    virtual VNode* CreateDir(VNode* dir, const char* name) = 0;
    virtual bool Write(VNode* file, const void* data, ulong len) = 0;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) = 0;
    virtual bool Remove(VNode* node) = 0;
    virtual BlockDevice* GetDevice() { return nullptr; }
};

}
