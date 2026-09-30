//! The disk log: areas prepared and not, headers whole and damaged, on
//! disks and on the partitions of disks, of every sector size a device may
//! claim; the log's lines before the area is known and after, from tasks
//! and from interrupt handlers -- and what ends up on the disks held to
//! what was logged and to what the area's header says: the area the one
//! the documentation says is taken, its header right, its text the lines
//! in the order they came, and nothing written anywhere else.

use crate::image::disklog::{self, Header};
use crate::image::part::{self, MbrEntry};
use crate::machine::disk::{self, Media, Op};
use crate::machine::{self, cmd, sched};
use crate::{reached, Input};

/// Lines at most: well inside the log's queue of 2048, so that none is
/// dropped for want of room and the text on disk is every one of them.
const LINES: u64 = 600;

struct Area {
    /// Where on its disk the prepared device starts, and its slot.
    slot: usize,
    start: u64,
    header: Header,
}

fn sector_size(r: &mut Input) -> u64 {
    r.pick(&[512, 512, 512, 4096, 4096, 1024, 2048, 520, 48, 47, 8192])
}

/// A header, mostly one the kernel takes, now and then one it must not:
/// the CRC wrong, the sector size another device's, the area bigger than
/// the device or too small to hold anything.
fn header(r: &mut Input, ss: u64, sectors: u64) -> Vec<u8> {
    let area = match r.u8() % 6 {
        0 => sectors,
        1 => r.pick(&[0, 1, 2, 3]),
        2 => sectors + r.range(1, 3),
        _ => r.range(2, sectors.clamp(2, 64)),
    };
    let h = Header {
        magic: if r.chance(16) { r.u64() } else { disklog::MAGIC },
        version: if r.chance(16) { r.u32() } else { disklog::VERSION },
        sector_size: if r.chance(32) { r.pick(&[512, 4096, 520, 0]) } else { ss as u32 },
        area_sectors: area,
        boot_seq: r.pick(&[0, 1, 41, u64::MAX]),
        log_bytes: r.pick(&[0, 512, u64::MAX]),
    };
    let crc = if r.chance(32) { Some(r.u32()) } else { None };
    disklog::header(&h, crc)
}

