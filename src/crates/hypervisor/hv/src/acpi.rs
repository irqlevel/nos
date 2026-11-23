//! The ACPI tables a PC's firmware would have left a guest: what an OS with
//! ACPI finds the machine by -- its CPUs, where its fixed hardware is and how
//! to turn it off, what is on its PCI bus and which interrupt each device
//! has. (ACPI 6.5, chapter 5; the tables are those of ACPI 6.3.)
//!
//! They go in the BIOS area below 1 MiB, the RSDP first, where a kernel that
//! scans for it finds it -- and the zero page points at it too, for one that
//! looks there: an XSDT naming the FADT and, for a guest whose CPUs have
//! local APICs, the MADT; the FADT naming the FACS, the DSDT and the fixed
//! hardware's registers (`devices::pm`); and the DSDT, the one table in AML,
//! with the S5 sleep state, the PCI host bridge and the panic device
//! (`devices::pvpanic`), `QEMU0001` on its port, as QEMU's DSDT has it.
//!
//! The host bridge has to be there: a Linux with ACPI finds PCI through the
//! namespace alone -- the root bridges the DSDT describes -- and never probes
//! the bus itself, so without one it would find no disks and no NICs. Its
//! `_CRS` gives the bus numbers, the I/O ranges and the window the
//! functions' MSI-X pages are in -- a BAR outside every window is one an OS
//! moves, and these do not move -- and its `_PRT` says which 8259 IRQ each
//! device's INTA is wired to, as its interrupt line register does.
//!
//! A guest given an IO-APIC finds it in the MADT, with the two overrides a
//! PC's has: the timer's IRQ 0 on its pin 2, and the SCI, IRQ 9, a level --
//! active high, as the 8259's ELCR has it. Every other ISA IRQ is on the pin
//! of its own number, and the `_PRT`'s numbers are those pins too.
//!
//! A guest with the BARs' hole -- one that holds a host's function -- has
//! PCI Express's configuration space too (ECAM, `Machine::ecam`): an MCFG
//! naming it, and in the DSDT a motherboard resource (PNP0C02) whose `_CRS`
//! reserves it, as the PCI Firmware spec has firmware reserve it -- Linux
//! takes ECAM only where it is (`pci_mmcfg_reserved`) -- and the memory map
//! reserves it too, for a kernel that looks there first.
//!
//! What is left out would need MMIO -- the HPET -- or is what a guest does
//! without: processor C- or P-states, the RTC and timer as namespace devices,
//! and GPEs. The firmware machine also describes COM1 as PNP0501 and in an
//! SPCR, so an OS need not discover its console by probing. A guest of one
//! CPU, which has no APIC, gets no MADT; a guest whose kernel has no ACPI, or
//! is told `acpi=off`, finds its CPUs in the MP table as before.
//!
//! Everything is laid out into a buffer the caller copies into guest memory:
//! plain data, with its checksums, holding nothing of the guest's.

use crate::devices::pci::HostBridge;
use crate::devices::{ioapic, pm, pvpanic, rtc};
use crate::lapic;
use crate::run::{COM1, COM1_IRQ, RESET_CONTROL, RESET_VALUE};

/// Where the tables go: the BIOS area, from 0xE0000 to 1 MiB, which the
/// memory map reserves -- the RSDP at its start, 16-byte aligned, where the
/// scan for it begins.
pub const AREA: u64 = 0xE_0000;
pub const AREA_END: u64 = 0x10_0000;
/// The most bytes the tables take: more than the most CPUs and devices need
/// -- the caller lays them out in a buffer of this.
pub const MAX_BYTES: usize = 4096;

/// A PCI device's interrupt as the `_PRT` gives it: its slot on bus 0, and
/// the 8259 IRQ its INTA is wired to.
#[derive(Clone, Copy, Default)]
pub struct Route {
    pub slot: u8,
    pub irq: u8,
}

/// What the tables describe that differs from guest to guest.
pub struct Machine<'a> {
    /// Its CPUs, APIC IDs 0 up, the first the boot CPU; and whether they
    /// have local APICs -- a MADT only if so.
    pub cpus: u32,
    pub apic: bool,
    /// Its local APICs come out of reset in x2APIC mode: type 9 in the MADT,
    /// and the matching type-10 NMI.
    pub x2apic: bool,
    /// Its IO-APIC's ID, when it has one.
    pub ioapic: Option<u8>,
    /// Each device on the PCI bus, and its interrupt.
    pub pci: &'a [Route],
    /// The window its functions' memory BARs are in, when any has one: a
    /// base and a length.
    pub mmio: Option<(u32, u32)>,
    /// The windows a host's function's BARs are in (`run::add_host_function`):
    /// below 4 GiB, and above RAM -- a base and a length each.
    pub bars32: Option<(u32, u32)>,
    pub bars64: Option<(u64, u64)>,
    /// PCI Express's configuration space, where the guest has it: its base,
    /// below 4 GiB, and the last bus it reaches, from 0.
    pub ecam: Option<(u32, u8)>,
    /// The host bridge `_CRS` describes.
    pub host_bridge: crate::devices::pci::HostBridge,
}

/* Who made the tables, in every header. */
const OEM_ID: &[u8; 6] = b"NOS   ";
const OEM_TABLE_ID: &[u8; 8] = b"HV      ";
const OEM_REVISION: u32 = 1;
const CREATOR_ID: &[u8; 4] = b"NOS ";
const CREATOR_REVISION: u32 = 1;

/* Revisions: ACPI 6.3's tables. The DSDT's revision 2 makes its integers
 * 64 bits wide. */
