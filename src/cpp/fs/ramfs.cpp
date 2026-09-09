#include "ramfs.h"

#include <lib/stdlib.h>
#include <mm/new.h>
#include <kernel/trace.h>

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
    Root.Ino = 0;
    Root.Flags = VNode::FlagDirLoaded;
    Root.OpenCount = 0;
}

RamFs::~RamFs()
{
    Unmount();
}

void RamFs::Unmount()
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
    VNode* node = new (Mm::NoThrow) VNode();
    if (node == nullptr)
    {
        Trace(0, "RamFs::AllocNode: alloc failed for '%s'", name);
        return nullptr;
    }

    Stdlib::MemSet(node, 0, sizeof(VNode));
    Stdlib::StrnCpy(node->Name, name, sizeof(node->Name));
    node->NodeType = type;
    node->Parent = nullptr;
    node->Children.Init();
    node->SiblingLink.Init();
    node->Data = nullptr;
    node->Size = 0;
    node->Capacity = 0;
    node->Ino = 0;
    node->Flags = (type == VNode::TypeDir) ? VNode::FlagDirLoaded : 0;
    node->OpenCount = 0;
    return node;
}

/* Make room for size bytes, keeping the current content */
bool RamFs::Reserve(VNode* file, ulong size)
{
    if (size <= file->Capacity)
        return true;

    ulong newCap = (file->Capacity != 0) ? file->Capacity : MinCapacity;
    while (newCap < size)
    {
        if (newCap * 2 < newCap)
        {
            Trace(0, "RamFs::Reserve: size %u overflows", (ulong)size);
            return false;
        }
        newCap *= 2;
    }

    u8* newBuf = (u8*)Mm::Alloc(newCap, 0);
    if (newBuf == nullptr)
    {
        Trace(0, "RamFs::Reserve: alloc %u bytes failed", (ulong)newCap);
        return false;
    }

    if (file->Data != nullptr)
    {
        Stdlib::MemCpy(newBuf, file->Data, file->Size);
        Mm::Free(file->Data);
    }

    file->Data = newBuf;
    file->Capacity = newCap;
    return true;
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
    {
        Trace(0, "RamFs::CreateFile: null dir or name");
        return nullptr;
    }

    if (dir->NodeType != VNode::TypeDir)
    {
        Trace(0, "RamFs::CreateFile: parent is not a dir");
        return nullptr;
    }

    // Check if already exists
    if (Lookup(dir, name) != nullptr)
    {
        Trace(0, "RamFs::CreateFile: '%s' already exists", name);
        return nullptr;
    }

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
    {
        Trace(0, "RamFs::CreateDir: null dir or name");
        return nullptr;
    }

    if (dir->NodeType != VNode::TypeDir)
    {
        Trace(0, "RamFs::CreateDir: parent is not a dir");
        return nullptr;
    }

    // Check if already exists
    if (Lookup(dir, name) != nullptr)
    {
        Trace(0, "RamFs::CreateDir: '%s' already exists", name);
        return nullptr;
    }

    VNode* node = AllocNode(name, VNode::TypeDir);
    if (node == nullptr)
        return nullptr;

    node->Parent = dir;
    dir->Children.InsertTail(&node->SiblingLink);
    return node;
}

bool RamFs::Write(VNode* file, const void* data, ulong len, ulong offset)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "RamFs::Write: null file or not a file");
        return false;
    }

    if (len == 0)
        return true;

    ulong end = offset + len;
    if (end < offset)
    {
        Trace(0, "RamFs::Write: offset %u + len %u overflows", (ulong)offset, (ulong)len);
        return false;
    }

    if (!Reserve(file, end))
        return false;

    /* Writing past the end leaves a hole that reads as zeros */
    if (offset > file->Size)
        Stdlib::MemSet(file->Data + file->Size, 0, offset - file->Size);

    Stdlib::MemCpy(file->Data + offset, data, len);
    if (end > file->Size)
        file->Size = end;
    return true;
}

bool RamFs::Truncate(VNode* file, ulong size)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "RamFs::Truncate: null file or not a file");
        return false;
    }

    if (size > file->Size)
    {
        if (!Reserve(file, size))
            return false;
        Stdlib::MemSet(file->Data + file->Size, 0, size - file->Size);
    }

    file->Size = size;
    return true;
}

bool RamFs::Rename(VNode* node, VNode* newDir, const char* newName)
{
    if (node == nullptr || newDir == nullptr || newName == nullptr)
    {
        Trace(0, "RamFs::Rename: null node, dir or name");
        return false;
    }

    if (node->Parent == nullptr)
    {
        Trace(0, "RamFs::Rename: cannot rename root");
        return false;
    }

    if (newDir->NodeType != VNode::TypeDir)
    {
        Trace(0, "RamFs::Rename: target parent is not a dir");
        return false;
    }

    if (Stdlib::StrLen(newName) >= sizeof(node->Name))
    {
        Trace(0, "RamFs::Rename: name '%s' too long", newName);
        return false;
    }

    if (Lookup(newDir, newName) != nullptr)
    {
        Trace(0, "RamFs::Rename: '%s' already exists", newName);
        return false;
    }

    node->SiblingLink.RemoveInit();
    Stdlib::StrnCpy(node->Name, newName, sizeof(node->Name));
    node->Parent = newDir;
    newDir->Children.InsertTail(&node->SiblingLink);
    return true;
}

bool RamFs::Read(VNode* file, void* buf, ulong len, ulong offset)
{
    if (file == nullptr || file->NodeType != VNode::TypeFile)
    {
        Trace(0, "RamFs::Read: null file or not a file");
        return false;
    }

    if (offset >= file->Size)
    {
        Trace(0, "RamFs::Read: offset %u beyond size %u", (ulong)offset, (ulong)file->Size);
        return false;
    }

    ulong avail = file->Size - offset;
    ulong toRead = (len < avail) ? len : avail;

    Stdlib::MemCpy(buf, file->Data + offset, toRead);
    return true;
}

bool RamFs::Remove(VNode* node)
{
    if (node == nullptr)
    {
        Trace(0, "RamFs::Remove: null node");
        return false;
    }

    // Cannot remove root
    if (node->Parent == nullptr)
    {
        Trace(0, "RamFs::Remove: cannot remove root");
        return false;
    }

    // If directory, recursively free all children
    if (node->NodeType == VNode::TypeDir)
    {
        while (!node->Children.IsEmpty())
        {
            Stdlib::ListEntry* entry = node->Children.RemoveHead();
            VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
            FreeTree(child);
        }
    }

    FreeNode(node);
    return true;
}

}
