#pragma once

#include "types.h"
#include "gdt.h"

namespace Kernel
{

namespace Core
{

class CpuState
{
public:
	CpuState();
	virtual ~CpuState();
	void Load();

	ulong GetCr0();
	ulong GetCr1();
	ulong GetCr2();
	ulong GetCr3();
	ulong GetCr4();
	ulong GetEsp();
	ulong GetEflags();

	u16 GetCs();
	u16 GetDs();
	u16 GetEs();
	u16 GetSs();
	u16 GetFs();
	u16 GetGs();

	bool GetCpudid();
	bool GetLongMode();

private:
	CpuState(const CpuState& other) = delete;
	CpuState(CpuState&& other) = delete;
	CpuState& operator=(const CpuState& other) = delete;
	CpuState& operator=(CpuState&& other) = delete;

	ulong Cr0;
	ulong Cr1;
	ulong Cr2;
	ulong Cr3;
	ulong Cr4;
	ulong Esp;
	ulong Eflags;

	u16 Cs;
	u16 Ds;
	u16 Es;
	u16 Ss;
	u16 Fs;
	u16 Gs;
	bool LongMode;
	bool Cpuid;

};

}
}
