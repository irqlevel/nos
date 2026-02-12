#include "vfs.h"

#include <lib/stdlib.h>

namespace Kernel
{

Vfs::Vfs()
    : MountCount(0)
{
    Stdlib::MemSet(Mounts, 0, sizeof(Mounts));
}

Vfs::~Vfs()
{
}

bool Vfs::Mount(const char* path, FileSystem* fs)
{
    if (path == nullptr || fs == nullptr)
        return false;

    if (path[0] != '/')
        return false;

    Stdlib::AutoLock lock(Lock);

    // Check for duplicate mount
    for (ulong i = 0; i < MountCount; i++)
    {
        if (Stdlib::StrCmp(Mounts[i].Path, path) == 0)
            return false;
    }

    if (MountCount >= MaxMounts)
        return false;

    Stdlib::StrnCpy(Mounts[MountCount].Path, path, MaxPath);
    Mounts[MountCount].Fs = fs;
    MountCount++;
    return true;
}

FileSystem* Vfs::Unmount(const char* path)
{
    if (path == nullptr)
        return nullptr;

    Stdlib::AutoLock lock(Lock);

    for (ulong i = 0; i < MountCount; i++)
    {
        if (Stdlib::StrCmp(Mounts[i].Path, path) == 0)
        {
            FileSystem* fs = Mounts[i].Fs;

            // Shift remaining entries
            for (ulong j = i; j + 1 < MountCount; j++)
            {
                Mounts[j] = Mounts[j + 1];
            }
            MountCount--;
            Stdlib::MemSet(&Mounts[MountCount], 0, sizeof(MountEntry));
            return fs;
        }
    }
    return nullptr;
}

bool Vfs::FindMount(const char* path, ulong& mountIdx, const char*& remainder)
{
    ulong bestLen = 0;
    ulong bestIdx = 0;
    bool found = false;

    for (ulong i = 0; i < MountCount; i++)
    {
        ulong mlen = Stdlib::StrLen(Mounts[i].Path);
        if (mlen == 0)
            continue;

        // Check if path starts with mount path
        if (Stdlib::StrnCmp(path, Mounts[i].Path, mlen) != 0)
            continue;

        // Must match exactly or be followed by '/'
        // Root mount "/" matches any absolute path
        if (path[mlen] != '\0' && path[mlen] != '/' &&
            !(mlen == 1 && Mounts[i].Path[0] == '/'))
            continue;

        if (mlen > bestLen)
        {
            bestLen = mlen;
            bestIdx = i;
            found = true;
        }
    }

    if (!found)
        return false;

    mountIdx = bestIdx;
    remainder = path + bestLen;
    if (*remainder == '/')
        remainder++;

    return true;
}

bool Vfs::ResolvePath(const char* path, FileSystem*& fs, VNode*& node,
                      VNode*& parent, char* lastName, ulong lastNameSize)
{
    fs = nullptr;
    node = nullptr;
    parent = nullptr;
    if (lastName)
        lastName[0] = '\0';

    ulong mountIdx;
    const char* remainder;

    if (!FindMount(path, mountIdx, remainder))
        return false;

    fs = Mounts[mountIdx].Fs;
    VNode* cur = fs->GetRoot();

    if (*remainder == '\0')
    {
        node = cur;
        return true;
    }

    // Walk path components
    const char* p = remainder;
    while (*p != '\0')
    {
        // Extract next component
        char component[64];
        ulong i = 0;
        while (*p != '\0' && *p != '/' && i < sizeof(component) - 1)
        {
            component[i++] = *p++;
        }
        component[i] = '\0';

        if (*p == '/')
            p++;

        if (i == 0)
            continue;

        // If there are more components, this must be a directory
        if (*p != '\0')
        {
            VNode* child = fs->Lookup(cur, component);
            if (child == nullptr || child->NodeType != VNode::TypeDir)
                return false;
            cur = child;
        }
        else
        {
            // Last component
            VNode* child = fs->Lookup(cur, component);
            parent = cur;
            if (lastName)
                Stdlib::StrnCpy(lastName, component, lastNameSize);
            if (child != nullptr)
            {
                node = child;
            }
            return true;
        }
    }

    node = cur;
    return true;
}

bool Vfs::ListDir(const char* path, Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0))
    {
        printer.Printf("path not found\n");
        return false;
    }

    if (node == nullptr)
    {
        printer.Printf("path not found\n");
        return false;
    }

    if (node->NodeType != VNode::TypeDir)
    {
        printer.Printf("not a directory\n");
        return false;
    }

    Stdlib::ListEntry* head = &node->Children;
    Stdlib::ListEntry* entry = head->Flink;
    while (entry != head)
    {
        VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
        const char* typeStr = (child->NodeType == VNode::TypeDir) ? "d" : "f";
        if (child->NodeType == VNode::TypeFile)
            printer.Printf("%s %u %s\n", typeStr, child->Size, child->Name);
        else
            printer.Printf("%s   %s\n", typeStr, child->Name);
        entry = entry->Flink;
    }

    return true;
}

bool Vfs::ReadFile(const char* path, Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0))
    {
        printer.Printf("path not found\n");
        return false;
    }

    if (node == nullptr)
    {
        printer.Printf("file not found\n");
        return false;
    }

    if (node->NodeType != VNode::TypeFile)
    {
        printer.Printf("not a file\n");
        return false;
    }

    if (node->Size == 0)
        return true;

    // Print content character by character to handle non-null-terminated data
    for (ulong i = 0; i < node->Size; i++)
    {
        char c = (char)node->Data[i];
        char buf[2] = { c, '\0' };
        printer.PrintString(buf);
    }
    printer.Printf("\n");
    return true;
}

bool Vfs::WriteFile(const char* path, const void* data, ulong len)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[64];

    if (!ResolvePath(path, fs, node, parent, lastName, sizeof(lastName)))
        return false;

    if (node == nullptr)
    {
        // Create file
        if (parent == nullptr || lastName[0] == '\0')
            return false;
        node = fs->CreateFile(parent, lastName);
        if (node == nullptr)
            return false;
    }

    if (node->NodeType != VNode::TypeFile)
        return false;

    return fs->Write(node, data, len);
}

bool Vfs::CreateDir(const char* path)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[64];

    if (!ResolvePath(path, fs, node, parent, lastName, sizeof(lastName)))
        return false;

    if (node != nullptr)
        return false; // already exists

    if (parent == nullptr || lastName[0] == '\0')
        return false;

    return (fs->CreateDir(parent, lastName) != nullptr);
}

bool Vfs::CreateFile(const char* path)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[64];

    if (!ResolvePath(path, fs, node, parent, lastName, sizeof(lastName)))
        return false;

    if (node != nullptr)
        return false; // already exists

    if (parent == nullptr || lastName[0] == '\0')
        return false;

    return (fs->CreateFile(parent, lastName) != nullptr);
}

bool Vfs::Remove(const char* path)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0))
        return false;

    if (node == nullptr)
        return false;

    return fs->Remove(node);
}

void Vfs::DumpMounts(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    for (ulong i = 0; i < MountCount; i++)
    {
        printer.Printf("%s on %s\n", Mounts[i].Fs->GetName(), Mounts[i].Path);
    }
}

}
