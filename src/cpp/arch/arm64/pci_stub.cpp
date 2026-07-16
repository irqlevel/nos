#include <drivers/pci.h>

/* No PCI on arm64 yet (QEMU virt exposes ECAM PCIe; the driver lands
   with the NVMe/MSI-ITS work). This stub keeps the shell's lspci
   command linkable. */

Pci::Pci()
{
}

Pci::~Pci()
{
}

void Pci::Dump(Stdlib::Printer& printer)
{
    printer.Printf("no pci support on this platform yet\n");
}
