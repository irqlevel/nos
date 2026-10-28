'use client';

import { useState } from 'react';

import { JobWatch } from './JobWatch';
import { call, Refusal, startJob } from '@/lib/client';
import { Field, Method } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';


/** A size typed with its unit, as the descriptor's bytes are. */
const UNITS: Array<[string, number]> = [['B', 1], ['KiB', 1024], ['MiB', 1024 ** 2], ['GiB', 1024 ** 3], ['TiB', 1024 ** 4]];

/** What a field's input says, made the value a host takes -- or why not. */
function valueOf(f: Field, raw: string, unit: number): { ok: true; v: unknown } | { ok: false; why: string } {
  if (raw === '') {
    return { ok: true, v: undefined };
  }
  switch (f.type) {
    case 'int':
    case 'port':
    case 'duration_ms':
    case 'duration_ns':
    case 'time_s':
    case 'bytes': {
      const n = Number(raw) * (f.type === 'bytes' ? unit : 1);
      if (!Number.isFinite(n) || !Number.isInteger(n)) return { ok: false, why: 'a whole number' };
      if (f.min !== undefined && n < f.min) return { ok: false, why: '≥ ' + f.min };
      if (f.max !== undefined && n > f.max) return { ok: false, why: '≤ ' + f.max };
      return { ok: true, v: n };
    }
    case 'bool':
      return { ok: true, v: raw === 'true' };
    case 'list':
      return { ok: true, v: raw.split(',').map((s) => s.trim()).filter(Boolean) };
    default:
      return { ok: true, v: raw };
  }
}

/** A method's form: its parameters from its descriptor, those the caller
 * fixed (`fixed`, a row's key) given and not asked; its confirm asked
 * before it runs -- with the value of `confirm_typed`'s parameter typed
 * again, where it has one; a job followed to its end. */
