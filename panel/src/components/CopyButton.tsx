'use client';

import { useEffect, useRef, useState } from 'react';

import type { Words } from '@/lib/i18n';

/** `text` put on the clipboard: by the Clipboard API where the page may use
 * it, else -- a page it is refused to, or a browser without it -- by a
 * selection copied the old way. Whether it got there. */
async function copyText(text: string): Promise<boolean> {
  try {
    if (window.isSecureContext && navigator.clipboard) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    /* refused: the old way, below */
  }
  const area = document.createElement('textarea');
  area.value = text;
  area.setAttribute('readonly', '');
  area.style.position = 'fixed';
  area.style.top = '0';
  area.style.opacity = '0';
  document.body.appendChild(area);
  area.select();
  let ok = false;
  try {
    ok = document.execCommand('copy');
  } catch {
    ok = false;
  }
  area.remove();
  return ok;
}

const SAID_MS = 2000;

/** A button that copies `value`, saying for a moment that it did -- or that
 * it could not, the value then to be selected by hand. */
export function CopyButton({ value, label, w }: { value: string; label: string; w: Words }) {
  const [said, setSaid] = useState<'copied' | 'failed' | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => {
    if (timer.current) clearTimeout(timer.current);
  }, []);

  async function copy() {
    const ok = await copyText(value);
    setSaid(ok ? 'copied' : 'failed');
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setSaid(null), SAID_MS);
  }

  return (
    <button type="button" onClick={() => void copy()} aria-label={label} title={value}
      className={'inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-xs hover:bg-bg-soft '
        + (said === 'failed' ? 'text-bad' : said === 'copied' ? 'text-ok' : 'text-fg-soft')}>
      <svg aria-hidden viewBox="0 0 16 16" className="h-3.5 w-3.5" fill="none" stroke="currentColor" strokeWidth="1.5">
        <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" />
        <path d="M10.5 5.5v-2a1.5 1.5 0 0 0-1.5-1.5H4A1.5 1.5 0 0 0 2.5 3.5V9A1.5 1.5 0 0 0 4 10.5h1.5" />
      </svg>
      <span>{said === 'copied' ? w.copied : said === 'failed' ? w.copyFailed : label}</span>
    </button>
  );
}