const RSDP_REVISION: u8 = 2;
const XSDT_REVISION: u8 = 1;
const FADT_REVISION: u8 = 6;
const FADT_MINOR: u8 = 3;
const MADT_REVISION: u8 = 5;
const MCFG_REVISION: u8 = 1;
const SPCR_REVISION: u8 = 2;
const DSDT_REVISION: u8 = 2;
const FACS_VERSION: u8 = 2;

const RSDP_BYTES: usize = 36;
/// An ACPI 6.x FADT's length: every field through the hypervisor vendor's.
const FADT_BYTES: usize = 276;
/// The revision 2 Serial Port Console Redirection table, before revision 4's
/// optional clock and namespace fields.
const SPCR_BYTES: usize = 80;
/// The RSDP's checksum covers its first 20 bytes -- ACPI 1.0's RSDP; the
/// extended checksum all of it.
const RSDP_V1_BYTES: usize = 20;
const FACS_BYTES: u32 = 64;
/// The FACS's alignment, which the specification requires; the others' is
/// only tidiness.
const FACS_ALIGN: usize = 64;
const TABLE_ALIGN: usize = 16;

/* The FADT's fields. */
/// PM profile: unspecified.
const PM_PROFILE_UNSPECIFIED: u8 = 0;
/// C2 and C3 latencies past these say the states are not supported.
const C2_UNSUPPORTED: u16 = 101;
const C3_UNSUPPORTED: u16 = 1001;
/// IA-PC boot architecture flags: there are legacy (ISA) devices -- the
/// serial port, the RTC, the PIT, the 8259s; no 8042 -- its port reads float
/// and it is left unprobed; no VGA.
const BOOT_LEGACY_DEVICES: u16 = 1 << 0;
const BOOT_NO_VGA: u16 = 1 << 2;
/* Fixed feature flags (ACPI 6.5, table 5.10). */
/// WBINVD flushes and invalidates: the guest's memory is coherent, and its
/// WBINVD, stepped past, has nothing to flush that matters.
const FLAG_WBINVD: u32 = 1 << 0;
/// C1, HLT, on every CPU.
const FLAG_PROC_C1: u32 = 1 << 2;
/// No fixed sleep button. (The power button is fixed: its bit, 4, is clear.)
const FLAG_SLP_BUTTON: u32 = 1 << 5;
/// RTC wake status is not in the fixed registers.
const FLAG_FIX_RTC: u32 = 1 << 6;
/// The reset register is there.
const FLAG_RESET_REG_SUP: u32 = 1 << 10;
/// No monitor, keyboard or mouse to detect.
const FLAG_HEADLESS: u32 = 1 << 12;
/// No legacy PM1 blocks or FACS: sleep control/status and the PM timer are
/// the whole fixed hardware interface (the firmware machine, as CloudHv's).
const FLAG_HW_REDUCED: u32 = 1 << 20;
const FADT_FLAGS: u32 = FLAG_WBINVD | FLAG_PROC_C1 | FLAG_SLP_BUTTON | FLAG_FIX_RTC | FLAG_RESET_REG_SUP | FLAG_HEADLESS;

/* A generic address structure: its address space and access size. */
const GAS_SYSTEM_IO: u8 = 1;
const ACCESS_BYTE: u8 = 1;
const ACCESS_WORD: u8 = 2;
const ACCESS_DWORD: u8 = 3;

/* The firmware console's SPCR: a full 16550 at COM1, 115200 8N1, available
 * through both the PC's interrupt controllers, and no flow control. */
const SPCR_INTERFACE_16550: u8 = 0;
const SPCR_INTERRUPT_8259: u8 = 1;
const SPCR_INTERRUPT_APIC: u8 = 2;
const SPCR_BAUD_115200: u8 = 7;
const SPCR_NO_PARITY: u8 = 0;
const SPCR_ONE_STOP_BIT: u8 = 1;
const SPCR_NO_FLOW_CONTROL: u8 = 0;
const SPCR_TERMINAL_VT_UTF8: u8 = 2;
const SPCR_NOT_PCI: u16 = 0xFFFF;

/* The MADT: its flags and entries. */
/// There are 8259s too, which an OS masks before it uses the APICs.
const MADT_PCAT_COMPAT: u32 = 1 << 0;
const MADT_LOCAL_APIC: u8 = 0;
const MADT_LOCAL_APIC_BYTES: u8 = 8;
const MADT_LOCAL_X2APIC: u8 = 9;
const MADT_LOCAL_X2APIC_BYTES: u8 = 16;
const MADT_LOCAL_APIC_NMI: u8 = 4;
const MADT_LOCAL_APIC_NMI_BYTES: u8 = 6;
const MADT_LOCAL_X2APIC_NMI: u8 = 10;
const MADT_LOCAL_X2APIC_NMI_BYTES: u8 = 12;
const MADT_IO_APIC: u8 = 1;
const MADT_IO_APIC_BYTES: u8 = 12;
const MADT_OVERRIDE: u8 = 2;
const MADT_OVERRIDE_BYTES: u8 = 10;
/// An override's bus: ISA.
const MADT_BUS_ISA: u8 = 0;
/// An override's flags: polarity and trigger as the bus has them -- ISA's,
/// edge and active high -- or level and active high, the SCI's.
const MADT_FLAGS_BUS: u16 = 0;
const MADT_FLAGS_LEVEL_HIGH: u16 = 0x1 | (0x3 << 2);
const MADT_ENABLED: u32 = 1 << 0;
/// The processor UID that means every processor, in an xAPIC or x2APIC NMI.
const MADT_ALL_PROCESSORS: u8 = 0xFF;
const MADT_ALL_X2APIC_PROCESSORS: u32 = u32::MAX;
/// NMI on LINT1, as a PC wires it; polarity and trigger as the bus has them.
const MADT_NMI_LINT: u8 = 1;
const MADT_NMI_FLAGS: u16 = 0;
/// The most CPUs a MADT of 8-bit APIC IDs lists.
const MADT_MAX_CPUS: u32 = 255;

