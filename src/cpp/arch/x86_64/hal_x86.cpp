#include <hal/console.h>
#include <hal/power.h>
#include <hal/cpu.h>

#include <arch/x86_64/asm.h>

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <drivers/serial.h>
#include <drivers/vga.h>
#include <drivers/acpi.h>

namespace Hal
{

void ConsoleWrite(const char *msg)
{
    Kernel::Serial::GetInstance().PrintString(msg);
    Kernel::Serial::GetInstance().Flush();

    if (Kernel::Parameters::GetInstance().IsTraceVga() && Kernel::VgaTerm::IsReady())
    {
        Kernel::VgaTerm::GetInstance().PrintString(msg);
    }
}

void ConsolePanicWrite(const char *msg)
{
    Kernel::Serial::GetInstance().PanicPrintString(msg);
    if (Kernel::VgaTerm::IsReady())
        Kernel::VgaTerm::GetInstance().PanicPrintString(msg);
}

void PowerOff()
{
    Trace(0, "ACPI shutdown");

    /* Try PM1a_CNT from FADT with SLP_TYP=5 (S5) | SLP_EN */
    ulong pm1a = Kernel::Acpi::GetInstance().GetPm1aCntPort();
    if (pm1a != 0)
    {
        Outw((u16)pm1a, (5 << 10) | (1 << 13));
        /* Brief busy-wait for the hardware to respond */
        for (volatile int i = 0; i < 1000000; i = i + 1) {}
    }

    /* QEMU/KVM fallback: PM1a_CNT port 0x604, SLP_TYP=0 for S5 */
    Outw(0x604, (1 << 13));

    /* QEMU debug exit device fallback */
    Outb(0xf4, 0x0);

    while (1) Hlt();
}

void Reset()
{
    Trace(0, "Reboot");

    /* Try ACPI FADT RESET_REG first */
    auto& acpi = Kernel::Acpi::GetInstance();
    if (acpi.HasResetReg())
    {
        Outb((u16)acpi.GetResetRegPort(), acpi.GetResetValue());
        for (volatile int i = 0; i < 1000000; i = i + 1) {}
    }

    /* Keyboard controller reset (pulse CPU reset line) */
    Outb(0x64, 0xFE);

    /* Fallback: PCI reset register */
    Outb(0xCF9, 0x06);

    while (1) Hlt();
}

}
