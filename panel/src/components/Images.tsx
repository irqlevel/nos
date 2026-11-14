'use client';

import Link from 'next/link';
import { useCallback, useEffect, useState } from 'react';

import { JobWatch } from './JobWatch';
import { MethodForm } from './MethodForm';
import { call, startJob } from '@/lib/client';
import { bytes, Descriptor } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';
import { CLOUD_IMAGES, NbsVolume, VOLUME_NAME } from '@/lib/images';

/** The images and snapshots of NBS's VMs are made of -- each a click from a
 * VM of a clone of it, or gone with its name typed -- and a new image made
 * of a cloud's image by its URL, a job of the vm provider's. */
export function Images({ host, vm, nbs, locale, w }: {
  host: string; vm: Descriptor; nbs: Descriptor | null; locale: Locale; w: Words;
}) {
  const [rows, setRows] = useState<NbsVolume[] | null>(null);
  const [error, setError] = useState('');
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(async () => {
    const r = await call(host, 'nbs', 'volumes');
    if (r.ok) {
      setRows(((r.data as { volumes?: NbsVolume[] }).volumes ?? []).filter((v) => v.kind !== 'volume'));
      setError('');
    } else {
      setRows(null);
      setError(r.error.code === 'no_provider' ? w.noNbs : r.error.message);
    }
  }, [host, w.noNbs]);

  useEffect(() => {
    void load();
  }, [load]);

  const del = nbs?.methods.volume_delete;
  const base = `/${locale}/h/${host}`;
  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold text-fg">{w.images}</h1>
      {error && <p role="alert" className="rounded-lg border border-line bg-card px-4 py-3 text-sm text-bad">{error}</p>}
      {rows !== null && (
        <section className="rounded-xl border border-line bg-card" data-testid="images">
          <div className="flex items-center justify-between border-b border-line px-4 py-2">
            <h2 className="text-sm font-medium text-fg">{w.images} · {w.snapshots}</h2>
            <button type="button" onClick={() => void load()} className="text-xs text-muted hover:text-fg">{w.refresh}</button>
          </div>
          {rows.length === 0 ? <p className="px-4 py-3 text-sm text-muted">{w.nothingYet}</p> : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm">
                <thead>
                  <tr className="text-left text-xs text-muted">
                    {[w.name, w.kind, w.size, w.madeOf, w.actions].map((h) => (
                      <th key={h} className="px-4 py-2 font-medium whitespace-nowrap">{h}</th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {rows.map((v) => [
                    <tr key={v.name} className="border-t border-line">
                      <td className="px-4 py-2 font-mono text-xs text-fg whitespace-nowrap">{v.name}</td>
                      <td className="px-4 py-2 text-xs text-fg-soft whitespace-nowrap">{v.kind === 'image' ? w.image : w.snapshot}</td>
                      <td className="px-4 py-2 font-mono text-xs text-fg-soft whitespace-nowrap">{bytes(v.size)}</td>
                      <td className="px-4 py-2 font-mono text-xs text-fg-soft whitespace-nowrap">{v.origin ?? '—'}</td>
                      <td className="space-x-2 px-4 py-2 whitespace-nowrap">
                        <Link href={`${base}/vms/new?image=${encodeURIComponent(v.name)}`}
                          className="rounded px-2 py-0.5 text-xs text-accent hover:bg-accent-soft">{w.createVm}</Link>
                        {del && (
                          <button type="button" onClick={() => setOpen(open === v.name ? null : v.name)}
                            className="rounded px-2 py-0.5 text-xs text-accent hover:bg-accent-soft">
                            {text(del.title, locale)}
                          </button>
                        )}
                      </td>
                    </tr>,
                    open === v.name && del ? (
                      <tr key={v.name + '/delete'} className="bg-bg-soft">
                        <td colSpan={5} className="px-4 py-3">
                          <MethodForm host={host} provider="nbs" name="volume_delete" method={del} fixed={{ name: v.name }}
                            locale={locale} w={w} onDone={() => void load()} />
                        </td>
                      </tr>
                    ) : null,
                  ])}
                </tbody>
              </table>
            </div>
          )}
        </section>
      )}
      <NewImage host={host} vm={vm} locale={locale} w={w} onDone={() => void load()} />
    </div>
  );
}

/** An image of NBS's made of a cloud's image by its URL -- or of a file of
 * nos's -- as `hv image` makes one, followed as it is downloaded and poured. */
function NewImage({ host, vm, locale, w, onDone }: {
  host: string; vm: Descriptor; locale: Locale; w: Words; onDone: () => void;
}) {
  const ru = locale === 'ru';
  const [name, setName] = useState('');
  const [from, setFrom] = useState(CLOUD_IMAGES[0]!.url);
  const [error, setError] = useState('');
  const [job, setJob] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const m = vm.methods.image;
  if (!m) return null;

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!VOLUME_NAME.test(name)) {
      setError(ru ? 'Имя: строчные буквы, цифры, . _ и -, буква или цифра первой, до 63.' :
        'A name: lowercase letters, digits, . _ and -, a letter or a digit first, at most 63.');
      return;
    }
    setError('');
    setBusy(true);
    const r = await startJob(host, 'vm', 'image', { name, from });
    if (r.ok) {
      setJob(r.id);
    } else {
      setBusy(false);
      setError(r.error.message);
    }
  }

  const input = 'mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg';
  return (
    <section className="max-w-xl space-y-3 rounded-xl border border-line bg-card p-4" data-testid="new-image">
      <h2 className="text-sm font-medium text-fg">{w.newImage}</h2>
      {m.help && <p className="text-xs text-muted">{text(m.help, locale)}</p>}
      <form onSubmit={submit} className="space-y-3">
        <label className="block text-sm text-fg-soft">{w.name}
          <input value={name} onChange={(e) => setName(e.target.value)} name="name" className={input + ' font-mono'} />
        </label>
        <label className="block text-sm text-fg-soft">{text(m.params?.from?.title, locale) || w.imageUrl}
          <select value={CLOUD_IMAGES.some((i) => i.url === from) ? from : ''}
            onChange={(e) => e.target.value && setFrom(e.target.value)} className={input}>
            {CLOUD_IMAGES.map((i) => <option key={i.url} value={i.url}>{i.label}</option>)}
            <option value="">…</option>
          </select>
          <input value={from} onChange={(e) => setFrom(e.target.value)} name="from" className={input + ' font-mono text-xs'} />
        </label>
        {error && <p role="alert" className="text-sm text-bad">{error}</p>}
        <button type="submit" disabled={busy}
          className="rounded-lg bg-accent px-3 py-2 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-60">
          {w.newImage}
        </button>
      </form>
      {job !== null && (
        <JobWatch key={job} host={host} id={job} w={w} onDone={() => {
          setBusy(false);
          onDone();
        }} />
      )}
    </section>
  );
}
