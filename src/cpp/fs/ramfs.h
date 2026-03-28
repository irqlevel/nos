#pragma once

#include <fs/filesystem.h>

namespace Kernel
{

class RamFs : public FileSystem
{
public:
    RamFs();
    virtual ~RamFs();

    virtual const char* GetName() override;
    virtual void Unmount() override;
    virtual VNode* GetRoot() override;
    virtual VNode* Lookup(VNode* dir, const char* name) override;
    virtual VNode* CreateFile(VNode* dir, const char* name) override;
    virtual VNode* CreateDir(VNode* dir, const char* name) override;
    virtual bool Write(VNode* file, const void* data, ulong len) override;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) override;
    virtual bool Remove(VNode* node) override;

private:
    RamFs(const RamFs& other) = delete;
    RamFs(RamFs&& other) = delete;
    RamFs& operator=(const RamFs& other) = delete;
    RamFs& operator=(RamFs&& other) = delete;

    VNode* AllocNode(const char* name, VNode::Type type);
    void FreeNode(VNode* node);
    void FreeTree(VNode* node);

    VNode Root;
};

}
