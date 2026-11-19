'use client';

import Link from 'next/link';
import { useCallback, useEffect, useRef, useState } from 'react';

import { DiskIo } from './DiskIo';
import { MethodForm } from './MethodForm';
import { ResourceTable } from './ResourceTable';
import { Terminal } from './Terminal';
import { call } from '@/lib/client';
import { bytes, Descriptor, show } from '@/lib/descriptor';
import { Locale, text, Words } from '@/lib/i18n';
import { NbsVolume } from '@/lib/images';

const TABS = ['overview', 'console', 'config', 'disks', 'forwards'] as const;
type Tab = (typeof TABS)[number];

/** The fw provider's method that puts a VM on another firewall. */
const ASSIGN = 'assign';

/** A VM's page: what it is and how it runs -- the firewall its network is
 * judged by among it -- its actions; its console; its vm.toml; its disks;
 * its forwards. Once it has moved to another host, where it went, and a
 * link to its page there where the panel knows that host by its name
 * (`hosts`, the panel's). */
export function VmView({ host, name, d, nbs = null, fw = null, hosts = [], locale, w }: {
  host: string; name: string; d: Descriptor; nbs?: Descriptor | null; fw?: Descriptor | null;
  hosts?: Array<{ id: string; name: string }>; locale: Locale; w: Words;
}) {
  const [tab, setTab] = useState<Tab>('overview');
  const [got, setGot] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState('');
  const [action, setAction] = useState<string | null>(null);
  /* Counted up as a snapshot is taken: its list loaded again */
  const [taken, setTaken] = useState(0);
  /* The host a move took it to, once the move is done: nothing of it is
   * here any more, and the page stops asking. While its move runs, a read
   * that fails is the move ending -- the VM gone from here before the job
   * says it is done -- and is not said as an error: the job says how it
   * ended, and the page reads it again where it stayed. */
  const [moved, setMoved] = useState<string | null>(null);
  const moving = useRef(false);

  const load = useCallback(async () => {
    const r = await call(host, 'vm', 'get', { name });
    if (r.ok) {
      setGot(r.data as Record<string, unknown>);
      setError('');
    } else if (!moving.current) setError(r.error.message);
  }, [host, name]);

  useEffect(() => {
    if (moved !== null) return;
    void load();
    const t = setInterval(() => void load(), 3000);
    return () => clearInterval(t);
  }, [load, moved]);

  const vm = (got?.vm ?? {}) as Record<string, unknown>;
  const fields = d.resources?.vms?.fields ?? {};
  const label: Record<Tab, string> = { overview: w.overview, console: w.console, config: w.config, disks: w.disks, forwards: w.forwards };
  const actions = ['start', 'stop', 'restart', 'snapshot', 'clone', 'migrate', 'delete'].filter((m) => d.methods[m]);
  /* The panel's id of the host it moved to, where the panel has one of that name */
  const there = moved === null ? undefined : hosts.find((h) => h.name === moved)?.id;
  /* Its disks that are volumes of NBS's: what has snapshots */
  const volumes = ((got?.disks ?? []) as Array<{ file?: unknown }>).map((x) => String(x.file ?? ''))
    .filter((f) => f.startsWith('nbs:')).map((f) => f.slice('nbs:'.length));

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-baseline gap-3">
        <h1 className="text-2xl font-semibold text-fg">{name}</h1>
        {/* Its state here is gone with it once it has moved: the note below says where */}
        {moved === null && <span className="text-sm text-muted" data-testid="vm-state">{show(fields.state, vm.state, locale)}</span>}
        {vm.protected === true && <span className="rounded bg-accent-soft px-2 py-0.5 text-xs text-fg">{text(fields.protected?.title, locale)}</span>}
      </div>
      {error && <p role="alert" className="text-sm text-bad">{error}</p>}
      {moved !== null && (
        <p role="status" data-testid="vm-moved" className="rounded-lg bg-accent-soft px-3 py-2 text-sm text-fg">
          {w.movedTo} <span className="font-mono">{moved}</span>
          {there && <> -- <Link href={`/${locale}/h/${there}/vms/${name}`} className="text-accent hover:underline">{w.openThere}</Link></>}
        </p>
      )}
      {moved === null && <div className="flex gap-1 border-b border-line" role="tablist">
        {TABS.map((t) => (
          <button key={t} type="button" role="tab" aria-selected={tab === t} onClick={() => setTab(t)}
            className={`px-3 py-2 text-sm ${tab === t ? 'border-b-2 border-accent text-fg' : 'text-muted hover:text-fg'}`}>
            {label[t]}
          </button>
        ))}
      </div>}

      {(tab === 'overview' || moved !== null) && (
        <div className="space-y-4">
          {moved === null && <dl className="grid gap-x-6 gap-y-2 rounded-xl border border-line bg-card p-4 text-sm md:grid-cols-2">
            {['state', 'cpus', 'memory_mib', 'node', 'node_memory_pct', 'ip', 'public', 'firewall', 'uptime_ms', 'restarts', 'exits']
              /* A NUMA node only where it has one: a host of nodes, or a vm.toml that asks for one */
              .filter((k) => fields[k] && (!k.startsWith('node') || (vm[k] !== null && vm[k] !== undefined))).map((k) => (
              <div key={k} className="flex justify-between gap-4">
                <dt className="text-muted">{text(fields[k]?.title, locale) || k}</dt>
                <dd className="font-mono text-fg-soft">{show(fields[k], vm[k], locale)}</dd>
              </div>
            ))}
            {typeof vm.reason === 'string' && <div className="md:col-span-2 text-xs text-muted">{vm.reason}</div>}
          </dl>}
          {moved === null && <div className="flex flex-wrap gap-2">
            {actions.map((m) => (
              <button key={m} type="button" onClick={() => setAction(action === m ? null : m)}
                className={`rounded-lg border px-3 py-1.5 text-sm ${action === m ? 'border-accent text-fg' : 'border-line text-fg-soft hover:border-accent'}`}>
                {text(d.methods[m]!.title, locale)}
              </button>
            ))}
            {fw?.methods[ASSIGN] && (
              <button type="button" onClick={() => setAction(action === ASSIGN ? null : ASSIGN)} data-testid="vm-firewall"
                className={`rounded-lg border px-3 py-1.5 text-sm ${action === ASSIGN ? 'border-accent text-fg' : 'border-line text-fg-soft hover:border-accent'}`}>
                {text(fw.methods[ASSIGN].title, locale)}
              </button>
            )}
          </div>}
          {action === ASSIGN && fw?.methods[ASSIGN] && (
            <div className="rounded-xl border border-line bg-card p-4">
              <MethodForm key={ASSIGN} host={host} provider="fw" name={ASSIGN} method={fw.methods[ASSIGN]} fixed={{ vm: name }}
                locale={locale} w={w} onDone={() => void load()} />
            </div>
          )}
          {action && action !== ASSIGN && d.methods[action] && (
            <div className="rounded-xl border border-line bg-card p-4">
              <MethodForm key={action} host={host} provider="vm" name={action} method={d.methods[action]!} fixed={{ name }}
                locale={locale} w={w}
                onStart={() => {
                  if (action === 'migrate') moving.current = true;
                }}
                onDone={(job) => {
                  if (action === 'migrate') {
                    moving.current = false;
                    const to = (job?.result as { host?: unknown } | null | undefined)?.host;
                    if (job?.status === 'ok' && typeof to === 'string') {
                      setError('');
                      setMoved(to);
                      return;
                    }
                  }
                  void load();
                  if (action === 'snapshot') setTaken((n) => n + 1);
                }} />
            </div>
          )}
          {moved === null && volumes.length > 0 && <Snapshots host={host} volumes={volumes} taken={taken} locale={locale} w={w} />}
        </div>
      )}

      {moved === null && tab === 'console' && <Terminal host={host} vm={name} w={w} />}

      {moved === null && tab === 'config' && <ConfigEditor host={host} name={name} d={d} locale={locale} w={w} />}

      {moved === null && tab === 'disks' && (
        <div className="space-y-4">
          {d.methods.disk_io && <DiskIo host={host} name={name} nbs={nbs} locale={locale} w={w} />}
          <DiskList host={host} name={name} locale={locale} d={d} w={w} />
          {(['disk_add', 'disk_attach', 'disk_detach'] as const).map((m) => d.methods[m] && (
            <div key={m} className="rounded-xl border border-line bg-card p-4">
              <h2 className="mb-2 text-sm font-medium text-fg">{text(d.methods[m]!.title, locale)}</h2>
              <MethodForm host={host} provider="vm" name={m} method={d.methods[m]!} fixed={{ name }} locale={locale} w={w} />
            </div>
          ))}
        </div>
      )}

      {moved === null && tab === 'forwards' && (
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

/** The snapshots of a VM's volumes of NBS's, each a click from a VM of a
 * clone of it. */
function Snapshots({ host, volumes, taken, locale, w }: {
  host: string; volumes: string[]; taken: number; locale: Locale; w: Words;
}) {
  const [rows, setRows] = useState<NbsVolume[] | null>(null);
  const of = volumes.join(' ');
  useEffect(() => {
    let live = true;
    void call(host, 'nbs', 'volumes').then((r) => {
      if (!live || !r.ok) return;
      const mine = of.split(' ');
      setRows(((r.data as { volumes?: NbsVolume[] }).volumes ?? [])
        .filter((v) => v.kind === 'snapshot' && v.origin !== null && mine.includes(v.origin)));
    });
    return () => {
      live = false;
    };
  }, [host, of, taken]);
  if (rows === null) return null;
  return (
    <section className="rounded-xl border border-line bg-card" data-testid="vm-snapshots">
      <h2 className="border-b border-line px-4 py-2 text-sm font-medium text-fg">{w.snapshots}</h2>
      {rows.length === 0 ? <p className="px-4 py-3 text-sm text-muted">{w.nothingYet}</p> : (
        <table className="w-full text-sm">
          <tbody>
            {rows.map((v) => (
              <tr key={v.name} className="border-t border-line first:border-t-0">
                <td className="px-4 py-2 font-mono text-xs text-fg">{v.name}</td>
                <td className="px-4 py-2 font-mono text-xs text-fg-soft">{v.origin}</td>
                <td className="px-4 py-2 font-mono text-xs text-fg-soft">{bytes(v.size)}</td>
                <td className="px-4 py-2 text-right">
                  <Link href={`/${locale}/h/${host}/vms/new?image=${encodeURIComponent(v.name)}`}
                    className="rounded px-2 py-0.5 text-xs text-accent hover:bg-accent-soft">{w.createVm}</Link>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}

function DiskList({ host, name, d, locale, w }: { host: string; name: string; d: Descriptor; locale: Locale; w: Words }) {
  const [disks, setDisks] = useState<Array<Record<string, unknown>>>([]);
  useEffect(() => {
    void call(host, 'vm', 'disks', { name }).then((r) => r.ok && setDisks(((r.data as { disks: Array<Record<string, unknown>> }).disks) ?? []));
  }, [host, name]);
  const f = d.resources?.disks?.fields ?? {};
  /* A device's disk: its id the file, and this boot's name beside it. */
  const columns = ['file', 'size_bytes', 'read_only', 'device_name'];
  return (
    <table className="w-full rounded-xl border border-line bg-card text-sm" data-testid="vm-disks">
      <thead><tr className="text-left text-xs text-muted">
        {columns.map((k) => <th key={k} className="px-4 py-2">{text(f[k]?.title, locale) || k}</th>)}
      </tr></thead>
      <tbody>
        {disks.map((disk) => (
          <tr key={String(disk.file)} className="border-t border-line">
            {columns.map((k) => <td key={k} className="px-4 py-2 font-mono text-xs">{show(f[k], disk[k], locale)}</td>)}
          </tr>
        ))}
        {disks.length === 0 && <tr><td className="px-4 py-2 text-muted" colSpan={columns.length}>{w.nothingYet}</td></tr>}
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
