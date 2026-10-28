'use client';

import { useEffect, useMemo, useState } from 'react';

import { Chart } from './Chart';
import { bytes } from '@/lib/descriptor';
import type { Words } from '@/lib/i18n';

interface Sample { ms: number; cpu: number | null; mem: number | null; rx: number | null; tx: number | null }
interface LogLine { seq: number; level: number; text: string; ms: number }
interface Job { id: number; provider: string; method: string; state: string; status: string | null; done: number; total: number }

/** What the dashboard keeps: an hour, a second at a time. */
const KEEP = 3600;

export function Dashboard({ locale, host, name, w }: { locale: string; host: string; name: string; w: Words }) {
  const [samples, setSamples] = useState<Sample[]>([]);
  const [online, setOnline] = useState<boolean | null>(null);
  const [why, setWhy] = useState('');
  const [cpus, setCpus] = useState<number[]>([]);
  const [total, setTotal] = useState<number | null>(null);
  const [log, setLog] = useState<LogLine[]>([]);
  const [jobs, setJobs] = useState<Job[]>([]);

  useEffect(() => {
    let gone = false;
    fetch(`/api/h/${host}/history?since_ms=${Date.now() - KEEP * 1000}`).then((r) => r.json()).then((h) => {
      if (gone) return;
      const rows = (h.samples ?? []) as Array<Record<string, number | null>>;
      setSamples((now) => [...rows.map((r) => ({ ms: Number(r.ms), cpu: r.cpu_busy_permille ?? null,
        mem: r.mem_used_bytes ?? null, rx: r.rx_bytes_per_s ?? null, tx: r.tx_bytes_per_s ?? null })), ...now].slice(-KEEP));
    }).catch(() => {});
    const es = new EventSource(`/api/events/${host}`);
    es.onmessage = (m) => {
      const e = JSON.parse(m.data);
      if (e.type === 'state') {
        setOnline(e.online);
        setWhy(e.why || '');
      } else if (e.type === 'metrics') {
        setOnline(true);
        setCpus(e.cpus);
        setTotal(e.mem_total_bytes);
        setSamples((now) => [...now, { ms: e.ms, cpu: e.cpu_busy_permille, mem: e.mem_used_bytes, rx: e.rx_bytes_per_s,
          tx: e.tx_bytes_per_s }].slice(-KEEP));
      } else if (e.type === 'log') {
        setLog((now) => [...now, ...e.lines].slice(-50));
      } else if (e.type === 'jobs') {
        setJobs(e.jobs);
      }
    };
    return () => {
      gone = true;
      es.close();
    };
  }, [host]);

  const t = useMemo(() => samples.map((s) => s.ms / 1000), [samples]);
  const last = samples[samples.length - 1];
  const running = jobs.filter((j) => j.state !== 'done');

  return (
    <div className="space-y-6">
      <div className="flex items-baseline gap-3">
        <h1 className="text-2xl font-semibold text-fg">{name}</h1>
        <span className={`text-sm ${online ? 'text-ok' : 'text-bad'}`} data-testid="host-state">
          {online === null ? '…' : online ? w.online : `${w.offline}${why ? ' -- ' + why : ''}`}
        </span>
      </div>

      <div className="grid gap-4 md:grid-cols-3">
        <Tile title={w.cpu} value={last?.cpu != null ? (last.cpu / 10).toFixed(1) + ' %' : '—'} />
        <Tile title={w.memory} value={last?.mem != null ? bytes(last.mem) + (total ? ' / ' + bytes(total) : '') : '—'} />
        <Tile title={w.network} value={last?.rx != null ? `↓ ${bytes(last.rx)}/s  ↑ ${bytes(last.tx ?? 0)}/s` : '—'} />
      </div>

      <section className="rounded-xl border border-line bg-card p-4">
        <h2 className="mb-2 text-sm font-medium text-fg-soft">{w.cpu}</h2>
        <Chart data={[t, samples.map((s) => (s.cpu == null ? null : s.cpu / 10))]} series={[{ label: '%', color: 'var(--accent)' }]}
          format={(v) => v.toFixed(0) + ' %'} locale={locale} timeLabel={w.time} />
        <div className="mt-3 flex flex-wrap gap-1" aria-label="per CPU">
          {cpus.map((c, i) => (
            <div key={i} title={`cpu ${i}: ${(c / 10).toFixed(1)} %`} className="h-6 w-6 rounded"
              style={{ background: `color-mix(in oklab, var(--accent) ${Math.round(c / 10)}%, var(--bg-soft))` }} />
          ))}
        </div>
      </section>

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="rounded-xl border border-line bg-card p-4">
          <h2 className="mb-2 text-sm font-medium text-fg-soft">{w.memory}</h2>
          <Chart data={[t, samples.map((s) => s.mem)]} series={[{ label: w.memory, color: 'var(--info)' }]} format={bytes}
            locale={locale} timeLabel={w.time} />
        </section>
        <section className="rounded-xl border border-line bg-card p-4">
          <h2 className="mb-2 text-sm font-medium text-fg-soft">{w.network}</h2>
          <Chart data={[t, samples.map((s) => s.rx), samples.map((s) => s.tx)]}
            series={[{ label: w.received, color: 'var(--ok)' }, { label: w.sent, color: 'var(--accent)' }]}
            format={(v) => bytes(v) + '/s'} locale={locale} timeLabel={w.time} />
        </section>
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <section className="rounded-xl border border-line bg-card p-4">
          <h2 className="mb-2 text-sm font-medium text-fg-soft">{w.jobs} ({running.length} {w.running})</h2>
          {jobs.length === 0 ? <p className="text-sm text-muted">{w.nothingYet}</p> : (
            <ul className="space-y-1 text-sm">
              {jobs.slice(-8).reverse().map((j) => (
                <li key={j.id} className="flex justify-between gap-2">
                  <span className="font-mono">#{j.id} {j.provider}.{j.method}</span>
                  <span className={j.status === 'ok' ? 'text-ok' : j.status ? 'text-bad' : 'text-muted'}>
                    {j.status ?? (j.total ? `${j.done}/${j.total}` : j.state)}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </section>
        <section className="rounded-xl border border-line bg-card p-4">
          <h2 className="mb-2 text-sm font-medium text-fg-soft">{w.log}</h2>
          {log.length === 0 ? <p className="text-sm text-muted">{w.nothingYet}</p> : (
            <pre className="max-h-64 overflow-auto font-mono text-xs leading-5 text-fg-soft" data-testid="log">
              {log.slice(-30).map((l) => `${l.seq}  ${l.text}`).join('\n')}
            </pre>
          )}
        </section>
      </div>
    </div>
  );
}

function Tile({ title, value }: { title: string; value: string }) {
  return (
    <div className="rounded-xl border border-line bg-card p-4">
      <div className="text-sm text-muted">{title}</div>
      <div className="mt-1 font-mono text-lg text-fg">{value}</div>
    </div>
  );
}
