#!/usr/bin/env python3
"""Mutants of the raft crate -- and of the cluster's registry, its first user
-- the gate must catch: each one rule taken out of
src/crates/raft/src/node.rs (or the file a mutant names), the fuzzer built
over it, and `raft-fuzz gate`
run -- which must fail; and so must the scenario written for that rule, on
its own. The source is put back after each, whatever happens (an exception,
^C, a build that fails), and checked against its hash before the next;
nothing here is piped anywhere, so no SIGPIPE can leave a mutant behind.

Usage: python3 fuzz/raft/mutants.py [name...]
Exit 0 when every mutant was caught, each by its own scenario too; 1 when
one was not.
"""
import hashlib
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
SRC = os.path.join(ROOT, 'src', 'crates', 'raft', 'src')
# The registry's mutants name their file from SRC.
REGISTRY = os.path.join('..', '..', 'registry', 'src', 'lib.rs')
OUT = os.path.join(ROOT, 'out', 'raft-mutants')
BINARY = os.path.join(OUT, 'release', 'raft-fuzz')

# name, what it takes out, the text it changes, what it becomes, and the
# scenario written for that rule.
MUTANTS = [
    ('figure8', 'the figure 8 check: replicas of an earlier term counted',
     'if mci > self.log.committed && self.log.term(mci) == Some(self.term) {',
     'if mci > self.log.committed {',
     'figure8'),
    ('vote_not_persisted', 'a vote not written before it is answered',
     'HardState { term: self.term, vote: self.vote, commit: self.log.committed }',
     'HardState { term: self.term, vote: NONE, commit: self.log.committed }',
     'vote_persisted'),
    ('timer_not_reset_on_vote', 'the election timer not started over by a vote given',
     'self.election_elapsed = 0;\n                self.vote = from;',
     'self.vote = from;',
     'timer_reset_on_vote'),
    ('commit_past_match', 'a follower committing past what its append showed to match',
     'self.log.commit_to(cmp::min(commit, last_new));',
     'self.log.commit_to(commit);',
     'commit_past_match'),
    ('prevote_is_a_vote', 'a PreVote won taken as the election won',
     'Outcome::Won if self.role == Role::PreCandidate => self.campaign_real(false),',
     'Outcome::Won if self.role == Role::PreCandidate => {\n'
     '                        self.term += 1;\n'
     '                        self.vote = self.id;\n'
     '                        self.become_leader();\n'
     '                        self.broadcast_append();\n'
     '                    }',
     'prevote_not_a_vote'),
    ('prev_term_ignored', 'an append taken with its previous entry of another term',
     'if self.log.match_term(prev_index, prev_term) {',
     'if prev_index <= self.log.last_index() {',
     'append_prev_term_mismatch'),
    ('change_before_first_commit', 'the 2015 rule: a change before the leader has committed in its term',
     'if self.log.committed < self.term_start {\n'
     '            return Err(Error::ChangeRefused(Refusal::NotYetCommittedInTerm));\n'
     '        }',
     '',
     'change_before_first_commit'),
    ('read_without_round', 'a read confirmed with no round of heartbeats',
     'if self.membership.voters().len() == 1 && self.membership.is_voter(self.id) {',
     'if true {',
     'stale_leader_read'),
    ('read_fault_ignored', "a term the storage failed to read taken as none, the member going on",
     'match self.storage.term(index) {\n'
     '            Ok(t) => Some(t),\n'
     '            Err(e) => {\n'
     '                self.failed(e);\n'
     '                None\n'
     '            }\n'
     '        }',
     'self.storage.term(index).ok()',
     'storage_fault_vote', 'raft_log.rs'),
    ('read_round_once', 'a round of reads asked by its own heartbeats alone',
     'let newest = self.reads.back().map_or(0, |r| r.round);',
     'let newest = 0;',
     'read_round_lost'),
    ('append_all_or_none', 'an append past the room left dropped whole',
     'let take = new.len().min(usize::try_from(room).unwrap_or(usize::MAX));',
     'let take = if new.len() as u64 > room { 0 } else { new.len() };',
     'partial_append'),
    ('snapshot_copies_tail', 'a snapshot taken that keeps no pieces of the log after it',
     'self.chunks.drain(..whole);',
     'self.chunks.drain(..whole);\n        self.chunks = self.chunks.concat().chunks(4096).map(|c| c.to_vec()).collect();',
     'snapshot_long_tail', 'storage.rs'),
    ('log_no_crc', "a log record read whatever its CRC says",
     '|| record_crc(&p) != crc {',
     '{',
     'log_torn_tail', 'log.rs'),
    ('log_no_seal', 'a segment not synced before the next is begun',
     'if let Some(name) = self.current_name() {\n            self.io(|f| f.sync(&name))?;\n        }\n        let seq',
     'let seq',
     'log_sealed_segments', 'log.rs'),
    ('log_open_not_synced', 'what an open reads -- a cut, a name -- not made the medium\'s before it is used',
     'for s in self.segments.clone().iter() {\n            self.io(|f| f.sync(&wal_name(s.seq)))?;\n        }\n        self.io(|f| f.sync_dir())?;\n',
     '',
     'log_open_syncs', 'log.rs'),
    ('log_snapshot_not_synced', 'a snapshot renamed before its bytes are on the medium',
     'self.io(|f| f.sync(&tmp))?;\n        self.io(|f| f.rename(&tmp, &name))?;',
     'self.io(|f| f.rename(&tmp, &name))?;',
     'log_snapshot_durable', 'log.rs'),
    ('log_compact_too_far', 'a segment removed that holds entries past the snapshot',
     'if s.seq >= self.marker_seq || (s.seq >= self.reset_seq && s.max_index > index) {',
     'if s.seq >= self.marker_seq {',
     'log_compaction', 'log.rs'),
    ('log_install_keeps_log', "a leader's snapshot not dropping the log before it once read back",
     'if let Some(m) = reset {',
     'if let Some(m) = reset.filter(|_| false) {',
     'log_install_drops_log', 'log.rs'),
    ('rsm_no_dedup', 'a command retried under its number applied again',
     'Some(s) if seq == s.last => {',
     'Some(s) if seq == s.last && false => {',
     'rsm_retry_once', 'rsm.rs'),
    ('rsm_read_before_applied', 'a read served before the member has applied as far as its index',
     'if index > self.applied {\n                break;\n            }\n            let Some',
     'let Some',
     'rsm_read_waits', 'rsm.rs'),
    ('rsm_late_index', 'a snapshot named at the index applied when it is done, not when it was frozen',
     'return match self.node.storage_mut().create_snapshot(t.index, t.conf, t.out.finish()) {',
     'let late = self.applied;\n                let conf = self.node.conf_state_at(late).unwrap_or(t.conf);\n                return match self.node.storage_mut().create_snapshot(late, conf, t.out.finish()) {',
     'rsm_snapshot_frozen', 'rsm.rs'),
    ('rsm_ctx_reused', "a read's context the same from start to start, an answer to one before a restart taken for one after",
     'let ctx = self.nonce.wrapping_add(self.next_ctx);',
     'let ctx = self.next_ctx;',
     'rsm_read_after_restart', 'rsm.rs'),
    ('rsm_sessions_not_snapshotted', 'the sessions left out of the snapshot',
     'head.extend_from_slice(&(self.sessions.len() as u32).to_le_bytes());\n        if !b.push(&head) {\n            return false;\n        }\n        for (&client, s) in self.sessions.iter() {',
     'head.extend_from_slice(&0u32.to_le_bytes());\n        if !b.push(&head) {\n            return false;\n        }\n        for (&client, s) in self.sessions.iter().take(0) {',
     'rsm_sessions_in_snapshot', 'rsm.rs'),
    # The cluster's registry (src/crates/registry), the crate's first user.
    ('registry_no_expiry', "an invitation kept past its time by the entry's clock",
     'self.invitations.retain(|i| i.expires > now);',
     'self.invitations.retain(|_| true);',
     'registry_expiry', REGISTRY),
    ('registry_token_twice', 'an invitation left open by the join that used it',
     'let inv = self.invitations.remove(at);',
     'let inv = self.invitations[at].clone();',
     'registry_join_once', REGISTRY),
    ('registry_removed_key_back', "a removed host's key taken by an invitation",
     'if self.key_removed(key) {\n                    return Err(Refusal::KeyRemoved);\n                }\n                if *expires <= *now {',
     'if *expires <= *now {',
     'registry_removed_key', REGISTRY),
    ('registry_node_reused', "a removed host's node given to the next host to join",
     'let node = self.next_node;\n                self.next_node += 1;',
     'let node = self.hosts.len() as u64 + 1;',
     'registry_removed_key', REGISTRY),
    ('registry_snapshot_keys_unchecked', 'a snapshot with a key twice taken',
     'if twice || self.key_removed(k) {',
     'if false {',
     'registry_snapshot_rules', REGISTRY),
    ('registry_all_vote', 'every host a voter, none a learner',
     'self.hosts.iter().take(VOTERS_MAX).map(|h| h.node).collect()',
     'self.hosts.iter().map(|h| h.node).collect()',
     'registry_voters', REGISTRY),
]