/* AML: the opcodes and prefixes the DSDT uses (ACPI 6.5, chapter 20). */
const ZERO_OP: u8 = 0x00;
const ONE_OP: u8 = 0x01;
const NAME_OP: u8 = 0x08;
const BYTE_PREFIX: u8 = 0x0A;
const WORD_PREFIX: u8 = 0x0B;
const DWORD_PREFIX: u8 = 0x0C;
const QWORD_PREFIX: u8 = 0x0E;
const STRING_PREFIX: u8 = 0x0D;
const SCOPE_OP: u8 = 0x10;
const BUFFER_OP: u8 = 0x11;
const PACKAGE_OP: u8 = 0x12;
const EXT_OP_PREFIX: u8 = 0x5B;
const DEVICE_OP: u8 = 0x82;
const ROOT_CHAR: u8 = b'\\';
/// A package length in three bytes: this in the lead byte's top bits (two
/// bytes follow), the length's low nibble in its bottom ones. Always three,
/// so it can be written before what it measures is: AML takes a longer
/// encoding than a length needs, as coreboot's generator has long relied on.
const PKG_LENGTH_BYTES: usize = 3;
const PKG_LENGTH_TWO_FOLLOW: u8 = 0x80;
const PKG_LENGTH_MAX: usize = 1 << 20;
/// PNP0A05, a CPU container; PNP0A03, a PCI host bridge; PNP0C02, a
/// motherboard's resources; and PNP0501, a 16550 serial port, as compressed
/// EISA IDs.
const EISA_PNP0A05: u32 = 0x050A_D041;
const EISA_PNP0A03: u32 = 0x030A_D041;
const EISA_PNP0C02: u32 = 0x020C_D041;
const EISA_PNP0501: u32 = 0x0105_D041;
/// A fixed range of 32-bit memory, read-write (ACPI 6.5, 6.4.3.4): what the
/// motherboard's `_CRS` reserves ECAM by -- with the end, its bytes.
const MEMORY32_FIXED_TAG: u8 = 0x86;
const MEMORY32_FIXED_BYTES: u16 = 9;
const MEMORY32_READ_WRITE: u8 = 1;
const ECAM_CRS_BYTES: u64 = 14;
/// A bus's MiB of ECAM.
const ECAM_BUS_SHIFT: u32 = 20;
/// A `_PRT` entry's address: any function of the slot.
const PRT_ANY_FUNCTION: u64 = 0xFFFF;
const PRT_SLOT_SHIFT: u32 = 16;
/// INTA, as a `_PRT` numbers pins.
const PRT_PIN_INTA: u64 = 0;
/// A `_PRT` entry's source of Zero: the index is a global interrupt -- the
/// 8259's IRQ, the machine being in PIC mode.
const PRT_SOURCE_GSI: u64 = 0;
const PRT_ENTRY_ELEMENTS: u8 = 4;
const S5_ELEMENTS: u8 = 4;

/* Resource descriptors, for the host bridge's `_CRS` (ACPI 6.5, 6.4). */
const IO_PORT_TAG: u8 = 0x47;
const IO_DECODE_16: u8 = 1;
/// A two-byte IRQ mask, with no optional trigger/polarity byte.
const IRQ_NO_FLAGS_TAG: u8 = 0x22;
const WORD_SPACE_TAG: u8 = 0x88;
const WORD_SPACE_BYTES: u16 = 13;
const DWORD_SPACE_TAG: u8 = 0x87;
const DWORD_SPACE_BYTES: u16 = 23;
const QWORD_SPACE_TAG: u8 = 0x8A;
const QWORD_SPACE_BYTES: u16 = 43;
const END_TAG: u8 = 0x79;
const SPACE_MEMORY: u8 = 0;
const SPACE_IO: u8 = 1;
const SPACE_BUS: u8 = 2;
/// General flags: a range the bridge produces, at a fixed minimum and
/// maximum, positively decoded.
const PRODUCER_FIXED: u8 = 0x0C;
/// I/O: both ISA and non-ISA addresses.
const IO_ENTIRE_RANGE: u8 = 0x03;
/// Memory: read-write, not cacheable.
const MEMORY_READ_WRITE: u8 = 0x01;
/// The configuration mechanism's ports, the bridge's own, and the I/O
/// either side of them.
const CONFIG_PORTS: u16 = 0xCF8;
const CONFIG_PORTS_LEN: u8 = 8;
const BUSES: u16 = 0x100;
/// The room a `_CRS` takes: every descriptor it may have.
const CRS_MAX: usize = 192;
/// The panic device's ID, which Linux's `pvpanic-mmio` binds to, and the
/// bytes of its `_CRS`: its port, and the end.
const PVPANIC_HID: &[u8; 8] = b"QEMU0001";
const PVPANIC_CRS_BYTES: u64 = 10;
/// COM1's I/O descriptor, IRQ 4's mask, and the resource template's end.
const COM1_CRS_BYTES: u64 = 15;