pub fn disklog(r: &mut Input) {
    let mut p = machine::params();
    p.disklog = -1;
    machine::set_params(p);
    machine::keep_logged();

    /* The disks: each prepared as a whole, or on a partition, or not. */
    let n = r.range(1, 3) as usize;
    for slot in 0..n {
        let ss = sector_size(r);
        let sectors = r.range(4, 256);
        let mut m = Media::new();
        match r.u8() % 4 {
            0 => m.write(0, &header(r, ss, sectors)),
            1 if ss >= 512 && ss <= 4096 => {
                let start = r.range(1, sectors - 2);
                let size = r.range(2, sectors - start);
                let e = MbrEntry { status: 0, kind: 0x83, start: start as u32, size: size as u32 };
                let mut s0 = part::mbr(&[e, MbrEntry::default(), MbrEntry::default(), MbrEntry::default()],
                                       part::MBR_SIGNATURE);
                if r.chance(64) {
                    /* The disk's own first sector prepared too, over its
                     * partition table. */
                    s0[..disklog::HEADER].copy_from_slice(&header(r, ss, sectors));
                }
                m.write(0, &s0);
                m.write(start * ss, &header(r, ss, size));
            }
            2 => {
                let noise = crate::input::noise(r.u32(), 64);
                m.write(0, &noise);
            }
            _ => {}
        }
        let mut d = disk::Disk::new(&format!("vd{}", (b'a' + slot as u8) as char), ss, sectors, m);
        d.latency = r.bool();
        disk::insert(slot, d);
        if disk::register(slot).is_none() {
            return;
        }
    }
    block::rust_partitions_probe();

    /* Every device of the table, as a stretch of one of the disks: its
     * slot, where it starts there, how long it is. A partition's start is
     * where a read of its first sector lands. */
    let mut devs: Vec<(block::Disk, usize, u64, u64)> = Vec::new();
    for index in 0..block::count() {
        let Some(dev) = block::at(index) else { continue };
        let root = dev.parent().unwrap_or(dev);
        let Some(slot) = disk::index_of(root.handle()) else { continue };
        let start = if dev.parent().is_some() {
            let before = disk::with(slot, |d| d.log.len());
            let mut buf = vec![0u8; dev.sector_size() as usize];
            if dev.read(0, &mut buf).is_err() {
                continue;
            }
            disk::with(slot, |d| d.log[before].sector)
        } else {
            0
        };
        devs.push((dev, slot, start, dev.sectors()));
    }

    /* Something holding one of the devices -- a filesystem mounted on it --
     * which the log must pass over, and anything overlapping it. */
    let held = if r.chance(64) && !devs.is_empty() {
        let (d, slot, start, len) = devs[r.below(devs.len() as u64) as usize];
        block::claim_as(d.handle(), c"a mounted filesystem").ok().map(|c| (c, slot, start, len))
    } else {
        None
    };

    /* The area the documentation says is taken: the first device, in the
     * table's order, whose first sector is a header whole and fitting it,
     * that nothing holds. */
    let mut want: Option<Area> = None;
    for &(dev, slot, start, len) in &devs {
        let ss = dev.sector_size();
        let first = disk::with(slot, |d| d.current.bytes(start.wrapping_mul(ss), (ss as usize).min(4096)));
        let Some(h) = disklog::prepared(&first, ss, len) else { continue };
        if held.is_some_and(|(_, hs, hstart, hlen)| hs == slot && hstart < start + len && start < hstart + hlen) {
            continue;
        }
        want = Some(Area { slot, start, header: h });
        break;
    }

    /* The boot so far: lines queued before anybody knows whether they are
     * wanted. */
    for k in 0..r.below(40) {
        kcore::trace!(0, "before the command line {} {}", k, "x".repeat(r.below(200) as usize));
    }
    let on = r.u8() % 8 != 0;
    let mut p = machine::params();
    p.disklog = on as i32;
    machine::set_params(p);

    /* Until it is found, nothing may write anything; then only the area. */
    for slot in 0..n {
        disk::with(slot, |d| d.no_writes = Some("no disk log area is known".into()));
    }
    if let (true, Some(a)) = (on, &want) {
        disk::with(a.slot, |d| d.no_writes = None);
    }
    let writes_before: Vec<usize> = (0..n).map(|s| disk::with(s, |d| d.log.len())).collect();

    // SAFETY: what the boot path calls, once.
    let found = unsafe { machine::rust_disklog_setup() } == 1;
    if let (true, Some(a)) = (found && r.chance(32), &want) {
        /* A write of the log's that fails, somewhere in the run. */
        let at = r.below(16);
        disk::with(a.slot, |d| d.fail.insert(d.log.len() as u64 + at));
    }
    invariant!(found == (on && want.is_some()), "the disk log's setup {} an area, where {}", if found { "found" } else { "did not find" },
               match (&want, on) {
                   (_, false) => "disklog=on was not given".to_string(),
                   (None, true) => "no device carries one it may take".to_string(),
                   (Some(a), true) => format!("disk slot {} carries one at sector {}", a.slot, a.start),
               });

    /* The log's lines: from here, from tasks of their own, and from
     * interrupt handlers -- each a line queued for the writer, and none
     * waiting for it. */
    let mut tasks = Vec::new();
    let mut lines = 0;
    while let Some(op) = r.op(5) {
        if lines >= LINES {
            break;
        }
        match op {
            0 | 1 => {
                kcore::trace!(0, "a line {} {}", lines, "y".repeat(r.below(230) as usize));
                lines += 1;
            }
            2 => {
                let count = r.range(1, 20);
                let pad = r.below(100) as usize;
                let id = sched::spawn("logger", sched::Kind::Kernel, machine::next_cpu(), Box::new(move || {
                    for k in 0..count {
                        kcore::trace!(0, "from a task {} {}", k, "z".repeat(pad));
                        if k % 3 == 0 {
                            kcore::task::yield_to_runnable();
                        }
                    }
                }));
                tasks.push(id);
                lines += count;
            }
            3 => {
                let pad = "w".repeat(r.below(100) as usize);
                sched::interrupt(|| kcore::trace!(0, "from an interrupt {}", pad));
                lines += 1;
            }
            _ => {
                let out = cmd::run("disklog");
                invariant!(!out.is_empty(), "the disklog command said nothing");
                kcore::task::sleep_ms(r.below(30));
            }
        }
    }
    for t in tasks {
        sched::join(t);
    }
    let logged = machine::logged();
    // SAFETY: what the shutdown path calls, once.
    unsafe { machine::rust_disklog_stop() };
    if let Some((c, ..)) = held {
        block::release(c);
    }

    /* Nothing written but the area. */
    for slot in 0..n {
        let writes: Vec<_> = disk::with(slot, |d| d.log[writes_before[slot]..].iter()
            .filter(|io| matches!(io.op, Op::Write | Op::WriteFua)).copied().collect());
        let Some(a) = want.as_ref().filter(|a| found && a.slot == slot) else {
            invariant!(writes.is_empty(), "disk slot {} written with no disk log area on it: {:?}", slot, writes);
            continue;
        };
        for io in &writes {
            invariant!(io.sector >= a.start && io.sector + io.count <= a.start + a.header.area_sectors,
                       "the disk log wrote sectors {}..+{} of slot {}, outside its area at {}..+{}", io.sector,
                       io.count, slot, a.start, a.header.area_sectors);
        }
    }

    let Some(a) = want.filter(|_| found) else { return };
    reached(if a.start != 0 { "an area on a partition, written" } else { "an area on a disk, written" });
    if held.is_some() {
        reached("an area found with a device held");
    }
    let (ss, faults) = disk::with(a.slot, |d| (d.sector_size, d.log.iter().any(|io| !io.ok)));

    /* The header: the area's own, this boot's number, and the length of
     * the text that is whole sectors. */
    let hs = disk::with(a.slot, |d| d.current.bytes(a.start * ss, (ss as usize).min(4096)));
    let Some(h) = disklog::prepared(&hs, ss, a.header.area_sectors) else {
        invariant!(false, "the disk log left its area's header unreadable: {:?}", disklog::parse(&hs));
        return;
    };
    invariant!(h.sector_size == a.header.sector_size && h.area_sectors == a.header.area_sectors
               && h.boot_seq == a.header.boot_seq.wrapping_add(1),
               "the disk log's header says {:?}, where it was {:?}", h, a.header);
    let capacity = (a.header.area_sectors - 1) * ss;
    invariant!(h.log_bytes % ss == 0 && h.log_bytes <= capacity,
               "the header says {} bytes of text, in an area of {} of sectors of {}", h.log_bytes, capacity, ss);

    /* The text: every line handed to the log, in the order it came -- as
     * far as the area holds it. */
    let expect: Vec<u8> = logged.into_iter().flat_map(|(_, l)| l).collect();
    let text = disk::with(a.slot, |d| d.current.bytes((a.start + 1) * ss, capacity as usize));
    let len = h.log_bytes as usize;
    if faults {
        reached("a write of the log's failed");
        return;
    }
    invariant!(len <= expect.len() && text[..len] == expect[..len],
               "the disk log's first {} bytes are not the {} logged: {:?}", len, expect.len(),
               String::from_utf8_lossy(&text[..len.min(256)]));
    let rest = expect.len() - len;
    /* What the writer would have put down next: whole sectors of a page's
     * worth at most. */
    let batch = (4096 / ss * ss) as usize;
    let next = rest.min(batch).div_ceil(ss as usize) as u64 * ss;
    reached(match ss {
        512 => "a log on 512-byte sectors",
        4096 => "a log on 4096-byte sectors",
        _ => "a log on sectors of another size",
    });
    if rest == 0 {
        reached("the log whole sectors");
    } else if rest < ss as usize && h.log_bytes + next <= capacity {
        reached("the log's tail in a sector of its own");
        /* Not full: the tail is there, in the sector after, padded. */
        invariant!(text[len..len + rest] == expect[len..] && text[len + rest..len + ss as usize].iter().all(|b| *b == 0),
                   "the disk log's last {} bytes are not the tail logged", rest);
    } else {
        reached("the area full");
        /* Full: the next piece would not have fitted. */
        invariant!(h.log_bytes + next > capacity, "the disk log stopped at {} of {} bytes, with {} bytes of \
                   room and {} more to write", h.log_bytes, expect.len(), capacity - h.log_bytes, rest);
    }
}