def digest(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


def build():
    return subprocess.run(['cargo', 'build', '--release', '--quiet', '--offline',
                           '--manifest-path', os.path.join(ROOT, 'fuzz', 'raft', 'Cargo.toml'),
                           '--target-dir', OUT], cwd=ROOT,
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
    for mutant in MUTANTS:
        name, what, old, new, scenario = mutant[:5]
        path = os.path.join(SRC, mutant[5] if len(mutant) > 5 else 'node.rs')
        if wanted and name not in wanted:
            continue
        with open(path) as f:
            good = f.read()
        good_digest = digest(path)
        tried += 1
        if good.count(old) != 1:
            print('BROKEN: %s -- its text is in %s %d times, not once' % (name, os.path.basename(path), good.count(old)))
            missed.append(name)
            continue
        try:
            with open(path, 'w') as f:
                f.write(good.replace(old, new))
            try:
                if build() != 0:
                    print('BROKEN: %s -- the mutant does not build' % name)
                    missed.append(name)
                    continue
                code, first = run('gate')
                own, own_first = run('scenarios', scenario)
                if code == 0:
                    print('SURVIVED: %s (%s)' % (name, what))
                    missed.append(name)
                elif own == 0:
                    print('CAUGHT, NOT BY ITS SCENARIO: %s -- %s passed; the gate: %s' % (name, scenario, first[:160]))
                    missed.append(name)
                else:
                    print('CAUGHT: %s (%s) -- %s' % (name, what, own_first[:200]))
            finally:
                with open(path, 'w') as f:
                    f.write(good)
        finally:
            with open(path, 'w') as f:
                f.write(good)
            if digest(path) != good_digest:
                sys.exit('%s is not what it was: restore it from git' % path)
    print('%d mutants, %d not caught as they must be%s' % (tried, len(missed), (': ' + ', '.join(missed)) if missed else ''))
    return 1 if missed else 0


if __name__ == '__main__':
    sys.exit(main())
