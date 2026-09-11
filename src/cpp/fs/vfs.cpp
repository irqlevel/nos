#include "vfs.h"

#include <block/block_device.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <kernel/trace.h>

namespace Kernel
{

/* ReadFile streams a file to a printer through a buffer of this size, so
   the file itself never has to fit in one allocation. */
static const ulong ReadFileChunk = 4096;

Vfs::Vfs()
    : MountCount(0)
{
    Stdlib::MemSet(Mounts, 0, sizeof(Mounts));
}

Vfs::~Vfs()
{
}

/* What a mount's claim on its device says to whoever is refused it */
static const char MountHolder[] = "a mounted filesystem";

bool Vfs::Mount(const char* path, FileSystem* fs, bool readOnly)
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

    if (Stdlib::StrLen(path) >= MaxPath)
    {
        Trace(0, "Vfs::Mount: path too long");
        return false;
    }

    Stdlib::AutoLock lock(Lock);

    // Check for duplicate mount path
    for (ulong i = 0; i < MountCount; i++)
    {
        if (Stdlib::StrCmp(Mounts[i].Path, path) == 0)
        {
            Trace(0, "Vfs::Mount: already mounted on %s", path);
            return false;
        }
    }

    // Check for duplicate block device
    BlockDevice* dev = fs->GetDevice();
    if (dev != nullptr)
    {
        for (ulong i = 0; i < MountCount; i++)
        {
            if (Mounts[i].Fs->GetDevice() == dev)
            {
                Trace(0, "Vfs::Mount: device %s already mounted on %s",
                      dev->GetName(), Mounts[i].Path);
                return false;
            }
        }
    }

    if (MountCount >= MaxMounts)
    {
        Trace(0, "Vfs::Mount: max mounts reached");
        return false;
    }

    /* The device is the filesystem's while it is mounted: nothing may write
       to it around the filesystem -- the disk log, a module going direct --
       nor another mount take it, or a disk or partition overlapping it */
    ulong claim = 0;
    if (dev != nullptr)
    {
        const char* heldBy = nullptr;
        claim = BlockDeviceTable::GetInstance().Claim(dev, MountHolder, heldBy);
        if (claim == 0)
        {
            Trace(0, "Vfs::Mount: %s is in use by %s", dev->GetName(), heldBy);
            return false;
        }
    }

    fs->ReadOnly = readOnly;
    if (!fs->Mount())
    {
        Trace(0, "Vfs::Mount: fs->Mount() failed for %s", path);
        BlockDeviceTable::GetInstance().Release(claim);
        return false;
    }

    /* The filesystem may have found an image it can only read */
    if (fs->ReadOnly && !readOnly)
    {
        Trace(0, "Vfs::Mount: %s mounted read-only on %s", fs->GetName(), path);
        readOnly = true;
    }

    Stdlib::StrnCpy(Mounts[MountCount].Path, path, MaxPath);
    Mounts[MountCount].Fs = fs;
    Mounts[MountCount].ReadOnly = readOnly;
    Mounts[MountCount].Claim = claim;
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

            if (fs->OpenFiles != 0)
            {
                Trace(0, "Vfs::Unmount: %s is busy (%u open files)", path, fs->OpenFiles);
                return nullptr;
            }

