'use client';

import { useState } from 'react';

import { MethodForm } from './MethodForm';
import { ResourceTable } from './ResourceTable';
import { Descriptor } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';


/** A provider's page, from its descriptor alone: its resources' tables,
 * then every method no resource shows -- reads to run and look at, actions
 * and jobs as forms. */
export function ProviderView({ host, d, locale, w }: { host: string; d: Descriptor; locale: Locale; w: Words }) {
  const resources = Object.entries(d.resources ?? {}).filter(([, r]) => !r.parent);
  const shown = new Set(resources.flatMap(([, r]) => [r.list, r.get, ...(r.actions ?? []).map((a) => a.method)]));
  const rest = Object.entries(d.methods).filter(([name]) => !shown.has(name));
  const [open, setOpen] = useState<string | null>(null);

  return (
    <div className="space-y-6">
      <div>
        <h1 className="text-2xl font-semibold text-fg">{text(d.title, locale)}</h1>
        {d.help && <p className="mt-1 text-sm text-muted">{text(d.help, locale)}</p>}
      </div>
      {resources.map(([id, r]) => <ResourceTable key={id} host={host} d={d} id={id} r={r} locale={locale} w={w} />)}
      {rest.length > 0 && (
        <section className="rounded-xl border border-line bg-card">
          <h2 className="border-b border-line px-4 py-2 text-sm font-medium text-fg">{w.actions}</h2>
          <ul>
            {rest.map(([name, m]) => (
              <li key={name} className="border-t border-line first:border-t-0">
                <button type="button" onClick={() => setOpen(open === name ? null : name)}
                  className="flex w-full items-center justify-between px-4 py-2 text-left text-sm hover:bg-bg-soft">
                  <span className="text-fg">{text(m.title, locale)}</span>
                  <span className="font-mono text-xs text-muted">{w[`kind_${m.kind}`]}</span>
                </button>
                {open === name && (
                  <div className="px-4 pb-4">
                    {m.help && <p className="mb-2 text-xs text-muted">{text(m.help, locale)}</p>}
                    <MethodForm host={host} provider={d.provider} name={name} method={m} locale={locale} w={w} />
                  </div>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
