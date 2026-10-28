'use client';

import Link from 'next/link';
import { useCallback, useEffect, useState } from 'react';
import type { Words } from '@/lib/i18n';

interface HostRow { id: string; name: string; url: string; pin: string; online: boolean; why: string | null }

/** The hosts the panel manages, whether each answers -- and why not, when
 * it does not -- and another added: its API's address and its key, which
 * `api status` prints on it; and there, the panel's key let in, by a name
 * of its own: `panel` is that host's own panel's, if it has one. */
export function HostsManager({ locale, w }: { locale: string; w: Words }) {
  const [list, setList] = useState<HostRow[]>([]);
  const [panelKey, setPanelKey] = useState<string | null>(null);
  const [name, setName] = useState('');
  const [url, setUrl] = useState('');
  const [pin, setPin] = useState('');
  const [error, setError] = useState('');

  const load = useCallback(async () => {
    const r = await fetch('/api/hosts');
    if (r.ok) {
      const b = await r.json();
      setList(b.hosts);
      setPanelKey(b.panel_key);
    }
  }, []);
  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), 5000);
    return () => clearInterval(t);
  }, [load]);

  async function add(e: React.FormEvent) {
    e.preventDefault();
    const r = await fetch('/api/hosts', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name, url, pin }) });
    if (r.ok) {
      setName('');
      setUrl('');
      setPin('');
      setError('');
      void load();
    } else {
      setError((await r.json()).error?.message ?? String(r.status));
    }
  }

  async function remove(id: string) {
    await fetch('/api/hosts?id=' + encodeURIComponent(id), { method: 'DELETE' });
    void load();
  }

  const input = 'mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg';
  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold text-fg">{w.hosts}</h1>
      {list.length === 0 && <p className="text-sm text-muted">{w.noHosts}</p>}
      <ul className="space-y-2">
        {list.map((h) => (
          <li key={h.id} data-testid={`host-${h.name}`}
            className="flex items-center justify-between gap-4 rounded-xl border border-line bg-card px-4 py-3 text-sm">
            <div className="min-w-0">
              <Link href={`/${locale}/h/${h.id}`} className="font-medium text-accent hover:underline">{h.name}</Link>
              <span className="ml-2 font-mono text-xs text-muted">{h.url}</span>
              <div className="font-mono text-xs text-muted">{h.pin.slice(0, 16)}…</div>
              {!h.online && h.why && <div className="mt-1 break-words text-xs text-bad">{h.why}</div>}
            </div>
            <div className="flex shrink-0 items-center gap-3">
              <span className={h.online ? 'text-ok' : 'text-bad'}>{h.online ? w.online : w.offline}</span>
              <button type="button" onClick={() => void remove(h.id)} className="text-xs text-muted hover:text-bad">{w.remove}</button>
            </div>
          </li>
        ))}
      </ul>
      <form onSubmit={add} className="space-y-3 rounded-xl border border-line bg-card p-4">
        <h2 className="text-sm font-medium text-fg">{w.addHost}</h2>
        <label className="block text-sm text-fg-soft">{w.name}<input name="name" value={name} onChange={(e) => setName(e.target.value)} className={input} /></label>
        <label className="block text-sm text-fg-soft">{w.hostAddress}<input name="url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="203.0.113.5:39234" className={input} /></label>
        <label className="block text-sm text-fg-soft">{w.hostKey}<input name="pin" value={pin} onChange={(e) => setPin(e.target.value)} className={input + ' font-mono text-xs'} /></label>
        {panelKey && (
          <p className="text-xs text-muted" data-testid="panel-key">{w.panelKey}{' '}
            <code className="break-all rounded bg-bg-soft px-1 font-mono text-fg">
              api client add panel-{panelKey.slice(0, 8)} admin {panelKey}
            </code>
          </p>
        )}
        {error && <p role="alert" className="text-sm text-bad">{error}</p>}
        <button type="submit" className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong">{w.addHost}</button>
      </form>
    </div>
  );
}