            fs->Unmount();
            BlockDeviceTable::GetInstance().Release(Mounts[i].Claim);

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

bool Vfs::IsMountReadOnly(const char* path)
{
    ulong mountIdx;
    const char* remainder;
    if (!FindMount(path, mountIdx, remainder))
        return false;
    return Mounts[mountIdx].ReadOnly;
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
        char component[MaxName];
        ulong i = 0;
        while (*p != '\0' && *p != '/' && i < sizeof(component) - 1)
        {
            component[i++] = *p++;
        }
        component[i] = '\0';

        // A component that doesn't fit is an error, not two components
        if (*p != '\0' && *p != '/')
        {
            Trace(0, "Vfs::ResolvePath: component too long in %s", path);
            return false;
        }

        if (*p == '/')
            p++;

        if (i == 0)
            continue;

        // "." and ".." are not stored as children; resolve them here
        if (component[0] == '.' && component[1] == '\0')
        {
            if (*p == '\0')
            {
                node = cur;
                return true;
            }
            continue;
        }
        if (component[0] == '.' && component[1] == '.' && component[2] == '\0')
        {
            if (cur->Parent != nullptr)
                cur = cur->Parent; // the mount root stays put
            if (*p == '\0')
            {
                node = cur;
                return true;
            }
            continue;
        }

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

bool Vfs::HasOpenFiles(VNode* node)
{
    if (node->OpenCount != 0)
        return true;

    Stdlib::ListEntry* head = &node->Children;
    for (Stdlib::ListEntry* entry = head->Flink; entry != head; entry = entry->Flink)
    {
        VNode* child = CONTAINING_RECORD(entry, VNode, SiblingLink);
        if (HasOpenFiles(child))
            return true;
    }
    return false;
}

/* True when node is other itself or one of its ancestors */
bool Vfs::IsAncestor(VNode* node, VNode* other)
{
    for (VNode* cur = other; cur != nullptr; cur = cur->Parent)
    {
        if (cur == node)
            return true;
    }
    return false;
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

    if (!fs->LoadDir(node))
    {
        printer.Printf("read failed\n");
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

bool Vfs::ReadDir(const char* path, ulong index, DirEntry& entry)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0) || node == nullptr)
        return false;

    if (node->NodeType != VNode::TypeDir)
        return false;

    if (!fs->LoadDir(node))
        return false;

    ulong i = 0;
    Stdlib::ListEntry* head = &node->Children;
    for (Stdlib::ListEntry* e = head->Flink; e != head; e = e->Flink, i++)
    {
        if (i != index)
            continue;

        VNode* child = CONTAINING_RECORD(e, VNode, SiblingLink);
        Stdlib::StrnCpy(entry.Name, child->Name, sizeof(entry.Name));
        entry.Type = child->NodeType;
        entry.Size = (child->NodeType == VNode::TypeFile) ? child->Size : 0;
        return true;
    }

    return false;
}

bool Vfs::Stat(const char* path, FileStat& st)
{
    Stdlib::AutoLock lock(Lock);

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0) || node == nullptr)
        return false;

    st.Type = node->NodeType;
    st.Size = (node->NodeType == VNode::TypeFile) ? node->Size : 0;
    st.Ino = node->Ino;
    return true;
}

File* Vfs::Open(const char* path, ulong flags)
{
    if (path == nullptr)
        return nullptr;

    if (flags & OpenAppend)
        flags |= OpenWrite;
    if ((flags & (OpenRead | OpenWrite)) == 0)
    {
        Trace(0, "Vfs::Open: %s: neither read nor write", path);
        return nullptr;
    }

    Stdlib::AutoLock lock(Lock);

    bool writes = (flags & (OpenWrite | OpenCreate | OpenTruncate)) != 0;
    if (writes && IsMountReadOnly(path))
    {
        Trace(0, "Vfs::Open: %s is on a readonly mount", path);
        return nullptr;
    }

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[MaxName];

    if (!ResolvePath(path, fs, node, parent, lastName, sizeof(lastName)))
    {
        Trace(0, "Vfs::Open: resolve failed for %s", path);
        return nullptr;
    }

    if (node == nullptr)
    {
        if ((flags & OpenCreate) == 0)
        {
            Trace(0, "Vfs::Open: %s not found", path);
            return nullptr;
        }

        if (parent == nullptr || lastName[0] == '\0')
        {
            Trace(0, "Vfs::Open: no parent dir for %s", path);
            return nullptr;
        }

        node = fs->CreateFile(parent, lastName);
        if (node == nullptr)
        {
            Trace(0, "Vfs::Open: create failed for %s", path);
            return nullptr;
        }
    }

    if (node->NodeType != VNode::TypeFile)
    {
        Trace(0, "Vfs::Open: %s is not a file", path);
        return nullptr;
    }

    if ((flags & OpenTruncate) && node->Size != 0)
    {
        if (!fs->Truncate(node, 0))
        {
            Trace(0, "Vfs::Open: truncate failed for %s", path);
            return nullptr;
        }
    }

    File* file = new (Mm::NoThrow) File();
    if (file == nullptr)
    {
        Trace(0, "Vfs::Open: alloc failed for %s", path);
        return nullptr;
    }

    file->Fs = fs;
    file->Node = node;
    file->Pos = (flags & OpenAppend) ? node->Size : 0;
    file->Flags = flags;

    node->OpenCount++;
    fs->OpenFiles++;
    return file;
}

void Vfs::Close(File* file)
{
    if (file == nullptr)
        return;

    Stdlib::AutoLock lock(Lock);

    file->Node->OpenCount--;
    file->Fs->OpenFiles--;
    delete file;
}