/// A cursor over the bytes the tables are laid out in. A write past their
/// end writes nothing and sets `overflow`, which is looked at once, at the
/// end: no table is made of a guest's input, so one that does not fit is a
/// machine larger than this was sized for, not an attack.
struct Out<'b> {
    buf: &'b mut [u8],
    at: usize,
    overflow: bool,
}

impl<'b> Out<'b> {
    fn new(buf: &'b mut [u8]) -> Out<'b> {
        Out { buf, at: 0, overflow: false }
    }

    fn put(&mut self, bytes: &[u8]) {
        match self.buf.get_mut(self.at..self.at + bytes.len()) {
            Some(to) => {
                to.copy_from_slice(bytes);
                self.at += bytes.len();
            }
            None => self.overflow = true,
        }
    }

    fn u8(&mut self, v: u8) {
        self.put(&[v]);
    }

    fn u16(&mut self, v: u16) {
        self.put(&v.to_le_bytes());
    }

    fn u32(&mut self, v: u32) {
        self.put(&v.to_le_bytes());
    }

    fn u64(&mut self, v: u64) {
        self.put(&v.to_le_bytes());
    }

    /// Zeros up to the next multiple of `to` (at most 64).
    fn align(&mut self, to: usize) {
        const ZEROS: [u8; FACS_ALIGN] = [0; FACS_ALIGN];
        let pad = (to - self.at % to) % to;
        self.put(&ZEROS[..pad.min(ZEROS.len())]);
    }

    /// Write `bytes` over what is at `at`, behind the cursor.
    fn patch(&mut self, at: usize, bytes: &[u8]) {
        match self.buf.get_mut(at..at + bytes.len()) {
            Some(to) => to.copy_from_slice(bytes),
            None => self.overflow = true,
        }
    }

    /// What has been written from `from` on.
    fn since(&self, from: usize) -> &[u8] {
        self.buf.get(from..self.at).unwrap_or(&[])
    }
}

/// The byte that makes `bytes` sum to zero.
fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b)))
}

/// Where the byte at offset `at` of the area is, in guest physical memory.
fn gpa(at: usize) -> u64 {
    AREA + at as u64
}

/// Lay the tables out in `buf`, as they are to sit at `AREA`: how many
/// bytes they took, or None when they do not fit -- a machine larger than
/// `MAX_BYTES` was sized for.
pub fn build(m: &Machine, buf: &mut [u8]) -> Option<usize> {
    let mut o = Out::new(buf);
    /* The RSDP's place, filled once the XSDT's address is known. */
    o.put(&[0; RSDP_BYTES]);

    let facs = if m.host_bridge == HostBridge::CloudHv {
        None
    } else {
        o.align(FACS_ALIGN);
        let at = o.at;
        write_facs(&mut o);
        Some(gpa(at))
    };
    o.align(TABLE_ALIGN);
    let dsdt = o.at;
    write_dsdt(&mut o, m);
    o.align(TABLE_ALIGN);
    let fadt = o.at;
    write_fadt(&mut o, m, facs, gpa(dsdt));
    let madt = if m.apic {
        o.align(TABLE_ALIGN);
        let at = o.at;
        write_madt(&mut o, m.cpus, m.x2apic, m.ioapic);
        Some(gpa(at))
    } else {
        None
    };
    let mcfg = m.ecam.map(|(base, last)| {
        o.align(TABLE_ALIGN);
        let at = o.at;
        write_mcfg(&mut o, base, last);
        gpa(at)
    });
    let spcr = if m.host_bridge == HostBridge::CloudHv {
        o.align(TABLE_ALIGN);
        let at = o.at;
        write_spcr(&mut o);
        Some(gpa(at))
    } else {
        None
    };
    o.align(TABLE_ALIGN);
    let xsdt = o.at;
    let start = header(&mut o, b"XSDT", XSDT_REVISION);
    o.u64(gpa(fadt));
    if let Some(madt) = madt {
        o.u64(madt);
    }
    if let Some(mcfg) = mcfg {
        o.u64(mcfg);
    }
    if let Some(spcr) = spcr {
        o.u64(spcr);
    }
    seal(&mut o, start);

    let rsdp = rsdp(gpa(xsdt));
    o.patch(0, &rsdp);
    (!o.overflow).then_some(o.at)
}

/// The RSDP, ACPI 2.0's: the XSDT's address, and no RSDT.
fn rsdp(xsdt: u64) -> [u8; RSDP_BYTES] {
    let mut r = [0u8; RSDP_BYTES];
    r[0..8].copy_from_slice(b"RSD PTR ");
    r[9..15].copy_from_slice(OEM_ID);
    r[15] = RSDP_REVISION;
    r[20..24].copy_from_slice(&(RSDP_BYTES as u32).to_le_bytes());
    r[24..32].copy_from_slice(&xsdt.to_le_bytes());
    /* The first checksum first: the extended one covers it. */
    r[8] = checksum(&r[..RSDP_V1_BYTES]);
    r[32] = checksum(&r);
    r
}

/// A table's header, its length and checksum left for `seal`: where the
/// table starts.
fn header(o: &mut Out, signature: &[u8; 4], revision: u8) -> usize {
    let start = o.at;
    o.put(signature);
    o.u32(0);
    o.u8(revision);
    o.u8(0);
    o.put(OEM_ID);
    o.put(OEM_TABLE_ID);
    o.u32(OEM_REVISION);
    o.put(CREATOR_ID);
    o.u32(CREATOR_REVISION);
    start
}

/// The table from `start` to here, its length and checksum put in.
fn seal(o: &mut Out, start: usize) {
    let len = o.at - start;
    o.patch(start + 4, &(len as u32).to_le_bytes());
    let sum = checksum(o.since(start));
    o.patch(start + 9, &[sum]);
}

