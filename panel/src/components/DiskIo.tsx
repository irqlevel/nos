'use client';

import { useEffect, useRef, useState } from 'react';

import { Chart } from './Chart';
import { call } from '@/lib/client';
import { bytes } from '@/lib/descriptor';
import type { Words } from '@/lib/i18n';

/** A disk's counters since its guest's boot, as the vm provider's
 * `disk_io` says them. */
interface Counts { file: string; reads: number; writes: number; read_bytes: number; written_bytes: number }
interface Io { running: boolean; restarts: number; at_ms: number; disks: Counts[] }

/** A disk's rates between two answers: IOPS and bytes a second, read and
 * written, at a moment (seconds, the wall clock's: the chart's time). */
interface Rate { t: number; ri: number; wi: number; rb: number; wb: number }

/** How often the counters are asked, and how much of them is kept: five
 * minutes. */
const EVERY_MS = 2000;
const KEEP = 150;

/** The rates of `now`'s disks against `before`'s: none for a disk whose
 * counters went back -- its guest booted again -- or that was not there. */
function rates(before: Io, now: Io, t: number): Record<string, Rate> {
  const out: Record<string, Rate> = {};
  const secs = (now.at_ms - before.at_ms) / 1000;
  if (secs <= 0 || before.restarts !== now.restarts) return out;
  for (const d of now.disks) {
    const p = before.disks.find((x) => x.file === d.file);
    if (!p || d.reads < p.reads || d.writes < p.writes || d.read_bytes < p.read_bytes
      || d.written_bytes < p.written_bytes) continue;
    out[d.file] = { t, ri: (d.reads - p.reads) / secs, wi: (d.writes - p.writes) / secs,
      rb: (d.read_bytes - p.read_bytes) / secs, wb: (d.written_bytes - p.written_bytes) / secs };
  }
  return out;
}

/** What a running VM's disks are doing: each disk's IOPS and bandwidth, read
 * and written, charted as the counters are asked every two seconds -- a
 * volume of NBS's among them as any disk. */
export function DiskIo({ host, name, locale, w }: { host: string; name: string; locale: string; w: Words }) {
  const [series, setSeries] = useState<Record<string, Rate[]>>({});
  const [running, setRunning] = useState<boolean | null>(null);
  const last = useRef<Io | null>(null);

  useEffect(() => {
    let live = true;
    last.current = null;
    setSeries({});
    const tick = async () => {
      const r = await call(host, 'vm', 'disk_io', { name });
      if (!live) return;
      const io = r.ok ? (r.data as Io) : null;
      setRunning(io?.running ?? false);
      const before = last.current;
      last.current = io?.running ? io : null;
      if (!before || !io?.running) return;
      const got = rates(before, io, Date.now() / 1000);
      setSeries((now) => {
        const next: Record<string, Rate[]> = {};
        for (const d of io.disks) {
          const kept = now[d.file] ?? [];
          const r1 = got[d.file];
          next[d.file] = r1 ? [...kept, r1].slice(-KEEP) : kept;
        }
        return next;
      });
    };
    void tick();
    const timer = setInterval(() => void tick(), EVERY_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [host, name]);

  if (running === false) {
    return <p className="rounded-xl border border-line bg-card px-4 py-3 text-sm text-muted" data-testid="disk-io">{w.notRunning}</p>;
  }
  const files = Object.keys(series);
  const iops = (v: number) => Math.round(v) + ' IOPS';
  const rate = (v: number) => bytes(Math.round(v)) + '/s';
  return (
    <section className="space-y-3" data-testid="disk-io">
      <h2 className="text-sm font-medium text-fg">{w.diskIo}</h2>
      {files.length === 0 && <p className="text-sm text-muted">…</p>}
      {files.map((f) => {
        const rs = series[f]!;
        const now = rs[rs.length - 1];
        const t = rs.map((r) => r.t);
        return (
          <div key={f} className="rounded-xl border border-line bg-card p-4" data-testid={`disk-io-${f}`}>
            <div className="flex flex-wrap items-baseline justify-between gap-2">
              <span className="font-mono text-sm text-fg">
                {f}{f.startsWith('nbs:') && <span className="ml-2 rounded bg-accent-soft px-1.5 py-0.5 text-xs">NBS</span>}
              </span>
              {now && (
                <span className="font-mono text-xs text-fg-soft" data-testid="disk-io-now">
                  {w.reads} {iops(now.ri)} · {rate(now.rb)} &nbsp; {w.writes} {iops(now.wi)} · {rate(now.wb)}
                </span>
              )}
            </div>
            {rs.length > 1 && (
              <div className="mt-2 grid gap-4 lg:grid-cols-2">
                <div>
                  <h3 className="mb-1 text-xs text-muted">IOPS</h3>
                  <Chart data={[t, rs.map((r) => r.ri), rs.map((r) => r.wi)]}
                    series={[{ label: w.reads, color: 'var(--ok)' }, { label: w.writes, color: 'var(--accent)' }]}
                    format={(v) => v.toFixed(0)} locale={locale} timeLabel={w.time} height={120} />
                </div>
                <div>
                  <h3 className="mb-1 text-xs text-muted">{w.bandwidth}</h3>
                  <Chart data={[t, rs.map((r) => r.rb), rs.map((r) => r.wb)]}
                    series={[{ label: w.reads, color: 'var(--ok)' }, { label: w.writes, color: 'var(--accent)' }]}
                    format={rate} locale={locale} timeLabel={w.time} height={120} />
                </div>
              </div>
            )}
          </div>
        );
      })}
    </section>
  );
}