bool Vfs::Read(File* file, void* buf, ulong len, ulong& bytesRead)
{
    bytesRead = 0;
    if (file == nullptr || buf == nullptr)
        return false;

    if ((file->Flags & OpenRead) == 0)
    {
        Trace(0, "Vfs::Read: not open for reading");
        return false;
    }

    Stdlib::AutoLock lock(Lock);

    VNode* node = file->Node;
    if (file->Pos >= node->Size || len == 0)
        return true;

    ulong avail = node->Size - file->Pos;
    ulong toRead = (len < avail) ? len : avail;

    if (!file->Fs->Read(node, buf, toRead, file->Pos))
        return false;

    file->Pos += toRead;
    bytesRead = toRead;
    return true;
}

bool Vfs::Write(File* file, const void* data, ulong len)
{
    if (file == nullptr || (data == nullptr && len != 0))
        return false;

    if ((file->Flags & OpenWrite) == 0)
    {
        Trace(0, "Vfs::Write: not open for writing");
        return false;
    }

    if (len == 0)
        return true;

    Stdlib::AutoLock lock(Lock);

    VNode* node = file->Node;
    if (file->Flags & OpenAppend)
        file->Pos = node->Size;

    if (file->Pos + len < file->Pos)
    {
        Trace(0, "Vfs::Write: offset overflow");
        return false;
    }

    if (!file->Fs->Write(node, data, len, file->Pos))
        return false;

    file->Pos += len;
    return true;
}

bool Vfs::Seek(File* file, ulong pos)
{
    if (file == nullptr)
        return false;

    Stdlib::AutoLock lock(Lock);
    file->Pos = pos;
    return true;
}

ulong Vfs::Tell(File* file)
{
    if (file == nullptr)
        return 0;

    Stdlib::AutoLock lock(Lock);
    return file->Pos;
}

ulong Vfs::GetSize(File* file)
{
    if (file == nullptr)
        return 0;

    Stdlib::AutoLock lock(Lock);
    return file->Node->Size;
}

bool Vfs::ReadFile(const char* path, Stdlib::Printer& printer)
{
    File* file = Open(path, OpenRead);
    if (file == nullptr)
    {
        printer.Printf("file not found\n");
        return false;
    }

    u8* buf = (u8*)Mm::Alloc(ReadFileChunk, 0);
    if (buf == nullptr)
    {
        Trace(0, "Vfs::ReadFile: alloc %u bytes failed", ReadFileChunk);
        printer.Printf("read failed\n");
        Close(file);
        return false;
    }

    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (!Read(file, buf, ReadFileChunk, got))
        {
            printer.Printf("read failed\n");
            ok = false;
            break;
        }
        if (got == 0)
            break;

        // Print character by character to handle non-null-terminated data
        for (ulong i = 0; i < got; i++)
        {
            char tmp[2] = { (char)buf[i], '\0' };
            printer.PrintString(tmp);
        }
    }

    Mm::Free(buf);
    Close(file);
    if (ok)
        printer.Printf("\n");
    return ok;
}

bool Vfs::WriteFile(const char* path, const void* data, ulong len)
{
    Stdlib::AutoLock lock(Lock);

    if (IsMountReadOnly(path))
    {
        Trace(0, "Vfs::WriteFile: %s is on a readonly mount", path);
        return false;
    }

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[MaxName];

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

    if (node->Size != 0 && !fs->Truncate(node, 0))
        return false;

    if (len == 0)
        return true;

    return fs->Write(node, data, len, 0);
}

bool Vfs::Truncate(const char* path, ulong size)
{
    Stdlib::AutoLock lock(Lock);

    if (IsMountReadOnly(path))
    {
        Trace(0, "Vfs::Truncate: %s is on a readonly mount", path);
        return false;
    }

    FileSystem* fs;
    VNode* node;
    VNode* parent;

    if (!ResolvePath(path, fs, node, parent, nullptr, 0) || node == nullptr)
    {
        Trace(0, "Vfs::Truncate: %s not found", path);
        return false;
    }

    if (node->NodeType != VNode::TypeFile)
    {
        Trace(0, "Vfs::Truncate: %s is not a file", path);
        return false;
    }

    return fs->Truncate(node, size);
}

bool Vfs::CreateDir(const char* path)
{
    Stdlib::AutoLock lock(Lock);

    if (IsMountReadOnly(path))
    {
        Trace(0, "Vfs::CreateDir: %s is on a readonly mount", path);
        return false;
    }

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[MaxName];

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

    if (IsMountReadOnly(path))
    {
        Trace(0, "Vfs::CreateFile: %s is on a readonly mount", path);
        return false;
    }

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    char lastName[MaxName];

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

    if (IsMountReadOnly(path))
    {
        Trace(0, "Vfs::Remove: %s is on a readonly mount", path);
        return false;
    }

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

    if (node->NodeType == VNode::TypeDir && !fs->LoadDir(node))
        return false;

    if (HasOpenFiles(node))
    {
        Trace(0, "Vfs::Remove: %s is open", path);
        return false;
    }

    return fs->Remove(node);
}