/// The MCFG (PCI Firmware 3.3, 4.1.2): ECAM's one allocation, at `base`,
/// of segment 0, buses 0 to `last`.
fn write_mcfg(o: &mut Out, base: u32, last: u8) {
    let start = header(o, b"MCFG", MCFG_REVISION);
    o.put(&[0; 8]);
    o.u64(u64::from(base));
    o.u16(0);
    o.u8(0);
    o.u8(last);
    o.u32(0);
    seal(o, start);
}

/// The firmware's serial console, in the revision 2 SPCR layout: its register
/// interface, address, interrupt, framing and terminal, and no PCI function.
fn write_spcr(o: &mut Out) {
    let start = header(o, b"SPCR", SPCR_REVISION);
    o.u8(SPCR_INTERFACE_16550);
    o.put(&[0; 3]);
    gas_io(o, 1, ACCESS_BYTE, COM1);
    o.u8(SPCR_INTERRUPT_8259 | SPCR_INTERRUPT_APIC);
    o.u8(COM1_IRQ);
    o.u32(u32::from(COM1_IRQ));
    o.u8(SPCR_BAUD_115200);
    o.u8(SPCR_NO_PARITY);
    o.u8(SPCR_ONE_STOP_BIT);
    o.u8(SPCR_NO_FLOW_CONTROL);
    o.u8(SPCR_TERMINAL_VT_UTF8);
    o.u8(0); // reserved
    o.u16(SPCR_NOT_PCI);
    o.u16(SPCR_NOT_PCI);
    o.u8(0); // PCI bus
    o.u8(0); // PCI device
    o.u8(0); // PCI function
    o.u32(0); // PCI flags
    o.u8(0); // PCI segment
    o.u32(0); // reserved
    if o.at - start != SPCR_BYTES {
        o.overflow = true;
    }
    seal(o, start);
}

/// The FACS: no firmware waking vector -- nothing is woken -- and a global
/// lock nobody but the guest ever takes.
fn write_facs(o: &mut Out) {
    let start = o.at;
    o.put(b"FACS");
    o.u32(FACS_BYTES);
    o.u32(0); // hardware signature
    o.u32(0); // firmware waking vector
    o.u32(0); // global lock
    o.u32(0); // flags
    o.u64(0); // 64-bit firmware waking vector
    o.u8(FACS_VERSION);
    o.put(&[0; 3]);
    o.u32(0); // OSPM flags
    o.put(&[0; 24]);
    if o.at - start != FACS_BYTES as usize {
        o.overflow = true;
    }
}

/// A generic address of `bytes` bytes of I/O ports at `port`, accessed
/// `access` at a time.
fn gas_io(o: &mut Out, bytes: u8, access: u8, port: u16) {
    o.u8(GAS_SYSTEM_IO);
    o.u8(bytes * 8);
    o.u8(0);
    o.u8(access);
    o.u64(u64::from(port));
}

/// A generic address that says there is none.
fn gas_none(o: &mut Out) {
    o.put(&[0; 12]);
}

/// The FADT: the fixed hardware's ports, the SCI, the reset register, and
/// where the FACS and the DSDT are. Each legacy PM1 block is given twice, as
/// ACPI 1.0's port and as the generic address that supersedes it, and the two
/// agree -- an OS checks, and says so when they do not. The firmware machine
/// is hardware-reduced instead: no FACS or PM1 blocks, its sleep registers in
/// the two generic addresses ACPI 5.0 added for them.
fn write_fadt(o: &mut Out, m: &Machine, facs: Option<u64>, dsdt: u64) {
    let start = header(o, b"FACP", FADT_REVISION);
    let reduced = m.host_bridge == HostBridge::CloudHv;
    /* The FACS by its 32-bit address, and not the 64-bit one as well: the
     * specification has one of the two zero. A hardware-reduced FADT has no
     * FACS at all. */
    o.u32(facs.unwrap_or(0) as u32);
    o.u32(dsdt as u32);
    o.u8(0); // reserved
    o.u8(PM_PROFILE_UNSPECIFIED);
    o.u16(u16::from(pm::SCI_IRQ));
    /* No SMI command port, and so no ACPI_ENABLE or ACPI_DISABLE to write to
     * it: the machine is in ACPI mode from the start. */
    o.u32(0);
    o.u8(0); // ACPI_ENABLE
    o.u8(0); // ACPI_DISABLE
    o.u8(0); // S4BIOS_REQ
    o.u8(0); // PSTATE_CNT
    o.u32(if reduced { 0 } else { u32::from(pm::PM1_EVT) });
    o.u32(0); // PM1b event block
    o.u32(if reduced { 0 } else { u32::from(pm::PM1_CNT) });
    o.u32(0); // PM1b control block
    o.u32(0); // PM2 control block
    o.u32(u32::from(pm::PM_TMR));
    o.u32(0); // GPE0 block
    o.u32(0); // GPE1 block
    o.u8(if reduced { 0 } else { pm::PM1_EVT_LEN });
    o.u8(if reduced { 0 } else { pm::PM1_CNT_LEN });
    o.u8(0); // PM2 control length
    o.u8(pm::PM_TMR_LEN);
    o.u8(0); // GPE0 block length
    o.u8(0); // GPE1 block length
    o.u8(0); // GPE1 base
    o.u8(0); // CST_CNT
    o.u16(C2_UNSUPPORTED);
    o.u16(C3_UNSUPPORTED);
    o.u16(0); // FLUSH_SIZE
    o.u16(0); // FLUSH_STRIDE
    o.u8(0); // DUTY_OFFSET
    o.u8(0); // DUTY_WIDTH
    o.u8(0); // DAY_ALRM: no day-of-month alarm
    o.u8(0); // MON_ALRM
    o.u8(rtc::CENTURY);
    o.u16(BOOT_LEGACY_DEVICES | BOOT_NO_VGA);
    o.u8(0); // reserved
    o.u32(FADT_FLAGS | if reduced { FLAG_HW_REDUCED } else { 0 });
    gas_io(o, 1, ACCESS_BYTE, RESET_CONTROL);
    o.u8(RESET_VALUE);
    o.u16(0); // ARM boot architecture
    o.u8(FADT_MINOR);
    o.u64(0); // the FACS's 64-bit address: FIRMWARE_CTRL has it
    o.u64(dsdt);
    if reduced {
        gas_none(o);
    } else {
        gas_io(o, pm::PM1_EVT_LEN, ACCESS_WORD, pm::PM1_EVT);
    }
    gas_none(o); // PM1b event block
    if reduced {
        gas_none(o);
    } else {
        gas_io(o, pm::PM1_CNT_LEN, ACCESS_WORD, pm::PM1_CNT);
    }
    gas_none(o); // PM1b control block
    gas_none(o); // PM2 control block
    gas_io(o, pm::PM_TMR_LEN, ACCESS_DWORD, pm::PM_TMR);
    gas_none(o); // GPE0 block
    gas_none(o); // GPE1 block
    if reduced {
        gas_io(o, 1, ACCESS_BYTE, pm::SLEEP_CONTROL);
        gas_io(o, 1, ACCESS_BYTE, pm::SLEEP_STATUS);
    } else {
        gas_none(o);
        gas_none(o);
    }
    o.u64(0); // hypervisor vendor identity
    if o.at - start != FADT_BYTES {
        o.overflow = true;
    }
    seal(o, start);
}

