'use client';

import { useCallback, useEffect, useState } from 'react';

import { MethodForm } from './MethodForm';
import { ResourceTable } from './ResourceTable';
import { Terminal } from './Terminal';
import { call } from '@/lib/client';
import { Descriptor, show } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';

const TABS = ['overview', 'console', 'config', 'disks', 'forwards'] as const;
type Tab = (typeof TABS)[number];

/** A VM's page: what it is and how it runs, its actions; its console; its
 * vm.toml; its disks; its forwards. */
export function VmView({ host, name, d, locale, w }: { host: string; name: string; d: Descriptor; locale: Locale; w: Words }) {
  const [tab, setTab] = useState<Tab>('overview');
  const [got, setGot] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState('');
  const [action, setAction] = useState<string | null>(null);

  const load = useCallback(async () => {
    const r = await call(host, 'vm', 'get', { name });
    if (r.ok) {
      setGot(r.data as Record<string, unknown>);
      setError('');
    } else setError(r.error.message);
  }, [host, name]);

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), 3000);
    return () => clearInterval(t);
  }, [load]);

  const vm = (got?.vm ?? {}) as Record<string, unknown>;
  const fields = d.resources?.vms?.fields ?? {};
  const label: Record<Tab, string> = { overview: w.overview, console: w.console, config: w.config, disks: w.disks, forwards: w.forwards };
  const actions = ['start', 'stop', 'restart', 'clone', 'delete'].filter((m) => d.methods[m]);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-baseline gap-3">
        <h1 className="text-2xl font-semibold text-fg">{name}</h1>
        <span className="text-sm text-muted" data-testid="vm-state">{show(fields.state, vm.state, locale)}</span>
        {vm.protected === true && <span className="rounded bg-accent-soft px-2 py-0.5 text-xs text-fg">{text(fields.protected?.title, locale)}</span>}
      </div>
      {error && <p role="alert" className="text-sm text-bad">{error}</p>}
      <div className="flex gap-1 border-b border-line" role="tablist">
        {TABS.map((t) => (
          <button key={t} type="button" role="tab" aria-selected={tab === t} onClick={() => setTab(t)}
            className={`px-3 py-2 text-sm ${tab === t ? 'border-b-2 border-accent text-fg' : 'text-muted hover:text-fg'}`}>
            {label[t]}
          </button>
        ))}
      </div>

      {tab === 'overview' && (
        <div className="space-y-4">
          <dl className="grid gap-x-6 gap-y-2 rounded-xl border border-line bg-card p-4 text-sm md:grid-cols-2">
            {['state', 'cpus', 'memory_mib', 'ip', 'uptime_ms', 'restarts', 'exits'].map((k) => (
              <div key={k} className="flex justify-between gap-4">
                <dt className="text-muted">{text(fields[k]?.title, locale) || k}</dt>
                <dd className="font-mono text-fg-soft">{show(fields[k], vm[k], locale)}</dd>
              </div>
            ))}
            {typeof vm.reason === 'string' && <div className="md:col-span-2 text-xs text-muted">{vm.reason}</div>}
          </dl>
          <div className="flex flex-wrap gap-2">
            {actions.map((m) => (
              <button key={m} type="button" onClick={() => setAction(action === m ? null : m)}
                className={`rounded-lg border px-3 py-1.5 text-sm ${action === m ? 'border-accent text-fg' : 'border-line text-fg-soft hover:border-accent'}`}>
                {text(d.methods[m]!.title, locale)}
              </button>
            ))}
          </div>
          {action && d.methods[action] && (
            <div className="rounded-xl border border-line bg-card p-4">
              <MethodForm key={action} host={host} provider="vm" name={action} method={d.methods[action]!} fixed={{ name }}
                locale={locale} w={w} onDone={() => void load()} />
            </div>
          )}
        </div>
      )}

      {tab === 'console' && <Terminal host={host} vm={name} w={w} />}

      {tab === 'config' && <ConfigEditor host={host} name={name} d={d} locale={locale} w={w} />}

      {tab === 'disks' && (
        <div className="space-y-4">
          <DiskList host={host} name={name} locale={locale} d={d} w={w} />
          {d.methods.disk_add && (
            <div className="rounded-xl border border-line bg-card p-4">
              <h2 className="mb-2 text-sm font-medium text-fg">{text(d.methods.disk_add.title, locale)}</h2>
              <MethodForm host={host} provider="vm" name="disk_add" method={d.methods.disk_add} fixed={{ name }} locale={locale} w={w} />
            </div>
          )}
        </div>
      )}

      {tab === 'forwards' && (
        <div className="space-y-4">
          {d.resources?.forwards && <ResourceTable host={host} d={d} id="forwards" r={d.resources.forwards} locale={locale} w={w} />}
          {d.methods.forward_add && (
            <div className="rounded-xl border border-line bg-card p-4">
              <h2 className="mb-2 text-sm font-medium text-fg">{text(d.methods.forward_add.title, locale)}</h2>
              <MethodForm host={host} provider="vm" name="forward_add" method={d.methods.forward_add} fixed={{ name }} locale={locale} w={w} />
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function DiskList({ host, name, d, locale, w }: { host: string; name: string; d: Descriptor; locale: Locale; w: Words }) {
  const [disks, setDisks] = useState<Array<Record<string, unknown>>>([]);
  useEffect(() => {
    void call(host, 'vm', 'disks', { name }).then((r) => r.ok && setDisks(((r.data as { disks: Array<Record<string, unknown>> }).disks) ?? []));
  }, [host, name]);
  const f = d.resources?.disks?.fields ?? {};
  return (
    <table className="w-full rounded-xl border border-line bg-card text-sm">
      <thead><tr className="text-left text-xs text-muted">
        {['file', 'size_bytes', 'read_only'].map((k) => <th key={k} className="px-4 py-2">{text(f[k]?.title, locale) || k}</th>)}
      </tr></thead>
      <tbody>
        {disks.map((disk) => (
          <tr key={String(disk.file)} className="border-t border-line">
            {['file', 'size_bytes', 'read_only'].map((k) => <td key={k} className="px-4 py-2 font-mono text-xs">{show(f[k], disk[k], locale)}</td>)}
          </tr>
        ))}
        {disks.length === 0 && <tr><td className="px-4 py-2 text-muted" colSpan={3}>{w.nothingYet}</td></tr>}
      </tbody>
    </table>
  );
}

function ConfigEditor({ host, name, d, locale, w }: { host: string; name: string; d: Descriptor; locale: Locale; w: Words }) {
  const [was, setWas] = useState('');
  const [now, setNow] = useState('');
  const [said, setSaid] = useState('');
  const load = useCallback(async () => {
    const r = await call(host, 'vm', 'config_get', { name });
    if (r.ok) {
      const t = (r.data as { text: string }).text;
      setWas(t);
      setNow(t);
    }
  }, [host, name]);
  useEffect(() => void load(), [load]);
  async function save() {
    const r = await call(host, 'vm', 'config_set', { name, text: now });
    setSaid(r.ok ? w.done : r.error.message);
    if (r.ok) void load();
  }
  const changed = was.split('\n').filter((l, i) => now.split('\n')[i] !== l).length;
  return (
    <div className="space-y-2">
      <textarea value={now} onChange={(e) => setNow(e.target.value)} rows={18} spellCheck={false} data-testid="config"
        className="w-full rounded-xl border border-line bg-card p-3 font-mono text-xs text-fg" />
      <div className="flex items-center gap-3">
        <button type="button" onClick={() => void save()} disabled={now === was}
          className="rounded-lg bg-accent px-3 py-1.5 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-50">
          {text(d.methods.config_set?.title, locale) || w.save}
        </button>
        {now !== was && <span className="text-xs text-muted">{changed} ±</span>}
        {said && <span role="status" className="text-sm text-fg-soft">{said}</span>}
      </div>
    </div>
  );
}