bool Vfs::Rename(const char* oldPath, const char* newPath)
{
    if (oldPath == nullptr || newPath == nullptr)
        return false;

    Stdlib::AutoLock lock(Lock);

    ulong oldMount, newMount;
    const char* rem;
    if (!FindMount(oldPath, oldMount, rem) || !FindMount(newPath, newMount, rem))
        return false;

    if (oldMount != newMount)
    {
        Trace(0, "Vfs::Rename: %s and %s are on different mounts", oldPath, newPath);
        return false;
    }

    if (Mounts[oldMount].ReadOnly)
    {
        Trace(0, "Vfs::Rename: %s is on a readonly mount", oldPath);
        return false;
    }

    FileSystem* fs;
    VNode* node;
    VNode* parent;
    if (!ResolvePath(oldPath, fs, node, parent, nullptr, 0) || node == nullptr)
    {
        Trace(0, "Vfs::Rename: %s not found", oldPath);
        return false;
    }

    if (node->Parent == nullptr)
    {
        Trace(0, "Vfs::Rename: cannot rename the root");
        return false;
    }

    if (node->NodeType == VNode::TypeDir && !fs->LoadDir(node))
        return false;

    if (HasOpenFiles(node))
    {
        Trace(0, "Vfs::Rename: %s is open", oldPath);
        return false;
    }

    FileSystem* newFs;
    VNode* target;
    VNode* newParent;
    char newName[MaxName];
    if (!ResolvePath(newPath, newFs, target, newParent, newName, sizeof(newName)))
    {
        Trace(0, "Vfs::Rename: resolve failed for %s", newPath);
        return false;
    }

    if (target != nullptr)
    {
        Trace(0, "Vfs::Rename: %s already exists", newPath);
        return false;
    }

    if (newParent == nullptr || newName[0] == '\0')
    {
        Trace(0, "Vfs::Rename: no parent dir for %s", newPath);
        return false;
    }

    /* A directory cannot move under itself */
    if (IsAncestor(node, newParent))
    {
        Trace(0, "Vfs::Rename: %s is inside %s", newPath, oldPath);
        return false;
    }

    return fs->Rename(node, newParent, newName);
}

bool Vfs::Sync()
{
    Stdlib::AutoLock lock(Lock);

    bool ok = true;
    for (ulong i = 0; i < MountCount; i++)
    {
        if (!Mounts[i].Fs->Sync())
            ok = false;
    }
    return ok;
}

void Vfs::DumpMounts(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    for (ulong i = 0; i < MountCount; i++)
    {
        const char* rwStr = Mounts[i].ReadOnly ? "ro" : "rw";
        char info[64];
        Mounts[i].Fs->GetInfo(info, sizeof(info));
        if (info[0] != '\0')
            printer.Printf("%s on %s  %s  %s\n", Mounts[i].Fs->GetName(), Mounts[i].Path, info, rwStr);
        else
            printer.Printf("%s on %s  %s\n", Mounts[i].Fs->GetName(), Mounts[i].Path, rwStr);
    }
}

void Vfs::UnmountAll()
{
    Stdlib::AutoLock lock(Lock);

    /* Unmount in reverse path-length order (deepest first)
       so child mounts are torn down before parents. */
    while (MountCount > 0)
    {
        ulong longest = 0;
        ulong longestIdx = 0;
        for (ulong i = 0; i < MountCount; i++)
        {
            ulong len = Stdlib::StrLen(Mounts[i].Path);
            if (len >= longest)
            {
                longest = len;
                longestIdx = i;
            }
        }

        FileSystem* fs = Mounts[longestIdx].Fs;
        Trace(0, "Vfs::UnmountAll: unmounting %s (%s)",
              Mounts[longestIdx].Path, fs->GetName());

        /* This is shutdown: a handle left open is abandoned, not honoured */
        if (fs->OpenFiles != 0)
            Trace(0, "Vfs::UnmountAll: %s has %u open files", Mounts[longestIdx].Path, fs->OpenFiles);

        fs->Unmount();
        BlockDeviceTable::GetInstance().Release(Mounts[longestIdx].Claim);
        delete fs;

        for (ulong j = longestIdx; j + 1 < MountCount; j++)
            Mounts[j] = Mounts[j + 1];
        MountCount--;
        Stdlib::MemSet(&Mounts[MountCount], 0, sizeof(MountEntry));
    }
}

}
