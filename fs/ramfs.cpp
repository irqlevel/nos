#include "ramfs.h"

#include <lib/stdlib.h>
#include <mm/new.h>

namespace Kernel
{

RamFs::RamFs()
{
    Stdlib::MemSet(&Root, 0, sizeof(Root));
    Stdlib::StrnCpy(Root.Name, "/", sizeof(Root.Name));
    Root.NodeType = VNode::TypeDir;
    Root.Parent = nullptr;
    Root.Children.Init();
    Root.SiblingLink.Init();
    Root.Data = nullptr;
    Root.Size = 0;
    Root.Capacity = 0;
}

RamFs::~RamFs()
{
    FreeTree(&Root);
}

const char* RamFs::GetName()
{
    return "ramfs";
}

VNode* RamFs::GetRoot()
{
    return &Root;
}

VNode* RamFs::AllocNode(const char* name, VNode::Type type)
{
    VNode* node = new VNode();
    if (node == nullptr)
        return nullptr;

    Stdlib::MemSet(node, 0, sizeof(VNode));
    Stdlib::StrnCpy(node->Name, name, sizeof(node->Name));
    node->NodeType = type;
    node->Parent = nullptr;
    node->Children.Init();
    node->SiblingLink.Init();
    node->Data = nullptr;
    node->Size = 0;
    node->Capacity = 0;
    return node;
}

void RamFs::FreeNode(VNode* node)
{
    if (node == nullptr)
        return;

    if (node->Data != nullptr)
    {
        Mm::Free(node->Data);
        node->Data = nullptr;
    }

    node->SiblingLink.RemoveInit();
    delete node;
}

void RamFs::FreeTree(VNode* node)
{
    if (node == nullptr)
        return;

    // Recursively free children
    while (!node->Children.IsEmpty())
    {
        Stdlib::ListEntry* entry = node->Children.RemoveHead();
        VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
        FreeTree(child);
    }

    // Don't delete the root (it's embedded, not heap-allocated)
    if (node != &Root)
    {
        if (node->Data != nullptr)
        {
            Mm::Free(node->Data);
            node->Data = nullptr;
        }
        delete node;
    }
}

VNode* RamFs::Lookup(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
        return nullptr;

    if (dir->NodeType != VNode::TypeDir)
        return nullptr;

    Stdlib::ListEntry* head = &dir->Children;
    Stdlib::ListEntry* entry = head->Flink;
    while (entry != head)
    {
        VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
        if (Stdlib::StrCmp(child->Name, name) == 0)
            return child;
        entry = entry->Flink;
    }
    return nullptr;
}

VNode* RamFs::CreateFile(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
        return nullptr;

    if (dir->NodeType != VNode::TypeDir)
        return nullptr;

    // Check if already exists
    if (Lookup(dir, name) != nullptr)
        return nullptr;

    VNode* node = AllocNode(name, VNode::TypeFile);
    if (node == nullptr)
        return nullptr;

    node->Parent = dir;
    dir->Children.InsertTail(&node->SiblingLink);
    return node;
}

VNode* RamFs::CreateDir(VNode* dir, const char* name)
{
    if (dir == nullptr || name == nullptr)
        return nullptr;

    if (dir->NodeType != VNode::TypeDir)
        return nullptr;

    // Check if already exists
    if (Lookup(dir, name) != nullptr)
        return nullptr;

    VNode* node = AllocNode(name, VNode::TypeDir);
    if (node == nullptr)
        return nullptr;

    node->Parent = dir;
    dir->Children.InsertTail(&node->SiblingLink);
    return node;
}

bool RamFs::Write(VNode* file, const void* data, ulong len)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
        return false;

    if (len == 0)
    {
        file->Size = 0;
        return true;
    }

    if (len > file->Capacity)
    {
        // Round up to next power-of-two-ish block
        ulong newCap = 64;
        while (newCap < len)
            newCap *= 2;

        u8* newBuf = (u8*)Mm::Alloc(newCap, 0);
        if (newBuf == nullptr)
            return false;

        if (file->Data != nullptr)
            Mm::Free(file->Data);

        file->Data = newBuf;
        file->Capacity = newCap;
    }

    Stdlib::MemCpy(file->Data, data, len);
    file->Size = len;
    return true;
}

bool RamFs::Read(VNode* file, void* buf, ulong len, ulong offset)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
        return false;

    if (offset >= file->Size)
        return false;

    ulong avail = file->Size - offset;
    ulong toRead = (len < avail) ? len : avail;

    Stdlib::MemCpy(buf, file->Data + offset, toRead);
    return true;
}

bool RamFs::Remove(VNode* node)
{
    if (node == nullptr)
        return false;

    // Cannot remove root
    if (node->Parent == nullptr)
        return false;

    // If directory, must be empty
    if (node->NodeType == VNode::TypeDir && !node->Children.IsEmpty())
        return false;

    FreeNode(node);
    return true;
}

}
