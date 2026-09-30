//! The partition tables: MBRs and GPTs of every shape, on disks of every
//! geometry a device may claim, read by the probe and held to what it
//! documents -- which slots become partitions, where, under what name;
//! nothing written while it reads -- then I/O through the devices it
//! registered, held to their bounds, and claims taken and given back, held
//! to what they are for: never two writers on the same sectors.

use crate::image::part::{self, Found, GptEntry, GptHeader, MbrEntry};
use crate::machine::disk::{self, Media, Op};
use crate::machine::cmd;
use crate::{reached, Input};

extern "C" {
    fn rust_block_selftest() -> i32;
}

/// The block layer's table holds this many devices.
const TABLE: usize = 48;
/// A partition's name, its NUL included, fits this.
const NAME_MAX: usize = 16;

/// A device of the table, as the target knows it.
#[derive(Clone, Debug)]
struct Dev {
    handle: usize,
    name: String,
    sector_size: u64,
    sectors: u64,
    /// The disk it is on and where, for a partition; for a disk itself its
    /// own handle and 0.
    root: usize,
    start: u64,
    /// The RAM disk's slot it is on.
    slot: usize,
}

struct World {
    devs: Vec<Dev>,
    /// Held claims: the claim, and the device.
    claims: Vec<(usize, usize)>,
    /// Given back: what a stale release is made of.
    released: Vec<usize>,
    next_slot: usize,
}

const NAMES: &[&str] = &["vda", "sda", "nvme0n", "hd", "mmcblk0p", "x", "nvme01234567890"];

fn sector_size(r: &mut Input) -> u64 {
    r.pick(&[512, 512, 512, 512, 4096, 4096, 1024, 2048, 520, 256, 8192, 0, 4097])
}

fn sectors(r: &mut Input) -> u64 {
    match r.u8() % 8 {
        0..=3 => r.range(1, 4096),
        4 => r.range(4096, 1 << 22),
        5 => u32::MAX as u64 + r.below(1 << 16),
        6 => u64::MAX - r.below(4),
        _ => r.pick(&[1, 2, 3, 34, 35, 2048, 2049, 1 << 32]),
    }
}

/// A sector number: an edge of the disk's, or anything.
fn lba(r: &mut Input, sectors: u64) -> u64 {
    match r.u8() % 8 {
        0 => 0,
        1 => 1,
        2 => sectors.saturating_sub(r.below(3)),
        3 => sectors.saturating_add(r.below(3)),
        4 => u64::MAX - r.below(3),
        5 => r.value32() as u64,
        6 => r.below(sectors.max(1)),
        _ => r.pick(&[2, 34, 2048, 1 << 32, 1 << 63]),
    }
}

fn guid(r: &mut Input) -> [u8; 16] {
    let mut g = [0u8; 16];
    if r.u8() % 4 != 0 {
        g[0] = r.u8() | 1;
        g[15] = r.u8();
    }
    g
}

