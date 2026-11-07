#!/usr/bin/env python3
"""The nbs fuzzer's mutants: one rule of NBS's metadata
(src/crates/common/storage/nbsmeta), of its chunk server's engine
(src/crates/common/storage/nbscs) or of their wire (nbswire) taken out at a
time, and the gate
(`nbs-fuzz gate`) must fail -- a rule the gate does not catch the loss of is
one it does not hold the crate to.

    python3 fuzz/nbs/mutants.py [name...]

Each file is restored from what it was before, whatever happens, and held to
it by its digest. Exit code 0 = every mutant caught."""

import hashlib
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SRC = os.path.join(ROOT, 'src', 'crates', 'common', 'storage')
OUT = os.path.join(ROOT, 'out', 'nbs-mutants')
BINARY = os.path.join(OUT, 'release', 'nbs-fuzz')
VENDOR = os.path.join(ROOT, 'src', 'crates', 'vendor')

# name, what it takes out, the file, the text it changes, what it becomes
MUTANTS = [
    ('home_first', "a replica on the volume's own host",
     'nbsmeta/src/place.rs',
     'if self.candidates[h].free > 0 {',
     'if false && self.candidates[h].free > 0 {'),
    ('most_room', 'the hosts with the most room taken first',
     'nbsmeta/src/place.rs',
     'let key = (c.free, tie(volume, index, c.cs));',
     'let key = (0, tie(volume, index, c.cs));'),
    ('rebuild_room', 'room kept to rebuild the biggest host',
     'nbsmeta/src/place.rs',
     'others >= self.candidates[biggest].reserved',
     'true'),
    ('too_few_is_shape', 'three full hosts no room rather than too few hosts',
     'nbsmeta/src/place.rs',
     '        self.candidates.len()\n',
     '        self.candidates.iter().filter(|c| c.free > 0).count()\n'),
    ('room_given_back', "a freed chunk's slots given back",
     'nbsmeta/src/storage.rs',
     '                c.reserved -= 1;',
     '                c.reserved -= 0;'),
    ('whole_or_none', 'a volume that does not fit refused whole',
     'nbsmeta/src/storage.rs',
     'trial.pick(volume, from + k).ok_or(Refusal::NoRoom)?;',
     'if trial.pick(volume, from + k).is_none() { break; }'),
    ('force_fences', 'a forced attach a new generation',
     'nbsmeta/src/storage.rs',
     '            v.generation += 1;',
     '            v.generation += u64::from(!force);'),
    ('device_elsewhere', "a device another host's refused",
     'nbsmeta/src/storage.rs',
     '(false, _) => Err(Refusal::DeviceElsewhere),',
     '(false, _) => Ok(d.id),'),
    ('name_taken', 'a volume made again with another size refused',
     'nbsmeta/src/storage.rs',
     'if v.kind == Kind::Volume && v.size_mib == size_mib { Ok(v.id) } else { Err(Refusal::NameTaken) }',
     'Ok(v.id)'),
    ('delete_attached', 'an attached volume kept',
     'nbsmeta/src/storage.rs',
     'if v.writer.is_some() || !v.readers.is_empty() {\n                return Err(Refusal::Attached);',
     'if false {\n                return Err(Refusal::Attached);'),
    ('divergence_said', "a digest not the member's own said",
     'nbsmeta/src/snapshot.rs',
     '            Some((_, mine)) => {\n                if self.diverged.is_none() {',
     '            Some((_, mine)) => {\n                if false && self.diverged.is_none() {'),
    ('free_list_checked', "a snapshot's free list held to its chunks",
     'nbsmeta/src/snapshot.rs',
     'if walked != free {',
     'if false && walked != free {'),
    ('reserved_checked', "a snapshot's reserved held to its replicas",
     'nbsmeta/src/snapshot.rs',
     'if replicas != c.reserved {',
     'if false && replicas != c.reserved {'),
    ('chunk_count_checked', "a snapshot's chunks held to the maps that name them",
     'nbsmeta/src/snapshot.rs',
     'if ch.count == 0 || ch.count != n || ch.flags & !CHUNK_WRITTEN != 0 {',
     'if ch.count == 0 || ch.flags & !CHUNK_WRITTEN != 0 {'),
    ('digest_streamed', 'a snapshot written in pieces as in one',
     'nbsmeta/src/snapshot.rs',
     '        self.sent = 0;\n        let s = &self.state;',
     '        let s = &self.state;'),
    ('incarnation_bumped', "a chunk's number taken again at a new incarnation",
     'nbsmeta/src/storage.rs',
     'let incarnation = ch.incarnation.wrapping_add(1).max(1);',
     'let incarnation = ch.incarnation.max(1);'),
    ('incarnation_checked', "a snapshot's chunks held to an incarnation",
     'nbsmeta/src/snapshot.rs',
     '            if ch.incarnation == 0 {\n                return Err(RestoreError(',
     '            if false {\n                return Err(RestoreError('),
    ('counters_checked', "a snapshot's counters held to what applying leaves them at",
     'nbsmeta/src/snapshot.rs',
     'if self.next_device == 0 || self.next_volume == 0 || self.next_check < crate::CHECK_EVERY {',
     'if false {'),
    # The cs's engine
    ('cs_zero_flushed', "a slot's zeros durable before the record that takes it",
     'nbscs/src/store.rs',
     'd.zero(base, SLOT).and_then(|_| self.devs[dv].d.flush()).is_err()',
     'd.zero(base, SLOT).is_err()'),
    ('cs_zeroed', 'a slot zeroed before a chunk takes it',
     'nbscs/src/store.rs',
     'd.zero(base, SLOT).and_then(|_| self.devs[dv].d.flush()).is_err()',
     'd.flush().is_err()'),
    ('cs_open_mark', "an open mark durable before a chunk's first write of a generation",
     'nbscs/src/store.rs',
     'seq: r.seq + 1, ..r };\n                    self.put(dv, slot, open)?;',
     'seq: r.seq + 1, ..r };\n                    if let Some(x) = self.devs[dv].records.get_mut(slot as usize) { *x = open; }'),
    ('cs_copy_order', "a record's copy B written only once copy A is flushed",
     'nbscs/src/store.rs',
     'if write_table_page(dev, p, copy).is_err() || dev.d.flush().is_err() {',
     'if write_table_page(dev, p, copy).is_err() || (copy == 1 && dev.d.flush().is_err()) {'),
    ('cs_fence_flushed', 'a fence durable before it is answered',
     'nbscs/src/store.rs',
     'if write_fence_page(dev, &self.fences, p, copy).is_ok() && dev.d.flush().is_ok() {',
     'if write_fence_page(dev, &self.fences, p, copy).is_ok() {'),
    ('cs_close_flushes', "a close durable only after the chunk's data",
     'nbscs/src/store.rs',
     '        if dev.dirty {\n            if dev.d.flush().is_err() {',
     '        if false && dev.dirty {\n            if dev.d.flush().is_err() {'),
    ('cs_old_incarnation', "a slot of the chunk a number was never taken for the one it is",
     'nbscs/src/store.rs',
     '        if r.incarnation < c.incarnation {\n            /* The chunk its number was: to be freed */\n            return Ok(None);\n        }',
     ''),
    ('cs_stale_refused', "a request of the chunk a number was refused",
     'nbscs/src/store.rs',
     '        if r.incarnation > c.incarnation {\n            return Err(Error::StaleChunk { incarnation: r.incarnation });\n        }',
     ''),
    ('cs_fence_held', 'a request of an older generation than the fence refused',
     'nbscs/src/store.rs',
     '        let cur = self.fence_of(volume);\n        if cur > generation {\n            return Err(Error::Fenced { generation: cur });\n        }\n        if cur < generation {',
     '        let cur = self.fence_of(volume);\n        if cur < generation {'),
    ('cs_record_crc', "a record whose CRC fails taken as no record",
     'nbscs/src/format.rs',
     'if crc_of(format, &b[..RECORD_CRC]) != le32(b, RECORD_CRC) || b[9..12] != [0, 0, 0]',
     'if b[9..12] != [0, 0, 0]'),
    ('cs_first_write_torn', "a record's first write torn in copy A taken as never written",
     'nbscs/src/format.rs',
     '        (Decoded::Bad, Decoded::Empty) => Some(Decoded::Empty),',
     ''),
    ('cs_fence_pages_repaired', "a fence page of which one copy is torn written again at the open",
     'nbscs/src/store.rs',
     'let this = fences_stale || fences != self.fences;',
     'let this = fences != self.fences;'),
    ('wire_whole', 'a request read whole, nothing past its end',
     'nbswire/src/lib.rs',
     'r.done().then_some(Request { id, volume, generation, op })',
     'Some(Request { id, volume, generation, op })'),
    ('wire_flags', 'a flag read as 0 or 1 alone',
     'nbswire/src/lib.rs',
     '            4 => Reply::Closed(match r.u8()? {\n                0 => false,\n                1 => true,\n                _ => return None,\n            }),',
     '            4 => Reply::Closed(r.u8()? != 0),'),
    ('cs_slot_given_back', 'a freed slot given back to the free list',
     'nbscs/src/store.rs',
     '        self.devs[dv].free.push(slot);\n        Ok(())',
     '        Ok(())'),
]


