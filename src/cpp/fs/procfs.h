#pragma once

#include <fs/ramfs.h>

namespace Kernel
{

class ProcFs : public RamFs
{
public:
    ProcFs();
    virtual ~ProcFs();

    virtual const char* GetName() override;
    virtual bool Mount() override;
    virtual bool Read(VNode* file, void* buf, ulong len, ulong offset) override;

private:
    ProcFs(const ProcFs& other) = delete;
    ProcFs(ProcFs&& other) = delete;
    ProcFs& operator=(const ProcFs& other) = delete;
    ProcFs& operator=(ProcFs&& other) = delete;

    void RefreshInterrupts();

    VNode* InterruptsNode;
};

}