/// The MADT: each CPU's local APIC, or x2APIC when `x2apic`, enabled, its
/// processor UID its APIC ID; the IO-APIC whose ID is `ioapic`, when there is
/// one, its pins global interrupts 0 up, with the overrides of the timer's IRQ
/// and the SCI's; and NMI on every CPU's LINT1.
fn write_madt(o: &mut Out, cpus: u32, x2apic: bool, ioapic: Option<u8>) {
    if cpus == 0 || cpus > MADT_MAX_CPUS {
        o.overflow = true;
        return;
    }
    let start = header(o, b"APIC", MADT_REVISION);
    o.u32(lapic::DEFAULT_BASE as u32);
    o.u32(MADT_PCAT_COMPAT);
    for id in 0..cpus as u8 {
        if x2apic {
            o.u8(MADT_LOCAL_X2APIC);
            o.u8(MADT_LOCAL_X2APIC_BYTES);
            o.u16(0); // reserved
            o.u32(id as u32);
            o.u32(MADT_ENABLED);
            o.u32(id as u32);
        } else {
            o.u8(MADT_LOCAL_APIC);
            o.u8(MADT_LOCAL_APIC_BYTES);
            o.u8(id);
            o.u8(id);
            o.u32(MADT_ENABLED);
        }
    }
    if let Some(id) = ioapic {
        o.u8(MADT_IO_APIC);
        o.u8(MADT_IO_APIC_BYTES);
        o.u8(id);
        o.u8(0);
        o.u32(ioapic::BASE as u32);
        o.u32(0);
        for (irq, flags) in [(0u8, MADT_FLAGS_BUS), (pm::SCI_IRQ, MADT_FLAGS_LEVEL_HIGH)] {
            o.u8(MADT_OVERRIDE);
            o.u8(MADT_OVERRIDE_BYTES);
            o.u8(MADT_BUS_ISA);
            o.u8(irq);
            o.u32(ioapic::isa_pin(irq) as u32);
            o.u16(flags);
        }
    }
    if x2apic {
        o.u8(MADT_LOCAL_X2APIC_NMI);
        o.u8(MADT_LOCAL_X2APIC_NMI_BYTES);
        o.u16(MADT_NMI_FLAGS);
        o.u32(MADT_ALL_X2APIC_PROCESSORS);
        o.u8(MADT_NMI_LINT);
        o.put(&[0; 3]); // reserved
    } else {
        o.u8(MADT_LOCAL_APIC_NMI);
        o.u8(MADT_LOCAL_APIC_NMI_BYTES);
        o.u8(MADT_ALL_PROCESSORS);
        o.u16(MADT_NMI_FLAGS);
        o.u8(MADT_NMI_LINT);
    }
    seal(o, start);
}

/// Open an AML object measured by a package length: its opcode, and the
/// length's room. Where the length is, for `close`.
fn open(o: &mut Out, op: u8) -> usize {
    o.u8(op);
    let at = o.at;
    o.put(&[0; PKG_LENGTH_BYTES]);
    at
}

/// Close the object whose length is at `at`: the length counts itself and
/// everything after it.
fn close(o: &mut Out, at: usize) {
    let len = o.at - at;
    if len >= PKG_LENGTH_MAX {
        o.overflow = true;
        return;
    }
    o.patch(at, &[PKG_LENGTH_TWO_FOLLOW | (len & 0xF) as u8, (len >> 4) as u8, (len >> 12) as u8]);
}

