#!/usr/bin/env python3
"""The nbs fuzzer's mutants: one rule of NBS's metadata
(src/crates/common/storage/nbsmeta), of its chunk server's engine
(src/crates/common/storage/nbscs), of their wire (nbswire) or of the client
(nbsclient) taken out at a time, and the gate
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
     'used.insert(trial.pick(volume, from + k).ok_or(Refusal::NoRoom)?);',
     'let Some(cs) = trial.pick(volume, from + k) else { break };\n            used.insert(cs);'),
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
     'if self.cs.iter().zip(held.iter()).any(|(c, &h)| c.reserved != h) {',
     'if false && self.cs.iter().zip(held.iter()).any(|(c, &h)| c.reserved != h) {'),
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
    ('down_bumps_version', "a replica taken out raises its set's version",
     'nbsmeta/src/storage.rs',
     '            s.active_until[at] = s.version;\n            s.version = version;',
     '            s.active_until[at] = s.version;\n            let _ = version;'),
    ('last_replica_kept', "a set's last active replica never taken out",
     'nbsmeta/src/storage.rs',
     '        if s.active() <= 1 {\n            return Err(Refusal::LastReplica);\n        }',
     ''),
    # N9a: the repair model
    ('cs_down_keeps_last', "a cs down never takes a set's last active member with it",
     'nbsmeta/src/storage.rs',
     '|| (s.active() > 1 && (0..REPLICAS).any(|k| s.cs[k] == cs && s.replicas[k] == Replica::Active))',
     '|| ((0..REPLICAS).any(|k| s.cs[k] == cs && s.replicas[k] == Replica::Active))'),
    ('cs_down_drops_joins', 'a cs down dropped as the joiner of its sets',
     'nbsmeta/src/storage.rs',
     'if let Some(j) = s.joiner.filter(|j| j.cs == cs) {\n                if s.cs[usize::from(j.place)] != cs {',
     'if let Some(j) = s.joiner.filter(|j| j.cs == cs && false) {\n                if s.cs[usize::from(j.place)] != cs {'),
    ('cs_down_until', 'a member down keeps the last version it was active at',
     'nbsmeta/src/storage.rs',
     '                    s.replicas[k] = Replica::Down;\n                    s.active_until[k] = s.version;',
     '                    s.replicas[k] = Replica::Down;\n                    s.active_until[k] = s.version + 1;'),
    ('join_stale', 'a join of a set moved on since its plan refused',
     'nbsmeta/src/storage.rs',
     '        if s.version != version {\n            return Err(Refusal::Stale);\n        }',
     ''),
    ('join_one_at_a_time', 'one join of a set at a time',
     'nbsmeta/src/storage.rs',
     '        if s.joiner.is_some() {\n            return Err(Refusal::Joining);\n        }',
     ''),
    ('join_room', 'a join in another\'s place only with room for the set',
     'nbsmeta/src/storage.rs',
     '            if c.free() < u64::from(s.chunks) {',
     '            if false && c.free() < u64::from(s.chunks) {'),
    ('join_in_place', 'an active member replaced only where its cs is drained',
     'nbsmeta/src/storage.rs',
     'if !(s.replicas[k] == Replica::Down && gone) && !m.draining() {',
     'if false && !(s.replicas[k] == Replica::Down && gone) && !m.draining() {'),
    ('join_reserves', "a joiner in another's place holds room for the set's chunks",
     'nbsmeta/src/storage.rs',
     '                c.reserved += u64::from(chunks);',
     '                c.reserved += 0;'),
    ('joined_gives_room_back', 'the member a joiner replaces gives its room back',
     'nbsmeta/src/storage.rs',
     '        if old != cs {\n            if let Some(c) = self.cs.get_mut(old as usize) {',
     '        if false && old != cs {\n            if let Some(c) = self.cs.get_mut(old as usize) {'),
    ('joined_in_order', "a set's three in order again once a joiner takes a place",
     'nbsmeta/src/storage.rs',
     '        members.sort_unstable_by_key(|m| m.0);',
     ''),
    ('replica_down_drops_joiner', 'a joiner silent to the writer has its join dropped',
     'nbsmeta/src/storage.rs',
     '        if let Some(j) = s.joiner.filter(|j| j.cs == cs) {\n            return self.unjoin(set, cs, j.since);\n        }',
     ''),
    ('clean_sets_only', 'new chunks only in a clean set',
     'nbsmeta/src/storage.rs',
     '        s.joiner.is_none() && s.replicas.iter().all(|&r| r == Replica::Active)',
     '        true'),
    ('empty_set_again', 'an empty set made again before a new one',
     'nbsmeta/src/storage.rs',
     '        let empty = self.sets.iter().position(|s| State::spare(s, used));',
     '        let empty: Option<usize> = None;'),
    ('spare_not_used', "an empty set made again only where it is no clean set the placement fills",
     'nbsmeta/src/storage.rs',
     ' && s.version < u32::MAX && !(State::is_clean(s) && used.contains(&s.cs))',
     ' && s.version < u32::MAX'),
    ('placement_up_only', 'new chunks only on cs\'s that are up',
     'nbsmeta/src/place.rs',
     ' || c.state != CsState::Up {',
     ' {'),
    ('joiner_checked', "a snapshot's joiner held to a place it can have",
     'nbsmeta/src/snapshot.rs',
     '                if !ok {\n                    return Err(RestoreError("a set\'s joiner',
     '                if false && !ok {\n                    return Err(RestoreError("a set\'s joiner'),
    ('until_checked', "a snapshot's members down held to a version before the set's",
     'nbsmeta/src/snapshot.rs',
     '                if !ok {\n                    return Err(RestoreError("a set\'s member down',
     '                if false && !ok {\n                    return Err(RestoreError("a set\'s member down'),
    ('controller_back', 'the controller has a member back join its own place',
     'nbsmeta/src/repair.rs',
     '        if set.replicas[k] == Replica::Down && cs_of(k).is_some_and(usable) {',
     '        if false && set.replicas[k] == Replica::Down && cs_of(k).is_some_and(usable) {'),
    ('controller_replaces', "the controller has another take a member gone's or drained's place",
     'nbsmeta/src/repair.rs',
     '        if (set.replicas[k] == Replica::Down && gone) || c.draining() {',
     '        if false && ((set.replicas[k] == Replica::Down && gone) || c.draining()) {'),
    ('controller_room', 'the controller joins only a cs with room for the set',
     'nbsmeta/src/repair.rs',
     '        if room < u64::from(set.chunks) {',
     '        if false && room < u64::from(set.chunks) {'),
    ('controller_changes_alone', "the controller's joins wait for a turn without a cs's change",
     'nbsmeta/src/repair.rs',
     '    if !out.is_empty() {\n        return out;\n    }',
     ''),
    ('controller_out', 'the controller has a cs silent past out_after out',
     'nbsmeta/src/repair.rs',
     '        let out_now = !v.answering && v.silent_ms >= out_after_ms && !v.maintenance;',
     '        let out_now = false;'),
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
    ('cs_strangers_left', "a device with anything past a superblock's bytes never taken for blank",
     'nbscs/src/format.rs',
     '    if !ours(a) || !ours(b) {',
     '    if false && (!ours(a) || !ours(b)) {'),
    ('cs_read_volume', "nothing of one volume's read through another's",
     'nbscs/src/store.rs',
     '        if r.volume != volume {\n            return Err(Error::NotTheVolumes);\n        }\n        if r.known != c.epoch && !verified {\n            return Err(Error::Epoch { known: r.known });\n        }\n        let dev = &mut self.devs[dv];\n        let at = dev.layout.data + u64::from(slot) * SLOT + u64::from(offset);\n        if dev.d.read(',
     '        if r.known != c.epoch && !verified {\n            return Err(Error::Epoch { known: r.known });\n        }\n        let dev = &mut self.devs[dv];\n        let at = dev.layout.data + u64::from(slot) * SLOT + u64::from(offset);\n        if dev.d.read('),
    ('wire_map_max', 'a map asked for past MAP_MAX read as none',
     'nbswire/src/lib.rs',
     '                if count > MAP_MAX {\n                    return None;\n                }\n                Ask::Map { name, from, count, index }',
     '                Ask::Map { name, from, count, index }'),
    ('cs_slot_given_back', 'a freed slot given back to the free list',
     'nbscs/src/store.rs',
     '        self.devs[dv].free.push(slot);\n        Ok(())',
     '        Ok(())'),
    # The writer's own (N8): caught by the cluster target
    ('attach_token', "a writer's attach asked again answered with the generation it gave",
     'nbsmeta/src/storage.rs',
     '.filter(|w| token != 0 && w.token == token && w.host == host)',
     '.filter(|_| false)'),
    ('down_by_writer', "a replica taken out only by the volume's writer now",
     'nbsmeta/src/storage.rs',
     'if v.writer.as_ref().is_none_or(|w| w.generation != generation) {',
     'if false && v.writer.as_ref().is_none_or(|w| w.generation != generation) {'),
    ('cs_found_open_refused', "a chunk found open refused to its writer",
     'nbswire/src/serve.rs',
     'if g != 0 && store.found_open(chunk.chunk, chunk.incarnation) == Some(g) {',
     'if false && store.found_open(chunk.chunk, chunk.incarnation) == Some(g) {'),
    ('cs_found_open_flush', "a flush refused to a writer with a chunk found open",
     'nbswire/src/serve.rs',
     'if g != 0 && store.found_open_in(v, g) {',
     'if false && store.found_open_in(v, g) {'),
    ('cs_flush_fenced', 'a flush refused to a writer fenced since',
     'nbswire/src/serve.rs',
     'store.flush_for(v, g)',
     'store.flush()'),
    ('client_write_all', 'a write done only once every replica in has it',
     'nbsclient/src/lib.rs',
     'p.waiting.remove(&cs);',
     'p.waiting.clear();'),
    ('client_fences', 'every cs fenced before the writer serves',
     'nbsclient/src/lib.rs',
     '                for cs in self.map.written_cs() {\n                    self.send(cs, What::Fence, now, bufs, out);',
     '                for cs in self.map.written_cs() {\n                    self.fenced.insert(cs);'),
    ('client_reconcile', 'a chunk left open made the same on its replicas',
     'nbsclient/src/lib.rs',
     'self.mend = places.into_iter().collect();',
     'self.mend = places.into_iter().filter(|_| false).collect();'),
    ('client_read_active', 'a read only of an active replica',
     'nbsclient/src/lib.rs',
     'self.cs[i] == cs && self.replicas[i] == ReplicaState::Active)',
     'self.cs[i] == cs)'),
    ('client_flush_order', 'a flush done only after the flushes before it',
     'nbsclient/src/lib.rs',
     '|| self.ops.iter().any(|(&other, x)| other < op && x.kind.flushes())',
     ''),
    ('client_flush_dirty', 'a flush of every cs that has acknowledged a write since the last began',
     'nbsclient/src/lib.rs',
     '                self.dirty.insert(cs);',
     ''),
    ('client_overlap', 'two writes that overlap never out at once',
     'nbsclient/src/lib.rs',
     '&& !self.ops.iter().any(|(&other, x)| other < id && x.kind == Kind::Write',
     '&& !self.ops.iter().any(|(&other, x)| false && other < id && x.kind == Kind::Write'),
    ('client_stop_waits', 'what was asked before the stop done before it',
     'nbsclient/src/lib.rs',
     'matches!(self.phase, Phase::Serving | Phase::Stopping) || o.tag & INTERNAL != 0',
     'self.phase == Phase::Serving || o.tag & INTERNAL != 0'),
    ('client_lost_last', "a writer its chunk's last replica refuses failed, not left waiting",
     'nbsclient/src/lib.rs',
     'Said::LastReplica if refused => {',
     'Said::LastReplica if false && refused => {'),
    ('client_refused_asked', 'a replica that refuses asked out as refusing, though asked out as silent',
     'nbsclient/src/lib.rs',
     '.is_some_and(|d| d.refused || !refused)',
     '.is_some()'),
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
