'use client';

import { useCallback, useEffect, useState } from 'react';

import { MethodForm } from './MethodForm';
import { call } from '@/lib/client';
import { Descriptor, Resource, show } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';

type Row = Record<string, unknown>;

/** A parameter an action is given from its row: "$field" the row's field. */
function bind(params: Record<string, unknown> | undefined, row: Row): Record<string, unknown> {
  return Object.fromEntries(Object.entries(params ?? {}).map(([k, v]) =>
    [k, typeof v === 'string' && v.startsWith('$') ? row[v.slice(1)] : v]));
}

/** A resource's rows, as its list method gives them, its columns as its
 * descriptor says them; each row's actions a click away. */
export function ResourceTable({ host, d, id, r, locale, w, every = 0, extra }: {
  host: string; d: Descriptor; id: string; r: Resource; locale: Locale; w: Words; every?: number;
  extra?: (row: Row) => React.ReactNode;
}) {
  const [rows, setRows] = useState<Row[] | null>(null);
  const [error, setError] = useState('');
  const [open, setOpen] = useState<{ key: unknown; method: string } | null>(null);

  const load = useCallback(async () => {
    const got = await call(host, d.provider, r.list);
    if (got.ok) {
      setRows(((got.data as Record<string, unknown>)[r.items] as Row[]) ?? []);
      setError('');
    } else {
      setError(got.error.message);
    }
  }, [host, d.provider, r.list, r.items]);

  useEffect(() => {
    void load();
    if (!every) return;
    const t = setInterval(() => void load(), every);
    return () => clearInterval(t);
  }, [load, every]);

  return (
    <section className="rounded-xl border border-line bg-card" data-testid={`resource-${id}`}>
      <div className="flex items-center justify-between border-b border-line px-4 py-2">
        <h2 className="text-sm font-medium text-fg">{text(r.title, locale)}</h2>
        <button type="button" onClick={() => void load()} className="text-xs text-muted hover:text-fg">{w.refresh}</button>
      </div>
      {error && <p role="alert" className="px-4 py-3 text-sm text-bad">{error}</p>}
      {rows !== null && rows.length === 0 && <p className="px-4 py-3 text-sm text-muted">{w.nothingYet}</p>}
      {rows !== null && rows.length > 0 && (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-xs text-muted">
                {r.columns.map((c) => <th key={c} className="px-4 py-2 font-medium">{text(r.fields[c]?.title, locale) || c}</th>)}
                {(r.actions?.length || extra) && <th className="px-4 py-2 font-medium">{w.actions}</th>}
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const key = row[r.key];
                return [
                  <tr key={String(key)} className="border-t border-line">
                    {r.columns.map((c) => (
                      <td key={c} className="px-4 py-2 font-mono text-xs text-fg-soft">
                        {extra && c === r.key ? extra(row) : show(r.fields[c], row[c], locale)}
                      </td>
                    ))}
                    {(r.actions?.length ?? 0) > 0 && (
                      <td className="space-x-2 px-4 py-2 whitespace-nowrap">
                        {r.actions!.map((a) => (
                          <button key={a.method} type="button" onClick={() => setOpen(open?.key === key && open?.method === a.method
                            ? null : { key, method: a.method })}
                            className="rounded px-2 py-0.5 text-xs text-accent hover:bg-accent-soft">
                            {text(d.methods[a.method]?.title, locale) || a.method}
                          </button>
                        ))}
                      </td>
                    )}
                  </tr>,
                  open && open.key === key && d.methods[open.method] ? (
                    <tr key={String(key) + '/form'} className="bg-bg-soft">
                      <td colSpan={r.columns.length + 1} className="px-4 py-3">
                        <MethodForm host={host} provider={d.provider} name={open.method} method={d.methods[open.method]!}
                          fixed={bind(r.actions!.find((a) => a.method === open?.method)?.params, row)} locale={locale} w={w}
                          onDone={() => void load()} />
                      </td>
                    </tr>
                  ) : null,
                ];
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