/// An AML integer, in the fewest bytes that hold it.
fn int(o: &mut Out, v: u64) {
    match v {
        0 => o.u8(ZERO_OP),
        1 => o.u8(ONE_OP),
        v if v <= 0xFF => {
            o.u8(BYTE_PREFIX);
            o.u8(v as u8);
        }
        v if v <= 0xFFFF => {
            o.u8(WORD_PREFIX);
            o.u16(v as u16);
        }
        v if v <= 0xFFFF_FFFF => {
            o.u8(DWORD_PREFIX);
            o.u32(v as u32);
        }
        v => {
            o.u8(QWORD_PREFIX);
            o.u64(v);
        }
    }
}

/// `Name (seg, ...)`: the object that follows is its value.
fn name(o: &mut Out, seg: &[u8; 4]) {
    o.u8(NAME_OP);
    o.put(seg);
}

/// The DSDT:
///
/// ```text
/// Name (\_S5, Package () { 5, 5, 0, 0 })
/// Scope (\_SB) {
///     Device (PCI0) {
///         Name (_HID, EisaId ("PNP0A03"))
///         Name (_UID, 0)
///         Name (_CRS, ResourceTemplate () { ... })
///         Name (_PRT, Package () { Package () { 0xSSSSFFFF, 0, 0, IRQ }, ... })
///     }
///     Device (PEVT) {
///         Name (_HID, "QEMU0001")
///         Name (_CRS, ResourceTemplate () { IO (Decode16, 0x505, 0x505, 1, 1) })
///     }
/// }
/// ```
fn write_dsdt(o: &mut Out, m: &Machine) {
    let start = header(o, b"DSDT", DSDT_REVISION);

    /* S5, soft off: the sleep type for PM1a's control register and PM1b's,
     * which there is none of. */
    name(o, b"_S5_");
    let s5 = open(o, PACKAGE_OP);
    o.u8(S5_ELEMENTS);
    int(o, u64::from(pm::SLP_TYP_S5));
    int(o, u64::from(pm::SLP_TYP_S5));
    int(o, 0);
    int(o, 0);
    close(o, s5);

    let scope = open(o, SCOPE_OP);
    o.u8(ROOT_CHAR);
    o.put(b"_SB_");
    o.u8(EXT_OP_PREFIX);
    let device = open(o, DEVICE_OP);
    o.put(b"PCI0");
    name(o, b"_HID");
    int(o, u64::from(EISA_PNP0A03));
    name(o, b"_UID");
    int(o, 0);

    let mut crs = [0u8; CRS_MAX];
    let mut r = Out::new(&mut crs);
    write_crs(&mut r, m);
    let (len, overflow) = (r.at, r.overflow);
    o.overflow |= overflow;
    name(o, b"_CRS");
    let buffer = open(o, BUFFER_OP);
    int(o, len as u64);
    o.put(&crs[..len]);
    close(o, buffer);

    /* A `_PRT` of no entries would be one an OS warns of, and nothing asks
     * for it: a bus with no devices has none. */
    if !m.pci.is_empty() {
        let Ok(entries) = u8::try_from(m.pci.len()) else {
            o.overflow = true;
            return;
        };
        name(o, b"_PRT");
        let prt = open(o, PACKAGE_OP);
        o.u8(entries);
        for route in m.pci {
            let entry = open(o, PACKAGE_OP);
            o.u8(PRT_ENTRY_ELEMENTS);
            int(o, (u64::from(route.slot) << PRT_SLOT_SHIFT) | PRT_ANY_FUNCTION);
            int(o, PRT_PIN_INTA);
            int(o, PRT_SOURCE_GSI);
            int(o, u64::from(route.irq));
            close(o, entry);
        }
        close(o, prt);
    }

    close(o, device);

    o.u8(EXT_OP_PREFIX);
    let panic = open(o, DEVICE_OP);
    o.put(b"PEVT");
    name(o, b"_HID");
    o.u8(STRING_PREFIX);
    o.put(PVPANIC_HID);
    o.u8(0);
    name(o, b"_CRS");
    let buffer = open(o, BUFFER_OP);
    int(o, PVPANIC_CRS_BYTES);
    o.u8(IO_PORT_TAG);
    o.u8(IO_DECODE_16);
    o.u16(pvpanic::PORT);
    o.u16(pvpanic::PORT);
    o.u8(1); // alignment
    o.u8(1); // length
    o.u8(END_TAG);
    o.u8(0);
    close(o, buffer);
    close(o, panic);

    /* ECAM, where the guest has it: a motherboard resource reserving it */
    if let Some((base, last)) = m.ecam {
        o.u8(EXT_OP_PREFIX);
        let ecam = open(o, DEVICE_OP);
        o.put(b"ECAM");
        name(o, b"_HID");
        int(o, u64::from(EISA_PNP0C02));
        name(o, b"_CRS");
        let buffer = open(o, BUFFER_OP);
        int(o, ECAM_CRS_BYTES);
        o.u8(MEMORY32_FIXED_TAG);
        o.u16(MEMORY32_FIXED_BYTES);
        o.u8(MEMORY32_READ_WRITE);
        o.u32(base);
        o.u32((u32::from(last) + 1) << ECAM_BUS_SHIFT);
        o.u8(END_TAG);
        o.u8(0);
        close(o, buffer);
        close(o, ecam);
    }

    if m.host_bridge == HostBridge::CloudHv {
        o.u8(EXT_OP_PREFIX);
        let serial = open(o, DEVICE_OP);
        o.put(b"COM1");
        name(o, b"_HID");
        int(o, u64::from(EISA_PNP0501));
        name(o, b"_UID");
        int(o, 1);
        name(o, b"_CRS");
        let buffer = open(o, BUFFER_OP);
        int(o, COM1_CRS_BYTES);
        o.u8(IO_PORT_TAG);
        o.u8(IO_DECODE_16);
        o.u16(COM1);
        o.u16(COM1);
        o.u8(0); // alignment
        o.u8(8); // eight registers
        o.u8(IRQ_NO_FLAGS_TAG);
        o.u16(1u16 << COM1_IRQ);
        o.u8(END_TAG);
        o.u8(0);
        close(o, buffer);
        close(o, serial);
    }

    if m.apic {
        write_cpus(o, m.cpus);
    }
    close(o, scope);
    seal(o, start);
}

