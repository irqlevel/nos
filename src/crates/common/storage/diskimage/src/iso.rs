//! An ISO 9660 image of a few files (ECMA-119, with Rock Ridge's names):
//! what a cloud gives cloud-init its NoCloud seed as -- a filesystem
//! labelled `cidata` holding `meta-data` and `user-data`, as `genisoimage
//! -volid cidata -rock` makes one -- which Linux reads with isofs alone.
//!
//! The names a file is known by are Rock Ridge's (RRIP, over SUSP): `NM`
//! gives Linux `user-data` where ISO 9660's own alphabet allows `USER_DAT.`,
//! and `PX` makes every file read-only to all. Linux turns Rock Ridge on for
//! the volume when the root's own record (`.`) has SUSP's `SP` and RRIP's
//! `ER`, which it has here.
//!
//! The image, a sector of 2 KiB at a time: sixteen of nothing (the system
//! area); the primary volume descriptor and the terminator after it; the
//! path table, little-endian and then big; the root directory, one sector;
//! each file from a sector of its own -- an empty one too, given a sector it
//! does not use; and zeros to 64 KiB, for readers that look at that much
//! first. One directory, no subdirectories: what a seed is.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// ISO 9660's logical sector, and block.
pub const SECTOR: usize = 2048;
/// The most files an image has: its directory is one sector.
pub const MAX_FILES: usize = 8;
/// The longest a file's name may be, and the most its files may hold.
pub const MAX_NAME: usize = 64;
pub const MAX_BYTES: usize = 1 << 20;
/// The longest a volume's label may be (its identifier's field).
pub const MAX_LABEL: usize = 32;

/// Where each part of the image is, by sector: the system area before them
/// all, sectors 0 to 15, zeros.
const PRIMARY: usize = 16;
const TERMINATOR: usize = 17;
const PATH_L: usize = 18;
const PATH_M: usize = 19;
const ROOT: usize = 20;
const FIRST_FILE: usize = 21;
/// The fewest sectors an image has, zeros to the end, counted in its
/// size: a reader may look at more of the start than a small one has --
/// libarchive reads 24 sectors before it calls a file ISO 9660 at all, and
/// takes one shorter than that for none.
const MIN_SECTORS: usize = 32;
/// A volume descriptor's types.
const TYPE_PRIMARY: u8 = 1;
const TYPE_TERMINATOR: u8 = 255;
const STANDARD_ID: &[u8; 5] = b"CD001";
/// The path table: one entry, the root's.
const PATH_TABLE_BYTES: usize = 10;
/// A directory record's length before its identifier.
const RECORD_FIXED: usize = 33;
const FLAG_DIRECTORY: u8 = 2;
/// Rock Ridge's file modes, links and owner: a directory anyone may list,
/// files anyone may read, owned by root.
const MODE_DIR: u32 = 0o040555;
const MODE_FILE: u32 = 0o100444;
/// SUSP's indicator, and RRIP's extension as `ER` names it (RRIP 1.10).
const SP_CHECK: [u8; 2] = [0xBE, 0xEF];
const ER_ID: &[u8] = b"RRIP_1991A";
const ER_DESCRIPTION: &[u8] =
    b"THE ROCK RIDGE INTERCHANGE PROTOCOL PROVIDES SUPPORT FOR POSIX FILE SYSTEM SEMANTICS";
const PX_BYTES: usize = 36;
const APPLICATION: &[u8] = b"NOS HV";

/// A file of the image: its name, and what it holds.
#[derive(Clone, Copy)]
pub struct File<'a> {
    pub name: &'a str,
    pub data: &'a [u8],
}

/// When an image is made, for its descriptor and its records: UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct When {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Why an image was not made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A label empty, too long, or of characters not a-z, A-Z, 0-9, _ and -.
    Label,
    /// A name empty, too long, `.` or `..`, of a character not printable
    /// ASCII or a `/`, or two files of one name.
    Name(String),
    TooMany,
    TooBig,
    NoMemory,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Label => write!(f, "a label of 1 to {} letters, digits, _ and -", MAX_LABEL),
            Error::Name(name) => write!(f, "\"{}\" is no name a file of the image can have", name),
            Error::TooMany => write!(f, "more than {} files", MAX_FILES),
            Error::TooBig => write!(f, "more than {} bytes of files", MAX_BYTES),
            Error::NoMemory => f.write_str("out of memory"),
        }
    }
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    b[at + 2..at + 4].copy_from_slice(&v.to_be_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    b[at + 4..at + 8].copy_from_slice(&v.to_be_bytes());
}

/// `text`, then spaces to the field's end.
fn padded(b: &mut [u8], at: usize, len: usize, text: &[u8]) {
    let n = text.len().min(len);
    b[at..at + n].copy_from_slice(&text[..n]);
    b[at + n..at + len].fill(b' ');
}

