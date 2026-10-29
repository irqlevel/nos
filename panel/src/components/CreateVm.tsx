'use client';

import Link from 'next/link';
import { useState } from 'react';

import { JobWatch } from './JobWatch';
import { startJob } from '@/lib/client';
import type { Words } from '@/lib/i18n';

/** Images a VM is made of in a click: the distributions' own cloud images,
 * which nos fetches itself. */
const IMAGES = [
  { label: 'Debian 13 (trixie)', url: 'https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2' },
  { label: 'Ubuntu 24.04 LTS', url: 'https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img' },
];
const GIB = 1024 ** 3;
const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/;

export function CreateVm({ host, locale, w }: { host: string; locale: string; w: Words }) {
  const [name, setName] = useState('');
  const [url, setUrl] = useState(IMAGES[0]!.url);
  const [disk, setDisk] = useState('20');
  const [mem, setMem] = useState('1024');
  const [cpus, setCpus] = useState('1');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [job, setJob] = useState<number | null>(null);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!NAME.test(name)) {
      setError(locale === 'ru' ? 'Имя: буква или цифра, затем буквы, цифры, . _ и -, до 32.' :
        'A name: a letter or a digit, then letters, digits, . _ and -, at most 32.');
      return;
    }
    setError('');
    const params: Record<string, unknown> = { name, url, memory_mib: Number(mem), cpus: Number(cpus) };
    if (disk) params.size_bytes = Math.round(Number(disk) * GIB);
    if (password) params.password = password;
    const r = await startJob(host, 'vm', 'create', params);
    if (r.ok) setJob(r.id);
    else setError(r.error.message);
  }

  const input = 'mt-1 w-full rounded-lg border border-line bg-bg px-3 py-2 text-fg';
  return (
    <div className="max-w-xl space-y-4">
      <h1 className="text-2xl font-semibold text-fg">{w.createVm}</h1>
      <form onSubmit={submit} className="space-y-3 rounded-xl border border-line bg-card p-4">
        <label className="block text-sm text-fg-soft">{w.name}
          <input value={name} onChange={(e) => setName(e.target.value)} name="name" className={input} />
        </label>
        <label className="block text-sm text-fg-soft">{w.imageUrl}
          <select value={IMAGES.some((i) => i.url === url) ? url : ''} onChange={(e) => e.target.value && setUrl(e.target.value)}
            className={input}>
            {IMAGES.map((i) => <option key={i.url} value={i.url}>{i.label}</option>)}
            <option value="">…</option>
          </select>
          <input value={url} onChange={(e) => setUrl(e.target.value)} name="url" className={input + ' font-mono text-xs'} />
        </label>
        <div className="grid grid-cols-3 gap-3">
          <label className="block text-sm text-fg-soft">{locale === 'ru' ? 'Диск, ГиБ' : 'Disk, GiB'}
            <input value={disk} onChange={(e) => setDisk(e.target.value)} inputMode="numeric" name="disk" className={input} />
          </label>
          <label className="block text-sm text-fg-soft">{locale === 'ru' ? 'Память, МиБ' : 'Memory, MiB'}
            <input value={mem} onChange={(e) => setMem(e.target.value)} inputMode="numeric" name="memory" className={input} />
          </label>
          <label className="block text-sm text-fg-soft">{w.cpu}
            <input value={cpus} onChange={(e) => setCpus(e.target.value)} inputMode="numeric" name="cpus" className={input} />
          </label>
        </div>
        <label className="block text-sm text-fg-soft">{locale === 'ru' ? 'Пароль консоли (необязательно)' : 'Console password (optional)'}
          <input value={password} onChange={(e) => setPassword(e.target.value)} type="password" name="password" className={input} />
        </label>
        {error && <p role="alert" className="text-sm text-bad">{error}</p>}
        <button type="submit" disabled={job !== null}
          className="rounded-lg bg-accent px-3 py-2 text-sm font-medium text-bg hover:bg-accent-strong disabled:opacity-60">
          {w.create}
        </button>
      </form>
      {job !== null && (
        <>
          <JobWatch host={host} id={job} w={w} />
          <Link href={`/${locale}/h/${host}/vms/${name}`} className="text-sm text-accent hover:underline">{name} →</Link>
        </>
      )}
    </div>
  );
}