/// The processors the MADT lists, as ACPI devices for an OS that enumerates
/// processors through the namespace too. No _STA: its absence means present,
/// enabled and working. No hotplug state or methods belong to a static VM.
fn write_cpus(o: &mut Out, cpus: u32) {
    if cpus == 0 || cpus > MADT_MAX_CPUS {
        o.overflow = true;
        return;
    }
    o.u8(EXT_OP_PREFIX);
    let container = open(o, DEVICE_OP);
    o.put(b"CPUS");
    name(o, b"_HID");
    o.u8(STRING_PREFIX);
    o.put(b"ACPI0010");
    o.u8(0);
    name(o, b"_CID");
    int(o, u64::from(EISA_PNP0A05));
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for id in 0..cpus {
        o.u8(EXT_OP_PREFIX);
        let cpu = open(o, DEVICE_OP);
        o.put(&[b'C', HEX[((id >> 8) & 0xF) as usize], HEX[((id >> 4) & 0xF) as usize], HEX[(id & 0xF) as usize]]);
        name(o, b"_HID");
        o.u8(STRING_PREFIX);
        o.put(b"ACPI0007");
        o.u8(0);
        name(o, b"_UID");
        int(o, u64::from(id));
        close(o, cpu);
    }
    close(o, container);
}

/// The host bridge's resources: bus 0 to 255 -- to ECAM's last, where the
/// guest has it, which has to cover the bridge's (Linux says so, as
/// firmware's fault) --; the configuration ports, which it decodes itself;
/// Q35's I/O either side of them, or CloudHv's 0xC000-0xFFFF alone; and the
/// memory windows the BARs are in.
fn write_crs(r: &mut Out, m: &Machine) {
    let last = m.ecam.map_or(BUSES - 1, |(_, last)| u16::from(last));
    word_space(r, SPACE_BUS, 0, 0, last, last + 1);
    r.u8(IO_PORT_TAG);
    r.u8(IO_DECODE_16);
    r.u16(CONFIG_PORTS);
    r.u16(CONFIG_PORTS);
    r.u8(1); // alignment
    r.u8(CONFIG_PORTS_LEN);
    match m.host_bridge {
        HostBridge::Q35 => {
            word_space(r, SPACE_IO, IO_ENTIRE_RANGE, 0, CONFIG_PORTS - 1, CONFIG_PORTS);
            /* From 0xD00 to the top: the length is what is left of 64 KiB. */
            let above = CONFIG_PORTS + u16::from(CONFIG_PORTS_LEN);
            word_space(r, SPACE_IO, IO_ENTIRE_RANGE, above, u16::MAX, 0u16.wrapping_sub(above));
        }
        HostBridge::CloudHv => {
            /* OVMF's aperture, and none of the legacy ports Windows PnP
             * must not rebalance away from COM1 and the chipset. */
            word_space(r, SPACE_IO, IO_ENTIRE_RANGE, 0xC000, u16::MAX, 0x4000);
        }
    }
    for (base, len) in [m.mmio, m.bars32].into_iter().flatten() {
        let last = (u64::from(base) + u64::from(len)).checked_sub(1).and_then(|l| u32::try_from(l).ok());
        match last {
            Some(last) if len != 0 => {
                r.u8(DWORD_SPACE_TAG);
                r.u16(DWORD_SPACE_BYTES);
                r.u8(SPACE_MEMORY);
                r.u8(PRODUCER_FIXED);
                r.u8(MEMORY_READ_WRITE);
                r.u32(0); // granularity
                r.u32(base);
                r.u32(last);
                r.u32(0); // translation
                r.u32(len);
            }
            _ => r.overflow = true,
        }
    }
    if let Some((base, len)) = m.bars64 {
        match base.checked_add(len).and_then(|e| e.checked_sub(1)) {
            Some(last) if len != 0 => {
                r.u8(QWORD_SPACE_TAG);
                r.u16(QWORD_SPACE_BYTES);
                r.u8(SPACE_MEMORY);
                r.u8(PRODUCER_FIXED);
                r.u8(MEMORY_READ_WRITE);
                r.u64(0); // granularity
                r.u64(base);
                r.u64(last);
                r.u64(0); // translation
                r.u64(len);
            }
            _ => r.overflow = true,
        }
    }
    r.u8(END_TAG);
    /* A checksum of zero: the template is taken as it is. */
    r.u8(0);
}

/// A word address space descriptor: a range of `kind` the bridge produces,
/// from `min` to `max`, `len` long.
fn word_space(r: &mut Out, kind: u8, type_flags: u8, min: u16, max: u16, len: u16) {
    r.u8(WORD_SPACE_TAG);
    r.u16(WORD_SPACE_BYTES);
    r.u8(kind);
    r.u8(PRODUCER_FIXED);
    r.u8(type_flags);
    r.u16(0); // granularity
    r.u16(min);
    r.u16(max);
    r.u16(0); // translation
    r.u16(len);
}
