//! The `smbios` target: the tables a guest is told what it is by
//! (`hv/src/smbios.rs`), made of what an operator says -- a VM's name, any
//! UUID -- and read back as Linux's `dmi_scan` reads them: the entry point
//! found where the scan looks, its checksum, the structures walked to their
//! end by their lengths and their strings, every string a structure names
//! there, printable and no longer than the most, and what the system says
//! held to a model of its own: the UUID, its first three fields least
//! significant byte first, and the serial as the name made printable.

use crate::smbios::{build, Machine, AREA, MAX_BYTES, MAX_STRING};
use crate::targets::Input;

const CHARS: [char; 12] = ['a', 'Z', '0', '-', '.', '_', ' ', '\u{0}', '\u{7f}', '\n', 'é', '日'];

fn text(r: &mut Input, max: u64) -> String {
    (0..r.below(max)).map(|_| r.pick(&CHARS)).collect()
}

/// A string as the tables must say it: its first `MAX_STRING` bytes,
/// anything but printable ASCII a `?`, and `-` for none.
fn said(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return b"-".to_vec();
    }
    s.bytes().take(MAX_STRING).map(|b| if (0x20..0x7F).contains(&b) { b } else { b'?' }).collect()
}

/// The structures, walked as Linux's `dmi_decode_table` walks them: (type,
/// formatted part, strings), up to and with the end.
fn walk(table: &[u8]) -> Vec<(u8, Vec<u8>, Vec<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    loop {
        invariant!(at + 4 <= table.len(), "a structure's header past the table's {} bytes", table.len());
        let kind = table[at];
        let len = usize::from(table[at + 1]);
        invariant!(len >= 4 && at + len <= table.len(), "structure {} of {} bytes at {}, past the table", kind, len, at);
        let formatted = table[at..at + len].to_vec();
        let mut s = at + len;
        let mut strings = Vec::new();
        /* Strings until an empty one; none at all is two zeros. */
        loop {
            let end = table[s..].iter().position(|&b| b == 0).map(|n| s + n);
            invariant!(end.is_some(), "structure {}'s strings run past the table", kind);
            let end = end.unwrap_or(s);
            if end == s {
                if strings.is_empty() {
                    invariant!(table.get(s + 1) == Some(&0), "structure {} with no strings and one zero", kind);
                    s += 2;
                } else {
                    s += 1;
                }
                break;
            }
            strings.push(table[s..end].to_vec());
            s = end + 1;
        }
        out.push((kind, formatted, strings));
        at = s;
        if kind == 127 {
            invariant!(at == table.len(), "{} bytes past the end-of-table structure", table.len() - at);
            return out;
        }
    }
}

pub fn smbios_target(r: &mut Input) {
    let mut uuid = [0u8; 16];
    for b in uuid.iter_mut() {
        *b = r.u8();
    }
    let serial = if r.u8() < 16 { "x".repeat(r.below(200) as usize) } else { text(r, 80) };
    let version = text(r, 40);
    let room = if r.u8() < 32 { r.below(MAX_BYTES as u64 + 1) as usize } else { MAX_BYTES };
    let mut buf = vec![0u8; room];
    let machine = Machine { uuid, serial: &serial, version: &version };
    let Some(len) = build(&machine, &mut buf) else {
        invariant!(room < MAX_BYTES, "tables that do not fit {} bytes", MAX_BYTES);
        return;
    };
    invariant!(len <= room, "{} bytes laid out in {}", len, room);
    let b = &buf[..len];

    /* The entry point: SMBIOS 3's, its checksum, the table where it says. */
    invariant!(&b[..5] == b"_SM3_", "no SMBIOS 3 entry point at the start");
    let entry_len = usize::from(b[6]);
    invariant!((24..=32).contains(&entry_len), "an entry point of {} bytes", entry_len);
    invariant!(b[..entry_len].iter().fold(0u8, |s, &x| s.wrapping_add(x)) == 0, "the entry point's checksum");
    invariant!(b[7] == 3, "SMBIOS {}, not 3", b[7]);
    let table_len = u32::from_le_bytes([b[12], b[13], b[14], b[15]]) as usize;
    let table_at = u64::from_le_bytes([b[16], b[17], b[18], b[19], b[20], b[21], b[22], b[23]]);
    invariant!(table_at >= AREA + entry_len as u64 && table_at % 16 == 0, "the table at {:#x}", table_at);
    let start = (table_at - AREA) as usize;
    invariant!(start + table_len == len, "a table of {} bytes at {}, the tables {}", table_len, start, len);

    let structures = walk(&b[start..]);
    let kinds: Vec<u8> = structures.iter().map(|s| s.0).collect();
    invariant!(kinds == [0, 1, 3, 32, 127], "structures {:?}", kinds);
    for (kind, formatted, strings) in &structures {
        for s in strings {
            invariant!(!s.is_empty() && s.len() <= MAX_STRING && s.iter().all(|&c| (0x20..0x7F).contains(&c)),
                       "structure {}'s string {:?}", kind, String::from_utf8_lossy(s));
        }
        /* Every string a structure names by its number is there. */
        let refs: &[usize] = match kind {
            0 => &[4, 5, 8],
            1 => &[4, 5, 6, 7, 0x19, 0x1A],
            3 => &[4, 6, 7, 8, 0x15],
            _ => &[],
        };
        for &at in refs {
            let n = usize::from(formatted[at]);
            invariant!(n >= 1 && n <= strings.len(), "structure {} names string {} of {}", kind, n, strings.len());
        }
    }

    /* The system: its UUID as SMBIOS stores one, its serial the name. */
    let (_, system, strings) = &structures[1];
    let mut want = uuid;
    want[0..4].reverse();
    want[4..6].reverse();
    want[6..8].reverse();
    invariant!(system[8..24] == want, "the UUID stored as {:02x?}, not {:02x?}", &system[8..24], want);
    let serial_at = usize::from(system[7]);
    invariant!(strings[serial_at - 1] == said(&serial), "the serial {:?} for {:?}",
               String::from_utf8_lossy(&strings[serial_at - 1]), serial);
    let version_at = usize::from(system[6]);
    invariant!(strings[version_at - 1] == said(&version), "the version {:?} for {:?}",
               String::from_utf8_lossy(&strings[version_at - 1]), version);
    /* The BIOS says it is a virtual machine. */
    let (_, bios, _) = &structures[0];
    invariant!(bios[0x13] & (1 << 4) != 0, "the BIOS does not say the machine is virtual");
}
