'use client';

import { useState } from 'react';

export function LoginForm({ locale, w }: { locale: string;
  w: { user: string; password: string; login: string; wrong: string; tooMany: string } }) {
  const [user, setUser] = useState('root');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError('');
    const res = await fetch('/api/login', { method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ user, password }) });
    setBusy(false);
    if (res.ok) {
      window.location.assign(`/${locale}`);
    } else {
      setError(res.status === 429 ? w.tooMany : w.wrong);
    }
  }

  return (
    <form onSubmit={submit} className="space-y-4">
      <label className="block text-sm text-fg-soft">
        {w.user}
        <input value={user} onChange={(e) => setUser(e.target.value)} autoComplete="username"
          className="mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg" />
      </label>
      <label className="block text-sm text-fg-soft">
        {w.password}
        <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus
          autoComplete="current-password" className="mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg" />
      </label>
      {error && <p role="alert" className="text-sm text-bad">{error}</p>}
      <button type="submit" disabled={busy}
        className="w-full rounded-lg bg-accent px-3 py-2 font-medium text-bg transition-colors hover:bg-accent-strong disabled:opacity-60">
        {w.login}
      </button>
    </form>
  );
}
