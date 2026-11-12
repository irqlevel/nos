//! The SMBIOS tables a PC's firmware would have left a guest (DMTF DSP0134,
//! version 3): what the machine is and who made it -- what Linux shows under
//! `/sys/class/dmi/id`, what `dmidecode` prints, and what systemd, cloud-init
//! and licensing code know a machine by: its maker, its product, and its
//! UUID and serial, which are to stay the same from one boot to the next.
//!
//! A kernel with no EFI finds them by scanning the 64 KiB from 0xF0000 for an
//! entry point on a 16-byte boundary -- SMBIOS 3's, `_SM3_`, which gives the
//! structures' address in 64 bits and which a kernel since 4.0 looks for
//! first, and after it the 32-bit one of SMBIOS 2.1, `_SM_` and its `_DMI_`
//! half, which an older kernel or `dmidecode` knows alone -- so they go
//! there, in the BIOS area the memory map reserves (`acpi::AREA`), the
//! structures right after the entry points: the BIOS (type 0, which says
//! the machine is a virtual one), the system (1: the maker, the product,
//! the version, the serial and the UUID), its chassis (3), the boot (32: no
//! errors) and the end (127).
//!
//! Everything is laid out into a buffer the caller copies into guest memory:
//! plain data, with the entry points' checksums, holding nothing of the
//! guest's. What the strings are made of is the operator's -- a VM's name --
//! so each is cut to `MAX_STRING` bytes of printable ASCII, anything else in
//! it a `?`, and an empty one a `-`: a structure's strings are told apart by
//! the zero between them, and an empty one would end its structure early.

/// Where the entry points go: the start of the 64 KiB a kernel scans.
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

/* The entry points: SMBIOS 3's -- its anchor, its length, the version the
 * tables are, and its revision -- and the 32-bit one, `_SM_`, whose second
 * half from `_DMI_` on has a checksum of its own, each on a 16-byte
 * boundary, the structures after both. */
const ANCHOR: &[u8; 5] = b"_SM3_";
const ENTRY_BYTES: usize = 0x18;
const VERSION_MAJOR: u8 = 3;
const VERSION_MINOR: u8 = 2;
const ENTRY_REVISION: u8 = 1;
const ANCHOR_32: &[u8; 4] = b"_SM_";
const ENTRY_32_BYTES: usize = 0x1F;
const ANCHOR_DMI: &[u8; 5] = b"_DMI_";
/// Where the `_DMI_` half begins in the 32-bit entry point, and its length.
const DMI_AT: usize = 0x10;
const DMI_BYTES: usize = 0x0F;
/// The 32-bit entry point's own revision, 2.1's: its formatted area unused.
const ENTRY_32_REVISION: u8 = 0;
/// The version again, as the 32-bit entry point's BCD byte says it: 3.2.
const VERSION_BCD: u8 = (VERSION_MAJOR << 4) | VERSION_MINOR;
/// Where each entry point is from `AREA`, and the structures.
const ENTRY_32_AT: usize = ENTRY_BYTES.next_multiple_of(16);
const TABLE_AT: usize = (ENTRY_32_AT + ENTRY_32_BYTES).next_multiple_of(16);
const _: () = assert!(ENTRY_32_BYTES < 32 && VERSION_MAJOR < 10 && VERSION_MINOR < 10);

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
    /* The entry points' room, what lies between them zeros whatever the
     * buffer held. */
    buf.get_mut(..TABLE_AT)?.fill(0);
    let structures = AREA + TABLE_AT as u64;
    let mut out = Out { buf, at: TABLE_AT };
    let start = out.at;
    /* The largest structure, strings and all, and how many there are: what
     * the 32-bit entry point says of them. */
    let mut sizes = Sizes { begun: start, largest: 0, count: 0 };

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
    sizes.ended(out.at);

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
    sizes.ended(out.at);

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
    sizes.ended(out.at);

    /* Type 32, the boot: no errors. */
    out.header(TYPE_BOOT, BOOT_BYTES, 32)?;
    out.put(&[0; 6])?;
    out.u8(0)?;
    out.strings(&[])?;
    sizes.ended(out.at);

    /* Type 127, the end. */
    out.header(TYPE_END, END_BYTES, 127)?;
    out.strings(&[])?;
    sizes.ended(out.at);

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
    *e.buf.get_mut(5)? = checksum(e.buf.get(..ENTRY_BYTES)?);

    /* The 32-bit one, the same table by its 32-bit address: the `_DMI_`
     * half's checksum first, then the whole's, which covers it. */
    let mut e = Out { buf: e.buf, at: ENTRY_32_AT };
    e.put(ANCHOR_32)?;
    e.u8(0)?;
    e.u8(ENTRY_32_BYTES as u8)?;
    e.u8(VERSION_MAJOR)?;
    e.u8(VERSION_MINOR)?;
    e.u16(u16::try_from(sizes.largest).ok()?)?;
    e.u8(ENTRY_32_REVISION)?;
    e.put(&[0; 5])?;
    e.put(ANCHOR_DMI)?;
    e.u8(0)?;
    e.u16(u16::try_from(table).ok()?)?;
    e.u32(u32::try_from(structures).ok()?)?;
    e.u16(sizes.count)?;
    e.u8(VERSION_BCD)?;
    let dmi = ENTRY_32_AT + DMI_AT;
    *e.buf.get_mut(dmi + ANCHOR_DMI.len())? = checksum(e.buf.get(dmi..dmi + DMI_BYTES)?);
    let whole = checksum(e.buf.get(ENTRY_32_AT..ENTRY_32_AT + ENTRY_32_BYTES)?);
    *e.buf.get_mut(ENTRY_32_AT + ANCHOR_32.len())? = whole;
    Some(len)
}

/// The byte that makes `bytes`, it among them where it is still zero, sum
/// to zero: an entry point's checksum.
fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |s, &b| s.wrapping_add(b)))
}

/// The structures' sizes as they are laid out, one after the other: the
/// largest, and how many.
struct Sizes {
    begun: usize,
    largest: usize,
    count: u16,
}

impl Sizes {
    /// A structure ended at `at`, the next one beginning there.
    fn ended(&mut self, at: usize) {
        self.largest = self.largest.max(at - self.begun);
        self.begun = at;
        self.count += 1;
    }
}
