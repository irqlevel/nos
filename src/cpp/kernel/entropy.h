#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

class EntropySource
{
public:
    virtual ~EntropySource() {}
    virtual const char* GetName() = 0;
    virtual bool GetRandom(u8* buf, ulong len) = 0;
};

class EntropySourceTable
{
public:
    static EntropySourceTable& GetInstance()
    {
        static EntropySourceTable instance;
        return instance;
    }

    bool Register(EntropySource* src);
    EntropySource* Find(const char* name);
    EntropySource* GetDefault();
    ulong GetCount();
    void Dump(Stdlib::Printer& printer);

    static const ulong MaxSources = 4;

private:
    EntropySourceTable();
    ~EntropySourceTable();
    EntropySourceTable(const EntropySourceTable& other) = delete;
    EntropySourceTable(EntropySourceTable&& other) = delete;
    EntropySourceTable& operator=(const EntropySourceTable& other) = delete;
    EntropySourceTable& operator=(EntropySourceTable&& other) = delete;

    EntropySource* Sources[MaxSources];
    ulong Count;
};

}