/// A disk's first sectors: an MBR, maybe the protective one and a GPT
/// behind it, maybe nothing; then a few bytes of it changed.
fn media(r: &mut Input, ss: u64, sectors: u64) -> Media {
    let mut m = Media::new();
    let ssz = (ss as usize).clamp(512, 4096);
    let gpt = r.u8() % 3 == 0;
    let mut entries = [MbrEntry::default(); 4];
    for (i, e) in entries.iter_mut().enumerate() {
        if r.u8() % 3 == 0 && !(gpt && i == 0) {
            continue;
        }
        e.status = r.pick(&[0, 0x80, 0x7F]);
        e.kind = if gpt && i == 0 { part::TYPE_GPT_PROTECTIVE } else { r.pick(&[0x83, 0x83, 0x0C, 0x05, 0xEE, 0, 0xFF]) };
        e.start = lba(r, sectors).min(u32::MAX as u64) as u32;
        e.size = match r.u8() % 4 {
            0 => sectors.saturating_sub(e.start as u64).min(u32::MAX as u64) as u32,
            1 => r.below(64) as u32,
            _ => r.value32(),
        };
    }
    let signature = if r.chance(16) { r.u16() } else { part::MBR_SIGNATURE };
    let mut s0 = part::mbr(&entries, signature);
    s0.resize(ssz, 0);
    m.write(0, &s0);
    if gpt {
        /* Mostly a table the probe takes, now and then a field of it
         * wrong. */
        let sane = r.u8() % 4 != 0;
        let any = r.u16() as u32;
        let entry_size = if sane { 128 } else { r.pick(&[128u32, 256, ssz as u32, 127, 0, ssz as u32 + 128, any]) };
        let any = r.u16() as u32;
        let h = GptHeader {
            signature: if !sane && r.chance(64) { r.u64() } else { part::GPT_SIGNATURE },
            header_size: if sane { 92 } else { r.pick(&[92u32, 128, ssz as u32, 91, ssz as u32 + 1, 0, any]) },
            crc: if !sane && r.chance(64) { Some(r.u32()) } else { None },
            my_lba: 1,
            alt_lba: sectors.saturating_sub(1),
            first_usable: 34,
            last_usable: sectors.saturating_sub(34),
            entry_lba: match r.u8() % 8 {
                0..=4 => 2,
                5 => lba(r, sectors),
                6 => u64::MAX - r.below(8),
                _ => 1,
            },
            entries: r.pick(&[1u32, 2, 4, 8, 8, 9, 16, 128, 128, 0, u32::MAX]),
            entry_size,
        };
        m.write(ss.max(1), &part::gpt_header(&h, ssz));
        let es = (entry_size as usize).clamp(128, 4096);
        if h.entry_lba < 1 << 40 {
            let mut table = Vec::new();
            let mut next = 34u64;
            for _ in 0..(r.range(1, 10) as usize) {
                let (first, last) = if r.u8() % 4 != 0 {
                    /* One after another, as a partitioning tool lays them
                     * out -- the last of them maybe past the disk's end. */
                    let first = next + r.below(64);
                    let last = first + r.below(sectors.clamp(1, 1 << 20));
                    next = last.saturating_add(1);
                    (first, last)
                } else {
                    let first = lba(r, sectors);
                    let last = match r.u8() % 4 {
                        0 => first.saturating_add(r.below(4096)),
                        1 => first.wrapping_sub(1),
                        2 => lba(r, sectors),
                        _ => u64::MAX,
                    };
                    (first, last)
                };
                table.extend_from_slice(&part::gpt_entry(&GptEntry { kind: guid(r), first, last }, es));
            }
            m.write(h.entry_lba.wrapping_mul(ss.max(1)), &table);
        }
    }
    for _ in 0..r.below(4) {
        let at = r.below(3 * ssz as u64);
        let b = r.u8();
        m.write(at, &[b]);
    }
    m
}

