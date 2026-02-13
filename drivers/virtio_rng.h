#pragma once

#include <include/types.h>
#include <kernel/entropy.h>
#include <kernel/spin_lock.h>
#include <kernel/asm.h>
#include <drivers/virtqueue.h>
#include <drivers/pci.h>
#include <drivers/virtio_pci.h>

namespace Kernel
{

class VirtioRng : public EntropySource
{
public:
    VirtioRng();
    virtual ~VirtioRng();

    bool Init(Pci::DeviceInfo* pciDev, const char* name);

    /* EntropySource interface */
    virtual const char* GetName() override;
    virtual bool GetRandom(u8* buf, ulong len) override;

    /* Discover and initialize all virtio-rng devices. */
    static void InitAll();

private:
    VirtioRng(const VirtioRng& other) = delete;
    VirtioRng(VirtioRng&& other) = delete;
    VirtioRng& operator=(const VirtioRng& other) = delete;
    VirtioRng& operator=(VirtioRng&& other) = delete;

    VirtioPci Transport;
    VirtQueue Queue;
    SpinLock Lock;
    u8* DmaBuf;
    ulong DmaBufPhys;
    bool Initialized;
    char DevName[8];

    static const ulong MaxInstances = 4;
    static VirtioRng Instances[MaxInstances];
    static ulong InstanceCount;
};

}
