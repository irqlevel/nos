'use client';

import { themeStorageKey } from '@/lib/theme';

export function ThemeToggle({ label }: { label: string }) {
  function toggle() {
    const classes = document.documentElement.classList;
    const dark = !classes.contains('dark');
    classes.toggle('dark', dark);
    classes.toggle('light', !dark);
    try {
      localStorage.setItem(themeStorageKey, dark ? 'dark' : 'light');
    } catch {
      /* Storage refused: the choice lasts this page only */
    }
  }
  return (
    <button type="button" onClick={toggle} aria-label={label} title={label}
      className="inline-flex size-8 items-center justify-center rounded-lg text-muted transition-colors hover:bg-bg-soft hover:text-fg">
      <span className="hidden dark:inline">☀</span>
      <span className="inline dark:hidden">☾</span>
    </button>
  );
}
