'use client';

import { useEffect, useState } from 'react';

import { call } from '@/lib/client';
import { bytes } from '@/lib/descriptor';
import { Words } from '@/lib/i18n';
import { Candidate, DeviceChoice, runSetup, SetupDevice, SetupHost, usableDevice, validateSetup } from '@/lib/setup';

export function ClusterSetup({ hosts, locale, w }: { hosts: SetupHost[]; locale: string; w: Words }) {
  const ru = locale === 'ru';
  const [selected, setSelected] = useState<string[]>([]);
  const [leaderId, setLeaderId] = useState('');
  const [clusterName, setClusterName] = useState('cluster');
  const [nbsName, setNbsName] = useState('store');
  const [phase, setPhase] = useState<'choose' | 'review' | 'running' | 'done' | 'error'>('choose');
  const [candidates, setCandidates] = useState<Candidate[]>([]);
  const [devices, setDevices] = useState<Record<string, DeviceChoice>>({});
  const [wipeApproved, setWipeApproved] = useState(false);
  const [log, setLog] = useState<string[]>([]);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const online = hosts.filter((h) => h.online).map((h) => h.id);
    setSelected((cur) => {
      const kept = cur.filter((id) => online.includes(id));
      return kept.length ? kept : online;
    });
    setLeaderId((cur) => cur && online.includes(cur) ? cur : (online[0] ?? ''));
  }, [hosts]);

  function toggle(id: string) {
    setSelected((cur) => cur.includes(id) ? cur.filter((x) => x !== id) : [...cur, id]);
  }

  async function review() {
    setBusy(true);
    setError('');
    try {
      const chosen = hosts.filter((h) => selected.includes(h.id));
      if (chosen.length < 3) throw new Error(w.setupThree);
      const got = await Promise.all(chosen.map(async (host): Promise<Candidate> => {
        const [cluster, nbs, local] = await Promise.all([
          call(host.id, 'cluster', 'status'), call(host.id, 'nbs', 'status'), call(host.id, 'vm', 'devices'),
        ]);
        for (const r of [cluster, nbs, local]) {
          if (!r.ok) throw new Error(`${host.name}: ${r.error.message}`);
        }
        return {
          host,
          cluster: cluster.ok ? cluster.data as Candidate['cluster'] : null as never,
          nbs: nbs.ok ? nbs.data as Candidate['nbs'] : null as never,
          devices: local.ok ? (local.data as { devices?: SetupDevice[] }).devices ?? [] : [],
        };
      }));
      const leader = got.find((c) => c.host.id === leaderId);
      if (leader?.cluster.member && leader.cluster.cluster) setClusterName(leader.cluster.cluster);
      if (leader?.nbs.member && leader.nbs.cluster) setNbsName(leader.nbs.cluster);
      const choices: Record<string, DeviceChoice> = {};
      for (const c of got) {
        const usable = c.devices.filter(usableDevice);
        const device = usable.find((d) => !d.holds) ?? usable[0];
        if (device?.id) choices[c.host.id] = { id: device.id, wipe: false };
      }
      setCandidates(got);
      setDevices(choices);
      setWipeApproved(false);
      setPhase('review');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function run() {
    const req = { candidates, leaderId, clusterName, nbsName, devices };
    try {
      validateSetup(req);
      if (candidates.some((c) => c.devices.find((d) => d.id === devices[c.host.id]?.id)?.holds) && !wipeApproved) return;
      setPhase('running');
      setLog([]);
      setError('');
      await runSetup(req, (line) => setLog((cur) => [...cur, line]));
      setPhase('done');
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setPhase('error');
    }
  }

  const needsWipe = candidates.some((c) => c.devices.find((d) => d.id === devices[c.host.id]?.id)?.holds);
  const input = 'mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg';
  return (
    <section className="space-y-4 rounded-xl border border-line bg-card p-4" data-testid="cluster-setup">
      <h2 className="text-sm font-medium text-fg">{w.setupTitle}</h2>
      {phase === 'choose' && (
        <>
          <p className="text-xs text-muted">{w.setupHelp}</p>
          <ul className="space-y-2">
            {hosts.map((h) => (
              <li key={h.id} className="flex items-center gap-3 text-sm text-fg-soft">
                <input type="checkbox" checked={selected.includes(h.id)} onChange={() => toggle(h.id)}
                  aria-label={`${w.setupUse} ${h.name}`} disabled={!h.online} />
                <input type="radio" name="setup-leader" checked={leaderId === h.id} onChange={() => setLeaderId(h.id)}
                  disabled={!selected.includes(h.id)} aria-label={`${w.setupFirst} ${h.name}`} />
                <span className="font-medium text-fg">{h.name}</span>
                <span className="text-xs text-muted">{selected.includes(h.id) && leaderId === h.id ? w.setupFirst : h.url}</span>
              </li>
            ))}
          </ul>
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="text-sm text-fg-soft">{w.setupClusterName}
              <input name="cluster_name" value={clusterName} onChange={(e) => setClusterName(e.target.value)} className={input} />
            </label>
            <label className="text-sm text-fg-soft">{w.setupNbsName}
              <input name="nbs_name" value={nbsName} onChange={(e) => setNbsName(e.target.value)} className={input} />
            </label>
          </div>
          <button type="button" onClick={() => void review()} disabled={busy || selected.length < 3 || !leaderId}
            className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-60">
            {w.setupReview}
          </button>
        </>
      )}
      {phase === 'review' && (
        <>
          <ul className="space-y-3">
            {candidates.map((c) => {
              const choice = devices[c.host.id];
              const usable = c.devices.filter(usableDevice);
              const current = usable.find((d) => d.id === choice?.id);
              return (
                <li key={c.host.id} className="rounded-lg bg-bg-soft p-3 text-sm text-fg-soft" data-testid={`setup-${c.host.name}`}>
                  <div className="font-medium text-fg">{c.cluster.this_host?.name ?? c.host.name}
                    {c.host.id === leaderId ? ` — ${w.setupFirst}` : ''}
                  </div>
                  <div className="mt-1 text-xs text-muted">
                    {c.cluster.member ? `${w.setupClusterReady}: ${c.cluster.cluster}` : w.setupClusterFresh}
                    {' · '}{c.nbs.member ? `${w.setupNbsReady}: ${c.nbs.cluster}` : w.setupNbsFresh}
                  </div>
                  <label className="mt-2 block text-xs">{w.setupDevice}
                    <select name={`device-${c.cluster.this_host?.name ?? c.host.name}`} value={choice?.id ?? ''} className={input}
                      onChange={(e) => setDevices((cur) => ({ ...cur, [c.host.id]: { id: e.target.value, wipe: false } }))}>
                      <option value="">—</option>
                      {usable.map((d) => (
                        <option key={d.id} value={d.id!}>
                          {d.name} — {bytes(d.size_bytes)}{d.holds ? ` — ${d.holds}` : ru ? ' — пусто' : ' — blank'}
                        </option>
                      ))}
                    </select>
                  </label>
                  {current?.holds && (
                    <label className="mt-2 flex items-center gap-2 text-xs text-bad">
                      <input type="checkbox" checked={choice?.wipe ?? false}
                        onChange={(e) => setDevices((cur) => ({ ...cur, [c.host.id]: { id: current.id!, wipe: e.target.checked } }))} />
                      {w.setupWipe}: {current.holds}
                    </label>
                  )}
                </li>
              );
            })}
          </ul>
          {needsWipe && (
            <label className="flex items-center gap-2 text-sm text-bad">
              <input type="checkbox" checked={wipeApproved} onChange={(e) => setWipeApproved(e.target.checked)} />
              {w.setupWipeApproved}
            </label>
          )}
          <div className="flex gap-2">
            <button type="button" onClick={() => setPhase('choose')} className="rounded-lg px-3 py-1.5 text-sm text-muted hover:text-fg">
              {w.cancel}
            </button>
            <button type="button" onClick={() => void run()} disabled={needsWipe && !wipeApproved}
              className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-60">
              {w.setupRun}
            </button>
          </div>
        </>
      )}
      {(phase === 'running' || phase === 'done' || phase === 'error') && (
        <div className="space-y-3">
          <pre className="max-h-64 overflow-auto rounded-lg bg-bg-soft p-3 font-mono text-xs text-fg-soft" data-testid="setup-log">
            {log.join('\n')}
          </pre>
          {phase === 'done' && <p className="text-sm text-ok" data-testid="setup-done">{w.setupDone}</p>}
          {phase === 'error' && (
            <button type="button" onClick={() => void review()} className="rounded-lg bg-accent px-3 py-1.5 text-sm text-bg">
              {w.setupRetry}
            </button>
          )}
        </div>
      )}
      {error && <p role="alert" className="text-sm text-bad">{error}</p>}
    </section>
  );
}
