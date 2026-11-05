#!/usr/bin/env python3
"""Mutants of the raft crate the gate must catch: each one rule taken out of
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
