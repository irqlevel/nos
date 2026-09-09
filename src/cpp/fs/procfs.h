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
    virtual VNode* Lookup(VNode* dir, const char* name) override;

private:
    ProcFs(const ProcFs& other) = delete;
    ProcFs(ProcFs&& other) = delete;
    ProcFs& operator=(const ProcFs& other) = delete;
    ProcFs& operator=(ProcFs&& other) = delete;

    void RefreshInterrupts();

    VNode* InterruptsNode;
};

}
