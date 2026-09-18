#include "rootfs.h"

#include <fs/vfs.h>
#include <fs/ramfs.h>
#include <fs/ext2.h>
#include <fs/nanofs.h>
#include <fs/procfs.h>
#include <fs/fstest.h>
#include <block/block_device.h>
#include <kernel/parameters.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/new.h>

namespace Kernel
{

/* root=auto takes the ext2 filesystem carrying this label, which is what
   scripts/mkrootfs.sh and scripts/build-disk.sh put on theirs */
static const char* RootLabelAuto = "nos";

/* Big enough to cross the direct and the single-indirect block range at
   any block size, and the double-indirect one at 1 KiB blocks, which is
   what the smoke-test image uses; small enough not to notice at boot */
static const ulong FsTestBootSize = 300 * 1024;

/* The block device the root spec names, or nullptr */
static BlockDevice* FindRootDevice(const Parameters::RootSpec& spec)
{
    auto& table = BlockDeviceTable::GetInstance();

    if (spec.Mode == Parameters::RootDevice)
        return table.Find(spec.Value);

    for (ulong i = 0; i < table.GetCount(); i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        Ext2Identity id;
        if (dev == nullptr || !Ext2Probe(dev, id))
            continue;

        switch (spec.Mode)
        {
        case Parameters::RootAuto:
            if (Stdlib::StrCmp(id.Label, RootLabelAuto) == 0)
                return dev;
            break;
        case Parameters::RootLabel:
            if (Stdlib::StrCmp(id.Label, spec.Value) == 0)
                return dev;
            break;
        case Parameters::RootUuid:
            if (Stdlib::MemCmp(id.Uuid, spec.Uuid, sizeof(id.Uuid)) == 0)
                return dev;
            break;
        default:
            break;
        }
    }

    return nullptr;
}

/* ext2 if the device carries one, else nanofs; false when neither mounts */
static bool MountRootOn(BlockDevice* dev, bool readOnly)
{
    auto& vfs = Vfs::GetInstance();
    Ext2Identity id;

    if (Ext2Probe(dev, id))
    {
        int mounted = Ext2Mount("/", dev, readOnly);
        if (mounted != Ext2NotMounted)
        {
            Trace(0, "MountRootFs: mounted ext2 on / from %s (%s)", dev->GetName(),
                  mounted == Ext2MountedRo ? "ro" : "rw");
            return true;
        }
        Trace(0, "MountRootFs: mounting ext2 from %s failed", dev->GetName());
        return false;
    }

    NanoFs* fs = new (Mm::NoThrow) NanoFs(dev);
    if (fs != nullptr && vfs.Mount("/", fs, readOnly))
    {
        Trace(0, "MountRootFs: mounted nanofs on / from %s (%s)", dev->GetName(),
              readOnly ? "ro" : "rw");
        return true;
    }
    delete fs;
    Trace(0, "MountRootFs: %s carries no filesystem this kernel mounts", dev->GetName());
    return false;
}

/* No root on disk: a ramfs on /, the first ext2 found read-only on /boot,
   the first nanofs found read-write on /data */
static void MountFallbackLayout()
{
    auto& vfs = Vfs::GetInstance();

    if (!RamFsMount("/"))
    {
        Trace(0, "MountRootFs: failed to mount ramfs on /");
        return;
    }
    Trace(0, "MountRootFs: mounted ramfs on / (rw)");

    vfs.CreateDir("/boot");

    auto& table = BlockDeviceTable::GetInstance();
    for (ulong i = 0; i < table.GetCount(); i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        Ext2Identity id;
        if (!Ext2Probe(dev, id))
            continue;
        if (Ext2Mount("/boot", dev, true) != Ext2NotMounted)
        {
            Trace(0, "MountRootFs: mounted ext2 on /boot from %s (ro)",
                dev->GetName());
            break;
        }
    }

    vfs.CreateDir("/data");

    for (ulong i = 0; i < table.GetCount(); i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        Ext2Identity id;
        if (Ext2Probe(dev, id))
            continue;
        NanoFs* nanofs = new (Mm::NoThrow) NanoFs(dev);
        if (nanofs != nullptr && vfs.Mount("/data", nanofs))
        {
            Trace(0, "MountRootFs: mounted nanofs on /data from %s (rw)",
                dev->GetName());
            break;
        }
        delete nanofs;
    }
}

static void MountProcFs()
{
    auto& vfs = Vfs::GetInstance();
    FileStat st;

    if (!vfs.Stat("/proc", st) && !vfs.CreateDir("/proc"))
    {
        Trace(0, "MountRootFs: no /proc directory and cannot make one");
        return;
    }

    if (ProcFsMount("/proc"))
        Trace(0, "MountRootFs: mounted procfs on /proc (ro)");
}

void MountRootFs()
{
    auto& params = Parameters::GetInstance();
    const Parameters::RootSpec& spec = params.GetRoot();
    if (spec.Mode == Parameters::RootNone)
        return;

    bool mounted = false;
    BlockDevice* dev = FindRootDevice(spec);
    if (dev != nullptr)
        mounted = MountRootOn(dev, params.IsRootReadOnly());
    else if (spec.Mode != Parameters::RootAuto)
        Trace(0, "MountRootFs: root %s not found", spec.Value);

    if (!mounted)
        MountFallbackLayout();

    MountProcFs();

    if (params.IsFsTest())
    {
        if (FsSelfTest("/", FsTestBootSize, nullptr))
            Trace(0, "fstest: passed");
        else
            Trace(0, "fstest: FAILED");
    }
}

}
