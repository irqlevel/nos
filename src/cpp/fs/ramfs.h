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
    virtual bool Write(VNode* file, const void* data, ulong len, ulong offset) override;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) override;
    virtual bool Truncate(VNode* file, ulong size) override;
    virtual bool Rename(VNode* node, VNode* newDir, const char* newName) override;
    virtual bool Remove(VNode* node) override;

private:
    RamFs(const RamFs& other) = delete;
    RamFs(RamFs&& other) = delete;
    RamFs& operator=(const RamFs& other) = delete;
    RamFs& operator=(RamFs&& other) = delete;

    /* The smallest buffer a file gets; it doubles from there */
    static const ulong MinCapacity = 64;

    VNode* AllocNode(const char* name, VNode::Type type);
    bool Reserve(VNode* file, ulong size);
    void FreeNode(VNode* node);
    void FreeTree(VNode* node);

    VNode Root;
};

}
