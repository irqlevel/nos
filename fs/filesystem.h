#pragma once

#include <fs/vnode.h>

namespace Kernel
{

class FileSystem
{
public:
    virtual ~FileSystem() {}
    virtual const char* GetName() = 0;
    virtual void GetInfo(char* buf, ulong bufSize) { if (buf && bufSize) buf[0] = '\0'; }
    virtual VNode* GetRoot() = 0;
    virtual VNode* Lookup(VNode* dir, const char* name) = 0;
    virtual VNode* CreateFile(VNode* dir, const char* name) = 0;
    virtual VNode* CreateDir(VNode* dir, const char* name) = 0;
    virtual bool Write(VNode* file, const void* data, ulong len) = 0;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) = 0;
    virtual bool Remove(VNode* node) = 0;
};

}