impl World {
    /// Makes, registers and looks at one more disk: the partitions the
    /// probe registers held to what its table says.
    fn add_disk(&mut self, r: &mut Input) {
        if self.next_slot >= disk::DISKS || self.devs.len() >= TABLE {
            return;
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        let ss = sector_size(r);
        let sectors = sectors(r);
        let name = format!("{}{}", r.pick(NAMES), (b'a' + slot as u8) as char);
        let media = media(r, ss, sectors);
        let mut d = disk::Disk::new(&name, ss, sectors, media.clone());
        d.seed_chaos(r.u64());
        disk::insert(slot, d);
        let Some(dev) = disk::register(slot) else {
            invariant!(false, "disk {} was not registered with {} devices in the table", name, self.devs.len());
            return;
        };
        self.devs.push(Dev { handle: dev.handle(), name: name.clone(), sector_size: ss, sectors, root: dev.handle(),
                             start: 0, slot });

        /* The probe reads; nothing it does may write. */
        for s in 0..self.next_slot {
            disk::with(s, |d| d.no_writes = Some("its partition table is being read".into()));
        }
        let before = block::count();
        block::rust_partitions_probe();
        for s in 0..self.next_slot {
            disk::with(s, |d| d.no_writes = None);
        }

        /* What it should have found: the table's slots as the probe
         * documents them, each a partition while the table has room and
         * its name fits. */
        let mut want: Vec<(String, Found)> = Vec::new();
        let mut room = TABLE - before as usize;
        for f in part::expect(&media, ss, sectors) {
            let pname = format!("{}{}", name, f.slot + 1);
            if pname.len() >= NAME_MAX || room == 0 {
                continue;
            }
            room -= 1;
            want.push((pname, f));
        }
        if !want.is_empty() {
            let gpt = (0..4).any(|i| media.bytes(446 + 16 * i + 4, 1)[0] == part::TYPE_GPT_PROTECTIVE);
            reached(if gpt { "a GPT's partitions registered" } else { "an MBR's partitions registered" });
            if want.len() >= 5 {
                reached("five partitions or more on a disk");
            }
        }
        let got: Vec<block::Disk> = (before..block::count()).filter_map(block::at).collect();
        let names: Vec<(String, u64)> = got.iter().map(|d| (d.name().to_string(), d.sectors())).collect();
        let wanted: Vec<(String, u64)> = want.iter().map(|(n, f)| (n.clone(), f.count)).collect();
        invariant!(names == wanted, "disk {} ({} sectors of {}): the probe registered {:?}, and the table says {:?}",
                   name, sectors, ss, names, wanted);
        for (d, (_, f)) in got.iter().zip(&want) {
            invariant!(d.parent().map(|p| p.handle()) == Some(dev.handle()),
                       "partition {} is not on the disk it was found on", d.name());
            /* Where it starts: what its first sector's read asks the disk
             * for. */
            let mut buf = vec![0u8; ss as usize];
            let ok = d.read(0, &mut buf).is_ok();
            let asked = disk::with(slot, |x| x.log.last().copied());
            invariant!(ok && asked.is_some_and(|io| io.op == Op::Read && io.sector == f.start),
                       "partition {}'s sector 0 read as {:?} ({}), where it starts at {}", d.name(), asked, ok,
                       f.start);
            self.devs.push(Dev { handle: d.handle(), name: d.name().to_string(), sector_size: ss, sectors: f.count,
                                 root: dev.handle(), start: f.start, slot });
        }
    }

    fn pick(&self, r: &mut Input) -> Option<Dev> {
        if self.devs.is_empty() {
            return None;
        }
        Some(self.devs[r.below(self.devs.len() as u64) as usize].clone())
    }

    /// A read or a write through a device: done exactly when it is whole
    /// sectors inside the device, and asked of its disk where the device
    /// is on it.
    fn io(&mut self, r: &mut Input) {
        let Some(d) = self.pick(r) else { return };
        let dev = block::Disk::from_handle(d.handle).expect("a device of the table");
        let sector = lba(r, d.sectors);
        let n = r.below(5);
        if d.sector_size == 0 || d.sector_size > 16384 {
            return;
        }
        let len = (n * d.sector_size) as usize;
        let write = r.bool();
        let fua = r.bool();
        let asked_before = disk::with(d.slot, |x| x.log.len());
        let ok = if write {
            let data = crate::input::noise(r.u32(), len);
            dev.write(sector, &data, fua).is_ok()
        } else {
            let mut buf = vec![0u8; len];
            dev.read(sector, &mut buf).is_ok()
        };
        let inside = n != 0 && sector.checked_add(n).is_some_and(|end| end <= d.sectors);
        reached(match (inside, d.root == d.handle) {
            (true, true) => "I/O inside a disk",
            (true, false) => "I/O inside a partition",
            (false, true) => "I/O past a disk's end, refused",
            (false, false) => "I/O past a partition's end, refused",
        });
        invariant!(ok == inside, "a {} of {} sectors at {} of {} ({} sectors of {}) {}", if write { "write" } else { "read" },
                   n, sector, d.name, d.sectors, d.sector_size, if ok { "was done" } else { "was refused" });
        if ok {
            let asked = disk::with(d.slot, |x| x.log[asked_before..].to_vec());
            invariant!(asked.len() == 1 && asked[0].sector == d.start + sector && asked[0].count == n,
                       "a request for {} sectors at {} of {} reached its disk as {:?}", n, sector, d.name, asked);
        }
    }

    /// A batch through a device: done exactly when every piece is one --
    /// a page of the buffer or the start of one, whole sectors, inside the
    /// device.
    fn pieces(&mut self, r: &mut Input) {
        let Some(d) = self.pick(r) else { return };
        let dev = block::Disk::from_handle(d.handle).expect("a device of the table");
        let pages = r.range(1, 3) as usize;
        let Some(mut buf) = kcore::dma::DmaBuffer::new(pages) else { return };
        let mut pieces = Vec::new();
        for _ in 0..r.range(1, 4) {
            let at = match r.u8() % 4 {
                0 => r.below(pages as u64 + 1) as usize * 4096,
                1 => r.below(pages as u64 * 4096) as usize,
                _ => r.below(pages as u64) as usize * 4096,
            };
            let len = match r.u8() % 4 {
                0 => r.below(8193) as usize,
                _ => (d.sector_size as usize).saturating_mul(r.range(1, 8) as usize),
            };
            pieces.push(block::Piece { sector: lba(r, d.sectors), at, len });
        }
        let ss = d.sector_size as usize;
        let valid = |p: &block::Piece| {
            ss != 0 && p.len != 0 && p.len % ss == 0 && p.at % 4096 == 0 && p.len <= 4096
                && p.at.checked_add(p.len).is_some_and(|e| e <= buf.len())
                && p.sector.checked_add((p.len / ss) as u64).is_some_and(|e| e <= d.sectors)
        };
        let expect = pieces.iter().all(valid);
        reached(if expect { "a batch done" } else { "a batch refused" });
        let ok = if r.bool() { dev.write_pieces(&buf, &pieces).is_ok() } else { dev.read_pieces(&mut buf, &pieces).is_ok() };
        invariant!(ok == expect, "a batch {:?} on {} ({} sectors of {}) {}", pieces, d.name, d.sectors, d.sector_size,
                   if ok { "was done" } else { "was refused" });
    }

    /// A claim: taken exactly when nothing held overlaps it on its disk.
    fn claim(&mut self, r: &mut Input) {
        let Some(d) = self.pick(r) else { return };
        let holder: &'static core::ffi::CStr = r.pick(&[c"a mounted filesystem", c"the disk log", c"diskwrite"]);
        let got = block::claim_as(d.handle, holder);
        let overlapping: Vec<String> = self.claims.iter().filter_map(|&(_, h)| {
            let o = self.devs.iter().find(|x| x.handle == h)?;
            let meet = o.root == d.root && o.start < d.start.saturating_add(d.sectors)
                && d.start < o.start.saturating_add(o.sectors);
            meet.then(|| o.name.clone())
        }).collect();
        match got {
            Ok(c) => {
                invariant!(overlapping.is_empty(), "{} (sectors {}..+{} of disk {}) claimed while {:?} holds sectors it \
                           has: two writers on the same sectors", d.name, d.start, d.sectors, d.root, overlapping);
                invariant!(c != 0, "a claim of 0 given out");
                self.claims.push((c, d.handle));
            }
            Err(_) => {
                reached(if self.claims.iter().any(|&(_, h)| {
                    self.devs.iter().any(|o| o.handle == h && o.root != o.handle) && d.root != d.handle
                }) { "a partition's claim refused over another's" } else { "a claim refused" });
                invariant!(!overlapping.is_empty(), "{} refused a claim with nothing held on its sectors ({:?})",
                           d.name, self.claims);
            }
        }
    }

    fn release(&mut self, r: &mut Input) {
        if r.chance(64) && !self.released.is_empty() {
            /* Given back again: nothing, whatever took the slot since. */
            let c = self.released[r.below(self.released.len() as u64) as usize];
            block::release(c);
            return;
        }
        if self.claims.is_empty() {
            return;
        }
        let i = r.below(self.claims.len() as u64) as usize;
        let (c, _) = self.claims.remove(i);
        block::release(c);
        self.released.push(c);
    }

    fn command(&mut self, r: &mut Input) {
        let name = match self.pick(r) {
            Some(d) if r.u8() % 8 != 0 => d.name,
            _ => r.pick(&["", "nonexistent", "vda1", "x"]).to_string(),
        };
        let sector = match r.u8() % 4 {
            0 => "0".to_string(),
            1 => u64::MAX.to_string(),
            2 => "-1".to_string(),
            _ => r.u32().to_string(),
        };
        let line = match r.u8() % 5 {
            0 => format!("partitions {}", name),
            1 => "disks".to_string(),
            2 => format!("diskread {} {}", name, sector),
            3 => format!("diskwrite {} {} {}", name, sector, r.pick(&["00", "deadbeef", "x", "0", ""])),
            _ => "disklog".to_string(),
        };
        let _ = cmd::run(&line);
    }
}

pub fn part(r: &mut Input) {
    let mut w = World { devs: Vec::new(), claims: Vec::new(), released: Vec::new(), next_slot: 0 };
    for _ in 0..r.range(1, 3) {
        w.add_disk(r);
    }
    while let Some(op) = r.op(9) {
        match op {
            0 | 1 => w.io(r),
            2 => w.pieces(r),
            3 => w.claim(r),
            4 => w.release(r),
            5 => w.command(r),
            6 => w.add_disk(r),
            7 => {
                /* Again: every disk has been looked at, and nothing is new. */
                let before = block::count();
                block::rust_partitions_probe();
                invariant!(block::count() == before, "a second probe registered {} more devices",
                           block::count() - before);
            }
            _ => {
                /* Lookups by name: each device answers to its own. */
                if let Some(d) = w.pick(r) {
                    let found = block::Disk::open(&d.name).map(|x| x.handle());
                    let first = w.devs.iter().find(|x| x.name == d.name).map(|x| x.handle);
                    invariant!(found == first, "{} looked up as {:?}, registered as {:?}", d.name, found, first);
                }
            }
        }
    }
    // SAFETY: the self-test takes nothing, and reads the table.
    invariant!(unsafe { rust_block_selftest() } == 0, "the block layer's self-test failed");
    for (c, _) in std::mem::take(&mut w.claims) {
        block::release(c);
    }
}