def digest(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


def build():
    return subprocess.run(['cargo', 'build', '--release', '--quiet', '--offline',
                           '--manifest-path', os.path.join(ROOT, 'fuzz', 'nbs', 'Cargo.toml'),
                           '--target-dir', OUT,
                           '--config', 'source.crates-io.replace-with="vendored-sources"',
                           '--config', 'source.vendored-sources.directory="%s"' % VENDOR], cwd=ROOT,
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE).returncode


def run(*args):
    """A run's exit code and its first failing line."""
    p = subprocess.run([BINARY] + list(args), cwd=ROOT, capture_output=True, text=True, timeout=900)
    first = next((l for l in p.stdout.splitlines() if l.startswith('FAIL')), '')
    return p.returncode, first


def main():
    wanted = set(sys.argv[1:])
    tried = 0
    missed = []
    for name, what, file, old, new in MUTANTS:
        if wanted and name not in wanted:
            continue
        path = os.path.join(SRC, file)
        with open(path) as f:
            good = f.read()
        good_digest = digest(path)
        tried += 1
        if good.count(old) != 1:
            print('BROKEN: %s -- its text is in %s %d times, not once' % (name, file, good.count(old)))
            missed.append(name)
            continue
        try:
            with open(path, 'w') as f:
                f.write(good.replace(old, new))
            if build() != 0:
                print('BROKEN: %s -- the mutant does not build' % name)
                missed.append(name)
                continue
            code, first = run('gate')
            if code == 0:
                print('SURVIVED: %s (%s)' % (name, what))
                missed.append(name)
            else:
                print('CAUGHT: %s (%s) -- %s' % (name, what, first[:200]))
        finally:
            with open(path, 'w') as f:
                f.write(good)
            if digest(path) != good_digest:
                sys.exit('%s is not what it was: restore it from git' % path)
    print('%d mutants, %d not caught%s' % (tried, len(missed), (': ' + ', '.join(missed)) if missed else ''))
    return 1 if missed else 0


if __name__ == '__main__':
    sys.exit(main())