/// A descriptor's date: "YYYYMMDDHHMMSScc" and the offset from UTC, 0.
fn long_date(b: &mut [u8], at: usize, w: When) {
    let digits = |v: u32, n: usize, out: &mut [u8]| {
        let mut v = v;
        for i in (0..n).rev() {
            out[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
    };
    digits(u32::from(w.year), 4, &mut b[at..at + 4]);
    for (i, v) in [w.month, w.day, w.hour, w.minute, w.second].iter().enumerate() {
        digits(u32::from(*v), 2, &mut b[at + 4 + 2 * i..at + 6 + 2 * i]);
    }
    b[at + 14..at + 16].copy_from_slice(b"00");
    b[at + 16] = 0;
}

/// A date not given: zeros, and an offset of 0.
fn no_date(b: &mut [u8], at: usize) {
    b[at..at + 16].fill(b'0');
    b[at + 16] = 0;
}

/// A directory record's date: years since 1900, then month to second, UTC.
fn short_date(w: When) -> [u8; 7] {
    [w.year.saturating_sub(1900).min(255) as u8, w.month, w.day, w.hour, w.minute, w.second, 0]
}

/// A directory record: the extent, its length, flags, ISO identifier and
/// the System Use entries after it -- an even number of bytes in all.
fn record(extent: usize, len: usize, flags: u8, id: &[u8], system_use: &[u8], when: When) -> Vec<u8> {
    let pad = usize::from(id.len() % 2 == 0);
    let mut total = RECORD_FIXED + id.len() + pad + system_use.len();
    total += total % 2;
    let mut r = alloc::vec![0u8; total];
    r[0] = total as u8;
    put32(&mut r, 2, extent as u32);
    put32(&mut r, 10, len as u32);
    r[18..25].copy_from_slice(&short_date(when));
    r[25] = flags;
    put16(&mut r, 28, 1);
    r[32] = id.len() as u8;
    r[RECORD_FIXED..RECORD_FIXED + id.len()].copy_from_slice(id);
    let su = RECORD_FIXED + id.len() + pad;
    r[su..su + system_use.len()].copy_from_slice(system_use);
    r
}

/// RRIP's `PX`: a file's mode, links and owner.
fn px(mode: u32, links: u32) -> [u8; PX_BYTES] {
    let mut e = [0u8; PX_BYTES];
    e[..4].copy_from_slice(&[b'P', b'X', PX_BYTES as u8, 1]);
    put32(&mut e, 4, mode);
    put32(&mut e, 12, links);
    /* uid and gid 0, both ways. */
    e
}

/// The root's own record's entries: SUSP's `SP`, RRIP's `ER`, its `PX`.
fn root_entries() -> Vec<u8> {
    let mut e = Vec::new();
    e.extend_from_slice(&[b'S', b'P', 7, 1, SP_CHECK[0], SP_CHECK[1], 0]);
    let er_len = 8 + ER_ID.len() + ER_DESCRIPTION.len();
    e.extend_from_slice(&[b'E', b'R', er_len as u8, 1, ER_ID.len() as u8, ER_DESCRIPTION.len() as u8, 0, 1]);
    e.extend_from_slice(ER_ID);
    e.extend_from_slice(ER_DESCRIPTION);
    e.extend_from_slice(&px(MODE_DIR, 2));
    e
}

/// A file's name as ISO 9660's level 1 has one -- 8 of A-Z, 0-9 and _, a
/// dot, 3 more, and the version -- made of its real one; where that is
/// another's already, its base's last character a digit, the first free.
fn iso_name(name: &str, taken: &[Vec<u8>]) -> Vec<u8> {
    let clean = |s: &str, max: usize| -> Vec<u8> {
        s.bytes().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { b'_' }).take(max).collect()
    };
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (clean(&name[..i], 8), clean(&name[i + 1..], 3)),
        _ => (clean(name, 8), Vec::new()),
    };
    let make = |base: &[u8]| {
        let mut id = base.to_vec();
        id.push(b'.');
        id.extend_from_slice(&ext);
        id.extend_from_slice(b";1");
        id
    };
    let id = make(&base);
    if !taken.contains(&id) {
        return id;
    }
    /* Ten digits for at most MAX_FILES names: one is free. */
    let mut stem = base;
    if stem.len() == 8 {
        stem.pop();
    }
    (b'0'..=b'9')
        .map(|d| {
            let mut b = stem.clone();
            b.push(d);
            make(&b)
        })
        .find(|id| !taken.contains(id))
        .unwrap_or(id)
}

fn valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= MAX_LABEL
        && label.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name != "." && name != ".."
        && name.bytes().all(|c| c.is_ascii_graphic() && c != b'/')
}

