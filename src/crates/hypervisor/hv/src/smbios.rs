//! The SMBIOS tables a PC's firmware would have left a guest (DMTF DSP0134,
//! version 3): what the machine is and who made it -- what Linux shows under
//! `/sys/class/dmi/id`, what `dmidecode` prints, and what systemd, cloud-init
//! and licensing code know a machine by: its maker, its product, and its
//! UUID and serial, which are to stay the same from one boot to the next.
//!
//! A kernel with no EFI finds them by scanning the 64 KiB from 0xF0000 for an
//! entry point on a 16-byte boundary -- SMBIOS 3's, `_SM3_`, which gives the
//! structures' address in 64 bits -- so they go there, in the BIOS area the
//! memory map reserves (`acpi::AREA`), the structures right after the entry
//! point: the BIOS (type 0, which says the machine is a virtual one), the
//! system (1: the maker, the product, the version, the serial and the UUID),
//! its chassis (3), the boot (32: no errors) and the end (127).
//!
//! Everything is laid out into a buffer the caller copies into guest memory:
//! plain data, with the entry point's checksum, holding nothing of the
//! guest's. What the strings are made of is the operator's -- a VM's name --
//! so each is cut to `MAX_STRING` bytes of printable ASCII, anything else in
//! it a `?`, and an empty one a `-`: a structure's strings are told apart by
//! the zero between them, and an empty one would end its structure early.

/// Where the entry point goes: the start of the 64 KiB a kernel scans.
pub const AREA: u64 = 0xF_0000;
/// The most bytes the tables take, entry point and all.
pub const MAX_BYTES: usize = 1024;
/// The longest a string is, its zero not counted.
pub const MAX_STRING: usize = 64;

/// What differs from guest to guest.
pub struct Machine<'a> {
    /// Its UUID, as RFC 4122 writes it, most significant byte first.
    pub uuid: [u8; 16],
    /// What it is called -- a VM's name -- its serial number.
    pub serial: &'a str,
    /// The hypervisor's version, the BIOS's and the product's.
    pub version: &'a str,
}

const MAKER: &str = "nos";
const PRODUCT: &str = "nos hv";
const FAMILY: &str = "nos";
/// The BIOS's release date, as SMBIOS writes it: when these tables took
/// their form.
const BIOS_DATE: &str = "10/27/2026";

/* The entry point: its anchor, its length, the version the tables are,
 * and its revision. */
const ANCHOR: &[u8; 5] = b"_SM3_";
const ENTRY_BYTES: usize = 0x18;
const VERSION_MAJOR: u8 = 3;
const VERSION_MINOR: u8 = 2;
const ENTRY_REVISION: u8 = 1;

/* The structures: their types, and the lengths of their formatted parts. */
const TYPE_BIOS: u8 = 0;
const TYPE_SYSTEM: u8 = 1;
const TYPE_CHASSIS: u8 = 3;
const TYPE_BOOT: u8 = 32;
const TYPE_END: u8 = 127;
const BIOS_BYTES: u8 = 0x1A;
const SYSTEM_BYTES: u8 = 0x1B;
const CHASSIS_BYTES: u8 = 0x16;
const BOOT_BYTES: u8 = 0x0B;
const END_BYTES: u8 = 4;

/// The BIOS's characteristics: none it has to describe (bit 3); and of the
/// extension's second byte, that the tables describe a virtual machine.
const BIOS_CHARACTERISTICS_NOT_SUPPORTED: u64 = 1 << 3;
const BIOS_EXT2_VIRTUAL_MACHINE: u8 = 1 << 4;
/// Where a BIOS's code would start, in 16-byte paragraphs: the F segment.
const BIOS_SEGMENT: u16 = 0xF000;
/// A release number it does not have.
const NO_RELEASE: u8 = 0xFF;
/// The system woke by its power switch; the chassis is "other", every state
/// of it safe, its security unknown.
const WAKE_POWER_SWITCH: u8 = 6;
const CHASSIS_OTHER: u8 = 1;
const STATE_SAFE: u8 = 3;
const SECURITY_UNKNOWN: u8 = 2;

