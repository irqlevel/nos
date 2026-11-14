'use client';

import { useEffect, useRef, useState } from 'react';

import { Chart } from './Chart';
import { Shown, ToneDot } from './ToneDot';
import { call } from '@/lib/client';
import { bytes, Descriptor, Tone, TONE_COLOR, toneOf, worst } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';

/** A disk's counters since its guest's boot, as the vm provider's
 * `disk_io` says them. */
interface Counts {
  file: string; reads: number; writes: number; read_bytes: number; written_bytes: number; errors: number;
}
interface Io { running: boolean; restarts: number; at_ms: number; disks: Counts[] }

/** A disk's rates between two answers: IOPS and bytes a second, read and
 * written, at a moment (seconds, the wall clock's: the chart's time). */
interface Rate { t: number; ri: number; wi: number; rb: number; wb: number }

/** A disk's errors counter as it was asked, by the wall clock's seconds. */
interface Errs { t: number; errors: number }

/** How often the counters are asked, and how much of them is kept: five
 * minutes; how often a volume's health; what errors make a disk red. */
const EVERY_MS = 2000;
const KEEP = 150;
const HEALTH_EVERY_MS = 10_000;
const ERRORS_SECS = 60;

/** What NBS says of a volume (the nbs provider's `volume`): its health in a
 * word and the counts behind it. */
type Health = Record<string, unknown> & { health?: string };

/** The counts behind a volume's health, as its descriptor names them. */
const HEALTH_COUNTS = ['chunks_unreachable', 'chunks_unlike', 'chunks_at_one', 'chunks_at_two', 'chunks_copying'];

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

/** The errors a disk had in the last ERRORS_SECS: its counter now against
 * where it was then -- 0 where it went back, its guest booted again. */
function recentErrors(log: Errs[]): number {
  const last = log[log.length - 1];
  if (!last) return 0;
  const from = log.find((e) => e.t >= last.t - ERRORS_SECS) ?? last;
  return Math.max(0, last.errors - from.errors);
}

/** What a running VM's disks are doing: each disk's IOPS and bandwidth, read
 * and written, charted as the counters are asked every two seconds -- and
 * how it is, its edge coloured by the worse of the two: red while its I/O
 * fails, and for a volume of NBS's what NBS says of its replicas, in the
 * tone NBS's descriptor gives it (a replica down or one copied onto,
 * yellow; one replica left, orange; a chunk with none, or replicas unlike,
 * red); green when both are well. */
export function DiskIo({ host, name, nbs = null, locale, w }: {
  host: string; name: string; nbs?: Descriptor | null; locale: Locale; w: Words;
}) {
  const [series, setSeries] = useState<Record<string, Rate[]>>({});
  const [errs, setErrs] = useState<Record<string, Errs[]>>({});
  const [running, setRunning] = useState<boolean | null>(null);
  const [health, setHealth] = useState<Record<string, Health | null>>({});
  const last = useRef<Io | null>(null);

  useEffect(() => {
    let live = true;
    last.current = null;
    setSeries({});
    setErrs({});
    const tick = async () => {
      const r = await call(host, 'vm', 'disk_io', { name });
      if (!live) return;
      const io = r.ok ? (r.data as Io) : null;
      setRunning(io?.running ?? false);
      const before = last.current;
      last.current = io?.running ? io : null;
      if (!io?.running) return;
      const t = Date.now() / 1000;
      setErrs((now) => {
        const next: Record<string, Errs[]> = {};
        for (const d of io.disks) {
          const kept = before && before.restarts !== io.restarts ? [] : now[d.file] ?? [];
          next[d.file] = [...kept, { t, errors: d.errors }].slice(-KEEP);
        }
        return next;
      });
      if (!before) return;
      const got = rates(before, io, t);
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

  /* Each volume of NBS's among them: how NBS says it is, asked again every
   * ten seconds -- null where NBS does not answer for it */
  const volumes = Object.keys(errs).filter((f) => f.startsWith('nbs:')).join(' ');
  useEffect(() => {
    if (!volumes) return;
    let live = true;
    const look = async () => {
      const got: Record<string, Health | null> = {};
      for (const f of volumes.split(' ')) {
        const r = await call(host, 'nbs', 'volume', { name: f.slice('nbs:'.length) });
        got[f] = r.ok ? (r.data as Health) : null;
      }
      if (live) setHealth(got);
    };
    void look();
    const timer = setInterval(() => void look(), HEALTH_EVERY_MS);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [host, volumes]);

  if (running === false) {
    return <p className="rounded-xl border border-line bg-card px-4 py-3 text-sm text-muted" data-testid="disk-io">{w.notRunning}</p>;
  }
  const files = Object.keys(errs);
  const iops = (v: number) => Math.round(v) + ' IOPS';
  const rate = (v: number) => bytes(Math.round(v)) + '/s';
  const fields = nbs?.resources?.volumes?.fields ?? {};
  return (
    <section className="space-y-3" data-testid="disk-io">
      <h2 className="text-sm font-medium text-fg">{w.diskIo}</h2>
      {files.length === 0 && <p className="text-sm text-muted">…</p>}
      {files.map((f) => {
        const rs = series[f] ?? [];
        const now = rs[rs.length - 1];
        const t = rs.map((r) => r.t);
        const log = errs[f] ?? [];
        const recent = recentErrors(log);
        const ever = log[log.length - 1]?.errors ?? 0;
        const vol = f.startsWith('nbs:') ? health[f] : undefined;
        const volTone = vol ? toneOf(fields.health, vol.health) : undefined;
        const tone: Tone = worst(recent > 0 ? 'bad' : 'good', volTone ?? 'good');
        const counts = vol ? HEALTH_COUNTS.filter((k) => typeof vol[k] === 'number' && (vol[k] as number) > 0) : [];
        return (
          <div key={f} className="rounded-xl border border-line bg-card p-4" data-testid={`disk-io-${f}`} data-tone={tone}
            style={{ borderLeft: `4px solid ${TONE_COLOR[tone]}` }}>
            <div className="flex flex-wrap items-baseline justify-between gap-2">
              <span className="inline-flex items-center gap-2 font-mono text-sm text-fg">
                <ToneDot tone={tone} />
                {f}{f.startsWith('nbs:') && <span className="rounded bg-accent-soft px-1.5 py-0.5 text-xs">NBS</span>}
              </span>
              {now && (
                <span className="font-mono text-xs text-fg-soft" data-testid="disk-io-now">
                  {w.reads} {iops(now.ri)} · {rate(now.rb)} &nbsp; {w.writes} {iops(now.wi)} · {rate(now.wb)}
                </span>
              )}
            </div>
            <div className="mt-1 flex flex-wrap gap-x-4 gap-y-1 text-xs text-fg-soft" data-testid="disk-health">
              {recent > 0 ? (
                <span className="text-bad">{recent} {w.ioErrors}</span>
              ) : ever > 0 ? (
                <span>{ever} {w.errorsSinceBoot}</span>
              ) : (
                <span>{w.noIoErrors}</span>
              )}
              {vol && <Shown f={fields.health} v={vol.health} locale={locale} />}
              {counts.map((k) => <span key={k}>{text(fields[k]?.title, locale) || k}: {String(vol![k])}</span>)}
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
