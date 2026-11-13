#!/usr/bin/env python3
"""Mutants of the raft crate -- and of the cluster's registry, its first user,
NBS's metadata, its second, and the asks a member of either sends -- the gate must catch: each one rule taken out of
src/crates/common/cluster/raft/src/node.rs (or the file a mutant names), the fuzzer built
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
SRC = os.path.join(ROOT, 'src', 'crates', 'common', 'cluster', 'raft', 'src')
# The registry's and nbsmeta's mutants name their file from SRC.
REGISTRY = os.path.join('..', '..', 'registry', 'src', 'lib.rs')
NBSMETA = os.path.join('..', '..', '..', 'storage', 'nbsmeta', 'src', 'lib.rs')
ASKS = 'asks.rs'
OUT = os.path.join(ROOT, 'out', 'raft-mutants')
BINARY = os.path.join(OUT, 'release', 'raft-fuzz')
# The kernel's vendored crates, for nbsmeta's sha2 (scripts/raft-test.py).
VENDOR = os.path.join(ROOT, 'src', 'crates', 'vendor')

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
    # The review of 2026-11 (fuzz/raft/src/review2.rs): each rule it added.
    ('snapshot_piece_twice', "a snapshot's answer that says nothing new answered with a piece",
     '        if next == sending.acked {',
     '        if false && next == sending.acked {',
     'snapshot_pieces_bounded'),
    ('snapshot_resend_always', "a snapshot's piece sent again at every heartbeat, moving or not",
     '&& pr.sending.as_mut().is_some_and(|s| !mem::replace(&mut s.moved, false));',
     '&& pr.sending.is_some();',
     'snapshot_pieces_bounded'),
    ('term_below_log_kept', "a member started at a term below its own log's last entry's",
     'let (term, vote) = if last_term > hs.term {',
     'let (term, vote) = if false && last_term > hs.term {',
     'term_behind_log'),
    ('probe_kept_on_match', 'a probe answered with what the leader knew left probing',
     'let probe_answered = pr.state == State::Probe && pr.matched == index;',
     'let probe_answered = false;',
     'probe_leaves_on_match'),
    ('stale_vote_answered_as_vote', "a vote request of a term gone by answered with a vote's no",
     '                Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. } | Body::Vote { .. } => {',
     '                Body::Vote { pre, .. } => {\n'
     '                    let pre = *pre;\n'
     '                    self.send(from, Body::VoteResp { pre, granted: false });\n'
     '                }\n'
     '                Body::Append { .. } | Body::Heartbeat { .. } | Body::Snapshot { .. } => {',
     'stale_vote_not_counted'),
    ('regress_not_repaired', 'a member that lost what it acknowledged probed with the same append for good',
     '        if rejected <= self.matched {\n            /* The probe it refused',
     '        if false && rejected <= self.matched {\n            /* The probe it refused',
     'regressed_follower_repaired', 'progress.rs'),
    ('added_voter_unheard', "a voter added in one step counted as not heard from by CheckQuorum's next look",
     '                    pr.recent_active = true;\n                    self.progress.insert(*id, pr);',
     '                    self.progress.insert(*id, pr);',
     'added_voter_heard'),
    ('transfer_unreachable_armed', 'a hand-over armed to a voter not heard from in an election\'s time',
     'if self.progress.get(&to).is_none_or(|p| p.idle >= self.cfg.election_ticks) {',
     'if false && self.progress.get(&to).is_none_or(|p| p.idle >= self.cfg.election_ticks) {',
     'transfer_unreachable_refused'),
    ('read_refusal_dropped', "a follower's read the leader refused dropped, not handed out failed",
     '                    self.push_read_failed(ctx);',
     '                    let _ = ctx;',
     'follower_read_refused'),
    ('leader_reads_unbounded', "a leader's reads taken past the room a Ready hands out",
     'if self.waiting_reads.len() + self.reads.len() + self.read_states.len() >= MAX_PENDING_READS {',
     'if self.waiting_reads.len() + self.reads.len() >= MAX_PENDING_READS {',
     'leader_reads_bounded'),
    ('rsm_read_never_aged', 'a read whose request or answer was lost waited for for good',
     'let limit = 4 * u64::from(self.node.config().election_ticks);',
     'let limit = u64::MAX;',
     'rsm_read_lost_fails', 'rsm.rs'),
    ('rsm_install_before_restore', "a leader's snapshot made the whole of the log before the state machine read it",
     '                self.restore(&snap)?;\n'
     '                self.node.storage_mut().apply_snapshot(snap).map_err(|e| self.stop(RsmError::Store(e)))?;',
     '                self.node.storage_mut().apply_snapshot(snap.clone()).map_err(|e| self.stop(RsmError::Store(e)))?;\n'
     '                self.restore(&snap)?;',
     'rsm_snapshot_refused_keeps_store', 'rsm.rs'),
    ('rsm_seq_zero_submitted', 'a command numbered 0 submitted',
     '        if seq == 0 {\n            return Err(RsmError::BadSeq);\n        }\n',
     '',
     'rsm_seq_zero', 'rsm.rs'),
    ('rsm_seq_zero_applied', 'a command numbered 0 in the log answered from its session',
     '                _ if seq == 0 => out.events.push(Event::Stale { client, seq }),\n',
     '',
     'rsm_seq_zero', 'rsm.rs'),
    ('rsm_caught_up_early', 'a member taken for caught up before the log it had committed is applied',
     '        self.applied >= self.start_commit\n',
     '        true\n',
     'rsm_caught_up', 'rsm.rs'),
    ('rsm_sync_always', 'every Ready synced, nothing in it to be durable or not',
     '            if must_sync {\n                self.node.storage_mut().sync()',
     '            if true {\n                self.node.storage_mut().sync()',
     'rsm_sync_when_needed', 'rsm.rs'),
    ('rsm_entries_not_synced', "a Ready's entries not synced before what it sends goes",
     'let mut must_sync = !rd.entries.is_empty();',
     'let mut must_sync = false;',
     'rsm_sync_when_needed', 'rsm.rs'),
    ('rsm_vote_not_synced', "a Ready's vote not synced before it is answered",
     'must_sync |= hs.term != self.written.term || hs.vote != self.written.vote;',
     'must_sync |= false;',
     'rsm_vote_durable', 'rsm.rs'),
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
    # The cluster's registry (src/crates/common/cluster/registry), the crate's first user.
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
    ('force_alone_uncommitted', 'recover leaving the changes it appended uncommitted',
     'store.set_hard_state(HardState { term, vote, commit: last })',
     'store.set_hard_state(HardState { term, vote, commit: committed_before })',
     'force_alone_after_loss', 'recover.rs'),
    ('force_alone_voters_only', 'recover taking out the voters and leaving the learners',
     'conf.voters.iter().chain(conf.learners.iter()).copied().filter(|&n| n != me).collect()',
     'conf.voters.iter().copied().filter(|&n| n != me).collect()',
     'force_alone_after_loss', 'recover.rs'),
    ('registry_retired_key_back', 'a key a host dropped taken again',
     'self.removed.iter().any(|r| r.keys.contains(key)) || self.retired.iter().any(|r| r.key == *key)',
     'self.removed.iter().any(|r| r.keys.contains(key))',
     'registry_keys', REGISTRY),
    ('registry_last_key', "a host's last key dropped",
     'if self.hosts[at].keys.len() == 1 {\n                    return Err(Refusal::LastKey);',
     'if false && self.hosts[at].keys.len() == 1 {\n                    return Err(Refusal::LastKey);',
     'registry_keys', REGISTRY),
    ('registry_address_taken', "an address another host has given to a host",
     'if *ip != 0 && self.address_in_use(*ip, *port, Some(at)) {',
     'if false && *ip != 0 && self.address_in_use(*ip, *port, Some(at)) {',
     'registry_addresses', REGISTRY),
    ('registry_port_ignored', "a host's address told from another's by its IP alone, its port not looked at",
     'self.ip == ip && self.port == port',
     'self.ip == ip',
     'registry_ports', REGISTRY),
    ('nbsmeta_removed_back', 'an incarnation removed taken again',
     'if self.id_removed(*member) {\n                    return Err(Refusal::MemberRemoved);',
     'if false && self.id_removed(*member) {\n                    return Err(Refusal::MemberRemoved);',
     'nbsmeta_replace', NBSMETA),
    ('nbsmeta_new_incarnation_taken', "a host's new incarnation taken for its old one",
     'return if m.id == *member { Ok(m.id) } else { Err(Refusal::HostIsMember) };',
     'return Ok(m.id);',
     'nbsmeta_replace', NBSMETA),
    ('nbsmeta_remove_again_refused', 'a remove asked again, once done, refused',
     'return self.removed.iter().rev().find(|r| r.host == *host).map(|r| r.id)\n                        .ok_or(Refusal::NoSuchHost);',
     'return Err(Refusal::NoSuchHost);',
     'nbsmeta_replace', NBSMETA),
    ('nbsmeta_last_member', "the cluster's last member removed",
     'if self.members.len() == 1 {\n                    return Err(Refusal::LastMember);',
     'if false && self.members.len() == 1 {\n                    return Err(Refusal::LastMember);',
     'nbsmeta_bounds', NBSMETA),
    ('nbsmeta_all_vote', 'every member a voter, none a learner',
     'self.members.iter().take(VOTERS_MAX).map(|m| m.id).collect()',
     'self.members.iter().map(|m| m.id).collect()',
     'nbsmeta_voters', NBSMETA),
    ('nbsmeta_snapshot_removed_unchecked', 'a snapshot with a removed member taken',
     'if self.id_removed(m.id) {\n                return Err(RestoreError("a member removed"));',
     'if false && self.id_removed(m.id) {\n                return Err(RestoreError("a member removed"));',
     'nbsmeta_snapshot_rules', NBSMETA),
    ('asks_resend_new_session', 'an ask sent again to a new leader under a new session: a command in a log twice, '
     'applied twice',
     'for a in self.asks.iter_mut() {\n            a.due = 0;\n        }',
     'for a in self.asks.iter_mut() {\n            a.due = 0;\n            a.client = None;\n        }',
     'asks_once', ASKS),
    ('asks_expired_ignored', 'a try under a session gone not asked again under a new one',
     'if let Some(at) = self.asks.iter().position(|a| a.client == Some(client)) {\n                    let request',
     'if let Some(at) = self.asks.iter().position(|a| false && a.client == Some(client)) {\n                    '
     'let request',
     'asks_expired', ASKS),
    ('asks_unknown_said_not_asked', 'an ask whose command was sent, its time up, told it was never asked',
     'let outcome = if a.sent {',
     'let outcome = if false && a.sent {',
     'asks_unknown', ASKS),
    ('asks_leader_tells_no_outcome', 'the leader that applied a command it took for another member not telling it',
     'self.owed.remove(&client).map(|to| (to, Told::Applied { client, index, answer: answer.clone() }))',
     'self.owed.remove(&client).filter(|_| false).map(|to| (to, Told::Applied { client, index, answer: '
     'answer.clone() }))',
     'asks_told', ASKS),
    ('asks_leader_tells_no_session', 'the leader that made a session another member asked for not telling it',
     'self.registering.remove(&(origin, request)).map(|to| (to, Told::Registered { request, client }))',
     'self.registering.remove(&(origin, request)).filter(|_| false).map(|to| (to, Told::Registered { request, '
     'client }))',
     'asks_told', ASKS),
]


def digest(path):
    with open(path, 'rb') as f:
        return hashlib.sha256(f.read()).hexdigest()


def build():
    return subprocess.run(['cargo', 'build', '--release', '--quiet', '--offline',
                           '--manifest-path', os.path.join(ROOT, 'fuzz', 'raft', 'Cargo.toml'),
                           '--target-dir', OUT,
                           '--config', 'source.crates-io.replace-with="vendored-sources"',
                           '--config', 'source.vendored-sources.directory="%s"' % VENDOR], cwd=ROOT,
                          stdout=subprocess.DEVNULL, stderr=subprocess.PIPE).returncode


# A run past this is cut short and counted as failing: a mutant that breaks
# a rule of safety can make a run go on for ever -- a gate in CI would hang,
# which is a failure too -- and what it said by then is kept.
RUN_LIMIT = 600


def run(*args):
    """A run's exit code and its first failing line."""
    p = subprocess.Popen([BINARY] + list(args), cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                         text=True)
    try:
        out, _ = p.communicate(timeout=RUN_LIMIT)
        code = p.returncode
    except subprocess.TimeoutExpired:
        p.kill()
        out, _ = p.communicate()
        code = -1
    lines = out.splitlines()
    first = next((l for l in lines if l.startswith('FAIL')), 'cut short after %d s' % RUN_LIMIT if code == -1 else '')
    return code, first


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