/// A buffer being laid out: bytes put at the end, None once it is full.
struct Out<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl Out<'_> {
    fn put(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.at.checked_add(bytes.len())?;
        self.buf.get_mut(self.at..end)?.copy_from_slice(bytes);
        self.at = end;
        Some(())
    }

    fn u8(&mut self, v: u8) -> Option<()> {
        self.put(&[v])
    }

    fn u16(&mut self, v: u16) -> Option<()> {
        self.put(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> Option<()> {
        self.put(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> Option<()> {
        self.put(&v.to_le_bytes())
    }

    /// A structure's header: its type, the length of its formatted part,
    /// its handle.
    fn header(&mut self, kind: u8, len: u8, handle: u16) -> Option<()> {
        self.u8(kind)?;
        self.u8(len)?;
        self.u16(handle)
    }

    /// A structure's strings after its formatted part, each as `string`
    /// makes it, then the zero that ends them -- two zeros for a structure
    /// with none.
    fn strings(&mut self, strings: &[&str]) -> Option<()> {
        if strings.is_empty() {
            self.u8(0)?;
        }
        for s in strings {
            self.string(s)?;
        }
        self.u8(0)
    }

    /// One string: printable ASCII, `MAX_STRING` bytes at the most, anything
    /// else a `?`, nothing a `-` -- and its zero.
    fn string(&mut self, s: &str) -> Option<()> {
        if s.is_empty() {
            self.u8(b'-')?;
        }
        for b in s.bytes().take(MAX_STRING) {
            self.u8(if (0x20..0x7F).contains(&b) { b } else { b'?' })?;
        }
        self.u8(0)
    }
}

/// The UUID as SMBIOS 2.6 on stores it: its first three fields least
/// significant byte first, the rest as RFC 4122 has it.
fn smbios_uuid(u: &[u8; 16]) -> [u8; 16] {
    let mut s = *u;
    s[0..4].reverse();
    s[4..6].reverse();
    s[6..8].reverse();
    s
}

/// The tables for `m`, laid out into `buf` to go at `AREA`: how many bytes,
/// or None when they do not fit.
pub fn build(m: &Machine, buf: &mut [u8]) -> Option<usize> {
    let structures = AREA + ENTRY_BYTES.next_multiple_of(16) as u64;
    let mut out = Out { buf, at: ENTRY_BYTES.next_multiple_of(16) };
    let start = out.at;

    /* Type 0, the BIOS: its vendor, version and date are strings 1 to 3. */
    out.header(TYPE_BIOS, BIOS_BYTES, 0)?;
    out.u8(1)?;
    out.u8(2)?;
    out.u16(BIOS_SEGMENT)?;
    out.u8(3)?;
    out.u8(0)?;
    out.u64(BIOS_CHARACTERISTICS_NOT_SUPPORTED)?;
    out.u8(0)?;
    out.u8(BIOS_EXT2_VIRTUAL_MACHINE)?;
    out.u8(NO_RELEASE)?;
    out.u8(NO_RELEASE)?;
    out.u8(NO_RELEASE)?;
    out.u8(NO_RELEASE)?;
    out.u16(0)?;
    out.strings(&[MAKER, m.version, BIOS_DATE])?;

    /* Type 1, the system: maker, product, version and serial are strings 1
     * to 4, the UUID, how it woke, and its SKU and family, 5 and 6. */
    out.header(TYPE_SYSTEM, SYSTEM_BYTES, 1)?;
    out.u8(1)?;
    out.u8(2)?;
    out.u8(3)?;
    out.u8(4)?;
    out.put(&smbios_uuid(&m.uuid))?;
    out.u8(WAKE_POWER_SWITCH)?;
    out.u8(5)?;
    out.u8(6)?;
    out.strings(&[MAKER, PRODUCT, m.version, m.serial, PRODUCT, FAMILY])?;

    /* Type 3, its chassis: maker, version, serial and asset tag strings 1
     * to 4, no elements in it, and its SKU string 5. */
    out.header(TYPE_CHASSIS, CHASSIS_BYTES, 3)?;
    out.u8(1)?;
    out.u8(CHASSIS_OTHER)?;
    out.u8(2)?;
    out.u8(3)?;
    out.u8(4)?;
    out.u8(STATE_SAFE)?;
    out.u8(STATE_SAFE)?;
    out.u8(STATE_SAFE)?;
    out.u8(SECURITY_UNKNOWN)?;
    out.u32(0)?;
    out.u8(0)?;
    out.u8(0)?;
    out.u8(0)?;
    out.u8(0)?;
    out.u8(5)?;
    out.strings(&[MAKER, m.version, m.serial, m.serial, PRODUCT])?;

    /* Type 32, the boot: no errors. */
    out.header(TYPE_BOOT, BOOT_BYTES, 32)?;
    out.put(&[0; 6])?;
    out.u8(0)?;
    out.strings(&[])?;

    /* Type 127, the end. */
    out.header(TYPE_END, END_BYTES, 127)?;
    out.strings(&[])?;

    let len = out.at;
    let table = u32::try_from(len - start).ok()?;

    /* The entry point, its checksum making its bytes sum to zero. */
    let mut e = Out { buf: out.buf, at: 0 };
    e.put(ANCHOR)?;
    e.u8(0)?;
    e.u8(ENTRY_BYTES as u8)?;
    e.u8(VERSION_MAJOR)?;
    e.u8(VERSION_MINOR)?;
    e.u8(0)?;
    e.u8(ENTRY_REVISION)?;
    e.u8(0)?;
    e.u32(table)?;
    e.u64(structures)?;
    let sum = e.buf.get(..ENTRY_BYTES)?.iter().fold(0u8, |s, &b| s.wrapping_add(b));
    *e.buf.get_mut(5)? = 0u8.wrapping_sub(sum);
    Some(len)
}
