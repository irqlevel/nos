'use client';

import Link from 'next/link';
import { useEffect, useState } from 'react';

import { JobWatch } from './JobWatch';
import { call, startJob } from '@/lib/client';
import { bytes } from '@/lib/descriptor';
import type { Words } from '@/lib/i18n';

/** Images a VM is made of in a click: the distributions' own cloud images,
 * which nos fetches itself. */
const IMAGES = [
  { label: 'Debian 13 (trixie)', url: 'https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.qcow2' },
  { label: 'Ubuntu 24.04 LTS', url: 'https://cloud-images.ubuntu.com/releases/noble/release/ubuntu-24.04-server-cloudimg-amd64.img' },
];
const GIB = 1024 ** 3;
const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,31}$/;
/** A volume's name of NBS's, as nbsmeta takes one: an image's clone's. */
const VOLUME = /^[a-z0-9][a-z0-9._-]{0,62}$/;
/** What `onto` names a volume of NBS's by. */
const NBS = 'nbs:';

/** A device of the host's a VM's disk may be (the vm provider's `devices`):
 * the image poured onto it in place of a file. `refused` says why it is not
 * given as it is -- force gives it over what is on it, or another VM's; one
 * the machine boots from or something holds is never given. */
interface Device {
  name: string; id: string | null; size_bytes: number; holds: string | null; in_use_by: string | null;
  boots_the_machine: string | null; refused: string | null;
}

/** A volume of NBS's (the nbs provider's `volumes`): an image or a snapshot
 * a VM is made of -- cloned, its clone the VM's disk -- or a volume a URL's
 * image is poured onto, taken from a writer elsewhere with force. */
interface Volume {
  name: string; kind: 'volume' | 'image' | 'snapshot'; size: number; writer: string | null;
}

function usable(d: Device): boolean {
  return d.id !== null && d.in_use_by === null && d.boots_the_machine === null;
}