export function MethodForm({ host, provider, name, method, fixed = {}, locale, w, onDone }: {
  host: string; provider: string; name: string; method: Method; fixed?: Record<string, unknown>;
  locale: Locale; w: Words; onDone?: () => void;
}) {
  const params = Object.entries(method.params ?? {}).filter(([k]) => !(k in fixed));
  const [raw, setRaw] = useState<Record<string, string>>(() =>
    Object.fromEntries(params.map(([k, f]) => [k, f.default === undefined || f.default === null ? '' : String(f.default)])));
  const [units, setUnits] = useState<Record<string, number>>(() =>
    Object.fromEntries(params.filter(([, f]) => f.type === 'bytes').map(([k]) => [k, 1024 ** 3])));
  const [error, setError] = useState<Refusal | null>(null);
  const [answer, setAnswer] = useState<unknown>(null);
  const [job, setJob] = useState<number | null>(null);
  const [asking, setAsking] = useState(false);
  const [typed, setTyped] = useState('');
  const [busy, setBusy] = useState(false);
  /* What the user types to go on, when the method asks: the parameter's
   * value, as given or fixed */
  const mustType = method.confirm_typed
    ? String(method.confirm_typed in fixed ? fixed[method.confirm_typed] ?? '' : raw[method.confirm_typed] ?? '')
    : null;

  async function run() {
    const body: Record<string, unknown> = { ...fixed };
    for (const [k, f] of params) {
      const got = valueOf(f, raw[k] ?? '', units[k] ?? 1);
      if (!got.ok) {
        setError({ code: 'invalid', message: got.why, field: k });
        return;
      }
      if (got.v !== undefined) body[k] = got.v;
      else if (f.required) {
        setError({ code: 'invalid', message: w.required, field: k });
        return;
      }
    }
    setBusy(true);
    setError(null);
    setAnswer(null);
    if (method.kind === 'job') {
      const r = await startJob(host, provider, name, body);
      if (r.ok) setJob(r.id);
      else setError(r.error);
    } else {
      const r = await call(host, provider, name, body);
      if (r.ok) {
        setAnswer(r.data);
        onDone?.();
      } else setError(r.error);
    }
    setBusy(false);
  }

  function submit(e: React.FormEvent) {
    e.preventDefault();
    if (method.confirm && !asking) {
      setTyped('');
      setAsking(true);
      return;
    }
    if (mustType !== null && (mustType === '' || typed !== mustType)) {
      return;
    }
    setAsking(false);
    void run();
  }

  return (
    <form onSubmit={submit} className="space-y-3" data-testid={`form-${provider}-${name}`}>
      {params.map(([k, f]) => (
        <label key={k} className="block text-sm text-fg-soft">
          <span>{text(f.title, locale)}{f.required ? ' *' : ''}</span>
          {f.help && <span className="block text-xs text-muted">{text(f.help, locale)}</span>}
          <div className="mt-1 flex gap-2">
            {f.type === 'bool' ? (
              <select value={raw[k]} onChange={(e) => setRaw({ ...raw, [k]: e.target.value })} name={k}
                className="rounded-lg border border-line bg-bg px-2 py-1.5 text-fg">
                <option value="">—</option>
                <option value="true">{locale === 'ru' ? 'да' : 'yes'}</option>
                <option value="false">{locale === 'ru' ? 'нет' : 'no'}</option>
              </select>
            ) : f.type === 'enum' ? (
              <select value={raw[k]} onChange={(e) => setRaw({ ...raw, [k]: e.target.value })} name={k}
                className="rounded-lg border border-line bg-bg px-2 py-1.5 text-fg">
                <option value="">—</option>
                {Object.entries(f.values ?? {}).map(([v, t]) => <option key={v} value={v}>{text(t, locale)}</option>)}
              </select>
            ) : f.type === 'text' ? (
              <textarea value={raw[k]} onChange={(e) => setRaw({ ...raw, [k]: e.target.value })} name={k} rows={8}
                className="w-full rounded-lg border border-line bg-bg px-2 py-1.5 font-mono text-xs text-fg" />
            ) : (
              <input value={raw[k]} onChange={(e) => setRaw({ ...raw, [k]: e.target.value })} name={k}
                inputMode={['int', 'port', 'bytes'].includes(f.type) ? 'numeric' : undefined}
                className="w-full rounded-lg border border-line bg-bg px-2 py-1.5 text-fg" />
            )}
            {f.type === 'bytes' && (
              <select aria-label="unit" value={units[k]} onChange={(e) => setUnits({ ...units, [k]: Number(e.target.value) })}
                className="rounded-lg border border-line bg-bg px-2 py-1.5 text-fg">
                {UNITS.map(([u, n]) => <option key={u} value={n}>{u}</option>)}
              </select>
            )}
          </div>
          {error?.field === k && <span className="text-xs text-bad">{error.message}</span>}
        </label>
      ))}
      {asking && method.confirm && <p className="rounded-lg bg-accent-soft px-3 py-2 text-sm text-fg">{text(method.confirm, locale)}</p>}
      {asking && mustType !== null && (
        <label className="block text-sm text-fg-soft">
          <span className="font-mono text-xs text-fg">{mustType}</span>
          <input value={typed} onChange={(e) => setTyped(e.target.value)} name="confirm_typed" autoComplete="off"
            aria-label={w.typeToConfirm} placeholder={w.typeToConfirm}
            className="mt-1 w-full rounded-lg border border-line bg-bg px-2 py-1.5 font-mono text-fg" />
        </label>
      )}
      <div className="flex gap-2">
        <button type="submit" disabled={busy || job !== null || (asking && mustType !== null && (mustType === '' || typed !== mustType))}
          className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-60">
          {asking ? w.confirm : text(method.title, locale) || w.run}
        </button>
        {asking && (
          <button type="button" onClick={() => setAsking(false)} className="rounded-lg px-3 py-1.5 text-sm text-muted hover:text-fg">
            {w.cancel}
          </button>
        )}
      </div>
      {error && !error.field && <p role="alert" className="text-sm text-bad">{error.message}</p>}
      {answer !== null && (
        <pre className="max-h-64 overflow-auto rounded-lg bg-bg-soft p-2 font-mono text-xs text-fg-soft" data-testid="answer">
          {JSON.stringify(answer, null, 2)}
        </pre>
      )}
      {job !== null && <JobWatch host={host} id={job} w={w} onDone={() => onDone?.()} />}
    </form>
  );
}