/// The image of `files`, labelled `label`, made at `when`.
pub fn image(label: &str, files: &[File], when: When) -> Result<Vec<u8>, Error> {
    if !valid_label(label) {
        return Err(Error::Label);
    }
    if files.len() > MAX_FILES {
        return Err(Error::TooMany);
    }
    let mut bytes = 0usize;
    for (i, f) in files.iter().enumerate() {
        if !valid_name(f.name) || files[..i].iter().any(|g| g.name == f.name) {
            return Err(Error::Name(String::from(f.name)));
        }
        bytes = bytes.checked_add(f.data.len()).filter(|&b| b <= MAX_BYTES).ok_or(Error::TooBig)?;
    }

    /* Each file's ISO name, and where its data goes: a sector of its own
     * at least. The directory lists them by ISO name, as ECMA-119 wants. */
    let mut ids: Vec<Vec<u8>> = Vec::new();
    for f in files {
        let id = iso_name(f.name, &ids);
        ids.push(id);
    }
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| ids[a].cmp(&ids[b]));
    let mut extent = Vec::with_capacity(files.len());
    let mut next = FIRST_FILE;
    for f in files {
        extent.push(next);
        next += f.data.len().div_ceil(SECTOR).max(1);
    }
    let sectors = next.max(MIN_SECTORS);

    let mut img = Vec::new();
    img.try_reserve_exact(sectors * SECTOR).map_err(|_| Error::NoMemory)?;
    img.resize(sectors * SECTOR, 0);

    /* The root directory: `.`, `..`, then the files. */
    let mut dir = record(ROOT, SECTOR, FLAG_DIRECTORY, &[0], &root_entries(), when);
    dir.extend_from_slice(&record(ROOT, SECTOR, FLAG_DIRECTORY, &[1], &px(MODE_DIR, 2), when));
    for &i in &order {
        let name = files[i].name.as_bytes();
        let mut su = Vec::new();
        su.extend_from_slice(&[b'N', b'M', (5 + name.len()) as u8, 1, 0]);
        su.extend_from_slice(name);
        su.extend_from_slice(&px(MODE_FILE, 1));
        dir.extend_from_slice(&record(extent[i], files[i].data.len(), 0, &ids[i], &su, when));
    }
    /* MAX_FILES records of at most 33 + 14 + 1 + 5 + MAX_NAME + 36 + 1
     * bytes each, after `.` and `..`: well inside the one sector -- and a
     * record may not cross into the next, where the first file is. */
    if dir.len() > SECTOR {
        return Err(Error::TooMany);
    }
    img[ROOT * SECTOR..ROOT * SECTOR + dir.len()].copy_from_slice(&dir);

    for (i, f) in files.iter().enumerate() {
        img[extent[i] * SECTOR..extent[i] * SECTOR + f.data.len()].copy_from_slice(f.data);
    }

    /* The path table, both ways: the root, its own parent. */
    let l = &mut img[PATH_L * SECTOR..PATH_L * SECTOR + PATH_TABLE_BYTES];
    l[0] = 1;
    l[2..6].copy_from_slice(&(ROOT as u32).to_le_bytes());
    l[6..8].copy_from_slice(&1u16.to_le_bytes());
    let m = &mut img[PATH_M * SECTOR..PATH_M * SECTOR + PATH_TABLE_BYTES];
    m[0] = 1;
    m[2..6].copy_from_slice(&(ROOT as u32).to_be_bytes());
    m[6..8].copy_from_slice(&1u16.to_be_bytes());

    let pvd = &mut img[PRIMARY * SECTOR..(PRIMARY + 1) * SECTOR];
    pvd[0] = TYPE_PRIMARY;
    pvd[1..6].copy_from_slice(STANDARD_ID);
    pvd[6] = 1;
    padded(pvd, 8, 32, b"");
    padded(pvd, 40, 32, label.as_bytes());
    put32(pvd, 80, sectors as u32);
    put16(pvd, 120, 1);
    put16(pvd, 124, 1);
    put16(pvd, 128, SECTOR as u16);
    put32(pvd, 132, PATH_TABLE_BYTES as u32);
    pvd[140..144].copy_from_slice(&(PATH_L as u32).to_le_bytes());
    pvd[148..152].copy_from_slice(&(PATH_M as u32).to_be_bytes());
    let root = record(ROOT, SECTOR, FLAG_DIRECTORY, &[0], &[], when);
    pvd[156..156 + root.len()].copy_from_slice(&root);
    padded(pvd, 190, 128, b"");
    padded(pvd, 318, 128, b"");
    padded(pvd, 446, 128, b"");
    padded(pvd, 574, 128, APPLICATION);
    padded(pvd, 702, 37, b"");
    padded(pvd, 739, 37, b"");
    padded(pvd, 776, 37, b"");
    long_date(pvd, 813, when);
    long_date(pvd, 830, when);
    no_date(pvd, 847);
    no_date(pvd, 864);
    pvd[881] = 1;

    let end = &mut img[TERMINATOR * SECTOR..(TERMINATOR + 1) * SECTOR];
    end[0] = TYPE_TERMINATOR;
    end[1..6].copy_from_slice(STANDARD_ID);
    end[6] = 1;
    Ok(img)
}
