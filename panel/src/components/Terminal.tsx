'use client';

import '@xterm/xterm/css/xterm.css';

import { FitAddon } from '@xterm/addon-fit';
import { Terminal as Xterm } from '@xterm/xterm';
import { useEffect, useRef, useState } from 'react';
import type { Words } from '@/lib/i18n';

/** A VM's console, live: what it prints written as it comes -- a gap said
 * where the host's ring lost some -- and what is typed sent, a few keys a
 * post. */
export function Terminal({ host, vm, w }: { host: string; vm: string; w: Words }) {
  const box = useRef<HTMLDivElement>(null);
  const [note, setNote] = useState('');

  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const term = new Xterm({ convertEol: false, cursorBlink: true, fontFamily: '"JetBrains Mono Variable", monospace',
      fontSize: 13, theme: { background: '#0b0c0f', foreground: '#d6d9df' }, scrollback: 5000 });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);
    fit.fit();
    const resize = new ResizeObserver(() => fit.fit());
    resize.observe(el);

    let id = '';
    let next: number | null = null;
    let pending = '';
    let flushing = false;
    const flush = async () => {
      if (flushing || !pending || !id) return;
      flushing = true;
      const keys = pending;
      pending = '';
      await fetch(`/api/console/${host}/${vm}?id=${encodeURIComponent(id)}`, { method: 'POST', body: keys,
        headers: { 'Content-Type': 'text/plain' } }).catch(() => {});
      flushing = false;
      if (pending) void flush();
    };
    const typed = term.onData((d) => {
      pending += d;
      setTimeout(() => void flush(), 15);
    });

    const es = new EventSource(`/api/console/${host}/${vm}`);
    es.onmessage = (m) => {
      const e = JSON.parse(m.data);
      if (e.type === 'id') {
        id = e.id;
      } else if (e.type === 'page') {
        if (next !== null && e.from > next) {
          term.write(`\r\n\x1b[2m[${e.from - next} ${w.lostGap}]\x1b[0m\r\n`);
        }
        term.write(e.text);
        next = e.next;
      } else if (e.type === 'refused') {
        setNote(e.code === 'forbidden' ? w.readerNoTyping : e.message || e.code);
      } else if (e.type === 'end') {
        setNote(e.why);
        es.close();
      }
    };
    return () => {
      es.close();
      typed.dispose();
      resize.disconnect();
      term.dispose();
    };
  }, [host, vm, w.lostGap, w.readerNoTyping]);

  return (
    <div className="space-y-2">
      <p className="text-xs text-muted">{w.detach}</p>
      <div ref={box} className="terminal h-[480px]" data-testid="terminal" />
      {note && <p role="status" className="text-sm text-warn">{note}</p>}
    </div>
  );
}
