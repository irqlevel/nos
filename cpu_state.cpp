#include "cpu_state.h"
#include "helpers32.h"

namespace Kernel
{

namespace Core
{

CpuState::CpuState()
	: Cr0(0)
	, Cr1(0)
	, Cr2(0)
	, Cr3(0)
	, Cr4(0)
	, Esp(0)
	, Eflags(0)
	, Cs(0)
	, Ds(0)
	, Es(0)
	, Ss(0)
	, Fs(0)
	, Gs(0)
	, LongMode(false)
	, Cpuid(false)
{
}

CpuState::~CpuState()
{
}

void CpuState::Load()
{
	Cr0 = get_cr0_32();
	Cr1 = 0; // Reserved
	Cr2 = get_cr2_32();
	Cr3 = get_cr3_32();
	Cr4 = get_cr4_32();

	Esp = get_esp_32();
	Eflags = get_eflags_32();
	Cs = get_cs_32();
	Ds = get_ds_32();
	Es = get_es_32();
	Ss = get_ss_32();
	Gs = get_gs_32();
	LongMode = check_long_mode_support_32();
	Cpuid = check_cpuid_support_32();

}

ulong CpuState::GetCr0()
{
	return Cr0;
}

ulong CpuState::GetCr1()
{
	return Cr1;
}

ulong CpuState::GetCr2()
{
	return Cr2;
}

ulong CpuState::GetCr3()
{
	return Cr3;
}

ulong CpuState::GetCr4()
{
	return Cr4;
}

ulong CpuState::GetEflags()
{
	return Eflags;
}

ulong CpuState::GetEsp()
{
	return Esp;
}

u16 CpuState::GetCs()
{
	return Cs;
}

u16 CpuState::GetDs()
{
	return Ds;
}

u16 CpuState::GetEs()
{
	return Es;
}

u16 CpuState::GetSs()
{
	return Ss;
}

u16 CpuState::GetFs()
{
	return Fs;
}

u16 CpuState::GetGs()
{
	return Gs;
}

bool CpuState::GetCpudid()
{
	return Cpuid;
}

bool CpuState::GetLongMode()
{
	return LongMode;
}

}
}