export function CreateVm({ host, locale, w }: { host: string; locale: string; w: Words }) {
  const ru = locale === 'ru';
  const [name, setName] = useState('');
  const [source, setSource] = useState<'url' | 'image'>('url');
  const [url, setUrl] = useState(IMAGES[0]!.url);
  const [image, setImage] = useState('');
  const [clone, setClone] = useState('');
  const [disk, setDisk] = useState('20');
  const [mem, setMem] = useState('1024');
  const [cpus, setCpus] = useState('1');
  const [password, setPassword] = useState('');
  const [devices, setDevices] = useState<Device[]>([]);
  /* Null where the host has no NBS, or is in no NBS cluster */
  const [volumes, setVolumes] = useState<Volume[] | null>(null);
  const [onto, setOnto] = useState('');
  const [force, setForce] = useState(false);
  const [error, setError] = useState('');
  const [job, setJob] = useState<number | null>(null);

  useEffect(() => {
    let live = true;
    call(host, 'vm', 'devices').then((r) => {
      if (live && r.ok) setDevices(((r.data as { devices?: Device[] }).devices ?? []).filter(usable));
    });
    call(host, 'nbs', 'volumes').then((r) => {
      if (live && r.ok) setVolumes((r.data as { volumes?: Volume[] }).volumes ?? []);
    });
    return () => { live = false; };
  }, [host]);
  const images = (volumes ?? []).filter((v) => v.kind !== 'volume');
  const targets = (volumes ?? []).filter((v) => v.kind === 'volume');
  const device = devices.find((d) => d.id === onto);
  const volume = onto.startsWith(NBS) ? targets.find((v) => NBS + v.name === onto) : undefined;
  const picked = images.find((v) => v.name === image);
  /* A file's size is asked; a device's and a volume's disk is theirs; an
   * image's clone is the image's size unless asked bigger. */
  const sized = source === 'image' || onto === '';

  function pickSource(s: 'url' | 'image') {
    setSource(s);
    setOnto('');
    setForce(false);
    setDisk(s === 'url' ? '20' : '');
    if (s === 'image' && !image && images.length) setImage(images[0]!.name);
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!NAME.test(name)) {
      setError(ru ? 'Имя: буква или цифра, затем буквы, цифры, . _ и -, до 32.' :
        'A name: a letter or a digit, then letters, digits, . _ and -, at most 32.');
      return;
    }
    const params: Record<string, unknown> = { name, memory_mib: Number(mem), cpus: Number(cpus) };
    if (source === 'image') {
      if (!picked) {
        setError(ru ? 'Выберите образ NBS.' : 'Pick an image of NBS\'s.');
        return;
      }
      const cloneName = clone || name;
      if (!VOLUME.test(cloneName)) {
        setError(ru ? 'Имя клона: строчные буквы, цифры, . _ и -, до 63.' :
          'The clone\'s name: lowercase letters, digits, . _ and -, at most 63.');
        return;
      }
      params.image = picked.name;
      if (cloneName !== name) params.onto = NBS + cloneName;
      if (disk) params.size_bytes = Math.round(Number(disk) * GIB);
    } else {
      params.url = url;
      if (onto) {
        params.onto = onto;
        if (force) params.force = true;
      } else if (disk) {
        params.size_bytes = Math.round(Number(disk) * GIB);
      }
    }
    if (password) params.password = password;
    setError('');
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
        {volumes !== null && (
          <fieldset className="flex gap-4 text-sm text-fg-soft" data-testid="source">
            <legend className="sr-only">{ru ? 'Из чего диск' : 'Its disk made of'}</legend>
            <label className="flex items-center gap-2">
              <input type="radio" name="source" value="url" checked={source === 'url'} onChange={() => pickSource('url')} />
              {ru ? 'Образ по URL' : 'An image by its URL'}
            </label>
            <label className="flex items-center gap-2">
              <input type="radio" name="source" value="image" checked={source === 'image'} onChange={() => pickSource('image')} />
              {ru ? 'Образ NBS' : 'An image of NBS\'s'}
            </label>
          </fieldset>
        )}
        {source === 'url' ? (
          <>
            <label className="block text-sm text-fg-soft">{w.imageUrl}
              <select value={IMAGES.some((i) => i.url === url) ? url : ''} onChange={(e) => e.target.value && setUrl(e.target.value)}
                className={input}>
                {IMAGES.map((i) => <option key={i.url} value={i.url}>{i.label}</option>)}
                <option value="">…</option>
              </select>
              <input value={url} onChange={(e) => setUrl(e.target.value)} name="url" className={input + ' font-mono text-xs'} />
            </label>
            <label className="block text-sm text-fg-soft">{ru ? 'Где диск' : 'Its disk'}
              <select value={onto} onChange={(e) => { setOnto(e.target.value); setForce(false); }} name="onto" className={input}>
                <option value="">{ru ? 'Файл в папке машины' : 'A file in its folder'}</option>
                {devices.length > 0 && (
                  <optgroup label={ru ? 'Устройство хоста' : 'A device of the host\'s'}>
                    {devices.map((d) => (
                      <option key={d.id!} value={d.id!}>
                        {d.name} · {(d.size_bytes / GIB).toFixed(1)} GiB{d.holds ? ' · ' + d.holds : ''}
                      </option>
                    ))}
                  </optgroup>
                )}
                {targets.length > 0 && (
                  <optgroup label={ru ? 'Том NBS' : 'A volume of NBS\'s'}>
                    {targets.map((v) => (
                      <option key={v.name} value={NBS + v.name}>
                        {NBS + v.name} · {bytes(v.size)}{v.writer ? ' · ' + (ru ? 'подключён на ' : 'attached on ') + v.writer : ''}
                      </option>
                    ))}
                  </optgroup>
                )}
              </select>
            </label>
            {device?.refused && (
              <div className="space-y-1 rounded-lg border border-line p-2 text-sm" data-testid="onto-refused">
                <p className="text-bad">{device.refused}</p>
                <label className="flex items-center gap-2 text-fg-soft">
                  <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} name="force" />
                  {ru ? 'Записать образ поверх данных' : 'Pour the image over what is on it'}
                </label>
              </div>
            )}
            {volume && (
              <div className="space-y-1 rounded-lg border border-line p-2 text-sm" data-testid="onto-volume">
                <p className="text-fg-soft">
                  {ru ? 'Образ запишется поверх всего, что есть на томе; диск образа должен в нём поместиться.' :
                    'The image is poured over whatever the volume holds; its disk must fit in the volume.'}
                </p>
                {volume.writer && (
                  <label className="flex items-center gap-2 text-fg-soft">
                    <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} name="force" />
                    {ru ? `Забрать том у ${volume.writer}` : `Take the volume from ${volume.writer}`}
                  </label>
                )}
              </div>
            )}
          </>
        ) : (
          <>
            {images.length === 0 ? (
              <p className="text-sm text-muted" data-testid="no-images">
                {ru ? 'В NBS нет ни образов, ни снимков: они делаются на странице блочного хранилища.' :
                  'NBS holds no image or snapshot yet: the Block storage page makes them.'}
              </p>
            ) : (
              <label className="block text-sm text-fg-soft">{ru ? 'Образ NBS' : 'An image of NBS\'s'}
                <select value={image} onChange={(e) => setImage(e.target.value)} name="image" className={input}>
                  {images.map((v) => (
                    <option key={v.name} value={v.name}>
                      {v.name} · {v.kind === 'image' ? (ru ? 'образ' : 'image') : (ru ? 'снимок' : 'snapshot')} · {bytes(v.size)}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <label className="block text-sm text-fg-soft">{ru ? 'Том-клон для диска (необязательно)' : 'Its disk\'s clone (optional)'}
              <input value={clone} onChange={(e) => setClone(e.target.value)} name="clone" placeholder={name}
                className={input + ' font-mono text-xs'} />
            </label>
          </>
        )}
        <div className="grid grid-cols-3 gap-3">
          <label className="block text-sm text-fg-soft">{ru ? 'Диск, ГиБ' : 'Disk, GiB'}
            <input value={sized ? disk : ''} onChange={(e) => setDisk(e.target.value)} inputMode="numeric" name="disk"
              disabled={!sized}
              placeholder={!sized ? (volume ? (ru ? 'размер тома' : "the volume's") : (ru ? 'размер устройства' : "the device's"))
                : source === 'image' && picked ? (picked.size / GIB).toFixed(1) : ''}
              className={input} />
          </label>
          <label className="block text-sm text-fg-soft">{ru ? 'Память, МиБ' : 'Memory, MiB'}
            <input value={mem} onChange={(e) => setMem(e.target.value)} inputMode="numeric" name="memory" className={input} />
          </label>
          <label className="block text-sm text-fg-soft">{w.cpu}
            <input value={cpus} onChange={(e) => setCpus(e.target.value)} inputMode="numeric" name="cpus" className={input} />
          </label>
        </div>
        <label className="block text-sm text-fg-soft">{ru ? 'Пароль консоли (необязательно)' : 'Console password (optional)'}
          <input value={password} onChange={(e) => setPassword(e.target.value)} type="password" name="password" className={input} />
        </label>
        {error && <p role="alert" className="text-sm text-bad">{error}</p>}
        <button type="submit" disabled={job !== null || (source === 'image' && !picked)}
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
