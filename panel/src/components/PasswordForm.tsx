'use client';

import { useState } from 'react';
import type { Words } from '@/lib/i18n';

export function PasswordForm({ w }: { w: Words }) {
  const [old, setOld] = useState('');
  const [now, setNow] = useState('');
  const [said, setSaid] = useState('');
  async function submit(e: React.FormEvent) {
    e.preventDefault();
    const r = await fetch('/api/password', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ old, new: now }) });
    setSaid(r.ok ? w.passwordChanged : (await r.json()).error?.message ?? String(r.status));
    if (r.ok) {
      setOld('');
      setNow('');
    }
  }
  const input = 'mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg';
  return (
    <form onSubmit={submit} className="space-y-3 rounded-xl border border-line bg-card p-4">
      <h2 className="text-sm font-medium text-fg">{w.changePassword}</h2>
      <label className="block text-sm text-fg-soft">{w.password}
        <input type="password" value={old} onChange={(e) => setOld(e.target.value)} autoComplete="current-password" className={input} />
      </label>
      <label className="block text-sm text-fg-soft">{w.newPassword}
        <input type="password" value={now} onChange={(e) => setNow(e.target.value)} autoComplete="new-password" className={input} />
      </label>
      {said && <p role="status" className="text-sm text-fg-soft">{said}</p>}
      <button type="submit" className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong">{w.save}</button>
    </form>
  );
}
