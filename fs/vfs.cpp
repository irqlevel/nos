#include "vfs.h"

#include <lib/stdlib.h>
#include <mm/new.h>
#include <kernel/trace.h>

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
    {
        Trace(0, "Vfs::Mount: null path or fs");
        return false;
    }

    if (path[0] != '/')
    {
        Trace(0, "Vfs::Mount: path must start with /");
        return false;
    }

    Stdlib::AutoLock lock(Lock);

    // Check for duplicate mount
    for (ulong i = 0; i < MountCount; i++)
    {
        if (Stdlib::StrCmp(Mounts[i].Path, path) == 0)
        {
            Trace(0, "Vfs::Mount: already mounted on %s", path);
            return false;
        }
    }

    if (MountCount >= MaxMounts)
    {
        Trace(0, "Vfs::Mount: max mounts reached");
        return false;
    }

    Stdlib::StrnCpy(Mounts[MountCount].Path, path, MaxPath);
    Mounts[MountCount].Fs = fs;
    MountCount++;
    return true;
}

FileSystem* Vfs::Unmount(const char* path)
{
    if (path == nullptr)
    {
        Trace(0, "Vfs::Unmount: null path");
        return nullptr;
    }

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
    Trace(0, "Vfs::Unmount: %s not found", path);
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
    {
        Trace(0, "Vfs::FindMount: no mount for %s", path);
        return false;
    }

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
    {
        Trace(0, "Vfs::ResolvePath: no mount for %s", path);
        return false;
    }

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
            {
                Trace(0, "Vfs::ResolvePath: component '%s' not found or not dir", component);
                return false;
            }
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

    // Read content through fs->Read() so on-disk filesystems work
    ulong size = node->Size;
    u8* buf = (u8*)Mm::Alloc(size + 1, 0);
    if (buf == nullptr)
    {
        Trace(0, "Vfs::ReadFile: alloc %u bytes failed", (ulong)size);
        printer.Printf("read failed\n");
        return false;
    }

    if (!fs->Read(node, buf, size, 0))
    {
        Mm::Free(buf);
        printer.Printf("read failed\n");
        return false;
    }

    // Print content character by character to handle non-null-terminated data
    for (ulong i = 0; i < size; i++)
    {
        char c = (char)buf[i];
        char tmp[2] = { c, '\0' };
        printer.PrintString(tmp);
    }
    Mm::Free(buf);
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
    {
        Trace(0, "Vfs::WriteFile: resolve failed for %s", path);
        return false;
    }

    if (node == nullptr)
    {
        // Create file
        if (parent == nullptr || lastName[0] == '\0')
        {
            Trace(0, "Vfs::WriteFile: no parent dir for %s", path);
            return false;
        }
        node = fs->CreateFile(parent, lastName);
        if (node == nullptr)
        {
            Trace(0, "Vfs::WriteFile: create failed for %s", path);
            return false;
        }
    }

    if (node->NodeType != VNode::TypeFile)
    {
        Trace(0, "Vfs::WriteFile: %s is not a file", path);
        return false;
    }

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
    {
        Trace(0, "Vfs::CreateDir: resolve failed for %s", path);
        return false;
    }

    if (node != nullptr)
    {
        Trace(0, "Vfs::CreateDir: %s already exists", path);
        return false; // already exists
    }

    if (parent == nullptr || lastName[0] == '\0')
    {
        Trace(0, "Vfs::CreateDir: no parent dir for %s", path);
        return false;
    }

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
    {
        Trace(0, "Vfs::CreateFile: resolve failed for %s", path);
        return false;
    }

    if (node != nullptr)
    {
        Trace(0, "Vfs::CreateFile: %s already exists", path);
        return false; // already exists
    }

    if (parent == nullptr || lastName[0] == '\0')
    {
        Trace(0, "Vfs::CreateFile: no parent dir for %s", path);
        return false;
    }

    return (fs->CreateFile(parent, lastName) != nullptr);
}

bool Vfs::Remove(const char* path)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0))
    {
        Trace(0, "Vfs::Remove: resolve failed for %s", path);
        return false;
    }

    if (node == nullptr)
    {
        Trace(0, "Vfs::Remove: %s not found", path);
        return false;
    }

    return fs->Remove(node);
}

void Vfs::DumpMounts(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    for (ulong i = 0; i < MountCount; i++)
    {
        char info[64];
        Mounts[i].Fs->GetInfo(info, sizeof(info));
        if (info[0] != '\0')
            printer.Printf("%s on %s  %s\n", Mounts[i].Fs->GetName(), Mounts[i].Path, info);
        else
            printer.Printf("%s on %s\n", Mounts[i].Fs->GetName(), Mounts[i].Path);
    }
}

}
