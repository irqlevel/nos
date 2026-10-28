'use client';

export function LogoutButton({ locale, label }: { locale: string; label: string }) {
  async function out() {
    await fetch('/api/logout', { method: 'POST' });
    window.location.assign(`/${locale}/login`);
  }
  return (
    <button type="button" onClick={out} className="rounded-lg px-2 py-1 text-muted hover:bg-bg-soft hover:text-fg">
      {label}
    </button>
  );
}
